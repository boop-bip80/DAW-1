//! Explicit native MIDI input-worker scaffold for software-instrument audition.
//!
//! **No application/UI code calls this yet.** A future per-launch consent action may start
//! `NativeMonitorWorker`; merely loading a session or saving a MIDI input route never opens
//! hardware. The worker owns midir streams, polls bounded event queues off the audio thread,
//! and sends only fixed-size messages into `midi_instrument_monitor`'s preallocated callback
//! queue. Driver timestamps are deliberately not treated as sample-accurate plugin offsets.
//!
//! Existing Record and external MIDI Thru runtimes open their own MIDI input ports. Until there
//! is a shared connection manager, the UI must enforce **exclusive** input ownership.

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use soundcraft_midi::{
    device::{self, MidiInputStream},
    live::TimedMidiMessage,
};

use crate::{
    TransportObserver, TransportSnapshot,
    midi_instrument_monitor::{MonitorPort, MonitorRouter},
};

const MAX_INPUT_PORTS: usize = 16;
const INPUT_QUEUE_CAPACITY: usize = 4096;
const MAX_EVENTS_PER_POLL: usize = 256;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PRESENCE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorInputPhase {
    Disabled,
    Connecting,
    Ready,
    Releasing,
    Failed,
}

/// Read-only clone. Its construction does not enumerate or connect MIDI ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInputStatus {
    pub phase: MonitorInputPhase,
    pub connected_inputs: Vec<String>,
    pub received: u64,
    pub input_loss: u64,
    pub cleanup_pending: bool,
    pub error: Option<String>,
}

impl Default for MonitorInputStatus {
    fn default() -> Self {
        Self { phase: MonitorInputPhase::Disabled, connected_inputs: Vec::new(), received: 0, input_loss: 0, cleanup_pending: false, error: None }
    }
}

/// An ephemeral, explicit-control-action precondition. It is never serialized or constructed
/// from saved settings. The UI must also reject a racing Record/MIDI-Thru start while active.
pub struct ExclusiveInputConsent(());

impl ExclusiveInputConsent {
    pub fn from_user_action(playing: bool, recording: bool, midi_thru_active_or_cleaning: bool) -> Result<Self, String> {
        if playing || recording {
            return Err("Stop playback and recording before enabling software-instrument MIDI monitoring".into());
        }
        if midi_thru_active_or_cleaning {
            return Err("Disable external MIDI thru and wait for its cleanup acknowledgment before opening keyboard monitoring".into());
        }
        Ok(Self(()))
    }
}

/// All methods are called by the MIDI control/worker thread, never the audio callback. Fake
/// implementations allow deterministic missing-port, overflow and unplug simulations.
trait MonitorInput {
    fn name(&self) -> &str;
    fn try_recv(&mut self) -> Option<TimedMidiMessage>;
    fn take_loss(&mut self) -> u64;
    fn still_present(&self) -> Result<bool, String>;
}

struct NativeInput {
    key: String,
    stream: MidiInputStream,
}

impl MonitorInput for NativeInput {
    fn name(&self) -> &str {
        &self.key
    }
    fn try_recv(&mut self) -> Option<TimedMidiMessage> {
        self.stream.try_recv()
    }
    fn take_loss(&mut self) -> u64 {
        self.stream.take_dropped_events().saturating_add(self.stream.take_truncated_packets())
    }
    fn still_present(&self) -> Result<bool, String> {
        self.stream.port_still_present().map_err(|error| error.to_string())
    }
}

fn input_key(name: &str) -> String {
    name.chars().filter(|ch| !ch.is_control() && *ch != '=').take(64).collect::<String>().trim().to_owned()
}

/// Check both the saved normalized port names and their *currently opened* identities. A
/// transient index is used only to open a port; it is never saved in a project or route.
fn open_native_sources(router: &MonitorRouter) -> Result<Vec<NativeInput>, String> {
    let selected = router.enabled_sources();
    if selected.is_empty() || selected.len() > MAX_INPUT_PORTS {
        return Err(format!("software-instrument monitoring requires 1..={MAX_INPUT_PORTS} enabled MIDI inputs"));
    }
    let scanned = device::input_ports().map_err(|error| format!("scan MIDI input devices: {error}"))?;
    let mut matching = Vec::with_capacity(selected.len());
    for wanted in selected {
        let ports: Vec<_> = scanned.iter().filter(|port| input_key(&port.name).eq_ignore_ascii_case(wanted)).collect();
        if ports.len() != 1 {
            return Err(format!("MIDI input {wanted:?} is {}: rescan devices", if ports.is_empty() { "unavailable" } else { "ambiguous" }));
        }
        matching.push(ports[0].clone());
    }
    let mut inputs = Vec::with_capacity(matching.len());
    for port in matching {
        let stream = device::open_input(port.index, INPUT_QUEUE_CAPACITY).map_err(|error| format!("open MIDI input {:?}: {error}", port.name))?;
        // Hot-plug may reorder ports between scanning and opening. Reject changed identity.
        if stream.port().index != port.index || input_key(&stream.port().name) != input_key(&port.name) {
            return Err(format!("MIDI input {:?} changed identity while opening; rescan and retry", port.name));
        }
        inputs.push(NativeInput { key: input_key(&port.name), stream });
    }
    Ok(inputs)
}

struct MonitorPoller<I: MonitorInput> {
    router: MonitorRouter,
    inputs: Vec<I>,
    start_generation: u64,
    session_revision: u64,
    last_presence: Instant,
    cursor: usize,
    status: MonitorInputStatus,
    cleanup_epoch: Option<u64>,
}

impl<I: MonitorInput> MonitorPoller<I> {
    fn new(router: MonitorRouter, inputs: Vec<I>, observed: TransportSnapshot, revision: u64) -> Result<Self, String> {
        if observed.playing || inputs.is_empty() || inputs.len() > MAX_INPUT_PORTS {
            return Err("software-instrument input requires a stopped transport and 1..16 input ports".into());
        }
        let sources = router.enabled_sources();
        if inputs.len() != sources.len() {
            return Err("live MIDI input connection set differs from enabled sources".into());
        }
        let mut unique = BTreeSet::new();
        for input in &inputs {
            let name = input.name();
            if name.is_empty() || input_key(name) != name || !unique.insert(name.to_lowercase()) || !sources.iter().any(|source| source == name) {
                return Err(format!("live MIDI input {name:?} is not an unambiguous enabled source"));
            }
        }
        let connected_inputs = inputs.iter().map(|input| input.name().to_owned()).collect();
        Ok(Self {
            router,
            inputs,
            start_generation: observed.generation,
            session_revision: revision,
            last_presence: Instant::now(),
            cursor: 0,
            status: MonitorInputStatus { phase: MonitorInputPhase::Ready, connected_inputs, ..MonitorInputStatus::default() },
            cleanup_epoch: None,
        })
    }

    fn stop(&mut self) {
        if self.cleanup_epoch.is_none() {
            self.cleanup_epoch = Some(self.router.stop());
        }
        self.inputs.clear(); // Close all midir ports on this worker, not the audio callback.
        self.status.connected_inputs.clear();
        self.status.phase = if self.status.error.is_some() { MonitorInputPhase::Failed } else { MonitorInputPhase::Releasing };
        self.status.cleanup_pending = self.cleanup_epoch.is_some_and(|epoch| !self.router.cleanup_acknowledged(epoch));
    }

    fn fail(&mut self, error: String, loss: u64) -> String {
        self.status.input_loss = self.status.input_loss.saturating_add(loss);
        self.status.error = Some(error.clone());
        self.stop();
        error
    }

    fn status(&mut self) -> MonitorInputStatus {
        if let Some(epoch) = self.cleanup_epoch {
            self.status.cleanup_pending = !self.router.cleanup_acknowledged(epoch);
            if !self.status.cleanup_pending && self.status.error.is_none() {
                self.status.phase = MonitorInputPhase::Disabled;
            }
        }
        self.status.clone()
    }

    /// One deterministic, strictly capped worker tick. Does not allocate per received MIDI
    /// event, never invokes a plugin or touches a device from an audio callback.
    fn tick(&mut self, observed: TransportSnapshot, revision: u64, force_presence: bool) -> Result<usize, String> {
        if self.cleanup_epoch.is_some() {
            return Ok(0);
        }
        if observed.playing || observed.generation != self.start_generation || revision != self.session_revision {
            self.stop();
            return Ok(0);
        }
        let lost = self.inputs.iter_mut().map(MonitorInput::take_loss).fold(0u64, u64::saturating_add);
        if lost > 0 {
            return Err(self.fail(format!("live MIDI input lost {lost} event(s); note cleanup requested"), lost));
        }
        if force_presence || self.last_presence.elapsed() >= PRESENCE_INTERVAL {
            self.last_presence = Instant::now();
            for index in 0..self.inputs.len() {
                let present = self.inputs[index].still_present();
                match present {
                    Ok(true) => {}
                    Ok(false) => {
                        let name = self.inputs[index].name().to_owned();
                        return Err(self.fail(format!("MIDI input {name:?} disconnected; note cleanup requested"), 0));
                    }
                    Err(error) => return Err(self.fail(format!("MIDI input presence check failed: {error}; note cleanup requested"), 0)),
                }
            }
        }
        let mut received = 0;
        while received < MAX_EVENTS_PER_POLL && !self.inputs.is_empty() {
            let mut found = false;
            for _ in 0..self.inputs.len() {
                let index = self.cursor % self.inputs.len();
                self.cursor = self.cursor.wrapping_add(1);
                let Some(event) = self.inputs[index].try_recv() else { continue };
                found = true;
                received += 1;
                if let Err(error) = self.router.try_send(self.inputs[index].name(), event.message) {
                    let error = format!("cannot route live MIDI from {:?}: {error}", self.inputs[index].name());
                    return Err(self.fail(error, 1));
                }
                self.status.received = self.status.received.saturating_add(1);
                if received == MAX_EVENTS_PER_POLL {
                    break;
                }
            }
            if !found {
                break;
            }
        }
        Ok(received)
    }
}

/// A dedicated native worker owning its input stream lifetime. A handle does not open a port
/// when created unless the caller has *explicitly* supplied a fresh user-action consent token.
/// `status()` is a cheap lock on the control/UI thread, never a device call.
pub struct NativeMonitorWorker {
    stop: Arc<AtomicBool>,
    port: MonitorPort,
    cleanup_epoch: Arc<AtomicU64>,
    status: Arc<Mutex<MonitorInputStatus>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NativeMonitorWorker {
    /// OS scanning and opening happen in the spawned worker, never on paint or the audio
    /// callback. The future SoundApp adapter must prevent simultaneous MIDI Record/Thru starts
    /// for the entire lifetime of this handle (not just while constructing the consent).
    pub fn start_explicit(
        _consent: ExclusiveInputConsent,
        router: MonitorRouter,
        observer: TransportObserver,
        session_revision: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let port = router.control_port();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let cleanup_epoch = Arc::new(AtomicU64::new(0));
        let worker_cleanup = Arc::clone(&cleanup_epoch);
        let worker_port = port.clone();
        let status = Arc::new(Mutex::new(MonitorInputStatus { phase: MonitorInputPhase::Connecting, ..MonitorInputStatus::default() }));
        let worker_status = Arc::clone(&status);
        let thread = thread::Builder::new()
            .name("soundcraft-midi-instrument-input".into())
            .spawn(move || {
                if worker_stop.load(Ordering::Acquire) {
                    let epoch = router.stop();
                    let _ = worker_cleanup.compare_exchange(0, epoch, Ordering::AcqRel, Ordering::Acquire);
                    return;
                }
                let starting = observer.snapshot();
                let initial_revision = session_revision.load(Ordering::Acquire);
                let mut poller = match open_native_sources(&router).and_then(|inputs| MonitorPoller::new(router, inputs, starting, initial_revision))
                {
                    Ok(poller) => poller,
                    Err(error) => {
                        let epoch = worker_port.invalidate();
                        let _ = worker_cleanup.compare_exchange(0, epoch, Ordering::AcqRel, Ordering::Acquire);
                        let mut latest = worker_status.lock().unwrap_or_else(PoisonError::into_inner);
                        *latest = MonitorInputStatus {
                            phase: MonitorInputPhase::Failed,
                            error: Some(error),
                            cleanup_pending: true,
                            ..MonitorInputStatus::default()
                        };
                        return;
                    }
                };
                while !worker_stop.load(Ordering::Acquire) {
                    let observed = observer.snapshot();
                    let revision = session_revision.load(Ordering::Acquire);
                    let _ = poller.tick(observed, revision, false);
                    *worker_status.lock().unwrap_or_else(PoisonError::into_inner) = poller.status();
                    if poller.cleanup_epoch.is_some() {
                        break;
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                poller.stop();
                if let Some(epoch) = poller.cleanup_epoch {
                    let _ = worker_cleanup.compare_exchange(0, epoch, Ordering::AcqRel, Ordering::Acquire);
                }
                *worker_status.lock().unwrap_or_else(PoisonError::into_inner) = poller.status();
            })
            .map_err(|error| format!("start live MIDI instrument worker: {error}"))?;
        Ok(Self { stop, port, cleanup_epoch, status, thread: Some(thread) })
    }

    /// Priority disable, even if a device callback queue is full. A missing output callback
    /// leaves cleanup pending indefinitely; the caller must not claim the notes are released.
    pub fn request_stop(&self) -> u64 {
        let mut epoch = self.cleanup_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            let requested = self.port.invalidate();
            epoch = match self.cleanup_epoch.compare_exchange(0, requested, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => requested,
                Err(existing) => existing,
            };
        }
        self.stop.store(true, Ordering::Release);
        epoch
    }

    pub fn status(&self) -> MonitorInputStatus {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let epoch = self.cleanup_epoch.load(Ordering::Acquire);
        if epoch != 0 {
            status.cleanup_pending = !self.port.cleanup_acknowledged(epoch);
            if status.phase == MonitorInputPhase::Releasing && !status.cleanup_pending {
                status.phase = MonitorInputPhase::Disabled;
            }
        }
        status
    }
}

impl Drop for NativeMonitorWorker {
    fn drop(&mut self) {
        self.request_stop();
        // Never synchronously wait for a driver scan or disconnect from an egui paint. The
        // dedicated worker observes the stop flag and drops its own device streams. A future
        // control-thread shutdown may explicitly join when the driver is known responsive.
        let _ = self.thread.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi_instrument_monitor::{MAX_MONITOR_EVENTS, monitor_pair};
    use soundcraft_midi::live::LiveMidiMessage;
    use soundcraft_mix::LiveInstrumentEvent;
    use soundcraft_model::{ChannelFormat, Insert, Session, TrackKind};
    use std::collections::VecDeque;

    struct FakeInput {
        name: String,
        pending: VecDeque<TimedMidiMessage>,
        loss: u64,
        present: bool,
    }

    impl FakeInput {
        fn new(name: &str) -> Self {
            Self { name: name.into(), pending: VecDeque::new(), loss: 0, present: true }
        }
        fn note(&mut self, channel: u8) {
            self.pending.push_back(TimedMidiMessage { timestamp_micros: 0, message: LiveMidiMessage::NoteOn { channel, pitch: 60, velocity: 100 } });
        }
    }

    impl MonitorInput for FakeInput {
        fn name(&self) -> &str {
            &self.name
        }
        fn try_recv(&mut self) -> Option<TimedMidiMessage> {
            self.pending.pop_front()
        }
        fn take_loss(&mut self) -> u64 {
            std::mem::take(&mut self.loss)
        }
        fn still_present(&self) -> Result<bool, String> {
            Ok(self.present)
        }
    }

    fn snapshot(generation: u64, playing: bool) -> TransportSnapshot {
        TransportSnapshot { position: 0, playing, speed: 1.0, generation }
    }

    fn fixture() -> (MonitorRouter, crate::midi_instrument_monitor::MonitorReader, soundcraft_model::TrackId) {
        let mut session = Session::default();
        let id = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let track = session.track_mut(id).unwrap();
        track.instrument = Some(Insert::new("fake:synth"));
        track.mixer.input_monitor = true;
        track.midi_input.port = Some("Keys".into());
        track.midi_input.channel = Some(2);
        let (port, receiver) = monitor_pair();
        let router = port.arm(&session, &["Keys".into()]).unwrap();
        (router, receiver, id)
    }

    #[test]
    fn no_implicit_consent_and_mutual_exclusion() {
        assert!(ExclusiveInputConsent::from_user_action(true, false, false).is_err());
        assert!(ExclusiveInputConsent::from_user_action(false, true, false).is_err());
        assert!(ExclusiveInputConsent::from_user_action(false, false, true).is_err());
        assert!(ExclusiveInputConsent::from_user_action(false, false, false).is_ok());
        assert_eq!(MonitorInputStatus::default().phase, MonitorInputPhase::Disabled);
    }

    #[test]
    fn fake_source_routes_only_exact_channel_and_bounded_tick() {
        let (router, mut receiver, id) = fixture();
        let mut source = FakeInput::new("Keys");
        source.note(0);
        source.note(1);
        let mut poller = MonitorPoller::new(router, vec![source], snapshot(7, false), 42).unwrap();
        assert_eq!(poller.tick(snapshot(7, false), 42, true).unwrap(), 2);
        assert!(receiver.epoch_changed());
        let mut messages = [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS];
        assert_eq!(receiver.drain(&mut messages), 1);
        assert_eq!(messages[0].track, id);
        assert_eq!(poller.status().received, 2);
    }

    #[test]
    fn ambiguous_or_unexpected_open_identity_rejected() {
        let (router, _, _) = fixture();
        assert!(MonitorPoller::new(router, vec![FakeInput::new("other")], snapshot(0, false), 1).is_err());
        let (router, _, _) = fixture();
        assert!(MonitorPoller::new(router, vec![FakeInput::new("Keys"), FakeInput::new("Keys")], snapshot(0, false), 1).is_err());
    }

    #[test]
    fn unplug_or_input_loss_disables_and_awaits_audio_callback_ack() {
        let (router, mut receiver, _) = fixture();
        let mut source = FakeInput::new("Keys");
        source.note(1);
        let mut poller = MonitorPoller::new(router, vec![source], snapshot(3, false), 17).unwrap();
        assert_eq!(poller.tick(snapshot(3, false), 17, true).unwrap(), 1);
        poller.inputs[0].present = false;
        assert!(poller.tick(snapshot(3, false), 17, true).is_err());
        assert_eq!(poller.status().phase, MonitorInputPhase::Failed);
        assert!(poller.status().cleanup_pending);
        assert!(receiver.epoch_changed());
        assert_eq!(receiver.drain(&mut [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS]), 0);
        let (router, _receiver, _) = fixture();
        let mut source = FakeInput::new("Keys");
        source.loss = 1;
        let mut poller = MonitorPoller::new(router, vec![source], snapshot(0, false), 1).unwrap();
        assert!(poller.tick(snapshot(0, false), 1, false).is_err());
        assert_eq!(poller.status().input_loss, 1);
    }

    #[test]
    fn stop_seek_or_session_revision_cancels_without_draining_stale_notes() {
        for (next_snapshot, next_revision) in [(snapshot(4, true), 5), (snapshot(5, false), 5), (snapshot(4, false), 6)] {
            let (router, mut receiver, _) = fixture();
            let mut source = FakeInput::new("Keys");
            source.note(1);
            let mut poller = MonitorPoller::new(router, vec![source], snapshot(4, false), 5).unwrap();
            assert_eq!(poller.tick(next_snapshot, next_revision, false).unwrap(), 0);
            assert_eq!(poller.status().phase, MonitorInputPhase::Releasing);
            assert!(poller.status().cleanup_pending);
            assert!(receiver.epoch_changed());
            let mut drained = [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS];
            assert_eq!(receiver.drain(&mut drained), 0);
            // The real AudioState, not a worker, is the only code allowed to ACK.
            assert!(poller.status().cleanup_pending);
        }
    }

    #[test]
    fn saturated_monitor_handoff_fails_closed_even_if_note_off_was_next() {
        let (router, mut receiver, _) = fixture();
        let mut source = FakeInput::new("Keys");
        for _ in 0..MAX_MONITOR_EVENTS {
            source.note(1);
        }
        source.pending.push_back(TimedMidiMessage { timestamp_micros: 1, message: LiveMidiMessage::NoteOff { channel: 1, pitch: 60, velocity: 64 } });
        let mut poller = MonitorPoller::new(router, vec![source], snapshot(0, false), 0).unwrap();
        assert_eq!(poller.tick(snapshot(0, false), 0, false).unwrap(), MAX_EVENTS_PER_POLL);
        assert!(poller.tick(snapshot(0, false), 0, false).is_err());
        assert_eq!(poller.status().phase, MonitorInputPhase::Failed);
        assert!(receiver.epoch_changed());
        assert_eq!(receiver.drain(&mut [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS]), 0);
    }
}
