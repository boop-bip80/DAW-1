//! Finding the clips under a block without walking every clip of a track.
//!
//! An edited drum take can hold tens of thousands of clips. Walking all of them for every block
//! costs the audio thread more than playing the few it finds, and drags megabytes of clip data
//! through the CPU caches ahead of the plugins. A [`ClipIndex`] answers "which clips overlap this
//! range" with two binary searches instead.
//!
//! It is built off the audio thread, from the same snapshot the audio thread will render, and it
//! only ever narrows the search: every candidate still goes through the same overlap test, in the
//! clips' own order, so a render with the index is bit-identical to one without. A track the index
//! does not describe (clips not sorted by start, or a different snapshot) is walked as before.

use soundcraft_model::{Clip, Session, Track, TrackId};
use soundcraft_time::{Range, Samples};

/// Where each track's clips lie in time, for one session snapshot. See the module docs.
#[derive(Debug, Default)]
pub struct ClipIndex {
    /// Sorted by track id.
    tracks: Vec<TrackClips>,
}

#[derive(Debug)]
struct TrackClips {
    track: TrackId,
    /// The clip list this describes: its address and length in the snapshot it was built from.
    /// A snapshot is immutable while it is shared, so a match means the same clips.
    clips_at: usize,
    len: usize,
    /// Clip starts, in clip order (which is sorted by start, or the track is not indexed).
    starts: Vec<Samples>,
    /// `max_end[i]`: the latest end among clips `0..=i`. Never decreases.
    max_end: Vec<Samples>,
}

impl ClipIndex {
    /// An index that describes no track: every lookup walks the whole track.
    pub const EMPTY: ClipIndex = ClipIndex { tracks: Vec::new() };

    /// Indexes the active playlist of every track of `s` whose clips are sorted by start.
    /// Allocates (two `Samples` per clip): build it off the audio thread.
    pub fn build(s: &Session) -> ClipIndex {
        let mut tracks: Vec<TrackClips> = s.tracks.iter().filter_map(TrackClips::build).collect();
        tracks.sort_unstable_by_key(|t| t.track.0);
        ClipIndex { tracks }
    }

    /// The clips of `t` that overlap `range`, in clip order: exactly those of
    /// `t.clips().iter().filter(|c| c.range().overlaps(&range))`.
    pub fn overlapping<'a>(&self, t: &'a Track, range: Range) -> impl Iterator<Item = &'a Clip> + 'a {
        let clips = t.clips();
        let span = self.span(t, clips, range).unwrap_or(0..clips.len());
        clips.get(span).unwrap_or_default().iter().filter(move |c| c.range().overlaps(&range))
    }

    /// The positions in `clips` that can overlap `range`, when this index describes them.
    fn span(&self, t: &Track, clips: &[Clip], range: Range) -> Option<std::ops::Range<usize>> {
        let at = self.tracks.binary_search_by_key(&t.id.0, |x| x.track.0).ok()?;
        let x = self.tracks.get(at)?;
        if x.len != clips.len() || x.clips_at != clips.as_ptr() as usize {
            return None;
        }
        // Clips from `hi` on start at or after the range's end; clips before `lo` (and so every
        // clip up to them) end at or before its start. Neither can overlap it.
        let hi = x.starts.partition_point(|&s| s < range.end);
        let lo = x.max_end.partition_point(|&e| e <= range.start).min(hi);
        Some(lo..hi)
    }

    /// Clips described, over all tracks (for tests and diagnostics).
    pub fn len(&self) -> usize {
        self.tracks.iter().map(|t| t.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TrackClips {
    fn build(t: &Track) -> Option<TrackClips> {
        let clips = t.clips();
        if clips.is_empty() || !clips.windows(2).all(|w| matches!(w, [a, b] if a.start <= b.start)) {
            return None;
        }
        let starts = clips.iter().map(|c| c.start).collect();
        let mut latest = Samples::MIN;
        let max_end = clips
            .iter()
            .map(|c| {
                latest = latest.max(c.end());
                latest
            })
            .collect();
        Some(TrackClips { track: t.id, clips_at: clips.as_ptr() as usize, len: clips.len(), starts, max_end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use soundcraft_model::{ChannelFormat, ClipId, SourceId, TrackKind};

    fn session_with(spans: &[(Samples, Samples)]) -> Session {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let pl = s.track_mut(t).and_then(|t| t.playlist_mut());
        if let Some(pl) = pl {
            for (i, &(start, length)) in spans.iter().enumerate() {
                pl.clips.push(Clip::audio(ClipId(i as u64 + 1), "c", SourceId(1), 0, start, length));
            }
        }
        s
    }

    fn naive(t: &Track, r: Range) -> Vec<u64> {
        t.clips().iter().filter(|c| c.range().overlaps(&r)).map(|c| c.id.0).collect()
    }

    fn indexed(ix: &ClipIndex, t: &Track, r: Range) -> Vec<u64> {
        ix.overlapping(t, r).map(|c| c.id.0).collect()
    }

    #[test]
    fn finds_the_clips_under_a_block() {
        let s = session_with(&[(0, 100), (100, 100), (150, 10), (300, 50)]);
        let ix = ClipIndex::build(&s);
        let t = &s.tracks[0];
        assert_eq!(ix.len(), 4);
        assert_eq!(indexed(&ix, t, Range::new(0, 64)), vec![1]);
        assert_eq!(indexed(&ix, t, Range::new(99, 101)), vec![1, 2]);
        assert_eq!(indexed(&ix, t, Range::new(140, 170)), vec![2, 3]);
        assert_eq!(indexed(&ix, t, Range::new(200, 300)), Vec::<u64>::new());
        assert_eq!(indexed(&ix, t, Range::new(349, 1000)), vec![4]);
    }

    #[test]
    fn a_long_clip_is_found_from_far_inside_it() {
        // The running maximum of the ends keeps a long early clip in reach of every later block.
        let s = session_with(&[(0, 10_000), (10, 5), (20, 5), (30, 5)]);
        let ix = ClipIndex::build(&s);
        assert_eq!(indexed(&ix, &s.tracks[0], Range::new(5_000, 5_128)), vec![1]);
    }

    #[test]
    fn unsorted_clips_are_walked() {
        let s = session_with(&[(500, 10), (0, 10)]);
        let ix = ClipIndex::build(&s);
        assert!(ix.is_empty(), "an unsorted track is not indexed");
        assert_eq!(indexed(&ix, &s.tracks[0], Range::new(0, 600)), vec![1, 2]);
    }

    #[test]
    fn another_snapshot_is_walked() {
        // An index used with a session it was not built from never hides a clip.
        let s = session_with(&[(0, 10), (20, 10)]);
        let ix = ClipIndex::build(&s);
        let mut other = s.clone();
        if let Some(pl) = other.tracks[0].playlist_mut() {
            pl.clips.push(Clip::audio(ClipId(9), "late", SourceId(1), 0, 40, 10));
        }
        assert_eq!(indexed(&ix, &other.tracks[0], Range::new(0, 100)), vec![1, 2, 9]);
        assert_eq!(indexed(&ClipIndex::EMPTY, &s.tracks[0], Range::new(0, 100)), vec![1, 2]);
    }

    proptest! {
        #[test]
        fn matches_a_walk_of_every_clip(
            mut spans in proptest::collection::vec((0i64..5_000, 1i64..800), 0..60),
            blocks in proptest::collection::vec((-100i64..6_000, 0i64..300), 1..40),
        ) {
            spans.sort_by_key(|s| s.0);
            let s = session_with(&spans);
            let ix = ClipIndex::build(&s);
            let t = &s.tracks[0];
            for (start, len) in blocks {
                let r = Range::new(start, start + len);
                prop_assert_eq!(indexed(&ix, t, r), naive(t, r));
            }
        }
    }
}
