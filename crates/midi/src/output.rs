//! Native external MIDI output discovery and bounded, off-callback scheduling.
//!
//! `MidiOutputSender::try_send_at` accepts a complete short MIDI message without
//! allocating, waiting for a lock, or calling the OS MIDI driver. Timestamps are
//! **monotonic microseconds since the stream was opened**, not transport ticks,
//! audio samples or driver timestamps. An ordinary worker thread sorts pending
//! messages by (deadline, enqueue sequence) and owns the blocking `midir` send.
//! This is best-effort wall-clock scheduling, not sample-accurate MIDI output.
//!
//! A shared reservation limit bounds the *sum* of packets in the channel and
//! the worker heap, including with far-future deadlines. A failed or unplugged
//! device stops the worker and reports loss; callers must handle note cleanup.
//! This module deliberately does not route DAW tracks or playback to hardware.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use thiserror::Error;

/// Maximum combined number of queued and worker-scheduled short messages.
pub const MAX_MIDI_OUTPUT_EVENTS: usize = 16_384;
/// Do not hold a driver's arbitrarily distant MIDI events indefinitely.
pub const MAX_SCHEDULE_AHEAD_MICROS: u64 = 60_000_000;
const MAX_OUTPUT_PORTS: usize = 256;
const WORKER_POLL_MICROS: u64 = 10_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputPortInfo {
    /// Ephemeral scan index, checked again on opening; never persist it in a session.
    pub index: usize,
    pub name: String,
    /// Opaque driver identity, also rechecked on open (not a session-persistent ID).
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MidiOutputError {
    #[error("MIDI output backend failed: {0}")]
    Backend(String),
    #[error("MIDI output port {0} is unavailable")]
    UnavailablePort(usize),
    #[error("MIDI output port {index} changed since scanning (name or driver identity)")]
    StalePort { index: usize },
    #[error("multiple MIDI output ports are named {0:?}; select an unambiguous port")]
    AmbiguousPort(String),
    #[error("MIDI output exposes more than {MAX_OUTPUT_PORTS} ports")]
    TooManyPorts,
    #[error("MIDI output capacity must be 1..={MAX_MIDI_OUTPUT_EVENTS}; received {0}")]
    InvalidCapacity(usize),
    #[error("MIDI output connection failed: {0}")]
    Connection(String),
    #[error("MIDI output worker could not start: {0}")]
    Worker(String),
    #[error("MIDI output connection timed out")]
    ConnectionTimeout,
    #[error("hardware MIDI output is unsupported on this platform")]
    Unsupported,
}

/// Errors returned immediately by the nonblocking producer; no heap payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MidiOutputSendError {
    #[error("invalid short MIDI message: {0}")]
    InvalidPacket(&'static str),
    #[error("MIDI message deadline is more than 60 seconds ahead")]
    TooFarAhead,
    #[error("MIDI output queue and schedule are full")]
    Full,
    #[error("MIDI output worker is not connected")]
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MidiOutputWorkerState {
    Starting = 0,
    Running = 1,
    Failed = 2,
    Stopped = 3,
}

impl MidiOutputWorkerState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::Failed,
            _ => Self::Stopped,
        }
    }
}

/// Cheap read-only status, intended for UI/control polling rather than callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputStatus {
    pub worker: MidiOutputWorkerState,
    /// Reserved events across both the channel and the worker's scheduling heap.
    pub in_flight: usize,
    pub accepted: u64,
    pub sent: u64,
    pub dropped_full: u64,
    pub rejected_invalid: u64,
    pub rejected_future: u64,
    pub dropped_out_of_order: u64,
    pub sent_late: u64,
    pub send_errors: u64,
    pub cancelled: u64,
    pub last_error: Option<String>,
}

struct Shared {
    capacity: usize,
    in_flight: AtomicUsize,
    shutdown: AtomicBool,
    worker: AtomicU8,
    accepted: AtomicU64,
    sent: AtomicU64,
    dropped_full: AtomicU64,
    rejected_invalid: AtomicU64,
    rejected_future: AtomicU64,
    dropped_out_of_order: AtomicU64,
    sent_late: AtomicU64,
    send_errors: AtomicU64,
    cancelled: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn new(capacity: usize, state: MidiOutputWorkerState) -> Self {
        Self {
            capacity,
            in_flight: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            worker: AtomicU8::new(state as u8),
            accepted: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            dropped_full: AtomicU64::new(0),
            rejected_invalid: AtomicU64::new(0),
            rejected_future: AtomicU64::new(0),
            dropped_out_of_order: AtomicU64::new(0),
            sent_late: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    fn state(&self) -> MidiOutputWorkerState {
        MidiOutputWorkerState::from_raw(self.worker.load(Ordering::Acquire))
    }

    /// A producer may have reserved immediately before a worker stopped. Saturating
    /// release also handles that race after the worker discards all reservations.
    fn release_one(&self) {
        let mut current = self.in_flight.load(Ordering::Relaxed);
        while current != 0 {
            match self.in_flight.compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return,
                Err(latest) => current = latest,
            }
        }
    }

    fn reserve_one(&self) -> bool {
        let mut current = self.in_flight.load(Ordering::Relaxed);
        loop {
            if current >= self.capacity {
                return false;
            }
            match self.in_flight.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(latest) => current = latest,
            }
        }
    }

    fn fail(&self, reason: String) {
        if let Ok(mut error) = self.last_error.lock() {
            *error = Some(reason);
        }
        self.worker.store(MidiOutputWorkerState::Failed as u8, Ordering::Release);
    }

    fn snapshot(&self) -> MidiOutputStatus {
        MidiOutputStatus {
            worker: self.state(),
            in_flight: self.in_flight.load(Ordering::Acquire),
            accepted: self.accepted.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            dropped_full: self.dropped_full.load(Ordering::Relaxed),
            rejected_invalid: self.rejected_invalid.load(Ordering::Relaxed),
            rejected_future: self.rejected_future.load(Ordering::Relaxed),
            dropped_out_of_order: self.dropped_out_of_order.load(Ordering::Relaxed),
            sent_late: self.sent_late.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ScheduledPacket {
    deadline_micros: u64,
    sequence: u64,
    bytes: [u8; 3],
    len: u8,
}

impl ScheduledPacket {
    fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn order(&self) -> (u64, u64) {
        (self.deadline_micros, self.sequence)
    }
}

fn validate_short_packet(bytes: &[u8]) -> Result<([u8; 3], u8), MidiOutputSendError> {
    let Some(&status) = bytes.first() else {
        return Err(MidiOutputSendError::InvalidPacket("empty packet"));
    };
    let length = match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => 3,
        0xC0..=0xDF | 0xF1 | 0xF3 => 2,
        0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE..=0xFF => 1,
        0xF0 | 0xF7 => return Err(MidiOutputSendError::InvalidPacket("SysEx is not supported")),
        _ => return Err(MidiOutputSendError::InvalidPacket("not a supported MIDI status byte")),
    };
    if bytes.len() != length {
        return Err(MidiOutputSendError::InvalidPacket("wrong short-message length"));
    }
    if bytes[1..].iter().any(|byte| byte & 0x80 != 0) {
        return Err(MidiOutputSendError::InvalidPacket("data byte has its high bit set"));
    }
    let mut packet = [0; 3];
    packet[..length].copy_from_slice(bytes);
    Ok((packet, u8::try_from(length).unwrap_or(3)))
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Clone on a control thread, then call `try_send_at` from realtime code. Its
/// steady-state path uses only fixed-size copies, atomics and channel `try_send`.
#[derive(Clone)]
pub struct MidiOutputSender {
    sender: Sender<ScheduledPacket>,
    state: Arc<Shared>,
    origin: Instant,
}

impl MidiOutputSender {
    /// Queue one complete 1–3 byte message for a monotonic deadline, measured
    /// in microseconds from the connected output stream's origin.
    ///
    /// A due timestamp in the past is sent as soon as the worker can process it;
    /// the worker rejects deadlines older than the most recently sent deadline
    /// rather than silently reordering a late arrival.
    pub fn try_send_at(&self, deadline_micros: u64, bytes: &[u8]) -> Result<(), MidiOutputSendError> {
        if self.state.state() != MidiOutputWorkerState::Running {
            return Err(MidiOutputSendError::Disconnected);
        }
        let (data, len) = match validate_short_packet(bytes) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.state.rejected_invalid.fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };
        if deadline_micros > elapsed_micros(self.origin).saturating_add(MAX_SCHEDULE_AHEAD_MICROS) {
            self.state.rejected_future.fetch_add(1, Ordering::Relaxed);
            return Err(MidiOutputSendError::TooFarAhead);
        }
        if !self.state.reserve_one() {
            self.state.dropped_full.fetch_add(1, Ordering::Relaxed);
            return Err(MidiOutputSendError::Full);
        }
        // The worker may have stopped after our first state check.
        if self.state.state() != MidiOutputWorkerState::Running {
            self.state.release_one();
            return Err(MidiOutputSendError::Disconnected);
        }
        // The worker assigns sequence order at channel receive time, so
        // concurrently cloned producers cannot generate reversed tie-breaks.
        let packet = ScheduledPacket { deadline_micros, sequence: 0, bytes: data, len };
        match self.sender.try_send(packet) {
            Ok(()) => {
                self.state.accepted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.state.release_one();
                self.state.dropped_full.fetch_add(1, Ordering::Relaxed);
                Err(MidiOutputSendError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.state.release_one();
                Err(MidiOutputSendError::Disconnected)
            }
        }
    }

    /// Schedule a packet for the current monotonic instant, never the audio sample clock.
    pub fn try_send_now(&self, bytes: &[u8]) -> Result<(), MidiOutputSendError> {
        self.try_send_at(self.now_micros(), bytes)
    }

    /// Current timestamp in the same origin/domain expected by `try_send_at`.
    /// The origin precedes OS connection setup and does not track playback time.
    pub fn now_micros(&self) -> u64 {
        elapsed_micros(self.origin)
    }

    pub fn status(&self) -> MidiOutputStatus {
        self.state.snapshot()
    }
}

fn output_queue(capacity: usize, initial: MidiOutputWorkerState) -> Result<(MidiOutputSender, Receiver<ScheduledPacket>), MidiOutputError> {
    if !(1..=MAX_MIDI_OUTPUT_EVENTS).contains(&capacity) {
        return Err(MidiOutputError::InvalidCapacity(capacity));
    }
    let (sender, receiver) = bounded(capacity);
    let state = Arc::new(Shared::new(capacity, initial));
    Ok((MidiOutputSender { sender, state, origin: Instant::now() }, receiver))
}

trait MidiSink {
    fn send(&mut self, packet: &[u8]) -> Result<(), String>;
}

impl<S: MidiSink + ?Sized> MidiSink for &mut S {
    fn send(&mut self, packet: &[u8]) -> Result<(), String> {
        (*self).send(packet)
    }
}

trait MidiClock {
    fn now_micros(&self) -> u64;
}

struct MonotonicClock(Instant);

impl MidiClock for MonotonicClock {
    fn now_micros(&self) -> u64 {
        elapsed_micros(self.0)
    }
}

struct OutputScheduler {
    heap: BinaryHeap<Reverse<ScheduledPacket>>,
    last_sent: Option<(u64, u64)>,
    next_sequence: u64,
}

impl OutputScheduler {
    fn new(capacity: usize) -> Self {
        Self { heap: BinaryHeap::with_capacity(capacity), last_sent: None, next_sequence: 0 }
    }

    fn push(&mut self, mut packet: ScheduledPacket, state: &Shared) {
        packet.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self.last_sent.is_some_and(|last| packet.order() <= last) {
            state.dropped_out_of_order.fetch_add(1, Ordering::Relaxed);
            state.release_one();
        } else {
            // The producer reserves capacity across channel + heap before send.
            self.heap.push(Reverse(packet));
        }
    }

    fn flush_due(&mut self, clock: &impl MidiClock, sink: &mut impl MidiSink, state: &Shared) -> bool {
        loop {
            let now = clock.now_micros();
            let Some(Reverse(packet)) = self.heap.peek().copied() else {
                return true;
            };
            if packet.deadline_micros > now {
                return true;
            }
            let _ = self.heap.pop();
            if self.last_sent.is_some_and(|last| packet.order() <= last) {
                state.dropped_out_of_order.fetch_add(1, Ordering::Relaxed);
                state.release_one();
                continue;
            }
            if let Err(reason) = sink.send(packet.bytes()) {
                state.send_errors.fetch_add(1, Ordering::Relaxed);
                state.release_one();
                state.fail(reason);
                return false;
            }
            self.last_sent = Some(packet.order());
            state.sent.fetch_add(1, Ordering::Relaxed);
            if now > packet.deadline_micros {
                state.sent_late.fetch_add(1, Ordering::Relaxed);
            }
            state.release_one();
        }
    }

    fn next_wait(&self, now_micros: u64) -> Duration {
        let until_due = self.heap.peek().map_or(WORKER_POLL_MICROS, |Reverse(packet)| packet.deadline_micros.saturating_sub(now_micros));
        Duration::from_micros(until_due.clamp(1, WORKER_POLL_MICROS))
    }
}

fn run_worker(mut sink: impl MidiSink, clock: impl MidiClock, receiver: Receiver<ScheduledPacket>, state: Arc<Shared>) {
    let mut scheduler = OutputScheduler::new(state.capacity);
    while !state.shutdown.load(Ordering::Acquire) && state.state() == MidiOutputWorkerState::Running {
        // Bound each drain pass so a fast producer cannot starve due sends.
        for _ in 0..state.capacity {
            match receiver.try_recv() {
                Ok(packet) => scheduler.push(packet, &state),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if !scheduler.flush_due(&clock, &mut sink, &state) {
            break;
        }
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        match receiver.recv_timeout(scheduler.next_wait(clock.now_micros())) {
            Ok(packet) => scheduler.push(packet, &state),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    if state.state() != MidiOutputWorkerState::Failed {
        state.worker.store(MidiOutputWorkerState::Stopped as u8, Ordering::Release);
    }
    // Disconnect before clearing reservations: no producer can enqueue after
    // the receiver is dropped, including producers racing the state transition.
    drop(receiver);
    let cancelled = state.in_flight.swap(0, Ordering::AcqRel);
    state.cancelled.fetch_add(u64::try_from(cancelled).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// Connected native stream; keep on a control thread, and clone `sender()` into
/// the playback pipeline before entering a realtime callback.
/// Dropping it requests asynchronous worker shutdown without joining/blocking.
pub struct MidiOutputStream {
    port: MidiOutputPortInfo,
    sender: MidiOutputSender,
    _worker: JoinHandle<()>,
}

impl MidiOutputStream {
    pub fn port(&self) -> &MidiOutputPortInfo {
        &self.port
    }

    pub fn sender(&self) -> MidiOutputSender {
        self.sender.clone()
    }

    pub fn status(&self) -> MidiOutputStatus {
        self.sender.status()
    }

    /// A best-effort hot-plug check. Port status can change immediately after
    /// this call; the send worker still reports errors via `status()`.
    pub fn port_still_present(&self) -> Result<bool, MidiOutputError> {
        let ports = output_ports()?;
        Ok(validate_port_identity(&self.port, &ports).is_ok())
    }
}

impl Drop for MidiOutputStream {
    fn drop(&mut self) {
        self.sender.state.shutdown.store(true, Ordering::Release);
        // JoinHandle drops without joining; a stalled OS MIDI driver never
        // blocks the audio or UI thread that releases a stream.
    }
}

fn validate_port_identity(selected: &MidiOutputPortInfo, ports: &[MidiOutputPortInfo]) -> Result<(), MidiOutputError> {
    let current = ports.iter().find(|port| port.index == selected.index);
    let Some(current) = current else {
        return Err(MidiOutputError::UnavailablePort(selected.index));
    };
    if current.name != selected.name || current.id != selected.id {
        return Err(MidiOutputError::StalePort { index: selected.index });
    }
    if ports.iter().filter(|port| port.name == selected.name).take(2).count() > 1 {
        return Err(MidiOutputError::AmbiguousPort(selected.name.clone()));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn describe_ports(output: &midir::MidiOutput, ports: &[midir::MidiOutputPort]) -> Result<Vec<MidiOutputPortInfo>, MidiOutputError> {
    if ports.len() > MAX_OUTPUT_PORTS {
        return Err(MidiOutputError::TooManyPorts);
    }
    ports
        .iter()
        .enumerate()
        .map(|(index, port)| {
            output
                .port_name(port)
                .map(|name| MidiOutputPortInfo { index, name, id: port.id() })
                .map_err(|error| MidiOutputError::Backend(error.to_string()))
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn output_ports() -> Result<Vec<MidiOutputPortInfo>, MidiOutputError> {
    let output = midir::MidiOutput::new("SoundCraft MIDI Output Scan").map_err(|error| MidiOutputError::Backend(error.to_string()))?;
    describe_ports(&output, &output.ports())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn output_ports() -> Result<Vec<MidiOutputPortInfo>, MidiOutputError> {
    Err(MidiOutputError::Unsupported)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn connect_output(selected: &MidiOutputPortInfo) -> Result<midir::MidiOutputConnection, MidiOutputError> {
    let output = midir::MidiOutput::new("SoundCraft MIDI Output").map_err(|error| MidiOutputError::Backend(error.to_string()))?;
    // Validate and connect using the *same* midir port object snapshot. Another
    // scan between these steps could silently retarget an index after hot-plug.
    let ports = output.ports();
    let discovered = describe_ports(&output, &ports)?;
    validate_port_identity(selected, &discovered)?;
    let port = ports.get(selected.index).ok_or(MidiOutputError::UnavailablePort(selected.index))?;
    output.connect(port, "SoundCraft MIDI Send").map_err(|error| MidiOutputError::Connection(error.to_string()))
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl MidiSink for midir::MidiOutputConnection {
    fn send(&mut self, packet: &[u8]) -> Result<(), String> {
        midir::MidiOutputConnection::send(self, packet).map_err(|error| error.to_string())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn open_output(selected: &MidiOutputPortInfo, capacity: usize) -> Result<MidiOutputStream, MidiOutputError> {
    if selected.index >= MAX_OUTPUT_PORTS {
        return Err(MidiOutputError::UnavailablePort(selected.index));
    }
    let (sender, receiver) = output_queue(capacity, MidiOutputWorkerState::Starting)?;
    let state = Arc::clone(&sender.state);
    let port = selected.clone();
    let worker_port = port.clone();
    let origin = sender.origin;
    let (ready_sender, ready_receiver) = bounded(1);
    let worker = thread::Builder::new()
        .name("soundcraft-midi-output".into())
        .spawn(move || match connect_output(&worker_port) {
            Ok(connection) => {
                state.worker.store(MidiOutputWorkerState::Running as u8, Ordering::Release);
                if ready_sender.send(Ok(())).is_ok() {
                    run_worker(connection, MonotonicClock(origin), receiver, state);
                }
            }
            Err(error) => {
                state.fail(error.to_string());
                let _ = ready_sender.send(Err(error));
            }
        })
        .map_err(|error| MidiOutputError::Worker(error.to_string()))?;
    match ready_receiver.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(())) => Ok(MidiOutputStream { port, sender, _worker: worker }),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            sender.state.shutdown.store(true, Ordering::Release);
            Err(MidiOutputError::ConnectionTimeout)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn open_output(_selected: &MidiOutputPortInfo, _capacity: usize) -> Result<MidiOutputStream, MidiOutputError> {
    Err(MidiOutputError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn set(&self, micros: u64) {
            self.0.store(micros, Ordering::Relaxed);
        }
    }

    impl MidiClock for FakeClock {
        fn now_micros(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[derive(Default)]
    struct FakeSink {
        packets: Vec<Vec<u8>>,
        fail_after: Option<usize>,
    }

    impl MidiSink for FakeSink {
        fn send(&mut self, packet: &[u8]) -> Result<(), String> {
            if self.fail_after.is_some_and(|limit| self.packets.len() >= limit) {
                return Err("fake device unplugged".into());
            }
            self.packets.push(packet.to_vec());
            Ok(())
        }
    }

    fn fake_output(capacity: usize) -> (MidiOutputSender, Receiver<ScheduledPacket>) {
        let Ok(queue) = output_queue(capacity, MidiOutputWorkerState::Running) else { panic!("valid synthetic output channel") };
        queue
    }

    fn drain_to_heap(receiver: &Receiver<ScheduledPacket>, scheduler: &mut OutputScheduler, shared: &Shared) {
        while let Ok(packet) = receiver.try_recv() {
            scheduler.push(packet, shared);
        }
    }

    #[test]
    fn limits_capacity_and_rejects_invalid_packets_without_enqueuing() {
        assert!(matches!(output_queue(0, MidiOutputWorkerState::Running), Err(MidiOutputError::InvalidCapacity(0))));
        assert!(matches!(output_queue(MAX_MIDI_OUTPUT_EVENTS + 1, MidiOutputWorkerState::Running), Err(MidiOutputError::InvalidCapacity(_))));
        let (sender, receiver) = fake_output(4);
        for invalid in [
            &[][..],
            &[60, 100][..],
            &[0x90, 60][..],
            &[0x80, 61, 0x90][..],
            &[0x90, 60, 99, 62][..],
            &[0xF0, 1][..],
            &[0xF7][..],
            &[0xF9][..],
            &[0xFD][..],
            &[0xF4][..],
        ] {
            assert!(matches!(sender.try_send_at(1, invalid), Err(MidiOutputSendError::InvalidPacket(_))));
        }
        assert_eq!(sender.status().rejected_invalid, 10);
        assert_eq!(sender.status().in_flight, 0);
        assert!(receiver.try_recv().is_err());

        for valid in [&[0x90, 60, 100][..], &[0xC2, 8][..], &[0xF2, 0, 1][..], &[0xF8][..]] {
            assert!(sender.try_send_at(1, valid).is_ok());
        }
        assert_eq!(receiver.len(), 4);
        let first = receiver.try_recv();
        assert_eq!(first.ok().map(|packet| packet.bytes().to_vec()), Some(vec![0x90, 60, 100]));
        let last = receiver.try_recv();
        assert_eq!(last.ok().map(|packet| packet.bytes().to_vec()), Some(vec![0xC2, 8]));
    }

    #[test]
    fn supports_each_defined_one_byte_realtime_status() {
        for byte in [0xF8, 0xFA, 0xFB, 0xFC, 0xFE, 0xFF] {
            assert_eq!(validate_short_packet(&[byte]), Ok(([byte, 0, 0], 1)));
        }
        assert_eq!(validate_short_packet(&[0xF6]), Ok(([0xF6, 0, 0], 1)));
        assert_eq!(validate_short_packet(&[0xF1, 3]), Ok(([0xF1, 3, 0], 2)));
        assert_eq!(validate_short_packet(&[0xF3, 6]), Ok(([0xF3, 6, 0], 2)));
    }

    #[test]
    fn worker_heap_and_channel_share_one_hard_capacity() {
        let (sender, receiver) = fake_output(2);
        assert_eq!(sender.try_send_at(40, &[0x91, 60, 100]), Ok(()));
        assert_eq!(sender.try_send_at(30, &[0x91, 61, 100]), Ok(()));
        let mut scheduler = OutputScheduler::new(2);
        drain_to_heap(&receiver, &mut scheduler, &sender.state);
        assert!(receiver.is_empty());
        assert_eq!(sender.status().in_flight, 2);
        assert_eq!(sender.try_send_at(50, &[0x91, 62, 100]), Err(MidiOutputSendError::Full));
        assert_eq!(sender.status().dropped_full, 1);

        let clock = FakeClock::default();
        let mut sink = FakeSink::default();
        clock.set(35);
        assert!(scheduler.flush_due(&clock, &mut sink, &sender.state));
        assert_eq!(sink.packets, vec![vec![0x91, 61, 100]]);
        assert_eq!(sender.status().in_flight, 1);
        assert_eq!(sender.try_send_at(50, &[0x91, 62, 100]), Ok(()));
        drain_to_heap(&receiver, &mut scheduler, &sender.state);
        clock.set(60);
        assert!(scheduler.flush_due(&clock, &mut sink, &sender.state));
        assert_eq!(sink.packets, vec![vec![0x91, 61, 100], vec![0x91, 60, 100], vec![0x91, 62, 100]]);
        assert_eq!(sender.status().in_flight, 0);
    }

    #[test]
    fn fake_clock_sorts_deadlines_and_keeps_fifo_for_equal_timestamps() {
        let (sender, receiver) = fake_output(5);
        for (at, pitch) in [(90, 70), (20, 60), (20, 61), (50, 65), (20, 62)] {
            assert_eq!(sender.try_send_at(at, &[0x90, pitch, 100]), Ok(()));
        }
        let mut scheduler = OutputScheduler::new(5);
        drain_to_heap(&receiver, &mut scheduler, &sender.state);
        let clock = FakeClock::default();
        let mut sink = FakeSink::default();
        clock.set(21);
        assert!(scheduler.flush_due(&clock, &mut sink, &sender.state));
        assert_eq!(sink.packets, vec![vec![0x90, 60, 100], vec![0x90, 61, 100], vec![0x90, 62, 100]]);
        clock.set(90);
        assert!(scheduler.flush_due(&clock, &mut sink, &sender.state));
        assert_eq!(sink.packets[3..], [vec![0x90, 65, 100], vec![0x90, 70, 100]]);
        assert_eq!(sender.status().sent, 5);
        assert_eq!(sender.status().in_flight, 0);
    }

    #[test]
    fn late_arrivals_never_reorder_already_emitted_deadlines() {
        let (sender, receiver) = fake_output(3);
        let clock = FakeClock::default();
        let mut sink = FakeSink::default();
        let mut scheduler = OutputScheduler::new(3);
        assert!(sender.try_send_at(30, &[0x90, 70, 1]).is_ok());
        drain_to_heap(&receiver, &mut scheduler, &sender.state);
        clock.set(30);
        assert!(scheduler.flush_due(&clock, &mut sink, &sender.state));
        assert!(sender.try_send_at(20, &[0x90, 60, 1]).is_ok());
        drain_to_heap(&receiver, &mut scheduler, &sender.state);
        assert_eq!(sender.status().dropped_out_of_order, 1);
        assert_eq!(sender.status().in_flight, 0);
        assert_eq!(sink.packets, vec![vec![0x90, 70, 1]]);
    }

    #[test]
    fn rejects_far_future_deadline_without_reserving_slots() {
        let (sender, receiver) = fake_output(1);
        let future = MAX_SCHEDULE_AHEAD_MICROS + 10_000_000;
        assert_eq!(sender.try_send_at(future, &[0xFC]), Err(MidiOutputSendError::TooFarAhead));
        assert_eq!(sender.status().rejected_future, 1);
        assert_eq!(sender.status().in_flight, 0);
        assert!(receiver.is_empty());
    }

    #[test]
    fn fake_worker_reports_unplug_error_and_discards_remaining_schedule() {
        let (sender, receiver) = fake_output(4);
        for pitch in [60, 61, 62] {
            assert_eq!(sender.try_send_at(0, &[0x90, pitch, 110]), Ok(()));
        }
        let state = Arc::clone(&sender.state);
        let mut sink = FakeSink { packets: Vec::new(), fail_after: Some(1) };
        let clock = FakeClock::default();
        clock.set(1);
        let worker = thread::spawn(move || {
            run_worker(&mut sink, clock, receiver, state);
            sink
        });
        let Ok(sink) = worker.join() else { panic!("synthetic worker must not panic") };
        assert_eq!(sink.packets, vec![vec![0x90, 60, 110]]);
        let status = sender.status();
        assert_eq!(status.worker, MidiOutputWorkerState::Failed);
        assert_eq!(status.sent, 1);
        assert_eq!(status.send_errors, 1);
        assert_eq!(status.cancelled, 1);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.last_error.as_deref(), Some("fake device unplugged"));
        assert_eq!(sender.try_send_now(&[0x80, 60, 0]), Err(MidiOutputSendError::Disconnected));
    }

    #[test]
    fn shutdown_cancels_far_future_queued_packets_without_waiting_on_due_time() {
        let (sender, receiver) = fake_output(2);
        assert_eq!(sender.try_send_at(55_000_000, &[0x90, 60, 100]), Ok(()));
        sender.state.shutdown.store(true, Ordering::Release);
        let state = Arc::clone(&sender.state);
        run_worker(FakeSink::default(), FakeClock::default(), receiver, state);
        assert_eq!(sender.status().worker, MidiOutputWorkerState::Stopped);
        assert_eq!(sender.status().in_flight, 0);
        assert_eq!(sender.status().cancelled, 1);
    }

    #[test]
    fn validates_output_identity_against_reorder_disappearance_and_duplicate_names() {
        let selection = MidiOutputPortInfo { index: 1, name: "Synth B".into(), id: "driver:22".into() };
        let mut scanned = vec![MidiOutputPortInfo { index: 0, name: "Synth A".into(), id: "driver:21".into() }, selection.clone()];
        assert_eq!(validate_port_identity(&selection, &scanned), Ok(()));
        scanned[1].name = "Synth C".into();
        assert!(matches!(validate_port_identity(&selection, &scanned), Err(MidiOutputError::StalePort { .. })));
        scanned[1] = selection.clone();
        scanned[1].id = "replacement:22".into();
        assert!(matches!(validate_port_identity(&selection, &scanned), Err(MidiOutputError::StalePort { .. })));
        let _ = scanned.pop();
        assert!(matches!(validate_port_identity(&selection, &scanned), Err(MidiOutputError::UnavailablePort(1))));
        scanned.push(selection.clone());
        scanned.push(MidiOutputPortInfo { index: 2, name: "Synth B".into(), id: "driver:23".into() });
        assert_eq!(validate_port_identity(&selection, &scanned), Err(MidiOutputError::AmbiguousPort("Synth B".into())));
    }
}
