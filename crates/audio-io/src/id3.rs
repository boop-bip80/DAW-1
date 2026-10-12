//! ID3v2.4 tags: an encoder (text, comment, URL, picture, chapter and table-of-contents frames)
//! and a reader for the frame layout, used to tag podcast MP3s with metadata and chapters.
//!
//! Written from the public ID3v2.4.0 structure and frames documents and the ID3v2 Chapter Frame
//! Addendum (id3.org). All text is written as UTF-8 (encoding byte 3).

use crate::{AudioError, Result};

/// Largest tag body the syncsafe size field can describe (2^28 - 1 bytes).
const MAX_TAG: usize = (1 << 28) - 1;
/// UTF-8 text encoding marker.
const UTF8: u8 = 3;

/// One ID3v2 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id3Frame {
    /// A text frame such as `TIT2` (title), `TALB` (album), `TPE1` (artist), `TRCK`, `TDRC`, `TLAN`.
    Text { id: [u8; 4], text: String },
    /// `TXXX`: user-defined text.
    UserText { description: String, value: String },
    /// `COMM`: a comment with a three-letter language code.
    Comment { language: [u8; 3], description: String, text: String },
    /// A URL frame such as `WOAS` (source) or `WORS` (station).
    Url { id: [u8; 4], url: String },
    /// `WXXX`: user-defined link.
    UserUrl { description: String, url: String },
    /// `APIC`: an attached picture; `kind` 3 is the front cover.
    Picture { mime: String, kind: u8, description: String, data: Vec<u8> },
    /// `CHAP`: a chapter from `start_ms` to `end_ms` with its own sub-frames (title, link, image).
    Chapter { element_id: String, start_ms: u32, end_ms: u32, frames: Vec<Id3Frame> },
    /// `CTOC`: a table of contents listing chapter element ids.
    TableOfContents { element_id: String, top_level: bool, ordered: bool, children: Vec<String>, frames: Vec<Id3Frame> },
}

/// Encode a complete ID3v2.4 tag (header and frames, no padding).
pub fn encode_id3v2(frames: &[Id3Frame]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for f in frames {
        write_frame(&mut body, f)?;
    }
    if body.len() > MAX_TAG {
        return Err(AudioError::TooLarge(format!("ID3 tag of {} bytes", body.len())));
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(10));
    out.extend_from_slice(b"ID3\x04\x00\x00");
    out.extend_from_slice(&syncsafe(body.len() as u32));
    out.extend_from_slice(&body);
    Ok(out)
}

/// The bytes after a leading ID3v2 tag (all of `bytes` when there is none or it is malformed).
pub fn strip_id3v2(bytes: &[u8]) -> &[u8] {
    match id3v2_len(bytes) {
        Some(n) => bytes.get(n..).unwrap_or(bytes),
        None => bytes,
    }
}

/// Total length of a leading ID3v2 tag, header and footer included.
fn id3v2_len(bytes: &[u8]) -> Option<usize> {
    let h = bytes.get(..10)?;
    if &h[..3] != b"ID3" || h[5] & 0x0f != 0 {
        return None;
    }
    let size = unsyncsafe(h.get(6..10)?)? as usize;
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    let total = size.checked_add(10)?.checked_add(footer)?;
    (total <= bytes.len()).then_some(total)
}

/// Replace any leading ID3v2 tag of `file` (an MP3 or other ID3-tagged stream) with `frames`.
pub fn retag(file: &[u8], frames: &[Id3Frame]) -> Result<Vec<u8>> {
    let tag = encode_id3v2(frames)?;
    let audio = strip_id3v2(file);
    let mut out = Vec::with_capacity(tag.len().saturating_add(audio.len()));
    out.extend_from_slice(&tag);
    out.extend_from_slice(audio);
    Ok(out)
}

/// The top-level frames of a leading ID3v2.3/2.4 tag, as `(id, body)` pairs.
pub fn read_id3v2_frames(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let total = id3v2_len(bytes).ok_or_else(|| AudioError::Malformed("no ID3v2 tag".into()))?;
    let version = bytes.get(3).copied().unwrap_or(0);
    let footer = if bytes.get(5).is_some_and(|f| f & 0x10 != 0) { 10 } else { 0 };
    let body = bytes.get(10..total.saturating_sub(footer)).ok_or_else(|| AudioError::Malformed("ID3 size".into()))?;
    read_frames(body, version)
}

/// Frames in `body` (a tag body or a CHAP/CTOC sub-frame area).
pub fn read_frames(body: &[u8], version: u8) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(h) = body.get(pos..pos.saturating_add(10)) {
        if h[0] == 0 {
            break; // padding
        }
        let id = String::from_utf8_lossy(&h[..4]).into_owned();
        let size = if version >= 4 {
            unsyncsafe(&h[4..8]).ok_or_else(|| AudioError::Malformed("frame size".into()))?
        } else {
            u32::from_be_bytes([h[4], h[5], h[6], h[7]])
        };
        let start = pos + 10;
        let end = start
            .checked_add(size as usize)
            .filter(|e| *e <= body.len())
            .ok_or_else(|| AudioError::Malformed(format!("frame {id} overruns the tag")))?;
        out.push((id, body.get(start..end).unwrap_or_default().to_vec()));
        pos = end;
    }
    Ok(out)
}

fn write_frame(out: &mut Vec<u8>, f: &Id3Frame) -> Result<()> {
    let mut b = Vec::new();
    let id: [u8; 4] = match f {
        Id3Frame::Text { id, text } => {
            check_id(id, b'T')?;
            b.push(UTF8);
            b.extend_from_slice(text.as_bytes());
            *id
        }
        Id3Frame::UserText { description, value } => {
            b.push(UTF8);
            push_cstr(&mut b, description);
            b.extend_from_slice(value.as_bytes());
            *b"TXXX"
        }
        Id3Frame::Comment { language, description, text } => {
            b.push(UTF8);
            b.extend_from_slice(language);
            push_cstr(&mut b, description);
            b.extend_from_slice(text.as_bytes());
            *b"COMM"
        }
        Id3Frame::Url { id, url } => {
            check_id(id, b'W')?;
            b.extend_from_slice(latin1(url).as_slice());
            *id
        }
        Id3Frame::UserUrl { description, url } => {
            b.push(UTF8);
            push_cstr(&mut b, description);
            b.extend_from_slice(latin1(url).as_slice());
            *b"WXXX"
        }
        Id3Frame::Picture { mime, kind, description, data } => {
            b.push(UTF8);
            b.extend_from_slice(latin1(mime).as_slice());
            b.push(0);
            b.push(*kind);
            push_cstr(&mut b, description);
            b.extend_from_slice(data);
            *b"APIC"
        }
        Id3Frame::Chapter { element_id, start_ms, end_ms, frames } => {
            push_element_id(&mut b, element_id)?;
            b.extend_from_slice(&start_ms.to_be_bytes());
            b.extend_from_slice(&end_ms.to_be_bytes());
            b.extend_from_slice(&[0xff; 8]); // byte offsets unused
            for sub in frames {
                write_frame(&mut b, sub)?;
            }
            *b"CHAP"
        }
        Id3Frame::TableOfContents { element_id, top_level, ordered, children, frames } => {
            push_element_id(&mut b, element_id)?;
            b.push(u8::from(*top_level) << 1 | u8::from(*ordered));
            let n = u8::try_from(children.len()).map_err(|_| AudioError::TooLarge("more than 255 chapters in one table of contents".into()))?;
            b.push(n);
            for c in children {
                push_element_id(&mut b, c)?;
            }
            for sub in frames {
                write_frame(&mut b, sub)?;
            }
            *b"CTOC"
        }
    };
    if b.len() > MAX_TAG {
        return Err(AudioError::TooLarge(format!("ID3 frame of {} bytes", b.len())));
    }
    out.extend_from_slice(&id);
    out.extend_from_slice(&syncsafe(b.len() as u32));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&b);
    Ok(())
}

fn check_id(id: &[u8; 4], first: u8) -> Result<()> {
    if id[0] == first && id.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        Ok(())
    } else {
        Err(AudioError::Unsupported(format!("`{}` is not a {} frame id", String::from_utf8_lossy(id), first as char)))
    }
}

/// Element ids are NUL-terminated Latin-1 strings; keep them ASCII and NUL-free.
fn push_element_id(b: &mut Vec<u8>, id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(AudioError::Unsupported(format!("bad ID3 element id `{id}`")));
    }
    b.extend_from_slice(id.as_bytes());
    b.push(0);
    Ok(())
}

fn push_cstr(b: &mut Vec<u8>, s: &str) {
    b.extend(s.bytes().filter(|c| *c != 0));
    b.push(0);
}

/// URLs are ISO-8859-1 in ID3; anything else is percent-encoded.
fn latin1(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.bytes() {
        if c.is_ascii() && c != 0 {
            out.push(c);
        } else {
            out.extend_from_slice(format!("%{c:02X}").as_bytes());
        }
    }
    out
}

fn syncsafe(n: u32) -> [u8; 4] {
    [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
}

fn unsyncsafe(b: &[u8]) -> Option<u32> {
    let b = b.get(..4)?;
    if b.iter().any(|x| x & 0x80 != 0) {
        return None;
    }
    Some(u32::from(b[0]) << 21 | u32::from(b[1]) << 14 | u32::from(b[2]) << 7 | u32::from(b[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapters() -> Vec<Id3Frame> {
        vec![
            Id3Frame::Text { id: *b"TIT2", text: "Episode 1: Grüße".into() },
            Id3Frame::Comment { language: *b"deu", description: String::new(), text: "Notes".into() },
            Id3Frame::TableOfContents {
                element_id: "toc".into(),
                top_level: true,
                ordered: true,
                children: vec!["chp0".into(), "chp1".into()],
                frames: vec![],
            },
            Id3Frame::Chapter {
                element_id: "chp0".into(),
                start_ms: 0,
                end_ms: 61_500,
                frames: vec![
                    Id3Frame::Text { id: *b"TIT2", text: "Intro".into() },
                    Id3Frame::UserUrl { description: String::new(), url: "https://example.org/ä".into() },
                ],
            },
            Id3Frame::Chapter {
                element_id: "chp1".into(),
                start_ms: 61_500,
                end_ms: 120_000,
                frames: vec![Id3Frame::Text { id: *b"TIT2", text: "Topic".into() }],
            },
        ]
    }

    #[test]
    fn tag_round_trips_through_the_reader() {
        let tag = encode_id3v2(&chapters()).unwrap();
        assert_eq!(&tag[..5], b"ID3\x04\x00");
        let frames = read_id3v2_frames(&tag).unwrap();
        let ids: Vec<&str> = frames.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(ids, ["TIT2", "COMM", "CTOC", "CHAP", "CHAP"]);
        assert_eq!(&frames[0].1[1..], "Episode 1: Grüße".as_bytes());
        // CTOC: "toc\0", flags (top level + ordered), count 2, "chp0\0chp1\0".
        assert_eq!(frames[2].1, b"toc\0\x03\x02chp0\0chp1\0");
        // CHAP: element id, start, end, 8 offset bytes, then sub-frames.
        let chap = &frames[3].1;
        assert_eq!(&chap[..5], b"chp0\0");
        assert_eq!(u32::from_be_bytes([chap[9], chap[10], chap[11], chap[12]]), 61_500);
        let subs = read_frames(&chap[21..], 4).unwrap();
        assert_eq!(subs[0].0, "TIT2");
        assert_eq!(subs[1].1, b"\x03\0https://example.org/%C3%A4");
    }

    #[test]
    fn retag_replaces_an_existing_tag_and_keeps_the_audio() {
        let audio = b"\xff\xfb\x90\x00frames";
        let first = retag(audio, &chapters()).unwrap();
        let second = retag(&first, &[Id3Frame::Text { id: *b"TIT2", text: "New".into() }]).unwrap();
        assert_eq!(strip_id3v2(&second), audio);
        assert_eq!(read_id3v2_frames(&second).unwrap().len(), 1);
    }

    #[test]
    fn bad_ids_and_oversized_tables_are_errors() {
        assert!(encode_id3v2(&[Id3Frame::Text { id: *b"WXYZ", text: String::new() }]).is_err());
        let children = (0..256).map(|i| format!("c{i}")).collect();
        assert!(
            encode_id3v2(&[Id3Frame::TableOfContents { element_id: "t".into(), top_level: true, ordered: true, children, frames: vec![] }]).is_err()
        );
        assert!(encode_id3v2(&[Id3Frame::Chapter { element_id: "a b".into(), start_ms: 0, end_ms: 1, frames: vec![] }]).is_err());
    }

    #[test]
    fn hostile_tags_never_panic() {
        let tag = encode_id3v2(&chapters()).unwrap();
        for cut in 0..tag.len() {
            let _ = read_id3v2_frames(&tag[..cut]);
            let _ = strip_id3v2(&tag[..cut]);
        }
        let mut bad = tag.clone();
        bad[6] = 0x7f; // huge size
        assert!(read_id3v2_frames(&bad).is_err());
        assert_eq!(strip_id3v2(&bad), &bad[..]);
    }
}
