//! Audio input capture for recording.
//!
//! A [`Recorder`] opens the default input device. While armed, the input callback copies each
//! block into a preallocated lock-free ring (it never locks or allocates), and a writer thread
//! streams the ring into one WAV file per take, rewriting the header every second so a crash leaves
//! a playable file.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
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

/// Single-producer single-consumer FIFO of samples (input callback → writer thread). The slots
/// are atomics, so it needs no lock and no `unsafe`.
struct CaptureRing {
    slots: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl CaptureRing {
    fn new(capacity: usize) -> CaptureRing {
        let n = capacity.max(2).next_power_of_two();
        CaptureRing { slots: (0..n).map(|_| AtomicU32::new(0)).collect(), mask: n - 1, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Queue up to `n` samples (`sample(i)` is the i-th); returns how many fit.
    fn push(&self, n: usize, sample: impl Fn(usize) -> f32) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let n = n.min(self.slots.len().saturating_sub(head.wrapping_sub(self.tail.load(Ordering::Acquire))));
        for i in 0..n {
            if let Some(slot) = self.slots.get(head.wrapping_add(i) & self.mask) {
                slot.store(sample(i).to_bits(), Ordering::Relaxed);
            }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
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
    /// Callbacks that have committed to capturing a block. Stop must wait for them before the
    /// writer's final ring drain, or their last samples could arrive after the WAV is finalized.
    capturing: AtomicUsize,
    ring: CaptureRing,
    /// Samples lost while the ring was full.
    dropped: AtomicUsize,
    /// Input callbacks so far, to tell a device that stopped sending.
    beats: AtomicU64,
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
    fn new(ring: CaptureRing) -> Shared {
        let (beats, fault, failed) = (AtomicU64::new(0), AtomicU8::new(0), Mutex::new(None));
        Shared {
            armed: AtomicBool::new(false),
            capturing: AtomicUsize::new(0),
            ring,
            dropped: AtomicUsize::new(0),
            beats,
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
    let mut scratch = vec![0.0f32; 8192 * usize::from(config.channels).max(1)];
    let chunk = scratch.len();
    let mut owed = 0;
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            crate::mark_audio_thread();
            sh.guarded(|| {
                for part in data.chunks(chunk) {
                    let Some(s) = scratch.get_mut(..part.len()) else { return };
                    for (o, x) in s.iter_mut().zip(part) {
                        *o = f32::from_sample(*x);
                    }
                    mon.push(s);
                    if sh.armed.load(Ordering::SeqCst) {
                        if !sh.capture_if_armed(|| sh.capture(s, &mut owed)) {
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
        let shared = Arc::new(Shared::new(ring));
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
        self.shared.armed.store(true, Ordering::SeqCst);
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

    /// Input the current take has lost to a slow disk so far, in milliseconds.
    pub fn lost_ms(&self) -> u64 {
        let frames = self.shared.dropped.load(Ordering::Relaxed) / self.channels.max(1);
        (frames as u64).saturating_mul(1000) / u64::from(self.sample_rate.max(1))
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
        let lost = self.shared.dropped.swap(0, Ordering::Relaxed) / self.channels.max(1);
        let slow =
            (lost > 0).then(|| format!("{} ms of input lost: the disk was too slow", lost.saturating_mul(1000) / self.sample_rate.max(1) as usize));
        (takes, problem.or(slow))
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
    for mut t in takes {
        t.write_header();
        if t.frames == 0 {
            let _ = std::fs::remove_file(&t.path);
        } else {
            done.push((t.id, t.path));
        }
        problem = problem.or(t.error);
    }
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
        let (mut owed, mut out) = (0, Vec::new());
        shared.capture(&[1.0; 6], &mut owed);
        shared.capture(&[2.0; 4], &mut owed);
        shared.ring.pop_all(&mut out);
        shared.capture(&[3.0; 2], &mut owed);
        shared.ring.pop_all(&mut out);
        assert_eq!(out, [vec![1.0; 6], vec![2.0; 2], vec![0.0; 2], vec![3.0; 2]].concat());
        assert_eq!((owed, shared.dropped.load(Ordering::Relaxed)), (0, 2));
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
        })
    }

    pub fn push(&self, samples: &[f32]) {
        if let Ok(mut s) = self.state.try_lock() {
            s.data.extend(samples.iter().copied().map(finite));
            while s.data.len() > self.cap {
                s.data.pop_front();
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
        let Ok(mut s) = self.state.try_lock() else { return };
        // Input frames consumed per output frame.
        let step = if self.rate == 0 || session_rate == 0 { 1.0 } else { f64::from(self.rate) / f64::from(session_rate) };
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
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::InputRing;

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
