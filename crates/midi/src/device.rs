//! Native MIDI input capture: device discovery, bounded handoff and safe reconnect checks.
//!
//! The device callback feeds the allocation-free live decoder into a preallocated, bounded
//! nonblocking queue. The consumer owns musical-time conversion and track routing; it must
//! check the loss counters and send All Notes Off if events have been dropped. Neither the
//! callback nor this module modifies a session or calls a plugin.
//!
//! On Linux, macOS and Windows the hardware adapter uses midir. Other native targets can use
//! the synthetic queue but report Unsupported for physical MIDI input. Web/WASM retains the
//! standalone live parser but does not expose this native module.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crossbeam_channel::{Receiver, Sender, bounded};
use thiserror::Error;

use crate::live::{LiveMidiParser, MidiEventBatch, TimedMidiMessage};

/// The maximum number of pending events reserved by one capture connection.
pub const MAX_MIDI_QUEUE_EVENTS: usize = 65_536;
/// A callback parses into this fixed stack batch before its nonblocking queue handoff.
const CALLBACK_BATCH_EVENTS: usize = 64;
/// Do not list or open arbitrary huge driver-provided port arrays.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
const MAX_MIDI_INPUT_PORTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MidiDeviceError {
    #[error("MIDI device backend failed: {0}")]
    Backend(String),
    #[error("MIDI input port {0} is unavailable")]
    UnavailablePort(usize),
    #[error("MIDI queue capacity must be 1..={MAX_MIDI_QUEUE_EVENTS}; received {0}")]
    InvalidCapacity(usize),
    #[error("MIDI input connection failed: {0}")]
    Connection(String),
    #[error("hardware MIDI input is unsupported on this platform")]
    Unsupported,
}

/// An input port as observed during a device scan. Indexes may change after hot-plug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiInputPortInfo {
    pub index: usize,
    pub name: String,
}

#[derive(Default)]
struct LossCounters {
    dropped_events: AtomicU64,
    truncated_packets: AtomicU64,
}

/// Events and loss information consumed outside the realtime/device callback.
pub struct MidiCaptureQueue {
    receiver: Receiver<TimedMidiMessage>,
    counters: Arc<LossCounters>,
}

impl MidiCaptureQueue {
    /// Nonblocking receive; None means the queue is empty (or the producer is gone).
    pub fn try_recv(&self) -> Option<TimedMidiMessage> {
        self.receiver.try_recv().ok()
    }

    pub fn pending(&self) -> usize {
        self.receiver.len()
    }

    /// Reset and return the number of event drops since the last check.
    pub fn take_dropped_events(&self) -> u64 {
        self.counters.dropped_events.swap(0, Ordering::Relaxed)
    }

    /// Reset and return the number of oversized driver packets that were truncated.
    /// A truncated packet can lose an unknown number of events, so never treat zero
    /// dropped_events as proof of loss-free capture when this counter is nonzero.
    pub fn take_truncated_packets(&self) -> u64 {
        self.counters.truncated_packets.swap(0, Ordering::Relaxed)
    }
}

/// Callback-owned MIDI decoder and bounded producer. Can also be driven by a fake
/// device in deterministic tests.
pub struct MidiIngress {
    parser: LiveMidiParser,
    sender: Sender<TimedMidiMessage>,
    counters: Arc<LossCounters>,
}

impl MidiIngress {
    /// Parse one packet and hand off decoded messages without blocking or allocating.
    pub fn push_packet(&mut self, timestamp_micros: u64, bytes: &[u8]) {
        let mut batch = MidiEventBatch::<CALLBACK_BATCH_EVENTS>::default();
        self.parser.push_packet(timestamp_micros, bytes, &mut batch);

        let mut lost = u64::try_from(batch.dropped()).unwrap_or(u64::MAX);
        for event in batch.iter() {
            if self.sender.try_send(event).is_err() {
                lost = lost.saturating_add(1);
            }
        }
        if lost > 0 {
            self.counters.dropped_events.fetch_add(lost, Ordering::Relaxed);
        }
        if batch.overflowed() && bytes.len() > crate::live::MAX_LIVE_PACKET_BYTES {
            self.counters.truncated_packets.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Create an isolated MIDI packet source and bounded consumer without opening hardware.
/// Capacity is strictly validated before allocating; 0 is never a rendezvous channel.
pub fn capture_channel(capacity: usize) -> Result<(MidiIngress, MidiCaptureQueue), MidiDeviceError> {
    if !(1..=MAX_MIDI_QUEUE_EVENTS).contains(&capacity) {
        return Err(MidiDeviceError::InvalidCapacity(capacity));
    }
    let (sender, receiver) = bounded(capacity);
    let counters = Arc::new(LossCounters::default());
    Ok((MidiIngress { parser: LiveMidiParser::default(), sender, counters: Arc::clone(&counters) }, MidiCaptureQueue { receiver, counters }))
}

/// A live input connection. Keep it alive while consuming events from its queue.
pub struct MidiInputStream {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    _connection: midir::MidiInputConnection<()>,
    port: MidiInputPortInfo,
    capture: MidiCaptureQueue,
}

impl MidiInputStream {
    pub fn port(&self) -> &MidiInputPortInfo {
        &self.port
    }

    pub fn try_recv(&self) -> Option<TimedMidiMessage> {
        self.capture.try_recv()
    }

    pub fn pending(&self) -> usize {
        self.capture.pending()
    }

    /// Borrow the bounded queue for transport-aware consumers. The OS callback owns the
    /// producer; receiving and musical-time conversion must remain off that callback.
    pub fn capture_queue(&self) -> &MidiCaptureQueue {
        &self.capture
    }

    pub fn take_dropped_events(&self) -> u64 {
        self.capture.take_dropped_events()
    }

    pub fn take_truncated_packets(&self) -> u64 {
        self.capture.take_truncated_packets()
    }

    /// Best-effort hot-unplug check for UI/control-thread polling. A renamed or
    /// reordered port returns false so callers can close, rescan and reconnect.
    /// No MIDI backend offers an atomic guarantee between this check and use.
    pub fn port_still_present(&self) -> Result<bool, MidiDeviceError> {
        let ports = input_ports()?;
        Ok(ports.iter().any(|p| p == &self.port))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn input_ports() -> Result<Vec<MidiInputPortInfo>, MidiDeviceError> {
    let input = midir::MidiInput::new("SoundCraft MIDI Scan").map_err(|e| MidiDeviceError::Backend(e.to_string()))?;
    let ports = input.ports();
    let mut found = Vec::with_capacity(ports.len().min(MAX_MIDI_INPUT_PORTS));
    for (index, port) in ports.iter().take(MAX_MIDI_INPUT_PORTS).enumerate() {
        let name = input.port_name(port).map_err(|e| MidiDeviceError::Backend(e.to_string()))?;
        found.push(MidiInputPortInfo { index, name });
    }
    Ok(found)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn input_ports() -> Result<Vec<MidiInputPortInfo>, MidiDeviceError> {
    Err(MidiDeviceError::Unsupported)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn open_input(port_index: usize, capacity: usize) -> Result<MidiInputStream, MidiDeviceError> {
    if port_index >= MAX_MIDI_INPUT_PORTS {
        return Err(MidiDeviceError::UnavailablePort(port_index));
    }
    // Validate the bounded queue before opening the device.
    let (mut ingress, capture) = capture_channel(capacity)?;
    let mut input = midir::MidiInput::new("SoundCraft MIDI Input").map_err(|e| MidiDeviceError::Backend(e.to_string()))?;
    // Keep clock/start/stop/active sensing; suppress SysEx payloads in the backend.
    input.ignore(midir::Ignore::Sysex);
    let ports = input.ports();
    let port = ports.get(port_index).ok_or(MidiDeviceError::UnavailablePort(port_index))?;
    let name = input.port_name(port).map_err(|e| MidiDeviceError::Backend(e.to_string()))?;
    let connection = input
        .connect(port, "SoundCraft Capture", move |timestamp_micros, bytes, _| ingress.push_packet(timestamp_micros, bytes), ())
        .map_err(|e| MidiDeviceError::Connection(e.to_string()))?;
    Ok(MidiInputStream { _connection: connection, port: MidiInputPortInfo { index: port_index, name }, capture })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn open_input(_port_index: usize, _capacity: usize) -> Result<MidiInputStream, MidiDeviceError> {
    Err(MidiDeviceError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::LiveMidiMessage;

    #[test]
    fn rejects_unbounded_or_zero_capacity() {
        assert!(matches!(capture_channel(0), Err(MidiDeviceError::InvalidCapacity(0))));
        assert!(matches!(capture_channel(MAX_MIDI_QUEUE_EVENTS + 1), Err(MidiDeviceError::InvalidCapacity(_))));
    }

    #[test]
    fn fake_device_preserves_running_status_and_source_timestamps() {
        let Ok((mut source, queue)) = capture_channel(8) else { panic!("bounded capture setup must succeed") };
        source.push_packet(101, &[0x92, 60]);
        assert!(queue.try_recv().is_none());
        source.push_packet(205, &[100, 61, 95]);
        assert_eq!(
            queue.try_recv(),
            Some(TimedMidiMessage { timestamp_micros: 205, message: LiveMidiMessage::NoteOn { channel: 2, pitch: 60, velocity: 100 } })
        );
        assert_eq!(
            queue.try_recv(),
            Some(TimedMidiMessage { timestamp_micros: 205, message: LiveMidiMessage::NoteOn { channel: 2, pitch: 61, velocity: 95 } })
        );
        assert!(queue.try_recv().is_none());
        assert_eq!(queue.take_dropped_events(), 0);
    }

    #[test]
    fn full_queue_loses_only_excess_and_reports_every_drop() {
        let Ok((mut source, queue)) = capture_channel(2) else { panic!("bounded capture setup must succeed") };
        source.push_packet(10, &[0x90, 60, 90, 61, 91, 62, 92, 63, 93]);
        assert_eq!(queue.pending(), 2);
        assert_eq!(queue.take_dropped_events(), 2);
        assert_eq!(queue.take_dropped_events(), 0);
        assert_eq!(queue.take_truncated_packets(), 0);
        assert!(queue.try_recv().is_some());
        source.push_packet(11, &[64, 94]);
        assert_eq!(queue.pending(), 2);
        assert_eq!(queue.take_dropped_events(), 0);
    }

    #[test]
    fn oversized_packet_and_disconnected_consumer_are_not_silent() {
        let Ok((mut source, queue)) = capture_channel(1) else { panic!("bounded capture setup must succeed") };
        let oversized = vec![0xF0; crate::live::MAX_LIVE_PACKET_BYTES + 1];
        source.push_packet(0, &oversized);
        assert_eq!(queue.take_truncated_packets(), 1);
        assert_eq!(queue.take_truncated_packets(), 0);
        drop(queue);
        source.push_packet(50, &[0x90, 60, 80]);
        // No crash, no allocation or blocking when the consumer has gone away.
    }

    #[test]
    fn callback_batch_limit_is_counted_independently_of_queue_capacity() {
        let Ok((mut source, queue)) = capture_channel(128) else { panic!("bounded capture setup must succeed") };
        let mut bytes = vec![0x90];
        for pitch in 0..80u8 {
            bytes.extend([pitch, 100]);
        }
        source.push_packet(55, &bytes);
        assert_eq!(queue.pending(), CALLBACK_BATCH_EVENTS);
        assert_eq!(queue.take_dropped_events(), 16);
    }
}
