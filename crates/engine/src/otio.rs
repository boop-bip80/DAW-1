//! Import and export the audio edit of a session as OpenTimelineIO (`.otio`) JSON.
//!
//! This is a deliberately bounded interchange format, not an AAF/OMF or Pro Tools session
//! reader. OTIO carries the edit decisions (tracks, source trims, gaps, muted clips), while a
//! consumer must have access to the referenced audio files. MIDI, buses, automation and plug-in
//! processing do not have an equivalent representation in this audio-only export.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_model::{ChannelFormat, Clip, ClipContent, Session, Source, TrackKind};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

// OTIO RationalTime uses floating-point values. Integers through 2^53 are exact; larger sample
// positions must be rejected instead of silently shifting edits on import into another tool.
const MAX_EXACT_SAMPLE: i64 = (1_i64 << 53) - 1;
const MAX_TRACKS: usize = 2_048;
const MAX_CLIPS: usize = 100_000;
const MAX_PATH_BYTES: usize = 8_192;
const MAX_OTIO_BYTES: usize = 4 * 1024 * 1024;
const MAX_MEDIA_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_MEDIA_BYTES: usize = 512 * 1024 * 1024;
const MAX_TOTAL_SAMPLES: usize = 64 * 1024 * 1024;
const MAX_SOURCES: usize = 1_024;

/// Completed JSON and a truthful summary of what it contains.
pub struct Export {
    pub json: Vec<u8>,
    pub tracks: usize,
    pub clips: usize,
    /// Source clip processing is retained as SoundCraft-specific metadata, but other tools may
    /// ignore it. The value counts such clips, so the caller can give a useful warning.
    pub processing_metadata_clips: usize,
}

fn unsupported(message: impl Into<String>) -> EngineError {
    EngineError::Unsupported(format!("OpenTimelineIO audio export: {}", message.into()))
}

fn rational_time(samples: i64, rate: u32) -> Result<Value> {
    if !(0..=MAX_EXACT_SAMPLE).contains(&samples) || rate == 0 {
        return Err(unsupported("sample position is outside OTIO's lossless range"));
    }
    Ok(json!({"OTIO_SCHEMA": "RationalTime.1", "value": samples as f64, "rate": f64::from(rate)}))
}

fn range(start: i64, length: i64, rate: u32) -> Result<Value> {
    Ok(json!({
        "OTIO_SCHEMA": "TimeRange.1",
        "start_time": rational_time(start, rate)?,
        "duration": rational_time(length, rate)?,
    }))
}

fn percent_encode_path(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 15)]));
        }
    }
    out
}

fn file_url(path: &Path) -> Result<String> {
    let raw = path.to_str().ok_or_else(|| unsupported("a media path is not UTF-8"))?;
    if raw.len() > MAX_PATH_BYTES || raw.contains('\0') {
        return Err(unsupported("a media path is invalid or too long"));
    }
    if cfg!(windows) {
        // canonicalize adds \\?\ on Windows. UNC paths need their server to remain the authority.
        let plain =
            raw.strip_prefix(r"\\?\UNC\").map(|rest| format!(r"\\{rest}")).unwrap_or_else(|| raw.strip_prefix(r"\\?\").unwrap_or(raw).to_string());
        let slash = plain.replace('\\', "/");
        if slash.starts_with("//") {
            return Ok(format!("file:{}", percent_encode_path(&slash)));
        }
        return Ok(format!("file:///{}", percent_encode_path(&slash)));
    }
    Ok(format!("file://{}", percent_encode_path(raw)))
}

pub(crate) fn media_url(session: &Session, source: &Source) -> Result<String> {
    if source.unsaved {
        return Err(unsupported(format!("source `{}` has unsaved audio; save the session first", source.name)));
    }
    if source.path.is_empty() {
        return Err(unsupported(format!("source `{}` has no media path", source.name)));
    }
    let path = Path::new(&source.path);
    let absolute: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let folder = session.folder.as_ref().ok_or_else(|| unsupported("the session has no media folder; save it first"))?;
        folder.join(path)
    };
    // IO helpers enforce read-root confinement before probing or resolving the source.
    let actual = crate::io::checked_media_path(&absolute).map_err(|err| EngineError::Io(format!("source `{}` is offline: {err}", source.name)))?;
    file_url(&actual)
}

/// Produce a schema-compatible OTIO audio timeline; no output file is created on failure.
///
/// OTIO Track items are sequential. A track with overlapping clips (including crossfades) is
/// rejected explicitly: pretending the items were sequential would silently shift the edit.
pub fn export_audio(session: &Session) -> Result<Export> {
    let rate = session.sample_rate.hz();
    if rate == 0 {
        return Err(unsupported("session sample rate is zero"));
    }
    let audio_tracks = session.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count();
    if audio_tracks > MAX_TRACKS {
        return Err(unsupported("more than 2048 audio tracks"));
    }
    if audio_tracks == 0 {
        return Err(unsupported("the session has no audio tracks to export"));
    }

    let mut tracks = Vec::with_capacity(audio_tracks);
    let mut count = 0usize;
    let mut processing_metadata_clips = 0usize;
    for track in session.tracks.iter().filter(|t| t.kind == TrackKind::Audio) {
        let mut clips = track.clips().iter().collect::<Vec<_>>();
        clips.sort_by_key(|c| (c.start, c.id.0));
        count = count.checked_add(clips.len()).ok_or_else(|| unsupported("too many clips"))?;
        if count > MAX_CLIPS {
            return Err(unsupported("more than 100000 audio clips"));
        }
        let mut children = Vec::with_capacity(clips.len().saturating_mul(2));
        let mut cursor = 0_i64;
        for clip in clips {
            let ClipContent::Audio { source, offset } = &clip.content else {
                return Err(unsupported(format!("track `{}` contains a non-audio clip", track.name)));
            };
            let (source, offset) = (*source, *offset);
            if clip.start < cursor || clip.length <= 0 || offset < 0 {
                return Err(unsupported(format!("track `{}` has overlapping clips or invalid edit ranges", track.name)));
            }
            let end = clip.start.checked_add(clip.length).ok_or_else(|| unsupported("clip end overflows"))?;
            let source_end = offset.checked_add(clip.length).ok_or_else(|| unsupported("source trim overflows"))?;
            rational_time(end, rate)?;
            let src = session.source(source).ok_or_else(|| unsupported(format!("clip `{}` references missing audio", clip.name)))?;
            if src.sample_rate != rate || u64::try_from(source_end).map_or(true, |n| n > src.frames) {
                return Err(unsupported(format!("clip `{}` does not fit its source at the session rate", clip.name)));
            }
            if !clip.stretch.is_finite() || clip.stretch != 1.0 {
                return Err(unsupported(format!("clip `{}` is time-stretched; render it before OTIO export", clip.name)));
            }
            if !clip.gain_db.is_finite() || clip.gain_env.iter().any(|(_, gain)| !gain.is_finite()) {
                return Err(unsupported(format!("clip `{}` has invalid gain data", clip.name)));
            }
            if clip.start > cursor {
                children.push(json!({
                    "OTIO_SCHEMA": "Gap.1", "name": "Gap", "enabled": true,
                    "effects": [], "markers": [], "metadata": {},
                    "source_range": range(0, clip.start - cursor, rate)?,
                }));
            }
            let has_processing = clip.gain_db != 0.0 || !clip.gain_env.is_empty() || clip.fade_in.len > 0 || clip.fade_out.len > 0;
            if has_processing {
                processing_metadata_clips += 1;
            }
            let uri = media_url(session, src)?;
            let frames = i64::try_from(src.frames).map_err(|_| unsupported("source duration exceeds OTIO's lossless range"))?;
            children.push(json!({
                "OTIO_SCHEMA": "Clip.1", "name": clip.name, "enabled": !clip.muted,
                "effects": [], "markers": [],
                "source_range": range(offset, clip.length, rate)?,
                "media_reference": {
                    "OTIO_SCHEMA": "ExternalReference.1", "name": src.name,
                    "target_url": uri, "available_range": range(0, frames, rate)?,
                    "metadata": {},
                },
                "metadata": {"soundcraft": {
                    "clip_id": clip.id.0, "source_id": source.0,
                    "gain_db": clip.gain_db, "gain_env": clip.gain_env,
                    "fade_in_samples": clip.fade_in.len, "fade_out_samples": clip.fade_out.len,
                }},
            }));
            cursor = end;
        }
        tracks.push(json!({
            "OTIO_SCHEMA": "Track.1", "name": track.name, "kind": "Audio",
            "children": children, "enabled": !track.inactive,
            "effects": [], "markers": [], "source_range": null,
            "metadata": {"soundcraft": {"track_id": track.id.0}},
        }));
    }
    let timeline = json!({
        "OTIO_SCHEMA": "Timeline.1", "name": session.name,
        "global_start_time": null,
        "metadata": {"soundcraft": {"sample_rate": rate, "audio_only": true}},
        "tracks": {
            "OTIO_SCHEMA": "Stack.1", "name": "tracks", "children": tracks,
            "effects": [], "markers": [], "source_range": null, "metadata": {},
        },
    });
    let json = serde_json::to_vec_pretty(&timeline).map_err(|e| EngineError::Io(format!("OTIO serialization failed: {e}")))?;
    Ok(Export { json, tracks: audio_tracks, clips: count, processing_metadata_clips })
}

/// The audio-only portion of an OTIO timeline that was appended to the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Import {
    pub tracks: usize,
    pub clips: usize,
    pub sources: usize,
}

struct ImportedSource {
    path: PathBuf,
    name: String,
    audio: AudioBuffer,
    format: FileFormat,
}

struct ImportedClip {
    name: String,
    source: usize,
    offset: i64,
    start: i64,
    length: i64,
    muted: bool,
}

struct ImportedTrack {
    name: String,
    inactive: bool,
    clips: Vec<ImportedClip>,
    channels: usize,
}

fn invalid_import(message: impl Into<String>) -> EngineError {
    EngineError::Unsupported(format!("OpenTimelineIO audio import: {}", message.into()))
}

fn expect_schema<'a>(value: &'a Value, schema: &str) -> Result<&'a Value> {
    if value.get("OTIO_SCHEMA").and_then(Value::as_str) != Some(schema) {
        return Err(invalid_import(format!("expected {schema}")));
    }
    Ok(value)
}

fn children(value: &Value) -> Result<&Vec<Value>> {
    value.get("children").and_then(Value::as_array).ok_or_else(|| invalid_import("missing composition children"))
}

fn plain_composable(value: &Value) -> Result<()> {
    for field in ["effects", "markers"] {
        if value.get(field).is_some_and(|v| v.as_array().is_none_or(|items| !items.is_empty())) {
            return Err(invalid_import(format!("{field} are not supported")));
        }
    }
    Ok(())
}

/// OTIO's time bases can differ from the current session's. Convert every boundary to the
/// nearest sample (ties away from zero), bounding the result to losslessly representable values.
/// Only Clip.2's media timecode origins may be negative; durations and native timeline positions
/// remain nonnegative, and the final decoded-file offset must be checked separately.
fn time_samples(value: &Value, rate: u32, allow_negative_start: bool) -> Result<i64> {
    expect_schema(value, "RationalTime.1")?;
    let t = value.get("value").and_then(Value::as_f64).ok_or_else(|| invalid_import("RationalTime has no numeric value"))?;
    let base = value.get("rate").and_then(Value::as_f64).ok_or_else(|| invalid_import("RationalTime has no numeric rate"))?;
    if !t.is_finite() || !base.is_finite() || (!allow_negative_start && t < 0.0) || base <= 0.0 || base > 384_000.0 {
        return Err(invalid_import("invalid RationalTime value or rate"));
    }
    let samples = (t * f64::from(rate) / base).round();
    let minimum = if allow_negative_start { -(MAX_EXACT_SAMPLE as f64) } else { 0.0 };
    if !samples.is_finite() || !(minimum..=(MAX_EXACT_SAMPLE as f64)).contains(&samples) {
        return Err(invalid_import("time is outside the supported sample range"));
    }
    Ok(samples as i64)
}

fn sample_range(value: &Value, rate: u32, allow_negative_start: bool) -> Result<(i64, i64)> {
    expect_schema(value, "TimeRange.1")?;
    let start = time_samples(value.get("start_time").ok_or_else(|| invalid_import("missing range start"))?, rate, allow_negative_start)?;
    let length = time_samples(value.get("duration").ok_or_else(|| invalid_import("missing range duration"))?, rate, false)?;
    start.checked_add(length).ok_or_else(|| invalid_import("time range overflows"))?;
    Ok((start, length))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Decode only local `file:///` URLs and path-relative references. Relative paths must remain
/// below the OTIO document's directory; every resulting path gets the engine's read-root check.
fn reference_path(uri: &str, document: &Path) -> Result<PathBuf> {
    if uri.len() > MAX_PATH_BYTES || uri.is_empty() || uri.chars().any(|c| matches!(c, '?' | '#' | '\0')) {
        return Err(invalid_import("media URL is empty, too long or has a query/fragment"));
    }
    let text = if let Some(path) = uri.strip_prefix("file://") {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(invalid_import("remote media authorities are not supported"));
        }
        path
    } else {
        if uri.contains("://") || uri.starts_with("file:") || uri.contains(':') {
            return Err(invalid_import("only local file URLs are supported"));
        }
        uri
    };
    let mut decoded = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(b) = input.next() {
        if b == b'%' {
            let hi = input.next().and_then(hex_digit).ok_or_else(|| invalid_import("invalid media URL escape"))?;
            let lo = input.next().and_then(hex_digit).ok_or_else(|| invalid_import("invalid media URL escape"))?;
            decoded.push((hi << 4) | lo);
        } else {
            decoded.push(b);
        }
    }
    let path = String::from_utf8(decoded).map_err(|_| invalid_import("media URL is not UTF-8"))?;
    if path.contains('\0') || path.len() > MAX_PATH_BYTES {
        return Err(invalid_import("invalid or too long decoded media path"));
    }
    #[cfg(windows)]
    let path = if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') { &path[1..] } else { path.as_str() };
    let referenced = Path::new(&path);
    if referenced.components().any(|part| part == Component::ParentDir) {
        return Err(invalid_import("media path contains parent directory traversal"));
    }
    if referenced.is_absolute() {
        return Ok(referenced.to_path_buf());
    }
    if referenced.components().any(|part| !matches!(part, Component::Normal(_) | Component::CurDir)) {
        return Err(invalid_import("relative media path escapes the timeline directory"));
    }
    let parent = document.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(referenced))
}

fn clip_media(clip: &Value) -> Result<(&str, Option<&Value>)> {
    let media = match clip.get("OTIO_SCHEMA").and_then(Value::as_str) {
        Some("Clip.1") => clip.get("media_reference"),
        Some("Clip.2") => {
            let key = clip.get("active_media_reference_key").and_then(Value::as_str).unwrap_or("DEFAULT_MEDIA");
            clip.get("media_references").and_then(|refs| refs.get(key))
        }
        _ => return Err(invalid_import("only Clip.1 and Clip.2 are supported")),
    }
    .ok_or_else(|| invalid_import("clip has no active media reference"))?;
    expect_schema(media, "ExternalReference.1")?;
    let url = media.get("target_url").and_then(Value::as_str).ok_or_else(|| invalid_import("clip has no external media URL"))?;
    Ok((url, media.get("available_range")))
}

fn reject_nontrivial_metadata(value: &Value) -> Result<()> {
    let Some(metadata) = value.pointer("/metadata/soundcraft") else { return Ok(()) };
    let own = metadata.as_object().ok_or_else(|| invalid_import("SoundCraft clip metadata must be an object"))?;
    // Other OTIO applications are free to attach their own metadata. Only SoundCraft's known
    // audio-processing fields are interpreted here: a malformed value must never be mistaken for
    // zero processing and silently dropped when a Clip.1/Clip.2 edit is reconstructed.
    let processing = || invalid_import("render SoundCraft clip processing before import; OTIO edits alone do not preserve it");
    if let Some(gain) = own.get("gain_db") {
        let gain = gain.as_f64().ok_or_else(|| invalid_import("SoundCraft clip `gain_db` must be a number"))?;
        if gain != 0.0 {
            return Err(processing());
        }
    }
    for field in ["fade_in_samples", "fade_out_samples"] {
        if let Some(fade) = own.get(field) {
            let fade = fade.as_i64().ok_or_else(|| invalid_import(format!("SoundCraft clip `{field}` must be an integer")))?;
            if fade != 0 {
                return Err(processing());
            }
        }
    }
    if let Some(envelope) = own.get("gain_env") {
        let envelope = envelope.as_array().ok_or_else(|| invalid_import("SoundCraft clip `gain_env` must be an array"))?;
        if !envelope.is_empty() {
            return Err(processing());
        }
    }
    Ok(())
}

/// Append an OTIO audio timeline as new native tracks, one undoable edit.
///
/// The input JSON, every trim/gap, every referenced path and *all decoded audio* are checked
/// before modifying the current document. Unsupported OTIO structures are errors, never silently
/// flattened. Referenced media must be available locally; the `.otio` file contains no audio.
pub fn import_audio(engine: &mut Engine, document: &Path, bytes: &[u8]) -> Result<Import> {
    import_audio_with_channels(engine, document, bytes, None)
}

/// Optional exact source-channel requirement for audio formats such as CMX3600's `A` / `AA`.
/// Enforced after all decoded media is staged, but before any native session mutation.
pub(crate) fn import_audio_with_channels(engine: &mut Engine, document: &Path, bytes: &[u8], expected_channels: Option<usize>) -> Result<Import> {
    if bytes.len() > MAX_OTIO_BYTES {
        return Err(invalid_import("timeline JSON exceeds the 4 MiB limit"));
    }
    let timeline: Value = serde_json::from_slice(bytes).map_err(|err| invalid_import(format!("invalid JSON: {err}")))?;
    expect_schema(&timeline, "Timeline.1")?;
    if timeline.get("global_start_time").is_some_and(|v| !v.is_null()) {
        return Err(invalid_import("global timeline offsets are not supported"));
    }
    let stack = expect_schema(timeline.get("tracks").ok_or_else(|| invalid_import("timeline has no tracks"))?, "Stack.1")?;
    plain_composable(stack)?;
    if stack.get("source_range").is_some_and(|v| !v.is_null()) {
        return Err(invalid_import("stack trims are not supported"));
    }
    let audio_tracks = children(stack)?;
    if audio_tracks.is_empty() || audio_tracks.len() > MAX_TRACKS {
        return Err(invalid_import("timeline must contain between 1 and 2048 audio tracks"));
    }
    let rate = engine.session().sample_rate.hz();
    if rate == 0 {
        return Err(invalid_import("session sample rate is zero"));
    }

    let mut sources = Vec::<ImportedSource>::new();
    let mut source_index = BTreeMap::<PathBuf, usize>::new();
    let mut tracks = Vec::with_capacity(audio_tracks.len());
    let mut clip_count = 0usize;
    let mut total_bytes = 0usize;
    let mut total_samples = 0usize;
    for (track_number, value) in audio_tracks.iter().enumerate() {
        expect_schema(value, "Track.1")?;
        if value.get("kind").and_then(Value::as_str) != Some("Audio") {
            return Err(invalid_import(format!("track {} is not an audio track", track_number + 1)));
        }
        plain_composable(value)?;
        if value.get("source_range").is_some_and(|v| !v.is_null()) {
            return Err(invalid_import("track trims are not supported"));
        }
        let mut track = ImportedTrack {
            name: value.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("OTIO Audio").to_owned(),
            inactive: value.get("enabled").and_then(Value::as_bool) == Some(false),
            clips: Vec::new(),
            channels: 1,
        };
        let mut cursor = 0i64;
        for child in children(value)? {
            plain_composable(child)?;
            match child.get("OTIO_SCHEMA").and_then(Value::as_str) {
                Some("Gap.1") => {
                    let (_, duration) = sample_range(child.get("source_range").ok_or_else(|| invalid_import("gap has no duration"))?, rate, false)?;
                    if duration <= 0 {
                        return Err(invalid_import("gap must have a positive duration"));
                    }
                    cursor = cursor
                        .checked_add(duration)
                        .filter(|v| *v <= MAX_EXACT_SAMPLE)
                        .ok_or_else(|| invalid_import("timeline exceeds the sample limit"))?;
                }
                Some("Clip.1" | "Clip.2") => {
                    reject_nontrivial_metadata(child)?;
                    clip_count = clip_count.checked_add(1).filter(|n| *n <= MAX_CLIPS).ok_or_else(|| invalid_import("more than 100000 clips"))?;
                    let is_clip_two = child.get("OTIO_SCHEMA").and_then(Value::as_str) == Some("Clip.2");
                    let (url, available_range) = clip_media(child)?;
                    // OTIO uses an ExternalReference's full available_range when a Clip has
                    // no explicit source_range. The official 0.18.1 writer serializes this
                    // common untrimmed case as "source_range": null.
                    let range = child
                        .get("source_range")
                        .filter(|range| !range.is_null())
                        .or_else(|| available_range.filter(|range| !range.is_null()))
                        .ok_or_else(|| invalid_import("clip needs a source trim or media available range"))?;
                    let (offset, duration) = sample_range(range, rate, is_clip_two)?;
                    if duration <= 0 {
                        return Err(invalid_import("clip must have a positive duration"));
                    }
                    let end = cursor
                        .checked_add(duration)
                        .filter(|v| *v <= MAX_EXACT_SAMPLE)
                        .ok_or_else(|| invalid_import("timeline exceeds the sample limit"))?;
                    let source_end = offset.checked_add(duration).ok_or_else(|| invalid_import("source trim overflows"))?;
                    // OTIO's Clip.source_range uses the same media-timecode coordinates as
                    // ExternalReference.available_range. A WAV's first decoded frame corresponds
                    // to the available range's start, not necessarily media-timecode zero.
                    // Without this subtraction a valid 01:00:00-origin edit plays the wrong
                    // samples even though every file and JSON range is otherwise well formed.
                    let media_offset = if let Some(available) = available_range.filter(|range| !range.is_null()) {
                        let (available_start, available_duration) = sample_range(available, rate, is_clip_two)?;
                        if available_duration <= 0 {
                            return Err(invalid_import("media available range must have a positive duration"));
                        }
                        let available_end =
                            available_start.checked_add(available_duration).ok_or_else(|| invalid_import("media available range overflows"))?;
                        if offset < available_start || source_end > available_end {
                            return Err(invalid_import("clip source trim lies outside the media available range"));
                        }
                        offset.checked_sub(available_start).ok_or_else(|| invalid_import("media source offset underflows"))?
                    } else {
                        offset
                    };
                    if media_offset < 0 {
                        return Err(invalid_import("clip media offset cannot be negative"));
                    }
                    let referenced = reference_path(url, document)?;
                    let media = crate::io::checked_media_path(&referenced)?;
                    let source = if let Some(&index) = source_index.get(&media) {
                        index
                    } else {
                        if sources.len() >= MAX_SOURCES {
                            return Err(invalid_import("more than 1024 unique audio sources"));
                        }
                        let bytes = crate::io::read_file_limited(&media, MAX_MEDIA_BYTES)?;
                        total_bytes = total_bytes
                            .checked_add(bytes.len())
                            .filter(|n| *n <= MAX_TOTAL_MEDIA_BYTES)
                            .ok_or_else(|| invalid_import("total media exceeds the 512 MiB limit"))?;
                        let name = media.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("Audio").to_owned();
                        let ext = media.extension().and_then(std::ffi::OsStr::to_str).map(str::to_ascii_lowercase);
                        let probe = soundcraft_audio_io::probe(&bytes, ext.as_deref())
                            .map_err(|err| EngineError::Io(format!("{}: {err}", media.display())))?;
                        if probe.sample_rate == 0 || !(1..=8).contains(&probe.channels) || probe.frames == 0 {
                            return Err(invalid_import("media has an invalid sample rate, channel count or duration"));
                        }
                        // Probe before allocating the decoded buffer, particularly for compressed
                        // formats whose decoded size can dwarf their on-disk size.
                        let estimated = u128::from(probe.frames)
                            .saturating_mul(u128::from(probe.channels))
                            .saturating_mul(u128::from(rate))
                            .checked_div(u128::from(probe.sample_rate))
                            .unwrap_or(u128::MAX);
                        if estimated > (MAX_TOTAL_SAMPLES - total_samples) as u128 {
                            return Err(invalid_import("decoded media exceeds the 64-million-sample limit"));
                        }
                        let (info, decoded) = soundcraft_audio_io::decode(&bytes, ext.as_deref())
                            .map_err(|err| EngineError::Io(format!("{}: {err}", media.display())))?;
                        let audio =
                            crate::io::at_rate(decoded, rate, MAX_TOTAL_SAMPLES).ok_or_else(|| invalid_import("decoded media is too large"))?;
                        let channels = audio.num_channels();
                        if !(1..=8).contains(&channels) || audio.frames() == 0 {
                            return Err(invalid_import("media has no audio or an unsupported channel count"));
                        }
                        let samples = channels.checked_mul(audio.frames()).ok_or_else(|| invalid_import("decoded media size overflows"))?;
                        total_samples = total_samples
                            .checked_add(samples)
                            .filter(|n| *n <= MAX_TOTAL_SAMPLES)
                            .ok_or_else(|| invalid_import("decoded media exceeds the 64-million-sample limit"))?;
                        let index = sources.len();
                        source_index.insert(media.clone(), index);
                        sources.push(ImportedSource { path: media, name, audio, format: info.format });
                        index
                    };
                    let src = &sources[source];
                    let media_end = media_offset.checked_add(duration).ok_or_else(|| invalid_import("decoded media offset overflows"))?;
                    if media_end > i64::try_from(src.audio.frames()).unwrap_or(i64::MAX) {
                        return Err(invalid_import(format!(
                            "clip `{}` exceeds the available media duration",
                            child.get("name").and_then(Value::as_str).unwrap_or("Audio")
                        )));
                    }
                    track.channels = track.channels.max(src.audio.num_channels());
                    track.clips.push(ImportedClip {
                        name: child.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Audio").to_owned(),
                        source,
                        offset: media_offset,
                        start: cursor,
                        length: duration,
                        muted: child.get("enabled").and_then(Value::as_bool) == Some(false),
                    });
                    cursor = end;
                }
                _ => return Err(invalid_import("only sequential audio clips and gaps are supported (no transitions or nested compositions)")),
            }
        }
        tracks.push(track);
    }
    if clip_count == 0 {
        return Err(invalid_import("timeline contains no audio clips"));
    }
    if let Some(expected) = expected_channels
        && (tracks.iter().any(|track| track.channels != expected) || sources.iter().any(|source| source.audio.num_channels() != expected))
    {
        return Err(invalid_import(format!("EDL requires exactly {expected} audio channel(s) in every media source")));
    }

    // Commit only after all parsing, path authorization, decoding and source-bound checks pass.
    // Engine::execute captures the prior session for undo and also rolls back failed commands.
    let source_count = sources.len();
    let track_count = tracks.len();
    let session = engine.session_mut();
    let ids = sources.into_iter().map(|src| crate::io::add_source(session, &src.name, src.audio, src.path.to_str(), src.format)).collect::<Vec<_>>();
    for track in tracks {
        let id = session.add_track(TrackKind::Audio, ChannelFormat::for_channels(track.channels), Some(&track.name));
        session.track_mut(id).ok_or_else(|| invalid_import("new track missing"))?.inactive = track.inactive;
        for clip in track.clips {
            let clip_id = session.new_clip_id();
            let mut native = Clip::audio(clip_id, clip.name, ids[clip.source], clip.offset, clip.start, clip.length);
            native.muted = clip.muted;
            session.track_mut(id).and_then(|t| t.playlist_mut()).ok_or_else(|| invalid_import("new playlist missing"))?.clips.push(native);
        }
    }
    Ok(Import { tracks: track_count, clips: clip_count, sources: source_count })
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::{EncodeOptions, FileFormat};
    use soundcraft_model::{ChannelFormat, Clip, SourceId, TrackKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    // otio_json serialized by the official OpenTimelineIO 0.18.1 Python adapter from two
    // Audio tracks, a Gap and two Clip.2 instances sharing a stereo ExternalReference.
    // Source/available starts are negative media timecode; the native file offsets are
    // positive. This JSON is embedded so CI never depends on Python or network access.
    const OFFICIAL_NEGATIVE_ORIGIN_CLIP_TWO: &str = r#"{"OTIO_SCHEMA":"Timeline.1","metadata":{},"name":"Signed media origins","global_start_time":null,"tracks":{"OTIO_SCHEMA":"Stack.1","metadata":{},"name":"tracks","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Track.1","metadata":{},"name":"Negative lead","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Gap.1","metadata":{},"name":"","source_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":1200.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":0.0}},"effects":[],"markers":[],"enabled":true,"color":null},{"OTIO_SCHEMA":"Clip.2","metadata":{},"name":"Lead","source_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":12000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-24000.0}},"effects":[],"markers":[],"enabled":true,"color":null,"media_references":{"DEFAULT_MEDIA":{"OTIO_SCHEMA":"ExternalReference.1","metadata":{},"name":"","available_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":96000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-48000.0}},"available_image_bounds":null,"target_url":"media/negative.wav"}},"active_media_reference_key":"DEFAULT_MEDIA"}],"kind":"Audio"},{"OTIO_SCHEMA":"Track.1","metadata":{},"name":"Negative back","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Clip.2","metadata":{},"name":"Back","source_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":3000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-46000.0}},"effects":[],"markers":[],"enabled":true,"color":null,"media_references":{"DEFAULT_MEDIA":{"OTIO_SCHEMA":"ExternalReference.1","metadata":{},"name":"","available_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":96000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-48000.0}},"available_image_bounds":null,"target_url":"media/negative.wav"}},"active_media_reference_key":"DEFAULT_MEDIA"}],"kind":"Audio"}]}}"#;

    // Independently generated with OpenTimelineIO 0.18.1's otio_json writer. Full source
    // deliberately omits its trim (source_range: null); OTIO's Clip.trimmed_range() returns
    // ExternalReference.available_range [-48000, +48000] for that clip.
    const OFFICIAL_UNTRIMMED_CLIP_TWO: &str = r#"{"OTIO_SCHEMA":"Timeline.1","metadata":{},"name":"Untrimmed reference","global_start_time":null,"tracks":{"OTIO_SCHEMA":"Stack.1","metadata":{},"name":"tracks","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Track.1","metadata":{},"name":"Untrimmed lead","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Gap.1","metadata":{},"name":"","source_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":1200.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":0.0}},"effects":[],"markers":[],"enabled":true,"color":null},{"OTIO_SCHEMA":"Clip.2","metadata":{},"name":"Full source","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"media_references":{"DEFAULT_MEDIA":{"OTIO_SCHEMA":"ExternalReference.1","metadata":{},"name":"","available_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":96000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-48000.0}},"available_image_bounds":null,"target_url":"media/negative.wav"}},"active_media_reference_key":"DEFAULT_MEDIA"}],"kind":"Audio"},{"OTIO_SCHEMA":"Track.1","metadata":{},"name":"Trimmed back","source_range":null,"effects":[],"markers":[],"enabled":true,"color":null,"children":[{"OTIO_SCHEMA":"Clip.2","metadata":{},"name":"Back","source_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":3000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-46000.0}},"effects":[],"markers":[],"enabled":true,"color":null,"media_references":{"DEFAULT_MEDIA":{"OTIO_SCHEMA":"ExternalReference.1","metadata":{},"name":"","available_range":{"OTIO_SCHEMA":"TimeRange.1","duration":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":96000.0},"start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":48000.0,"value":-48000.0}},"available_image_bounds":null,"target_url":"media/negative.wav"}},"active_media_reference_key":"DEFAULT_MEDIA"}],"kind":"Audio"}]}}"#;

    fn pack_negative_origin_bundle(timeline: &Value, wav: &[u8]) -> Vec<u8> {
        use std::io::Write as _;

        let manifest = serde_json::to_vec(timeline).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in [("version.txt", b"1.0.0".as_slice()), ("content.otio", manifest.as_slice()), ("media/negative.wav", wav)] {
            let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn fixture() -> (Session, PathBuf) {
        let folder = std::env::temp_dir().join(format!("soundcraft-otio-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&folder).unwrap();
        let media = folder.join("drums #1.wav");
        std::fs::write(&media, b"placeholder").unwrap();
        let mut s = Session { folder: Some(folder), ..Session::default() };
        let id = SourceId(s.alloc());
        s.sources.push(Source {
            id,
            name: "Drums".into(),
            path: "drums #1.wav".into(),
            channels: 1,
            frames: 480_000,
            sample_rate: 48_000,
            format: FileFormat::Wav,
            time_reference: 0,
            unsaved: false,
        });
        let first = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Percussion"));
        let second = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Overdub"));
        let cid = s.new_clip_id();
        s.track_mut(first).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "Take A", id, 12_000, 48_000, 96_000));
        let cid = s.new_clip_id();
        s.track_mut(second).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "Take B", id, 96_000, 0, 48_000));
        (s, media)
    }

    /// The exporter only needs a source path; import tests require an actual decodable WAV.
    fn decodable_fixture() -> (Session, PathBuf) {
        let (session, media) = fixture();
        let audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.125; 480_000]] };
        let wav = soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap();
        std::fs::write(&media, wav).unwrap();
        (session, media)
    }

    fn write_interchange(session: &Session, media: &Path) -> PathBuf {
        let document = media.with_extension("otio");
        let output = export_audio(session).unwrap();
        std::fs::write(&document, &output.json).unwrap();
        document
    }

    fn first_clip_as_official_clip_two(timeline: &mut Value) -> &mut Value {
        let clip = &mut timeline["tracks"]["children"][0]["children"][1];
        let reference = clip.as_object_mut().unwrap().remove("media_reference").unwrap();
        clip["OTIO_SCHEMA"] = json!("Clip.2");
        clip["media_references"] = json!({"DEFAULT_MEDIA": reference});
        clip["active_media_reference_key"] = json!("DEFAULT_MEDIA");
        clip
    }

    #[test]
    fn serializes_multi_track_audio_with_precise_gaps_and_source_trims() {
        let (s, path) = fixture();
        let output = export_audio(&s).unwrap();
        assert_eq!((output.tracks, output.clips), (2, 2));
        let timeline: Value = serde_json::from_slice(&output.json).unwrap();
        assert_eq!(timeline["OTIO_SCHEMA"], "Timeline.1");
        let tracks = timeline["tracks"]["children"].as_array().unwrap();
        assert_eq!(tracks[0]["kind"], "Audio");
        assert_eq!(tracks[0]["children"][0]["OTIO_SCHEMA"], "Gap.1");
        assert_eq!(tracks[0]["children"][0]["source_range"]["duration"]["value"], 48_000.0);
        assert_eq!(tracks[0]["children"][1]["source_range"]["start_time"]["value"], 12_000.0);
        assert_eq!(tracks[0]["children"][1]["source_range"]["duration"]["value"], 96_000.0);
        assert!(tracks[0]["children"][1]["media_reference"]["target_url"].as_str().unwrap().ends_with("drums%20%231.wav"));
        assert_eq!(tracks[1]["children"].as_array().unwrap().len(), 1);
        assert_eq!(tracks[1]["children"][0]["source_range"]["start_time"]["value"], 96_000.0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_overlap_without_silent_timeline_shift() {
        let (mut s, path) = fixture();
        let t = s.tracks[0].id;
        let cid = s.new_clip_id();
        let src = s.sources[0].id;
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "Crossfade", src, 0, 72_000, 60_000));
        assert!(matches!(export_audio(&s), Err(EngineError::Unsupported(message)) if message.contains("overlapping")));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_unsaved_offline_or_stretched_sources() {
        let (mut s, path) = fixture();
        s.sources[0].unsaved = true;
        assert!(matches!(export_audio(&s), Err(EngineError::Unsupported(message)) if message.contains("save the session")));
        s.sources[0].unsaved = false;
        s.tracks[0].playlist_mut().unwrap().clips[0].stretch = 1.25;
        assert!(matches!(export_audio(&s), Err(EngineError::Unsupported(message)) if message.contains("time-stretched")));
        std::fs::remove_file(path).unwrap();
        s.tracks[0].playlist_mut().unwrap().clips[0].stretch = 1.0;
        assert!(matches!(export_audio(&s), Err(EngineError::Io(message)) if message.contains("offline")));
    }

    #[test]
    fn rejects_inexact_sample_positions() {
        assert!(rational_time(MAX_EXACT_SAMPLE, 48_000).is_ok());
        assert!(rational_time(MAX_EXACT_SAMPLE + 1, 48_000).is_err());
        assert!(rational_time(-1, 48_000).is_err());
    }

    #[test]
    fn respects_media_read_root_before_resolving_paths() {
        let (session, media) = fixture();
        let allowed = media.parent().unwrap().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let roots = crate::file_access::AutomationRoots::new(&[allowed.as_path()], &[]).unwrap().unwrap();
        let result = crate::file_access::confine(Some(&roots), || export_audio(&session));
        assert!(matches!(result, Err(EngineError::Io(message)) if message.contains("outside the read root")));
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn failed_command_does_not_write_partial_interchange() {
        use serde_json::json;
        let (mut session, media) = fixture();
        session.sources[0].unsaved = true;
        let file = media.with_extension("otio");
        let mut engine = crate::Engine::new(session);
        let result = engine.execute("file.export_otio", &json!({"path": file.to_string_lossy()}));
        assert!(result.is_err());
        assert!(!file.exists());
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn export_command_needs_replace_before_overwriting_a_file() {
        let (session, media) = fixture();
        let path = media.with_extension("otio");
        let path_str = path.to_str().unwrap();
        let mut engine = crate::Engine::new(session);
        engine.execute("file.export_otio", &json!({"path": path_str})).unwrap();
        assert!(
            matches!(engine.execute("file.export_otio", &json!({"path": path_str})), Err(EngineError::BadParams(_, message)) if message.contains("replace"))
        );
        engine.execute("file.export_otio", &json!({"path": path_str, "replace": true})).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn import_export_roundtrip_keeps_two_tracks_gaps_trims_and_undo() {
        let (session, media) = decodable_fixture();
        let document = write_interchange(&session, &media);
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let result = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap();
        assert_eq!(result["audio_tracks"], 2);
        assert_eq!(result["audio_clips"], 2);
        assert_eq!(result["media_sources"], 1);
        let imported = &engine.session().tracks;
        let percussion = imported.iter().find(|t| t.name == "Percussion").unwrap();
        let first = &percussion.clips()[0];
        assert_eq!((first.start, first.length), (48_000, 96_000));
        assert!(matches!(&first.content, ClipContent::Audio { offset: 12_000, .. }));
        let overdub = imported.iter().find(|t| t.name == "Overdub").unwrap();
        let second = &overdub.clips()[0];
        assert_eq!((second.start, second.length), (0, 48_000));
        assert!(matches!(&second.content, ClipContent::Audio { offset: 96_000, .. }));
        assert_eq!(engine.session().sources.len(), 1, "a reused media reference must decode only once");
        assert!(engine.can_undo());
        assert!(engine.undo());
        assert_eq!(*engine.session(), before);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn accepts_official_clip_two_media_references_and_muted_audio() {
        let (session, media) = decodable_fixture();
        let document = media.with_extension("otio");
        let mut value: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        let clips = value["tracks"]["children"][0]["children"].as_array_mut().unwrap();
        let first = clips.last_mut().unwrap();
        let media_reference = first.as_object_mut().unwrap().remove("media_reference").unwrap();
        first["OTIO_SCHEMA"] = json!("Clip.2");
        first["media_references"] = json!({"DEFAULT_MEDIA": media_reference});
        first["active_media_reference_key"] = json!("DEFAULT_MEDIA");
        first["enabled"] = json!(false);
        value["tracks"]["children"][0]["enabled"] = json!(false);
        std::fs::write(&document, serde_json::to_vec(&value).unwrap()).unwrap();
        let mut engine = Engine::default();
        let result = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap();
        assert_eq!(result["audio_clips"], 2);
        let percussion = engine.session().tracks.iter().find(|t| t.name == "Percussion").unwrap();
        assert!(percussion.inactive);
        assert!(percussion.clips()[0].muted);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn imports_a_timeline_serialized_by_the_official_otio_python_adapter() {
        // This fixture was generated by OpenTimelineIO 0.18.1's otio_json adapter. Only the
        // media URL is replaced so the test can use a fresh local WAV on every platform.
        let (_, media) = decodable_fixture();
        let mut official: Value = serde_json::from_str(include_str!("../tests/fixtures/otio_official_clip2.otio")).unwrap();
        official["tracks"]["children"][0]["children"][1]["media_references"]["DEFAULT_MEDIA"]["target_url"] =
            json!(file_url(&crate::io::checked_media_path(&media).unwrap()).unwrap());
        let document = media.with_extension("otio");
        let mut engine = Engine::default();
        let imported = import_audio(&mut engine, &document, &serde_json::to_vec(&official).unwrap()).unwrap();
        assert_eq!(imported, Import { tracks: 1, clips: 1, sources: 1 });
        let clip = &engine.session().tracks.iter().find(|t| t.name == "External edit").unwrap().clips()[0];
        assert_eq!((clip.start, clip.length), (10, 20));
        assert_eq!(clip.name, "Clip from OTIO");
        assert!(matches!(&clip.content, ClipContent::Audio { offset: 5, .. }));
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn clip_one_and_clip_two_media_timecode_origins_preserve_trimmed_waveforms_and_undo() {
        let (session, media) = decodable_fixture();
        let ramp = (0..480_000).map(|frame| frame as f32 / 960_000.0).collect::<Vec<_>>();
        let wav = soundcraft_audio_io::encode(&AudioBuffer { sample_rate: 48_000, channels: vec![ramp.clone()] }, &EncodeOptions::default()).unwrap();
        std::fs::write(&media, wav).unwrap();
        let document = media.with_extension("otio");
        let mut timeline: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        // Both edits refer to one physical WAV that starts at timecode sample 10,000.
        // Clip.1 uses media timecode 12,000 (file frame 2,000); Clip.2 uses 96,000
        // (file frame 86,000). Offsets must not be used as raw file-frame indices.
        for track in timeline["tracks"]["children"].as_array_mut().unwrap() {
            for clip in track["children"].as_array_mut().unwrap() {
                if clip["OTIO_SCHEMA"] == "Clip.1" {
                    clip["media_reference"]["available_range"]["start_time"]["value"] = json!(10_000.0);
                    clip["media_reference"]["available_range"]["duration"]["value"] = json!(470_000.0);
                }
            }
        }
        let clip_two = &mut timeline["tracks"]["children"][1]["children"][0];
        let reference = clip_two.as_object_mut().unwrap().remove("media_reference").unwrap();
        clip_two["OTIO_SCHEMA"] = json!("Clip.2");
        clip_two["media_references"] = json!({"DEFAULT_MEDIA": reference});
        clip_two["active_media_reference_key"] = json!("DEFAULT_MEDIA");
        std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();

        let mut engine = Engine::default();
        let before = engine.session().clone();
        let result = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap();
        assert_eq!(result["audio_tracks"], 2);
        assert_eq!(result["audio_clips"], 2);
        assert_eq!(result["media_sources"], 1);
        let percussion = engine.session().tracks.iter().find(|t| t.name == "Percussion").unwrap();
        let overdub = engine.session().tracks.iter().find(|t| t.name == "Overdub").unwrap();
        assert!(matches!(&percussion.clips()[0].content, ClipContent::Audio { offset: 2_000, .. }));
        assert!(matches!(&overdub.clips()[0].content, ClipContent::Audio { offset: 86_000, .. }));
        let first = soundcraft_mix::render_clips(engine.session(), percussion.id, soundcraft_time::Range::new(48_000, 48_001));
        let second = soundcraft_mix::render_clips(engine.session(), overdub.id, soundcraft_time::Range::new(0, 1));
        assert!((first[0][0] - ramp[2_000]).abs() < 0.000001, "Clip.1 read from the wrong media frame");
        assert!((second[0][0] - ramp[86_000]).abs() < 0.000001, "Clip.2 read from the wrong media frame");
        assert!(engine.can_undo());
        assert!(engine.undo());
        assert_eq!(*engine.session(), before);
        assert!(engine.redo());
        assert_eq!(engine.session().sources.len(), 1);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn official_clip_two_negative_timecode_otioz_keeps_stereo_channels_gaps_and_undo() {
        let folder =
            std::env::temp_dir().join(format!("soundcraft-otio-signed-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        let project = folder.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let archive = folder.join("signed-origin.otioz");
        let timeline: Value = serde_json::from_str(OFFICIAL_NEGATIVE_ORIGIN_CLIP_TWO).unwrap();
        let left = (0..96_000).map(|frame| frame as f32 / 200_000.0).collect::<Vec<_>>();
        let right = (0..96_000).map(|frame| -0.7 + frame as f32 / 300_000.0).collect::<Vec<_>>();
        let wav =
            soundcraft_audio_io::encode(&AudioBuffer { sample_rate: 48_000, channels: vec![left.clone(), right.clone()] }, &EncodeOptions::default())
                .unwrap();
        let mut engine = Engine::new(Session { folder: Some(project.clone()), ..Session::default() });
        engine.path = Some(project.join("project.scraft").to_str().unwrap().to_owned());
        let before = engine.session().clone();

        // The official Clip.2 timecode is negative, but without available_range it would
        // require a negative decoded-file offset and must fail before committing any media.
        let mut bad = timeline.clone();
        bad["tracks"]["children"][0]["children"][1]["media_references"]["DEFAULT_MEDIA"]["available_range"] = Value::Null;
        std::fs::write(&archive, pack_negative_origin_bundle(&bad, &wav)).unwrap();
        let error = engine.execute("file.import_otioz", &json!({"path": archive.to_str().unwrap()})).unwrap_err();
        assert!(format!("{error}").contains("media offset cannot be negative"), "unexpected error: {error}");
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        assert!(!project.join("Audio Files").exists(), "failed bundle import leaked private media");

        std::fs::write(&archive, pack_negative_origin_bundle(&timeline, &wav)).unwrap();
        let result = engine.execute("file.import_otioz", &json!({"path": archive.to_str().unwrap()})).unwrap();
        assert_eq!(result["audio_tracks"], 2);
        assert_eq!(result["audio_clips"], 2);
        assert_eq!(result["media_sources"], 1);
        assert_eq!(result["extracted_media_files"], 1);
        let lead = engine.session().tracks.iter().find(|t| t.name == "Negative lead").unwrap();
        let back = engine.session().tracks.iter().find(|t| t.name == "Negative back").unwrap();
        assert_eq!((lead.clips()[0].start, lead.clips()[0].length), (1_200, 12_000));
        assert_eq!((back.clips()[0].start, back.clips()[0].length), (0, 3_000));
        assert!(matches!(&lead.clips()[0].content, ClipContent::Audio { offset: 24_000, .. }));
        assert!(matches!(&back.clips()[0].content, ClipContent::Audio { offset: 2_000, .. }));
        for (track, position, source_frame) in [(lead.id, 1_200, 24_000), (back.id, 0, 2_000)] {
            let rendered = soundcraft_mix::render_clips(engine.session(), track, soundcraft_time::Range::new(position, position + 1));
            assert_eq!(rendered.len(), 2, "official stereo source must retain both distinct channels");
            assert!((rendered[0][0] - left[source_frame]).abs() < 0.0001, "wrong left waveform or media origin");
            assert!((rendered[1][0] - right[source_frame]).abs() < 0.0001, "right channel shifted or mixed into left");
        }
        assert!(engine.can_undo());
        assert!(engine.undo());
        assert_eq!(*engine.session(), before);
        assert!(engine.redo());
        assert_eq!(engine.session().sources.len(), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn official_untrimmed_clip_two_uses_full_available_range_without_shifting_stereo_audio() {
        let folder =
            std::env::temp_dir().join(format!("soundcraft-otio-untrimmed-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        let project = folder.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let archive = folder.join("untrimmed.otioz");
        let timeline: Value = serde_json::from_str(OFFICIAL_UNTRIMMED_CLIP_TWO).unwrap();
        let left = (0..96_000).map(|frame| 0.15 + frame as f32 / 300_000.0).collect::<Vec<_>>();
        let right = (0..96_000).map(|frame| -0.65 + frame as f32 / 250_000.0).collect::<Vec<_>>();
        let wav =
            soundcraft_audio_io::encode(&AudioBuffer { sample_rate: 48_000, channels: vec![left.clone(), right.clone()] }, &EncodeOptions::default())
                .unwrap();
        std::fs::write(&archive, pack_negative_origin_bundle(&timeline, &wav)).unwrap();
        let mut engine = Engine::new(Session { folder: Some(project.clone()), ..Session::default() });
        engine.path = Some(project.join("project.scraft").to_str().unwrap().to_owned());
        let before = engine.session().clone();

        let result = engine.execute("file.import_otioz", &json!({"path": archive.to_str().unwrap()})).unwrap();
        assert_eq!(result["audio_tracks"], 2);
        assert_eq!(result["audio_clips"], 2);
        assert_eq!(result["media_sources"], 1);
        assert_eq!(result["extracted_media_files"], 1);
        let lead = engine.session().tracks.iter().find(|t| t.name == "Untrimmed lead").unwrap();
        let back = engine.session().tracks.iter().find(|t| t.name == "Trimmed back").unwrap();
        assert_eq!((lead.clips()[0].start, lead.clips()[0].length), (1_200, 96_000));
        assert_eq!((back.clips()[0].start, back.clips()[0].length), (0, 3_000));
        assert!(matches!(&lead.clips()[0].content, ClipContent::Audio { offset: 0, .. }));
        assert!(matches!(&back.clips()[0].content, ClipContent::Audio { offset: 2_000, .. }));
        // The absent clip trim means exactly all 96000 decoded frames, *not* timecode
        // -48000 interpreted as a negative file seek, and not merely 48000 frames.
        for (track, position, source_frame) in [(lead.id, 1_200, 0), (lead.id, 49_200, 48_000), (back.id, 0, 2_000)] {
            let rendered = soundcraft_mix::render_clips(engine.session(), track, soundcraft_time::Range::new(position, position + 1));
            assert_eq!(rendered.len(), 2);
            assert!((rendered[0][0] - left[source_frame]).abs() < 0.0001, "incorrect left source frame {source_frame}");
            assert!((rendered[1][0] - right[source_frame]).abs() < 0.0001, "right source frame {source_frame} was mixed or shifted");
        }
        assert!(engine.undo());
        assert_eq!(*engine.session(), before);
        assert!(engine.redo());
        assert_eq!(engine.session().sources.len(), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn untrimmed_clip_two_rejects_missing_invalid_or_unrendered_ranges_atomically() {
        let folder =
            std::env::temp_dir().join(format!("soundcraft-otio-no-range-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
        let project = folder.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let archive = folder.join("no-range.otioz");
        let timeline: Value = serde_json::from_str(OFFICIAL_UNTRIMMED_CLIP_TWO).unwrap();
        let wav = soundcraft_audio_io::encode(
            &AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 96_000], vec![-0.45; 96_000]] },
            &EncodeOptions::default(),
        )
        .unwrap();
        let mut engine = Engine::new(Session { folder: Some(project.clone()), ..Session::default() });
        engine.path = Some(project.join("project.scraft").to_str().unwrap().to_owned());
        let before = engine.session().clone();
        let reference_path = "/tracks/children/0/children/1/media_references/DEFAULT_MEDIA/available_range";
        let mut no_available = timeline.clone();
        no_available.pointer_mut(reference_path).unwrap().clone_from(&Value::Null);
        let mut zero_duration = timeline.clone();
        zero_duration.pointer_mut(&format!("{reference_path}/duration/value")).unwrap().clone_from(&json!(0.0));
        let mut negative_duration = timeline.clone();
        negative_duration.pointer_mut(&format!("{reference_path}/duration/value")).unwrap().clone_from(&json!(-1.0));
        let mut exceeds_file = timeline.clone();
        exceeds_file.pointer_mut(&format!("{reference_path}/duration/value")).unwrap().clone_from(&json!(96_001.0));
        let mut unrendered_gain = timeline.clone();
        unrendered_gain["tracks"]["children"][0]["children"][1]["metadata"]["soundcraft"]["gain_db"] = json!(6.0);

        for (case, invalid) in [
            ("missing media duration", no_available),
            ("zero media duration", zero_duration),
            ("negative media duration", negative_duration),
            ("media duration beyond WAV", exceeds_file),
            ("unrendered gain", unrendered_gain),
        ] {
            std::fs::write(&archive, pack_negative_origin_bundle(&invalid, &wav)).unwrap();
            let error = engine.execute("file.import_otioz", &json!({"path": archive.to_str().unwrap()})).unwrap_err();
            assert_eq!(*engine.session(), before, "{case} modified the session: {error}");
            assert!(!engine.can_undo(), "{case} added an undo step");
            assert!(!project.join("Audio Files").exists(), "{case} left extracted media after rejection");
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn rejects_unbounded_or_invalid_negative_media_ranges_without_partial_import() {
        let (_, media) = decodable_fixture();
        let original_wav = std::fs::read(&media).unwrap();
        let document = media.with_extension("otio");
        let mut timeline: Value = serde_json::from_str(OFFICIAL_NEGATIVE_ORIGIN_CLIP_TWO).unwrap();
        let local_url = file_url(&crate::io::checked_media_path(&media).unwrap()).unwrap();
        for track in timeline["tracks"]["children"].as_array_mut().unwrap() {
            for clip in track["children"].as_array_mut().unwrap() {
                if clip["OTIO_SCHEMA"] == "Clip.2" {
                    clip["media_references"]["DEFAULT_MEDIA"]["target_url"] = json!(local_url);
                }
            }
        }
        let clip_path = "/tracks/children/0/children/1";
        let mut no_origin = timeline.clone();
        no_origin.pointer_mut(&format!("{clip_path}/media_references/DEFAULT_MEDIA/available_range")).unwrap().clone_from(&Value::Null);
        let mut outside_available = timeline.clone();
        outside_available
            .pointer_mut(&format!("{clip_path}/media_references/DEFAULT_MEDIA/available_range/start_time/value"))
            .unwrap()
            .clone_from(&json!(-20_000.0));
        let mut negative_duration = timeline.clone();
        negative_duration.pointer_mut(&format!("{clip_path}/source_range/duration/value")).unwrap().clone_from(&json!(-1.0));
        let mut excessive_origin = timeline.clone();
        excessive_origin.pointer_mut(&format!("{clip_path}/source_range/start_time/value")).unwrap().clone_from(&json!(-9_007_199_254_740_992.0));
        let mut negative_gap = timeline.clone();
        negative_gap["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"] = json!(-1.0);
        let mut clip_one = timeline.clone();
        let legacy = clip_one.pointer_mut(clip_path).unwrap();
        legacy["OTIO_SCHEMA"] = json!("Clip.1");
        legacy["media_reference"] = legacy["media_references"]["DEFAULT_MEDIA"].clone();
        let mut global_origin = timeline.clone();
        global_origin["global_start_time"] = json!({"OTIO_SCHEMA": "RationalTime.1", "value": -1.0, "rate": 48_000.0});

        let mut engine = Engine::default();
        let before = engine.session().clone();
        for (case, value) in [
            ("missing media origin", no_origin),
            ("clip outside available range", outside_available),
            ("negative duration", negative_duration),
            ("unbounded origin", excessive_origin),
            ("negative gap duration", negative_gap),
            ("signed Clip.1 origin", clip_one),
            ("global timeline offset", global_origin),
        ] {
            let error = import_audio(&mut engine, &document, &serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert_eq!(*engine.session(), before, "{case} mutated the session: {error}");
            assert!(!engine.can_undo(), "{case} added an undo step");
            assert_eq!(std::fs::read(&media).unwrap(), original_wav, "{case} altered external media");
        }
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn inconsistent_media_available_ranges_fail_without_partial_import_or_file_changes() {
        let (session, media) = decodable_fixture();
        let original_media = std::fs::read(&media).unwrap();
        let document = media.with_extension("otio");
        let exported: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        let cases = [(13_000.0, 470_000.0), (10_000.0, 1_000.0), (10_000.0, 0.0)];
        let mut engine = Engine::default();
        let before = engine.session().clone();
        for (available_start, available_duration) in cases {
            let mut timeline = exported.clone();
            let reference = &mut timeline["tracks"]["children"][0]["children"][1]["media_reference"]["available_range"];
            reference["start_time"]["value"] = json!(available_start);
            reference["duration"]["value"] = json!(available_duration);
            std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();
            let error = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap_err();
            assert!(format!("{error}").contains("available range"), "unexpected inconsistent-range error: {error}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
            assert_eq!(std::fs::read(&media).unwrap(), original_media);
        }
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn rejects_invalid_second_clip_without_adding_first_clip_or_source() {
        let (session, media) = decodable_fixture();
        let document = media.with_extension("otio");
        let mut value: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        value["tracks"]["children"][1]["children"][0]["media_reference"]["target_url"] = json!("https://invalid.example/audio.wav");
        std::fs::write(&document, serde_json::to_vec(&value).unwrap()).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let error = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap_err();
        assert!(format!("{error}").contains("only local file URLs"));
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn rejects_non_audio_tracks_nested_compositions_and_unrendered_processing() {
        let (session, media) = decodable_fixture();
        let export = export_audio(&session).unwrap();
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let mut value: Value = serde_json::from_slice(&export.json).unwrap();
        value["tracks"]["children"][0]["kind"] = json!("Video");
        assert!(import_audio(&mut engine, &media.with_extension("otio"), &serde_json::to_vec(&value).unwrap()).is_err());
        value["tracks"]["children"][0]["kind"] = json!("Audio");
        value["tracks"]["children"][0]["children"][1]["OTIO_SCHEMA"] = json!("Transition.1");
        assert!(import_audio(&mut engine, &media.with_extension("otio"), &serde_json::to_vec(&value).unwrap()).is_err());
        value["tracks"]["children"][0]["children"][1]["OTIO_SCHEMA"] = json!("Clip.1");
        value["tracks"]["children"][0]["children"][1]["metadata"]["soundcraft"]["gain_db"] = json!(6.0);
        assert!(import_audio(&mut engine, &media.with_extension("otio"), &serde_json::to_vec(&value).unwrap()).is_err());
        assert_eq!(*engine.session(), before);
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn clip_two_malformed_soundcraft_gain_and_fade_metadata_fails_atomically() {
        let (session, media) = decodable_fixture();
        let original_audio = std::fs::read(&media).unwrap();
        let document = media.with_extension("otio");
        let exported: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        let cases = [
            ("gain_db", json!("6")),
            ("gain_db", Value::Null),
            ("fade_in_samples", json!("500")),
            ("fade_out_samples", json!(0.0)),
            ("gain_env", json!({"point": 1})),
        ];
        let mut engine = Engine::default();
        let before = engine.session().clone();
        for (field, malformed) in cases {
            let mut timeline = exported.clone();
            let clip = first_clip_as_official_clip_two(&mut timeline);
            clip["metadata"]["soundcraft"][field] = malformed;
            std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();
            let error = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap_err();
            assert!(format!("{error}").contains(field), "wrong error for malformed {field}: {error}");
            assert_eq!(*engine.session(), before, "malformed {field} changed the document");
            assert!(!engine.can_undo(), "malformed {field} added an undo step");
            assert_eq!(std::fs::read(&media).unwrap(), original_audio, "malformed {field} changed source media");
        }
        let mut timeline = exported;
        first_clip_as_official_clip_two(&mut timeline)["metadata"]["soundcraft"] = json!("not an object");
        std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();
        assert!(engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).is_err());
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn clip_two_nondefault_soundcraft_processing_must_be_rendered_before_import() {
        let (session, media) = decodable_fixture();
        let document = media.with_extension("otio");
        let exported: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        let processing =
            [("gain_db", json!(6.0)), ("fade_in_samples", json!(100)), ("fade_out_samples", json!(100)), ("gain_env", json!([[0, 0.5]]))];
        let mut engine = Engine::default();
        let before = engine.session().clone();
        for (field, value) in processing {
            let mut timeline = exported.clone();
            first_clip_as_official_clip_two(&mut timeline)["metadata"]["soundcraft"][field] = value;
            std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();
            let error = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap_err();
            assert!(format!("{error}").contains("render SoundCraft clip processing"), "nondefault {field} was ignored: {error}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
        }
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn clip_two_zero_processing_and_unrelated_vendor_metadata_remain_importable() {
        let (session, media) = decodable_fixture();
        let document = media.with_extension("otio");
        let mut timeline: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        let clip = first_clip_as_official_clip_two(&mut timeline);
        clip["metadata"]["soundcraft"]["gain_db"] = json!(0.0);
        clip["metadata"]["soundcraft"]["fade_in_samples"] = json!(0);
        clip["metadata"]["soundcraft"]["fade_out_samples"] = json!(0);
        clip["metadata"]["soundcraft"]["gain_env"] = json!([]);
        clip["metadata"]["another_editor"] = json!({"custom_tag": "preserve ordinary third-party edit metadata"});
        std::fs::write(&document, serde_json::to_vec(&timeline).unwrap()).unwrap();
        let mut engine = Engine::default();
        let imported = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap();
        assert_eq!(imported["audio_tracks"], 2);
        assert_eq!(imported["audio_clips"], 2);
        assert_eq!(imported["media_sources"], 1);
        assert!(engine.can_undo());
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn rejects_traversal_and_outside_automation_root_before_reading_media() {
        let (session, media) = decodable_fixture();
        let document = write_interchange(&session, &media);
        let mut value: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        value["tracks"]["children"][0]["children"][1]["media_reference"]["target_url"] = json!("../drums%20%231.wav");
        let mut engine = Engine::default();
        assert!(
            matches!(import_audio(&mut engine, &document, &serde_json::to_vec(&value).unwrap()), Err(EngineError::Unsupported(message)) if message.contains("traversal"))
        );
        // The document lives inside the allowed root; the audio does not.
        let allowed = media.parent().unwrap().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let allowed_document = allowed.join("import.otio");
        std::fs::copy(&document, &allowed_document).unwrap();
        let roots = crate::file_access::AutomationRoots::new(&[allowed.as_path()], &[]).unwrap().unwrap();
        let error =
            crate::file_access::confine(Some(&roots), || engine.execute("file.import_otio", &json!({"path": allowed_document.to_str().unwrap()})));
        assert!(matches!(error, Err(EngineError::Io(message)) if message.contains("outside the read root")));
        assert!(engine.session().sources.is_empty());
        std::fs::remove_file(allowed_document).unwrap();
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }

    #[test]
    fn bounded_json_missing_media_and_inexact_times_fail_closed() {
        let (session, media) = decodable_fixture();
        let document = write_interchange(&session, &media);
        let mut engine = Engine::default();
        std::fs::write(&document, vec![b' '; MAX_OTIO_BYTES + 1]).unwrap();
        let error = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap_err();
        assert!(format!("{error}").contains("import limit"));
        assert_eq!(engine.session().tracks.len(), Engine::default().session().tracks.len());

        let mut value: Value = serde_json::from_slice(&export_audio(&session).unwrap().json).unwrap();
        value["tracks"]["children"][0]["children"][1]["source_range"]["duration"]["value"] = json!(1_000_000_000.0);
        assert!(import_audio(&mut engine, &document, &serde_json::to_vec(&value).unwrap()).is_err());
        assert!(reference_path("file://remote/share.wav", &document).is_err());
        assert!(reference_path("./%2E%2E/file.wav", &document).is_err());
        assert!(reference_path("%2E%2E/file.wav", &document).is_err());
        assert!(reference_path("file:///tmp/a%ZZ.wav", &document).is_err());
        assert_eq!(time_samples(&json!({"OTIO_SCHEMA": "RationalTime.1", "value": 441.0, "rate": 44100.0}), 48_000, false).unwrap(), 480);
        std::fs::remove_file(document).unwrap();
        std::fs::remove_file(media).unwrap();
    }
}
