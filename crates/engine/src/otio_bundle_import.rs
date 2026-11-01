//! Import a bounded, media-inclusive OpenTimelineIO `.otioz` bundle into a saved session.
//!
//! ZIP entry names are never extracted directly. We validate the complete archive/manifest,
//! allocate a private directory under the session's Audio Files folder, and stream only its
//! explicitly referenced media to generated safe filenames. The existing OTIO audio importer
//! then validates every edit and fully decodes the media in a temporary Engine. Only the final
//! validated native document is committed, as a single undo step. This is not AAF/OMF/PTX.

use crate::{Engine, EngineError, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAX_ARCHIVE_BYTES: u64 = 517 * 1024 * 1024;
const MAX_CONTENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_VERSION_BYTES: u64 = 64;
const MAX_SINGLE_MEDIA_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_MEDIA_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MEDIA_FILES: usize = 1024;
const MAX_ARCHIVE_ENTRIES: usize = MAX_MEDIA_FILES + 3;
const MAX_MEMBER_NAME_BYTES: usize = 256;

/// Read only the bounded ZIP end record and optional comment before asking the ZIP library to
/// allocate its central-directory index. ZIP64 and split archives are unnecessary for this
/// small, audio-only subset and would weaken our fixed entry-count allocation bound.
fn preflight_entry_count<R: Read + Seek>(reader: &mut R) -> Result<()> {
    const END_RECORD_BYTES: usize = 22;
    const MAX_COMMENT_BYTES: usize = u16::MAX as usize;
    let io_error = |err: std::io::Error| EngineError::Io(format!("OpenTimelineIO bundle ZIP end record: {err}"));
    let file_size = reader.seek(SeekFrom::End(0)).map_err(io_error)?;
    let tail_len = file_size.min((END_RECORD_BYTES + MAX_COMMENT_BYTES) as u64) as usize;
    if tail_len < END_RECORD_BYTES {
        return Err(unsupported("missing ZIP end-of-central-directory record"));
    }
    reader.seek(SeekFrom::End(-(tail_len as i64))).map_err(io_error)?;
    let mut tail = vec![0u8; tail_len];
    reader.read_exact(&mut tail).map_err(io_error)?;
    for at in (0..=tail_len - END_RECORD_BYTES).rev() {
        if &tail[at..at + 4] != b"PK\x05\x06" {
            continue;
        }
        let word = |offset: usize| u16::from_le_bytes([tail[at + offset], tail[at + offset + 1]]);
        let dword = |offset: usize| u32::from_le_bytes([tail[at + offset], tail[at + offset + 1], tail[at + offset + 2], tail[at + offset + 3]]);
        if at + END_RECORD_BYTES + usize::from(word(20)) != tail_len {
            continue;
        }
        let entries = word(10);
        if word(4) != 0 || word(6) != 0 || word(8) != entries {
            return Err(unsupported("multidisk ZIP bundles are not supported"));
        }
        if entries == u16::MAX || dword(12) == u32::MAX || dword(16) == u32::MAX {
            return Err(unsupported("ZIP64 bundles are outside the bounded audio subset"));
        }
        if usize::from(entries) > MAX_ARCHIVE_ENTRIES {
            return Err(unsupported("ZIP contains too many entries"));
        }
        return Ok(());
    }
    Err(unsupported("missing ZIP end-of-central-directory record"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Import {
    pub tracks: usize,
    pub clips: usize,
    pub sources: usize,
    pub extracted_media_files: usize,
}

fn unsupported(why: impl Into<String>) -> EngineError {
    EngineError::Unsupported(format!("OpenTimelineIO audio bundle: {}", why.into()))
}

fn zerr(err: zip::result::ZipError) -> EngineError {
    EngineError::Io(format!("OpenTimelineIO bundle ZIP: {err}"))
}

#[derive(Debug, Clone)]
struct MediaMember {
    index: usize,
    size: u64,
    ext: String,
}

struct ArchiveIndex {
    content: usize,
    version: usize,
    media: BTreeMap<String, MediaMember>,
}

fn media_member_extension(name: &str) -> Result<String> {
    let basename = name.strip_prefix("media/").ok_or_else(|| unsupported("media entries must live directly under `media/`"))?;
    if basename.is_empty()
        || basename == "."
        || basename == ".."
        || name.len() > MAX_MEMBER_NAME_BYTES
        || basename.chars().any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':' | '?' | '#'))
    {
        return Err(unsupported("unsafe media archive name (nested paths and traversal are forbidden)"));
    }
    let ext = basename.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).ok_or_else(|| unsupported("media file has no audio extension"))?;
    if !matches!(
        ext.as_str(),
        "wav" | "wave" | "bwf" | "aiff" | "aif" | "aifc" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "mp4" | "aac" | "alac" | "caf"
    ) {
        return Err(unsupported(format!("unsupported media extension `{ext}`; use an audio format SoundCraft can decode")));
    }
    Ok(ext)
}

fn audit_archive<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<ArchiveIndex> {
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(unsupported("archive contains too many entries"));
    }
    let mut names = BTreeSet::new();
    let mut content = None;
    let mut version = None;
    let mut media = BTreeMap::new();
    let mut total_media = 0u64;
    let mut total_all = 0u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(zerr)?;
        let name = file.name().map_err(zerr)?.into_owned();
        if !names.insert(name.clone()) {
            return Err(unsupported("duplicate ZIP member name"));
        }
        if file.encrypted() || !matches!(file.compression(), zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated) {
            return Err(unsupported("encrypted or unsupported compressed ZIP member"));
        }
        if file.is_symlink() {
            return Err(unsupported("symbolic links are never allowed in portable audio bundles"));
        }
        // ZIP 9 defines a directory entry by a trailing slash or backslash in its member name.
        // This examines archive metadata, not the host filesystem; keep all host IO in `io`.
        if name.ends_with('/') || name.ends_with('\\') {
            if name != "media/" || file.size() != 0 {
                return Err(unsupported("only an optional empty `media/` directory entry is permitted"));
            }
            continue;
        }
        if file.unix_mode().is_some_and(|mode| {
            let kind = mode & 0o170000;
            kind != 0 && kind != 0o100000
        }) {
            return Err(unsupported("nonregular ZIP entries are not supported"));
        }
        let size = file.size();
        total_all = total_all.checked_add(size).ok_or_else(|| unsupported("ZIP size total overflows"))?;
        if total_all > MAX_TOTAL_MEDIA_BYTES + MAX_CONTENT_BYTES + MAX_VERSION_BYTES {
            return Err(unsupported("ZIP exceeds the aggregate uncompressed size limit"));
        }
        match name.as_str() {
            "content.otio" => {
                if size == 0 || size > MAX_CONTENT_BYTES {
                    return Err(unsupported("content.otio is missing or exceeds the 4 MiB limit"));
                }
                content = Some(index);
            }
            "version.txt" => {
                if size == 0 || size > MAX_VERSION_BYTES {
                    return Err(unsupported("version.txt is invalid or oversized"));
                }
                version = Some(index);
            }
            _ => {
                let ext = media_member_extension(&name)?;
                if media.len() >= MAX_MEDIA_FILES || size == 0 || size > MAX_SINGLE_MEDIA_BYTES {
                    return Err(unsupported("too many media entries or an audio asset exceeds the 256 MiB limit"));
                }
                total_media = total_media
                    .checked_add(size)
                    .filter(|total| *total <= MAX_TOTAL_MEDIA_BYTES)
                    .ok_or_else(|| unsupported("referenced audio media exceeds the 512 MiB aggregate limit"))?;
                media.insert(name, MediaMember { index, size, ext });
            }
        }
    }
    let (content, version) =
        (content.ok_or_else(|| unsupported("missing content.otio"))?, version.ok_or_else(|| unsupported("missing version.txt"))?);
    if media.is_empty() {
        return Err(unsupported("the archive contains no audio media"));
    }
    Ok(ArchiveIndex { content, version, media })
}

fn read_small<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, index: usize, limit: u64) -> Result<Vec<u8>> {
    let mut member = archive.by_index(index).map_err(zerr)?;
    let expected = member.size();
    if expected > limit {
        return Err(unsupported("a ZIP manifest entry exceeds its memory limit"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(expected).map_err(|_| unsupported("manifest entry too large"))?);
    member.by_ref().take(limit + 1).read_to_end(&mut bytes).map_err(|err| EngineError::Io(format!("OTIO bundle ZIP entry: {err}")))?;
    if bytes.len() as u64 != expected {
        return Err(unsupported("ZIP manifest member length differs from its directory entry"));
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Media references must resolve to a literal bundled relative media entry, never to a path on
/// the exporting computer, a network location, or an escaped ZIP path component.
fn bundled_media_url(url: &str) -> Result<String> {
    if url.len() > 8_192 || !url.starts_with("media/") || url.chars().any(|ch| ch.is_control() || matches!(ch, '?' | '#' | ':' | '\\')) {
        return Err(unsupported("only relative `media/<filename>` audio URLs are permitted"));
    }
    // OpenTimelineIO's official bundle writer can emit a literal '%' in a relative media
    // filename. Its URL helper accepts both encoded and unencoded paths: decode *valid* %HH
    // sequences but preserve other percent signs as literal filename bytes. The decoded result
    // still must be one flat, safe `media/` member and match the audited archive index exactly.
    let input = url.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%'
            && let (Some(hi), Some(lo)) = (input.get(index + 1).and_then(|b| hex_digit(*b)), input.get(index + 2).and_then(|b| hex_digit(*b)))
        {
            bytes.push((hi << 4) | lo);
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(bytes).map_err(|_| unsupported("media URL is not valid UTF-8"))?;
    media_member_extension(&decoded)?;
    Ok(decoded)
}

/// Validate the audio-only semantic subset and rewrite references to newly allocated safe names.
/// No untrusted archive basename is ever used to create a file on disk.
fn rewrite_manifest(content: &[u8], index: &ArchiveIndex) -> Result<(Vec<u8>, BTreeMap<String, String>)> {
    let mut value: Value = serde_json::from_slice(content).map_err(|err| unsupported(format!("invalid content.otio JSON: {err}")))?;
    if value.get("OTIO_SCHEMA").and_then(Value::as_str) != Some("Timeline.1") {
        return Err(unsupported("content.otio must contain an OTIO Timeline.1"));
    }
    let tracks =
        value.pointer_mut("/tracks/children").and_then(Value::as_array_mut).ok_or_else(|| unsupported("OTIO timeline has no audio track array"))?;
    let mut safe_names = BTreeMap::new();
    for (number, (path, member)) in index.media.iter().enumerate() {
        safe_names.insert(path.clone(), format!("media/source-{:06}.{}", number + 1, member.ext));
    }
    let mut used = BTreeSet::new();
    for track in tracks {
        if track.get("OTIO_SCHEMA").and_then(Value::as_str) != Some("Track.1") || track.get("kind").and_then(Value::as_str) != Some("Audio") {
            return Err(unsupported("only sequential audio tracks are supported"));
        }
        let children = track.get_mut("children").and_then(Value::as_array_mut).ok_or_else(|| unsupported("OTIO audio track has no children"))?;
        for clip in children {
            let schema = clip.get("OTIO_SCHEMA").and_then(Value::as_str);
            let reference = match schema {
                Some("Gap.1") => continue,
                Some("Clip.1") => clip.get_mut("media_reference").ok_or_else(|| unsupported("clip has no media reference"))?,
                Some("Clip.2") => {
                    let key = clip.get("active_media_reference_key").and_then(Value::as_str).unwrap_or("DEFAULT_MEDIA").to_owned();
                    let references = clip
                        .get_mut("media_references")
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| unsupported("Clip.2 media references missing"))?;
                    if references.len() != 1 {
                        return Err(unsupported("Clip.2 alternate media references are not supported"));
                    }
                    references.get_mut(&key).ok_or_else(|| unsupported("Clip.2 active media reference missing"))?
                }
                _ => return Err(unsupported("transitions, nested compositions, video and effects are not supported in audio bundle import")),
            };
            if reference.get("OTIO_SCHEMA").and_then(Value::as_str) != Some("ExternalReference.1") {
                return Err(unsupported("every audio clip requires a bundled external media reference"));
            }
            let original = reference.get("target_url").and_then(Value::as_str).ok_or_else(|| unsupported("clip is missing a media URL"))?;
            let original = bundled_media_url(original)?;
            let safe = safe_names.get(&original).ok_or_else(|| unsupported(format!("clip references missing bundled media `{original}`")))?;
            used.insert(original);
            reference["target_url"] = Value::String(safe.clone());
        }
    }
    if used.len() != safe_names.len() {
        return Err(unsupported("archive contains unreferenced audio files"));
    }
    let rewritten = serde_json::to_vec(&value).map_err(|err| EngineError::Io(format!("OTIO bundle manifest serialization: {err}")))?;
    if rewritten.len() as u64 > MAX_CONTENT_BYTES {
        return Err(unsupported("rewritten audio timeline exceeds the 4 MiB limit"));
    }
    Ok((rewritten, safe_names))
}

/// Tracks only private files and directories allocated by this importer. Failure never calls a
/// recursive delete or erases an existing destination, and a panic still triggers best-effort
/// cleanup. On success the media remains in the session so undo can be redone without relinking.
struct StagedMedia {
    root: PathBuf,
    parent: PathBuf,
    parent_created: bool,
    root_created: bool,
    files: Vec<PathBuf>,
    media_dir: bool,
    committed: bool,
}

impl Drop for StagedMedia {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for path in self.files.iter().rev() {
            if let Err(err) = crate::io::remove_created_file(path) {
                log::warn!("OTIOZ failed import cleanup: {err}");
            }
        }
        if self.media_dir
            && let Err(err) = crate::io::remove_created_empty_dir(&self.root.join("media"))
        {
            log::warn!("OTIOZ media directory cleanup: {err}");
        }
        if self.root_created
            && let Err(err) = crate::io::remove_created_empty_dir(&self.root)
        {
            log::warn!("OTIOZ private directory cleanup: {err}");
        }
        if self.parent_created
            && let Err(err) = crate::io::remove_created_empty_dir(&self.parent)
        {
            log::warn!("OTIOZ new Audio Files directory cleanup: {err}");
        }
    }
}

fn private_destination(engine: &Engine) -> Result<StagedMedia> {
    let (parent, parent_created) = crate::io::private_session_audio_dir(engine)?;
    let mut staged =
        StagedMedia { root: parent.clone(), parent, parent_created, root_created: false, files: Vec::new(), media_dir: false, committed: false };
    for _ in 0..64 {
        let unique = crate::new_scheme_id(engine.session());
        let root = staged.parent.join(format!("OTIOZ-import-{unique:016x}"));
        if crate::io::create_private_dir(&root)? {
            staged.root = root;
            staged.root_created = true;
            return Ok(staged);
        }
    }
    Err(unsupported("could not allocate a unique import media directory"))
}

pub fn import_audio(engine: &mut Engine, archive_path: &Path) -> Result<Import> {
    // Read the ZIP central directory from a checked file handle without loading the archive.
    let mut file = crate::io::open_regular_limited(archive_path, MAX_ARCHIVE_BYTES)?;
    preflight_entry_count(&mut file)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zerr)?;
    // ZIP 9 indexes central-directory entries by filename and de-duplicates them in `len()`.
    // Its explicit consistency check detects raw duplicate entries, header mismatches and
    // overlapping byte ranges before our own visible-entry audit or filesystem mutation.
    archive.validate_header_consistency().map_err(zerr)?;
    let index = audit_archive(&mut archive)?;
    let version = read_small(&mut archive, index.version, MAX_VERSION_BYTES)?;
    if version != b"1.0.0" && version != b"1.0.0\n" {
        return Err(unsupported("only OTIOZ version 1.0.0 is supported"));
    }
    let content = read_small(&mut archive, index.content, MAX_CONTENT_BYTES)?;
    let (manifest, names) = rewrite_manifest(&content, &index)?;
    let mut actual_unpacked = (version.len() as u64).checked_add(content.len() as u64).ok_or_else(|| unsupported("unpacked size overflows"))?;
    let mut actual_media = 0u64;

    let mut staged = private_destination(engine)?;
    let media_dir = staged.root.join("media");
    if !crate::io::create_private_dir(&media_dir)? {
        return Err(unsupported("private media directory unexpectedly exists"));
    }
    staged.media_dir = true;
    for (original_name, safe_name) in names {
        let member = index.media.get(&original_name).ok_or_else(|| unsupported("media manifest changed unexpectedly"))?;
        let target = staged.root.join(safe_name);
        let mut source = archive.by_index(member.index).map_err(zerr)?;
        let written = crate::io::copy_new_file_limited(&target, &mut source, member.size, MAX_SINGLE_MEDIA_BYTES)?;
        staged.files.push(target);
        actual_media = actual_media
            .checked_add(written)
            .filter(|n| *n <= MAX_TOTAL_MEDIA_BYTES)
            .ok_or_else(|| unsupported("extracted audio exceeds 512 MiB"))?;
        actual_unpacked = actual_unpacked
            .checked_add(written)
            .filter(|n| *n <= MAX_TOTAL_MEDIA_BYTES + MAX_CONTENT_BYTES + MAX_VERSION_BYTES)
            .ok_or_else(|| unsupported("decompressed bundle exceeds the aggregate size cap"))?;
    }

    // `document` need not itself be on disk: the OTIO importer resolves relative media URLs
    // against the parent of this virtual document and fully validates every audio source.
    let virtual_document = staged.root.join("content.otio");
    let mut preview = Engine::new(engine.session().clone());
    let imported = crate::otio::import_audio(&mut preview, &virtual_document, &manifest)?;
    let extracted_media_files = staged.files.len();
    *engine.session_mut() = preview.session().clone();
    staged.committed = true;
    Ok(Import { tracks: imported.tracks, clips: imported.clips, sources: imported.sources, extracted_media_files })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileFormat};
    use soundcraft_model::{ChannelFormat, Clip, Source, SourceAudio, TrackId, TrackKind};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        folder: PathBuf,
        project: PathBuf,
        archive: PathBuf,
        session: soundcraft_model::Session,
        tracks: [TrackId; 2],
    }

    impl Fixture {
        fn new() -> Self {
            let folder =
                std::env::temp_dir().join(format!("soundcraft-otioz-import-{}-{}", std::process::id(), NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)));
            let project = folder.join("project");
            std::fs::create_dir_all(&project).unwrap();
            let mut session = soundcraft_model::Session { name: "Portable edit".into(), folder: Some(project.clone()), ..Default::default() };
            let data = [
                AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.31; 12_000], vec![-0.47; 12_000]] },
                AudioBuffer { sample_rate: 48_000, channels: vec![vec![-0.19; 12_000]] },
            ];
            let mut sources = Vec::new();
            for (index, audio) in data.into_iter().enumerate() {
                let path = folder.join(format!("take-{index}.wav"));
                std::fs::write(&path, soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap()).unwrap();
                let channels = audio.num_channels();
                let id = soundcraft_model::SourceId(session.alloc());
                session.pool.insert(id, Arc::new(SourceAudio::new(audio)));
                session.sources.push(Source {
                    id,
                    name: format!("Source {index}"),
                    path: path.to_str().unwrap().into(),
                    channels: channels as u16,
                    frames: 12_000,
                    sample_rate: 48_000,
                    format: FileFormat::Wav,
                    time_reference: 0,
                    unsaved: false,
                });
                sources.push(id);
            }
            let stereo = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Stereo"));
            let mono = session.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Mono"));
            for (track, src, clips) in
                [(stereo, sources[0], [(2_000, 0, 4_000), (8_000, 6_000, 2_000)]), (mono, sources[1], [(0, 0, 2_000), (4_000, 4_000, 4_000)])]
            {
                for (number, (offset, start, duration)) in clips.into_iter().enumerate() {
                    let id = session.new_clip_id();
                    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(
                        id,
                        format!("Clip {number}"),
                        src,
                        offset,
                        start,
                        duration,
                    ));
                }
            }
            let archive = folder.join("portable.otioz");
            Self { folder, project, archive, session, tracks: [stereo, mono] }
        }

        fn engine(&self) -> Engine {
            let mut engine = Engine::new(self.session.clone());
            engine.path = Some(self.project.join("project.scraft").to_str().unwrap().to_owned());
            engine
        }

        fn bundle(&self) -> Vec<u8> {
            crate::otio_bundle::export_audio(&self.session).unwrap().bytes
        }

        fn write(&self, data: &[u8]) {
            std::fs::write(&self.archive, data).unwrap();
        }

        fn owned_import_dirs(&self) -> Vec<PathBuf> {
            let dir = self.project.join("Audio Files");
            let Ok(iter) = std::fs::read_dir(dir) else { return Vec::new() };
            iter.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("OTIOZ-import-")))
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.folder).unwrap();
        }
    }

    type Entry = (String, Vec<u8>, zip::CompressionMethod);

    fn entries(bytes: &[u8]) -> Vec<Entry> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|index| {
                let mut file = archive.by_index(index).unwrap();
                let mut content = Vec::new();
                file.read_to_end(&mut content).unwrap();
                (file.name().unwrap().into_owned(), content, file.compression())
            })
            .collect()
    }

    fn pack(entries: &[Entry]) -> Vec<u8> {
        use std::io::Write as _;

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, compression) in entries {
            let options = zip::write::SimpleFileOptions::default().compression_method(*compression);
            writer.start_file(name.as_str(), options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// Change one central-directory field without allocating the declared amount of media.
    /// This creates hostile ZIP headers with impossible lengths, CRCs and compression methods.
    fn alter_central(mut bytes: Vec<u8>, name: &str, field: usize, replacement: &[u8]) -> Vec<u8> {
        let mut cursor = 0usize;
        let mut modified = false;
        while cursor + 46 <= bytes.len() {
            if &bytes[cursor..cursor + 4] != b"PK\x01\x02" {
                cursor += 1;
                continue;
            }
            let size = |at| u16::from_le_bytes([bytes[cursor + at], bytes[cursor + at + 1]]) as usize;
            let filename_len = size(28);
            let extension_len = size(30);
            let comment_len = size(32);
            let filename_begin = cursor + 46;
            let next = filename_begin + filename_len + extension_len + comment_len;
            assert!(next <= bytes.len());
            if &bytes[filename_begin..filename_begin + filename_len] == name.as_bytes() {
                bytes[cursor + field..cursor + field + replacement.len()].copy_from_slice(replacement);
                modified = true;
                break;
            }
            cursor = next;
        }
        assert!(modified, "expected ZIP central directory entry {name}");
        bytes
    }

    /// ZIP 9's safe writer refuses duplicate names. Produce a deliberately hostile fixture by
    /// renaming a same-length entry in both ZIP local and central headers after writing it.
    fn duplicate_version_name(mut bytes: Vec<u8>) -> Vec<u8> {
        let old = b"version.txu";
        let new = b"version.txt";
        let mut changed = 0;
        for start in 0..=bytes.len() - old.len() {
            if &bytes[start..start + old.len()] == old {
                bytes[start..start + old.len()].copy_from_slice(new);
                changed += 1;
            }
        }
        assert_eq!(changed, 2, "fixture must mutate exactly one local and one central ZIP entry name");
        bytes
    }

    fn import_command(engine: &mut Engine, path: &Path) -> Result<Value> {
        engine.execute("file.import_otioz", &json!({"path": path.to_str().unwrap()}))
    }

    #[test]
    fn native_otioz_roundtrip_keeps_asymmetric_stereo_and_timing_with_undo_redo() {
        let fixture = Fixture::new();
        let range = soundcraft_time::Range::new(0, 8_000);
        let original = fixture.tracks.map(|id| soundcraft_mix::render_clips(&fixture.session, id, range));
        assert!((original[0][0][0] - 0.31).abs() < 0.00001);
        assert!((original[0][1][0] + 0.47).abs() < 0.00001);
        fixture.write(&fixture.bundle());
        let mut engine = fixture.engine();
        let before = engine.session().clone();
        let imported = import_command(&mut engine, &fixture.archive).unwrap();
        assert_eq!(imported["audio_tracks"], 2);
        assert_eq!(imported["audio_clips"], 4);
        assert_eq!(imported["media_sources"], 2);
        assert_eq!(imported["extracted_media_files"], 2);
        assert_eq!(fixture.owned_import_dirs().len(), 1);
        let old_ids = before.tracks.iter().map(|track| track.id).collect::<std::collections::HashSet<_>>();
        let imported_tracks =
            engine.session().tracks.iter().filter(|track| track.kind == TrackKind::Audio && !old_ids.contains(&track.id)).collect::<Vec<_>>();
        assert_eq!(imported_tracks.len(), 2);
        for (number, track_name) in ["Stereo", "Mono"].iter().enumerate() {
            let found = imported_tracks[number];
            assert!(found.name.starts_with(track_name), "imported track name should remain recognizable");
            assert_eq!(found.clips()[1].start, if number == 0 { 6_000 } else { 4_000 });
            let rendered = soundcraft_mix::render_clips(engine.session(), found.id, range);
            assert_eq!(rendered.len(), original[number].len());
            for (channel, (expected, actual)) in original[number].iter().zip(&rendered).enumerate() {
                for (sample, (&a, &b)) in expected.iter().zip(actual).enumerate() {
                    assert!((a - b).abs() < 0.00001, "track {track_name} channel {channel} sample {sample}: {a} vs {b}");
                }
            }
        }
        for src in engine.session().sources.iter().skip(before.sources.len()) {
            assert!(Path::new(&src.path).starts_with(fixture.project.join("Audio Files")));
            assert!(Path::new(&src.path).exists());
        }
        assert!(engine.can_undo());
        engine.undo();
        assert_eq!(*engine.session(), before);
        assert_eq!(fixture.owned_import_dirs().len(), 1, "undo preserves media so redo never loses it");
        engine.redo();
        assert_eq!(engine.session().sources.len(), before.sources.len() + 2);
    }

    #[test]
    fn official_style_deflated_clip2_archive_imports_without_python_runtime() {
        let fixture = Fixture::new();
        let mut files = entries(&fixture.bundle());
        for (name, bytes, compression) in &mut files {
            if name == "version.txt" {
                *bytes = b"1.0.0".to_vec(); // Official OpenTimelineIO 0.18.1 writer omits newline.
                *compression = zip::CompressionMethod::Deflated;
            } else if name == "content.otio" {
                let mut manifest: Value = serde_json::from_slice(bytes).unwrap();
                for track in manifest["tracks"]["children"].as_array_mut().unwrap() {
                    for item in track["children"].as_array_mut().unwrap() {
                        if item["OTIO_SCHEMA"] != "Clip.1" {
                            continue;
                        }
                        let old = item.as_object_mut().unwrap().remove("media_reference").unwrap();
                        item["OTIO_SCHEMA"] = Value::String("Clip.2".into());
                        item["active_media_reference_key"] = Value::String("DEFAULT_MEDIA".into());
                        item["media_references"] = json!({"DEFAULT_MEDIA": old});
                    }
                }
                *bytes = serde_json::to_vec(&manifest).unwrap();
                *compression = zip::CompressionMethod::Deflated;
            }
        }
        fixture.write(&pack(&files));
        let mut engine = fixture.engine();
        let imported = import_command(&mut engine, &fixture.archive).unwrap();
        assert_eq!((imported["audio_tracks"].as_u64(), imported["audio_clips"].as_u64()), (Some(2), Some(4)));
        assert_eq!(fixture.owned_import_dirs().len(), 1);
    }

    /// Opt-in external compatibility oracle: point this at an archive emitted by the official
    /// `opentimelineio.adapters.otioz` writer (0.18+). CI's synthetic fixtures always run, while
    /// this test deliberately reports a skip unless an independently generated file is supplied.
    #[test]
    fn official_open_timeline_io_generated_fixture_when_supplied() {
        let Some(path) = std::env::var_os("SOUNDCRAFT_OFFICIAL_OTIOZ_FIXTURE") else {
            eprintln!("SKIP official OpenTimelineIO OTIOZ fixture: set SOUNDCRAFT_OFFICIAL_OTIOZ_FIXTURE to an independently generated .otioz");
            return;
        };
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let imported = import_command(&mut engine, Path::new(&path)).expect("official OTIOZ 0.18 archive must import");
        assert_eq!(imported["audio_tracks"], 2);
        assert_eq!(imported["audio_clips"], 4);
        assert_eq!(imported["media_sources"], 2);
        assert_eq!(imported["extracted_media_files"], 2);
        assert_eq!(fixture.owned_import_dirs().len(), 1);
        for source in engine.session().sources.iter().skip(fixture.session.sources.len()) {
            assert!(Path::new(&source.path).starts_with(fixture.project.join("Audio Files")));
            assert!(Path::new(&source.path).is_file());
        }
    }

    #[test]
    fn accepts_literal_and_encoded_percent_media_urls_without_unsafe_extraction_names() {
        let fixture = Fixture::new();
        let original = entries(&fixture.bundle());
        let original_name = original.iter().find(|(name, _, _)| name.starts_with("media/")).unwrap().0.clone();
        let bundled_name = "media/take%raw.wav";
        for target_url in [bundled_name, "media/take%25raw.wav"] {
            let mut files = original.clone();
            files.iter_mut().find(|(name, _, _)| *name == original_name).unwrap().0 = bundled_name.to_owned();
            let (_, content, _) = files.iter_mut().find(|(name, _, _)| name == "content.otio").unwrap();
            let mut timeline: Value = serde_json::from_slice(content).unwrap();
            for track in timeline["tracks"]["children"].as_array_mut().unwrap() {
                for clip in track["children"].as_array_mut().unwrap() {
                    if clip["OTIO_SCHEMA"] == "Clip.1" && clip["media_reference"]["target_url"].as_str() == Some(original_name.as_str()) {
                        clip["media_reference"]["target_url"] = Value::String(target_url.to_owned());
                    }
                }
            }
            *content = serde_json::to_vec(&timeline).unwrap();
            fixture.write(&pack(&files));
            let mut engine = fixture.engine();
            let before = engine.session().clone();
            let imported = import_command(&mut engine, &fixture.archive).unwrap();
            assert_eq!(imported["audio_tracks"], 2);
            assert_eq!(imported["audio_clips"], 4);
            assert_eq!(imported["media_sources"], 2);
            for source in engine.session().sources.iter().skip(before.sources.len()) {
                assert!(Path::new(&source.path).is_file());
                assert!(!source.path.contains('%'), "archive basenames must never become filesystem names");
            }
            assert!(engine.undo());
            assert_eq!(*engine.session(), before);
        }
    }

    #[test]
    fn literal_percent_compatibility_never_permits_encoded_traversal_or_separators() {
        assert_eq!(bundled_media_url("media/take%raw.wav").unwrap(), "media/take%raw.wav");
        assert_eq!(bundled_media_url("media/take%2g.wav").unwrap(), "media/take%2g.wav");
        assert_eq!(bundled_media_url("media/take%25raw.wav").unwrap(), "media/take%raw.wav");
        for unsafe_url in ["media/take%2Fraw.wav", "media/take%5Craw.wav", "media/%2e%2e%2Fsecret.wav", "media/C%3Aevil.wav", "media/take%00raw.wav"]
        {
            assert!(bundled_media_url(unsafe_url).is_err(), "accepted escaped unsafe media member {unsafe_url}");
        }
    }

    #[test]
    fn accepts_case_insensitive_bundle_extension_without_relaxing_archive_validation() {
        let fixture = Fixture::new();
        let upper = fixture.folder.join("Portable.OTIOZ");
        std::fs::write(&upper, fixture.bundle()).unwrap();
        let mut engine = fixture.engine();
        let imported = import_command(&mut engine, &upper).unwrap();
        assert_eq!(imported["audio_tracks"], 2);
        assert_eq!(fixture.owned_import_dirs().len(), 1);
    }

    #[test]
    fn rejects_traversal_duplicate_entry_and_bad_manifest_without_session_or_filesystem_mutation() {
        let fixture = Fixture::new();
        let base = entries(&fixture.bundle());
        let mut cases = Vec::<Vec<u8>>::new();
        for malicious in
            ["../escape.wav", "media/../secret.wav", "media/nested/secret.wav", "media\\bad.wav", "/media/abs.wav", "C:/root.wav", "media/x.exe"]
        {
            let mut items = base.clone();
            items.push((malicious.into(), b"evil".to_vec(), zip::CompressionMethod::Stored));
            cases.push(pack(&items));
        }
        let mut duplicate = base.clone();
        duplicate.push(("version.txu".into(), b"1.0.0".to_vec(), zip::CompressionMethod::Stored));
        cases.push(duplicate_version_name(pack(&duplicate)));
        let mut wrong_version = base.clone();
        wrong_version.iter_mut().find(|(name, _, _)| name == "version.txt").unwrap().1 = b"2.0.0".to_vec();
        cases.push(pack(&wrong_version));
        let mut missing_media = base.clone();
        missing_media.retain(|(name, _, _)| !name.starts_with("media/"));
        cases.push(pack(&missing_media));
        for (number, case) in cases.into_iter().enumerate() {
            fixture.write(&case);
            let mut engine = fixture.engine();
            let before = engine.session().clone();
            assert!(import_command(&mut engine, &fixture.archive).is_err(), "accepted archive case {number}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
            assert!(fixture.owned_import_dirs().is_empty(), "created private files for invalid archive {number}");
        }
    }

    #[test]
    fn rejects_external_urls_and_bad_media_then_cleans_only_owned_staging() {
        let fixture = Fixture::new();
        let original = entries(&fixture.bundle());
        for url in
            ["file:///etc/passwd", "https://host.invalid/audio.wav", "media/%2e%2e/secret.wav", "media/..%2Fsecret.wav", "media/C:%5csecret.wav"]
        {
            let mut files = original.clone();
            let (_, content, _) = files.iter_mut().find(|(n, _, _)| n == "content.otio").unwrap();
            let mut timeline: Value = serde_json::from_slice(content).unwrap();
            timeline["tracks"]["children"][0]["children"][0]["media_reference"]["target_url"] = Value::String(url.into());
            *content = serde_json::to_vec(&timeline).unwrap();
            fixture.write(&pack(&files));
            let mut engine = fixture.engine();
            let before = engine.session().clone();
            assert!(import_command(&mut engine, &fixture.archive).is_err(), "accepted external URL {url}");
            assert_eq!(*engine.session(), before);
            assert!(fixture.owned_import_dirs().is_empty());
        }

        let mut broken_audio = original;
        let (_, audio, _) = broken_audio.iter_mut().find(|(name, _, _)| name.starts_with("media/")).unwrap();
        *audio = b"this is not a WAV file".to_vec();
        fixture.write(&pack(&broken_audio));
        let mut engine = fixture.engine();
        let before = engine.session().clone();
        assert!(import_command(&mut engine, &fixture.archive).is_err());
        assert_eq!(*engine.session(), before);
        assert!(!engine.can_undo());
        assert!(fixture.owned_import_dirs().is_empty(), "decoder failure must remove every privately staged media file");
        assert!(!fixture.project.join("Audio Files").exists(), "a failed import must remove its newly created empty Audio Files directory");
    }

    #[test]
    fn rejects_lying_zip_sizes_crc_failures_symlinks_and_unsupported_compression() {
        let fixture = Fixture::new();
        let bytes = fixture.bundle();
        let names = entries(&bytes).into_iter().filter_map(|(name, _, _)| name.starts_with("media/").then_some(name)).collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        let inflated = alter_central(bytes.clone(), &names[0], 24, &(MAX_SINGLE_MEDIA_BYTES as u32 + 1).to_le_bytes());
        let oversize_content = alter_central(bytes.clone(), "content.otio", 24, &(MAX_CONTENT_BYTES as u32 + 1).to_le_bytes());
        let bad_crc = alter_central(bytes.clone(), &names[1], 16, &0u32.to_le_bytes());
        let symlink_mode = (0o120777u32 << 16).to_le_bytes();
        let symlink = alter_central(bytes.clone(), &names[0], 38, &symlink_mode);
        let bad_compression = alter_central(bytes, "content.otio", 10, &99u16.to_le_bytes());
        for (case, invalid) in [inflated, oversize_content, bad_crc, symlink, bad_compression].into_iter().enumerate() {
            fixture.write(&invalid);
            let mut engine = fixture.engine();
            let before = engine.session().clone();
            assert!(import_command(&mut engine, &fixture.archive).is_err(), "accepted hostile archive {case}");
            assert_eq!(*engine.session(), before);
            assert!(!engine.can_undo());
            assert!(fixture.owned_import_dirs().is_empty(), "hostile archive {case} left extracted media");
        }
    }

    #[test]
    fn refuses_excessive_entries_before_zip_directory_index_allocation() {
        let fixture = Fixture::new();
        let mut entries = entries(&fixture.bundle());
        for number in 0..=MAX_ARCHIVE_ENTRIES {
            entries.push((format!("extra-{number:04}.wav"), vec![number as u8], zip::CompressionMethod::Stored));
        }
        let excessive = pack(&entries);
        assert!(matches!(
            preflight_entry_count(&mut Cursor::new(&excessive)),
            Err(EngineError::Unsupported(message)) if message.contains("too many entries")
        ));
        fixture.write(&excessive);
        let mut engine = fixture.engine();
        let before = engine.session().clone();
        assert!(import_command(&mut engine, &fixture.archive).is_err());
        assert_eq!(*engine.session(), before);
        assert!(!fixture.project.join("Audio Files").exists());

        // The cheap preflight must also reject a forged ZIP directory entry count without
        // passing an attacker-controlled huge count into the ZIP parser.
        let mut forged = fixture.bundle();
        let end = forged.len() - 22;
        assert_eq!(&forged[end..end + 4], b"PK\x05\x06");
        let large = u16::try_from(MAX_ARCHIVE_ENTRIES + 1).unwrap().to_le_bytes();
        forged[end + 8..end + 10].copy_from_slice(&large);
        forged[end + 10..end + 12].copy_from_slice(&large);
        assert!(matches!(
            preflight_entry_count(&mut Cursor::new(&forged)),
            Err(EngineError::Unsupported(message)) if message.contains("too many entries")
        ));
    }

    #[test]
    fn requires_saved_session_and_respects_checked_write_roots() {
        let fixture = Fixture::new();
        fixture.write(&fixture.bundle());
        let mut unsaved = Engine::new(fixture.session.clone());
        let original = unsaved.session().clone();
        assert!(
            matches!(import_command(&mut unsaved, &fixture.archive), Err(EngineError::Unsupported(message)) if message.contains("save the session"))
        );
        assert_eq!(*unsaved.session(), original);
        assert!(fixture.owned_import_dirs().is_empty());

        let allowed_write = fixture.folder.join("allowed-output");
        std::fs::create_dir_all(&allowed_write).unwrap();
        let roots = crate::file_access::AutomationRoots::new(&[fixture.folder.as_path()], &[allowed_write.as_path()]).unwrap().unwrap();
        let mut engine = fixture.engine();
        let result = crate::file_access::confine(Some(&roots), || import_command(&mut engine, &fixture.archive));
        assert!(result.is_err(), "importer must not create media outside a permitted write root");
        assert_eq!(*engine.session(), fixture.session);
        assert!(fixture.owned_import_dirs().is_empty());
    }

    #[test]
    fn never_trusts_a_session_folder_disagreeing_with_its_saved_document_path() {
        let fixture = Fixture::new();
        fixture.write(&fixture.bundle());
        let other_folder = fixture.folder.join("unrelated");
        std::fs::create_dir(&other_folder).unwrap();
        let mut engine = fixture.engine();
        engine.session_mut().folder = Some(other_folder.clone());
        let before = engine.session().clone();
        assert!(matches!(
            import_command(&mut engine, &fixture.archive),
            Err(EngineError::Unsupported(message)) if message.contains("differs from the saved session")
        ));
        assert_eq!(*engine.session(), before);
        assert!(!other_folder.join("Audio Files").exists());
        assert!(fixture.owned_import_dirs().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn audio_files_symlink_outside_write_root_is_never_followed_or_removed() {
        let fixture = Fixture::new();
        fixture.write(&fixture.bundle());
        let external = fixture.folder.join("external-media");
        std::fs::create_dir(&external).unwrap();
        let sentinel = external.join("do-not-delete.txt");
        std::fs::write(&sentinel, "untouched user data").unwrap();
        std::os::unix::fs::symlink(&external, fixture.project.join("Audio Files")).unwrap();
        let mut engine = fixture.engine();
        let before = engine.session().clone();
        // The engine must refuse a symlink even with no explicit automation root.
        assert!(import_command(&mut engine, &fixture.archive).is_err());
        assert_eq!(*engine.session(), before);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched user data");
        assert!(fixture.owned_import_dirs().is_empty());

        // And it must not escape a checked write root via the same link.
        let roots = crate::file_access::AutomationRoots::new(&[fixture.folder.as_path()], &[fixture.project.as_path()]).unwrap().unwrap();
        assert!(crate::file_access::confine(Some(&roots), || import_command(&mut engine, &fixture.archive)).is_err());
        assert_eq!(*engine.session(), before);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched user data");
        assert!(fixture.owned_import_dirs().is_empty());
    }
}
