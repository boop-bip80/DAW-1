//! Export the audio edit of a session to OpenTimelineIO (`.otio`) JSON.
//!
//! This is a deliberately bounded interchange format, not an AAF/OMF or Pro Tools session
//! reader. OTIO carries the edit decisions (tracks, source trims, gaps, muted clips), while a
//! consumer must have access to the referenced audio files. MIDI, buses, automation and plug-in
//! processing do not have an equivalent representation in this audio-only export.

use crate::{EngineError, Result};
use serde_json::{Value, json};
use soundcraft_model::{ClipContent, Session, Source, TrackKind};
use std::path::{Path, PathBuf};

// OTIO RationalTime uses floating-point values. Integers through 2^53 are exact; larger sample
// positions must be rejected instead of silently shifting edits on import into another tool.
const MAX_EXACT_SAMPLE: i64 = (1_i64 << 53) - 1;
const MAX_TRACKS: usize = 2_048;
const MAX_CLIPS: usize = 100_000;
const MAX_PATH_BYTES: usize = 8_192;

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

fn media_url(session: &Session, source: &Source) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::FileFormat;
    use soundcraft_model::{ChannelFormat, Clip, SourceId, TrackKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

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
}
