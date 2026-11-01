//! Portable, media-inclusive OpenTimelineIO `.otioz` audio exports.
//!
//! The official OpenTimelineIO file-bundle format stores `content.otio`, `version.txt` and
//! uniquely named source media below `media/` in a ZIP archive. Unlike plain `.otio` JSON, a
//! recipient can take this bundle to another computer without resolving absolute source paths.
//! This is cut-only audio editorial interchange, not a rendered mix or an AAF/OMF/PTX file.

use crate::{EngineError, Result};
use serde_json::Value;
use soundcraft_audio_io::FileFormat;
use soundcraft_model::{Mixer, Route, Session, Source, SourceId, TrackKind};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

pub const MAX_TOTAL_MEDIA_BYTES: usize = 512 * 1024 * 1024;
const MAX_SINGLE_MEDIA_BYTES: usize = 256 * 1024 * 1024;
const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCES: usize = 1_024;

#[derive(Debug, Clone, Copy)]
struct Limits {
    single_media: usize,
    total_media: usize,
}

const EXPORT_LIMITS: Limits = Limits { single_media: MAX_SINGLE_MEDIA_BYTES, total_media: MAX_TOTAL_MEDIA_BYTES };

/// A portable archive of the audio editing decisions and all referenced media files.
pub struct Export {
    pub bytes: Vec<u8>,
    pub tracks: usize,
    pub clips: usize,
    pub media_sources: usize,
}

fn unsupported(message: impl Into<String>) -> EngineError {
    EngineError::Unsupported(format!("OpenTimelineIO audio bundle: {}", message.into()))
}

/// Nothing here renders effects into media. Reject any exported edit processing that a receiving
/// editor may silently discard; input routing/record-arm are acquisition settings, not edits.
fn ensure_raw_edits(session: &Session) -> Result<()> {
    for track in session.tracks.iter().filter(|track| track.kind == TrackKind::Audio) {
        let mixer = &track.mixer;
        if mixer.mute || mixer.solo || mixer.solo_safe || mixer.volume_db != 0.0 || mixer.trim_db != 0.0 {
            return Err(unsupported(format!("track `{}` has mute/solo/fader processing; render or reset it first", track.name)));
        }
        if mixer.phase_invert || mixer.pan != Mixer::new(track.format).pan || mixer.surround.is_some() {
            return Err(unsupported(format!("track `{}` has nonportable pan/phase processing", track.name)));
        }
        if mixer.inserts.iter().any(Option::is_some) || mixer.sends.iter().any(Option::is_some) || track.instrument.is_some() {
            return Err(unsupported(format!("track `{}` has nonportable inserts, sends or instruments", track.name)));
        }
        if mixer.vca.is_some() || !track.automation.is_empty() || mixer.output != Route::Main || track.frozen || track.elastic.is_some() {
            return Err(unsupported(format!("track `{}` has nonportable automation, routing or frozen processing", track.name)));
        }
        for clip in track.clips() {
            if clip.gain_db != 0.0 || !clip.gain_env.is_empty() || clip.fade_in.len != 0 || clip.fade_out.len != 0 {
                return Err(unsupported(format!("clip `{}` has nonportable gain or fades; render them first", clip.name)));
            }
        }
    }
    Ok(())
}

fn source_path(session: &Session, source: &Source) -> Result<PathBuf> {
    if source.unsaved || source.path.is_empty() {
        return Err(unsupported(format!("source `{}` is unsaved or has no media file", source.name)));
    }
    let path = Path::new(&source.path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        session.folder.as_ref().ok_or_else(|| unsupported("save this session before bundling relative media"))?.join(path)
    };
    crate::io::checked_media_path(absolute).map_err(|err| EngineError::Io(format!("source `{}` is offline: {err}", source.name)))
}

/// Deliberately use format-derived safe extensions and source-ID names, not untrusted basenames.
/// Two different directories may both contain `take.wav`; their archive names must not collide.
fn media_name(source: &Source) -> Result<String> {
    if source.format == FileFormat::Other {
        return Err(unsupported(format!("source `{}` has an unrecognized audio container", source.name)));
    }
    // The codec family does not reliably identify its container: AAC may be ADTS (`.aac`)
    // or MPEG-4 (`.m4a`). Preserve the original extension, but never trust it as a ZIP path.
    let ext = Path::new(&source.path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| unsupported(format!("source `{}` needs a recognized audio file extension", source.name)))?;
    if !matches!(
        ext.as_str(),
        "wav" | "wave" | "bwf" | "aiff" | "aif" | "aifc" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "mp4" | "aac" | "alac" | "caf"
    ) {
        return Err(unsupported(format!("source `{}` has an unsupported audio file extension", source.name)));
    }
    Ok(format!("media/source-{:016x}.{ext}", source.id.0))
}

fn internal_error(message: &str) -> EngineError {
    EngineError::Io(format!("OpenTimelineIO bundle staging failed: {message}"))
}

/// Create a native OTIOZ bundle, with bounded media reads and safe, relative archive links.
/// The command writes its output only if every source has passed validation and archival.
pub fn export_audio(session: &Session) -> Result<Export> {
    export_audio_with_limits(session, EXPORT_LIMITS)
}

fn export_audio_with_limits(session: &Session, limits: Limits) -> Result<Export> {
    ensure_raw_edits(session)?;
    let exported = crate::otio::export_audio(session)?;
    if exported.json.len() > MAX_CONTENT_BYTES {
        return Err(unsupported("timeline exceeds the 4 MiB portable OTIO limit"));
    }
    let mut timeline: Value = serde_json::from_slice(&exported.json).map_err(|_| internal_error("generated timeline JSON is invalid"))?;
    let tracks = timeline
        .get_mut("tracks")
        .and_then(|stack| stack.get_mut("children"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| internal_error("generated timeline lacks an audio track array"))?;

    // Canonical source path is the dedupe key; source IDs give deterministic, safe archive names.
    let mut by_path = BTreeMap::<PathBuf, String>::new();
    let mut files = BTreeMap::<String, PathBuf>::new();
    for track in tracks {
        let children = track.get_mut("children").and_then(Value::as_array_mut).ok_or_else(|| internal_error("track has no children"))?;
        for child in children {
            if child.get("OTIO_SCHEMA").and_then(Value::as_str) != Some("Clip.1") {
                continue;
            }
            let id = child
                .get("metadata")
                .and_then(|m| m.get("soundcraft"))
                .and_then(|m| m.get("source_id"))
                .and_then(Value::as_u64)
                .ok_or_else(|| internal_error("generated clip has no source id"))?;
            let src = session.source(SourceId(id)).ok_or_else(|| internal_error("generated clip refers to a missing media source"))?;
            let path = source_path(session, src)?;
            let name = if let Some(existing) = by_path.get(&path) {
                existing.clone()
            } else {
                if files.len() >= MAX_SOURCES {
                    return Err(unsupported("more than 1024 distinct source audio files"));
                }
                let name = media_name(src)?;
                if files.insert(name.clone(), path.clone()).is_some() {
                    return Err(internal_error("archive media names collided"));
                }
                by_path.insert(path, name.clone());
                name
            };
            let reference = child.get_mut("media_reference").ok_or_else(|| internal_error("generated clip has no media reference"))?;
            let url = reference.get_mut("target_url").ok_or_else(|| internal_error("generated clip has no URL"))?;
            *url = Value::String(name);
        }
    }

    let contents = serde_json::to_vec_pretty(&timeline).map_err(|err| EngineError::Io(format!("OTIO bundle JSON: {err}")))?;
    if contents.len() > MAX_CONTENT_BYTES {
        return Err(unsupported("timeline exceeds the 4 MiB portable OTIO limit"));
    }
    let zerr = |err: zip::result::ZipError| EngineError::Io(format!("OTIO bundle ZIP: {err}"));
    let ioerr = |err: std::io::Error| EngineError::Io(format!("OTIO bundle ZIP: {err}"));
    // The workspace ZIP feature set supports stored entries. All entries remain valid standard
    // ZIP members; audio is already compressed/encoded and does not benefit from deflation.
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("content.otio", options).map_err(zerr)?;
    zip.write_all(&contents).map_err(ioerr)?;
    zip.start_file("version.txt", options).map_err(zerr)?;
    zip.write_all(b"1.0.0\n").map_err(ioerr)?;

    let mut total_media = 0usize;
    for (name, path) in &files {
        let available = limits.total_media.saturating_sub(total_media);
        let read_limit = limits.single_media.min(available);
        let bytes = crate::io::read_file_limited(path, read_limit)?;
        total_media =
            total_media.checked_add(bytes.len()).filter(|n| *n <= limits.total_media).ok_or_else(|| unsupported("media exceeds 512 MiB"))?;
        zip.start_file(name.as_str(), options).map_err(zerr)?;
        zip.write_all(&bytes).map_err(ioerr)?;
    }
    let bytes = zip.finish().map_err(zerr)?.into_inner();
    // ZIP's own local headers, central directory and timeline must also remain bounded.
    let maximum = limits.total_media.saturating_add(MAX_CONTENT_BYTES).saturating_add(256 * 1024);
    if bytes.len() > maximum {
        return Err(unsupported("portable bundle exceeds the archive size limit"));
    }
    Ok(Export { bytes, tracks: exported.tracks, clips: exported.clips, media_sources: files.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use serde_json::json;
    use soundcraft_audio_io::{AudioBuffer, EncodeOptions};
    use soundcraft_model::{ChannelFormat, Clip, SourceAudio, TrackId};
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        session: Session,
        directory: PathBuf,
        sources: [PathBuf; 2],
        tracks: [TrackId; 2],
    }

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("soundcraft-otioz-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
            let folders = [directory.join("stereo"), directory.join("mono")];
            for folder in &folders {
                std::fs::create_dir_all(folder).unwrap();
            }
            // Deliberate basename collision: both original media files are called `take.wav`.
            let sources = [folders[0].join("take.wav"), folders[1].join("take.wav")];
            let buffers = [
                AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.31; 12_000], vec![-0.47; 12_000]] },
                AudioBuffer { sample_rate: 48_000, channels: vec![vec![-0.19; 12_000]] },
            ];
            let mut session = Session { name: "Portable audio".into(), folder: Some(directory.clone()), ..Session::default() };
            let mut source_ids = Vec::new();
            for (index, (path, buffer)) in sources.iter().zip(buffers).enumerate() {
                std::fs::write(path, soundcraft_audio_io::encode(&buffer, &EncodeOptions::default()).unwrap()).unwrap();
                let id = SourceId(session.alloc());
                let channels = buffer.num_channels();
                session.pool.insert(id, Arc::new(SourceAudio::new(buffer)));
                session.sources.push(Source {
                    id,
                    name: format!("Take {index}"),
                    path: path.to_str().unwrap().into(),
                    channels: channels as u16,
                    frames: 12_000,
                    sample_rate: 48_000,
                    format: FileFormat::Wav,
                    time_reference: 0,
                    unsaved: false,
                });
                source_ids.push(id);
            }
            let stereo = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Stereo"));
            let mono = session.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Mono"));
            for (track, source, clips) in
                [(stereo, source_ids[0], [(2_000, 0, 4_000), (8_000, 6_000, 2_000)]), (mono, source_ids[1], [(0, 0, 2_000), (4_000, 4_000, 4_000)])]
            {
                for (index, (offset, start, length)) in clips.into_iter().enumerate() {
                    let id = session.new_clip_id();
                    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(
                        id,
                        format!("Clip {index}"),
                        source,
                        offset,
                        start,
                        length,
                    ));
                }
            }
            Self { session, directory, sources, tracks: [stereo, mono] }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    #[test]
    fn bundle_is_deterministic_spec_layout_and_media_names_are_unique() {
        let fixture = Fixture::new();
        let first = export_audio(&fixture.session).unwrap();
        let repeated = export_audio(&fixture.session).unwrap();
        assert_eq!(first.bytes, repeated.bytes, "same source session must produce byte-identical OTIOZ");
        assert_eq!((first.tracks, first.clips, first.media_sources), (2, 4, 2));

        let mut archive = zip::ZipArchive::new(Cursor::new(&first.bytes)).unwrap();
        assert_eq!(archive.len(), 4, "content, version and exactly two distinct media assets");
        let mut version = String::new();
        archive.by_name("version.txt").unwrap().read_to_string(&mut version).unwrap();
        assert_eq!(version, "1.0.0\n");
        let mut json = String::new();
        archive.by_name("content.otio").unwrap().read_to_string(&mut json).unwrap();
        let timeline: Value = serde_json::from_str(&json).unwrap();
        let tracks = timeline["tracks"]["children"].as_array().unwrap();
        assert_eq!(tracks.len(), 2);
        for (index, track) in tracks.iter().enumerate() {
            assert_eq!(track["kind"], "Audio");
            let children = track["children"].as_array().unwrap();
            assert_eq!(children.len(), 3, "each track has two clips separated by a gap");
            assert_eq!(children[1]["OTIO_SCHEMA"], "Gap.1");
            for (clip_number, clip) in [&children[0], &children[2]].into_iter().enumerate() {
                let url = clip["media_reference"]["target_url"].as_str().unwrap();
                let expected = media_name(&fixture.session.sources[index]).unwrap();
                assert_eq!(url, expected, "source trims must reference bundled media, not absolute paths");
                assert!(url.starts_with("media/") && !url.contains("..") && !url.contains('\\'));
                let expected_offset: f64 = match (index, clip_number) {
                    (0, 0) => 2_000.0,
                    (0, 1) => 8_000.0,
                    (1, 0) => 0.0,
                    _ => 4_000.0,
                };
                assert_eq!(clip["source_range"]["start_time"]["value"].as_f64(), Some(expected_offset));
                assert_eq!(clip["name"], format!("Clip {clip_number}"));
            }
        }
        for (index, src) in fixture.sources.iter().enumerate() {
            let name = media_name(&fixture.session.sources[index]).unwrap();
            let mut audio = Vec::new();
            archive.by_name(&name).unwrap().read_to_end(&mut audio).unwrap();
            assert_eq!(audio, std::fs::read(src).unwrap(), "archive must contain original media without alteration");
        }
        // Opt-in external compatibility oracle: `OTIOZ_ORACLE_OUTPUT=/tmp/test.otioz cargo test ...`.
        if let Ok(output) = std::env::var("OTIOZ_ORACLE_OUTPUT") {
            std::fs::write(output, &first.bytes).unwrap();
        }
    }

    #[test]
    fn relocation_and_native_reimport_preserve_stereo_left_right_and_gaps() {
        let fixture = Fixture::new();
        let range = soundcraft_time::Range::new(0, 8_000);
        let original = fixture.tracks.map(|track| soundcraft_mix::render_clips(&fixture.session, track, range));
        assert!((original[0][0][0] - 0.31).abs() < 0.00001);
        assert!((original[0][1][0] + 0.47).abs() < 0.00001);
        assert!((original[1][0][0] + 0.19).abs() < 0.00001);

        let output = export_audio(&fixture.session).unwrap();
        let folder = fixture.directory.join("new-machine");
        std::fs::create_dir_all(folder.join("media")).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(&output.bytes)).unwrap();
        let mut content = Vec::new();
        archive.by_name("content.otio").unwrap().read_to_end(&mut content).unwrap();
        let document = folder.join("portable.otio");
        std::fs::write(&document, &content).unwrap();
        for src in &fixture.session.sources {
            let name = media_name(src).unwrap();
            let mut media = Vec::new();
            archive.by_name(&name).unwrap().read_to_end(&mut media).unwrap();
            std::fs::write(folder.join(name), media).unwrap();
        }
        let mut engine = Engine::default();
        let before = engine.session().clone();
        let imported = engine.execute("file.import_otio", &json!({"path": document.to_str().unwrap()})).unwrap();
        assert_eq!(
            (imported["audio_tracks"].as_u64(), imported["audio_clips"].as_u64(), imported["media_sources"].as_u64()),
            (Some(2), Some(4), Some(2))
        );
        for (index, name) in ["Stereo", "Mono"].iter().enumerate() {
            let track = engine.session().tracks.iter().find(|t| &t.name == name).unwrap();
            assert_eq!(track.clips()[0].start, 0);
            assert_eq!(track.clips()[1].start, if index == 0 { 6_000 } else { 4_000 });
            let rendered = soundcraft_mix::render_clips(engine.session(), track.id, range);
            assert_eq!(rendered.len(), original[index].len());
            for (channel, (was, now)) in original[index].iter().zip(&rendered).enumerate() {
                for (sample, (&a, &b)) in was.iter().zip(now).enumerate() {
                    assert!((a - b).abs() < 0.00001, "track {name} channel {channel} sample {sample}: {a} changed to {b}");
                }
            }
        }
        assert!(engine.can_undo());
        engine.undo();
        assert_eq!(*engine.session(), before);
    }

    #[test]
    fn fails_closed_for_unrendered_processing_missing_media_and_bounded_sizes() {
        let mut fixture = Fixture::new();
        let first_track = fixture.tracks[0];
        fixture.session.track_mut(first_track).unwrap().mixer.mute = true;
        assert!(matches!(export_audio(&fixture.session), Err(EngineError::Unsupported(message)) if message.contains("mute/solo")));
        fixture.session.track_mut(first_track).unwrap().mixer.mute = false;
        fixture.session.track_mut(first_track).unwrap().playlist_mut().unwrap().clips[0].gain_db = -3.0;
        assert!(matches!(export_audio(&fixture.session), Err(EngineError::Unsupported(message)) if message.contains("gain or fades")));
        fixture.session.track_mut(first_track).unwrap().playlist_mut().unwrap().clips[0].gain_db = 0.0;

        assert!(export_audio_with_limits(&fixture.session, Limits { single_media: 64, total_media: 64 }).is_err());
        assert!(export_audio_with_limits(&fixture.session, Limits { single_media: 256_000, total_media: 100_000 }).is_err());
        fixture.session.sources[0].unsaved = true;
        assert!(matches!(export_audio(&fixture.session), Err(EngineError::Unsupported(message)) if message.contains("unsaved")));
        fixture.session.sources[0].unsaved = false;
        let destination = fixture.directory.join("missing.otioz");
        std::fs::remove_file(&fixture.sources[1]).unwrap();
        let mut engine = Engine::new(fixture.session.clone());
        assert!(engine.execute("file.export_otioz", &json!({"path": destination.to_str().unwrap()})).is_err());
        assert!(!destination.exists(), "failed preflight must not write a partial bundle");
    }

    #[test]
    fn command_requires_otioz_extension_and_replace_confirmation() {
        let fixture = Fixture::new();
        let destination = fixture.directory.join("session.otioz");
        let mut engine = Engine::new(fixture.session.clone());
        assert!(engine.execute("file.export_otioz", &json!({"path": destination.with_extension("zip").to_str().unwrap()})).is_err());
        let exported = engine.execute("file.export_otioz", &json!({"path": destination.to_str().unwrap()})).unwrap();
        assert_eq!(exported["media_sources"], 2);
        let original = std::fs::read(&destination).unwrap();
        assert!(crate::io::replaces(&engine, "file.export_otioz", &destination));
        assert!(
            matches!(engine.execute("file.export_otioz", &json!({"path": destination.to_str().unwrap()})), Err(EngineError::BadParams(_, message)) if message.contains("replace"))
        );
        assert_eq!(std::fs::read(&destination).unwrap(), original);
        engine.execute("file.export_otioz", &json!({"path": destination.to_str().unwrap(), "replace": true})).unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), original);
    }
}
