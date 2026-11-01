//! Pure, bounded translation of routed MIDI clips into short output messages.
//!
//! This is **not** a device or transport scheduler. It examines an immutable
//! session and returns sample-positioned events for a future native-output
//! runtime to map onto its best-effort monotonic clock. No MIDI devices are
//! opened, packets are sent, or work is added to the audio callback here.
//! A track without an explicit `midi_output` route never generates events.

use soundcraft_midi::CtrlKind;
use soundcraft_model::{Clip, ClipContent, Session, Track, TrackId, TrackKind};
use soundcraft_time::{MAX_POSITION, Range, Samples};
use thiserror::Error;

/// Hard limit across all routed tracks within one requested scheduling window.
pub const MAX_MIDI_OUTPUT_WINDOW_EVENTS: usize = 65_536;
/// Bound CPU spent scanning arbitrarily dense imported or corrupted clip data.
pub const MAX_MIDI_OUTPUT_SCANNED_EVENTS: usize = 500_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MidiOutputCollectError {
    #[error("invalid MIDI output sample window")]
    InvalidRange,
    #[error("MIDI output window exceeds {MAX_MIDI_OUTPUT_WINDOW_EVENTS} events")]
    TooManyEvents,
    #[error("MIDI output window exceeds {MAX_MIDI_OUTPUT_SCANNED_EVENTS} inspected events")]
    TooManyScannedEvents,
    #[error("invalid external MIDI route on track {0}")]
    InvalidRoute(u64),
    #[error("invalid MIDI {reason} on track {track}, clip {clip}")]
    InvalidMidi { track: u64, clip: u64, reason: &'static str },
}

/// A MIDI 1.0 channel message at a session sample position. The channel/status
/// is already encoded, but `len` must be respected: Program Change and Channel
/// Pressure are **two-byte** messages, unlike three-byte plugin MIDI buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiOutputEvent {
    pub at_samples: Samples,
    pub track: TrackId,
    pub bytes: [u8; 3],
    pub len: u8,
}

impl MidiOutputEvent {
    pub fn message(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

struct OrderedEvent {
    event: MidiOutputEvent,
    /// Note-off before controller before note-on at the exact same sample.
    priority: u8,
    received: usize,
}

fn push(out: &mut Vec<OrderedEvent>, event: MidiOutputEvent, priority: u8) -> Result<(), MidiOutputCollectError> {
    if out.len() >= MAX_MIDI_OUTPUT_WINDOW_EVENTS {
        return Err(MidiOutputCollectError::TooManyEvents);
    }
    out.push(OrderedEvent { event, priority, received: out.len() });
    Ok(())
}

fn midi_offset(s: &Session, track: &Track) -> Samples {
    s.edit.value(&format!("midi.offset.{}", track.id.0), 0.0) as i64
}

#[derive(Debug, Clone, Copy)]
struct MidiTransform {
    velocity: i64,
    transpose: i64,
    duration: f64,
    delay_ticks: i64,
    offset_samples: Samples,
}

impl MidiTransform {
    fn for_track(s: &Session, track: &Track) -> Self {
        let property = |name: &str, default| s.edit.value(&format!("rtp.{}.{name}", track.id.0), default);
        Self {
            velocity: property("velocity", 0.0).clamp(-127.0, 127.0) as i64,
            transpose: property("transpose", 0.0).clamp(-127.0, 127.0) as i64,
            duration: property("duration", 100.0).clamp(1.0, 1000.0) / 100.0,
            delay_ticks: property("delay", 0.0).clamp(-1e7, 1e7) as i64,
            offset_samples: midi_offset(s, track),
        }
    }
}

fn invalid(track: &Track, clip: &Clip, reason: &'static str) -> MidiOutputCollectError {
    MidiOutputCollectError::InvalidMidi { track: track.id.0, clip: clip.id.0, reason }
}

fn wire_controller(kind: CtrlKind, value: i32, transpose: i64, channel: u8) -> Option<([u8; 3], u8)> {
    if channel > 15 {
        return None;
    }
    let status = channel;
    match kind {
        CtrlKind::Cc(number) if number < 128 && (0..=127).contains(&value) => Some(([0xB0 | status, number, value as u8], 3)),
        CtrlKind::PitchBend if (-8192..=8191).contains(&value) => {
            let signed = value + 8192;
            Some(([0xE0 | status, (signed & 0x7F) as u8, ((signed >> 7) & 0x7F) as u8], 3))
        }
        CtrlKind::ChannelPressure if (0..=127).contains(&value) => Some(([0xD0 | status, value as u8, 0], 2)),
        CtrlKind::PolyPressure(pitch) if pitch < 128 && (0..=127).contains(&value) => {
            Some(([0xA0 | status, (i64::from(pitch) + transpose).clamp(0, 127) as u8, value as u8], 3))
        }
        CtrlKind::Program if (0..=127).contains(&value) => Some(([0xC0 | status, value as u8, 0], 2)),
        _ => None,
    }
}

fn emit_clip(
    s: &Session,
    track: &Track,
    clip: &Clip,
    window: Range,
    channel_override: Option<u8>,
    transform: MidiTransform,
    out: &mut Vec<OrderedEvent>,
) -> Result<(), MidiOutputCollectError> {
    let ClipContent::Midi { sequence } = &clip.content else { return Ok(()) };
    let start = clip.start.saturating_add(transform.offset_samples);
    let end = clip.end().saturating_add(transform.offset_samples);
    // The ending boundary is inclusive for note-offs: a note clipped to the
    // exact clip end still needs its off in the *next* half-open window.
    if clip.muted || end < window.start || start >= window.end || end <= start {
        return Ok(());
    }
    let base = s.tempo.samples_to_ticks(clip.start, s.sample_rate);
    for note in &sequence.notes {
        if note.pitch > 127
            || note.velocity == 0
            || note.velocity > 127
            || note.release_velocity > 127
            || note.channel > 15
            || note.start < 0
            || note.length <= 0
        {
            return Err(invalid(track, clip, "note"));
        }
        let channel = channel_override.unwrap_or(note.channel);
        let tick = base.saturating_add(note.start.saturating_add(transform.delay_ticks).max(0));
        let on = s.tempo.tick_to_samples(tick, s.sample_rate).saturating_add(transform.offset_samples);
        let transformed_len = ((note.length as f64 * transform.duration) as i64).max(1);
        let off =
            s.tempo.tick_to_samples(tick.saturating_add(transformed_len), s.sample_rate).min(clip.end()).saturating_add(transform.offset_samples);
        if on < start || on >= end || off <= on {
            continue;
        }
        let pitch = (i64::from(note.pitch) + transform.transpose).clamp(0, 127) as u8;
        let velocity = (i64::from(note.velocity) + transform.velocity).clamp(1, 127) as u8;
        if (window.start..window.end).contains(&on) {
            push(out, MidiOutputEvent { at_samples: on, track: track.id, bytes: [0x90 | channel, pitch, velocity], len: 3 }, 2)?;
        }
        if (window.start..window.end).contains(&off) {
            push(out, MidiOutputEvent { at_samples: off, track: track.id, bytes: [0x80 | channel, pitch, note.release_velocity], len: 3 }, 0)?;
        }
    }
    for ctrl in &sequence.ctrls {
        let channel = channel_override.unwrap_or(ctrl.channel);
        let Some((bytes, len)) = wire_controller(ctrl.kind, ctrl.value, transform.transpose, channel) else {
            return Err(invalid(track, clip, "controller"));
        };
        if ctrl.tick < 0 {
            return Err(invalid(track, clip, "controller time"));
        }
        let at = s
            .tempo
            .tick_to_samples(base.saturating_add(ctrl.tick.saturating_add(transform.delay_ticks).max(0)), s.sample_rate)
            .saturating_add(transform.offset_samples);
        if at >= start && at < end && (window.start..window.end).contains(&at) {
            push(out, MidiOutputEvent { at_samples: at, track: track.id, bytes, len }, 1)?;
        }
    }
    Ok(())
}

fn track_audible(s: &Session, track: &Track, any_solo: bool) -> bool {
    if !track.kind.is_midi() || track.inactive || track.mixer.mute || (any_solo && !track.mixer.solo && !track.mixer.solo_safe) {
        return false;
    }
    if let Some(vca) = track.mixer.vca
        && s.tracks.iter().any(|candidate| candidate.id.0 == vca && candidate.kind == TrackKind::Vca && candidate.mixer.mute)
    {
        return false;
    }
    true
}

/// Return sample-ordered short messages for explicitly routed MIDI/instrument
/// tracks in `[window.start, window.end)`. No hardware I/O or realtime-callback
/// work occurs. Bad session data and oversized windows fail before any caller
/// can dispatch a *partial* list.
pub fn collect_midi_output_window(s: &Session, window: Range) -> Result<Vec<MidiOutputEvent>, MidiOutputCollectError> {
    if window.start < 0 || window.end <= window.start || window.end > MAX_POSITION {
        return Err(MidiOutputCollectError::InvalidRange);
    }
    let any_solo = s.tracks.iter().any(|track| track.mixer.solo && !track.inactive);
    let mut events = Vec::new();
    let mut scanned = 0usize;
    for track in &s.tracks {
        let Some(route) = &track.midi_output else { continue };
        if route.port.is_empty() || route.channel.is_some_and(|channel| !(1..=16).contains(&channel)) {
            return Err(MidiOutputCollectError::InvalidRoute(track.id.0));
        }
        if !track_audible(s, track, any_solo) {
            continue;
        }
        let transform = MidiTransform::for_track(s, track);
        for clip in track.clips() {
            let clip_start = clip.start.saturating_add(transform.offset_samples);
            let clip_end = clip.end().saturating_add(transform.offset_samples);
            if clip.muted || clip_end < window.start || clip_start >= window.end || clip_end <= clip_start {
                continue;
            }
            if let ClipContent::Midi { sequence } = &clip.content {
                scanned = scanned.saturating_add(sequence.notes.len()).saturating_add(sequence.ctrls.len());
                if scanned > MAX_MIDI_OUTPUT_SCANNED_EVENTS {
                    return Err(MidiOutputCollectError::TooManyScannedEvents);
                }
                emit_clip(s, track, clip, window, route.channel.map(|channel| channel - 1), transform, &mut events)?;
            }
        }
    }
    events.sort_by_key(|item| (item.event.at_samples, item.priority, item.event.track.0, item.received));
    Ok(events.into_iter().map(|item| item.event).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_midi::{CtrlEvent, Note, Sequence};
    use soundcraft_model::{ChannelFormat, MidiOutputRoute};

    fn note(pitch: u8, channel: u8, start: i64, length: i64) -> Note {
        Note { pitch, velocity: 100, release_velocity: 64, channel, start, length }
    }

    fn session_with_clip(sequence: Sequence, length_samples: Samples) -> (Session, TrackId) {
        let mut session = Session::default();
        let track = session.add_track(TrackKind::Midi, ChannelFormat::Mono, Some("External"));
        let clip = Clip::midi(session.new_clip_id(), "MIDI", 0, length_samples, sequence);
        let Some(playlist) = session.track_mut(track).and_then(Track::playlist_mut) else { panic!("new MIDI track must have a playlist") };
        playlist.clips.push(clip);
        (session, track)
    }

    fn route(session: &mut Session, track: TrackId, channel: Option<u8>) {
        let Some(track) = session.track_mut(track) else { panic!("synthetic route track") };
        track.midi_output = Some(MidiOutputRoute { port: "Explicit Fake Synth".into(), channel });
    }

    fn messages(events: &[MidiOutputEvent]) -> Vec<(Samples, Vec<u8>)> {
        events.iter().map(|event| (event.at_samples, event.message().to_vec())).collect()
    }

    #[test]
    fn legacy_none_route_is_silent_and_all_controllers_keep_wire_lengths() {
        let sequence = Sequence {
            notes: vec![note(60, 2, 0, 480)],
            ctrls: vec![
                CtrlEvent { tick: 240, channel: 2, kind: CtrlKind::Cc(7), value: 100 },
                CtrlEvent { tick: 240, channel: 2, kind: CtrlKind::Program, value: 12 },
                CtrlEvent { tick: 240, channel: 2, kind: CtrlKind::ChannelPressure, value: 32 },
                CtrlEvent { tick: 240, channel: 2, kind: CtrlKind::PitchBend, value: -8192 },
                CtrlEvent { tick: 240, channel: 2, kind: CtrlKind::PolyPressure(61), value: 55 },
            ],
        };
        let (mut session, id) = session_with_clip(sequence, 24_000);
        assert!(collect_midi_output_window(&session, Range::new(0, 24_001)).unwrap().is_empty(), "no route must mean no hardware output");
        route(&mut session, id, None);
        let events = collect_midi_output_window(&session, Range::new(0, 24_001)).unwrap();
        assert!(events.iter().all(|event| event.track == id));
        assert_eq!(
            messages(&events),
            vec![
                (0, vec![0x92, 60, 100]),
                (6000, vec![0xB2, 7, 100]),
                (6000, vec![0xC2, 12]),
                (6000, vec![0xD2, 32]),
                (6000, vec![0xE2, 0, 0]),
                (6000, vec![0xA2, 61, 55]),
                (12_000, vec![0x82, 60, 64]),
            ]
        );
    }

    #[test]
    fn clip_end_note_off_survives_next_half_open_window_and_ties_prioritize_off() {
        let sequence = Sequence { notes: vec![note(60, 0, 0, 960), note(61, 0, 480, 480)], ctrls: Vec::new() };
        let (mut session, id) = session_with_clip(sequence, 12_000);
        route(&mut session, id, None);
        assert_eq!(messages(&collect_midi_output_window(&session, Range::new(0, 12_000)).unwrap()), vec![(0, vec![0x90, 60, 100])]);
        assert_eq!(messages(&collect_midi_output_window(&session, Range::new(12_000, 12_001)).unwrap()), vec![(12_000, vec![0x80, 60, 64])]);

        let sequence = Sequence { notes: vec![note(60, 0, 0, 480), note(60, 0, 480, 480)], ctrls: Vec::new() };
        let (mut retrigger, id) = session_with_clip(sequence, 48_000);
        route(&mut retrigger, id, None);
        assert_eq!(
            messages(&collect_midi_output_window(&retrigger, Range::new(12_000, 12_001)).unwrap()),
            vec![(12_000, vec![0x80, 60, 64]), (12_000, vec![0x90, 60, 100])]
        );
    }

    #[test]
    fn tempo_offset_and_realtime_properties_transform_events_on_selected_channel() {
        let sequence =
            Sequence { notes: vec![note(60, 3, 0, 480)], ctrls: vec![CtrlEvent { tick: 240, channel: 3, kind: CtrlKind::Cc(1), value: 64 }] };
        let (mut session, id) = session_with_clip(sequence, 48_000);
        route(&mut session, id, Some(10));
        session.edit.values.insert(format!("midi.offset.{}", id.0), 240.0);
        session.edit.values.insert(format!("rtp.{}.velocity", id.0), 20.0);
        session.edit.values.insert(format!("rtp.{}.transpose", id.0), 2.0);
        session.edit.values.insert(format!("rtp.{}.duration", id.0), 50.0);
        session.edit.values.insert(format!("rtp.{}.delay", id.0), 120.0);
        let output = collect_midi_output_window(&session, Range::new(0, 24_000)).unwrap();
        assert_eq!(messages(&output), vec![(3240, vec![0x99, 62, 120]), (9240, vec![0x89, 62, 64]), (9240, vec![0xB9, 1, 64]),]);
    }

    #[test]
    fn tempo_change_inside_a_note_uses_the_session_map_for_on_off_and_controllers() {
        let seq = Sequence { notes: vec![note(67, 1, 0, 960)], ctrls: vec![CtrlEvent { tick: 720, channel: 1, kind: CtrlKind::Cc(11), value: 99 }] };
        let (mut session, id) = session_with_clip(seq, 48_000);
        route(&mut session, id, None);
        session.tempo.set_tempo(480, 60.0).unwrap();
        assert_eq!(
            messages(&collect_midi_output_window(&session, Range::new(0, 48_000)).unwrap()),
            vec![(0, vec![0x91, 67, 100]), (24_000, vec![0xB1, 11, 99]), (36_000, vec![0x81, 67, 64])]
        );
    }

    #[test]
    fn muted_soloed_and_inactive_tracks_do_not_generate_output() {
        let (mut session, id) = session_with_clip(Sequence { notes: vec![note(60, 0, 0, 480)], ctrls: Vec::new() }, 48_000);
        route(&mut session, id, None);
        let window = Range::new(0, 24_000);
        assert_eq!(collect_midi_output_window(&session, window).unwrap().len(), 2);
        session.track_mut(id).unwrap().mixer.mute = true;
        assert!(collect_midi_output_window(&session, window).unwrap().is_empty());
        session.track_mut(id).unwrap().mixer.mute = false;
        session.track_mut(id).unwrap().inactive = true;
        assert!(collect_midi_output_window(&session, window).unwrap().is_empty());
        session.track_mut(id).unwrap().inactive = false;
        let other = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Solo Audio"));
        session.track_mut(other).unwrap().mixer.solo = true;
        assert!(collect_midi_output_window(&session, window).unwrap().is_empty());
        session.track_mut(id).unwrap().mixer.solo_safe = true;
        assert_eq!(collect_midi_output_window(&session, window).unwrap().len(), 2);
    }

    #[test]
    fn invalid_document_and_window_fail_atomically_without_partial_output() {
        let (mut session, id) = session_with_clip(Sequence { notes: vec![note(60, 0, 0, 480)], ctrls: Vec::new() }, 48_000);
        route(&mut session, id, None);
        assert!(matches!(collect_midi_output_window(&session, Range::new(2, 2)), Err(MidiOutputCollectError::InvalidRange)));
        {
            let Some(track) = session.track_mut(id) else { panic!("track exists") };
            let Some(clip) = track.playlist_mut().and_then(|playlist| playlist.clips.first_mut()) else { panic!("clip exists") };
            let ClipContent::Midi { sequence } = &mut clip.content else { panic!("MIDI clip") };
            sequence.notes.push(note(64, 16, 0, 480));
        }
        assert!(matches!(collect_midi_output_window(&session, Range::new(0, 48_000)), Err(MidiOutputCollectError::InvalidMidi { .. })));
        {
            let Some(track) = session.track_mut(id) else { panic!("track exists") };
            let Some(clip) = track.playlist_mut().and_then(|playlist| playlist.clips.first_mut()) else { panic!("clip exists") };
            let ClipContent::Midi { sequence } = &mut clip.content else { panic!("MIDI clip") };
            let _ = sequence.notes.pop();
            sequence.ctrls.push(CtrlEvent { tick: 0, channel: 0, kind: CtrlKind::Program, value: 255 });
        }
        assert!(matches!(collect_midi_output_window(&session, Range::new(0, 48_000)), Err(MidiOutputCollectError::InvalidMidi { .. })));
    }

    #[test]
    fn huge_dense_window_hits_hard_output_limit_without_partial_result() {
        let many = vec![note(60, 0, 0, 480); (MAX_MIDI_OUTPUT_WINDOW_EVENTS / 2) + 1];
        let (mut session, id) = session_with_clip(Sequence { notes: many, ctrls: Vec::new() }, 48_000);
        route(&mut session, id, None);
        assert_eq!(collect_midi_output_window(&session, Range::new(0, 48_000)), Err(MidiOutputCollectError::TooManyEvents));
    }
}
