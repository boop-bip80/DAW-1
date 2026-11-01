//! A deliberately narrow CMX 3600 audio Edit Decision List (EDL) interchange.
//!
//! CMX 3600 / SMPTE 258M is a *frame*-based cut list, not an audio project file. This module
//! supports mono (`A`), linked stereo (`AA`), or two independently edited mono lanes (`A1` and
//! `A2`), with straight cuts, source/record trims and common file/name remarks. Existing media
//! must be locally accessible and its channel count must match the event's semantics. Video,
//! fades, dissolves, more than two lanes and source-speed changes are unsupported. AAF/OMF and
//! proprietary Pro Tools sessions are different formats and are not handled here.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use soundcraft_model::{ClipContent, Route, Session, SourceId, Track, TrackId, TrackKind};
use soundcraft_time::FrameRate;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

pub const MAX_EDL_BYTES: usize = 2 * 1024 * 1024;
const MAX_EDITS: usize = 999;
const MAX_LINE_BYTES: usize = 8_384;
const MAX_TIMECODE_HOURS: u64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    pub edits: usize,
    pub unique_sources: usize,
}

pub struct Export {
    pub bytes: Vec<u8>,
    pub summary: ExportSummary,
}

fn unsupported(message: impl Into<String>) -> EngineError {
    EngineError::Unsupported(format!("CMX3600 audio EDL: {}", message.into()))
}

/// Select a whole-frame, non-drop EDL timecode rate, defaulting to the session's setup.
/// The EDL does not self-describe its rate, so importing external EDLs may require `fps`.
pub fn frame_rate(session: &Session, requested: Option<u64>) -> Result<u32> {
    let fps = match requested {
        Some(n @ (24 | 25 | 30)) => u32::try_from(n).unwrap_or(24),
        Some(_) => return Err(unsupported("`fps` must be 24, 25, or 30 non-drop frames per second")),
        None => match session.frame_rate {
            FrameRate::Fps24 => 24,
            FrameRate::Fps25 => 25,
            FrameRate::Fps30 => 30,
            _ => return Err(unsupported("choose `fps: 24`, `25`, or `30`; fractional/drop-frame session rate is not supported")),
        },
    };
    Ok(fps)
}

fn ascii_remark<'a>(value: &'a str, name: &str, limit: usize) -> Result<&'a str> {
    if value.is_empty() || value.len() > limit || !value.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
        return Err(unsupported(format!("{name} must be 1..={limit} printable ASCII characters without newlines")));
    }
    Ok(value)
}

fn sample_frame(samples: i64, rate: u32, fps: u32) -> Result<u64> {
    if samples < 0 || rate == 0 {
        return Err(unsupported("negative sample position or zero session sample rate"));
    }
    let product = i128::from(samples) * i128::from(fps);
    if product % i128::from(rate) != 0 {
        return Err(unsupported("edit is not aligned to a whole EDL frame; move/trim it to the frame grid before export"));
    }
    let frames = u64::try_from(product / i128::from(rate)).map_err(|_| unsupported("EDL timecode overflows"))?;
    if frames >= MAX_TIMECODE_HOURS * 3600 * u64::from(fps) {
        return Err(unsupported("EDL timecode must fit within 24 hours"));
    }
    Ok(frames)
}

fn timecode(frames: u64, fps: u32) -> String {
    let fps = u64::from(fps);
    let ff = frames % fps;
    let seconds = frames / fps;
    format!("{:02}:{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60, ff)
}

/// An EDL describes raw source edits, not the sound of a mixed track. Do not export a cut list
/// that appears to preserve playback when the chosen track contains processing the file cannot
/// carry. Recording arm, input routing and input monitoring are capture setup, not clip effects.
fn reject_track_processing(track: &Track) -> Result<()> {
    let mixer = &track.mixer;
    if mixer.mute || mixer.solo || mixer.solo_safe {
        return Err(unsupported("track mute/solo state is not represented; clear it before exporting"));
    }
    if mixer.volume_db != 0.0 || mixer.trim_db != 0.0 {
        return Err(unsupported("track fader/trim gain must be 0 dB; render the mixed audio before exporting"));
    }
    let default_pan: &[f32] = if track.channels() == 2 { &[-1.0, 1.0] } else { &[0.0] };
    if mixer.phase_invert || mixer.pan.as_slice() != default_pan || mixer.surround.is_some() {
        return Err(unsupported("track phase/pan processing is not represented; render or reset it before exporting"));
    }
    if mixer.inserts.iter().any(Option::is_some) || track.instrument.is_some() {
        return Err(unsupported("track inserts/instrument processing cannot be represented in an EDL"));
    }
    if mixer.sends.iter().any(Option::is_some) {
        return Err(unsupported("configured track sends cannot be represented in an EDL"));
    }
    if mixer.vca.is_some() || !track.automation.is_empty() {
        return Err(unsupported("track VCA assignment or automation lanes cannot be represented in an EDL"));
    }
    if mixer.output != Route::Main {
        return Err(unsupported("track output routing must be the main output for an EDL"));
    }
    if track.frozen || track.elastic.is_some() {
        return Err(unsupported("frozen/Elastic track processing must be rendered before EDL export"));
    }
    Ok(())
}

/// Compatibility entrypoint for a single mono (`A`) or linked-stereo (`AA`) track.
pub fn export_audio(session: &Session, track_id: TrackId, fps: u32) -> Result<Export> {
    export_audio_tracks(session, &[track_id], fps)
}

struct PreparedCut<'a> {
    lane: &'static str,
    source: SourceId,
    name: &'a str,
    media_url: String,
    source_in: u64,
    source_out: u64,
    record_in: u64,
    record_out: u64,
}

/// Export one mono/stereo track, or exactly two independently edited mono tracks (`A1`, `A2`).
///
/// Each lane must begin at zero, have non-overlapping whole-frame cuts, matching channel-count
/// source files and no unrendered processing. Lanes may have simultaneous cuts. Events are
/// interleaved by record position with deterministic A1-before-A2 ordering on ties.
pub fn export_audio_tracks(session: &Session, track_ids: &[TrackId], fps: u32) -> Result<Export> {
    if !(1..=2).contains(&track_ids.len()) || (track_ids.len() == 2 && track_ids[0] == track_ids[1]) {
        return Err(unsupported("choose one mono/stereo track or two different mono audio tracks"));
    }
    let title = ascii_remark(&session.name, "session title", 72)?;
    let rate = session.sample_rate.hz();
    let mut events = Vec::<PreparedCut<'_>>::new();
    for (position, track_id) in track_ids.iter().enumerate() {
        let track = session.track(*track_id).ok_or_else(|| unsupported("selected track does not exist"))?;
        let channels = track.channels();
        if track.kind != TrackKind::Audio || !(1..=2).contains(&channels) || track.inactive || (track_ids.len() == 2 && channels != 1) {
            return Err(unsupported("select one active mono or linked stereo audio track, or two active mono audio tracks"));
        }
        reject_track_processing(track)?;
        let lane = if track_ids.len() == 2 {
            if position == 0 { "A1" } else { "A2" }
        } else if channels == 2 {
            "AA"
        } else {
            "A"
        };
        let mut clips = track.clips().iter().collect::<Vec<_>>();
        clips.sort_by_key(|c| (c.start, c.id.0));
        if clips.is_empty() || clips.len() > MAX_EDITS.saturating_sub(events.len()) {
            return Err(unsupported("EDL requires 1..=999 cuts across the selected tracks, with cuts on each lane"));
        }
        let mut cursor = 0_i64;
        for (number, clip) in clips.iter().enumerate() {
            let ClipContent::Audio { source, offset } = clip.content else {
                return Err(unsupported(format!("clip `{}` is not audio", clip.name)));
            };
            if clip.start < cursor || clip.length <= 0 || offset < 0 {
                return Err(unsupported("overlapping or invalid clips cannot be flattened to CMX3600 cuts"));
            }
            if number == 0 && clip.start != 0 {
                return Err(unsupported("leading silence must be filled or moved to time zero before EDL export"));
            }
            if clip.muted
                || clip.gain_db != 0.0
                || !clip.gain_env.is_empty()
                || clip.fade_in.len != 0
                || clip.fade_out.len != 0
                || !clip.stretch.is_finite()
                || clip.stretch != 1.0
            {
                return Err(unsupported(format!("render unportable mute, gain, fades or stretch on clip `{}` before exporting", clip.name)));
            }
            let end = clip.start.checked_add(clip.length).ok_or_else(|| unsupported("record end overflows"))?;
            let source_end = offset.checked_add(clip.length).ok_or_else(|| unsupported("source end overflows"))?;
            let media = session.source(source).ok_or_else(|| unsupported("clip references missing source"))?;
            if usize::from(media.channels) != channels || media.sample_rate != rate || u64::try_from(source_end).map_or(true, |n| n > media.frames) {
                return Err(unsupported(format!("clip `{}` has no matching {channels}-channel media at the session rate", clip.name)));
            }
            let name = ascii_remark(&clip.name, "clip name", 80)?;
            let (source_in, source_out) = (sample_frame(offset, rate, fps)?, sample_frame(source_end, rate, fps)?);
            let (record_in, record_out) = (sample_frame(clip.start, rate, fps)?, sample_frame(end, rate, fps)?);
            let media_url = crate::otio::media_url(session, media)?;
            events.push(PreparedCut { lane, source, name, media_url, source_in, source_out, record_in, record_out });
            cursor = end;
        }
    }
    events.sort_by_key(|event| (event.record_in, event.lane));
    let mut out = format!("TITLE: {title}\nFCM: NON-DROP FRAME\n\n");
    let mut reels = BTreeMap::<SourceId, usize>::new();
    for (number, event) in events.iter().enumerate() {
        let next_reel = reels.len() + 1;
        let reel = *reels.entry(event.source).or_insert(next_reel);
        let reel = format!("SC{reel:06}");
        writeln!(
            out,
            "{:03}  {reel} {:<5} C        {} {} {} {}",
            number + 1,
            event.lane,
            timecode(event.source_in, fps),
            timecode(event.source_out, fps),
            timecode(event.record_in, fps),
            timecode(event.record_out, fps)
        )
        .map_err(|err| EngineError::Io(format!("EDL formatting failed: {err}")))?;
        writeln!(out, "* FROM CLIP NAME: {}", event.name).map_err(|err| EngineError::Io(format!("EDL formatting failed: {err}")))?;
        writeln!(out, "* FROM FILE: {}\n", event.media_url).map_err(|err| EngineError::Io(format!("EDL formatting failed: {err}")))?;
        if out.len() > MAX_EDL_BYTES {
            return Err(unsupported("EDL text exceeds the 2 MiB limit"));
        }
    }
    Ok(Export { bytes: out.into_bytes(), summary: ExportSummary { edits: events.len(), unique_sources: reels.len() } })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioLane {
    Mono,
    LinkedStereo,
    IndependentA1,
    IndependentA2,
}

impl AudioLane {
    fn channels(self) -> usize {
        if self == Self::LinkedStereo { 2 } else { 1 }
    }

    fn independent(self) -> bool {
        matches!(self, Self::IndependentA1 | Self::IndependentA2)
    }
}

#[derive(Debug)]
struct Cut {
    lane: AudioLane,
    reel: String,
    source_in: u64,
    source_out: u64,
    record_in: u64,
    record_out: u64,
    name: Option<String>,
    media_url: Option<String>,
}

fn read_timecode(value: &str, fps: u32) -> Result<u64> {
    if value.len() != 11 || value.as_bytes().get(2) != Some(&b':') || value.as_bytes().get(5) != Some(&b':') || value.as_bytes().get(8) != Some(&b':')
    {
        return Err(unsupported("expected non-drop HH:MM:SS:FF timecode"));
    }
    let parse = |a, b| value.get(a..b).and_then(|p| p.parse::<u64>().ok()).ok_or_else(|| unsupported("invalid timecode digits"));
    let (h, m, s, f) = (parse(0, 2)?, parse(3, 5)?, parse(6, 8)?, parse(9, 11)?);
    if h >= MAX_TIMECODE_HOURS || m >= 60 || s >= 60 || f >= u64::from(fps) {
        return Err(unsupported("timecode component is outside its valid range"));
    }
    Ok(((h * 60 + m) * 60 + s) * u64::from(fps) + f)
}

fn parse_cut(line: &str, number: usize, fps: u32) -> Result<Cut> {
    let tokens = line.split_ascii_whitespace().collect::<Vec<_>>();
    if tokens.len() != 8 || tokens[0].len() != 3 || tokens[0].parse::<usize>().ok() != Some(number + 1) {
        return Err(unsupported("expected successive three-digit CMX3600 event numbers and eight fields"));
    }
    if tokens[1].is_empty() || tokens[1].len() > 8 || !tokens[1].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(unsupported("source reel must be 1..=8 ASCII letters, digits or underscores"));
    }
    let lane = match tokens[2] {
        "A" => AudioLane::Mono,
        "AA" => AudioLane::LinkedStereo,
        "A1" => AudioLane::IndependentA1,
        "A2" => AudioLane::IndependentA2,
        _ => return Err(unsupported("only mono A, linked stereo AA, or independent mono A1/A2 events are supported")),
    };
    if tokens[3] != "C" {
        return Err(unsupported("only straight cuts (C) are supported"));
    }
    let (source_in, source_out) = (read_timecode(tokens[4], fps)?, read_timecode(tokens[5], fps)?);
    let (record_in, record_out) = (read_timecode(tokens[6], fps)?, read_timecode(tokens[7], fps)?);
    if source_out <= source_in || record_out <= record_in || source_out - source_in != record_out - record_in {
        return Err(unsupported("source and record durations must match and be positive"));
    }
    Ok(Cut { lane, reel: tokens[1].into(), source_in, source_out, record_in, record_out, name: None, media_url: None })
}

fn finish_cut(cut: Cut, edits: &mut Vec<Cut>) -> Result<()> {
    if cut.media_url.as_deref().is_none_or(str::is_empty) {
        return Err(unsupported("each event requires a `* FROM FILE:` media URL; reel-only EDLs require relinking"));
    }
    if let Some(prior) = edits.first() {
        if prior.lane.independent() != cut.lane.independent() || (!prior.lane.independent() && prior.lane != cut.lane) {
            return Err(unsupported("mixed A and AA or independent A1/A2 events have ambiguous channel semantics"));
        }
    }
    if let Some(prior) = edits.iter().rev().find(|prior| prior.lane == cut.lane) {
        if cut.record_in < prior.record_out {
            return Err(unsupported("EDL record timecodes overlap or are out of order within one audio lane"));
        }
    } else if !cut.lane.independent() && cut.record_in != 0 {
        return Err(unsupported("EDL must start at record time 00:00:00:00 (no implicit leading trim)"));
    }
    edits.push(cut);
    Ok(())
}

fn time(value: u64, fps: u32) -> Value {
    json!({"OTIO_SCHEMA": "RationalTime.1", "value": value as f64, "rate": f64::from(fps)})
}

fn range(start: u64, duration: u64, fps: u32) -> Value {
    json!({"OTIO_SCHEMA": "TimeRange.1", "start_time": time(start, fps), "duration": time(duration, fps)})
}

/// Parse strict frame-based mono, linked-stereo or two independent mono CMX3600 lanes, then use the OTIO importer
/// to authorize paths, bound/fully decode media, and commit the resulting edits atomically.
/// Unsupported EDL constructs fail before any session change.
pub fn import_audio(engine: &mut Engine, document: &Path, bytes: &[u8], fps: u32) -> Result<crate::otio::Import> {
    if bytes.len() > MAX_EDL_BYTES {
        return Err(unsupported("EDL exceeds the 2 MiB input limit"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| unsupported("EDL is not valid UTF-8"))?;
    let mut cuts = Vec::<Cut>::new();
    let mut pending: Option<Cut> = None;
    let mut saw_rate = false;
    let mut title = "Imported EDL";
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw).trim();
        if line.len() > MAX_LINE_BYTES {
            return Err(unsupported("EDL line exceeds the media-URL limit"));
        }
        if line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("TITLE:") {
            if pending.is_some() || !cuts.is_empty() {
                return Err(unsupported("TITLE must precede all events"));
            }
            title = ascii_remark(value.trim(), "EDL title", 72)?;
        } else if let Some(value) = line.strip_prefix("FCM:") {
            if saw_rate || pending.is_some() || !cuts.is_empty() || value.trim() != "NON-DROP FRAME" {
                return Err(unsupported("only one header `FCM: NON-DROP FRAME` is supported"));
            }
            saw_rate = true;
        } else if let Some(value) = line.strip_prefix("* FROM FILE:").or_else(|| line.strip_prefix("* SOURCE FILE:")) {
            let Some(ref mut cut) = pending else { return Err(unsupported("FROM FILE comment has no event")) };
            if cut.media_url.is_some() {
                return Err(unsupported("duplicate media URL for event"));
            }
            cut.media_url = Some(ascii_remark(value.trim(), "EDL media URL", 8_192)?.to_owned());
        } else if let Some(value) = line.strip_prefix("* FROM CLIP NAME:") {
            let Some(ref mut cut) = pending else { return Err(unsupported("FROM CLIP NAME has no event")) };
            if cut.name.is_some() {
                return Err(unsupported("duplicate clip name for event"));
            }
            cut.name = Some(ascii_remark(value.trim(), "clip name", 80)?.to_owned());
        } else if line.starts_with('*') {
            return Err(unsupported("unrecognized event remarks may change the edit and are not imported"));
        } else {
            if !saw_rate {
                return Err(unsupported("EDL requires `FCM: NON-DROP FRAME` before events"));
            }
            if let Some(previous) = pending.take() {
                finish_cut(previous, &mut cuts)?;
            }
            if cuts.len() >= MAX_EDITS {
                return Err(unsupported("more than 999 edits"));
            }
            pending = Some(parse_cut(line, cuts.len(), fps)?);
        }
    }
    if let Some(last) = pending {
        finish_cut(last, &mut cuts)?;
    }
    if !saw_rate || cuts.is_empty() {
        return Err(unsupported("EDL contains no valid audio events"));
    }

    let first_lane = cuts.first().map_or(AudioLane::Mono, |cut| cut.lane);
    let expected_channels = first_lane.channels();
    let independent = first_lane.independent();
    let mut first_children = Vec::with_capacity(cuts.len().saturating_mul(2));
    let mut second_children = Vec::new();
    let mut first_cursor = 0_u64;
    let mut second_cursor = 0_u64;
    for cut in cuts {
        let (children, cursor) =
            if cut.lane == AudioLane::IndependentA2 { (&mut second_children, &mut second_cursor) } else { (&mut first_children, &mut first_cursor) };
        if cut.record_in > *cursor {
            children.push(json!({"OTIO_SCHEMA": "Gap.1", "source_range": range(0, cut.record_in - *cursor, fps)}));
        }
        children.push(json!({
            "OTIO_SCHEMA": "Clip.1", "name": cut.name.unwrap_or(cut.reel),
            "source_range": range(cut.source_in, cut.source_out - cut.source_in, fps),
            "media_reference": {"OTIO_SCHEMA": "ExternalReference.1", "target_url": cut.media_url},
        }));
        *cursor = cut.record_out;
    }
    let mut tracks = Vec::with_capacity(2);
    if !first_children.is_empty() {
        tracks.push(json!({"OTIO_SCHEMA": "Track.1", "name": if independent { "A1" } else { title }, "kind": "Audio", "children": first_children}));
    }
    if !second_children.is_empty() {
        tracks.push(json!({"OTIO_SCHEMA": "Track.1", "name": "A2", "kind": "Audio", "children": second_children}));
    }
    let timeline = json!({
        "OTIO_SCHEMA": "Timeline.1", "name": title,
        "tracks": {"OTIO_SCHEMA": "Stack.1", "children": tracks}
    });
    let json = serde_json::to_vec(&timeline).map_err(|err| EngineError::Io(format!("EDL staging failed: {err}")))?;
    crate::otio::import_audio_with_channels(engine, document, &json, Some(expected_channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileFormat};
    use soundcraft_model::{AutoParam, AutomationLane, ChannelFormat, Clip, Insert, Mixer, SendSlot, Source, SourceAudio, TrackKind};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (Session, PathBuf, TrackId) {
        fixture_channels(1)
    }

    fn fixture_channels(channels: usize) -> (Session, PathBuf, TrackId) {
        let folder = std::env::temp_dir().join(format!("soundcraft-cmx3600-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&folder).unwrap();
        let media = folder.join("voice #1.wav");
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: if channels == 2 { vec![vec![0.22; 96_000], vec![-0.47; 96_000]] } else { vec![vec![0.125; 96_000]] },
        };
        std::fs::write(&media, soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap()).unwrap();
        let mut session = Session { name: "SoundCraft test".into(), folder: Some(folder), ..Session::default() };
        let sid = SourceId(session.alloc());
        session.pool.insert(sid, Arc::new(SourceAudio::new(audio)));
        session.sources.push(Source {
            id: sid,
            name: "Voice".into(),
            path: media.to_str().unwrap().into(),
            channels: channels as u16,
            frames: 96_000,
            sample_rate: 48_000,
            format: FileFormat::Wav,
            time_reference: 0,
            unsaved: false,
        });
        let tid = session.add_track(TrackKind::Audio, ChannelFormat::for_channels(channels), Some("VO"));
        let first = session.new_clip_id();
        session.track_mut(tid).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(first, "Voice 1", sid, 2_000, 0, 4_000));
        let second = session.new_clip_id();
        session.track_mut(tid).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(second, "Voice 2", sid, 8_000, 6_000, 2_000));
        (session, media, tid)
    }

    fn fixture_two_lanes() -> (Session, PathBuf, PathBuf, [TrackId; 2]) {
        let (mut session, first_media, a1) = fixture();
        let second_media = first_media.parent().unwrap().join("independent #2.wav");
        let second_audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![-0.42; 96_000]] };
        std::fs::write(&second_media, soundcraft_audio_io::encode(&second_audio, &EncodeOptions::default()).unwrap()).unwrap();
        let second_source = SourceId(session.alloc());
        session.pool.insert(second_source, Arc::new(SourceAudio::new(second_audio)));
        session.sources.push(Source {
            id: second_source,
            name: "Independent".into(),
            path: second_media.to_str().unwrap().into(),
            channels: 1,
            frames: 96_000,
            sample_rate: 48_000,
            format: FileFormat::Wav,
            time_reference: 0,
            unsaved: false,
        });
        let a2 = session.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Other"));
        let first = session.new_clip_id();
        session.track_mut(a2).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(first, "Other 1", second_source, 6_000, 0, 2_000));
        let second = session.new_clip_id();
        session.track_mut(a2).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(second, "Other 2", second_source, 10_000, 4_000, 4_000));
        (session, first_media, second_media, [a1, a2])
    }

    fn clean_two_lane_fixture(first_media: &Path, second_media: &Path, document: Option<&Path>) {
        if let Some(document) = document {
            std::fs::remove_file(document).unwrap();
        }
        std::fs::remove_file(first_media).unwrap();
        std::fs::remove_file(second_media).unwrap();
    }

    #[test]
    fn exports_frame_exact_mono_cuts_with_reel_comments_and_record_gap() {
        let (session, media, track) = fixture();
        let exported = export_audio(&session, track, 24).unwrap();
        assert_eq!(exported.summary, ExportSummary { edits: 2, unique_sources: 1 });
        let output = String::from_utf8(exported.bytes).unwrap();
        assert!(output.starts_with("TITLE: SoundCraft test\nFCM: NON-DROP FRAME\n"));
        assert!(output.contains("001  SC000001 A     C        00:00:00:01 00:00:00:03 00:00:00:00 00:00:00:02"));
        assert!(output.contains("002  SC000001 A     C        00:00:00:04 00:00:00:05 00:00:00:03 00:00:00:04"));
        assert!(output.contains("* FROM CLIP NAME: Voice 1"));
        assert!(output.contains("voice%20%231.wav"));
        let media_url = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        let portable = output.replace(&media_url, "file:///tmp/golden-source.wav");
        assert_eq!(portable, include_str!("../tests/fixtures/cmx3600_audio.edl"));
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn export_import_roundtrip_keeps_source_frames_gap_and_is_one_undo_step() {
        let (session, media, track) = fixture();
        let document = media.with_extension("edl");
        let output = export_audio(&session, track, 24).unwrap();
        std::fs::write(&document, output.bytes).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let imported = engine.execute("file.import_edl", &json!({"path": document.to_str().unwrap(), "fps":24})).unwrap();
        assert_eq!(imported["audio_tracks"], 1);
        assert_eq!(imported["audio_clips"], 2);
        assert_eq!(imported["media_sources"], 1);
        let track = engine.session().tracks.iter().find(|t| t.name == "SoundCraft test").unwrap();
        assert_eq!((track.clips()[0].start, track.clips()[0].length), (0, 4_000));
        assert_eq!((track.clips()[1].start, track.clips()[1].length), (6_000, 2_000));
        assert!(matches!(&track.clips()[0].content, ClipContent::Audio { offset: 2_000, .. }));
        assert!(matches!(&track.clips()[1].content, ClipContent::Audio { offset: 8_000, .. }));
        assert!(engine.can_undo());
        engine.undo();
        assert_eq!(*engine.session(), before);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn stereo_aa_roundtrip_keeps_separate_left_and_right_without_gain_or_channel_duplication() {
        let (session, media, track) = fixture_channels(2);
        let original = soundcraft_mix::render_clips(&session, track, soundcraft_time::Range::new(0, 8_000));
        assert_eq!(original.len(), 2);
        assert!((original[0][0] - 0.22).abs() < 0.00001);
        assert!((original[1][0] + 0.47).abs() < 0.00001);
        assert_eq!(original[0][5_000], 0.0, "one-frame editorial gap must be silent");
        assert_eq!(original[1][5_000], 0.0);

        let exported = export_audio(&session, track, 24).unwrap();
        let text = String::from_utf8(exported.bytes.clone()).unwrap();
        assert!(text.contains("001  SC000001 AA    C"));
        assert!(text.contains("002  SC000001 AA    C"));
        let url = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        assert_eq!(text.replace(&url, "file:///tmp/golden-stereo.wav"), include_str!("../tests/fixtures/cmx3600_stereo.edl"));

        let document = media.with_extension("edl");
        std::fs::write(&document, exported.bytes).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let result = engine.execute("file.import_edl", &json!({"path": document.to_str().unwrap(), "fps":24})).unwrap();
        assert_eq!(result["audio_tracks"], 1, "one AA event list is one linked stereo track, never two doubled tracks");
        assert_eq!(result["audio_clips"], 2);
        assert_eq!(result["media_sources"], 1);
        let imported_track = engine.session().tracks.iter().find(|t| t.name == "SoundCraft test").unwrap();
        assert_eq!(imported_track.format, ChannelFormat::Stereo);
        assert_eq!(imported_track.clips()[0].gain_db, 0.0);
        assert_eq!(imported_track.clips()[1].gain_db, 0.0);
        assert_eq!(imported_track.clips()[1].start, 6_000);
        let imported = soundcraft_mix::render_clips(engine.session(), imported_track.id, soundcraft_time::Range::new(0, 8_000));
        assert_eq!(imported.len(), original.len());
        for (channel, (was, now)) in original.iter().zip(imported.iter()).enumerate() {
            assert_eq!(was.len(), now.len());
            for (sample, (&a, &b)) in was.iter().zip(now.iter()).enumerate() {
                assert!((a - b).abs() < 0.00001, "channel {channel} sample {sample}: {a} changed to {b}");
            }
        }
        assert!(engine.can_undo());
        engine.undo();
        assert_eq!(*engine.session(), before);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn independent_a1_a2_cuts_keep_two_mono_sources_gaps_overlaps_and_one_undo_step() {
        let (session, first_media, second_media, lanes) = fixture_two_lanes();
        let range = soundcraft_time::Range::new(0, 8_000);
        let originals = lanes.map(|id| soundcraft_mix::render_clips(&session, id, range));
        assert_eq!(originals[0].len(), 1);
        assert_eq!(originals[1].len(), 1);
        assert!((originals[0][0][0] - 0.125).abs() < 0.00001);
        assert!((originals[1][0][0] + 0.42).abs() < 0.00001);
        assert_eq!(originals[0][0][5_000], 0.0);
        assert_eq!(originals[1][0][3_000], 0.0);

        let exported = export_audio_tracks(&session, &lanes, 24).unwrap();
        assert_eq!(exported.summary, ExportSummary { edits: 4, unique_sources: 2 });
        let text = String::from_utf8(exported.bytes.clone()).unwrap();
        assert!(text.contains("001  SC000001 A1    C"));
        assert!(text.contains("002  SC000002 A2    C"));
        assert!(text.contains("003  SC000002 A2    C"));
        assert!(text.contains("004  SC000001 A1    C"));
        let first_url = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        let second_url = crate::otio::media_url(&session, &session.sources[1]).unwrap();
        assert_eq!(
            text.replace(&first_url, "file:///tmp/golden-a1.wav").replace(&second_url, "file:///tmp/golden-a2.wav"),
            include_str!("../tests/fixtures/cmx3600_two_lanes.edl")
        );

        let document = first_media.with_extension("edl");
        std::fs::write(&document, exported.bytes).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let result = engine.execute("file.import_edl", &json!({"path": document.to_str().unwrap(), "fps": 24})).unwrap();
        assert_eq!(result["audio_tracks"], 2);
        assert_eq!(result["audio_clips"], 4);
        assert_eq!(result["media_sources"], 2);
        for (index, name) in ["A1", "A2"].iter().enumerate() {
            let restored = engine.session().tracks.iter().find(|t| &t.name == name).unwrap();
            assert_eq!(restored.format, ChannelFormat::Mono);
            assert_eq!(restored.clips().len(), 2);
            assert_eq!(restored.clips()[0].gain_db, 0.0);
            let now = soundcraft_mix::render_clips(engine.session(), restored.id, range);
            assert_eq!(now.len(), 1);
            for (sample, (&original, &imported)) in originals[index][0].iter().zip(&now[0]).enumerate() {
                assert!((original - imported).abs() < 0.00001, "lane {name} sample {sample}: {original} changed to {imported}");
            }
        }
        assert!(engine.can_undo());
        engine.undo();
        assert_eq!(*engine.session(), before);

        let explicit = first_media.with_file_name("explicit.edl");
        let command = Engine::new(session.clone())
            .execute("file.export_edl", &json!({"path": explicit.to_str().unwrap(), "tracks": [lanes[0].0, lanes[1].0], "fps": 24}))
            .unwrap();
        assert_eq!(command["edits"], 4);
        assert_eq!(command["tracks"], json!([lanes[0], lanes[1]]));
        assert_eq!(std::fs::read(&explicit).unwrap(), text.as_bytes());
        std::fs::remove_file(explicit).unwrap();

        let selected = first_media.with_file_name("selected.edl");
        let mut selection_session = session;
        selection_session.edit.selected_tracks = lanes.to_vec();
        let selected_result =
            Engine::new(selection_session).execute("file.export_edl", &json!({"path": selected.to_str().unwrap(), "fps": 24})).unwrap();
        assert_eq!(selected_result["tracks"], json!([lanes[0], lanes[1]]));
        assert_eq!(std::fs::read(&selected).unwrap(), text.as_bytes());
        std::fs::remove_file(selected).unwrap();
        clean_two_lane_fixture(&first_media, &second_media, Some(&document));
    }

    #[test]
    fn independent_lane_import_accepts_initial_lane_gap_and_official_adapter_fixture() {
        let (session, first_media, second_media, _) = fixture_two_lanes();
        let a1 = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        let a2 = crate::otio::media_url(&session, &session.sources[1]).unwrap();
        let fixture = include_str!("../tests/fixtures/cmx3600_two_lanes.edl")
            .replace("file:///tmp/golden-a1.wav", &a1)
            .replace("file:///tmp/golden-a2.wav", &a2);
        // A2 begins one frame late; A1 still starts at zero. The independent-lane gap is explicit.
        let shifted = fixture
            .replace("00:00:00:03 00:00:00:04 00:00:00:00 00:00:00:01", "00:00:00:03 00:00:00:04 00:00:00:01 00:00:00:02")
            .replace("00:00:00:05 00:00:00:07 00:00:00:02 00:00:00:04", "00:00:00:05 00:00:00:07 00:00:00:03 00:00:00:05");
        let document = first_media.with_extension("edl");
        let mut engine = Engine::default();
        let imported = import_audio(&mut engine, &document, shifted.as_bytes(), 24).unwrap();
        assert_eq!((imported.tracks, imported.clips, imported.sources), (2, 4, 2));
        let lane_a2 = engine.session().tracks.iter().find(|t| t.name == "A2").unwrap();
        assert_eq!(lane_a2.clips()[0].start, 2_000);
        assert_eq!(lane_a2.clips()[1].start, 6_000);
        clean_two_lane_fixture(&first_media, &second_media, None);
    }

    #[test]
    fn independent_edl_rejects_mixed_lanes_overlap_wrong_channel_media_and_event_overflow_atomically() {
        let (session, first_media, second_media, lanes) = fixture_two_lanes();
        let document = first_media.with_extension("edl");
        let good = String::from_utf8(export_audio_tracks(&session, &lanes, 24).unwrap().bytes).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let mut overflow = good.clone();
        for number in 5..=1000 {
            let begin = number as u64 - 1;
            writeln!(overflow, "{number:03}  SC000001 A1    C        00:00:00:00 00:00:00:01 {} {}", timecode(begin, 24), timecode(begin + 1, 24))
                .unwrap();
            writeln!(overflow, "* FROM FILE: file:///tmp/not-read-until-validation.wav\n").unwrap();
        }
        let err = import_audio(&mut engine, &document, overflow.as_bytes(), 24).unwrap_err();
        assert!(format!("{err}").contains("more than 999"), "unexpected event-cap error: {err}");
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        let broken = [
            good.replace("003  SC000002 A2    C", "003  SC000002 AA    C"),
            good.replace("004  SC000001 A1    C", "004  SC000001 A     C"),
            good.replace(
                "003  SC000002 A2    C        00:00:00:05 00:00:00:07 00:00:00:02 00:00:00:04",
                "003  SC000002 A2    C        00:00:00:05 00:00:00:07 00:00:00:00 00:00:00:02",
            ),
            good.replace("003  SC000002 A2    C", "003  SC000002 A3    C"),
            good.replace("001  SC000001 A1    C", "001  SC000001 A1    D"),
        ];
        for invalid in broken {
            let err = import_audio(&mut engine, &document, invalid.as_bytes(), 24).unwrap_err();
            assert!(matches!(err, EngineError::Unsupported(_)), "unexpected error: {err}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
        }
        // Each A1/A2 lane accepts only mono source files, not one channel of linked stereo.
        let (stereo_session, stereo_media, stereo_track) = fixture_channels(2);
        let stereo_url = crate::otio::media_url(&stereo_session, &stereo_session.sources[0]).unwrap();
        let mono_url = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        let second_url = crate::otio::media_url(&session, &session.sources[1]).unwrap();
        let invalid = good.replace(&mono_url, &stereo_url);
        let err = import_audio(&mut engine, &document, invalid.as_bytes(), 24).unwrap_err();
        assert!(format!("{err}").contains("requires exactly"), "expected strict mono validation: {err}");
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        let missing = good.replace(&second_url, "file:///tmp/soundcraft-cmx3600-nonexistent-media.wav");
        assert!(import_audio(&mut engine, &document, missing.as_bytes(), 24).is_err());
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        assert!(export_audio_tracks(&stereo_session, &[stereo_track, stereo_track], 24).is_err());
        assert!(export_audio_tracks(&session, &[lanes[0], stereo_track], 24).is_err());
        std::fs::remove_file(stereo_media).unwrap();
        clean_two_lane_fixture(&first_media, &second_media, None);
    }

    #[test]
    fn stereo_and_mono_channel_flags_require_matching_media_before_mutation() {
        for channels in [1, 2] {
            let (session, media, track) = fixture_channels(channels);
            let document = media.with_extension("edl");
            let good = String::from_utf8(export_audio(&session, track, 24).unwrap().bytes).unwrap();
            let mismatch = if channels == 1 { good.replace(" A     C ", " AA    C ") } else { good.replace(" AA    C ", " A     C ") };
            let mut engine = Engine::default();
            let before = engine.session().clone();
            let err = import_audio(&mut engine, &document, mismatch.as_bytes(), 24).unwrap_err();
            assert!(format!("{err}").contains("requires exactly"), "channel mismatch error: {err}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());

            let mixed = if channels == 1 {
                good.replace("002  SC000001 A     C", "002  SC000001 AA    C")
            } else {
                good.replace("002  SC000001 AA    C", "002  SC000001 A     C")
            };
            let err = import_audio(&mut engine, &document, mixed.as_bytes(), 24).unwrap_err();
            assert!(format!("{err}").contains("mixed A and AA"), "mixed lanes error: {err}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
            std::fs::remove_file(media).unwrap();
        }
    }

    #[test]
    fn accepts_independently_parsable_cmx3600_fixture() {
        let (session, media, _) = fixture();
        let golden = include_str!("../tests/fixtures/cmx3600_audio.edl");
        let document = media.with_extension("edl");
        let absolute = crate::io::checked_media_path(&media).unwrap();
        let url = crate::otio::media_url(&session, &session.sources[0]).unwrap();
        let bytes = golden.replace("file:///tmp/golden-source.wav", &url);
        let mut engine = Engine::default();
        let imported = import_audio(&mut engine, &document, bytes.as_bytes(), 24).unwrap();
        assert_eq!((imported.tracks, imported.clips, imported.sources), (1, 2, 1));
        assert_eq!(engine.session().sources[0].path, absolute.to_str().unwrap());
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn rejects_edits_not_exactly_on_the_frame_grid_or_with_unrendered_processing() {
        let (mut session, media, track) = fixture();
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].length += 1;
        assert!(matches!(export_audio(&session, track, 24), Err(EngineError::Unsupported(message)) if message.contains("whole EDL frame")));
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].length -= 1;
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].fade_in.len = 100;
        assert!(matches!(export_audio(&session, track, 24), Err(EngineError::Unsupported(message)) if message.contains("render")));
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].fade_in.len = 0;
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].start = 2_000;
        assert!(matches!(export_audio(&session, track, 24), Err(EngineError::Unsupported(message)) if message.contains("leading silence")));
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn refuses_unrendered_track_mixer_processing_but_allows_recording_setup() {
        let (mut session, media, track) = fixture();
        let mixer = &mut session.track_mut(track).unwrap().mixer;
        mixer.record_arm = true;
        mixer.input = Route::Hardware("In 2".into());
        mixer.input_monitor = true;
        assert!(export_audio(&session, track, 24).is_ok(), "capture settings must not block editorial interchange");

        for (case, expected) in [
            (0, "mute/solo"),
            (1, "mute/solo"),
            (2, "fader/trim"),
            (3, "fader/trim"),
            (4, "phase/pan"),
            (5, "phase/pan"),
            (6, "inserts"),
            (7, "sends"),
            (8, "VCA"),
            (9, "VCA"),
            (10, "output routing"),
            (11, "frozen/Elastic"),
            (12, "frozen/Elastic"),
            (13, "inserts"),
        ] {
            let t = session.track_mut(track).unwrap();
            t.mixer = Mixer::new(ChannelFormat::Mono);
            t.automation.clear();
            t.frozen = false;
            t.elastic = None;
            t.instrument = None;
            match case {
                0 => t.mixer.mute = true,
                1 => t.mixer.solo = true,
                2 => t.mixer.volume_db = -3.0,
                3 => t.mixer.trim_db = 2.0,
                4 => t.mixer.phase_invert = true,
                5 => t.mixer.pan[0] = 0.25,
                6 => t.mixer.inserts[0] = Some(Insert::new("eq7")),
                7 => t.mixer.sends[0] = Some(SendSlot::new(Route::Main)),
                8 => t.mixer.vca = Some(123),
                9 => t.automation.push(AutomationLane::new(AutoParam::Volume)),
                10 => t.mixer.output = Route::Hardware("Out 2".into()),
                11 => t.frozen = true,
                12 => t.elastic = Some("polyphonic".into()),
                13 => t.instrument = Some(Insert::new("sampler")),
                _ => {}
            }
            let err = export_audio(&session, track, 24).err().unwrap();
            assert!(format!("{err}").contains(expected), "case {case} returned wrong error: {err}");
        }
        // The same validation runs before a command writes any output.
        session.track_mut(track).unwrap().mixer.volume_db = -6.0;
        let document = media.with_extension("edl");
        let mut engine = Engine::new(session);
        assert!(engine.execute("file.export_edl", &json!({"path": document.to_str().unwrap(), "track": track.0, "fps": 24})).is_err());
        assert!(!document.exists());
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn rejects_video_transitions_dropframe_missing_refs_and_overlap_atomically() {
        let (session, media, track) = fixture();
        let document = media.with_extension("edl");
        let correct = String::from_utf8(export_audio(&session, track, 24).unwrap().bytes).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        for broken in [
            correct.replace(" A     C ", " V     C "),
            correct.replace(" A     C ", " A     D "),
            correct.replace("FCM: NON-DROP FRAME", "FCM: DROP FRAME"),
            correct.replace("* FROM FILE:", "* UNKNOWN FILE:"),
            correct.replace("00:00:00:03 00:00:00:04", "00:00:00:01 00:00:00:02"),
            correct.replace("00:00:00:01 00:00:00:03", "00:00:00:01 00:00:00:05"),
        ] {
            assert!(import_audio(&mut engine, &document, broken.as_bytes(), 24).is_err(), "accepted malformed cut: {broken}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
        }
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn checked_media_root_and_bounded_input_are_enforced_before_session_edit() {
        let (session, media, track) = fixture();
        let document = media.with_extension("edl");
        let correct = export_audio(&session, track, 24).unwrap().bytes;
        let mut engine = Engine::default();
        assert!(import_audio(&mut engine, &document, &vec![b' '; MAX_EDL_BYTES + 1], 24).is_err());
        let allowed = media.parent().unwrap().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let chosen = allowed.join("chosen.edl");
        std::fs::write(&chosen, &correct).unwrap();
        let roots = crate::file_access::AutomationRoots::new(&[allowed.as_path()], &[]).unwrap().unwrap();
        let err =
            crate::file_access::confine(Some(&roots), || engine.execute("file.import_edl", &json!({"path": chosen.to_str().unwrap(), "fps":24})));
        assert!(matches!(err, Err(EngineError::Io(message)) if message.contains("outside the read root")));
        assert!(engine.session().sources.is_empty());
        std::fs::remove_file(chosen).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn timecode_and_rate_reject_out_of_range_or_fractional_values() {
        let (session, media, _) = fixture();
        assert_eq!(read_timecode("00:00:59:23", 24).unwrap(), 59 * 24 + 23);
        for code in ["00:00:59;23", "00:00:59:24", "24:00:00:00", "00:60:00:00", "01:00:0A:00"] {
            assert!(read_timecode(code, 24).is_err());
        }
        assert_eq!(frame_rate(&session, Some(25)).unwrap(), 25);
        assert!(frame_rate(&session, Some(2997)).is_err());
        assert!(sample_frame(1, 48_000, 24).is_err());
        assert_eq!(sample_frame(2_000, 48_000, 24).unwrap(), 1);
        std::fs::remove_file(media).unwrap();
    }
}
