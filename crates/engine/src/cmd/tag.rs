//! Metadata tags on exported files: ID3v2.4 for MP3 (text, comment, links, cover, chapters).

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_audio_io::id3::{self, Id3Frame};
use soundcraft_model::MarkerKind;

/// Largest MP3 accepted for tagging.
const MAX_FILE: u64 = 4 << 30;
/// Largest cover or chapter image embedded.
const MAX_IMAGE: u64 = 16 << 20;
/// Longest accepted text value, in bytes.
const MAX_TEXT: usize = 64 * 1024;
/// Most chapters in one table of contents (the ID3 entry count is one byte).
const MAX_CHAPTERS: usize = 255;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        noundo "file.tag_mp3",
        "Tag MP3...",
        [],
        None,
        "{path, out?: path (default: in place), title?, artist?, album?, album_artist?, genre?, date?, track?, disc?, language?, comment?, url?, cover?: image path, chapters?: [{start: seconds, end?, title, url?, image?}] | 'markers', offset?: position subtracted from marker times} — replaces the file's ID3v2 tag",
        always,
        tag_mp3
    )]
}

/// A chapter in milliseconds from the start of the file.
struct Chapter {
    start_ms: u32,
    end_ms: Option<u32>,
    title: String,
    url: String,
    image: String,
}

fn text(p: &Value, key: &str) -> Result<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= MAX_TEXT => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(bad("file.tag_mp3", format!("`{key}` must be a string of at most {MAX_TEXT} bytes"))),
    }
}

fn read_capped(path: &str, cap: u64) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path).map_err(|err| EngineError::Io(format!("{path}: {err}")))?.len();
    if len > cap {
        return Err(bad("file.tag_mp3", format!("{path} is larger than {cap} bytes")));
    }
    std::fs::read(path).map_err(|err| EngineError::Io(format!("{path}: {err}")))
}

fn image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG") {
        Some("image/png")
    } else {
        None
    }
}

/// An `APIC` frame for the image at `path`.
fn picture(path: &str, kind: u8) -> Result<Id3Frame> {
    let data = read_capped(path, MAX_IMAGE)?;
    let mime = image_mime(&data).ok_or_else(|| bad("file.tag_mp3", format!("{path} is not a JPEG or PNG image")))?;
    Ok(Id3Frame::Picture { mime: mime.into(), kind, description: String::new(), data })
}

/// Milliseconds from seconds, clamped to the ID3 range.
fn ms(seconds: f64) -> u32 {
    if seconds.is_finite() { (seconds * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32 } else { 0 }
}

fn chapters_param(e: &Engine, p: &Value) -> Result<Vec<Chapter>> {
    const CMD: &str = "file.tag_mp3";
    let mut out = match p.get("chapters") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) if s == "markers" => {
            let offset = position_param(e, CMD, p, "offset")?.unwrap_or(0);
            let s = e.session();
            let to_ms = |t: Samples| ms(s.sample_rate.seconds(t.saturating_sub(offset)));
            s.markers
                .iter()
                .filter(|m| m.kind != MarkerKind::None && m.start >= offset)
                .map(|m| Chapter {
                    start_ms: to_ms(m.start),
                    end_ms: (m.kind == MarkerKind::Selection && m.end > m.start).then(|| to_ms(m.end)),
                    title: m.name.clone(),
                    url: String::new(),
                    image: String::new(),
                })
                .collect()
        }
        Some(Value::Array(items)) => {
            if items.len() > MAX_CHAPTERS {
                return Err(bad(CMD, format!("at most {MAX_CHAPTERS} chapters")));
            }
            let mut v = Vec::with_capacity(items.len());
            for c in items {
                let start = c.get("start").and_then(Value::as_f64).ok_or_else(|| bad(CMD, "each chapter needs a `start` in seconds"))?;
                v.push(Chapter {
                    start_ms: ms(start),
                    end_ms: c.get("end").and_then(Value::as_f64).map(ms),
                    title: text(c, "title")?.unwrap_or_default(),
                    url: text(c, "url")?.unwrap_or_default(),
                    image: text(c, "image")?.unwrap_or_default(),
                });
            }
            v
        }
        Some(_) => return Err(bad(CMD, "`chapters` must be an array or \"markers\"")),
    };
    if out.len() > MAX_CHAPTERS {
        return Err(bad(CMD, format!("at most {MAX_CHAPTERS} chapters")));
    }
    out.sort_by_key(|c| c.start_ms);
    Ok(out)
}

fn tag_mp3(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "file.tag_mp3";
    let path = str_param(p, "path").ok_or_else(|| bad(CMD, "`path` required"))?.to_string();
    let out = str_param(p, "out").map_or_else(|| path.clone(), str::to_string);
    let chapters = chapters_param(e, p)?;
    let file = read_capped(&path, MAX_FILE)?;
    let duration_ms = soundcraft_audio_io::probe(id3::strip_id3v2(&file), Some("mp3")).ok().map(|i| ms(i.duration_secs()));

    let mut frames = Vec::new();
    for (key, id) in [
        ("title", b"TIT2"),
        ("artist", b"TPE1"),
        ("album", b"TALB"),
        ("album_artist", b"TPE2"),
        ("genre", b"TCON"),
        ("date", b"TDRC"),
        ("track", b"TRCK"),
        ("disc", b"TPOS"),
        ("language", b"TLAN"),
    ] {
        if let Some(t) = text(p, key)? {
            frames.push(Id3Frame::Text { id: *id, text: t });
        }
    }
    if let Some(t) = text(p, "comment")? {
        frames.push(Id3Frame::Comment { language: *b"und", description: String::new(), text: t });
    }
    if let Some(u) = text(p, "url")? {
        frames.push(Id3Frame::UserUrl { description: String::new(), url: u });
    }
    if let Some(c) = text(p, "cover")? {
        frames.push(picture(&c, 3)?);
    }
    if !chapters.is_empty() {
        let ids: Vec<String> = (0..chapters.len()).map(|i| format!("chp{i}")).collect();
        frames.push(Id3Frame::TableOfContents {
            element_id: "toc".into(),
            top_level: true,
            ordered: true,
            children: ids.clone(),
            frames: Vec::new(),
        });
        for (i, (c, id)) in chapters.iter().zip(&ids).enumerate() {
            let next = chapters.get(i + 1).map(|n| n.start_ms);
            let end = c.end_ms.or(next).or(duration_ms).unwrap_or(c.start_ms).max(c.start_ms);
            let mut sub = vec![Id3Frame::Text { id: *b"TIT2", text: c.title.clone() }];
            if !c.url.is_empty() {
                sub.push(Id3Frame::UserUrl { description: String::new(), url: c.url.clone() });
            }
            if !c.image.is_empty() {
                sub.push(picture(&c.image, 0)?);
            }
            frames.push(Id3Frame::Chapter { element_id: id.clone(), start_ms: c.start_ms, end_ms: end, frames: sub });
        }
    }
    let bytes = id3::retag(&file, &frames).map_err(|err| bad(CMD, err.to_string()))?;
    // Write next to the target and rename, so a failed write never truncates the original.
    let tmp = format!("{out}.tagging");
    std::fs::write(&tmp, &bytes).map_err(|err| EngineError::Io(format!("{tmp}: {err}")))?;
    std::fs::rename(&tmp, &out).map_err(|err| EngineError::Io(format!("{out}: {err}")))?;
    Ok(json!({"path": out, "frames": frames.len(), "chapters": chapters.len(), "bytes": bytes.len()}))
}

#[cfg(test)]
mod tests {
    use crate::demo::demo_engine;
    use serde_json::json;
    use soundcraft_audio_io::id3::{read_frames, read_id3v2_frames, strip_id3v2};

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("soundcraft-tag-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tags_text_cover_and_chapters_and_keeps_audio() {
        let dir = tmp("full");
        let mp3 = dir.join("ep.mp3");
        let audio = b"\xff\xfb\x90\x00not really mpeg";
        std::fs::write(&mp3, audio).unwrap();
        let cover = dir.join("cover.png");
        std::fs::write(&cover, b"\x89PNG\r\n\x1a\nfake").unwrap();
        let mut e = demo_engine();
        let r = e
            .execute(
                "file.tag_mp3",
                &json!({
                    "path": mp3, "title": "Episode 1", "artist": "Host", "track": 1, "cover": cover,
                    "chapters": [{"start": 61.5, "title": "Topic", "url": "https://example.org"}, {"start": 0, "title": "Intro"}]
                }),
            )
            .unwrap();
        assert_eq!(r["chapters"], 2);
        let bytes = std::fs::read(&mp3).unwrap();
        assert_eq!(strip_id3v2(&bytes), audio);
        let frames = read_id3v2_frames(&bytes).unwrap();
        let ids: Vec<&str> = frames.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(ids, ["TIT2", "TPE1", "TRCK", "APIC", "CTOC", "CHAP", "CHAP"]);
        assert_eq!(&frames[2].1[1..], b"1");
        // Chapters are sorted; the first one ends where the second starts.
        let chap = &frames[5].1;
        assert_eq!(&chap[..5], b"chp0\0");
        assert_eq!(u32::from_be_bytes([chap[9], chap[10], chap[11], chap[12]]), 61_500);
        let subs = read_frames(&chap[21..], 4).unwrap();
        assert_eq!(&subs[0].1[1..], b"Intro");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn session_markers_become_chapters() {
        let dir = tmp("markers");
        let mp3 = dir.join("ep.mp3");
        std::fs::write(&mp3, b"\xff\xfb\x90\x00").unwrap();
        let out = dir.join("tagged.mp3");
        let mut e = demo_engine();
        let n = e.session().markers.iter().filter(|m| m.kind != soundcraft_model::MarkerKind::None).count();
        let r = e.execute("file.tag_mp3", &json!({"path": mp3, "out": out, "chapters": "markers"})).unwrap();
        assert_eq!(r["chapters"], n);
        assert!(out.exists());
        assert_eq!(std::fs::read(&mp3).unwrap(), b"\xff\xfb\x90\x00", "`out` leaves the source alone");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn bad_input_is_an_error_and_leaves_the_file_alone() {
        let dir = tmp("bad");
        let mp3 = dir.join("ep.mp3");
        std::fs::write(&mp3, b"audio").unwrap();
        let not_image = dir.join("cover.txt");
        std::fs::write(&not_image, b"hello").unwrap();
        let mut e = demo_engine();
        assert!(e.execute("file.tag_mp3", &json!({"path": mp3, "cover": not_image})).is_err());
        assert!(e.execute("file.tag_mp3", &json!({"path": mp3, "chapters": [{"title": "no start"}]})).is_err());
        let many: Vec<_> = (0..256).map(|i| json!({"start": i})).collect();
        assert!(e.execute("file.tag_mp3", &json!({"path": mp3, "chapters": many})).is_err());
        assert!(e.execute("file.tag_mp3", &json!({"path": dir.join("missing.mp3")})).is_err());
        assert_eq!(std::fs::read(&mp3).unwrap(), b"audio");
        std::fs::remove_dir_all(dir).ok();
    }
}
