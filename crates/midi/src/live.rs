//! Bounded live MIDI decoding and off-audio-thread take assembly.
//!
//! This module deliberately has no operating-system MIDI dependency. A device backend can pass
//! timestamped byte packets to LiveMidiParser, and transfer its fixed-size batches to the
//! recording/control thread. No MIDI device is opened here. The caller must handle overflow
//! explicitly (for example, stop recording and send All Notes Off): silently losing a note-off
//! can leave a synthesizer sounding indefinitely.

use crate::{CtrlEvent, CtrlKind, MidiError, Note, PPQ, Sequence};

/// Upper bound on bytes parsed in one device callback. Overlong packets mark overflow and reset
/// running status, so a faulty driver cannot monopolize the callback indefinitely.
pub const MAX_LIVE_PACKET_BYTES: usize = 4096;

/// Upper bound on events stored by a single recorded MIDI take.
pub const MAX_LIVE_TAKE_EVENTS: usize = 1_000_000;

const NOTE_SLOTS: usize = 16 * 128;

/// MIDI channel-voice and system realtime events. Channels use zero-based MIDI numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveMidiMessage {
    NoteOn { channel: u8, pitch: u8, velocity: u8 },
    NoteOff { channel: u8, pitch: u8, velocity: u8 },
    PolyPressure { channel: u8, pitch: u8, pressure: u8 },
    ControlChange { channel: u8, controller: u8, value: u8 },
    ProgramChange { channel: u8, program: u8 },
    ChannelPressure { channel: u8, pressure: u8 },
    /// Signed pitch bend, centered at zero, from -8192 through +8191.
    PitchBend { channel: u8, value: i16 },
    /// Realtime wire status (0xF8 clock, 0xFA start, 0xFB continue, 0xFC stop,
    /// 0xFE active sensing, 0xFF reset).
    Realtime(u8),
}

/// A decoded event with its original device timestamp in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedMidiMessage {
    pub timestamp_micros: u64,
    pub message: LiveMidiMessage,
}

/// A callback-local, fixed-capacity event batch with no allocation or locking.
///
/// Use a nonblocking transport to pass batches to a non-realtime consumer; the batch itself is
/// not a cross-thread queue. Overflow is sticky until clear, and must not be ignored by a player.
#[derive(Debug, Clone)]
pub struct MidiEventBatch<const N: usize> {
    events: [Option<TimedMidiMessage>; N],
    len: usize,
    dropped: usize,
    overflowed: bool,
}

impl<const N: usize> Default for MidiEventBatch<N> {
    fn default() -> Self {
        Self {
            events: [None; N],
            len: 0,
            dropped: 0,
            overflowed: false,
        }
    }
}

impl<const N: usize> MidiEventBatch<N> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns false instead of extending memory when the fixed capacity is reached.
    pub fn push(&mut self, event: TimedMidiMessage) -> bool {
        match self.events.get_mut(self.len) {
            Some(slot) => {
                *slot = Some(event);
                self.len += 1;
                true
            }
            None => {
                self.dropped = self.dropped.saturating_add(1);
                self.overflowed = true;
                false
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = TimedMidiMessage> + '_ {
        self.events[..self.len].iter().flatten().copied()
    }

    pub fn clear(&mut self) {
        self.events[..self.len].fill(None);
        self.len = 0;
        self.dropped = 0;
        self.overflowed = false;
    }

    fn mark_overflow(&mut self) {
        self.overflowed = true;
    }
}

/// Streaming decoder for standard MIDI 1.0 bytes, including running status.
///
/// It does not store SysEx payloads, and it discards unsupported system-common messages. Realtime
/// messages do not disturb a partially received note or controller message. The state occupies
/// a few bytes and never allocates.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveMidiParser {
    running_status: Option<u8>,
    data: [u8; 2],
    have: u8,
    sysex: bool,
}

impl LiveMidiParser {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Decode one wire byte. At most one message is returned.
    pub fn push_byte(&mut self, byte: u8) -> Option<LiveMidiMessage> {
        if byte >= 0xF8 {
            return matches!(byte, 0xF8 | 0xFA | 0xFB | 0xFC | 0xFE | 0xFF)
                .then_some(LiveMidiMessage::Realtime(byte));
        }

        if self.sysex {
            if byte < 0x80 {
                return None;
            }
            self.sysex = false;
            if byte == 0xF7 {
                return None;
            }
            // A new status implicitly ends malformed/unclosed SysEx. Resynchronise below.
        }

        if byte >= 0x80 {
            self.have = 0;
            if byte < 0xF0 {
                self.running_status = Some(byte);
            } else {
                // System-common messages cancel running status, unlike realtime messages.
                self.running_status = None;
                self.sysex = byte == 0xF0;
            }
            return None;
        }

        let status = self.running_status?;
        let Some(slot) = self.data.get_mut(usize::from(self.have)) else {
            self.reset();
            return None;
        };
        *slot = byte;
        self.have += 1;

        let needed = if matches!(status & 0xF0, 0xC0 | 0xD0) { 1 } else { 2 };
        if self.have < needed {
            return None;
        }
        self.have = 0;

        let channel = status & 0x0F;
        let a = self.data[0];
        let b = self.data[1];
        match status & 0xF0 {
            0x80 => Some(LiveMidiMessage::NoteOff { channel, pitch: a, velocity: b }),
            0x90 if b == 0 => Some(LiveMidiMessage::NoteOff { channel, pitch: a, velocity: 64 }),
            0x90 => Some(LiveMidiMessage::NoteOn { channel, pitch: a, velocity: b }),
            0xA0 => Some(LiveMidiMessage::PolyPressure { channel, pitch: a, pressure: b }),
            0xB0 => Some(LiveMidiMessage::ControlChange { channel, controller: a, value: b }),
            0xC0 => Some(LiveMidiMessage::ProgramChange { channel, program: a }),
            0xD0 => Some(LiveMidiMessage::ChannelPressure { channel, pressure: a }),
            0xE0 => {
                let value = (i16::from(b) << 7) | i16::from(a);
                Some(LiveMidiMessage::PitchBend { channel, value: value - 8192 })
            }
            _ => None,
        }
    }

    /// Decode a timestamped packet into a bounded callback-local batch.
    ///
    /// If a driver supplies an excessively long packet, only its prefix is parsed, the parser
    /// resets, and batch.overflowed becomes true. Callers must treat that as data loss.
    pub fn push_packet<const N: usize>(
        &mut self,
        timestamp_micros: u64,
        bytes: &[u8],
        batch: &mut MidiEventBatch<N>,
    ) {
        for &byte in bytes.iter().take(MAX_LIVE_PACKET_BYTES) {
            if let Some(message) = self.push_byte(byte) {
                batch.push(TimedMidiMessage { timestamp_micros, message });
            }
        }
        if bytes.len() > MAX_LIVE_PACKET_BYTES {
            self.reset();
            batch.mark_overflow();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveNote {
    start: i64,
    velocity: u8,
}

/// Assemble decoded live MIDI into a session Sequence on a non-audio thread.
///
/// One active note is tracked per channel/pitch. A repeated Note On closes the previous note at
/// the retrigger tick. Recording controller and note vectors allocates, so never invoke this type
/// from an audio or MIDI driver callback: use a bounded batch there instead.
#[derive(Debug)]
pub struct LiveMidiTake {
    active: [Option<ActiveNote>; NOTE_SLOTS],
    sequence: Sequence,
}

impl Default for LiveMidiTake {
    fn default() -> Self {
        Self { active: [None; NOTE_SLOTS], sequence: Sequence::default() }
    }
}

fn invalid(reason: &'static str) -> MidiError {
    MidiError::Invalid { offset: 0, reason }
}

fn note_slot(channel: u8, pitch: u8) -> Result<usize, MidiError> {
    if channel >= 16 || pitch >= 128 {
        return Err(invalid("invalid live MIDI channel or pitch"));
    }
    Ok(usize::from(channel) * 128 + usize::from(pitch))
}

fn valid_channel(channel: u8) -> Result<(), MidiError> {
    if channel >= 16 { Err(invalid("invalid live MIDI channel")) } else { Ok(()) }
}

fn valid_data(value: u8) -> Result<(), MidiError> {
    if value >= 128 { Err(invalid("invalid live MIDI data byte")) } else { Ok(()) }
}

impl LiveMidiTake {
    fn event_room(&self) -> Result<(), MidiError> {
        if self.sequence.notes.len().saturating_add(self.sequence.ctrls.len()) >= MAX_LIVE_TAKE_EVENTS {
            Err(MidiError::TooManyEvents(MAX_LIVE_TAKE_EVENTS))
        } else {
            Ok(())
        }
    }

    fn close(&mut self, channel: u8, pitch: u8, tick: i64, release_velocity: u8) -> Result<(), MidiError> {
        let slot = note_slot(channel, pitch)?;
        valid_data(release_velocity)?;
        let Some(held) = self.active.get(slot).copied().flatten() else {
            return Ok(());
        };
        self.event_room()?;
        if let Some(active) = self.active.get_mut(slot) {
            *active = None;
        }
        self.sequence.notes.push(Note {
            pitch,
            velocity: held.velocity,
            release_velocity,
            channel,
            start: held.start,
            length: tick.saturating_sub(held.start).max(1),
        });
        Ok(())
    }

    fn ctrl(&mut self, tick: i64, channel: u8, kind: CtrlKind, value: i32) -> Result<(), MidiError> {
        valid_channel(channel)?;
        self.event_room()?;
        self.sequence.ctrls.push(CtrlEvent { tick, channel, kind, value });
        Ok(())
    }

    /// Record a live event at an already-resolved musical tick. Realtime clock/status messages
    /// are intentionally not placed into the channel-voice sequence.
    pub fn record(&mut self, tick: i64, message: LiveMidiMessage) -> Result<(), MidiError> {
        if tick < 0 {
            return Err(invalid("negative live MIDI tick"));
        }
        match message {
            LiveMidiMessage::NoteOn { channel, pitch, velocity } => {
                let slot = note_slot(channel, pitch)?;
                valid_data(velocity)?;
                if velocity == 0 {
                    return self.close(channel, pitch, tick, 64);
                }
                self.close(channel, pitch, tick, 64)?;
                if let Some(active) = self.active.get_mut(slot) {
                    *active = Some(ActiveNote { start: tick, velocity });
                }
                Ok(())
            }
            LiveMidiMessage::NoteOff { channel, pitch, velocity } =>
                self.close(channel, pitch, tick, velocity),
            LiveMidiMessage::PolyPressure { channel, pitch, pressure } => {
                note_slot(channel, pitch)?;
                valid_data(pressure)?;
                self.ctrl(tick, channel, CtrlKind::PolyPressure(pitch), i32::from(pressure))
            }
            LiveMidiMessage::ControlChange { channel, controller, value } => {
                valid_data(controller)?;
                valid_data(value)?;
                self.ctrl(tick, channel, CtrlKind::Cc(controller), i32::from(value))
            }
            LiveMidiMessage::ProgramChange { channel, program } => {
                valid_data(program)?;
                self.ctrl(tick, channel, CtrlKind::Program, i32::from(program))
            }
            LiveMidiMessage::ChannelPressure { channel, pressure } => {
                valid_data(pressure)?;
                self.ctrl(tick, channel, CtrlKind::ChannelPressure, i32::from(pressure))
            }
            LiveMidiMessage::PitchBend { channel, value } => {
                if !(-8192..=8191).contains(&value) {
                    return Err(invalid("invalid pitch bend value"));
                }
                self.ctrl(tick, channel, CtrlKind::PitchBend, i32::from(value))
            }
            LiveMidiMessage::Realtime(_) => Ok(()),
        }
    }

    /// Translate a device timestamp using a constant tempo and record the message. For a tempo
    /// map, the caller should convert timestamps to musical ticks itself and use record().
    /// bpm_milli = BPM x 1000, for example 120000 for 120 BPM.
    pub fn record_timed(&mut self, event: TimedMidiMessage, origin_micros: u64, start_tick: i64, bpm_milli: u32)
        -> Result<(), MidiError>
    {
        let elapsed = event.timestamp_micros.saturating_sub(origin_micros);
        let tick = start_tick.saturating_add(micros_to_ticks(elapsed, bpm_milli)?);
        self.record(tick, event.message)
    }

    /// Close any notes still held at the take end and return sorted content.
    pub fn finish(mut self, end_tick: i64) -> Result<Sequence, MidiError> {
        if end_tick < 0 {
            return Err(invalid("negative live MIDI take end"));
        }
        for channel in 0..16u8 {
            for pitch in 0..128u8 {
                self.close(channel, pitch, end_tick, 64)?;
            }
        }
        self.sequence.sort();
        Ok(self.sequence)
    }
}

/// Convert elapsed monotonic microseconds to model ticks at constant tempo, saturating rather
/// than overflowing on pathological driver timestamps. Zero tempo is an error.
pub fn micros_to_ticks(elapsed_micros: u64, bpm_milli: u32) -> Result<i64, MidiError> {
    if bpm_milli == 0 {
        return Err(invalid("MIDI recording tempo must be positive"));
    }
    let ticks = u128::from(elapsed_micros)
        .saturating_mul(u128::from(bpm_milli))
        .saturating_mul(u128::from(PPQ))
        / 60_000_000_000u128;
    Ok(i64::try_from(ticks).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_status_and_note_on_zero_velocity() {
        let mut p = LiveMidiParser::default();
        assert_eq!(p.push_byte(0x92), None);
        assert_eq!(p.push_byte(60), None);
        assert_eq!(p.push_byte(98), Some(LiveMidiMessage::NoteOn { channel: 2, pitch: 60, velocity: 98 }));
        assert_eq!(p.push_byte(60), None);
        assert_eq!(p.push_byte(0), Some(LiveMidiMessage::NoteOff { channel: 2, pitch: 60, velocity: 64 }));
    }

    #[test]
    fn realtime_preserves_partial_message_and_sysex_discards_payload() {
        let mut p = LiveMidiParser::default();
        assert_eq!(p.push_byte(0xB0), None);
        assert_eq!(p.push_byte(7), None);
        assert_eq!(p.push_byte(0xF8), Some(LiveMidiMessage::Realtime(0xF8)));
        assert_eq!(p.push_byte(99), Some(LiveMidiMessage::ControlChange { channel: 0, controller: 7, value: 99 }));
        assert_eq!(p.push_byte(0xF0), None);
        assert_eq!(p.push_byte(0x01), None);
        assert_eq!(p.push_byte(0xFA), Some(LiveMidiMessage::Realtime(0xFA)));
        assert_eq!(p.push_byte(0x02), None);
        assert_eq!(p.push_byte(0xF7), None);
        assert_eq!(p.push_byte(7), None);
        assert_eq!(p.push_byte(99), None);
    }

    #[test]
    fn pitch_bend_and_program_change_and_status_reset() {
        let mut p = LiveMidiParser::default();
        assert_eq!(p.push_byte(0xE1), None);
        assert_eq!(p.push_byte(0), None);
        assert_eq!(p.push_byte(64), Some(LiveMidiMessage::PitchBend { channel: 1, value: 0 }));
        assert_eq!(p.push_byte(0xC1), None);
        assert_eq!(p.push_byte(10), Some(LiveMidiMessage::ProgramChange { channel: 1, program: 10 }));
        assert_eq!(p.push_byte(11), Some(LiveMidiMessage::ProgramChange { channel: 1, program: 11 }));
        assert_eq!(p.push_byte(0xF6), None);
        assert_eq!(p.push_byte(12), None);
    }

    #[test]
    fn fixed_batch_exposes_overflow_and_preserves_timestamps() {
        let mut p = LiveMidiParser::default();
        let mut batch = MidiEventBatch::<2>::default();
        p.push_packet(1234, &[0x90, 60, 90, 61, 91, 62, 92], &mut batch);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.dropped(), 1);
        assert!(batch.overflowed());
        let events: Vec<_> = batch.iter().collect();
        assert_eq!(events[0].timestamp_micros, 1234);
        assert_eq!(events[1].message, LiveMidiMessage::NoteOn { channel: 0, pitch: 61, velocity: 91 });
        batch.clear();
        assert!(batch.is_empty());
        assert!(!batch.overflowed());
    }

    #[test]
    fn long_packet_marks_loss_and_resets_running_status() {
        let mut p = LiveMidiParser::default();
        let mut batch = MidiEventBatch::<1>::default();
        let bytes = vec![0xF0; MAX_LIVE_PACKET_BYTES + 1];
        p.push_packet(0, &bytes, &mut batch);
        assert!(batch.overflowed());
        assert_eq!(p.push_byte(60), None);
    }

    #[test]
    fn take_retriggers_and_records_controllers_without_stuck_notes() {
        let mut take = LiveMidiTake::default();
        let on = LiveMidiMessage::NoteOn { channel: 1, pitch: 60, velocity: 100 };
        assert!(take.record(0, on).is_ok());
        assert!(take.record(120, on).is_ok());
        assert!(take.record(190, LiveMidiMessage::ControlChange { channel: 1, controller: 64, value: 127 }).is_ok());
        assert!(take.record(360, LiveMidiMessage::NoteOff { channel: 1, pitch: 60, velocity: 50 }).is_ok());
        assert!(take.record(380, LiveMidiMessage::NoteOn { channel: 0, pitch: 67, velocity: 80 }).is_ok());
        let seq = take.finish(500).unwrap_or_default();
        assert_eq!(seq.notes.len(), 3);
        assert_eq!((seq.notes[0].start, seq.notes[0].length), (0, 120));
        assert_eq!((seq.notes[1].start, seq.notes[1].length), (120, 240));
        assert_eq!((seq.notes[2].start, seq.notes[2].length), (380, 120));
        assert_eq!(seq.notes[1].release_velocity, 50);
        assert_eq!(seq.ctrls[0].kind, CtrlKind::Cc(64));
    }

    #[test]
    fn timed_recording_and_invalid_values() {
        let mut take = LiveMidiTake::default();
        let on = LiveMidiMessage::NoteOn { channel: 0, pitch: 60, velocity: 90 };
        let off = LiveMidiMessage::NoteOff { channel: 0, pitch: 60, velocity: 64 };
        assert!(take.record_timed(TimedMidiMessage { timestamp_micros: 501000, message: on }, 1000, 0, 120000).is_ok());
        assert!(take.record_timed(TimedMidiMessage { timestamp_micros: 1001000, message: off }, 1000, 0, 120000).is_ok());
        let seq = take.finish(1920).unwrap_or_default();
        assert_eq!((seq.notes[0].start, seq.notes[0].length), (960, 960));
        assert!(micros_to_ticks(1000, 0).is_err());
        assert_eq!(micros_to_ticks(u64::MAX, u32::MAX), Ok(i64::MAX));
        let mut bad = LiveMidiTake::default();
        assert!(bad.record(0, LiveMidiMessage::NoteOn { channel: 16, pitch: 60, velocity: 90 }).is_err());
        assert!(bad.record(-1, on).is_err());
    }
}
