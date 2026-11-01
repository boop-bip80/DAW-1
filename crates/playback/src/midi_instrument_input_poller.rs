//! Hardware-free bounded MIDI input poller for native software-instrument audition.
//!
//! This module opens **no** OS ports, creates **no** threads, and has no UI or session-load
//! activation. A future explicitly consented native worker may implement [`MonitorInput`] with
//! an already connected device. Every operation here runs on that off-audio thread, forwards
//! fixed-size messages through the monitor's preallocated nonblocking queue, and fails closed
//! on a missed NoteOff, unplug, playback/seek, or session revision change.
//!
//! `release_requested` is deliberately **not** a held-note release acknowledgment. CLAP and
//! VST3 can queue note-offs until an actual `plugin.process`; a separate verified cleanup
//! receipt design is required before the UI may gate transport on successful note release.

use std::collections::BTreeSet;

use soundcraft_midi::live::TimedMidiMessage;

use crate::{TransportSnapshot, midi_instrument_monitor::MonitorRouter};

pub const MAX_POLL_INPUTS: usize = 16;
pub const MAX_POLL_EVENTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollPhase {
    Active,
    Cancelled,
    Failed,
}

/// Read-only bookkeeping on a non-realtime worker. No driver enumeration or connection occurs
/// when this value is constructed or cloned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollStatus {
    pub phase: PollPhase,
    pub connected_sources: Vec<String>,
    pub received: u64,
    pub input_loss: u64,
    /// The producer is disabled and stale queued messages will be discarded. This says nothing
    /// about whether a CLAP/VST3 instrument has processed the corresponding note-offs.
    pub release_requested: bool,
    pub error: Option<String>,
}

impl Default for PollStatus {
    fn default() -> Self {
        Self { phase: PollPhase::Cancelled, connected_sources: Vec::new(), received: 0, input_loss: 0, release_requested: false, error: None }
    }
}

/// Pure worker-side port abstraction. Device callbacks only enqueue into a bounded transport;
/// `try_recv`, loss accounting, and unplug detection are performed off the audio callback.
pub trait MonitorInput {
    /// Exact normalized source name; an OS adapter must verify it after connecting.
    fn source_name(&self) -> &str;
    fn try_recv(&mut self) -> Option<TimedMidiMessage>;
    fn take_lost(&mut self) -> u64;
    fn still_present(&self) -> Result<bool, String>;
}

pub struct MidiInstrumentInputPoller<I: MonitorInput> {
    router: MonitorRouter,
    sources: Vec<I>,
    start_generation: u64,
    start_revision: u64,
    cursor: usize,
    status: PollStatus,
}

impl<I: MonitorInput> MidiInstrumentInputPoller<I> {
    /// No device opening or implicit consent. A future native adapter must verify the selected
    /// ports against a *fresh* OS scan and supply already connected sources under a separate
    /// per-launch user action. This constructor still rejects a playing transport/stale route.
    pub fn new(router: MonitorRouter, sources: Vec<I>, observed: TransportSnapshot, session_revision: u64) -> Result<Self, String> {
        if observed.playing {
            return Err("Stop playback before arming software-instrument MIDI input".into());
        }
        if sources.is_empty() || sources.len() > MAX_POLL_INPUTS || sources.len() != router.enabled_sources().len() {
            return Err(format!("software-instrument MIDI input requires 1..={MAX_POLL_INPUTS} connected, selected ports"));
        }
        let mut seen = BTreeSet::new();
        for source in &sources {
            let name = source.source_name();
            if name.is_empty()
                || name.trim() != name
                || name.chars().count() > 64
                || name.chars().any(|character| character.is_control() || character == '=')
                || !seen.insert(name.to_lowercase())
                || !router.enabled_sources().iter().any(|selected| selected == name)
            {
                return Err(format!("MIDI input source {name:?} is invalid, ambiguous, or not enabled"));
            }
        }
        let names = sources.iter().map(|input| input.source_name().to_owned()).collect();
        Ok(Self {
            router,
            sources,
            start_generation: observed.generation,
            start_revision: session_revision,
            cursor: 0,
            status: PollStatus { phase: PollPhase::Active, connected_sources: names, ..PollStatus::default() },
        })
    }

    pub fn status(&self) -> PollStatus {
        self.status.clone()
    }

    /// Stop input delivery and request callback-side panic, without ever claiming completed
    /// note release. The exact requested epoch is only an internal cancellation token.
    pub fn stop(&mut self) {
        if self.status.phase != PollPhase::Active {
            return;
        }
        let _ = self.router.stop();
        self.sources.clear();
        self.status.connected_sources.clear();
        self.status.release_requested = true;
        self.status.phase = if self.status.error.is_some() { PollPhase::Failed } else { PollPhase::Cancelled };
    }

    fn fail(&mut self, reason: String, lost: u64) -> String {
        self.status.input_loss = self.status.input_loss.saturating_add(lost);
        self.status.error = Some(reason.clone());
        self.stop();
        reason
    }

    /// One bounded worker poll. A caller chooses when to request presence verification; there
    /// is no background clock, device call, or allocation in the audio callback. Even when the
    /// oldest backlog contains thousands of events, one tick consumes at most 256.
    pub fn poll(&mut self, observed: TransportSnapshot, session_revision: u64, check_presence: bool) -> Result<usize, String> {
        if self.status.phase != PollPhase::Active {
            return Ok(0);
        }
        if observed.playing || observed.generation != self.start_generation || session_revision != self.start_revision {
            self.stop();
            return Ok(0);
        }
        let lost = self.sources.iter_mut().map(MonitorInput::take_lost).fold(0u64, u64::saturating_add);
        if lost != 0 {
            return Err(self.fail(format!("MIDI input lost {lost} packet(s); priority note cleanup requested"), lost));
        }
        if check_presence {
            for index in 0..self.sources.len() {
                let present = self.sources[index].still_present();
                match present {
                    Ok(true) => {}
                    Ok(false) => {
                        let name = self.sources[index].source_name().to_owned();
                        return Err(self.fail(format!("MIDI input {name:?} disconnected"), 0));
                    }
                    Err(error) => return Err(self.fail(format!("MIDI input presence check failed: {error}"), 0)),
                }
            }
        }
        let mut received = 0;
        while received < MAX_POLL_EVENTS && !self.sources.is_empty() {
            let mut found = false;
            for _ in 0..self.sources.len() {
                let index = self.cursor % self.sources.len();
                self.cursor = self.cursor.wrapping_add(1);
                let Some(message) = self.sources[index].try_recv() else { continue };
                found = true;
                received += 1;
                if let Err(error) = self.router.try_send(self.sources[index].source_name(), message.message) {
                    let reason = format!("cannot route MIDI input {:?}: {error}", self.sources[index].source_name());
                    return Err(self.fail(reason, 1));
                }
                self.status.received = self.status.received.saturating_add(1);
                if received == MAX_POLL_EVENTS {
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

impl<I: MonitorInput> Drop for MidiInstrumentInputPoller<I> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::midi_instrument_monitor::{MAX_MONITOR_EVENTS, monitor_pair};
    use soundcraft_midi::live::LiveMidiMessage;
    use soundcraft_mix::LiveInstrumentEvent;
    use soundcraft_model::{ChannelFormat, Insert, Session, TrackId, TrackKind};

    struct FakeInput {
        name: String,
        queue: VecDeque<TimedMidiMessage>,
        lost: u64,
        present: bool,
    }

    impl FakeInput {
        fn new(name: &str) -> Self {
            Self { name: name.to_owned(), queue: VecDeque::new(), lost: 0, present: true }
        }
        fn note(&mut self, channel: u8) {
            self.queue.push_back(TimedMidiMessage { timestamp_micros: 0, message: LiveMidiMessage::NoteOn { channel, pitch: 60, velocity: 100 } });
        }
    }

    impl MonitorInput for FakeInput {
        fn source_name(&self) -> &str {
            &self.name
        }
        fn try_recv(&mut self) -> Option<TimedMidiMessage> {
            self.queue.pop_front()
        }
        fn take_lost(&mut self) -> u64 {
            std::mem::take(&mut self.lost)
        }
        fn still_present(&self) -> Result<bool, String> {
            Ok(self.present)
        }
    }

    fn snapshot(generation: u64, playing: bool) -> TransportSnapshot {
        TransportSnapshot { position: 0, playing, speed: 1.0, generation }
    }

    fn fixture() -> (MonitorRouter, crate::midi_instrument_monitor::MonitorReader, TrackId) {
        let mut s = Session::default();
        let track = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let t = s.track_mut(track).unwrap();
        t.instrument = Some(Insert::new("fake:test-synth"));
        t.mixer.input_monitor = true;
        t.midi_input.port = Some("Keys".into());
        t.midi_input.channel = Some(2);
        let (port, reader) = monitor_pair();
        (port.arm(&s, &["Keys".into()]).unwrap(), reader, track)
    }

    #[test]
    fn preflight_requires_stopped_transport_unique_exact_named_source() {
        let (router, _reader, _) = fixture();
        assert!(MidiInstrumentInputPoller::new(router, vec![FakeInput::new("Keys")], snapshot(1, true), 5).is_err());
        let (router, _reader, _) = fixture();
        assert!(MidiInstrumentInputPoller::new(router, vec![FakeInput::new("Unselected")], snapshot(1, false), 5).is_err());
        let (router, _reader, _) = fixture();
        assert!(MidiInstrumentInputPoller::new(router, vec![FakeInput::new("Keys"), FakeInput::new("Keys")], snapshot(1, false), 5).is_err());
    }

    #[test]
    fn source_and_channel_filter_with_controller_preserved() {
        let (router, mut reader, track) = fixture();
        let mut fake = FakeInput::new("Keys");
        fake.note(0);
        fake.note(1);
        fake.queue
            .push_back(TimedMidiMessage { timestamp_micros: 1, message: LiveMidiMessage::ControlChange { channel: 1, controller: 64, value: 127 } });
        let mut poller = MidiInstrumentInputPoller::new(router, vec![fake], snapshot(7, false), 12).unwrap();
        assert_eq!(poller.poll(snapshot(7, false), 12, true).unwrap(), 3);
        assert!(reader.epoch_changed());
        let mut out = [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS];
        assert_eq!(reader.drain(&mut out), 2);
        assert_eq!(out[0].track, track);
        assert!(matches!(out[1].message, LiveMidiMessage::ControlChange { controller: 64, value: 127, .. }));
    }

    #[test]
    fn queue_saturation_on_lost_noteoff_fails_closed_without_release_claim() {
        let (router, mut reader, _) = fixture();
        let mut fake = FakeInput::new("Keys");
        for _ in 0..MAX_MONITOR_EVENTS {
            fake.note(1);
        }
        fake.queue.push_back(TimedMidiMessage { timestamp_micros: 2, message: LiveMidiMessage::NoteOff { channel: 1, pitch: 60, velocity: 0 } });
        let mut poller = MidiInstrumentInputPoller::new(router, vec![fake], snapshot(0, false), 0).unwrap();
        assert_eq!(poller.poll(snapshot(0, false), 0, false).unwrap(), MAX_POLL_EVENTS);
        assert!(poller.poll(snapshot(0, false), 0, false).is_err());
        assert_eq!(poller.status().phase, PollPhase::Failed);
        assert!(poller.status().release_requested);
        assert!(!reader.enabled());
        assert!(reader.epoch_changed());
        assert_eq!(reader.drain(&mut [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS]), 0);
    }

    #[test]
    fn unplug_and_loss_are_fail_closed() {
        let (router, _reader, _) = fixture();
        let mut lost = FakeInput::new("Keys");
        lost.lost = 3;
        let mut poller = MidiInstrumentInputPoller::new(router, vec![lost], snapshot(1, false), 1).unwrap();
        assert!(poller.poll(snapshot(1, false), 1, false).is_err());
        assert_eq!(poller.status().input_loss, 3);
        assert!(poller.status().release_requested);
        let (router, _reader, _) = fixture();
        let mut missing = FakeInput::new("Keys");
        missing.present = false;
        let mut poller = MidiInstrumentInputPoller::new(router, vec![missing], snapshot(1, false), 1).unwrap();
        assert!(poller.poll(snapshot(1, false), 1, true).is_err());
        assert_eq!(poller.status().phase, PollPhase::Failed);
    }

    #[test]
    fn stopped_or_seek_or_replaced_session_cancels_before_messages() {
        for (observed, revision) in [(snapshot(2, true), 1), (snapshot(3, false), 1), (snapshot(2, false), 2)] {
            let (router, mut reader, _) = fixture();
            let mut input = FakeInput::new("Keys");
            input.note(1);
            let mut poller = MidiInstrumentInputPoller::new(router, vec![input], snapshot(2, false), 1).unwrap();
            assert_eq!(poller.poll(observed, revision, false).unwrap(), 0);
            assert!(poller.status().release_requested);
            assert_eq!(poller.status().phase, PollPhase::Cancelled);
            assert!(reader.epoch_changed());
            assert_eq!(reader.drain(&mut [LiveInstrumentEvent::EMPTY; MAX_MONITOR_EVENTS]), 0);
        }
    }

    #[test]
    fn stale_poller_stop_and_drop_do_not_cancel_newly_armed_router() {
        let mut session = Session::default();
        let id = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        session.track_mut(id).unwrap().instrument = Some(Insert::new("fake:synth"));
        session.track_mut(id).unwrap().mixer.input_monitor = true;
        let (port, _reader) = monitor_pair();
        let old_router = port.arm(&session, &["Keys".into()]).unwrap();
        let mut old_poller = MidiInstrumentInputPoller::new(old_router, vec![FakeInput::new("Keys")], snapshot(5, false), 8).unwrap();
        let new_router = port.arm(&session, &["Keys".into()]).unwrap();
        old_poller.stop();
        drop(old_poller);
        assert!(port.enabled());
        assert_eq!(new_router.try_send("Keys", LiveMidiMessage::NoteOn { channel: 0, pitch: 60, velocity: 100 }).unwrap(), 1);
    }
}
