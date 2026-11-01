//! Diagnostic-only estimates of a *single* audio stream's effective sample rate.
//!
//! CPAL timestamps can have unrelated origins for input and output. Each probe therefore sees
//! only one stream's capture (input) or predicted playback (output) timestamps, paired with the
//! number of frames in that stream's original device callbacks. The estimates are not duplex
//! synchronization, latency measurements, or a correction to the audio path.

use std::sync::atomic::{AtomicI32, Ordering};

/// Wait for a full second of frames so per-callback timestamp jitter has less influence.
const MIN_WINDOW_NS: u128 = 1_000_000_000;
/// An idle device or a clock jump should invalidate an estimate, not smear it into the next one.
const MAX_WINDOW_NS: u128 = 10_000_000_000;
/// A sample rate >2% from the advertised rate is more likely an xrun/timestamp discontinuity.
const MAX_ABS_PPM: f64 = 20_000.0;
const UNKNOWN_PPM: i32 = i32::MIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateEvent {
    /// Not enough frames have elapsed since the last estimate.
    Pending,
    /// The approximate effective rate, in parts per million above the configured sample rate.
    Estimate(i32),
    /// A timestamp, callback count, or rate discontinuity invalidated the prior estimate.
    Invalid,
}

/// One mutable probe per CPAL input or output callback, never shared between stream clocks.
pub(crate) struct StreamRateProbe {
    nominal_hz: u32,
    total_frames: u64,
    anchor: Option<(u64, u128)>,
    last_timestamp_ns: Option<u128>,
    filtered_ppm: Option<i32>,
}

impl StreamRateProbe {
    pub(crate) fn new(nominal_hz: u32) -> Self {
        Self { nominal_hz, total_frames: 0, anchor: None, last_timestamp_ns: None, filtered_ppm: None }
    }

    /// Observe *once per original device callback*, before the caller chunks its audio buffer.
    /// `timestamp_ns` must use this stream's own clock; `frames` is the total interleaved sample
    /// count divided by this stream's channel count. Neither branch allocates, locks, or logs.
    pub(crate) fn observe(&mut self, timestamp_ns: u128, frames: usize) -> RateEvent {
        let current_frame = self.total_frames;
        let Some(next_frames) = u64::try_from(frames).ok().and_then(|frames| current_frame.checked_add(frames)) else {
            // There is no usable frame index after an overflow. Start a fresh observation epoch.
            self.total_frames = 0;
            self.anchor = None;
            self.last_timestamp_ns = None;
            self.filtered_ppm = None;
            return RateEvent::Invalid;
        };
        self.total_frames = next_frames;

        if self.nominal_hz == 0 || frames == 0 {
            return self.reanchor(timestamp_ns, current_frame);
        }
        if self.last_timestamp_ns.is_some_and(|last| timestamp_ns <= last) {
            return self.reanchor(timestamp_ns, current_frame);
        }
        self.last_timestamp_ns = Some(timestamp_ns);

        let Some((anchor_frame, anchor_ns)) = self.anchor else {
            self.anchor = Some((current_frame, timestamp_ns));
            return RateEvent::Pending;
        };
        let Some(elapsed_ns) = timestamp_ns.checked_sub(anchor_ns) else {
            return self.reanchor(timestamp_ns, current_frame);
        };
        if elapsed_ns > MAX_WINDOW_NS {
            return self.reanchor(timestamp_ns, current_frame);
        }
        if elapsed_ns < MIN_WINDOW_NS {
            return RateEvent::Pending;
        }

        let elapsed_frames = current_frame.saturating_sub(anchor_frame);
        let effective_hz = elapsed_frames as f64 * 1_000_000_000.0 / elapsed_ns as f64;
        let raw_ppm = (effective_hz / f64::from(self.nominal_hz) - 1.0) * 1_000_000.0;
        if !raw_ppm.is_finite() || raw_ppm.abs() > MAX_ABS_PPM {
            return self.reanchor(timestamp_ns, current_frame);
        }

        let current_ppm = raw_ppm.round() as i32;
        // A bounded running average dampens callback timestamp jitter without any allocation.
        let filtered = self.filtered_ppm.map_or(current_ppm, |prev| (prev * 3 + current_ppm) / 4);
        self.filtered_ppm = Some(filtered);
        self.anchor = Some((current_frame, timestamp_ns));
        RateEvent::Estimate(filtered)
    }

    fn reanchor(&mut self, timestamp_ns: u128, frame: u64) -> RateEvent {
        self.anchor = Some((frame, timestamp_ns));
        self.last_timestamp_ns = Some(timestamp_ns);
        self.filtered_ppm = None;
        RateEvent::Invalid
    }
}

/// One atomic is enough: a sentinel makes an invalid estimate unambiguous to the UI thread.
pub(crate) struct RateTelemetry(AtomicI32);

impl Default for RateTelemetry {
    fn default() -> Self {
        Self(AtomicI32::new(UNKNOWN_PPM))
    }
}

impl RateTelemetry {
    pub(crate) fn publish(&self, event: RateEvent) {
        match event {
            RateEvent::Pending => {}
            RateEvent::Estimate(ppm) => self.0.store(ppm, Ordering::Relaxed),
            RateEvent::Invalid => self.0.store(UNKNOWN_PPM, Ordering::Relaxed),
        }
    }

    pub(crate) fn read(&self) -> Option<i32> {
        match self.0.load(Ordering::Relaxed) {
            UNKNOWN_PPM => None,
            ppm => Some(ppm),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed irregular-sized original callback blocks. Times use the stream's own independent
    /// origin, with optional alternating ±jitter, and each callback marks its first frame.
    fn feed(probe: &mut StreamRateProbe, origin_ns: u128, ppm: i32, jitter_ns: i64, blocks: usize) -> Vec<i32> {
        let frames = [128usize, 480, 192, 1024, 256, 64, 512, 384];
        let mut total = 0u64;
        let mut estimates = Vec::new();
        for i in 0..blocks {
            let elapsed_ns = (total as f64 * 1_000_000_000.0 / (48_000.0 * (1.0 + f64::from(ppm) / 1_000_000.0))) as u128;
            let jitter = if i % 2 == 0 { jitter_ns } else { -jitter_ns };
            let at = origin_ns.saturating_add(elapsed_ns).saturating_add_signed(i128::from(jitter));
            let block = frames[i % frames.len()];
            if let RateEvent::Estimate(estimate) = probe.observe(at, block) {
                estimates.push(estimate);
            }
            total += block as u64;
        }
        estimates
    }

    #[test]
    fn independently_started_streams_have_no_shared_time_origin_assumption() {
        let mut input = StreamRateProbe::new(48_000);
        let mut output = StreamRateProbe::new(48_000);
        let input_ppm = feed(&mut input, 1_000_000_000, 250, 0, 1700);
        let output_ppm = feed(&mut output, 10_000_000_000_000_000, -100, 0, 1700);
        assert!(input_ppm.len() >= 8 && output_ppm.len() >= 8);
        assert!((input_ppm.last().copied().unwrap_or_default() - 250).abs() <= 2);
        assert!((output_ppm.last().copied().unwrap_or_default() + 100).abs() <= 2);
    }

    #[test]
    fn irregular_callbacks_and_jitter_keep_bounded_drift_estimate() {
        let mut probe = StreamRateProbe::new(48_000);
        let measured = feed(&mut probe, 700_000_000_000, 500, 20_000, 2200);
        assert!(measured.len() >= 10);
        assert!((measured.last().copied().unwrap_or_default() - 500).abs() <= 30);
    }

    #[test]
    fn timestamp_discontinuities_invalidate_and_recover_without_spurious_rate() {
        let mut probe = StreamRateProbe::new(48_000);
        let telemetry = RateTelemetry::default();
        assert_eq!(telemetry.read(), None);
        let steady = feed(&mut probe, 10_000_000_000, 0, 0, 900);
        assert!(!steady.is_empty());
        telemetry.publish(RateEvent::Estimate(steady[steady.len() - 1]));
        assert_eq!(telemetry.read(), Some(0));

        let bad = probe.observe(3, 480);
        assert_eq!(bad, RateEvent::Invalid);
        telemetry.publish(bad);
        assert_eq!(telemetry.read(), None);
        assert_eq!(probe.observe(3, 480), RateEvent::Invalid);
        assert_eq!(probe.observe(4, 480), RateEvent::Pending);
        assert_eq!(probe.observe(20_000_000_000, 480), RateEvent::Invalid);
        let recovered = feed(&mut probe, 21_000_000_000, 0, 0, 1300);
        assert!(recovered.iter().any(|p| p.abs() <= 2));
    }

    #[test]
    fn arithmetic_edge_cases_invalidate_without_panicking() {
        let mut probe = StreamRateProbe::new(48_000);
        probe.total_frames = u64::MAX;
        assert_eq!(probe.observe(u128::MAX, 1), RateEvent::Invalid);
        assert_eq!(probe.observe(u128::MAX - 123, 480), RateEvent::Pending);
        assert_eq!(probe.observe(u128::MAX - 123, 480), RateEvent::Invalid);
        let mut zero = StreamRateProbe::new(0);
        assert_eq!(zero.observe(123, 0), RateEvent::Invalid);
        assert_eq!(zero.observe(124, 480), RateEvent::Invalid);
    }
}
