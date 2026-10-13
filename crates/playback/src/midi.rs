//! Hardware MIDI input devices and lock-free event queue.
//!
//! A [`MidiManager`] discovers physical MIDI input ports via `midir` (CoreMIDI, ALSA, WinMM)
//! and streams parsed MIDI events into a preallocated, lock-free, allocation-free [`MidiInputRing`].
//! The audio callback drains this ring without blocking or allocating.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

#[cfg(not(target_arch = "wasm32"))]
use midir::{Ignore, MidiInput, MidiInputConnection};

/// Default capacity for the lock-free MIDI input ring (power of two).
pub const DEFAULT_RING_CAPACITY: usize = 2048;

/// A live MIDI event captured from a physical device or virtual keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveMidiEvent {
    /// Source physical port or virtual device id.
    pub port_id: u32,
    /// High-resolution monotonic timestamp in microseconds.
    pub timestamp_us: u64,
    /// Number of valid bytes (1 to 3).
    pub len: u8,
    /// Raw MIDI 1.0 bytes (e.g. `[0x90, 60, 100]`).
    pub data: [u8; 3],
}

impl LiveMidiEvent {
    /// Note On event with velocity > 0.
    pub fn is_note_on(&self) -> bool {
        self.len == 3 && (self.data[0] & 0xF0 == 0x90) && self.data[2] > 0
    }

    /// Note Off event (either 0x80 or 0x90 with velocity 0).
    pub fn is_note_off(&self) -> bool {
        self.len == 3 && ((self.data[0] & 0xF0 == 0x80) || ((self.data[0] & 0xF0 == 0x90) && self.data[2] == 0))
    }

    /// Pitch (0..=127) for Note On or Note Off.
    pub fn note_pitch(&self) -> u8 {
        self.data[1] & 0x7F
    }

    /// Velocity (0..=127) for Note On or Note Off.
    pub fn note_velocity(&self) -> u8 {
        self.data[2] & 0x7F
    }

    /// MIDI channel (0..=15).
    pub fn channel(&self) -> u8 {
        self.data[0] & 0x0F
    }

    /// Pack `len` and `data` into a single `u32`.
    fn pack_word(&self) -> u32 {
        (self.len as u32) | ((self.data[0] as u32) << 8) | ((self.data[1] as u32) << 16) | ((self.data[2] as u32) << 24)
    }

    /// Unpack `len` and `data` from a packed `u32`.
    fn unpack_word(port_id: u32, timestamp_us: u64, word: u32) -> Self {
        let len = (word & 0xFF) as u8;
        let d0 = ((word >> 8) & 0xFF) as u8;
        let d1 = ((word >> 16) & 0xFF) as u8;
        let d2 = ((word >> 24) & 0xFF) as u8;
        Self { port_id, timestamp_us, len, data: [d0, d1, d2] }
    }
}

/// Parse raw incoming MIDI bytes into a [`LiveMidiEvent`].
/// Filters Active Sensing (0xFE) and malformed packets.
pub fn parse_midi_message(port_id: u32, timestamp_us: u64, bytes: &[u8]) -> Option<LiveMidiEvent> {
    if bytes.is_empty() {
        return None;
    }
    let status = bytes[0];
    if status == 0xFE {
        // Active Sensing: periodic keep-alive pulse sent by some keyboards, ignore.
        return None;
    }
    match status & 0xF0 {
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => {
            if bytes.len() >= 3 {
                Some(LiveMidiEvent { port_id, timestamp_us, len: 3, data: [status, bytes[1] & 0x7F, bytes[2] & 0x7F] })
            } else {
                None
            }
        }
        0xC0 | 0xD0 => {
            if bytes.len() >= 2 {
                Some(LiveMidiEvent { port_id, timestamp_us, len: 2, data: [status, bytes[1] & 0x7F, 0] })
            } else {
                None
            }
        }
        0xF0 => {
            // Real-time messages (0xF8..=0xFF except 0xFE)
            if status >= 0xF8 { Some(LiveMidiEvent { port_id, timestamp_us, len: 1, data: [status, 0, 0] }) } else { None }
        }
        _ => None,
    }
}

struct Slot {
    sequence: AtomicUsize,
    word: AtomicU32,
    port: AtomicU32,
    time: AtomicU64,
}

/// Lock-free, bounded multi-producer single-consumer ring buffer for [`LiveMidiEvent`].
/// Never blocks, never allocates during push/pop, and uses zero `unsafe`.
pub struct MidiInputRing {
    slots: Box<[Slot]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    dropped: AtomicUsize,
}

impl MidiInputRing {
    pub fn new(capacity: usize) -> Arc<Self> {
        let n = capacity.max(2).next_power_of_two();
        let slots = (0..n)
            .map(|i| Slot { sequence: AtomicUsize::new(i), word: AtomicU32::new(0), port: AtomicU32::new(0), time: AtomicU64::new(0) })
            .collect();
        Arc::new(Self { slots, mask: n - 1, head: AtomicUsize::new(0), tail: AtomicUsize::new(0), dropped: AtomicUsize::new(0) })
    }

    /// Push an event into the ring.
    /// Thread-safe for multiple concurrent producers (e.g. multiple MIDI ports and UI thread).
    /// Returns `true` on success, or `false` if the buffer is full (event dropped).
    pub fn push(&self, event: LiveMidiEvent) -> bool {
        let word = event.pack_word();
        let port = event.port_id;
        let time = event.timestamp_us;
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let idx = head & self.mask;
            let Some(slot) = self.slots.get(idx) else {
                return false;
            };
            let seq = slot.sequence.load(Ordering::Acquire);
            let diff = (seq as isize).wrapping_sub(head as isize);
            if diff == 0 {
                match self.head.compare_exchange_weak(head, head.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        slot.word.store(word, Ordering::Relaxed);
                        slot.port.store(port, Ordering::Relaxed);
                        slot.time.store(time, Ordering::Relaxed);
                        slot.sequence.store(head.wrapping_add(1), Ordering::Release);
                        return true;
                    }
                    Err(actual) => head = actual,
                }
            } else if diff < 0 {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            } else {
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }

    /// Pop a single event from the ring.
    /// Expected to be called from the single consumer (audio thread).
    pub fn pop(&self) -> Option<LiveMidiEvent> {
        let tail = self.tail.load(Ordering::Relaxed);
        let idx = tail & self.mask;
        let slot = self.slots.get(idx)?;
        let seq = slot.sequence.load(Ordering::Acquire);
        let diff = (seq as isize).wrapping_sub(tail.wrapping_add(1) as isize);
        if diff == 0 {
            let word = slot.word.load(Ordering::Relaxed);
            let port = slot.port.load(Ordering::Relaxed);
            let time = slot.time.load(Ordering::Relaxed);
            slot.sequence.store(tail.wrapping_add(self.mask).wrapping_add(1), Ordering::Release);
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            Some(LiveMidiEvent::unpack_word(port, time, word))
        } else {
            None
        }
    }

    /// Drain all pending events calling `f` for each event in order.
    pub fn drain(&self, mut f: impl FnMut(LiveMidiEvent)) {
        while let Some(ev) = self.pop() {
            f(ev);
        }
    }

    /// Total events dropped due to a full buffer.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Current approximate number of queued events.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail)
    }

    /// Returns `true` if the ring has no events queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Information about a detected physical or system MIDI input device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MidiDeviceInfo {
    pub id: u32,
    pub name: String,
    pub connected: bool,
}

/// Query the OS for available MIDI input devices.
pub fn list_input_devices() -> Vec<MidiDeviceInfo> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Ok(midi_in) = MidiInput::new("SoundCraft") else {
            return Vec::new();
        };
        let mut devices = Vec::new();
        for (i, port) in midi_in.ports().iter().enumerate() {
            let name = midi_in.port_name(port).unwrap_or_else(|_| format!("MIDI Port {i}"));
            devices.push(MidiDeviceInfo { id: i as u32, name, connected: false });
        }
        devices
    }
    #[cfg(target_arch = "wasm32")]
    {
        Vec::new()
    }
}

/// Manages physical MIDI hardware connections and streams input into a [`MidiInputRing`].
pub struct MidiManager {
    ring: Arc<MidiInputRing>,
    #[cfg(not(target_arch = "wasm32"))]
    connections: HashMap<String, MidiInputConnection<()>>,
}

impl MidiManager {
    pub fn new(ring: Arc<MidiInputRing>) -> Self {
        Self {
            ring,
            #[cfg(not(target_arch = "wasm32"))]
            connections: HashMap::new(),
        }
    }

    pub fn ring(&self) -> &Arc<MidiInputRing> {
        &self.ring
    }

    /// Enumerate available MIDI input devices and mark whether they are currently connected.
    pub fn list_devices(&self) -> Vec<MidiDeviceInfo> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut devices = list_input_devices();
            for dev in &mut devices {
                dev.connected = self.connections.contains_key(&dev.name);
            }
            devices
        }
        #[cfg(target_arch = "wasm32")]
        {
            list_input_devices()
        }
    }

    /// Names of all currently connected devices.
    pub fn connected_devices(&self) -> Vec<String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.connections.keys().cloned().collect()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Vec::new()
        }
    }

    /// Connect to a specific input device by name.
    pub fn connect(&mut self, device_name: &str) -> Result<(), String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.connections.contains_key(device_name) {
                return Ok(());
            }
            let mut midi_in = MidiInput::new("SoundCraft Device").map_err(|e| format!("Failed to initialize MIDI input: {e}"))?;
            midi_in.ignore(Ignore::ActiveSense);
            let ports = midi_in.ports();
            let mut matched = None;
            for (idx, port) in ports.iter().enumerate() {
                if let Ok(name) = midi_in.port_name(port)
                    && name == device_name
                {
                    matched = Some((idx as u32, idx));
                    break;
                }
            }
            let (port_id, port_idx) = matched.ok_or_else(|| format!("MIDI device “{device_name}” not found"))?;
            let port = ports.get(port_idx).ok_or_else(|| format!("Port index {port_idx} out of range"))?;
            let ring = Arc::clone(&self.ring);
            let conn = midi_in
                .connect(
                    port,
                    device_name,
                    move |timestamp_us, bytes, _| {
                        if let Some(event) = parse_midi_message(port_id, timestamp_us, bytes) {
                            ring.push(event);
                        }
                    },
                    (),
                )
                .map_err(|e| format!("Failed to connect to “{device_name}”: {e}"))?;
            self.connections.insert(device_name.to_string(), conn);
            Ok(())
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = device_name;
            Err("MIDI hardware I/O is not supported on WebAssembly".into())
        }
    }

    /// Disconnect from a device.
    pub fn disconnect(&mut self, device_name: &str) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.connections.remove(device_name);
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = device_name;
        }
    }

    /// Sync active connections with a desired list of enabled device names.
    pub fn sync_enabled(&mut self, enabled: &[String]) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Remove disconnected devices
            self.connections.retain(|name, _| enabled.contains(name));
            // Connect newly enabled devices
            for name in enabled {
                if !self.connections.contains_key(name) {
                    let _ = self.connect(name);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = enabled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_push_pop_fifo() {
        let ring = MidiInputRing::new(4);
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);

        let e1 = LiveMidiEvent { port_id: 1, timestamp_us: 100, len: 3, data: [0x90, 60, 100] };
        let e2 = LiveMidiEvent { port_id: 1, timestamp_us: 200, len: 3, data: [0x80, 60, 64] };

        assert!(ring.push(e1));
        assert!(ring.push(e2));
        assert_eq!(ring.len(), 2);

        assert_eq!(ring.pop(), Some(e1));
        assert_eq!(ring.pop(), Some(e2));
        assert_eq!(ring.pop(), None);
        assert!(ring.is_empty());
    }

    #[test]
    fn ring_full_drops_events() {
        let ring = MidiInputRing::new(2); // next power of two is 2
        let ev = LiveMidiEvent { port_id: 0, timestamp_us: 1, len: 3, data: [0x90, 60, 100] };

        assert!(ring.push(ev));
        assert!(ring.push(ev));
        assert!(!ring.push(ev), "ring must reject when full");
        assert_eq!(ring.dropped(), 1);

        assert!(ring.pop().is_some());
        assert!(ring.push(ev), "push succeeds after popping");
    }

    #[test]
    fn ring_drain() {
        let ring = MidiInputRing::new(8);
        for i in 0..5 {
            ring.push(LiveMidiEvent { port_id: i, timestamp_us: i as u64 * 10, len: 3, data: [0x90, i as u8, 100] });
        }

        let mut drained = Vec::new();
        ring.drain(|e| drained.push(e.port_id));
        assert_eq!(drained, vec![0, 1, 2, 3, 4]);
        assert!(ring.is_empty());
    }

    #[test]
    fn parse_midi_messages() {
        // Note On
        let ev = parse_midi_message(0, 50, &[0x90, 60, 100]).expect("Note On parsed");
        assert!(ev.is_note_on());
        assert!(!ev.is_note_off());
        assert_eq!(ev.note_pitch(), 60);
        assert_eq!(ev.note_velocity(), 100);
        assert_eq!(ev.channel(), 0);

        // Note On with velocity 0 is Note Off
        let ev0 = parse_midi_message(0, 60, &[0x90, 60, 0]).expect("Note On vel 0 parsed");
        assert!(!ev0.is_note_on());
        assert!(ev0.is_note_off());
        assert_eq!(ev0.note_pitch(), 60);

        // Standard Note Off (0x80)
        let ev_off = parse_midi_message(0, 70, &[0x80, 64, 64]).expect("Note Off parsed");
        assert!(ev_off.is_note_off());
        assert_eq!(ev_off.note_pitch(), 64);

        // Control Change (0xB0)
        let ev_cc = parse_midi_message(1, 80, &[0xB0, 7, 127]).expect("CC parsed");
        assert_eq!(ev_cc.len, 3);
        assert_eq!(ev_cc.data, [0xB0, 7, 127]);

        // Program Change (0xC0, 2 bytes)
        let ev_pc = parse_midi_message(1, 90, &[0xC0, 10]).expect("Program Change parsed");
        assert_eq!(ev_pc.len, 2);
        assert_eq!(ev_pc.data[0], 0xC0);
        assert_eq!(ev_pc.data[1], 10);

        // Active Sensing (0xFE) ignored
        assert_eq!(parse_midi_message(0, 100, &[0xFE]), None);

        // Empty bytes
        assert_eq!(parse_midi_message(0, 100, &[]), None);
    }

    #[test]
    fn enumeration_does_not_panic() {
        let devices = list_input_devices();
        // Even if no MIDI devices are connected, it should return a Vec without error
        let _ = devices.len();
    }

    #[test]
    fn midi_manager_initialization() {
        let ring = MidiInputRing::new(16);
        let mut mgr = MidiManager::new(ring);
        assert_eq!(mgr.connected_devices(), Vec::<String>::new());
        mgr.sync_enabled(&[]);
        assert_eq!(mgr.connected_devices(), Vec::<String>::new());
    }
}
