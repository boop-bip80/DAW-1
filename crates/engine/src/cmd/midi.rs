//! MIDI note editing (MIDI Editor, pencil on MIDI tracks).
use super::{CommandSpec, always, bad, clip_id, has_tracks, i64_or, range_param, tracks_required};

use crate::{Engine, Result, cmd};
use serde_json::{Value, json};
use soundcraft_midi::{CtrlKind, Note, Sequence, live::MAX_LIVE_TAKE_EVENTS};
use soundcraft_model::{Clip, ClipContent, ClipId, TrackId};
use soundcraft_time::{MAX_POSITION, TICKS_PER_QUARTER};
use std::collections::HashSet;

const MAX_RECORDED_TRACKS: usize = 64;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "midi.notes", "List MIDI Notes", [], None, "{clip}", always, notes),
        cmd!(
            "midi.new_clip",
            "New MIDI Clip",
            ["Clip"],
            None,
            "{track, start?, end?|length?} (default: the edit selection, or 1 bar)",
            has_tracks,
            new_clip
        ),
        cmd!("midi.note_add", "Add Note", [], None, "{clip, pitch, start_ticks, length_ticks?: 480, velocity?: 100, channel?: 0}", always, note_add),
        cmd!("midi.note_edit", "Edit Note", [], None, "{clip, index, pitch?, start_ticks?, length_ticks?, velocity?}", always, note_edit),
        cmd!("midi.note_delete", "Delete Notes", [], None, "{clip, indices: [n]}", always, note_delete),
        cmd!("midi.notes_move", "Move Notes", [], None, "{clip, indices: [n], ticks?: 0, semitones?: 0}", always, notes_move),
        // A single undo step for every track in a recording pass. The large sample payload is
        // deliberately excluded from the command journal (the undo snapshot already owns it).
        cmd!(@spec false, true, false;
            "midi.record_take", "Commit MIDI Recording", [], None,
            "{start, end, takes: [{track, sequence: {notes, ctrls}}]} — validated MIDI takes in samples",
            always, record_take),
    ]
}

/// A completed live MIDI pass is inserted atomically across tracks. All input is validated
/// before the first document mutation, so malformed controller values or an invalid track
/// cannot silently leave half a multi-track performance behind.
fn record_take(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "midi.record_take";
    let start = p.get("start").and_then(Value::as_i64).ok_or_else(|| bad(CMD, "start sample required"))?;
    let end = p.get("end").and_then(Value::as_i64).ok_or_else(|| bad(CMD, "end sample required"))?;
    if !(0..MAX_POSITION).contains(&start) || end <= start || end > MAX_POSITION {
        return Err(bad(CMD, "invalid MIDI recording sample range"));
    }
    let raw_takes = p.get("takes").and_then(Value::as_array).ok_or_else(|| bad(CMD, "takes array required"))?;
    if raw_takes.is_empty() || raw_takes.len() > MAX_RECORDED_TRACKS {
        return Err(bad(CMD, "expected 1..=64 MIDI tracks"));
    }

    let s = e.session();
    let start_tick = s.tempo.samples_to_ticks(start, s.sample_rate);
    let end_tick = s.tempo.samples_to_ticks(end, s.sample_rate);
    let length_ticks = end_tick.saturating_sub(start_tick).max(1);
    let mut seen = HashSet::new();
    let mut parsed = Vec::with_capacity(raw_takes.len());
    let mut total_events = 0usize;
    for raw in raw_takes {
        let id = TrackId(raw.get("track").and_then(Value::as_u64).ok_or_else(|| bad(CMD, "take track id required"))?);
        if !seen.insert(id) {
            return Err(bad(CMD, "a recording contains duplicate track ids"));
        }
        let track = s.track(id).ok_or_else(|| bad(CMD, "record target track no longer exists"))?;
        if !track.kind.is_midi() || track.inactive {
            return Err(bad(CMD, "recording target must be an active MIDI or instrument track"));
        }
        let raw_seq = raw.get("sequence").ok_or_else(|| bad(CMD, "sequence required"))?;
        let notes_len = raw_seq.get("notes").and_then(Value::as_array).map(Vec::len).ok_or_else(|| bad(CMD, "notes array required"))?;
        let ctrls_len = raw_seq.get("ctrls").and_then(Value::as_array).map(Vec::len).ok_or_else(|| bad(CMD, "ctrls array required"))?;
        total_events = total_events.saturating_add(notes_len).saturating_add(ctrls_len);
        if total_events > MAX_LIVE_TAKE_EVENTS {
            return Err(bad(CMD, "MIDI take exceeds the supported event limit"));
        }
        let sequence: Sequence = serde_json::from_value(raw_seq.clone()).map_err(|_| bad(CMD, "invalid MIDI take sequence"))?;
        for n in &sequence.notes {
            if n.pitch > 127
                || n.channel > 15
                || !(1..=127).contains(&n.velocity)
                || n.release_velocity > 127
                || n.start < 0
                || n.length <= 0
                || n.end() > length_ticks
            {
                return Err(bad(CMD, "invalid MIDI note or out-of-range note time"));
            }
        }
        for c in &sequence.ctrls {
            if c.channel > 15 || c.tick < 0 || c.tick >= length_ticks || !valid_recorded_ctrl(c.kind, c.value) {
                return Err(bad(CMD, "invalid MIDI controller or out-of-range event time"));
            }
        }
        parsed.push((id, sequence));
    }

    let s = e.session_mut();
    let mut ids = Vec::with_capacity(parsed.len());
    for (track, mut sequence) in parsed {
        sequence.sort();
        let id = s.new_clip_id();
        let name = s.track(track).map(|t| format!("{} Take", t.name)).unwrap_or_else(|| "MIDI Take".into());
        crate::edit::place_clip(s, track, Clip::midi(id, name, start, end.saturating_sub(start), sequence));
        ids.push(id.0);
    }
    Ok(json!({"clips": ids, "start": start, "end": end}))
}

fn valid_recorded_ctrl(kind: CtrlKind, value: i32) -> bool {
    match kind {
        CtrlKind::Cc(n) => n < 128 && (0..=127).contains(&value),
        CtrlKind::PitchBend => (-8192..=8191).contains(&value),
        CtrlKind::ChannelPressure | CtrlKind::Program => (0..=127).contains(&value),
        CtrlKind::PolyPressure(n) => n < 128 && (0..=127).contains(&value),
    }
}

fn seq_mut<'a>(e: &'a mut Engine, id: ClipId, cmd: &str) -> Result<&'a mut Sequence> {
    match e.session_mut().find_clip_mut(id).map(|c| &mut c.content) {
        Some(ClipContent::Midi { sequence }) => Ok(sequence),
        Some(_) => Err(bad(cmd, "not a MIDI clip")),
        None => Err(bad(cmd, "no such clip")),
    }
}

fn notes(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.notes")?;
    match e.session().find_clip(id).map(|(_, c)| &c.content) {
        Some(ClipContent::Midi { sequence }) => Ok(json!({"notes": sequence.notes})),
        _ => Err(bad("midi.notes", "not a MIDI clip")),
    }
}

fn new_clip(e: &mut Engine, p: &Value) -> Result<Value> {
    let t = tracks_required(e, "midi.new_clip", p)?.first().copied().ok_or_else(|| bad("midi.new_clip", "no track"))?;
    if !e.session().track(t).is_some_and(|x| x.kind.is_midi()) {
        return Err(bad("midi.new_clip", "not a MIDI or instrument track"));
    }
    let mut r = range_param(e, "midi.new_clip", p)?;
    if r.is_empty() {
        let s = e.session();
        let bar = soundcraft_time::GridValue::Note { value: soundcraft_time::NoteValue::Bar, dotted: false, triplet: false };
        let start = bar.line_at_or_before(r.start, s.sample_rate, &s.tempo, s.frame_rate);
        r = soundcraft_time::Range::new(start, bar.next_line(start, s.sample_rate, &s.tempo, s.frame_rate));
    }
    let s = e.session_mut();
    let id = s.new_clip_id();
    let name = s.track(t).map(|x| x.name.clone()).unwrap_or_default();
    crate::edit::place_clip(s, t, Clip::midi(id, name, r.start, r.len(), Sequence::default()));
    Ok(json!({"clip": id, "start": r.start, "end": r.end}))
}

fn note_add(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_add")?;
    let n = Note {
        pitch: i64_or(p, "pitch", 60).clamp(0, 127) as u8,
        velocity: i64_or(p, "velocity", 100).clamp(1, 127) as u8,
        release_velocity: 64,
        channel: i64_or(p, "channel", 0).clamp(0, 15) as u8,
        start: i64_or(p, "start_ticks", 0).max(0),
        length: i64_or(p, "length_ticks", TICKS_PER_QUARTER / 2).max(1),
    };
    let seq = seq_mut(e, id, "midi.note_add")?;
    seq.notes.push(n);
    seq.sort();
    let index = seq.notes.iter().position(|x| *x == n).unwrap_or(0);
    Ok(json!({"index": index, "notes": seq.notes.len()}))
}

fn note_edit(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_edit")?;
    let idx = p.get("index").and_then(Value::as_u64).ok_or_else(|| bad("midi.note_edit", "`index` required"))? as usize;
    let seq = seq_mut(e, id, "midi.note_edit")?;
    let n = seq.notes.get_mut(idx).ok_or_else(|| bad("midi.note_edit", "index out of range"))?;
    if let Some(v) = p.get("pitch").and_then(Value::as_i64) {
        n.pitch = v.clamp(0, 127) as u8;
    }
    if let Some(v) = p.get("start_ticks").and_then(Value::as_i64) {
        n.start = v.max(0);
    }
    if let Some(v) = p.get("length_ticks").and_then(Value::as_i64) {
        n.length = v.max(1);
    }
    if let Some(v) = p.get("velocity").and_then(Value::as_i64) {
        n.velocity = v.clamp(1, 127) as u8;
    }
    let edited = *n;
    seq.sort();
    Ok(json!({"note": edited, "index": seq.notes.iter().position(|x| *x == edited)}))
}

fn indices(p: &Value) -> Vec<usize> {
    p.get("indices").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect()).unwrap_or_default()
}

fn note_delete(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.note_delete")?;
    let idx = indices(p);
    let seq = seq_mut(e, id, "midi.note_delete")?;
    let before = seq.notes.len();
    let mut i = 0;
    seq.notes.retain(|_| {
        let keep = !idx.contains(&i);
        i += 1;
        keep
    });
    Ok(json!({"deleted": before - seq.notes.len()}))
}

fn notes_move(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = clip_id(p, "midi.notes_move")?;
    let idx = indices(p);
    let dt = i64_or(p, "ticks", 0);
    let ds = i64_or(p, "semitones", 0);
    let seq = seq_mut(e, id, "midi.notes_move")?;
    for (i, n) in seq.notes.iter_mut().enumerate() {
        if idx.contains(&i) {
            n.start = n.start.saturating_add(dt).max(0);
            n.pitch = i64::from(n.pitch).saturating_add(ds).clamp(0, 127) as u8;
        }
    }
    let moved: Vec<Note> = seq.notes.iter().enumerate().filter(|(i, _)| idx.contains(i)).map(|(_, n)| *n).collect();
    seq.sort();
    // Where the moved notes ended up after sorting, so the editor can keep them selected.
    let mut taken = vec![false; seq.notes.len()];
    let mut indices = Vec::with_capacity(moved.len());
    for m in &moved {
        if let Some(j) = seq.notes.iter().enumerate().position(|(j, n)| !taken[j] && n == m) {
            taken[j] = true;
            indices.push(j);
        }
    }
    indices.sort_unstable();
    Ok(json!({"moved": idx.len(), "indices": indices}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, TrackKind};

    #[test]
    fn create_clip_add_edit_move_delete() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let r = e.execute("midi.new_clip", &json!({"track": t.0})).unwrap();
        let clip = r["clip"].as_u64().unwrap();
        e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": 0})).unwrap();
        e.execute("midi.note_add", &json!({"clip": clip, "pitch": 64, "start_ticks": 480})).unwrap();
        e.execute("midi.note_edit", &json!({"clip": clip, "index": 0, "velocity": 30})).unwrap();
        e.execute("midi.notes_move", &json!({"clip": clip, "indices": [1], "semitones": 3})).unwrap();
        let n = e.execute("midi.notes", &json!({"clip": clip})).unwrap();
        assert_eq!(n["notes"][0]["velocity"], 30);
        assert_eq!(n["notes"][1]["pitch"], 67);
        e.execute("midi.note_delete", &json!({"clip": clip, "indices": [0]})).unwrap();
        assert_eq!(e.execute("midi.notes", &json!({"clip": clip})).unwrap()["notes"].as_array().map(Vec::len), Some(1));
        assert!(e.execute("midi.note_edit", &json!({"clip": clip, "index": 99})).is_err());
    }

    #[test]
    fn notes_move_huge_params_do_not_overflow() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let r = e.execute("midi.new_clip", &json!({"track": t.0})).unwrap();
        let clip = r["clip"].as_u64().unwrap();
        e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": 480})).unwrap();
        e.execute("midi.notes_move", &json!({"clip": clip, "indices": [0], "ticks": i64::MAX})).unwrap();
        let n = e.execute("midi.notes", &json!({"clip": clip})).unwrap();
        assert_eq!(n["notes"][0]["start"], i64::MAX);
        e.execute("midi.notes_move", &json!({"clip": clip, "indices": [0], "semitones": i64::MAX})).unwrap();
        let n = e.execute("midi.notes", &json!({"clip": clip})).unwrap();
        assert_eq!(n["notes"][0]["pitch"], 127);
    }

    #[test]
    fn notes_move_reports_where_the_moved_notes_went() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let clip = e.execute("midi.new_clip", &json!({"track": t.0})).unwrap()["clip"].as_u64().unwrap();
        for start in [0, 480, 960] {
            e.execute("midi.note_add", &json!({"clip": clip, "pitch": 60, "start_ticks": start})).unwrap();
        }
        let r = e.execute("midi.notes_move", &json!({"clip": clip, "indices": [0], "ticks": 720})).unwrap();
        assert_eq!(r["indices"], json!([1]));
        let notes = e.execute("midi.notes", &json!({"clip": clip})).unwrap();
        assert_eq!(notes["notes"][1]["start"], 720);
    }

    #[test]
    fn rejects_audio_tracks() {
        let mut e = Engine::default();
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        assert!(e.execute("midi.new_clip", &json!({"track": t.0})).is_err());
    }

    #[test]
    fn recorded_multitrack_take_validates_atomically_and_undoes_together() {
        let mut e = Engine::default();
        let first = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let second = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Stereo, Some("MIDI"));
        let audio = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Not MIDI"));
        let one_note = json!({"notes": [{
            "pitch": 60, "velocity": 100, "release_velocity": 64,
            "channel": 0, "start": 0, "length": 480
        }], "ctrls": []});
        let takes = json!({"start": 0, "end": 48_000, "takes": [
            {"track": first.0, "sequence": one_note},
            {"track": audio.0, "sequence": {"notes": [], "ctrls": []}}
        ]});
        assert!(e.execute("midi.record_take", &takes).is_err());
        assert!(e.session().track(first).is_some_and(|t| t.clips().is_empty()));
        assert!(e.session().track(audio).is_some_and(|t| t.clips().is_empty()));

        let result = e
            .execute(
                "midi.record_take",
                &json!({"start": 0, "end": 48_000, "takes": [
                    {"track": first.0, "sequence": one_note},
                    {"track": second.0, "sequence": {"notes": [], "ctrls": []}}
                ]}),
            )
            .unwrap();
        assert_eq!(result["clips"].as_array().map(Vec::len), Some(2));
        assert_eq!(e.session().track(first).map(|t| t.clips().len()), Some(1));
        assert_eq!(e.session().track(second).map(|t| t.clips().len()), Some(1));
        assert!(e.undo());
        assert!(e.session().track(first).is_some_and(|t| t.clips().is_empty()));
        assert!(e.session().track(second).is_some_and(|t| t.clips().is_empty()));
    }
}
