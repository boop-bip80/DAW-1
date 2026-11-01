//! Explicitly opted-in, native MIDI playback scheduling outside the audio callback.
//!
//! Saved `Track::midi_output` routes are inert until the user enables external
//! transmission for this application launch. The worker reads an immutable
//! session snapshot and an atomic `TransportObserver`, collects bounded
//! half-open sample windows, and queues short packets on the existing bounded
//! MIDI output workers. No MIDI driver, queue, heap or session scan is touched
//! by the realtime audio callback.
//!
//! A device's monotonic microseconds and the DAW's absolute session samples
//! have no measured common origin. The lookahead mapping is best effort, **not
//! sample accurate**, and never represents latency calibration or hardware
//! validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use soundcraft_midi::output::{
    MidiOutputPortInfo, MidiOutputSendError, MidiOutputSender, MidiOutputStream, MidiOutputWorkerState, open_output, output_ports,
};
use soundcraft_model::Session;
use soundcraft_time::{MAX_POSITION, Range, Samples};
use thiserror::Error;

use crate::{TransportObserver, TransportSnapshot, midi_out::collect_midi_output_window};

const MAX_ACTIVE_PORTS: usize = 8;
const PORT_EVENT_CAPACITY: usize = 4_096;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const WINDOW_AHEAD_MICROS: f64 = 25_000.0;
const MAX_WINDOW_LAG_MICROS: f64 = 250_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiOutputRuntimePhase {
    Disabled,
    Connecting,
    Ready,
    Sending,
    Disabling,
    Failed,
}

/// Read-only control/UI snapshot, never queried from the audio callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputRuntimeStatus {
    /// Explicit per-launch opt-in, always false on startup and never persisted.
    pub enabled: bool,
    pub phase: MidiOutputRuntimePhase,
    pub connected_ports: Vec<String>,
    pub scheduled_events: u64,
    pub dropped_events: u64,
    pub cleanup_pending: bool,
    pub last_error: Option<String>,
}

impl Default for MidiOutputRuntimeStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            phase: MidiOutputRuntimePhase::Disabled,
            connected_ports: Vec::new(),
            scheduled_events: 0,
            dropped_events: 0,
            cleanup_pending: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum MidiOutputRuntimeError {
    #[error("select a named output on a MIDI or instrument track before enabling external MIDI")]
    NoSelectedRoute,
    #[error("a maximum of {MAX_ACTIVE_PORTS} simultaneous MIDI output ports is supported")]
    TooManyPorts,
    #[error("MIDI output port {0:?} is unavailable; rescan and select it again")]
    Unavailable(String),
    #[error("MIDI output port {0:?} is ambiguous; identical device names cannot be routed safely")]
    Ambiguous(String),
    #[error("MIDI output device error: {0}")]
    Device(String),
    #[error("MIDI output scheduler is still cleaning up; wait before enabling again")]
    CleaningUp,
    #[error("MIDI output runtime is not available: {0}")]
    Runtime(String),
}

fn routed_port_names(session: &Session) -> Result<BTreeSet<String>, MidiOutputRuntimeError> {
    let mut names = BTreeSet::new();
    for track in &session.tracks {
        if !track.kind.is_midi() {
            continue;
        }
        if let Some(route) = &track.midi_output {
            let name = route.port.trim();
            if name.is_empty() {
                return Err(MidiOutputRuntimeError::Unavailable(route.port.clone()));
            }
            names.insert(name.to_owned());
            if names.len() > MAX_ACTIVE_PORTS {
                return Err(MidiOutputRuntimeError::TooManyPorts);
            }
        }
    }
    if names.is_empty() {
        return Err(MidiOutputRuntimeError::NoSelectedRoute);
    }
    Ok(names)
}

fn preflight_port(name: &str, discovered: &[MidiOutputPortInfo]) -> Result<MidiOutputPortInfo, MidiOutputRuntimeError> {
    let normalized = name.trim().to_lowercase();
    if discovered.iter().filter(|port| port.name.trim().to_lowercase() == normalized).take(2).count() > 1 {
        return Err(MidiOutputRuntimeError::Ambiguous(name.to_owned()));
    }
    let mut matches = discovered.iter().filter(|port| port.name.trim() == name);
    let Some(port) = matches.next() else { return Err(MidiOutputRuntimeError::Unavailable(name.to_owned())) };
    if matches.next().is_some() {
        return Err(MidiOutputRuntimeError::Ambiguous(name.to_owned()));
    }
    Ok(port.clone())
}

/// Minimal dispatcher interface shared by the native midir worker and
/// deterministic fake ports. No sink send occurs in the realtime callback.
trait MidiPort {
    fn now_micros(&self) -> u64;
    fn try_send_at(&mut self, micros: u64, packet: &[u8]) -> Result<(), MidiOutputSendError>;
    fn cancel_and_release(&mut self) -> Result<u64, MidiOutputSendError>;
    fn worker_state(&self) -> MidiOutputWorkerState;
    fn acknowledged_generation(&self) -> u64;
}

struct NativePort {
    sender: MidiOutputSender,
    _stream: MidiOutputStream,
}

impl MidiPort for NativePort {
    fn now_micros(&self) -> u64 {
        self.sender.now_micros()
    }

    fn try_send_at(&mut self, micros: u64, packet: &[u8]) -> Result<(), MidiOutputSendError> {
        self.sender.try_send_at(micros, packet)
    }

    fn cancel_and_release(&mut self) -> Result<u64, MidiOutputSendError> {
        self.sender.cancel_and_release()
    }

    fn worker_state(&self) -> MidiOutputWorkerState {
        self.sender.status().worker
    }

    fn acknowledged_generation(&self) -> u64 {
        self.sender.status().acknowledged_generation
    }
}

fn opened_ports(session: &Session) -> Result<BTreeMap<String, NativePort>, MidiOutputRuntimeError> {
    let names = routed_port_names(session)?;
    // One scan, on the scheduling thread, only after explicit enable. The
    // output layer checks the chosen index/name/driver ID again upon open.
    let discovered = output_ports().map_err(|error| MidiOutputRuntimeError::Device(error.to_string()))?;
    let selected: Vec<_> = names.iter().map(|name| preflight_port(name, &discovered)).collect::<Result<_, _>>()?;
    let mut opened = BTreeMap::new();
    for (name, port) in names.into_iter().zip(selected) {
        let stream = open_output(&port, PORT_EVENT_CAPACITY).map_err(|error| MidiOutputRuntimeError::Device(error.to_string()))?;
        opened.insert(name, NativePort { sender: stream.sender(), _stream: stream });
    }
    Ok(opened)
}

fn ahead_samples(session: &Session, speed: f32, micros: f64) -> Samples {
    (session.sample_rate.as_f64() * f64::from(speed) * micros / 1_000_000.0).ceil().clamp(1.0, 1_000_000.0) as Samples
}

fn deadline_micros(base: u64, observed: TransportSnapshot, at_samples: Samples, sample_rate: f64) -> u64 {
    let ahead = (i128::from(at_samples) - i128::from(observed.position)).max(0) as f64;
    let delta = (ahead * 1_000_000.0 / (sample_rate * f64::from(observed.speed))).round().clamp(0.0, 60_000_000.0) as u64;
    base.saturating_add(delta)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tick {
    Idle,
    WaitingForCleanup,
    Queued,
}

/// One per explicit opt-in. Any unrecoverable dispatch/collection failure
/// freezes this planner until the user disables and explicitly enables again.
struct WindowScheduler<P: MidiPort> {
    ports: BTreeMap<String, P>,
    generation: Option<u64>,
    session_revision: Option<u64>,
    cursor: Samples,
    pending: BTreeMap<String, u64>,
    blocked: bool,
    scheduled: u64,
    dropped: u64,
    last_error: Option<String>,
}

impl<P: MidiPort> WindowScheduler<P> {
    fn new(ports: BTreeMap<String, P>) -> Self {
        Self {
            ports,
            generation: None,
            session_revision: None,
            cursor: 0,
            pending: BTreeMap::new(),
            blocked: false,
            scheduled: 0,
            dropped: 0,
            last_error: None,
        }
    }

    fn cancel_all(&mut self) -> Result<(), String> {
        self.pending.clear();
        let mut first_error = None;
        for (name, port) in &mut self.ports {
            match port.cancel_and_release() {
                Ok(generation) => {
                    self.pending.insert(name.clone(), generation);
                }
                Err(error) if first_error.is_none() => first_error = Some(format!("{name}: {error}")),
                Err(_) => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn cleanup_complete(&mut self) -> Result<bool, String> {
        for (name, generation) in &self.pending {
            let Some(port) = self.ports.get(name) else { return Err(format!("{name}: output stream disappeared during cleanup")) };
            if port.worker_state() != MidiOutputWorkerState::Running {
                return Err(format!("{name}: MIDI output worker stopped before note cleanup was acknowledged"));
            }
            if port.acknowledged_generation() < *generation {
                return Ok(false);
            }
        }
        self.pending.clear();
        Ok(true)
    }

    fn fail(&mut self, reason: String) -> String {
        self.dropped = self.dropped.saturating_add(1);
        self.last_error = Some(reason.clone());
        self.blocked = true;
        // Best-effort release of any notes already accepted on a partially
        // sent window; do not retry and duplicate that window's NoteOns.
        let _ = self.cancel_all();
        reason
    }

    fn tick(
        &mut self,
        session: &Session,
        session_revision: u64,
        observed: TransportSnapshot,
        may_dispatch: impl Fn() -> bool,
    ) -> Result<Tick, String> {
        if self.blocked {
            return Err(self.last_error.clone().unwrap_or_else(|| "MIDI output scheduler stopped".into()));
        }
        if self.generation != Some(observed.generation) || self.session_revision != Some(session_revision) {
            // A new persisted route is not implicit permission to connect a
            // new physical port. Only names selected when the user opted in
            // can emit; require disable/re-enable after a route change.
            match routed_port_names(session) {
                Ok(selected) => {
                    if let Some(unapproved) = selected.iter().find(|name| !self.ports.contains_key(*name)) {
                        return Err(self.fail(format!("new MIDI output {unapproved:?} requires fresh explicit opt-in")));
                    }
                }
                Err(MidiOutputRuntimeError::NoSelectedRoute) => {}
                Err(error) => return Err(self.fail(error.to_string())),
            }
            self.generation = Some(observed.generation);
            self.session_revision = Some(session_revision);
            self.cursor = observed.position.max(0);
            self.cancel_all().map_err(|error| self.fail(error))?;
            return Ok(Tick::WaitingForCleanup);
        }
        if !self.cleanup_complete().map_err(|error| self.fail(error))? {
            return Ok(Tick::WaitingForCleanup);
        }
        if let Some(name) = self.ports.iter().find(|(_, port)| port.worker_state() != MidiOutputWorkerState::Running).map(|(name, _)| name.clone()) {
            return Err(self.fail(format!("{name}: external MIDI output disconnected")));
        }
        if !observed.playing {
            return Ok(Tick::Idle);
        }
        if !observed.speed.is_finite() || observed.speed <= 0.0 || observed.position < 0 {
            return Err(self.fail("invalid playback transport observation".into()));
        }
        let rate = session.sample_rate.as_f64();
        let lookahead = ahead_samples(session, observed.speed, WINDOW_AHEAD_MICROS);
        let max_lag = ahead_samples(session, observed.speed, MAX_WINDOW_LAG_MICROS);
        if observed.position.saturating_sub(self.cursor) > max_lag {
            self.cursor = observed.position;
            self.cancel_all().map_err(|error| self.fail(error))?;
            self.dropped = self.dropped.saturating_add(1);
            self.last_error = Some("MIDI output fell over 250 ms behind and resynchronized; some events may be missing".into());
            return Ok(Tick::WaitingForCleanup);
        }
        let horizon = observed.position.saturating_add(lookahead).min(MAX_POSITION);
        if horizon <= self.cursor {
            return Ok(Tick::Queued);
        }
        let events = collect_midi_output_window(session, Range::new(self.cursor, horizon)).map_err(|error| self.fail(error.to_string()))?;
        // Capture each stream's monotonic now once per tick. Repeated deadlines
        // within the same sample stay equal and preserve collector FIFO order.
        let bases: BTreeMap<_, _> = self.ports.iter().map(|(name, port)| (name.clone(), port.now_micros())).collect();
        for event in &events {
            if !may_dispatch() {
                self.cancel_all().map_err(|error| self.fail(error))?;
                return Ok(Tick::WaitingForCleanup);
            }
            let name = session
                .track(event.track)
                .and_then(|track| track.midi_output.as_ref())
                .map(|route| route.port.trim())
                .ok_or_else(|| self.fail("MIDI output track route disappeared during collection".into()))?;
            let Some(base) = bases.get(name) else {
                return Err(self.fail(format!("new MIDI output {name:?} is not part of the user-approved ports; disable and re-enable")));
            };
            let at = deadline_micros(*base, observed, event.at_samples, rate);
            let Some(port) = self.ports.get_mut(name) else { return Err(self.fail(format!("MIDI output {name:?} disconnected"))) };
            if let Err(error) = port.try_send_at(at, event.message()) {
                return Err(self.fail(format!("{name}: {error}; MIDI window cancelled rather than partially retried")));
            }
            self.scheduled = self.scheduled.saturating_add(1);
        }
        self.cursor = horizon;
        Ok(Tick::Queued)
    }
}

struct RuntimeControl {
    /// All access is control/scheduler-thread only, never the audio callback.
    session: Mutex<(Arc<Session>, u64)>,
    desired_enabled: AtomicBool,
    request_revision: AtomicU64,
    shutdown: AtomicBool,
    status: Mutex<MidiOutputRuntimeStatus>,
}

impl RuntimeControl {
    fn session_snapshot(&self) -> Result<(Arc<Session>, u64), MidiOutputRuntimeError> {
        self.session
            .lock()
            .map(|slot| (Arc::clone(&slot.0), slot.1))
            .map_err(|_| MidiOutputRuntimeError::Runtime("session handoff lock was poisoned".into()))
    }

    fn publish(&self, f: impl FnOnce(&mut MidiOutputRuntimeStatus)) {
        if let Ok(mut status) = self.status.lock() {
            f(&mut status);
        }
    }
}

/// External MIDI is disabled on each app launch. Only explicit control-thread
/// construction of this runtime can discover/open a port; a stored route does
/// nothing by itself. Dropping the handle requests asynchronous cancellation
/// without joining or blocking the UI or audio callback.
pub struct MidiOutputRuntime {
    control: Arc<RuntimeControl>,
    _worker: JoinHandle<()>,
}

impl MidiOutputRuntime {
    /// Start the separate native scheduler only after explicit user consent.
    /// Device discovery and connection happen on its worker, not this caller.
    pub fn start_enabled(session: Arc<Session>, observer: TransportObserver) -> Result<Self, MidiOutputRuntimeError> {
        routed_port_names(&session)?;
        let control = Arc::new(RuntimeControl {
            session: Mutex::new((session, 0)),
            desired_enabled: AtomicBool::new(true),
            request_revision: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            status: Mutex::new(MidiOutputRuntimeStatus {
                enabled: true,
                phase: MidiOutputRuntimePhase::Connecting,
                ..MidiOutputRuntimeStatus::default()
            }),
        });
        let thread_control = Arc::clone(&control);
        let worker = thread::Builder::new()
            .name("soundcraft-midi-playback".into())
            .spawn(move || run_scheduler_thread(thread_control, observer))
            .map_err(|error| MidiOutputRuntimeError::Runtime(error.to_string()))?;
        Ok(Self { control, _worker: worker })
    }

    /// Enable again only after the prior stream cleanup has completed.
    /// `false` always requests panic/cleanup, even during playback or a
    /// slow OS connection attempt. It never waits for the device.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), MidiOutputRuntimeError> {
        let status = self.status();
        if enabled && status.phase == MidiOutputRuntimePhase::Disabling {
            return Err(MidiOutputRuntimeError::CleaningUp);
        }
        if enabled && status.phase == MidiOutputRuntimePhase::Failed {
            return Err(MidiOutputRuntimeError::Runtime("disable external MIDI before retrying; a failed panic may require restarting".into()));
        }
        if enabled {
            let (session, _) = self.control.session_snapshot()?;
            routed_port_names(&session)?;
        }
        if self.control.desired_enabled.swap(enabled, Ordering::AcqRel) != enabled {
            self.control.request_revision.fetch_add(1, Ordering::AcqRel);
            self.control.publish(|status| {
                status.enabled = enabled;
                status.phase = if enabled { MidiOutputRuntimePhase::Connecting } else { MidiOutputRuntimePhase::Disabling };
                status.cleanup_pending = !enabled;
                if enabled {
                    status.last_error = None;
                }
            });
        }
        Ok(())
    }

    /// Replace the immutable source for *future* worker-thread windows. The
    /// new session and revision are one mutex-protected handoff: the worker
    /// never reads a new document under an old handoff revision.
    pub fn update_session(&self, session: Arc<Session>) -> Result<(), MidiOutputRuntimeError> {
        let mut slot = self.control.session.lock().map_err(|_| MidiOutputRuntimeError::Runtime("session handoff lock was poisoned".into()))?;
        slot.0 = session;
        slot.1 = slot.1.wrapping_add(1);
        Ok(())
    }

    /// Cheap control/UI status; it neither rescans ports nor opens hardware.
    pub fn status(&self) -> MidiOutputRuntimeStatus {
        let mut status = self.control.status.lock().map(|guard| guard.clone()).unwrap_or_else(|_| MidiOutputRuntimeStatus {
            phase: MidiOutputRuntimePhase::Failed,
            last_error: Some("MIDI output status lock was poisoned".into()),
            ..MidiOutputRuntimeStatus::default()
        });
        status.enabled = self.control.desired_enabled.load(Ordering::Acquire);
        status
    }
}

impl Drop for MidiOutputRuntime {
    fn drop(&mut self) {
        self.control.desired_enabled.store(false, Ordering::Release);
        self.control.request_revision.fetch_add(1, Ordering::AcqRel);
        self.control.shutdown.store(true, Ordering::Release);
        // Never join a possibly stalled OS MIDI driver from a UI/audio thread.
    }
}

fn publish_scheduler_status(control: &RuntimeControl, scheduler: &WindowScheduler<NativePort>, phase: MidiOutputRuntimePhase) {
    control.publish(|status| {
        status.enabled = control.desired_enabled.load(Ordering::Acquire);
        status.phase = phase;
        status.scheduled_events = scheduler.scheduled;
        status.dropped_events = scheduler.dropped;
        status.cleanup_pending = !scheduler.pending.is_empty();
        status.last_error = scheduler.last_error.clone();
    });
}

fn run_scheduler_thread(control: Arc<RuntimeControl>, observer: TransportObserver) {
    let mut scheduler: Option<WindowScheduler<NativePort>> = None;
    let mut active_request: u64 = 0;
    let mut frozen_request: Option<u64> = None;
    let mut stopping = false;
    let mut cleanup_failed = false;
    loop {
        let shutting_down = control.shutdown.load(Ordering::Acquire);
        let enabled = control.desired_enabled.load(Ordering::Acquire) && !shutting_down;
        let requested = control.request_revision.load(Ordering::Acquire);
        if let Some(plan) = &mut scheduler
            && (!enabled || requested != active_request || stopping)
        {
            if !stopping {
                stopping = true;
                let cancel_error = plan.cancel_all().err();
                control.publish(|status| {
                    status.phase = MidiOutputRuntimePhase::Disabling;
                    status.cleanup_pending = true;
                    if let Some(error) = cancel_error {
                        status.last_error = Some(format!("MIDI note cleanup could not be requested: {error}"));
                    }
                });
            }
            match plan.cleanup_complete() {
                Ok(true) => {
                    scheduler = None;
                    stopping = false;
                    control.publish(|status| {
                        status.phase = if enabled { MidiOutputRuntimePhase::Connecting } else { MidiOutputRuntimePhase::Disabled };
                        status.connected_ports.clear();
                        status.cleanup_pending = false;
                    });
                }
                Ok(false) => {}
                Err(error) => {
                    // Do not silently reopen another connection after a failed
                    // hardware panic. The stream is dropped without joining.
                    cleanup_failed = true;
                    scheduler = None;
                    stopping = false;
                    control.publish(|status| {
                        status.phase = MidiOutputRuntimePhase::Failed;
                        status.cleanup_pending = false;
                        status.last_error = Some(format!("{error}; external notes may still be sounding"));
                    });
                }
            }
        } else if !enabled {
            if !cleanup_failed {
                control.publish(|status| {
                    status.phase = MidiOutputRuntimePhase::Disabled;
                    status.connected_ports.clear();
                    status.cleanup_pending = false;
                });
            }
        } else if scheduler.is_none() && !cleanup_failed && frozen_request != Some(requested) {
            active_request = requested;
            control.publish(|status| {
                status.phase = MidiOutputRuntimePhase::Connecting;
                status.last_error = None;
            });
            match control.session_snapshot().and_then(|(selected, _)| opened_ports(&selected)) {
                Ok(ports) => {
                    // An opt-out during an OS open must never transmit, even if
                    // the driver did not return until seconds later.
                    if control.desired_enabled.load(Ordering::Acquire)
                        && !control.shutdown.load(Ordering::Acquire)
                        && control.request_revision.load(Ordering::Acquire) == active_request
                    {
                        control.publish(|status| {
                            status.phase = MidiOutputRuntimePhase::Ready;
                            status.connected_ports = ports.keys().cloned().collect();
                            status.cleanup_pending = false;
                        });
                        scheduler = Some(WindowScheduler::new(ports));
                    }
                }
                Err(error) => {
                    frozen_request = Some(requested);
                    control.publish(|status| {
                        status.phase = MidiOutputRuntimePhase::Failed;
                        status.last_error = Some(error.to_string());
                    });
                }
            }
        } else if let Some(plan) = &mut scheduler {
            match control.session_snapshot() {
                Ok((updated, revision)) => {
                    let observed = observer.snapshot();
                    match plan.tick(&updated, revision, observed, || {
                        control.desired_enabled.load(Ordering::Acquire)
                            && !control.shutdown.load(Ordering::Acquire)
                            && observer.snapshot().generation == observed.generation
                    }) {
                        Ok(Tick::Idle) => publish_scheduler_status(&control, plan, MidiOutputRuntimePhase::Ready),
                        Ok(Tick::WaitingForCleanup) => publish_scheduler_status(&control, plan, MidiOutputRuntimePhase::Ready),
                        Ok(Tick::Queued) => publish_scheduler_status(&control, plan, MidiOutputRuntimePhase::Sending),
                        Err(error) => {
                            publish_scheduler_status(&control, plan, MidiOutputRuntimePhase::Failed);
                            control.publish(|status| status.last_error = Some(error));
                            frozen_request = Some(requested);
                        }
                    }
                }
                Err(error) => {
                    plan.fail(error.to_string());
                    publish_scheduler_status(&control, plan, MidiOutputRuntimePhase::Failed);
                    frozen_request = Some(requested);
                }
            }
        }
        if shutting_down && scheduler.is_none() {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
    use soundcraft_model::{ChannelFormat, Clip, ClipContent, MidiOutputRoute, TrackKind};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Sent {
        generation: u64,
        deadline: u64,
        bytes: Vec<u8>,
    }

    /// Fully synthetic clock/port with controllable queue saturation and
    /// disconnect. Tests never enumerate or open a physical output device.
    struct FakePort {
        now: u64,
        generation: u64,
        acknowledged: u64,
        immediate_ack: bool,
        disconnected: bool,
        full_after: Option<usize>,
        cancellations: u64,
        sent: Vec<Sent>,
    }

    impl Default for FakePort {
        fn default() -> Self {
            Self {
                now: 10_000,
                generation: 0,
                acknowledged: 0,
                immediate_ack: true,
                disconnected: false,
                full_after: None,
                cancellations: 0,
                sent: Vec::new(),
            }
        }
    }

    impl MidiPort for FakePort {
        fn now_micros(&self) -> u64 {
            self.now
        }

        fn try_send_at(&mut self, micros: u64, bytes: &[u8]) -> Result<(), MidiOutputSendError> {
            if self.disconnected {
                return Err(MidiOutputSendError::Disconnected);
            }
            if self.full_after.is_some_and(|limit| self.sent.len() >= limit) {
                return Err(MidiOutputSendError::Full);
            }
            self.sent.push(Sent { generation: self.generation, deadline: micros, bytes: bytes.to_vec() });
            Ok(())
        }

        fn cancel_and_release(&mut self) -> Result<u64, MidiOutputSendError> {
            if self.disconnected {
                return Err(MidiOutputSendError::Disconnected);
            }
            self.generation += 1;
            self.cancellations += 1;
            if self.immediate_ack {
                self.acknowledged = self.generation;
            }
            Ok(self.generation)
        }

        fn worker_state(&self) -> MidiOutputWorkerState {
            if self.disconnected { MidiOutputWorkerState::Failed } else { MidiOutputWorkerState::Running }
        }

        fn acknowledged_generation(&self) -> u64 {
            self.acknowledged
        }
    }

    fn clip_session(pitch: u8, port: &str, controllers: bool) -> Session {
        let mut session = Session::default();
        let track_id = session.add_track(TrackKind::Midi, ChannelFormat::Mono, Some("Hardware MIDI"));
        let sequence = Sequence {
            notes: vec![Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 480 }],
            ctrls: if controllers { vec![CtrlEvent { tick: 240, channel: 0, kind: CtrlKind::Cc(1), value: 96 }] } else { Vec::new() },
        };
        let clip = Clip::midi(session.new_clip_id(), "Device take", 0, 48_000, sequence);
        let Some(track) = session.track_mut(track_id) else { panic!("test track") };
        track.midi_output = Some(MidiOutputRoute { port: port.into(), channel: None });
        let Some(playlist) = track.playlist_mut() else { panic!("test playlist") };
        playlist.clips.push(clip);
        session
    }

    fn snap(position: Samples, generation: u64, playing: bool, speed: f32) -> TransportSnapshot {
        TransportSnapshot { position, generation, playing, speed }
    }

    fn fake_scheduler() -> WindowScheduler<FakePort> {
        WindowScheduler::new(BTreeMap::from([("Fake Synth".into(), FakePort::default())]))
    }

    fn messages(plan: &WindowScheduler<FakePort>) -> Vec<(u64, Vec<u8>)> {
        plan.ports["Fake Synth"].sent.iter().map(|sent| (sent.deadline, sent.bytes.clone())).collect()
    }

    #[test]
    fn no_persisted_route_and_ambiguous_stale_ports_are_rejected_before_open() {
        assert!(matches!(routed_port_names(&Session::default()), Err(MidiOutputRuntimeError::NoSelectedRoute)));
        let selected = routed_port_names(&clip_session(60, "Fake Synth", false)).expect("explicit route");
        assert_eq!(selected, BTreeSet::from(["Fake Synth".to_owned()]));
        let discovered = [
            MidiOutputPortInfo { index: 0, name: "Fake Synth".into(), id: "device:1".into() },
            MidiOutputPortInfo { index: 1, name: " fake synth ".into(), id: "device:2".into() },
        ];
        assert!(matches!(preflight_port("Fake Synth", &discovered), Err(MidiOutputRuntimeError::Ambiguous(_))));
        assert!(matches!(preflight_port("Missing Synth", &discovered), Err(MidiOutputRuntimeError::Unavailable(_))));
        assert_eq!(preflight_port("Fake Synth", &discovered[..1]).expect("unique").id, "device:1");
    }

    #[test]
    fn overlapping_ticks_send_every_note_and_noteoff_exactly_once_per_generation() {
        let session = clip_session(60, "Fake Synth", false);
        let mut plan = fake_scheduler();
        let at_start = snap(0, 1, true, 1.0);
        assert_eq!(plan.tick(&session, 0, at_start, || true), Ok(Tick::WaitingForCleanup));
        assert!(messages(&plan).is_empty(), "no packet before generation cleanup is acknowledged");
        assert_eq!(plan.tick(&session, 0, at_start, || true), Ok(Tick::Queued));
        assert_eq!(plan.tick(&session, 0, at_start, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan), vec![(10_000, vec![0x90, 60, 100])]);

        // Independent half-open windows cross the exact note-off boundary.
        let before_off = snap(11_600, 1, true, 1.0);
        assert_eq!(plan.tick(&session, 0, before_off, || true), Ok(Tick::Queued));
        assert_eq!(plan.tick(&session, 0, before_off, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan), vec![(10_000, vec![0x90, 60, 100]), (18_333, vec![0x80, 60, 64])]);
        assert_eq!(plan.scheduled, 2);
    }

    #[test]
    fn stop_seek_loop_speed_and_session_replace_cancel_before_new_midi() {
        let session = clip_session(60, "Fake Synth", true);
        let mut plan = fake_scheduler();
        let mut observed = snap(0, 1, true, 1.0);
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan).len(), 1);

        // Stop releases the first note and cannot schedule a new window.
        observed = snap(100, 2, false, 1.0);
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Idle));
        assert_eq!(messages(&plan).len(), 1);

        // Seek back to zero and a loop wrap each start a fresh generation;
        // repeated polling within one generation never duplicates NoteOn.
        for generation in [3, 4] {
            observed = snap(0, generation, true, 1.0);
            assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
            assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Queued));
            assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Queued));
        }
        assert_eq!(messages(&plan).len(), 3);
        assert_eq!(plan.ports["Fake Synth"].cancellations, 4);

        // At 2x transport speed, a controller 1,000 samples ahead has a
        // ~10.4 ms deadline, not the 20.8 ms deadline used at normal speed.
        observed = snap(5_000, 5, true, 2.0);
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan).last(), Some(&(20_417, vec![0xB0, 1, 96])));

        // Replacing the immutable session changes event content only after a
        // fresh cleanup acknowledgment, even at the same sample position.
        let replacement = clip_session(69, "Fake Synth", false);
        observed = snap(0, 6, true, 1.0);
        assert_eq!(plan.tick(&replacement, 1, observed, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&replacement, 1, observed, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan).last(), Some(&(10_000, vec![0x90, 69, 100])));
        assert_eq!(plan.ports["Fake Synth"].cancellations, 6);
    }

    #[test]
    fn cleanup_acknowledgment_gates_new_epoch_and_opt_out_aborts_partial_window() {
        let mut session = clip_session(60, "Fake Synth", true);
        let Some(clip) = session.tracks.first_mut().and_then(|track| track.playlist_mut()).and_then(|playlist| playlist.clips.first_mut()) else {
            panic!("test clip");
        };
        let ClipContent::Midi { sequence } = &mut clip.content else { panic!("test MIDI sequence") };
        sequence.ctrls.push(CtrlEvent { tick: 245, channel: 0, kind: CtrlKind::Cc(2), value: 90 });
        let mut plan = fake_scheduler();
        plan.ports.get_mut("Fake Synth").expect("port").immediate_ack = false;
        let observed = snap(0, 1, true, 1.0);
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::WaitingForCleanup));
        assert!(messages(&plan).is_empty());
        plan.ports.get_mut("Fake Synth").expect("port").acknowledged = 1;
        assert_eq!(plan.tick(&session, 0, observed, || true), Ok(Tick::Queued));
        assert_eq!(messages(&plan).len(), 1);

        // Simulate an opt-out or seek arriving between the two due messages.
        let allowed = std::cell::Cell::new(0usize);
        let newer = snap(5_000, 2, true, 1.0);
        plan.ports.get_mut("Fake Synth").expect("port").immediate_ack = true;
        assert_eq!(plan.tick(&session, 0, newer, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(
            plan.tick(&session, 0, newer, || {
                let n = allowed.get();
                allowed.set(n + 1);
                n == 0
            }),
            Ok(Tick::WaitingForCleanup)
        );
        assert!(plan.ports["Fake Synth"].cancellations >= 3);
    }

    #[test]
    fn full_queue_or_disconnected_port_fails_closed_without_retrying_partial_take() {
        let session = clip_session(60, "Fake Synth", true);
        let mut full = fake_scheduler();
        full.ports.get_mut("Fake Synth").expect("port").full_after = Some(0);
        let obs = snap(0, 1, true, 1.0);
        assert_eq!(full.tick(&session, 0, obs, || true), Ok(Tick::WaitingForCleanup));
        assert!(full.tick(&session, 0, obs, || true).is_err());
        assert!(full.tick(&session, 0, obs, || true).is_err());
        assert!(messages(&full).is_empty());
        assert!(full.blocked);
        assert!(full.ports["Fake Synth"].cancellations >= 2);

        let mut unplugged = fake_scheduler();
        assert_eq!(unplugged.tick(&session, 0, obs, || true), Ok(Tick::WaitingForCleanup));
        unplugged.ports.get_mut("Fake Synth").expect("port").disconnected = true;
        assert!(unplugged.tick(&session, 0, obs, || true).is_err());
        assert!(unplugged.blocked);
        assert!(messages(&unplugged).is_empty());
    }

    #[test]
    fn changing_route_to_new_hardware_requires_fresh_opt_in_even_if_no_notes_are_due() {
        let approved = clip_session(60, "Fake Synth", false);
        let other = clip_session(70, "Different Port", false);
        let mut plan = fake_scheduler();
        let stopped = snap(0, 1, false, 1.0);
        assert_eq!(plan.tick(&approved, 0, stopped, || true), Ok(Tick::WaitingForCleanup));
        assert_eq!(plan.tick(&approved, 0, stopped, || true), Ok(Tick::Idle));
        assert!(plan.tick(&other, 1, stopped, || true).is_err());
        assert!(plan.blocked);
        assert!(messages(&plan).is_empty());
        assert!(plan.last_error.as_deref().is_some_and(|error| error.contains("fresh explicit opt-in")));
    }
}
