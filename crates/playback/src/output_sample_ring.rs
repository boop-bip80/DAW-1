//! Bounded, allocation-free single-producer/single-consumer interleaved output samples.
//!
//! This is a prerequisite for moving native CLAP/VST3/AU processing to a host-owned
//! `RenderWorker`, independent of CPAL's device callback lifetime. It is intentionally
//! **not wired to playback** yet: this ring alone does not make plugin shutdown safe.
//! A future worker must call CLAP `stop_processing` on its audio thread, return the
//! entire `AudioState` to the main/control thread, and only then allow native destroy.
//!
//! The producer commits whole frames with a Release head store after writing every
//! channel. The consumer acquires that head before reading, and releases the tail only
//! after consuming complete frames. Each sample is an `AtomicU32` containing its exact
//! `f32` bits, requiring no unsafe code, callback heap work, or mutexes. A caller must
//! provide exactly one producer and one consumer; extra concurrent writers/readers
//! violate the FIFO contract even though individual slot accesses remain memory-safe.
//! Reinitialization requires `&mut self` **after both endpoints have stopped/joined**.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Maximum interleaved output channels (SoundCraft's widest live mix).
pub const MAX_OUTPUT_CHANNELS: usize = 16;
/// Maximum capacity in complete frames. At 16 channels the slots consume 2 MiB.
pub const MAX_OUTPUT_RING_FRAMES: usize = 32_768;

/// Invalid construction parameters; the ring never silently clamps a requested layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRingConfigError {
    InvalidChannels(usize),
    InvalidCapacityFrames(usize),
}

/// Fixed-size SPSC frame FIFO. Methods take `&self` so distinct producer and consumer
/// threads can operate without locks; they must never call the same role concurrently.
#[derive(Debug)]
pub struct OutputSampleRing {
    channels: usize,
    capacity_frames: usize,
    slots: Box<[AtomicU32]>,
    /// Number of complete frames published by the producer (only producer writes).
    head: AtomicUsize,
    /// Number of complete frames consumed (only consumer writes).
    tail: AtomicUsize,
    /// Control-thread stream generation. Changing it requires exclusive ownership.
    generation: AtomicU64,
}

impl OutputSampleRing {
    /// Allocates every slot once, on the control thread before either endpoint starts.
    /// Capacity is exact, not rounded up or truncated.
    pub fn new(channels: usize, capacity_frames: usize, generation: u64) -> Result<Self, OutputRingConfigError> {
        if !(1..=MAX_OUTPUT_CHANNELS).contains(&channels) {
            return Err(OutputRingConfigError::InvalidChannels(channels));
        }
        if !(1..=MAX_OUTPUT_RING_FRAMES).contains(&capacity_frames) {
            return Err(OutputRingConfigError::InvalidCapacityFrames(capacity_frames));
        }
        let Some(samples) = channels.checked_mul(capacity_frames) else {
            return Err(OutputRingConfigError::InvalidCapacityFrames(capacity_frames));
        };
        Ok(Self {
            channels,
            capacity_frames,
            slots: (0..samples).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            generation: AtomicU64::new(generation),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Approximate queued complete frames during concurrent operation; exact when quiescent.
    pub fn queued_frames(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        head.wrapping_sub(tail).min(self.capacity_frames)
    }

    /// Producer only. Returns whole frames committed, or `None` for a buffer with
    /// incomplete interleaved frames. Saturation is reported as `Some(0)`; unread data
    /// is never overwritten. No heap allocation, mutex, or wait occurs.
    pub fn try_write_interleaved(&self, samples: &[f32]) -> Option<usize> {
        if !samples.len().is_multiple_of(self.channels) {
            return None;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let queued = head.wrapping_sub(tail).min(self.capacity_frames);
        let frames = (samples.len() / self.channels).min(self.capacity_frames - queued);
        for (offset, frame) in samples.chunks_exact(self.channels).take(frames).enumerate() {
            let base = (head.wrapping_add(offset) % self.capacity_frames) * self.channels;
            for (channel, sample) in frame.iter().enumerate() {
                self.slots[base + channel].store(sample.to_bits(), Ordering::Relaxed);
            }
        }
        self.head.store(head.wrapping_add(frames), Ordering::Release);
        Some(frames)
    }

    /// Consumer only. Copies available complete frames and fills the rest with silence;
    /// returns frames actually read or `None` for a non-frame-aligned output buffer.
    /// Even malformed output buffers are silenced, never left with stale audio.
    pub fn read_or_silence(&self, output: &mut [f32]) -> Option<usize> {
        if !output.len().is_multiple_of(self.channels) {
            output.fill(0.0);
            return None;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let frames = (output.len() / self.channels).min(head.wrapping_sub(tail).min(self.capacity_frames));
        for (offset, frame) in output.chunks_exact_mut(self.channels).take(frames).enumerate() {
            let base = (tail.wrapping_add(offset) % self.capacity_frames) * self.channels;
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = f32::from_bits(self.slots[base + channel].load(Ordering::Relaxed));
            }
        }
        output[frames * self.channels..].fill(0.0);
        self.tail.store(tail.wrapping_add(frames), Ordering::Release);
        Some(frames)
    }

    /// Discard all stale output after reconnect or transport generation change. The mutable
    /// borrow is intentional: both endpoints must be quiescent and all Arc clones released
    /// before this reset. No old frame is readable once the new generation is published.
    pub fn reset_for_generation(&mut self, generation: u64) {
        self.head.store(0, Ordering::Relaxed);
        self.tail.store(0, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn ring(channels: usize, frames: usize) -> OutputSampleRing {
        OutputSampleRing::new(channels, frames, 1).unwrap()
    }

    #[test]
    fn rejects_unbounded_or_non_frame_aligned_buffers_without_mutating_the_fifo() {
        assert!(matches!(OutputSampleRing::new(0, 2, 0), Err(OutputRingConfigError::InvalidChannels(0))));
        assert!(matches!(OutputSampleRing::new(17, 2, 0), Err(OutputRingConfigError::InvalidChannels(17))));
        assert!(matches!(OutputSampleRing::new(2, 0, 0), Err(OutputRingConfigError::InvalidCapacityFrames(0))));
        assert!(matches!(OutputSampleRing::new(2, MAX_OUTPUT_RING_FRAMES + 1, 0), Err(OutputRingConfigError::InvalidCapacityFrames(_))));
        let ring = ring(2, 2);
        assert_eq!(ring.try_write_interleaved(&[1.0, 2.0, 3.0]), None);
        assert_eq!(ring.queued_frames(), 0);
        let mut malformed = [9.0; 3];
        assert_eq!(ring.read_or_silence(&mut malformed), None);
        assert_eq!(malformed, [0.0; 3]);
        assert_eq!(ring.queued_frames(), 0);
    }

    #[test]
    fn mono_wrap_saturates_without_overwriting_and_silences_underruns() {
        let ring = ring(1, 3);
        assert_eq!(ring.try_write_interleaved(&[1.0, 2.0, 3.0, 4.0]), Some(3));
        assert_eq!(ring.queued_frames(), 3);
        assert_eq!(ring.try_write_interleaved(&[5.0]), Some(0));
        let mut first = [0.0; 2];
        assert_eq!(ring.read_or_silence(&mut first), Some(2));
        assert_eq!(first, [1.0, 2.0]);
        assert_eq!(ring.try_write_interleaved(&[4.0, 5.0, 6.0]), Some(2));
        let mut out = [99.0; 5];
        assert_eq!(ring.read_or_silence(&mut out), Some(3));
        assert_eq!(out, [3.0, 4.0, 5.0, 0.0, 0.0]);
        assert_eq!(ring.queued_frames(), 0);
    }

    #[test]
    fn stereo_saturation_never_splits_a_frame() {
        let ring = ring(2, 2);
        assert_eq!(ring.try_write_interleaved(&[10.0, 11.0, 20.0, 21.0, 30.0, 31.0]), Some(2));
        let mut first = [0.0; 2];
        assert_eq!(ring.read_or_silence(&mut first), Some(1));
        assert_eq!(first, [10.0, 11.0]);
        assert_eq!(ring.try_write_interleaved(&[30.0, 31.0]), Some(1));
        let mut output = [99.0; 6];
        assert_eq!(ring.read_or_silence(&mut output), Some(2));
        assert_eq!(output, [20.0, 21.0, 30.0, 31.0, 0.0, 0.0]);
    }

    #[test]
    fn sixteen_channel_frames_stay_aligned_across_wrap() {
        let ring = ring(16, 2);
        let frames: Vec<f32> = (0..48).map(|n| n as f32 + 0.25).collect();
        assert_eq!(ring.try_write_interleaved(&frames[..32]), Some(2));
        let mut first = [0.0; 16];
        assert_eq!(ring.read_or_silence(&mut first), Some(1));
        assert_eq!(first.as_slice(), &frames[..16]);
        assert_eq!(ring.try_write_interleaved(&frames[32..]), Some(1));
        let mut rest = [0.0; 32];
        assert_eq!(ring.read_or_silence(&mut rest), Some(2));
        assert_eq!(rest.as_slice(), &frames[16..]);
    }

    #[test]
    fn generation_reset_requires_unique_ownership_and_never_replays_old_samples() {
        let mut shared = Arc::new(ring(2, 3));
        assert_eq!(shared.try_write_interleaved(&[1.0, 2.0, 3.0, 4.0]), Some(2));
        let callback = Arc::clone(&shared);
        assert!(Arc::get_mut(&mut shared).is_none(), "a live callback/worker reference prohibits reset");
        drop(callback);
        let exclusive = Arc::get_mut(&mut shared).unwrap();
        exclusive.reset_for_generation(2);
        assert_eq!(exclusive.generation(), 2);
        assert_eq!(exclusive.queued_frames(), 0);
        let mut output = [99.0; 4];
        assert_eq!(exclusive.read_or_silence(&mut output), Some(0));
        assert_eq!(output, [0.0; 4]);
        assert_eq!(exclusive.try_write_interleaved(&[5.0, 6.0]), Some(1));
        assert_eq!(exclusive.read_or_silence(&mut output), Some(1));
        assert_eq!(output, [5.0, 6.0, 0.0, 0.0]);
    }

    #[test]
    fn reconnect_reset_discards_full_old_generation_at_capacity_one() {
        let mut ring = ring(1, 1);
        assert_eq!(ring.try_write_interleaved(&[42.0]), Some(1));
        assert_eq!(ring.try_write_interleaved(&[43.0]), Some(0));
        ring.reset_for_generation(u64::MAX);
        assert_eq!(ring.generation(), u64::MAX);
        assert_eq!(ring.try_write_interleaved(&[44.0]), Some(1));
        let mut output = [0.0; 2];
        assert_eq!(ring.read_or_silence(&mut output), Some(1));
        assert_eq!(output, [44.0, 0.0]);
    }

    #[test]
    fn concurrent_spsc_frames_remain_ordered_without_locks() {
        let ring = Arc::new(ring(2, 7));
        let producer = Arc::clone(&ring);
        let consumer = Arc::clone(&ring);
        let writer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for frame in 1..=5_000 {
                let samples = [frame as f32, -(frame as f32)];
                while producer.try_write_interleaved(&samples) == Some(0) {
                    assert!(Instant::now() < deadline, "output FIFO producer stalled");
                    thread::yield_now();
                }
            }
        });
        let reader = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut out = [0.0; 2];
            for frame in 1..=5_000 {
                while consumer.read_or_silence(&mut out) == Some(0) {
                    assert!(Instant::now() < deadline, "output FIFO consumer stalled");
                    thread::yield_now();
                }
                assert_eq!(out, [frame as f32, -(frame as f32)], "SPSC output must preserve frame order");
            }
        });
        writer.join().unwrap();
        reader.join().unwrap();
        assert_eq!(ring.queued_frames(), 0);
    }
}
