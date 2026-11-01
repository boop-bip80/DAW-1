//! Hardware-free, explicitly armed software-instrument MIDI audition.
//!
//! This module does not open input devices, invoke plugins, or persist user consent. A
//! control/device worker preflights exact enabled source names and routes messages into a
//! fixed-size queue. The audio callback performs at most 256 nonblocking receives per render
//! block; on overflow, a separate atomic epoch forces an All Notes Off before any new notes.
//! These events start at the *next rendered block*, not at a sample-accurate device timestamp.

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use soundcraft_midi::live::LiveMidiMessage;
use soundcraft_mix::LiveInstrumentEvent;
use soundcraft_model::{Session, TrackId, TrackKind};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use thiserror::Error;

/// Hard maximum for queued messages and for one audio-block drain.
pub const MAX_MONITOR_EVENTS: usize = 256;
const MAX_MONITOR_SOURCES: usize = 64;
const MAX_MONITOR_ROUTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MonitorError {
    #[error("no uniquely named, enabled MIDI input sources")]
    NoSources,
    #[error("invalid or ambiguous MIDI input source {0:?}")]
    InvalidSource(String),
    #[error("invalid MIDI input route for track {0:?}")]
    InvalidRoute(TrackId),
    #[error("no audible, armed/monitored instrument tracks have an input route")]
    NoRoutes,
    #[error("live MIDI monitor event is malformed")]
    InvalidMessage,
    #[error("live MIDI monitor has been invalidated; re-arm it on the control thread")]
    Invalidated,
    #[error("live MIDI input source {0:?} is not enabled")]
    UnknownSource(String),
    #[error("live MIDI queue overflowed; all software-instrument notes must be released")]
    Overflow,
}

#[derive(Debug, Clone, Copy)]
struct QueuedEvent {
    epoch: u64,
    event: LiveInstrumentEvent,
}

#[derive(Default)]
struct MonitorState {
    /// A single atomically updated word: bit 0 is enabled; upper bits are a monotonic
    /// generation. Comparing only the generation then writing a separate enabled flag would
    /// let a stale worker A disable a freshly re-armed worker B between those operations.
    epoch: AtomicU64,
    /// Published by the audio callback *after* it actually releases sounding voices.
    /// A queued note-off or an off-thread invalidation is never itself an acknowledgment.
    cleanup_ack_epoch: AtomicU64,
    dropped: AtomicU64,
}

/// A cheap, clonable control-thread handle. It never connects to a device. Keeping this
/// separate from the receiver means there is no channel installation/allocation on the callback.
#[derive(Clone)]
pub struct MonitorPort {
    tx: Sender<QueuedEvent>,
    state: Arc<MonitorState>,
}

/// Callback-owned bounded receiver. Neither draining nor checking the epoch allocates or waits.
pub struct MonitorReader {
    rx: Receiver<QueuedEvent>,
    state: Arc<MonitorState>,
    seen_epoch: u64,
}

/// Preallocated once before the audio state is handed to the device callback.
pub fn monitor_pair() -> (MonitorPort, MonitorReader) {
    let (tx, rx) = bounded(MAX_MONITOR_EVENTS);
    let state = Arc::new(MonitorState::default());
    (MonitorPort { tx, state: Arc::clone(&state) }, MonitorReader { rx, state, seen_epoch: 0 })
}

#[derive(Debug, Clone, Copy)]
struct InstrumentRoute {
    track: TrackId,
    source: Option<usize>,
    channel: Option<u8>,
}

/// A route snapshot created explicitly on the control thread. If a session or transport
/// discontinuity invalidates this snapshot, new messages fail closed until a fresh preflight.
pub struct MonitorRouter {
    port: MonitorPort,
    epoch: u64,
    sources: Vec<String>,
    routes: Vec<InstrumentRoute>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && name.chars().count() <= 64 && !name.chars().any(|ch| ch.is_control() || ch == '=')
}

fn audible(session: &Session, track: &soundcraft_model::Track, any_solo: bool) -> bool {
    if !matches!(track.kind, TrackKind::Instrument | TrackKind::Midi)
        || track.instrument.is_none()
        || track.inactive
        || track.mixer.mute
        || !(track.mixer.record_arm || track.mixer.input_monitor)
        || (any_solo && !track.mixer.solo && !track.mixer.solo_safe)
    {
        return false;
    }
    !track.mixer.vca.is_some_and(|vca| session.tracks.iter().any(|other| other.kind == TrackKind::Vca && other.id.0 == vca && other.mixer.mute))
}

impl MonitorPort {
    /// Stop all routing immediately and request callback-side note cleanup via the atomic epoch.
    /// No queue slot is required, so even a full queue cannot prevent the panic signal.
    pub fn invalidate(&self) -> u64 {
        loop {
            let old = self.state.epoch.load(Ordering::Acquire);
            let next = old.wrapping_add(2) & !1;
            if self.state.epoch.compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return next;
            }
        }
    }

    /// Atomically invalidate *only* this exact armed route. A late Stop/Drop from an old
    /// native worker is harmless after the user has armed a replacement route.
    pub fn invalidate_if(&self, armed_epoch: u64) -> Option<u64> {
        if armed_epoch & 1 == 0 {
            return None;
        }
        let next = armed_epoch.wrapping_add(2) & !1;
        self.state.epoch.compare_exchange(armed_epoch, next, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| next)
    }

    /// Return the actual cleanup token when this route still owns the port, or the already
    /// disabled token after its queue overflow. If a newer armed route exists, leave it alone
    /// and return this route's old token (never pretend to have cleaned up the newer route).
    pub fn cancel_owned(&self, armed_epoch: u64) -> u64 {
        if let Some(next) = self.invalidate_if(armed_epoch) {
            return next;
        }
        let current = self.state.epoch.load(Ordering::Acquire);
        if current & 1 == 0 { current } else { armed_epoch }
    }

    fn arm_new(&self) -> u64 {
        loop {
            let old = self.state.epoch.load(Ordering::Acquire);
            let next = old.wrapping_add(2) | 1;
            if self.state.epoch.compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return next;
            }
        }
    }

    /// Only a completed audio callback can acknowledge the requested panic generation.
    /// If the audio device is gone and callbacks never run, this must stay false.
    pub fn cleanup_acknowledged(&self, requested_epoch: u64) -> bool {
        self.state.cleanup_ack_epoch.load(Ordering::Acquire) >= requested_epoch
    }

    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }

    pub fn enabled(&self) -> bool {
        self.state.epoch.load(Ordering::Acquire) & 1 != 0
    }

    /// A pure preflight with provided, **already verified** enabled source identities. This
    /// cannot establish physical connection: the eventual native adapter must independently
    /// confirm unique current OS port names before passing its source packets to the router.
    /// An existing router is invalidated only after the entire new preflight succeeds.
    pub fn arm(&self, session: &Session, enabled_sources: &[String]) -> Result<MonitorRouter, MonitorError> {
        if enabled_sources.is_empty() || enabled_sources.len() > MAX_MONITOR_SOURCES {
            return Err(MonitorError::NoSources);
        }
        for (i, name) in enabled_sources.iter().enumerate() {
            if !valid_name(name) || enabled_sources[..i].iter().any(|other| other.eq_ignore_ascii_case(name)) {
                return Err(MonitorError::InvalidSource(name.clone()));
            }
        }
        let any_solo = session.tracks.iter().any(|track| track.mixer.solo && !track.inactive);
        let mut routes = Vec::new();
        for track in &session.tracks {
            if !audible(session, track, any_solo) {
                continue;
            }
            let source = if let Some(name) = &track.midi_input.port {
                Some(enabled_sources.iter().position(|enabled| enabled == name).ok_or(MonitorError::InvalidRoute(track.id))?)
            } else {
                None
            };
            if track.midi_input.channel.is_some_and(|ch| !(1..=16).contains(&ch)) || routes.len() == MAX_MONITOR_ROUTES {
                return Err(MonitorError::InvalidRoute(track.id));
            }
            routes.push(InstrumentRoute { track: track.id, source, channel: track.midi_input.channel.map(|ch| ch - 1) });
        }
        if routes.is_empty() {
            return Err(MonitorError::NoRoutes);
        }
        let epoch = self.arm_new();
        Ok(MonitorRouter { port: self.clone(), epoch, sources: enabled_sources.to_vec(), routes })
    }
}

impl MonitorRouter {
    /// The exact (normalized and enabled) input identities used for route preflight.
    /// Native workers must independently verify each identity against freshly opened ports.
    pub fn enabled_sources(&self) -> &[String] {
        &self.sources
    }

    /// Callback-safe control handle, cloned before moving the router into its native worker.
    /// Status and stop operations use atomics; the worker alone owns OS input connections.
    pub fn control_port(&self) -> MonitorPort {
        self.port.clone()
    }

    /// Exact active epoch owned by this router, for generation-checked native-worker Stop.
    pub fn armed_epoch(&self) -> u64 {
        self.epoch
    }

    /// Request immediate producer shutdown and priority callback-side held-note cleanup.
    /// Idempotent for a stale router: dropping/releasing an old route cannot disable a new one.
    pub fn stop(&self) -> u64 {
        self.port.cancel_owned(self.epoch)
    }

    pub fn cleanup_acknowledged(&self, epoch: u64) -> bool {
        self.port.cleanup_acknowledged(epoch)
    }

    /// Call from a native device/control worker, never the audio callback. The route scan and
    /// fixed-size `try_send` require no allocation. Any dropped event disables the entire
    /// monitor and schedules priority note cleanup, including a dropped NoteOff or sustain CC.
    pub fn try_send(&self, source: &str, message: LiveMidiMessage) -> Result<usize, MonitorError> {
        if !self.port.enabled() || self.port.state.epoch.load(Ordering::Acquire) != self.epoch {
            return Err(MonitorError::Invalidated);
        }
        let Some(source_index) = self.sources.iter().position(|name| name == source) else {
            let _ = self.port.invalidate_if(self.epoch);
            return Err(MonitorError::UnknownSource(source.to_owned()));
        };
        let Some(channel) = valid_channel(message)? else { return Ok(0) };
        let mut sent = 0;
        for route in &self.routes {
            if route.source.is_some_and(|index| index != source_index) || route.channel.is_some_and(|ch| ch != channel) {
                continue;
            }
            let entry = QueuedEvent { epoch: self.epoch, event: LiveInstrumentEvent { track: route.track, message } };
            match self.port.tx.try_send(entry) {
                Ok(()) => sent += 1,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    self.port.state.dropped.fetch_add(1, Ordering::Relaxed);
                    let _ = self.port.invalidate_if(self.epoch);
                    return Err(MonitorError::Overflow);
                }
            }
        }
        Ok(sent)
    }
}

impl Drop for MonitorRouter {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn valid_channel(message: LiveMidiMessage) -> Result<Option<u8>, MonitorError> {
    use LiveMidiMessage::*;
    let (channel, data_ok) = match message {
        NoteOn { channel, pitch, velocity } | NoteOff { channel, pitch, velocity } => (channel, pitch < 128 && velocity < 128),
        PolyPressure { channel, pitch, pressure } => (channel, pitch < 128 && pressure < 128),
        ControlChange { channel, controller, value } => (channel, controller < 128 && value < 128),
        ProgramChange { channel, program } => (channel, program < 128),
        ChannelPressure { channel, pressure } => (channel, pressure < 128),
        PitchBend { channel, value } => (channel, (-8192..=8191).contains(&value)),
        Realtime(_) => return Ok(None),
    };
    if channel > 15 || !data_ok { Err(MonitorError::InvalidMessage) } else { Ok(Some(channel)) }
}

impl MonitorReader {
    /// The callback must run this even while otherwise silent: a saturated queue or Stop must
    /// release a previously sounding instrument. It is a single atomic read.
    pub fn epoch_changed(&mut self) -> bool {
        let now = self.state.epoch.load(Ordering::Acquire);
        if now == self.seen_epoch {
            return false;
        }
        self.seen_epoch = now;
        true
    }

    /// Audio-callback only, strictly after `MixEngine::panic_live_notes`. The release store
    /// lets a control worker wait for *actual callback execution* before restarting transport.
    pub(crate) fn acknowledge_cleanup(&self) {
        self.state.cleanup_ack_epoch.store(self.seen_epoch, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.state.epoch.load(Ordering::Acquire) & 1 != 0
    }

    /// At most one queue capacity per block. Discard stale events even if a new route was
    /// armed; old source messages can never bleed into a different session/track route.
    pub fn drain(&self, dest: &mut [LiveInstrumentEvent]) -> usize {
        let enabled = self.enabled();
        let epoch = self.state.epoch.load(Ordering::Acquire);
        let mut len = 0;
        for _ in 0..dest.len().min(MAX_MONITOR_EVENTS) {
            let Ok(item) = self.rx.try_recv() else { break };
            if enabled && item.epoch == epoch {
                dest[len] = item.event;
                len += 1;
            }
        }
        len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, Insert};

    fn session() -> (Session, TrackId, TrackId) {
        let mut s = Session::default();
        let a = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let b = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        for id in [a, b] {
            let t = s.track_mut(id).unwrap();
            t.instrument = Some(Insert::new("synth"));
            t.mixer.record_arm = true;
        }
        s.track_mut(a).unwrap().midi_input.port = Some("Keys A".into());
        s.track_mut(a).unwrap().midi_input.channel = Some(2);
        s.track_mut(b).unwrap().midi_input.port = Some("Keys B".into());
        (s, a, b)
    }

    fn note(channel: u8) -> LiveMidiMessage {
        LiveMidiMessage::NoteOn { channel, pitch: 60, velocity: 100 }
    }

    #[test]
    fn unique_ports_and_channel_preflight_fail_closed() {
        let (port, _) = monitor_pair();
        let (mut s, a, _) = session();
        assert!(matches!(port.arm(&s, &["Keys A".into(), "keys a".into()]), Err(MonitorError::InvalidSource(_))));
        assert!(matches!(port.arm(&s, &["Keys A".into()]), Err(MonitorError::InvalidRoute(_))));
        s.track_mut(a).unwrap().midi_input.channel = Some(17);
        assert!(matches!(port.arm(&s, &["Keys A".into(), "Keys B".into()]), Err(MonitorError::InvalidRoute(_))));
        assert!(!port.enabled());
    }

    #[test]
    fn source_and_one_based_channel_isolation() {
        let (port, mut receiver) = monitor_pair();
        let (session, a, b) = session();
        let router = port.arm(&session, &["Keys A".into(), "Keys B".into()]).unwrap();
        assert!(receiver.epoch_changed());
        assert_eq!(router.try_send("Keys A", note(0)).unwrap(), 0);
        assert_eq!(router.try_send("Keys A", note(1)).unwrap(), 1);
        assert_eq!(router.try_send("Keys B", note(1)).unwrap(), 1);
        let mut drained = [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS];
        assert_eq!(receiver.drain(&mut drained), 2);
        assert_eq!(drained[0].track, a);
        assert_eq!(drained[1].track, b);
        assert!(!receiver.epoch_changed());
    }

    #[test]
    fn dropping_an_obsolete_router_does_not_disable_a_new_route() {
        let (port, _receiver) = monitor_pair();
        let (s, _, _) = session();
        let old = port.arm(&s, &["Keys A".into(), "Keys B".into()]).unwrap();
        let new = port.arm(&s, &["Keys A".into(), "Keys B".into()]).unwrap();
        drop(old);
        assert!(port.enabled());
        assert_eq!(new.try_send("Keys A", note(1)).unwrap(), 1);
    }

    #[test]
    fn overflow_and_unknown_source_force_epoch_cleanup() {
        let (port, mut receiver) = monitor_pair();
        let (s, _, _) = session();
        let router = port.arm(&s, &["Keys A".into(), "Keys B".into()]).unwrap();
        assert!(receiver.epoch_changed());
        for _ in 0..MAX_MONITOR_EVENTS {
            assert_eq!(router.try_send("Keys A", note(1)).unwrap(), 1);
        }
        assert_eq!(router.try_send("Keys A", note(1)), Err(MonitorError::Overflow));
        assert!(!port.enabled());
        assert_eq!(port.dropped(), 1);
        assert!(receiver.epoch_changed());
        let mut drained = [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS];
        assert_eq!(receiver.drain(&mut drained), 0);
        let new_router = port.arm(&s, &["Keys A".into(), "Keys B".into()]).unwrap();
        assert_eq!(new_router.try_send("other", note(1)), Err(MonitorError::UnknownSource("other".into())));
        assert!(receiver.epoch_changed());
        assert_eq!(router.try_send("Keys A", note(1)), Err(MonitorError::Invalidated));
    }

    #[test]
    fn muted_and_unarmed_instruments_never_get_routes() {
        let (port, _receiver) = monitor_pair();
        let (mut s, a, b) = session();
        s.track_mut(a).unwrap().mixer.mute = true;
        s.track_mut(b).unwrap().mixer.record_arm = false;
        assert!(matches!(port.arm(&s, &["Keys A".into(), "Keys B".into()]), Err(MonitorError::NoRoutes)));
        s.track_mut(b).unwrap().mixer.input_monitor = true;
        let router = port.arm(&s, &["Keys A".into(), "Keys B".into()]).unwrap();
        assert_eq!(router.try_send("Keys B", note(0)).unwrap(), 1);
    }

    /// No OS or audio device. This fake instrument is driven by the *real* AudioState ->
    /// MixEngine render path and emits a constant sample only while its note is sounding.
    #[derive(Default)]
    struct SynthCounts {
        ons: AtomicU64,
        offs: AtomicU64,
        controls: AtomicU64,
        panics: AtomicU64,
        processed_panics: AtomicU64,
    }

    static TEST_SYNTH_INFO: soundcraft_dsp::PluginInfo = soundcraft_dsp::PluginInfo {
        id: "clap:test.live-audition",
        name: "Fake live audition synth",
        short_name: "Live Test",
        category: soundcraft_dsp::Category::Other,
        params: &[],
        is_instrument: true,
        audiosuite: false,
    };

    struct FakeSynth {
        counts: Arc<SynthCounts>,
        pressed: bool,
        sustained: bool,
        pedal: bool,
        pending_panic: bool,
    }

    impl soundcraft_dsp::Plugin for FakeSynth {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            &TEST_SYNTH_INFO
        }
        fn prepare(&mut self, _rate: f32, _block: usize, _channels: usize) {}
        fn reset(&mut self) {
            self.pressed = false;
            self.sustained = false;
            self.pedal = false;
            self.pending_panic = false;
        }
        fn set_param(&mut self, _id: &str, _value: f32) -> bool {
            false
        }
        fn param(&self, _id: &str) -> Option<f32> {
            None
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            // Deliberately CLAP-like: all_notes_off *queues* a release. Only the next plugin
            // processing block consumes it. An early atomic ACK would lie about held notes.
            if self.pending_panic {
                self.pressed = false;
                self.sustained = false;
                self.pedal = false;
                self.pending_panic = false;
                self.counts.processed_panics.fetch_add(1, Ordering::SeqCst);
            }
            if self.pressed || self.sustained {
                for channel in io {
                    for sample in channel.iter_mut().take(frames) {
                        *sample += 0.25;
                    }
                }
            }
        }
        fn note_on(&mut self, _offset: usize, _note: u8, _velocity: u8) {
            self.counts.ons.fetch_add(1, Ordering::SeqCst);
            self.pressed = true;
        }
        fn note_off(&mut self, _offset: usize, _note: u8) {
            self.counts.offs.fetch_add(1, Ordering::SeqCst);
            self.pressed = false;
            self.sustained = self.pedal;
        }
        fn midi(&mut self, _offset: usize, packet: [u8; 3]) {
            if packet[0] & 0xf0 == 0xb0 {
                self.counts.controls.fetch_add(1, Ordering::SeqCst);
                if packet[1] == 64 {
                    self.pedal = packet[2] >= 64;
                    if !self.pedal {
                        self.sustained = false;
                    }
                }
            }
        }
        fn all_notes_off(&mut self) {
            self.counts.panics.fetch_add(1, Ordering::SeqCst);
            self.pending_panic = true;
        }
    }

    fn audio_fixture() -> (crate::AudioState, std::sync::mpsc::Sender<crate::Cmd>, Session, Arc<SynthCounts>) {
        use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32};
        let mut session = Session::default();
        let id = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let mut seq = soundcraft_midi::Sequence::default();
        seq.notes.push(soundcraft_midi::Note { pitch: 72, velocity: 100, release_velocity: 64, channel: 1, start: 0, length: 480 });
        let cid = session.new_clip_id();
        {
            let t = session.track_mut(id).unwrap();
            t.instrument = Some(Insert::new(TEST_SYNTH_INFO.id));
            t.mixer.input_monitor = true;
            t.midi_input.port = Some("Keys A".into());
            t.midi_input.channel = Some(2);
            // An actual timeline note must never sound while stopped.
            t.playlist_mut().unwrap().clips.push(soundcraft_model::Clip::midi(cid, "Timeline note", 0, 48_000, seq));
        }
        let shared = Arc::new(crate::Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            transport_generation: AtomicU64::new(0),
            meters: std::sync::Mutex::new(crate::MeterSnapshot::default()),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let (garbage, _unused) = std::sync::mpsc::channel();
        let mut audio = crate::AudioState::new(rx, shared, Arc::new(session.clone()), 48_000.0, garbage);
        let counts = Arc::new(SynthCounts::default());
        audio.mix.adopt(vec![soundcraft_mix::PreparedInstance {
            track: id,
            slot: soundcraft_mix::INSTRUMENT_SLOT,
            id: TEST_SYNTH_INFO.id.into(),
            ch: 2,
            plugin: Box::new(FakeSynth { counts: Arc::clone(&counts), pressed: false, sustained: false, pedal: false, pending_panic: false }),
        }]);
        (audio, tx, session, counts)
    }

    fn output(audio: &mut crate::AudioState) -> f32 {
        let mut interleaved = [0.0f32; 256];
        audio.fill(&mut interleaved, 2);
        interleaved.iter().copied().fold(0.0, f32::max)
    }

    #[test]
    fn actual_stopped_audio_callback_renders_live_note_cc_sustain_and_release() {
        let (mut audio, _tx, session, counts) = audio_fixture();
        assert_eq!(output(&mut audio), 0.0, "no preflight, no note and no timeline clip");
        let router = audio.midi_monitor_port.arm(&session, &["Keys A".into()]).unwrap();
        assert_eq!(router.try_send("Keys A", note(0)).unwrap(), 0, "wrong MIDI channel ignored");
        assert_eq!(router.try_send("Keys A", note(1)).unwrap(), 1);
        assert!(output(&mut audio) > 0.0, "live instrument is audible with transport stopped");
        assert!(output(&mut audio) > 0.0, "sustained voice survives repeated stopped blocks");
        assert_eq!(counts.ons.load(Ordering::SeqCst), 1, "timeline MIDI did not play");
        router.try_send("Keys A", LiveMidiMessage::ControlChange { channel: 1, controller: 64, value: 127 }).unwrap();
        router.try_send("Keys A", LiveMidiMessage::NoteOff { channel: 1, pitch: 60, velocity: 64 }).unwrap();
        assert!(output(&mut audio) > 0.0, "CC64 holds released voice");
        router.try_send("Keys A", LiveMidiMessage::ControlChange { channel: 1, controller: 64, value: 0 }).unwrap();
        assert_eq!(output(&mut audio), 0.0, "pedal release silences the fake instrument");
        assert_eq!(counts.offs.load(Ordering::SeqCst), 1);
        assert!(counts.controls.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn overflow_stop_seek_and_session_change_release_live_notes() {
        let (mut audio, tx, session, counts) = audio_fixture();
        let sources = &["Keys A".into()];
        let router = audio.midi_monitor_port.arm(&session, sources).unwrap();
        router.try_send("Keys A", note(1)).unwrap();
        assert!(output(&mut audio) > 0.0);
        for _ in 0..MAX_MONITOR_EVENTS {
            router.try_send("Keys A", note(1)).unwrap();
        }
        assert_eq!(router.try_send("Keys A", note(1)), Err(MonitorError::Overflow));
        let overflow_epoch = router.port.state.epoch.load(Ordering::Acquire);
        assert!(!router.cleanup_acknowledged(overflow_epoch), "overflow flag is not a callback cleanup ACK");
        assert_eq!(output(&mut audio), 0.0, "priority epoch panic bypasses a full queue");
        assert!(router.cleanup_acknowledged(overflow_epoch));
        assert!(counts.panics.load(Ordering::SeqCst) >= 1);
        let router = audio.midi_monitor_port.arm(&session, sources).unwrap();
        router.try_send("Keys A", note(1)).unwrap();
        assert!(output(&mut audio) > 0.0);
        tx.send(crate::Cmd::Stop).unwrap();
        assert_eq!(output(&mut audio), 0.0);
        assert_eq!(router.try_send("Keys A", note(1)), Err(MonitorError::Invalidated));
        let router = audio.midi_monitor_port.arm(&session, sources).unwrap();
        router.try_send("Keys A", note(1)).unwrap();
        assert!(output(&mut audio) > 0.0);
        tx.send(crate::Cmd::Locate(1000)).unwrap();
        assert_eq!(output(&mut audio), 0.0);
        let router = audio.midi_monitor_port.arm(&session, sources).unwrap();
        router.try_send("Keys A", note(1)).unwrap();
        assert!(output(&mut audio) > 0.0);
        tx.send(crate::Cmd::Session(Arc::new(session), Vec::new())).unwrap();
        assert_eq!(output(&mut audio), 0.0);
    }

    #[test]
    fn explicit_stop_ack_is_deferred_until_real_audio_panic() {
        let (mut audio, _tx, session, counts) = audio_fixture();
        let router = audio.midi_monitor_port.arm(&session, &["Keys A".into()]).unwrap();
        router.try_send("Keys A", note(1)).unwrap();
        assert!(output(&mut audio) > 0.0);
        let requested_epoch = router.stop();
        assert!(!router.cleanup_acknowledged(requested_epoch));
        assert_eq!(output(&mut audio), 0.0);
        assert!(router.cleanup_acknowledged(requested_epoch));
        assert!(counts.panics.load(Ordering::SeqCst) >= 1);
        assert!(counts.processed_panics.load(Ordering::SeqCst) >= 1, "ACK requires a real post-panic plugin.process block");
    }
}
