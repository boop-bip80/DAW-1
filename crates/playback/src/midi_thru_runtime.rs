//! Opt-in, idle-only native MIDI keyboard-to-external-output monitoring worker.
//!
//! This module is deliberately NOT a live instrument audio monitoring path. It
//! uses already-connected output senders owned by `midi_out_runtime`, and opens
//! bounded *input* streams only when the separate MIDI-thru consent is enabled.
//! All discovery, queue polling, packet routing, device sends and note cleanup
//! are on the MIDI scheduler thread, never on an OS MIDI/audio callback. Device
//! timestamps have no shared calibrated output clock: `try_send_now` is best
//! effort, and even the worker's 5 ms polling cadence is not an audio latency
//! guarantee. Input/output port names can change on hot-plug; no indexes persist.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use soundcraft_midi::{
    device::{self, MidiInputStream},
    live::TimedMidiMessage,
};
use soundcraft_model::Session;

use crate::{
    TransportSnapshot,
    midi_live_thru::{MAX_THRU_INPUT_PORTS, MidiThruEvent, MidiThruPhase, MidiThruRouter, MidiThruSink, MidiThruStatus},
};

const INPUT_QUEUE_CAPACITY: usize = 4_096;
const MAX_EVENTS_PER_POLL: usize = 256;
const HOTPLUG_INTERVAL: Duration = Duration::from_secs(1);

/// Native MIDI-thru status, read only from the control/UI thread. A `Failed`
/// phase is sticky until the user explicitly disables and retries; neither a
/// saved route nor a previous launch re-enables the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiThruRuntimeStatus {
    pub enabled: bool,
    pub phase: MidiThruPhase,
    pub connected_inputs: Vec<String>,
    pub sent_packets: u64,
    pub dropped_inputs: u64,
    pub held_notes: usize,
    pub cleanup_pending: bool,
    pub last_error: Option<String>,
}

impl Default for MidiThruRuntimeStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            phase: MidiThruPhase::Disabled,
            connected_inputs: Vec::new(),
            sent_packets: 0,
            dropped_inputs: 0,
            held_notes: 0,
            cleanup_pending: false,
            last_error: None,
        }
    }
}

impl MidiThruRuntimeStatus {
    fn from_router(router: &MidiThruStatus, inputs: &[String]) -> Self {
        Self {
            enabled: router.phase == MidiThruPhase::Enabled,
            phase: router.phase,
            connected_inputs: inputs.to_vec(),
            sent_packets: router.sent_packets,
            dropped_inputs: router.input_drops,
            held_notes: router.held_notes,
            cleanup_pending: router.cleanup_pending,
            last_error: router.last_error.clone(),
        }
    }
}

/// Nonblocking queue consumer. Fake implementations make unplug and callback
/// overflow deterministic without opening hardware.
trait ThruInput {
    fn name(&self) -> &str;
    fn try_recv(&mut self) -> Option<TimedMidiMessage>;
    fn take_loss(&mut self) -> u64;
    fn still_present(&self) -> Result<bool, String>;
}

struct NativeInput {
    key: String,
    stream: MidiInputStream,
}

impl ThruInput for NativeInput {
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
    name.chars().filter(|character| !character.is_control() && *character != '=').take(64).collect::<String>().trim().to_owned()
}

fn selected_inputs(session: &Session) -> Result<Vec<String>, String> {
    let names: Vec<_> =
        session.edit.flags.iter().filter_map(|flag| flag.strip_prefix("midi.input.")).take(MAX_THRU_INPUT_PORTS + 1).map(str::to_owned).collect();
    if names.is_empty() || names.len() > MAX_THRU_INPUT_PORTS {
        return Err("enable 1–16 named MIDI input devices in Setup before opting into MIDI thru".into());
    }
    let mut unique = BTreeSet::new();
    for name in &names {
        if name.is_empty() || input_key(name) != *name || !unique.insert(name.to_lowercase()) {
            return Err(format!("MIDI input {name:?} is invalid or ambiguous; rescan input devices"));
        }
    }
    Ok(names)
}

/// OS scans and connections are performed only by the *already enabled* MIDI
/// worker, not by constructing its read-only status or by loading a session.
fn open_selected_inputs(session: &Session, output_names: &[String]) -> Result<(Vec<NativeInput>, Vec<String>), String> {
    let selected = selected_inputs(session)?;
    // Reject obvious virtual loopback aliases. Different names can still be
    // patched into a feedback loop externally; the event/queue limits remain.
    for input in &selected {
        if output_names.iter().any(|output| output.trim().eq_ignore_ascii_case(input)) {
            return Err(format!("input {input:?} shares an output name; refuse potential MIDI feedback loop"));
        }
    }
    let scan = device::input_ports().map_err(|error| format!("scan MIDI input ports: {error}"))?;
    let mut matches = Vec::new();
    for wanted in &selected {
        let ports: Vec<_> = scan.iter().filter(|port| input_key(&port.name) == *wanted).collect();
        if ports.len() != 1 {
            return Err(format!(
                "MIDI input {wanted:?} is {}: rescan devices before enabling thru",
                if ports.is_empty() { "unavailable" } else { "ambiguous" }
            ));
        }
        matches.push(ports[0].clone());
    }
    let mut opened = Vec::with_capacity(matches.len());
    for port in matches {
        let stream = device::open_input(port.index, INPUT_QUEUE_CAPACITY).map_err(|error| format!("open MIDI input {:?}: {error}", port.name))?;
        if stream.port().index != port.index || input_key(&stream.port().name) != input_key(&port.name) {
            return Err(format!("MIDI input {:?} changed while connecting; rescan and retry", port.name));
        }
        opened.push(NativeInput { key: input_key(&port.name), stream });
    }
    Ok((opened, selected))
}

/// A separate explicit consent is represented by constructing this object;
/// the caller must never call `start_native` from session restore or output
/// route selection. Its input devices are dropped before cleanup completes.
struct ThruPoller<I: ThruInput> {
    inputs: Vec<I>,
    router: MidiThruRouter,
    start_generation: u64,
    start_revision: u64,
    last_hotplug: Instant,
    cursor: usize,
}

impl<I: ThruInput> ThruPoller<I> {
    fn start(
        session: &Session,
        enabled_inputs: &[String],
        inputs: Vec<I>,
        outputs: &[String],
        observed: TransportSnapshot,
        session_revision: u64,
    ) -> Result<Self, String> {
        if observed.playing {
            return Err("MIDI thru is idle-only; Stop playback/recording before enabling it".into());
        }
        let names: Vec<_> = inputs.iter().map(|input| input.name().to_owned()).collect();
        let router = MidiThruRouter::enable(session, enabled_inputs, &names, outputs, observed.generation).map_err(|error| error.to_string())?;
        Ok(Self { inputs, router, start_generation: observed.generation, start_revision: session_revision, last_hotplug: Instant::now(), cursor: 0 })
    }

    fn stop(&mut self, sink: &mut impl MidiThruSink) {
        self.router.stop(sink);
        self.inputs.clear(); // Input connections are released before output ACK.
    }

    fn status(&self) -> MidiThruRuntimeStatus {
        let names: Vec<_> = self.inputs.iter().map(|input| input.name().to_owned()).collect();
        MidiThruRuntimeStatus::from_router(&self.router.status(), &names)
    }

    fn tick(&mut self, sink: &mut impl MidiThruSink, observed: TransportSnapshot, revision: u64, force_hotplug: bool) -> Result<usize, String> {
        if observed.playing || observed.generation != self.start_generation || revision != self.start_revision {
            self.stop(sink);
        }
        if self.router.status().phase != MidiThruPhase::Enabled {
            self.router.poll_cleanup(sink);
            return Ok(0);
        }
        let dropped = self.inputs.iter_mut().map(ThruInput::take_loss).fold(0u64, u64::saturating_add);
        if dropped > 0 {
            self.inputs.clear();
            return Err(self.router.report_input_loss(dropped, sink).err().map_or_else(|| "MIDI input overflow".into(), |error| error.to_string()));
        }
        if force_hotplug || self.last_hotplug.elapsed() >= HOTPLUG_INTERVAL {
            self.last_hotplug = Instant::now();
            for index in 0..self.inputs.len() {
                let present = match self.inputs[index].still_present() {
                    Ok(present) => present,
                    Err(error) => {
                        let lost = self.router.report_input_loss(1, sink).err();
                        self.inputs.clear();
                        return Err(format!(
                            "MIDI input port check failed: {error}; {}",
                            lost.map_or_else(|| "note cleanup requested".into(), |error| error.to_string())
                        ));
                    }
                };
                if !present {
                    let alive: Vec<_> =
                        self.inputs.iter().enumerate().filter(|(other, _)| *other != index).map(|(_, port)| port.name().to_owned()).collect();
                    let error = self
                        .router
                        .verify_connections(&alive, sink)
                        .err()
                        .map_or_else(|| "MIDI input disconnected".into(), |error| error.to_string());
                    self.inputs.clear();
                    return Err(error);
                }
            }
        }
        let names: Vec<_> = self.inputs.iter().map(|input| input.name().to_owned()).collect();
        if let Err(error) = self.router.verify_connections(&names, sink) {
            self.inputs.clear();
            return Err(error.to_string());
        }
        let mut processed = 0;
        while processed < MAX_EVENTS_PER_POLL && !self.inputs.is_empty() {
            let mut did_receive = false;
            for _ in 0..self.inputs.len() {
                let index = self.cursor % self.inputs.len();
                self.cursor = self.cursor.wrapping_add(1);
                let next = self.inputs[index].try_recv();
                let Some(event) = next else { continue };
                did_receive = true;
                // Only a single stack-allocated short message crosses into
                // the dispatcher; no callback or audio thread is involved.
                let result = self.router.dispatch_batch(&[MidiThruEvent { source_port: self.inputs[index].name(), message: event.message }], sink);
                if let Err(error) = result {
                    self.inputs.clear();
                    return Err(error.to_string());
                }
                processed += 1;
                if processed == MAX_EVENTS_PER_POLL {
                    break;
                }
            }
            if !did_receive {
                break;
            }
        }
        Ok(processed)
    }

    fn cleanup_complete(&mut self, sink: &impl MidiThruSink) -> bool {
        self.router.poll_cleanup(sink);
        let status = self.router.status();
        status.phase != MidiThruPhase::Enabled && !status.cleanup_pending
    }
}

pub(crate) struct NativeThru {
    poller: ThruPoller<NativeInput>,
}

impl NativeThru {
    pub(crate) fn start_native(session: &Session, observed: TransportSnapshot, revision: u64, outputs: &[String]) -> Result<Self, String> {
        let (opened, selected) = open_selected_inputs(session, outputs)?;
        let poller = ThruPoller::start(session, &selected, opened, outputs, observed, revision)?;
        Ok(Self { poller })
    }

    pub(crate) fn tick(&mut self, sink: &mut impl MidiThruSink, observed: TransportSnapshot, revision: u64) -> Result<usize, String> {
        self.poller.tick(sink, observed, revision, false)
    }

    pub(crate) fn stop(&mut self, sink: &mut impl MidiThruSink) {
        self.poller.stop(sink);
    }

    pub(crate) fn cleanup_complete(&mut self, sink: &impl MidiThruSink) -> bool {
        self.poller.cleanup_complete(sink)
    }

    pub(crate) fn status(&self) -> MidiThruRuntimeStatus {
        self.poller.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi_live_thru::MAX_THRU_EVENTS_PER_BATCH;
    use soundcraft_midi::live::LiveMidiMessage;
    use soundcraft_model::{ChannelFormat, MidiInputRoute, MidiOutputRoute, TrackKind};
    use std::collections::{BTreeMap, VecDeque};

    struct FakeInput {
        name: String,
        events: VecDeque<TimedMidiMessage>,
        dropped: u64,
        present: bool,
    }

    impl ThruInput for FakeInput {
        fn name(&self) -> &str {
            &self.name
        }
        fn try_recv(&mut self) -> Option<TimedMidiMessage> {
            self.events.pop_front()
        }
        fn take_loss(&mut self) -> u64 {
            std::mem::take(&mut self.dropped)
        }
        fn still_present(&self) -> Result<bool, String> {
            Ok(self.present)
        }
    }

    #[derive(Default)]
    struct FakeSink {
        micros: u64,
        sends: Vec<(String, u64, Vec<u8>)>,
        generation: BTreeMap<String, u64>,
        ack: BTreeMap<String, u64>,
        present: bool,
        full: bool,
    }

    impl MidiThruSink for FakeSink {
        fn try_send_now(&mut self, port: &str, packet: &[u8]) -> Result<(), String> {
            if self.full {
                return Err("queue full".into());
            }
            if !self.present {
                return Err("disconnected".into());
            }
            self.sends.push((port.into(), self.micros, packet.to_vec()));
            Ok(())
        }
        fn cancel_and_release(&mut self, port: &str) -> Result<u64, String> {
            if !self.present {
                return Err("disconnected".into());
            }
            let generation = self.generation.get(port).copied().unwrap_or(0) + 1;
            self.generation.insert(port.into(), generation);
            Ok(generation)
        }
        fn cleanup_acknowledged(&self, port: &str, generation: u64) -> bool {
            self.ack.get(port).copied().unwrap_or(0) >= generation
        }
        fn port_connected(&self, _port: &str) -> bool {
            self.present
        }
    }

    fn transport(generation: u64, playing: bool) -> TransportSnapshot {
        TransportSnapshot { generation, playing, position: 0, speed: 1.0 }
    }
    fn session() -> Session {
        let mut session = Session::default();
        let id = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Thru"));
        let track = session.track_mut(id).unwrap();
        track.mixer.record_arm = true;
        track.midi_input = MidiInputRoute { port: Some("Fake Keys".into()), channel: Some(1) };
        track.midi_output = Some(MidiOutputRoute { port: "Fake Synth".into(), channel: Some(3) });
        session.edit.flags.insert("midi.input.Fake Keys".into());
        session
    }
    fn note(pitch: u8, on: bool) -> TimedMidiMessage {
        TimedMidiMessage {
            timestamp_micros: 33,
            message: if on {
                LiveMidiMessage::NoteOn { channel: 0, pitch, velocity: 100 }
            } else {
                LiveMidiMessage::NoteOff { channel: 0, pitch, velocity: 64 }
            },
        }
    }
    fn input(events: Vec<TimedMidiMessage>) -> FakeInput {
        FakeInput { name: "Fake Keys".into(), events: events.into(), dropped: 0, present: true }
    }
    fn start(events: Vec<TimedMidiMessage>) -> ThruPoller<FakeInput> {
        ThruPoller::start(&session(), &["Fake Keys".into()], vec![input(events)], &["Fake Synth".into()], transport(3, false), 4).unwrap()
    }
    fn sink() -> FakeSink {
        FakeSink { present: true, ..Default::default() }
    }

    #[test]
    fn selected_inputs_never_auto_open_from_a_saved_output_route() {
        let mut session = session();
        assert_eq!(selected_inputs(&session).unwrap(), vec!["Fake Keys"]);
        session.edit.flags.clear();
        assert!(selected_inputs(&session).is_err());
        session.edit.flags.insert("midi.input.Fake Keys".into());
        session.edit.flags.insert("midi.input.fake keys".into());
        assert!(selected_inputs(&session).is_err(), "ambiguous input names cannot be auto selected");
    }

    #[test]
    fn bounded_fake_queue_sends_to_existing_output_now_with_source_and_channel_filtering() {
        let mut poller = start(vec![
            note(60, true),
            TimedMidiMessage { timestamp_micros: 40, message: LiveMidiMessage::ControlChange { channel: 0, controller: 7, value: 99 } },
            note(60, false),
        ]);
        let mut sink = sink();
        sink.micros = 9_900;
        assert_eq!(poller.tick(&mut sink, transport(3, false), 4, true).unwrap(), 3);
        assert_eq!(
            sink.sends.iter().map(|(_, when, bytes)| (*when, bytes.as_slice())).collect::<Vec<_>>(),
            vec![(9_900, &[0x92, 60, 100][..]), (9_900, &[0xb2, 7, 99]), (9_900, &[0x82, 60, 64])]
        );
        assert_eq!(poller.status().held_notes, 0);
    }

    #[test]
    fn stop_seek_loop_or_record_transport_disables_thru_and_requires_release_ack() {
        for observed in [transport(4, false), transport(3, true)] {
            let mut poller = start(vec![note(62, true)]);
            let mut sink = sink();
            poller.tick(&mut sink, transport(3, false), 4, false).unwrap();
            assert_eq!(poller.status().held_notes, 1);
            poller.tick(&mut sink, observed, 4, false).unwrap();
            assert_eq!(poller.status().phase, MidiThruPhase::CleaningUp);
            assert!(poller.status().cleanup_pending);
            assert!(!poller.cleanup_complete(&sink));
            sink.ack = sink.generation.clone();
            assert!(poller.cleanup_complete(&sink));
            assert_eq!(poller.status().phase, MidiThruPhase::Disabled);
            assert_eq!(poller.status().connected_inputs.len(), 0, "input ports released before ACK");
        }
    }

    #[test]
    fn any_session_revision_change_fails_closed_without_implicit_reenable() {
        let mut poller = start(vec![note(60, true)]);
        let mut sink = sink();
        poller.tick(&mut sink, transport(3, false), 4, false).unwrap();
        poller.tick(&mut sink, transport(3, false), 5, false).unwrap();
        assert_eq!(poller.status().phase, MidiThruPhase::CleaningUp);
        assert_eq!(poller.status().held_notes, 0);
        assert!(poller.tick(&mut sink, transport(3, false), 5, false).unwrap() == 0);
    }

    #[test]
    fn input_queue_loss_or_output_saturation_is_fatal_and_cancels_sender() {
        let mut poller = start(vec![note(60, true)]);
        poller.inputs[0].dropped = 2;
        let mut sink = sink();
        assert!(poller.tick(&mut sink, transport(3, false), 4, false).unwrap_err().contains("lost 2"));
        assert_eq!(poller.status().phase, MidiThruPhase::Failed);
        assert_eq!(poller.status().dropped_inputs, 2);
        assert_eq!(sink.generation.get("Fake Synth"), Some(&1));

        let mut poller = start(vec![note(60, true)]);
        let mut sink = sink();
        sink.full = true;
        assert!(poller.tick(&mut sink, transport(3, false), 4, false).unwrap_err().contains("queue full"));
        assert_eq!(poller.status().phase, MidiThruPhase::Failed);
        assert_eq!(sink.generation.get("Fake Synth"), Some(&1));
    }

    #[test]
    fn hot_unplug_and_output_loss_never_silently_continue_thru() {
        let mut poller = start(vec![note(60, true)]);
        poller.inputs[0].present = false;
        let mut sink = sink();
        assert!(poller.tick(&mut sink, transport(3, false), 4, true).is_err());
        assert_eq!(poller.status().phase, MidiThruPhase::Failed);
        assert_eq!(sink.generation.get("Fake Synth"), Some(&1));

        let mut poller = start(vec![note(60, true)]);
        let mut sink = sink();
        sink.present = false;
        assert!(poller.tick(&mut sink, transport(3, false), 4, true).is_err());
        assert_eq!(poller.status().phase, MidiThruPhase::Failed);
    }

    #[test]
    fn bounded_poll_cannot_be_monopolized_by_an_input_flood() {
        let mut poller = start((0..MAX_THRU_EVENTS_PER_BATCH).map(|i| note((i % 128) as u8, true)).collect());
        let mut sink = sink();
        assert_eq!(poller.tick(&mut sink, transport(3, false), 4, false).unwrap(), MAX_EVENTS_PER_POLL);
        assert!(poller.inputs[0].events.len() >= MAX_THRU_EVENTS_PER_BATCH - MAX_EVENTS_PER_POLL);
    }
}
