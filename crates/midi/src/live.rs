//! Live MIDI 1.0 event primitives.

/// A live MIDI event captured from a physical device or virtual keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    pub fn pack_word(&self) -> u32 {
        (self.len as u32) | ((self.data[0] as u32) << 8) | ((self.data[1] as u32) << 16) | ((self.data[2] as u32) << 24)
    }

    /// Unpack `len` and `data` from a packed `u32`.
    pub fn unpack_word(port_id: u32, timestamp_us: u64, word: u32) -> Self {
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
