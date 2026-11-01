//! Audio input capture for recording.
//!
//! A [`Recorder`] opens the default input device. While armed, the input callback copies each
//! block into a preallocated lock-free ring (it never locks or allocates), and a writer thread
//! streams the ring into one WAV file per take, rewriting the header every second so a crash leaves
//! a playable file.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use soundcraft_model::TrackId;

/// What [`Recorder::finish`] returns: the takes that hold audio and the first problem met.
pub type Takes = (Vec<(TrackId, PathBuf)>, Option<String>);

/// Frames each block of a [`LiveTake`] sums up.
pub const LIVE_BLOCK: usize = 128;
/// Blocks a live take keeps per channel at most (three hours at 48 kHz); its file records on.
const LIVE_BLOCKS: usize = 1 << 22;

/// A take while it records, summed up for drawing: per channel, the (min, max) of each
/// [`LIVE_BLOCK`] frames so far.
pub type LiveTake = Vec<Vec<(f32, f32)>>;

const RING_SECONDS: usize = 4;
/// Maximum scratch bytes allocated per WAV take, even when the input ring has filled up.
const TAKE_WRITE_CHUNK_BYTES: usize = 64 * 1024;
/// How long a recording input may go without a callback before it counts as gone.
const STALL: Duration = Duration::from_secs(2);
const GONE: u8 = 1;
const BROKEN: u8 = 2;

/// Why the input can't go on.
#[derive(Debug, Clone, PartialEq)]
pub enum Trouble {
    /// The device went away or stopped sending audio, or its callback hit a bug.
    Device(String),
    /// Writing a take failed (a full disk, say).
    Disk(String),
}

/// Input capture FIFO pressure since the current take was armed. The peak is a bounded sample
/// occupancy percentage, and `affected_frames` counts physical input frames with at least one
/// sample dropped. This is a disk/writer backlog diagnostic, **not** an input/output clock or
/// measured latency estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapturePressure {
    pub peak_percent: u8,
    pub affected_frames: u64,
}

impl CapturePressure {
    /// A user-facing warning, constructed on the UI/writer thread rather than an audio callback.
    pub fn warning(self, sample_rate: u32) -> Option<String> {
        if self.affected_frames > 0 {
            let ms = self.affected_frames.saturating_mul(1000) / u64::from(sample_rate.max(1));
            let duration = if ms == 0 { "less than 1 ms".to_string() } else { format!("{ms} ms") };
            Some(format!(
                "Recording buffer overrun: {} input frame(s) had missing samples ({duration}); buffer peaked at {}%. Check disk throughput.",
                self.affected_frames, self.peak_percent
            ))
        } else if self.peak_percent >= 80 {
            Some(format!("Recording buffer reached {}% full. The disk may be falling behind; check disk throughput.", self.peak_percent))
        } else {
            None
        }
    }
}

/// Single-producer single-consumer FIFO of samples (input callback → writer thread). The slots
/// are atomics, so it needs no lock and no `unsafe`.
struct CaptureRing {
    slots: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    /// Maximum queued sample slots this arm, updated by its sole realtime producer.
    peak_samples: AtomicUsize,
}

impl CaptureRing {
    fn new(capacity: usize) -> CaptureRing {
        let n = capacity.max(2).next_power_of_two();
        CaptureRing {
            slots: (0..n).map(|_| AtomicU32::new(0)).collect(),
            mask: n - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            peak_samples: AtomicUsize::new(0),
        }
    }

    /// Queue up to `n` samples (`sample(i)` is the i-th); returns how many fit.
    fn push(&self, n: usize, sample: impl Fn(usize) -> f32) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let queued = head.wrapping_sub(self.tail.load(Ordering::Acquire)).min(self.slots.len());
        let n = n.min(self.slots.len().saturating_sub(queued));
        for i in 0..n {
            if let Some(slot) = self.slots.get(head.wrapping_add(i) & self.mask) {
                slot.store(sample(i).to_bits(), Ordering::Relaxed);
            }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        // One atomic max per producer write, never an allocation or mutex. A concurrent drain
        // can make this a conservative peak but never greater than the ring's actual capacity.
        self.peak_samples.fetch_max(queued + n, Ordering::Relaxed);
        n
    }

    fn peak_percent(&self) -> u8 {
        let capacity = self.slots.len().max(1) as u128;
        let peak = self.peak_samples.load(Ordering::Relaxed).min(self.slots.len()) as u128;
        (peak.saturating_mul(100) / capacity).min(100) as u8
    }

    fn pop_all(&self, out: &mut Vec<f32>) {
        let (tail, head) = (self.tail.load(Ordering::Relaxed), self.head.load(Ordering::Acquire));
        out.extend(
            (0..head.wrapping_sub(tail))
                .map(|i| self.slots.get(tail.wrapping_add(i) & self.mask).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)))),
        );
        self.tail.store(head, Ordering::Release);
    }
}

/// An input sample, or silence for NaN and infinities, which would poison the take and the mix.
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

struct Shared {
    armed: AtomicBool,
    /// Actual input-device channel count; each callback chunk begins on a complete frame.
    capture_channels: usize,
    /// Every arming transition advances this generation. The input callback may never see the
    /// brief disarmed interval between adjacent takes, so its pending silence must use an epoch.
    arm_epoch: AtomicU64,
    /// Callbacks that have committed to capturing a block. Stop must wait for them before the
    /// writer's final ring drain, or their last samples could arrive after the WAV is finalized.
    capturing: AtomicUsize,
    ring: CaptureRing,
    /// Samples lost while the ring was full.
    dropped: AtomicUsize,
    /// Physical input frames that had at least one sample dropped; partial frames count too.
    affected_frames: AtomicU64,
    /// Input callbacks so far, to tell a device that stopped sending.
    beats: AtomicU64,
    /// Approximate single-input-stream clock rate relative to its configured rate.
    input_rate: crate::clock::RateTelemetry,
    /// Set from the input stream when it can't go on: `GONE` or `BROKEN`.
    fault: AtomicU8,
    /// The first failure writing a take, until it's reported.
    failed: Mutex<Option<String>>,
    /// Tests can observe the writer reaching the stop path without timing sleeps.
    #[cfg(test)]
    writer_saw_stop: AtomicBool,
}

/// Drop is run during a callback unwind too, so a guarded panic cannot strand the writer.
struct CaptureInFlight<'a>(&'a AtomicUsize);

impl Drop for CaptureInFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    #[cfg(test)]
    fn new(ring: CaptureRing) -> Shared {
        Self::with_channels(ring, 1)
    }

    fn with_channels(ring: CaptureRing, channels: usize) -> Shared {
        let (beats, fault, failed) = (AtomicU64::new(0), AtomicU8::new(0), Mutex::new(None));
        Shared {
            armed: AtomicBool::new(false),
            capture_channels: channels.max(1),
            arm_epoch: AtomicU64::new(0),
            capturing: AtomicUsize::new(0),
            ring,
            dropped: AtomicUsize::new(0),
            affected_frames: AtomicU64::new(0),
            beats,
            input_rate: crate::clock::RateTelemetry::default(),
            fault,
            failed,
            #[cfg(test)]
            writer_saw_stop: AtomicBool::new(false),
        }
    }

    /// Run the input callback's `work`. After a panic in it, the input is broken and does
    /// nothing more.
    fn guarded(&self, work: impl FnOnce()) {
        self.beats.fetch_add(1, Ordering::Relaxed);
        if self.fault.load(Ordering::Relaxed) != BROKEN && std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
            self.fault.store(BROKEN, Ordering::Relaxed);
        }
    }

    /// The input callback's work while armed. Samples that don't fit are owed (`owed`) and queued
    /// as silence before the next block, so the take keeps its timing.
    fn capture(&self, data: &[f32], owed: &mut usize) {
        *owed -= self.ring.push(*owed, |_| 0.0);
        let kept = if *owed == 0 { self.ring.push(data.len(), |i| data.get(i).copied().map_or(0.0, finite)) } else { 0 };
        *owed = owed.saturating_add(data.len() - kept);
        self.dropped.fetch_add(data.len() - kept, Ordering::Relaxed);
        // `CaptureRing` has power-of-two slots, which need not divide 3/5/6/... channels. Its
        // last accepted samples can therefore be a *partial* frame. Count that affected frame,
        // not just dropped_samples / channels (which underreports even one damaged stereo frame).
        let frames = data.len().div_ceil(self.capture_channels);
        let intact = kept / self.capture_channels;
        // A short/malformed backend block that was accepted in full has lost *nothing*; the
        // final partial frame is only affected if a sample actually failed to enter the ring.
        let affected = if kept == data.len() { 0 } else { frames.saturating_sub(intact) };
        self.affected_frames.fetch_add(u64::try_from(affected).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Carry overflow silence only within one take. Stop and re-arm can both happen between two
    /// CPAL callbacks, so checking `armed` alone cannot reset the callback's persistent `owed`.
    fn capture_for_arm(&self, data: &[f32], owed: &mut usize, seen_epoch: &mut u64) {
        let epoch = self.arm_epoch.load(Ordering::SeqCst);
        if *seen_epoch != epoch {
            *owed = 0;
            *seen_epoch = epoch;
        }
        self.capture(data, owed);
    }

    /// Publish a new generation before allowing the callback to capture into its take. Atomic
    /// `fetch_add` wraps without panicking; every successive arming transition has a new value.
    fn begin_arm(&self) {
        self.ring.peak_samples.store(0, Ordering::Relaxed);
        self.affected_frames.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.arm_epoch.fetch_add(1, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Register before the final armed check: a callback accepted before stop remains visible to
    /// the writer until its ring push completes. SeqCst orders disarm, registration and the
    /// writer's quiescence check even if the two threads race at the stop boundary.
    fn capture_if_armed(&self, work: impl FnOnce()) -> bool {
        self.capturing.fetch_add(1, Ordering::SeqCst);
        let _in_flight = CaptureInFlight(&self.capturing);
        if !self.armed.load(Ordering::SeqCst) {
            return false;
        }
        work();
        true
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    /// Live input for monitoring (always fed while the input stream runs).
    pub monitor: Arc<InputRing>,
    #[cfg(not(target_os = "freebsd"))]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: usize,
    /// The take writer, and what each track's take holds so far.
    writer: Option<(std::thread::JoinHandle<Takes>, Vec<(TrackId, Arc<Mutex<LiveTake>>)>)>,
    /// The callback count last seen, and since when.
    seen: (u64, Instant),
}

/// The input stream: device samples are converted to f32 in a scratch buffer allocated here (so
/// the callback never allocates), then feed the monitor ring and, while armed, the capture.
#[cfg(not(target_os = "freebsd"))]
fn build_input<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: &Arc<Shared>,
    monitor: &Arc<InputRing>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    use cpal::traits::DeviceTrait;
    let (sh, errors) = (Arc::clone(shared), Arc::clone(shared));
    let mon = Arc::clone(monitor);
    let channels = usize::from(config.channels).max(1);
    let mut rate_probe = crate::clock::StreamRateProbe::new(config.sample_rate);
    let mut scratch = vec![0.0f32; 8192 * usize::from(config.channels).max(1)];
    let chunk = scratch.len();
    let mut owed = 0;
    let mut seen_epoch = 0;
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            crate::mark_audio_thread();
            sh.guarded(|| {
                // Capture timestamps belong only to this input stream. The output stream's
                // timestamp origin may differ; this observation cannot measure duplex latency.
                sh.input_rate.publish(rate_probe.observe(info.timestamp().capture.as_nanos(), data.len() / channels));
                for part in data.chunks(chunk) {
                    let Some(s) = scratch.get_mut(..part.len()) else { return };
                    for (o, x) in s.iter_mut().zip(part) {
                        *o = f32::from_sample(*x);
                    }
                    mon.push(s);
                    if sh.armed.load(Ordering::SeqCst) {
                        if !sh.capture_if_armed(|| sh.capture_for_arm(s, &mut owed, &mut seen_epoch)) {
                            owed = 0;
                        }
                    } else {
                        owed = 0;
                    }
                }
            });
        },
        move |e: cpal::Error| {
            // Possibly the audio thread (see `crate::mark_audio_thread`).
            crate::mark_audio_thread();
            // cpal 0.18 reports every operation through one error type; a device that went away
            // or a stream that must be rebuilt (`StreamInvalidated`) stops capture.
            // `DeviceChanged` (a rerouted default device) and an xrun are not faults.
            if crate::devices::stream_lost(&e) {
                let _ = errors.fault.compare_exchange(0, GONE, Ordering::Relaxed, Ordering::Relaxed);
            }
            log::warn!("input stream error: {e}");
        },
        None,
    )
}

/// The input configuration at `rate` when the device offers one: the best sample format first,
/// then the most channels (so a stereo interface is preferred over a mono one). `None` lets the
/// caller use the device's default input configuration (the mix resamples when the rates differ).
fn pick_input_config(configs: &[cpal::SupportedStreamConfigRange], rate: u32) -> Option<cpal::SupportedStreamConfig> {
    configs
        .iter()
        .filter_map(|c| {
            let rank = crate::devices::format_rank(c.sample_format())?;
            if !c.contains_rate(rate) {
                return None;
            }
            c.try_with_sample_rate(rate).map(|config| (rank, std::cmp::Reverse(c.channels()), config))
        })
        .min_by_key(|(rank, channels, _)| (*rank, *channels))
        .map(|(_, _, config)| config)
}

/// Try the requested device buffer first, then the device default. A stream can build
/// successfully but fail to start; that failure must also release the stream and retry.
/// When no fixed buffer was requested, the default is attempted only once.
fn first_started_input<S, E: std::fmt::Display>(
    preferred: cpal::StreamConfig,
    build: impl FnMut(&cpal::StreamConfig) -> Result<S, E>,
    start: impl Fn(&S) -> bool,
) -> Option<(cpal::StreamConfig, S)> {
    let mut attempts = vec![preferred];
    if preferred.buffer_size != cpal::BufferSize::Default {
        attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..preferred });
    }
    crate::first_started(attempts, build, start)
}

impl Recorder {
    /// Open the default input device. Errors when there is none (the caller shows a message).
    pub fn open() -> Result<Recorder, String> {
        Recorder::open_with(&crate::AudioConfig::default())
    }

    /// Diagnostic-only effective input rate error in parts per million from CPAL capture
    /// timestamps. `None` means not enough reliable observations yet. This is not a device
    /// latency measurement and cannot establish that the output uses the same clock.
    pub fn input_rate_error_ppm(&self) -> Option<i32> {
        self.shared.input_rate.read()
    }

    /// Open the input device `audio` chooses (else the default) at its chosen rate and buffer
    /// size when the device offers them, else at the device's defaults. Integer devices (ASIO
    /// drivers usually are) are converted to f32.
    #[cfg(not(target_os = "freebsd"))]
    pub fn open_with(audio: &crate::AudioConfig) -> Result<Recorder, String> {
        use cpal::traits::DeviceTrait;
        let host = crate::devices::host(audio);
        let device = crate::devices::device(&host, audio.input.as_deref(), false).ok_or("no audio input device")?;
        let name = crate::devices::device_name(&device).unwrap_or_else(|| "Input".into());
        // At the chosen rate when the device offers it, so input and output run together: the
        // best format first, then the most channels.
        let configs: Vec<cpal::SupportedStreamConfigRange> = device.supported_input_configs().map(|configs| configs.collect()).unwrap_or_default();
        let at_rate = audio.sample_rate.and_then(|r| pick_input_config(&configs, r));
        let cfg = match at_rate {
            Some(c) => c,
            None => {
                let d = device.default_input_config().map_err(|e| e.to_string())?;
                // cpal 0.18's default can name a format these callbacks do not decode (I24, F64);
                // open the same rate and channels in a format they do.
                if crate::devices::format_rank(d.sample_format()).is_some() {
                    d
                } else {
                    crate::devices::supported_format_config(&configs, d.sample_rate(), d.channels())
                        .ok_or_else(|| format!("unsupported input sample format {:?}", d.sample_format()))?
                }
            }
        };
        let format = cfg.sample_format();
        if crate::devices::format_rank(format).is_none() {
            return Err(format!("unsupported input sample format {format:?}"));
        }
        let mut config = cfg.config();
        config.buffer_size = crate::devices::buffer_size(audio.buffer_size, cfg.buffer_size());
        let channels = usize::from(config.channels).max(1);
        let ring = CaptureRing::new((config.sample_rate as usize).saturating_mul(channels * RING_SECONDS).min(1 << 25));
        let shared = Arc::new(Shared::with_channels(ring, channels));
        let monitor = InputRing::new(usize::from(config.channels), config.sample_rate);
        let build = |config: cpal::StreamConfig| match format {
            cpal::SampleFormat::I32 => build_input::<i32>(&device, config, &shared, &monitor),
            cpal::SampleFormat::I16 => build_input::<i16>(&device, config, &shared, &monitor),
            _ => build_input::<f32>(&device, config, &shared, &monitor),
        };
        // A device may accept a fixed buffer during construction but reject it when the
        // stream starts. Try the selected low-latency size first, then a fresh default
        // stream after either kind of failure.
        let (config, stream) = first_started_input(config, |attempt| build(*attempt), crate::start_stream)
            .ok_or_else(|| format!("cannot open or start audio input device \"{name}\""))?;
        let seen = (0, Instant::now());
        Ok(Recorder { monitor, shared, _stream: Some(stream), device_name: name, sample_rate: config.sample_rate, channels, writer: None, seen })
    }

    #[cfg(target_os = "freebsd")]
    pub fn open_with(_audio: &crate::AudioConfig) -> Result<Recorder, String> {
        Err("recording is not supported on this platform yet".into())
    }

    /// Create each take's file and start recording into them: a track's take records the given
    /// input channels into a new file.
    pub fn arm(&mut self, takes: Vec<(TrackId, Vec<usize>, PathBuf)>) -> Result<(), String> {
        self.arm_with_spawn(takes, |shared, takes, channels| {
            std::thread::Builder::new().name("take writer".into()).spawn(move || write_takes(&shared, takes, channels))
        })
    }

    /// Separate thread creation so tests can inject a deterministic spawn failure after all
    /// take files have been created. A failed arm must leave no new WAVs behind.
    fn arm_with_spawn(
        &mut self,
        takes: Vec<(TrackId, Vec<usize>, PathBuf)>,
        spawn: impl FnOnce(Arc<Shared>, Vec<TakeWriter>, usize) -> std::io::Result<std::thread::JoinHandle<Takes>>,
    ) -> Result<(), String> {
        if self.writer.is_some() {
            return Err("already recording".into());
        }
        // Preflight every track before opening the first file. A stale input route from
        // another interface must never wrap to input 1 or leave half-created take files.
        for (_, inputs, path) in &takes {
            if inputs.is_empty() {
                return Err(format!("{}: no recording input channels selected", path.display()));
            }
            if let Some(channel) = inputs.iter().copied().find(|channel| *channel >= self.channels) {
                return Err(format!(
                    "{}: hardware input {} is unavailable on “{}” ({} input channels)",
                    path.display(),
                    channel.saturating_add(1),
                    self.device_name,
                    self.channels
                ));
            }
        }
        let mut created = Vec::new();
        for (id, inputs, path) in takes {
            match TakeWriter::create(id, inputs, path, self.sample_rate) {
                Ok(take) => created.push(take),
                Err(error) => {
                    // Only these handles came from successful create_new calls in THIS arm.
                    // Close them before unlinking, including on Windows.
                    let paths = created.iter().map(|t: &TakeWriter| t.path.clone()).collect::<Vec<_>>();
                    drop(created);
                    return Err(cleanup_new_takes(error, paths));
                }
            }
        }
        let paths = created.iter().map(|t| t.path.clone()).collect::<Vec<_>>();
        let takes = created;
        let live = takes.iter().map(|t| (t.id, Arc::clone(&t.live))).collect();
        let ring = &self.shared.ring;
        ring.tail.store(ring.head.load(Ordering::Acquire), Ordering::Release);
        *self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.shared.begin_arm();
        match spawn(Arc::clone(&self.shared), takes, self.channels) {
            Ok(w) => self.writer = Some((w, live)),
            Err(e) => {
                self.shared.armed.store(false, Ordering::SeqCst);
                return Err(cleanup_new_takes(e.to_string(), paths));
            }
        }
        Ok(())
    }

    /// What the take recording onto `track` holds so far.
    pub fn live_take(&self, track: TrackId) -> Option<MutexGuard<'_, LiveTake>> {
        let (_, take) = self.writer.as_ref()?.1.iter().find(|(id, _)| *id == track)?;
        Some(take.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Why the input can't go on, if it can't: the device went away or, while recording, stopped
    /// sending audio; its callback hit a bug; or writing a take failed (reported once) or stopped
    /// unexpectedly.
    pub fn trouble(&mut self) -> Option<Trouble> {
        if let Some(e) = self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(Trouble::Disk(e));
        }
        // The writer normally runs until `finish` disarms capture. An early exit means
        // the take files are no longer receiving audio, even if the input device is healthy.
        if self.shared.armed.load(Ordering::Acquire) && self.writer.as_ref().is_some_and(|(writer, _)| writer.is_finished()) {
            return Some(Trouble::Disk("the audio take writer stopped unexpectedly; check the files on disk".into()));
        }
        let name = &self.device_name;
        match self.shared.fault.load(Ordering::Relaxed) {
            GONE => return Some(Trouble::Device(format!("“{name}” is unavailable"))),
            BROKEN => return Some(Trouble::Device("the audio input hit an internal error (see the log)".into())),
            _ => {}
        }
        let beats = self.shared.beats.load(Ordering::Relaxed);
        if beats != self.seen.0 || !self.shared.armed.load(Ordering::Relaxed) {
            self.seen = (beats, Instant::now());
        }
        (self.seen.1.elapsed() >= STALL).then(|| Trouble::Device(format!("“{name}” stopped sending audio")))
    }

    /// An allocation-free atomic snapshot of this take's FIFO pressure and damaged input frames.
    /// Call `warning` from the UI thread for actionable, correctly rounded diagnostics.
    pub fn capture_pressure(&self) -> CapturePressure {
        CapturePressure { peak_percent: self.shared.ring.peak_percent(), affected_frames: self.shared.affected_frames.load(Ordering::Relaxed) }
    }

    /// Approximate duration of affected input frames in whole milliseconds (truncates below 1 ms).
    /// Use `capture_pressure().affected_frames` to report even sub-millisecond dropouts precisely.
    pub fn lost_ms(&self) -> u64 {
        self.capture_pressure().affected_frames.saturating_mul(1000) / u64::from(self.sample_rate.max(1))
    }

    pub fn finish(&mut self) -> Takes {
        self.shared.armed.store(false, Ordering::SeqCst);
        let (takes, problem) = match self.writer.take() {
            Some((writer, _)) => match writer.join() {
                Ok(result) => result,
                Err(_) => (Vec::new(), Some("the audio take writer panicked; check the files on disk for recoverable audio".into())),
            },
            None => (Vec::new(), None),
        };
        let pressure = self.capture_pressure();
        self.shared.affected_frames.store(0, Ordering::Relaxed);
        self.shared.dropped.store(0, Ordering::Relaxed);
        let slow = if pressure.affected_frames > 0 { pressure.warning(self.sample_rate) } else { None };
        let problem = match (problem, slow) {
            (Some(mut failure), Some(loss)) => {
                failure.push_str("; ");
                failure.push_str(&loss);
                Some(failure)
            }
            (Some(failure), None) => Some(failure),
            (None, Some(loss)) => Some(loss),
            (None, None) => None,
        };
        (takes, problem)
    }
}

/// Only paths returned by successful `File::create_new` calls are eligible for rollback. An
/// existing take or arbitrary user file that made creation fail is never included. Report any
/// cleanup failure instead of silently leaving an orphan WAV to be mistaken for a real take.
fn cleanup_new_takes(mut error: String, paths: Vec<PathBuf>) -> String {
    for path in paths {
        if let Err(why) = std::fs::remove_file(&path) {
            error.push_str(&format!("; could not remove newly-created {}: {why}", path.display()));
        }
    }
    error
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.finish();
    }
}

const RECOVERY_PATH_PREVIEW_BYTES: usize = 160;

/// A bounded, escaped preview for the UI status and log. The path may contain newlines or an
/// unexpectedly long name; formatting directly into a capped sink avoids even a large temporary
/// allocation on the writer thread. It does not reopen, canonicalize, or change the file path.
fn recovery_path_preview(path: &Path) -> String {
    struct Capped {
        text: String,
        truncated: bool,
    }
    impl std::fmt::Write for Capped {
        fn write_str(&mut self, part: &str) -> std::fmt::Result {
            let mut end = RECOVERY_PATH_PREVIEW_BYTES.saturating_sub(self.text.len()).min(part.len());
            while !part.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&part[..end]);
            if end < part.len() {
                self.truncated = true;
                return Err(std::fmt::Error);
            }
            Ok(())
        }
    }
    let mut capped = Capped { text: String::with_capacity(RECOVERY_PATH_PREVIEW_BYTES + 24), truncated: false };
    let _ = std::fmt::write(&mut capped, format_args!("{path:?}"));
    if capped.truncated {
        capped.text.push_str("… [path abbreviated]");
    }
    capped.text
}

/// Multiple failed first chunks may leave *several* WAVs for manual recovery. The first disk
/// error is already returned, but it contains only its own path: add enough bounded context to
/// find the other files without constructing an unbounded status message.
#[derive(Default)]
struct RecoveryFiles {
    count: usize,
    examples: Vec<String>,
}

impl RecoveryFiles {
    fn add(&mut self, path: &Path) {
        self.count = self.count.saturating_add(1);
        if self.examples.len() < 3 {
            self.examples.push(recovery_path_preview(path));
        }
    }

    fn append_to(self, problem: &mut Option<String>) {
        if self.count == 0 {
            return;
        }
        let others = self.count.saturating_sub(self.examples.len());
        let mut notice = format!(
            "{} failed WAV file(s) were left on disk for possible manual recovery (not added to the session): {}",
            self.count,
            self.examples.join(", ")
        );
        if others > 0 {
            notice.push_str(&format!("; {others} more path(s) omitted — inspect the recording folders"));
        }
        match problem {
            Some(first_error) => {
                first_error.push_str("; ");
                first_error.push_str(&notice);
            }
            None => *problem = Some(notice),
        }
    }
}

fn write_takes(shared: &Shared, mut takes: Vec<TakeWriter>, channels: usize) -> Takes {
    let (mut block, mut header_written, mut reported) = (Vec::new(), Instant::now(), false);
    loop {
        let stopping = !shared.armed.load(Ordering::SeqCst);
        // Snapshot this BEFORE popping: even if an active callback finishes during the pop,
        // perform one more drain to include every sample it published.
        let captures_finished = shared.capturing.load(Ordering::SeqCst) == 0;
        #[cfg(test)]
        if stopping {
            shared.writer_saw_stop.store(true, Ordering::Release);
        }
        // The ring holds interleaved samples and a pop can end mid-frame: write whole frames and
        // keep the remainder for the next pop, so channels never shift.
        shared.ring.pop_all(&mut block);
        let whole = block.len() - block.len() % channels.max(1);
        takes.iter_mut().for_each(|t| t.write(&block[..whole], channels));
        block.drain(..whole);
        if !reported && let Some(e) = takes.iter().find_map(|t| t.error.clone()) {
            *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
            reported = true;
        }
        if stopping && captures_finished {
            break;
        }
        if header_written.elapsed() >= Duration::from_secs(1) {
            header_written = Instant::now();
            takes.iter_mut().for_each(TakeWriter::write_header);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (mut done, mut problem) = (Vec::new(), None);
    let mut recoverable = RecoveryFiles::default();
    for mut t in takes {
        t.write_header();
        let TakeWriter { id, path, file, frames, error, .. } = t;
        // A failed first write_all may leave recoverable complete samples on disk even though no
        // full 64-KiB chunk was committed to `frames`. Never delete that evidence or import it as
        // a successful take. Only a genuinely empty *successful* take is disposable.
        // Close the file before unlinking an empty take (required on Windows).
        drop(file);
        if frames > 0 {
            done.push((id, path));
        } else if error.is_none() {
            let _ = std::fs::remove_file(&path);
        } else {
            recoverable.add(&path);
        }
        problem = problem.or(error);
    }
    recoverable.append_to(&mut problem);
    (done, problem)
}

/// Encode full interleaved frames and drain them with `write_all`. Return the number of *whole*
/// frames committed before the first I/O error: a later failing chunk must not hide previously
/// written audio from a take's WAV header. `scratch` is reused, never grown beyond 64 KiB.
fn write_interleaved_chunks(
    sink: &mut impl Write,
    scratch: &mut Vec<u8>,
    block: &[f32],
    channels: usize,
    inputs: &[usize],
) -> (usize, Option<std::io::Error>) {
    let Some(frame_bytes) = inputs.len().checked_mul(std::mem::size_of::<f32>()).filter(|&n| n > 0 && n <= TAKE_WRITE_CHUNK_BYTES) else {
        return (0, Some(std::io::Error::new(std::io::ErrorKind::InvalidInput, "too many recording channels for the WAV write buffer")));
    };
    let frames_per_chunk = TAKE_WRITE_CHUNK_BYTES / frame_bytes;
    let mut committed = 0;
    let mut pending = 0;
    scratch.clear();
    for frame in block.chunks_exact(channels.max(1)) {
        for &input in inputs {
            scratch.extend_from_slice(&frame.get(input).copied().unwrap_or(0.0).to_le_bytes());
        }
        pending += 1;
        if pending == frames_per_chunk {
            if let Err(e) = sink.write_all(scratch) {
                return (committed, Some(e));
            }
            committed += pending;
            pending = 0;
            scratch.clear();
        }
    }
    if pending > 0 {
        if let Err(e) = sink.write_all(scratch) {
            return (committed, Some(e));
        }
        committed += pending;
    }
    (committed, None)
}

/// One take's file. After an error it stops writing; its header keeps covering what was written.
struct TakeWriter {
    id: TrackId,
    inputs: Vec<usize>,
    path: PathBuf,
    file: File,
    rate: u32,
    frames: u64,
    error: Option<String>,
    live: Arc<Mutex<LiveTake>>,
    /// Writer-thread-only reusable buffer; the audio callback never touches it.
    scratch: Vec<u8>,
}

impl TakeWriter {
    fn create(id: TrackId, inputs: Vec<usize>, path: PathBuf, rate: u32) -> Result<TakeWriter, String> {
        // Reserve before creating the take so allocation failure cannot orphan a new WAV.
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(TAKE_WRITE_CHUNK_BYTES).map_err(|e| format!("{}: cannot reserve WAV write buffer: {e}", path.display()))?;
        let file = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| File::create_new(&path));
        let file = file.map_err(|e| format!("{}: {e}", path.display()))?;
        let live = Arc::new(Mutex::new(vec![Vec::new(); inputs.len()]));
        let mut take = TakeWriter { id, inputs, path, file, rate, frames: 0, error: None, live, scratch };
        take.write_header();
        if let Some(error) = take.error.take() {
            let path = take.path.clone();
            drop(take);
            return Err(cleanup_new_takes(error, vec![path]));
        }
        Ok(take)
    }

    /// Append this take's channels of `block` (interleaved, `channels` wide).
    fn write(&mut self, block: &[f32], channels: usize) {
        let channels = channels.max(1);
        if self.error.is_some() {
            return;
        }
        let (committed, error) = write_interleaved_chunks(&mut self.file, &mut self.scratch, block, channels, &self.inputs);
        // A late disk error may follow several fully written chunks. Keep those frames playable
        // and visible in live-take summaries instead of discarding their count.
        if committed > 0 {
            self.summarize(&block[..committed * channels], channels);
            self.frames = self.frames.saturating_add(committed as u64);
        }
        if let Some(e) = error {
            self.fail(e);
        }
    }

    /// Add this take's channels of `block` to its live take.
    fn summarize(&self, block: &[f32], channels: usize) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        for (f, n) in block.chunks_exact(channels).zip(self.frames..) {
            let at = (n / LIVE_BLOCK as u64) as usize;
            for (blocks, &i) in live.iter_mut().zip(&self.inputs) {
                let v = f.get(i).copied().unwrap_or(0.0);
                match blocks.get_mut(at) {
                    Some(b) => *b = (b.0.min(v), b.1.max(v)),
                    None if at < LIVE_BLOCKS => blocks.push((v, v)),
                    None => {}
                }
            }
        }
    }

    fn write_header(&mut self) {
        let header = match soundcraft_audio_io::wav_float_header(self.inputs.len(), self.rate, self.frames) {
            Ok(h) => h,
            Err(e) => return self.fail(e),
        };
        let f = &mut self.file;
        if let Err(e) = f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&header)).and_then(|()| f.seek(SeekFrom::End(0))) {
            self.fail(e);
        }
    }

    fn fail(&mut self, e: impl std::fmt::Display) {
        self.error.get_or_insert_with(|| format!("{}: {e}", self.path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(path: &PathBuf) -> Vec<Vec<f32>> {
        soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), None).unwrap().1.channels
    }

    #[test]
    fn a_full_ring_owes_the_lost_samples_as_silence() {
        let shared = Shared::new(CaptureRing::new(8));
        shared.begin_arm();
        let (mut owed, mut seen_epoch, mut out) = (0, 0, Vec::new());
        shared.capture_for_arm(&[1.0; 6], &mut owed, &mut seen_epoch);
        shared.capture_for_arm(&[2.0; 4], &mut owed, &mut seen_epoch);
        shared.ring.pop_all(&mut out);
        shared.capture_for_arm(&[3.0; 2], &mut owed, &mut seen_epoch);
        shared.ring.pop_all(&mut out);
        assert_eq!(out, [vec![1.0; 6], vec![2.0; 2], vec![0.0; 2], vec![3.0; 2]].concat());
        assert_eq!((owed, shared.dropped.load(Ordering::Relaxed)), (0, 2));
        assert_eq!(seen_epoch, shared.arm_epoch.load(Ordering::SeqCst));
    }

    /// A full capture ring leaves callback-local owed silence. Stop and re-arm may finish before
    /// the next input device callback, so that callback never sees `armed == false` to reset it.
    /// The next take must start with only its own samples, not silence owed by the prior take.
    #[test]
    fn rapid_stop_then_rearm_does_not_copy_overflow_silence_into_the_next_take() {
        let shared = Shared::new(CaptureRing::new(8));
        let (mut owed, mut seen_epoch) = (0, 0);
        shared.begin_arm();
        shared.capture_for_arm(&[0.25; 8], &mut owed, &mut seen_epoch);
        shared.capture_for_arm(&[0.5; 6], &mut owed, &mut seen_epoch);
        assert_eq!(owed, 6, "the first take owes six dropped samples");
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 6);

        shared.armed.store(false, Ordering::SeqCst);
        let mut first_take = Vec::new();
        shared.ring.pop_all(&mut first_take);
        assert_eq!(first_take, [0.25; 8]);
        // No input callback ran while disarmed: `owed` is still six here.
        shared.begin_arm();
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0, "loss telemetry must start fresh with each take");
        shared.capture_for_arm(&[0.75; 4], &mut owed, &mut seen_epoch);
        let mut second_take = Vec::new();
        shared.ring.pop_all(&mut second_take);
        assert_eq!(second_take, [0.75; 4]);
        assert_eq!(owed, 0);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0, "the fresh take must not lose samples to prior debt");
    }

    /// An eight-slot ring cannot hold whole three-channel frames at its capacity boundary.
    /// When two samples of the next frame are accepted and one is lost, that *frame* is damaged,
    /// as is the following wholly dropped frame. The old `dropped_samples / channels` rounded
    /// four lost samples down to one frame. Peak occupancy remains available after a drain.
    #[test]
    fn capture_pressure_counts_partial_three_channel_frames_and_resets_each_take() {
        let shared = Arc::new(Shared::with_channels(CaptureRing::new(8), 3));
        let slots = shared.ring.slots.as_ptr(); // callback must reuse preallocated ring storage
        shared.begin_arm();
        let mut owed = 0;
        shared.capture(&[0.1; 6], &mut owed);
        let mut r = recorder(&shared);
        r.channels = 3;
        assert_eq!(r.capture_pressure(), CapturePressure { peak_percent: 75, affected_frames: 0 });
        shared.capture(&[0.2; 6], &mut owed); // two samples kept, four discarded
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 4);
        assert_eq!(r.capture_pressure(), CapturePressure { peak_percent: 100, affected_frames: 2 });
        assert_eq!(r.lost_ms(), 0, "two damaged frames at 48 kHz are shorter than 1 ms");
        assert_eq!(shared.ring.slots.as_ptr(), slots, "producer counters must not grow ring storage");

        let mut drained = Vec::new();
        shared.ring.pop_all(&mut drained);
        assert_eq!(r.capture_pressure().peak_percent, 100, "high-water must survive a completed drain");
        let (_, problem) = r.finish();
        let problem = problem.expect("even a sub-millisecond loss must be reported");
        assert!(problem.contains("2 input frame(s)") && problem.contains("less than 1 ms"), "{problem}");
        assert!(!problem.contains("0 ms"), "never report a real dropout as zero duration: {problem}");

        shared.begin_arm();
        assert_eq!(r.capture_pressure(), CapturePressure::default());
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capture_pressure_counts_a_single_partially_dropped_stereo_frame() {
        let shared = Shared::with_channels(CaptureRing::new(4), 2);
        shared.begin_arm();
        // Model an incomplete queued frame at a callback boundary (also exercises defensive
        // handling of malformed backend blocks). One slot remains for the following L/R pair.
        assert_eq!(shared.ring.push(3, |_| 0.25), 3);
        let mut owed = 0;
        shared.capture(&[0.5, -0.5], &mut owed);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(shared.affected_frames.load(Ordering::Relaxed), 1);
        assert_eq!(shared.ring.peak_percent(), 100);
        assert_eq!(CapturePressure { peak_percent: 79, affected_frames: 0 }.warning(48_000), None);
        assert!(CapturePressure { peak_percent: 80, affected_frames: 0 }.warning(48_000).is_some());
    }

    #[test]
    fn short_backend_block_without_fifo_loss_does_not_report_a_dropout() {
        let shared = Shared::with_channels(CaptureRing::new(4), 3);
        shared.begin_arm();
        let mut owed = 0;
        shared.capture(&[0.25, -0.25], &mut owed);
        assert_eq!(owed, 0);
        assert_eq!(shared.affected_frames.load(Ordering::Relaxed), 0);
        assert_eq!(shared.ring.peak_percent(), 50);
    }

    /// The take writer's I/O failure must remain visible even if the input also overruns.
    /// Previously `problem.or(slow)` silently hid the second failure from the user.
    #[test]
    fn finish_reports_both_writer_failure_and_short_capture_dropout() {
        let shared = Arc::new(Shared::with_channels(CaptureRing::new(4), 2));
        shared.begin_arm();
        let mut owed = 0;
        shared.capture(&[0.25; 6], &mut owed); // one frame dropped
        let mut r = recorder(&shared);
        r.channels = 2;
        r.writer = Some((std::thread::spawn(|| (Vec::new(), Some("synthetic disk I/O failure".into()))), Vec::new()));

        let (takes, problem) = r.finish();
        assert!(takes.is_empty());
        let problem = problem.expect("the disk error and capture loss both need reporting");
        assert!(problem.contains("synthetic disk I/O failure"), "{problem}");
        assert!(problem.contains("1 input frame(s)") && problem.contains("less than 1 ms"), "{problem}");
        assert_eq!(r.capture_pressure().affected_frames, 0, "finished take pressure should be cleared");
    }

    #[test]
    fn arm_epoch_wrap_resets_owed_silence_without_panicking() {
        let shared = Shared::new(CaptureRing::new(8));
        shared.arm_epoch.store(u64::MAX, Ordering::SeqCst);
        let (mut owed, mut seen_epoch) = (5, u64::MAX);
        shared.begin_arm();
        assert_eq!(shared.arm_epoch.load(Ordering::SeqCst), 0);
        shared.capture_for_arm(&[0.875; 4], &mut owed, &mut seen_epoch);
        let mut second_take = Vec::new();
        shared.ring.pop_all(&mut second_take);
        assert_eq!(second_take, [0.875; 4]);
        assert_eq!((owed, seen_epoch), (0, 0));
    }

    #[test]
    fn nan_and_infinite_input_records_as_silence() {
        let shared = Shared::new(CaptureRing::new(8));
        let (mut owed, mut out) = (0, Vec::new());
        shared.capture(&[0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5], &mut owed);
        shared.ring.pop_all(&mut out);
        assert_eq!(out, [0.5, 0.0, 0.0, 0.0, -0.5]);
    }

    /// Build a cpal configuration range for tests with a 64..1024-frame buffer.
    fn range(channels: u16, min: u32, max: u32, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(channels, min, max, cpal::SupportedBufferSize::Range { min: 64, max: 1024 }, format)
    }

    /// At the chosen rate, input selection prefers the best supported format, then more channels; a rate the device
    /// does not offer returns `None` so the caller can fall back.
    #[test]
    fn input_config_prefers_the_best_format_then_more_channels_at_the_chosen_rate() {
        let configs = [
            range(1, 44_100, 192_000, cpal::SampleFormat::I32),
            range(2, 44_100, 96_000, cpal::SampleFormat::I16),
            range(2, 48_000, 96_000, cpal::SampleFormat::F32),
            range(8, 44_100, 48_000, cpal::SampleFormat::F32),
        ];
        let c = pick_input_config(&configs, 96_000).unwrap();
        assert_eq!((c.sample_rate(), c.channels(), c.sample_format()), (96_000, 2, cpal::SampleFormat::F32));
        let c = pick_input_config(&configs, 48_000).unwrap();
        assert_eq!((c.sample_rate(), c.channels(), c.sample_format()), (48_000, 8, cpal::SampleFormat::F32), "more channels win in the same format");
        assert!(pick_input_config(&configs, 192_001).is_none(), "an unsupported rate falls back to the device default");
    }

    #[test]
    fn failed_input_start_releases_stream_before_retrying_default_buffer() {
        use std::cell::Cell;

        struct MockStream<'a> {
            buffer: cpal::BufferSize,
            drops: &'a Cell<usize>,
        }
        impl Drop for MockStream<'_> {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }

        let preferred = cpal::StreamConfig { channels: 2, sample_rate: 48_000, buffer_size: cpal::BufferSize::Fixed(128) };
        let drops = Cell::new(0);
        let mut attempted = Vec::new();
        let (chosen, stream) = first_started_input(
            preferred,
            |cfg| {
                attempted.push(cfg.buffer_size);
                if cfg.buffer_size == cpal::BufferSize::Default {
                    assert_eq!(drops.get(), 1, "the failed fixed-buffer stream must be dropped first");
                }
                Ok::<_, &'static str>(MockStream { buffer: cfg.buffer_size, drops: &drops })
            },
            |stream| stream.buffer == cpal::BufferSize::Default,
        )
        .expect("the default-buffer stream starts");
        assert_eq!(attempted, [cpal::BufferSize::Fixed(128), cpal::BufferSize::Default]);
        assert_eq!(chosen.buffer_size, cpal::BufferSize::Default);
        assert_eq!(drops.get(), 1);
        drop(stream);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn failed_input_build_retries_default_buffer_but_default_is_not_duplicated() {
        let preferred = cpal::StreamConfig { channels: 2, sample_rate: 48_000, buffer_size: cpal::BufferSize::Fixed(128) };
        let mut attempted = Vec::new();
        let (chosen, _) = first_started_input(
            preferred,
            |cfg| {
                attempted.push(cfg.buffer_size);
                if cfg.buffer_size == cpal::BufferSize::Default { Ok(cpal::BufferSize::Default) } else { Err("fixed buffer rejected") }
            },
            |_| true,
        )
        .expect("the default-buffer stream starts");
        assert_eq!(chosen.buffer_size, cpal::BufferSize::Default);
        assert_eq!(attempted, [cpal::BufferSize::Fixed(128), cpal::BufferSize::Default]);

        let mut default_attempts = 0;
        let default = cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..preferred };
        let none = first_started_input(
            default,
            |_| {
                default_attempts += 1;
                Err::<(), _>("default buffer rejected")
            },
            |_| true,
        );
        assert!(none.is_none());
        assert_eq!(default_attempts, 1);
    }

    fn recorder(shared: &Arc<Shared>) -> Recorder {
        Recorder {
            shared: Arc::clone(shared),
            monitor: InputRing::new(1, 48_000),
            #[cfg(not(target_os = "freebsd"))]
            _stream: None,
            device_name: "Mic In".into(),
            sample_rate: 48_000,
            channels: 1,
            writer: None,
            seen: (0, Instant::now()),
        }
    }

    /// A stalled armed input, a panicking callback and a lost device all surface as `Trouble`; a quiet input that is
    /// not recording does not.
    #[test]
    fn a_bug_a_stall_or_a_lost_device_is_trouble() {
        let shared = Arc::new(Shared::new(CaptureRing::new(64)));
        let mut r = recorder(&shared);
        shared.armed.store(true, Ordering::Release);
        assert_eq!(r.trouble(), None);
        r.seen.1 = Instant::now().checked_sub(STALL).unwrap();
        assert_eq!(r.trouble(), Some(Trouble::Device("“Mic In” stopped sending audio".into())));
        shared.guarded(|| {});
        assert_eq!(r.trouble(), None, "it sent again");
        shared.armed.store(false, Ordering::Release);
        r.seen.1 = Instant::now().checked_sub(STALL).unwrap();
        assert_eq!(r.trouble(), None, "a quiet input that isn't recording is fine");
        shared.guarded(|| std::panic::panic_any("a bug in the input"));
        let mut ran = false;
        shared.guarded(|| ran = true);
        assert!(!ran);
        assert_eq!(r.trouble(), Some(Trouble::Device("the audio input hit an internal error (see the log)".into())));
        let gone = Arc::new(Shared::new(CaptureRing::new(64)));
        gone.fault.store(GONE, Ordering::Relaxed);
        assert_eq!(recorder(&gone).trouble(), Some(Trouble::Device("“Mic In” is unavailable".into())));
    }

    /// A writer that exits unexpectedly must not leave the recording UI claiming that capture
    /// is still succeeding, or return success when the user stops the take.
    #[test]
    fn a_panicked_writer_reports_trouble_and_a_failed_take() {
        let shared = Arc::new(Shared::new(CaptureRing::new(64)));
        let mut r = recorder(&shared);
        let writer = std::thread::spawn(|| -> Takes { std::panic::panic_any("synthetic take writer failure") });
        while !writer.is_finished() {
            std::thread::yield_now();
        }
        shared.armed.store(true, Ordering::Release);
        r.writer = Some((writer, Vec::new()));
        assert!(matches!(r.trouble(), Some(Trouble::Disk(e)) if e.contains("writer stopped unexpectedly")));

        let (takes, problem) = r.finish();
        assert!(takes.is_empty());
        assert!(problem.is_some_and(|e| e.contains("writer panicked") && e.contains("recoverable audio")));
        assert!(!shared.armed.load(Ordering::Acquire));
        assert!(r.writer.is_none());
    }

    /// Stop can race a callback that already accepted input but has not finished copying it.
    /// Wait for the writer to actually observe stop while the callback is blocked, then publish
    /// the samples: the final WAV must contain them, with no timing-sensitive sleeps in the test.
    #[test]
    fn stop_drains_an_inflight_callback_before_finalizing_its_wav() {
        let dir = std::env::temp_dir().join(format!("soundcraft-stop-inflight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Audio Files/Vox.wav");
        let shared = Arc::new(Shared::new(CaptureRing::new(1 << 12)));
        let mut r = recorder(&shared);
        r.arm(vec![(TrackId(1), vec![0], path.clone())]).unwrap();

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let callback_shared = Arc::clone(&shared);
        let callback = std::thread::spawn(move || {
            let mut owed = 0;
            callback_shared.guarded(|| {
                assert!(callback_shared.capture_if_armed(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    callback_shared.capture(&[0.375; 480], &mut owed);
                }));
            });
        });

        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(shared.capturing.load(Ordering::SeqCst), 1);
        shared.armed.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !shared.writer_saw_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(shared.writer_saw_stop.load(Ordering::Acquire), "writer did not reach the stop path");
        assert!(!r.writer.as_ref().unwrap().0.is_finished(), "the writer must wait for the accepted callback");

        release_tx.send(()).unwrap();
        callback.join().unwrap();
        let (takes, problem) = r.finish();
        assert_eq!(problem, None);
        assert_eq!(takes, vec![(TrackId(1), path.clone())]);
        assert_eq!(decode(&path), vec![vec![0.375; 480]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn panicking_and_disarmed_callbacks_cannot_strand_capture_inflight() {
        let shared = Shared::new(CaptureRing::new(64));
        let mut ran = false;
        assert!(!shared.capture_if_armed(|| ran = true));
        assert!(!ran);
        assert_eq!(shared.capturing.load(Ordering::SeqCst), 0);

        shared.armed.store(true, Ordering::SeqCst);
        shared.guarded(|| {
            shared.capture_if_armed(|| std::panic::panic_any("synthetic recording callback failure"));
        });
        assert_eq!(shared.capturing.load(Ordering::SeqCst), 0, "the guard drops when a callback unwinds");
        assert_eq!(shared.fault.load(Ordering::Relaxed), BROKEN);
    }

    #[test]
    fn a_failed_write_is_trouble_while_still_recording() {
        let dir = std::env::temp_dir().join(format!("soundcraft-failed-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut take = TakeWriter::create(TrackId(1), vec![0], dir.join("Vox_01.wav"), 48_000).unwrap();
        take.file = File::open(&take.path).unwrap();
        let shared = Arc::new(Shared::new(CaptureRing::new(1 << 12)));
        shared.armed.store(true, Ordering::Release);
        let mut r = recorder(&shared);
        let sh = Arc::clone(&shared);
        let writer = std::thread::spawn(move || write_takes(&sh, vec![take], 1));
        shared.capture(&[0.5; 480], &mut 0);
        let trouble = (0..300).find_map(|_| {
            std::thread::sleep(Duration::from_millis(10));
            r.trouble()
        });
        assert!(matches!(&trouble, Some(Trouble::Disk(e)) if e.contains("Vox_01.wav")), "{trouble:?}");
        assert_eq!(r.trouble(), None, "reported once");
        shared.armed.store(false, Ordering::Release);
        assert!(writer.join().unwrap().1.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_multitrack_route_fails_before_creating_any_take_file() {
        let dir = std::env::temp_dir().join(format!("soundcraft-input-bounds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Arc::new(Shared::new(CaptureRing::new(1024)));
        let mut r = recorder(&shared);
        r.channels = 2;
        let first = dir.join("Audio Files/Valid.wav");
        let second = dir.join("Audio Files/Invalid.wav");
        let error = r.arm(vec![(TrackId(1), vec![0], first.clone()), (TrackId(2), vec![2], second.clone())]).unwrap_err();
        assert!(error.contains("hardware input 3") && error.contains("2 input channels"), "{error}");
        assert!(!first.exists() && !second.exists(), "an invalid later track must not create the first take");
        assert!(!shared.armed.load(Ordering::Acquire));
        assert!(r.writer.is_none());
        assert!(r.arm(vec![(TrackId(3), Vec::new(), first.clone())]).is_err());
        assert!(!first.exists());
    }

    #[test]
    fn failed_second_take_creation_removes_only_the_first_new_file() {
        let dir = std::env::temp_dir().join(format!("soundcraft-failed-second-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = dir.join("Audio Files/Guitar.wav");
        let existing = dir.join("Audio Files/Vocal.wav");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        let sentinel = b"an existing take must never be overwritten or deleted";
        std::fs::write(&existing, sentinel).unwrap();

        let shared = Arc::new(Shared::new(CaptureRing::new(512)));
        let mut r = recorder(&shared);
        let error = r.arm(vec![(TrackId(1), vec![0], first.clone()), (TrackId(2), vec![0], existing.clone())]).unwrap_err();
        assert!(error.contains("Vocal.wav"), "{error}");
        assert!(!first.exists(), "the earlier new WAV must be rolled back when a later file cannot be opened");
        assert_eq!(std::fs::read(&existing).unwrap(), sentinel);
        assert!(!shared.armed.load(Ordering::SeqCst));
        assert!(r.writer.is_none());

        // A corrected second attempt can immediately reuse the same filename.
        r.arm(vec![(TrackId(1), vec![0], first.clone())]).unwrap();
        shared.capture(&[0.25; 8], &mut 0);
        let (finished, problem) = r.finish();
        assert_eq!((finished, problem), (vec![(TrackId(1), first.clone())], None));
        assert_eq!(decode(&first), vec![vec![0.25; 8]]);
        assert_eq!(std::fs::read(&existing).unwrap(), sentinel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn simulated_writer_spawn_failure_rolls_back_all_new_takes_not_preexisting_files() {
        let dir = std::env::temp_dir().join(format!("soundcraft-failed-writer-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = dir.join("Audio Files/Guitar.wav");
        let second = dir.join("Audio Files/Vocal.wav");
        let existing = dir.join("Audio Files/Earlier.wav");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        let sentinel = b"keep a real recording safe";
        std::fs::write(&existing, sentinel).unwrap();

        let shared = Arc::new(Shared::new(CaptureRing::new(512)));
        let mut r = recorder(&shared);
        let error = r
            .arm_with_spawn(vec![(TrackId(1), vec![0], first.clone()), (TrackId(2), vec![0], second.clone())], |_shared, takes, _channels| {
                assert_eq!(takes.len(), 2);
                assert!(first.exists() && second.exists(), "failure is injected after both WAV headers were created");
                drop(takes);
                Err(std::io::Error::other("synthetic thread spawn failure"))
            })
            .unwrap_err();
        assert!(error.contains("synthetic thread spawn failure"), "{error}");
        assert!(!first.exists() && !second.exists(), "neither brand-new file should remain after failed arm");
        assert_eq!(std::fs::read(&existing).unwrap(), sentinel);
        assert!(!shared.armed.load(Ordering::SeqCst));
        assert!(r.writer.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_new_wav_header_does_not_leave_an_empty_take() {
        let dir = std::env::temp_dir().join(format!("soundcraft-failed-take-header-{}", std::process::id()));
        let path = dir.join("Audio Files/Invalid.wav");
        let _ = std::fs::remove_dir_all(&dir);
        let error = TakeWriter::create(TrackId(1), vec![0], path.clone(), 0).err().unwrap();
        assert!(error.contains("sample rate"), "{error}");
        assert!(!path.exists(), "the new zero-length WAV is removed if header initialization fails");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multichannel_take_records_selected_physical_inputs_in_order() {
        let dir = std::env::temp_dir().join(format!("soundcraft-routed-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Arc::new(Shared::new(CaptureRing::new(1024)));
        let mut r = recorder(&shared);
        r.channels = 4;
        let path = dir.join("Audio Files/Room.wav");
        r.arm(vec![(TrackId(1), vec![3, 1], path.clone())]).unwrap();
        let input: Vec<f32> = (0..16).flat_map(|_| [0.1, 0.2, 0.3, 0.4]).collect();
        shared.capture(&input, &mut 0);
        let (takes, problem) = r.finish();
        assert_eq!((takes.len(), problem), (1, None));
        assert_eq!(decode(&path), vec![vec![0.4; 16], vec![0.2; 16]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_interleaved_take_preserves_routing_across_bounded_write_chunks() {
        let dir = std::env::temp_dir().join(format!("soundcraft-bounded-wav-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Audio Files/Room.wav");
        let mut take = TakeWriter::create(TrackId(1), vec![3, 1], path.clone(), 48_000).unwrap();
        let scratch = take.scratch.as_ptr();
        let frames = 100_007;
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let value = (i % 97) as f32 / 100.0;
                [value, -value, value / 2.0, 1.0 - value]
            })
            .collect();
        take.write(&input, 4);
        assert_eq!(take.frames, frames as u64);
        assert_eq!(take.error, None);
        assert_eq!(take.scratch.as_ptr(), scratch, "chunked writes reuse the same preallocated scratch");
        assert!(take.scratch.len() <= TAKE_WRITE_CHUNK_BYTES);
        take.write_header();
        drop(take);

        let decoded = decode(&path);
        let (quadruples, remainder) = input.as_chunks::<4>();
        assert!(remainder.is_empty());
        assert_eq!(decoded, vec![quadruples.iter().map(|f| f[3]).collect::<Vec<_>>(), quadruples.iter().map(|f| f[1]).collect::<Vec<_>>()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deliberately return short writes, then fail partway into the following chunk. `write_all`
    /// must complete each earlier chunk, and a failed chunk must not count as complete frames.
    #[test]
    fn short_writes_and_late_io_error_report_only_committed_full_chunks() {
        struct LimitedWriter {
            bytes: Vec<u8>,
            fail_at: usize,
            largest_request: usize,
        }
        impl Write for LimitedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.largest_request = self.largest_request.max(bytes.len());
                if self.bytes.len() >= self.fail_at {
                    return Err(std::io::Error::other("synthetic disk failure"));
                }
                let n = bytes.len().min(7).min(self.fail_at - self.bytes.len());
                self.bytes.extend_from_slice(&bytes[..n]);
                Ok(n)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let input: Vec<f32> = (0..20_001).flat_map(|i| [i as f32, 0.5, -0.5, -(i as f32)]).collect();
        let mut scratch = Vec::with_capacity(TAKE_WRITE_CHUNK_BYTES);
        let mut short = LimitedWriter { bytes: Vec::new(), fail_at: usize::MAX, largest_request: 0 };
        let (written, error) = write_interleaved_chunks(&mut short, &mut scratch, &input, 4, &[3, 1]);
        assert!(error.is_none());
        assert_eq!(written, 20_001);
        assert_eq!(short.bytes.len(), written * 2 * std::mem::size_of::<f32>());
        assert!(short.largest_request <= TAKE_WRITE_CHUNK_BYTES);
        assert_eq!(&short.bytes[..8], &[(-0.0f32).to_le_bytes(), 0.5f32.to_le_bytes()].concat());

        let mut fail = LimitedWriter { bytes: Vec::new(), fail_at: TAKE_WRITE_CHUNK_BYTES + 3, largest_request: 0 };
        let (written, error) = write_interleaved_chunks(&mut fail, &mut scratch, &input, 4, &[3, 1]);
        assert_eq!(written, TAKE_WRITE_CHUNK_BYTES / 8, "only the first complete chunk is committed");
        assert!(error.is_some_and(|e| e.to_string().contains("synthetic disk failure")));
        assert_eq!(fail.bytes.len(), TAKE_WRITE_CHUNK_BYTES + 3, "a late error may leave bytes from the unfinished chunk on disk");
        assert_eq!(fail.bytes[..TAKE_WRITE_CHUNK_BYTES], short.bytes[..TAKE_WRITE_CHUNK_BYTES]);
        assert!(fail.largest_request <= TAKE_WRITE_CHUNK_BYTES);
        assert!(scratch.len() <= TAKE_WRITE_CHUNK_BYTES);
    }

    /// `write_all` can commit one or more complete samples before failing its *first* 64-KiB
    /// chunk. Since no whole chunk was committed, `frames` remains zero, but the WAV decoder can
    /// recover the bytes after the unfinished zero-sized header. Keep this file for recovery,
    /// without advertising a failed take as a usable clip; still remove a truly empty take.
    #[test]
    fn zero_counted_frames_after_io_error_preserve_recoverable_wav_but_not_empty_takes() {
        let dir = std::env::temp_dir().join(format!("soundcraft-recover-first-failed-chunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (failed_path, second_path, empty_path, existing_path) =
            (dir.join("Failed.wav"), dir.join("SecondFailed.wav"), dir.join("Empty.wav"), dir.join("Existing.wav"));
        let mut failed = TakeWriter::create(TrackId(1), vec![0], failed_path.clone(), 48_000).unwrap();
        // Simulate the on-disk aftermath of a short write before the first complete chunk.
        failed.file.write_all(&0.375f32.to_le_bytes()).unwrap();
        failed.error = Some("synthetic disk failure after a recoverable sample".into());
        assert_eq!(failed.frames, 0);
        let mut second_failed = TakeWriter::create(TrackId(3), vec![0], second_path.clone(), 48_000).unwrap();
        second_failed.file.write_all(&0.25f32.to_le_bytes()).unwrap();
        second_failed.error = Some("another failed WAV write".into());
        let empty = TakeWriter::create(TrackId(2), vec![0], empty_path.clone(), 48_000).unwrap();
        let sentinel = b"pre-existing recording must remain untouched";
        std::fs::write(&existing_path, sentinel).unwrap();

        let shared = Shared::new(CaptureRing::new(32)); // disarmed: flush and finalize at once
        let (done, problem) = write_takes(&shared, vec![failed, second_failed, empty], 1);
        assert!(done.is_empty(), "a failed zero-count WAV is not imported as a completed take");
        let problem = problem.expect("the first disk error and all retained file paths are reported");
        assert!(problem.starts_with("synthetic disk failure"), "preserve the original first I/O error: {problem}");
        assert!(problem.contains("2 failed WAV file(s)"), "show how many unimported files remain: {problem}");
        assert!(
            problem.contains(&format!("{failed_path:?}")) && problem.contains(&format!("{second_path:?}")),
            "both distinct preserved paths must be discoverable: {problem}"
        );
        assert!(problem.contains("not added to the session"), "never imply a failed file became a successful take: {problem}");
        assert!(failed_path.exists(), "a short failed write can contain recoverable audio");
        assert_eq!(decode(&failed_path), vec![vec![0.375]]);
        assert_eq!(decode(&second_path), vec![vec![0.25]]);
        assert!(!empty_path.exists(), "a successfully opened but empty take is disposable");
        assert_eq!(std::fs::read(existing_path).unwrap(), sentinel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_notice_escapes_controls_and_bounds_long_or_numerous_paths() {
        let strange = PathBuf::from(format!("recordings/line\nbreak/{}", "x".repeat(4_096)));
        let preview = recovery_path_preview(&strange);
        assert!(preview.contains("\\n") && !preview.contains('\n'), "path controls must be escaped: {preview}");
        assert!(preview.contains("[path abbreviated]"));
        assert!(preview.len() <= RECOVERY_PATH_PREVIEW_BYTES + "… [path abbreviated]".len());

        let mut paths = RecoveryFiles::default();
        for _ in 0..10 {
            paths.add(&strange);
        }
        let mut problem = Some("original disk failure".into());
        paths.append_to(&mut problem);
        let problem = problem.unwrap();
        assert!(problem.starts_with("original disk failure; "));
        assert!(problem.contains("10 failed WAV file(s)") && problem.contains("7 more path(s) omitted"));
        assert!(problem.len() < 800, "ten hostile paths must not create an unbounded status: {} bytes", problem.len());
    }

    #[test]
    fn takes_stream_into_their_files() {
        let dir = std::env::temp_dir().join(format!("soundcraft-takes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gtr = TakeWriter::create(TrackId(1), vec![1, 2], dir.join("Gtr_01.wav"), 48_000).unwrap();
        let mut vox = TakeWriter::create(TrackId(2), vec![0], dir.join("Vox_01.wav"), 48_000).unwrap();
        assert!(TakeWriter::create(TrackId(3), vec![0], dir.join("Vox_01.wav"), 48_000).is_err(), "never overwrites a file");
        vox.write(&[0.5; 300], 3);
        vox.write_header();
        vox.write(&[0.25; 150], 3);
        assert_eq!(decode(&dir.join("Vox_01.wav"))[0].len(), 100, "the header covers what it has seen");

        let shared = Arc::new(Shared::new(CaptureRing::new(1 << 16)));
        shared.armed.store(true, Ordering::Release);
        let sh = Arc::clone(&shared);
        let writer = std::thread::spawn(move || write_takes(&sh, vec![gtr, vox], 3));
        let input: Vec<f32> = (0..3 * 4_800).map(|i| i as f32 / 20_000.0).collect();
        let mut owed = 0;
        input.chunks(3 * 480).for_each(|block| shared.capture(block, &mut owed));
        shared.armed.store(false, Ordering::Release);
        let (done, problem) = writer.join().unwrap();
        let channel = |c: usize| input.iter().skip(c).step_by(3).copied().collect::<Vec<f32>>();
        assert_eq!((done.len(), problem), (2, None));
        assert_eq!(decode(&done[0].1), vec![channel(1), channel(2)]);
        assert_eq!(decode(&done[1].1), vec![[vec![0.5; 100], vec![0.25; 50], channel(0)].concat()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_sums_itself_up_for_drawing_while_it_records() {
        let dir = std::env::temp_dir().join(format!("soundcraft-live-take-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut take = TakeWriter::create(TrackId(4), vec![2, 0], dir.join("Gtr_01.wav"), 48_000).unwrap();
        let input: Vec<f32> = (0..300).flat_map(|f| [f as f32, -1.0, -(f as f32)]).collect();
        take.write(&input[..3 * 100], 3);
        take.write(&input[3 * 100..], 3);
        let live = take.live.lock().unwrap().clone();
        assert_eq!(live, [[(-127.0, 0.0), (-255.0, -128.0), (-299.0, -256.0)], [(0.0, 127.0), (128.0, 255.0), (256.0, 299.0)]]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Ring state behind the lock: interleaved input frames at the input device's rate, plus the
/// fractional read position (in input frames) of the next output sample.
struct RingState {
    data: std::collections::VecDeque<f32>,
    phase: f64,
}

/// No pending monitor trim. A pending request survives a contended `try_lock` until the next
/// successful output read, so stale samples can never slip through after an idle period.
const NO_MONITOR_TRIM: usize = usize::MAX;

/// These counters describe the *monitoring* path only; the separate WAV capture FIFO is not
/// affected by a failed monitor lock. An output read can happen during playback even when no
/// track is monitoring, so consumers must only interpret output-silence counts for active
/// monitoring periods. Occupancy is not a clock-drift or hardware-latency measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MonitorFifoHealth {
    /// Complete interleaved input frames currently queued, sampled without acquiring the FIFO.
    pub queued_frames: usize,
    pub capacity_frames: usize,
    /// Samples discarded because the input callback could not acquire the FIFO's `try_lock`.
    pub skipped_input_samples: u64,
    /// Input frames with at least one skipped sample, including a partial interleaved frame.
    pub skipped_input_frames: u64,
    /// Samples pushed out of the *oldest* end by the half-second FIFO capacity limit.
    pub evicted_oldest_samples: u64,
    /// Input frames touched by that eviction, even if only one channel sample was evicted.
    pub evicted_oldest_frames: u64,
    /// Session-rate frames returned as silence by `pop_into` because monitor input was missing
    /// or its `try_lock` failed. These are *not* physical output-device frames, and intentional
    /// zero-valued samples in the input signal are not counted.
    pub silent_output_frames: u64,
}

/// A small interleaved FIFO from the input callback to the output callback, for input
/// monitoring. Both sides only `try_lock`; it holds at most ~0.5 s and drops the oldest frames.
/// The input device may run at a different rate than the session, so the reader converts
/// rates with linear interpolation (cheap, allocation-free, fine for monitoring).
pub struct InputRing {
    state: Mutex<RingState>,
    pub channels: std::sync::atomic::AtomicUsize,
    /// Input device sample rate in Hz (0 when unknown, which reads as "same as the session").
    rate: u32,
    /// Most samples kept: half a second of input, or 24 000 frames when the rate is unknown.
    cap: usize,
    /// Keep this many newest frames before the next read (0 clears all), or no pending request.
    pending_keep: AtomicUsize,
    /// Best-effort occupancy snapshot; the two callbacks update it while holding `state`.
    queued_samples: AtomicUsize,
    skipped_input_samples: AtomicU64,
    skipped_input_frames: AtomicU64,
    evicted_oldest_samples: AtomicU64,
    evicted_oldest_frames: AtomicU64,
    silent_output_frames: AtomicU64,
}

impl InputRing {
    pub fn new(channels: usize, rate: u32) -> Arc<InputRing> {
        let ch = channels.max(1);
        let frames = if rate == 0 { 24_000 } else { rate as usize / 2 };
        Arc::new(InputRing {
            state: Mutex::new(RingState { data: std::collections::VecDeque::with_capacity(rate.max(48_000) as usize * ch), phase: 0.0 }),
            channels: std::sync::atomic::AtomicUsize::new(ch),
            rate,
            cap: frames * ch,
            pending_keep: AtomicUsize::new(NO_MONITOR_TRIM),
            queued_samples: AtomicUsize::new(0),
            skipped_input_samples: AtomicU64::new(0),
            skipped_input_frames: AtomicU64::new(0),
            evicted_oldest_samples: AtomicU64::new(0),
            evicted_oldest_frames: AtomicU64::new(0),
            silent_output_frames: AtomicU64::new(0),
        })
    }

    /// An allocation-free, lock-free diagnostic snapshot of this stream's monitor FIFO. Each
    /// counter is individually atomic; the fields need not describe the exact same instant when
    /// input and output callbacks race. No counter measures input/output clock alignment.
    pub fn health(&self) -> MonitorFifoHealth {
        let ch = self.channels.load(Ordering::Relaxed).max(1);
        MonitorFifoHealth {
            queued_frames: self.queued_samples.load(Ordering::Relaxed) / ch,
            capacity_frames: self.cap / ch,
            skipped_input_samples: self.skipped_input_samples.load(Ordering::Relaxed),
            skipped_input_frames: self.skipped_input_frames.load(Ordering::Relaxed),
            evicted_oldest_samples: self.evicted_oldest_samples.load(Ordering::Relaxed),
            evicted_oldest_frames: self.evicted_oldest_frames.load(Ordering::Relaxed),
            silent_output_frames: self.silent_output_frames.load(Ordering::Relaxed),
        }
    }

    /// Clear accumulated losses for a new monitoring interval without discarding queued input
    /// or altering interpolation. Atomic callback increments before/after a reset are assigned
    /// to the old/new interval respectively; `health` itself is not a transactionally atomic read.
    pub fn reset_health(&self) {
        self.skipped_input_samples.store(0, Ordering::Relaxed);
        self.skipped_input_frames.store(0, Ordering::Relaxed);
        self.evicted_oldest_samples.store(0, Ordering::Relaxed);
        self.evicted_oldest_frames.store(0, Ordering::Relaxed);
        self.silent_output_frames.store(0, Ordering::Relaxed);
    }

    /// Drop old monitoring samples while the output is silent. Input capture into WAV files uses
    /// a different ring and is unaffected. Never block an audio callback on the input producer.
    pub(crate) fn discard_idle(&self) {
        self.request_trim(0);
    }

    /// On enabling monitoring, retain at most the newest 10 ms so the first audible block is
    /// current input, rather than the half-second backlog an idle output may have accumulated.
    pub(crate) fn resume_monitoring(&self) {
        let rate = if self.rate == 0 { 48_000 } else { self.rate };
        self.request_trim((rate as usize / 100).max(2));
    }

    fn request_trim(&self, keep: usize) {
        self.pending_keep.store(keep, Ordering::Release);
        if let Ok(mut s) = self.state.try_lock() {
            self.trim_pending(&mut s);
        }
    }

    fn trim_pending(&self, s: &mut RingState) {
        let keep = self.pending_keep.swap(NO_MONITOR_TRIM, Ordering::AcqRel);
        if keep == NO_MONITOR_TRIM {
            return;
        }
        let ch = self.channels.load(Ordering::Relaxed).max(1);
        // Remove complete interleaved frames only; a new read starts at a frame boundary.
        let remove = (s.data.len() / ch).saturating_sub(keep) * ch;
        if remove == s.data.len() {
            s.data.clear();
        } else if remove > 0 {
            s.data.drain(..remove);
        }
        s.phase = 0.0;
        self.queued_samples.store(s.data.len(), Ordering::Relaxed);
    }

    pub fn push(&self, samples: &[f32]) {
        if let Ok(mut s) = self.state.try_lock() {
            s.data.extend(samples.iter().copied().map(finite));
            let ch = self.channels.load(Ordering::Relaxed).max(1);
            let evicted = s.data.len().saturating_sub(self.cap);
            while s.data.len() > self.cap {
                s.data.pop_front();
            }
            self.queued_samples.store(s.data.len(), Ordering::Relaxed);
            if evicted > 0 {
                self.evicted_oldest_samples.fetch_add(u64::try_from(evicted).unwrap_or(u64::MAX), Ordering::Relaxed);
                self.evicted_oldest_frames.fetch_add(u64::try_from(evicted.div_ceil(ch)).unwrap_or(u64::MAX), Ordering::Relaxed);
            }
        } else {
            let ch = self.channels.load(Ordering::Relaxed).max(1);
            if !samples.is_empty() {
                self.skipped_input_samples.fetch_add(u64::try_from(samples.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
                self.skipped_input_frames.fetch_add(u64::try_from(samples.len().div_ceil(ch)).unwrap_or(u64::MAX), Ordering::Relaxed);
            }
        }
    }

    /// Fill `out` (planar, at `session_rate`) with up to `frames` frames, converting from the
    /// input rate. Missing frames are silence.
    pub fn pop_into(&self, out: &mut [Vec<f32>], frames: usize, session_rate: u32) {
        let ch = self.channels.load(Ordering::Relaxed).max(1);
        for c in out.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        let Ok(mut s) = self.state.try_lock() else {
            self.silent_output_frames.fetch_add(u64::try_from(frames).unwrap_or(u64::MAX), Ordering::Relaxed);
            return;
        };
        self.trim_pending(&mut s);
        // Input frames consumed per output frame.
        let step = if self.rate == 0 || session_rate == 0 { 1.0 } else { f64::from(self.rate) / f64::from(session_rate) };
        let mut rendered = 0;
        for f in 0..frames {
            // Drop input frames we have moved past; keep the one at `phase` and the next one.
            while s.phase >= 1.0 && s.data.len() >= 2 * ch {
                s.data.drain(..ch);
                s.phase -= 1.0;
            }
            if s.phase >= 1.0 || s.data.len() < 2 * ch {
                break;
            }
            let fr = s.phase as f32;
            for c in 0..ch {
                let a = s.data.get(c).copied().unwrap_or(0.0);
                let b = s.data.get(ch + c).copied().unwrap_or(a);
                if let Some(x) = out.get_mut(c).and_then(|o| o.get_mut(f)) {
                    *x = a + (b - a) * fr;
                }
            }
            s.phase += step;
            rendered += 1;
        }
        self.queued_samples.store(s.data.len(), Ordering::Relaxed);
        if rendered < frames {
            self.silent_output_frames.fetch_add(u64::try_from(frames - rendered).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::{InputRing, MonitorFifoHealth, NO_MONITOR_TRIM, Ordering};

    /// Both callbacks have an intentional `try_lock`: when another thread owns the FIFO, input
    /// samples are dropped and output frames are silent, but neither callback may wait for it.
    /// A partial backend block still counts the *affected* input frame, not just whole pairs.
    #[test]
    fn monitor_fifo_health_counts_lock_contention_for_one_two_and_three_channels() {
        for channels in [1, 2, 3] {
            let ring = InputRing::new(channels, 8);
            let blocked = channels * 3 + 1;
            let guard = ring.state.lock().unwrap();
            ring.push(&vec![0.25; blocked]);
            let mut output = vec![vec![1.0; 5]; channels];
            ring.pop_into(&mut output, 5, 8);
            assert!(output.iter().all(|channel| channel.iter().all(|&v| v == 0.0)));
            assert_eq!(
                ring.health(),
                MonitorFifoHealth {
                    queued_frames: 0,
                    capacity_frames: 4,
                    skipped_input_samples: blocked as u64,
                    skipped_input_frames: 4,
                    evicted_oldest_samples: 0,
                    evicted_oldest_frames: 0,
                    silent_output_frames: 5,
                },
                "{channels}-channel monitor lock contention must not be invisible"
            );
            drop(guard);
            ring.push(&vec![0.875; channels * 2]);
            assert_eq!(ring.health().queued_frames, 2, "subsequent successful input must still be queued");
            ring.reset_health();
            let reset = ring.health();
            assert_eq!(reset.queued_frames, 2);
            assert_eq!((reset.skipped_input_samples, reset.skipped_input_frames, reset.silent_output_frames), (0, 0, 0));
        }
    }

    /// The finite queue keeps the newest complete interleaved frames and reports each *oldest*
    /// discarded frame; intentional idle trims should not be called input overruns.
    #[test]
    fn monitor_fifo_health_counts_oldest_frame_evictions_without_reallocating() {
        for channels in [1, 2, 3] {
            let ring = InputRing::new(channels, 8); // 4 input frames fit
            let reserved = ring.state.lock().unwrap().data.capacity();
            let input: Vec<f32> = (0..6).flat_map(|frame| std::iter::repeat_n(frame as f32, channels)).collect();
            ring.push(&input);
            let pressure = ring.health();
            assert_eq!((pressure.queued_frames, pressure.capacity_frames), (4, 4));
            assert_eq!(pressure.evicted_oldest_samples, (2 * channels) as u64);
            assert_eq!(pressure.evicted_oldest_frames, 2);
            assert_eq!((pressure.skipped_input_samples, pressure.silent_output_frames), (0, 0));
            assert_eq!(ring.state.lock().unwrap().data.capacity(), reserved, "monitoring may not grow backing storage for a bounded callback");

            let mut output = vec![vec![0.0; 3]; channels];
            ring.pop_into(&mut output, 3, 8);
            assert!(output.iter().all(|c| c.as_slice() == [2.0, 3.0, 4.0]), "latest frames must retain correct channel alignment");
            let currently_queued = ring.health().queued_frames;
            ring.reset_health();
            let reset = ring.health();
            assert_eq!(reset.queued_frames, currently_queued, "resetting diagnostics must not change live input data");
            assert_eq!((reset.evicted_oldest_samples, reset.evicted_oldest_frames, reset.silent_output_frames), (0, 0, 0));
        }
    }

    /// A malformed fragment is not expected from CPAL, but it can force the oldest side to
    /// evict just one sample. Report the touched frame rather than rounding 1/3 down to zero.
    #[test]
    fn monitor_fifo_health_reports_partial_three_channel_eviction() {
        let ring = InputRing::new(3, 8);
        ring.push(&[0.125; 12]);
        ring.push(&[0.5]);
        let health = ring.health();
        assert_eq!(health.queued_frames, 4);
        assert_eq!(health.evicted_oldest_samples, 1);
        assert_eq!(health.evicted_oldest_frames, 1);
    }

    /// Interpolation needs two input frames. Once only one remains, output is zeroed even if the
    /// signal is nonzero; count exactly the requested frames that could not be rendered.
    #[test]
    fn monitor_fifo_health_counts_interpolation_underrun_not_silent_input_samples() {
        let ring = InputRing::new(2, 48_000);
        ring.push(&[0.5, -0.5, 0.75, -0.75]);
        let mut output = vec![vec![1.0; 4]; 2];
        ring.pop_into(&mut output, 4, 48_000);
        assert_eq!(output[0], [0.5, 0.0, 0.0, 0.0]);
        assert_eq!(output[1], [-0.5, 0.0, 0.0, 0.0]);
        assert_eq!(ring.health().silent_output_frames, 3);
        assert_eq!(ring.health().queued_frames, 1);

        ring.reset_health();
        ring.discard_idle();
        ring.push(&[0.0, 0.0, 0.0, 0.0]);
        ring.pop_into(&mut output, 1, 48_000);
        assert_eq!((output[0][0], output[1][0]), (0.0, 0.0));
        assert_eq!(ring.health().silent_output_frames, 0, "valid zero-valued input is not an underrun");
    }

    #[test]
    fn monitor_fifo_health_idle_and_resume_trims_reset_occupancy_without_evictions() {
        let ring = InputRing::new(3, 1000);
        ring.push(&[0.25; 900]); // 300 frames buffered, no overflow
        assert_eq!(ring.health().queued_frames, 300);
        ring.resume_monitoring(); // retain newest 10ms at 1000Hz
        assert_eq!(ring.health().queued_frames, 10);
        ring.discard_idle();
        let health = ring.health();
        assert_eq!(health.queued_frames, 0);
        assert_eq!(health.evicted_oldest_samples, 0, "intentional stale-input trim isn't a FIFO overrun");
        assert_eq!(health.silent_output_frames, 0);
    }

    /// A 1 kHz sine recorded at 16 kHz must come out as a 1 kHz sine at 48 kHz, across calls.
    #[test]
    fn monitor_converts_input_rate_to_session_rate() {
        let ring = InputRing::new(1, 16_000);
        let input: Vec<f32> = (0..4_000).map(|i| (2.0 * std::f64::consts::PI * 1_000.0 * f64::from(i) / 16_000.0).sin() as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 48]];
        let mut k = 0usize;
        for _ in 0..10 {
            ring.pop_into(&mut out, 48, 48_000);
            for v in &out[0] {
                let want = (2.0 * std::f64::consts::PI * 1_000.0 * k as f64 / 48_000.0).sin() as f32;
                assert!((v - want).abs() < 0.05, "sample {k}: got {v}, want {want}");
                k += 1;
            }
        }
    }

    /// Input faster than the session (96 kHz into 48 kHz): every other frame is read, in order.
    #[test]
    fn monitor_decimates_when_input_is_faster() {
        let ring = InputRing::new(1, 96_000);
        let input: Vec<f32> = (0..1_000).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 100]];
        ring.pop_into(&mut out, 100, 48_000);
        for (k, v) in out[0].iter().enumerate() {
            assert_eq!(*v, (2 * k) as f32, "output {k}");
        }
    }

    /// Same rate: samples pass through unchanged.
    #[test]
    fn monitor_passes_through_at_equal_rates() {
        let ring = InputRing::new(2, 48_000);
        let input: Vec<f32> = (0..200).map(|i| i as f32).collect();
        ring.push(&input);
        let mut out = vec![vec![0.0f32; 50], vec![0.0f32; 50]];
        ring.pop_into(&mut out, 50, 48_000);
        assert_eq!(out[0][0], 0.0);
        assert_eq!(out[1][0], 1.0);
        assert_eq!(out[0][49], 98.0);
    }

    /// Nothing buffered: silence, not stale data.
    #[test]
    fn monitor_underrun_is_silence() {
        let ring = InputRing::new(1, 16_000);
        let mut out = vec![vec![1.0f32; 16]];
        ring.pop_into(&mut out, 16, 48_000);
        assert!(out[0].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn idle_monitor_discards_stale_stereo_frames_and_resets_interpolation_phase() {
        let ring = InputRing::new(2, 48_000);
        ring.push(&[0.125, -0.125].repeat(24_000));
        let mut out = vec![vec![0.0f32; 32], vec![0.0f32; 32]];
        ring.pop_into(&mut out, 1, 48_000); // leaves a nonzero read phase
        ring.discard_idle();
        let fresh: Vec<f32> = (0..512).flat_map(|i| [i as f32, -(i as f32)]).collect();
        ring.push(&fresh);
        ring.pop_into(&mut out, 32, 48_000);
        assert_eq!(out[0][..32], (0..32).map(|i| i as f32).collect::<Vec<_>>());
        assert_eq!(out[1][..32], (0..32).map(|i| -(i as f32)).collect::<Vec<_>>());
    }

    #[test]
    fn contended_idle_trim_is_applied_before_any_later_monitor_read() {
        let ring = InputRing::new(1, 48_000);
        ring.push(&[0.125; 24_000]);
        let guard = ring.state.lock().unwrap();
        ring.discard_idle(); // a realtime callback must never wait for the input producer
        assert_eq!(ring.pending_keep.load(Ordering::Acquire), 0);
        drop(guard);
        let mut out = vec![vec![1.0f32; 32]];
        ring.pop_into(&mut out, 32, 48_000);
        assert!(out[0].iter().all(|v| *v == 0.0), "the queued trim must suppress old input");
        ring.push(&[0.875; 128]);
        ring.pop_into(&mut out, 32, 48_000);
        assert_eq!(out[0], [0.875; 32]);
    }

    #[test]
    fn contended_monitor_resume_keeps_only_fresh_interleaved_frames() {
        let ring = InputRing::new(2, 48_000);
        ring.push(&[0.125, -0.125].repeat(24_000));
        let guard = ring.state.lock().unwrap();
        ring.resume_monitoring();
        assert_eq!(ring.pending_keep.load(Ordering::Acquire), 480, "the 10 ms trim remains pending on try_lock contention");
        drop(guard);
        ring.push(&[0.875, -0.5].repeat(512));
        let mut out = vec![vec![0.0f32; 128], vec![0.0f32; 128]];
        ring.pop_into(&mut out, 128, 48_000);
        assert_eq!(out[0], [0.875; 128]);
        assert_eq!(out[1], [-0.5; 128]);
        assert_eq!(ring.pending_keep.load(Ordering::Acquire), NO_MONITOR_TRIM);
    }

    /// NaN and infinities from the device monitor as silence.
    #[test]
    fn monitor_silences_nan_and_infinite_input() {
        let ring = InputRing::new(1, 48_000);
        ring.push(&[0.5, f32::NAN, f32::INFINITY, -0.5, 0.0, 0.0, 0.0, 0.0]);
        let mut out = vec![vec![1.0f32; 4]];
        ring.pop_into(&mut out, 4, 48_000);
        assert_eq!(out[0], [0.5, 0.0, 0.0, -0.5]);
    }
}
