//! Bounded, opt-in MIDI input-to-output routing on a non-audio control thread.
//!
//! This is an **unconnected building block**, not live monitoring in the DAW:
//! it never discovers or opens an OS device, owns no thread, and cannot send
//! anything without its caller explicitly constructing an enabled router and
//! supplying already-connected ports. A future app bridge must obtain a fresh
//! per-launch MIDI-thru consent, own input streams, and share output workers
//! with ordinary clip playback. Saved track routes grant neither permission.
//!
//! Input device microseconds and output workers' monotonic clocks do not have
//! a measured shared origin. Messages are queued for *now* in arrival order;
//! there is no sample-accurate monitoring or measured hardware latency here.
//! Output worker cancellation is asynchronous: even a cleanup acknowledgement
//! cannot guarantee that disconnected hardware received All Notes Off.

use soundcraft_midi::live::LiveMidiMessage;
use soundcraft_model::{Session, TrackKind};
use thiserror::Error;

/// Limit session-derived routing work for any one incoming message.
pub const MAX_THRU_ROUTES: usize = 64;
/// Limit active input ports and matching output connections.
pub const MAX_THRU_INPUT_PORTS: usize = 16;
pub const MAX_THRU_OUTPUT_PORTS: usize = 8;
/// A lost note-off must fail closed, not grow held-note state without a bound.
pub const MAX_THRU_HELD_NOTES: usize = 1_024;
/// Caller must drain hardware callbacks in bounded control-thread batches.
pub const MAX_THRU_EVENTS_PER_BATCH: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiThruPhase {
    Enabled,
    CleaningUp,
    Disabled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiThruStatus {
    pub phase: MidiThruPhase,
    pub routes: usize,
    pub held_notes: usize,
    pub sent_packets: u64,
    pub input_drops: u64,
    pub cleanup_pending: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MidiThruError {
    #[error("MIDI thru requires an explicit armed, audible input-to-output route")]
    NoRoutes,
    #[error("invalid or unavailable live MIDI thru route: {0}")]
    InvalidRoute(String),
    #[error("unrecognized live MIDI input {0:?}; monitoring disabled")]
    UnknownInput(String),
    #[error("malformed live MIDI event")]
    InvalidMessage,
    #[error("live MIDI thru is not enabled")]
    Disabled,
    #[error("live MIDI thru lost {0} input messages; monitoring disabled")]
    InputOverflow(u64),
    #[error("live MIDI thru exceeded its {0}-note safety limit")]
    TooManyHeldNotes(usize),
    #[error("live MIDI output {port:?} failed: {reason}")]
    Output { port: String, reason: String },
}

/// Control-thread adapter to an *already user-enabled* MIDI output worker.
/// All three operations must be nonblocking. In particular, `cancel_and_release`
/// must bypass a saturated normal event queue and return a generation for ACK.
/// No part of this trait may be invoked from the realtime audio callback.
pub trait MidiThruSink {
    fn try_send_now(&mut self, port: &str, packet: &[u8]) -> Result<(), String>;
    fn cancel_and_release(&mut self, port: &str) -> Result<u64, String>;
    fn cleanup_acknowledged(&self, port: &str, generation: u64) -> bool;
    fn port_connected(&self, port: &str) -> bool;
}

/// Source identity must come from the previously preflighted, currently open
/// input connection, not arbitrary user-typed text or a transient OS index.
#[derive(Debug, Clone, Copy)]
pub struct MidiThruEvent<'a> {
    pub source_port: &'a str,
    pub message: LiveMidiMessage,
}

#[derive(Debug, Clone, Copy)]
struct ThruRoute {
    input: Option<usize>,
    input_channel: Option<u8>,
    output: usize,
    output_channel: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct HeldNote {
    route: usize,
    input: usize,
    input_channel: u8,
    pitch: u8,
    output_channel: u8,
}

#[derive(Debug, Clone, Copy)]
struct PendingRelease {
    output: usize,
    generation: u64,
}

/// No `Default` implementation: monitoring is created only by the caller's
/// explicit `enable` action, and is never restored from a saved session.
pub struct MidiThruRouter {
    inputs: Vec<String>,
    outputs: Vec<String>,
    routes: Vec<ThruRoute>,
    held: Vec<HeldNote>,
    pending: Vec<PendingRelease>,
    transport_generation: u64,
    phase: MidiThruPhase,
    sent_packets: u64,
    input_drops: u64,
    last_error: Option<String>,
}

fn input_key(name: &str) -> String {
    name.chars().filter(|character| !character.is_control() && *character != '=').take(64).collect::<String>().trim().to_owned()
}

fn distinct_ports(ports: &[String], max: usize, kind: &str) -> Result<(), MidiThruError> {
    if ports.is_empty() || ports.len() > max {
        return Err(MidiThruError::InvalidRoute(format!("expected 1..={max} {kind} ports")));
    }
    for (index, name) in ports.iter().enumerate() {
        if name.trim() != name || name.is_empty() || (kind == "input" && input_key(name) != *name) {
            return Err(MidiThruError::InvalidRoute(format!("invalid {kind} port name {name:?}")));
        }
        if ports[..index].iter().any(|other| other.to_lowercase() == name.to_lowercase()) {
            return Err(MidiThruError::InvalidRoute(format!("ambiguous {kind} port name {name:?}")));
        }
    }
    Ok(())
}

fn audible(session: &Session, track: &soundcraft_model::Track, any_solo: bool) -> bool {
    if !track.kind.is_midi()
        || !track.mixer.record_arm
        || track.inactive
        || track.mixer.mute
        || (any_solo && !track.mixer.solo && !track.mixer.solo_safe)
    {
        return false;
    }
    if let Some(vca) = track.mixer.vca
        && session.tracks.iter().any(|other| other.kind == TrackKind::Vca && other.id.0 == vca && other.mixer.mute)
    {
        return false;
    }
    true
}

impl MidiThruRouter {
    /// Preflight only. `enabled_inputs` are globally enabled preference names;
    /// `connected_inputs` and `connected_outputs` must be verified by the caller
    /// against actual open connections *after* an explicit per-launch consent.
    /// This function never talks to an OS or starts a MIDI output worker.
    pub fn enable(
        session: &Session,
        enabled_inputs: &[String],
        connected_inputs: &[String],
        connected_outputs: &[String],
        transport_generation: u64,
    ) -> Result<Self, MidiThruError> {
        distinct_ports(enabled_inputs, MAX_THRU_INPUT_PORTS, "input")?;
        distinct_ports(connected_inputs, MAX_THRU_INPUT_PORTS, "input")?;
        distinct_ports(connected_outputs, MAX_THRU_OUTPUT_PORTS, "output")?;
        if enabled_inputs.len() != connected_inputs.len() || enabled_inputs.iter().any(|name| !connected_inputs.contains(name)) {
            return Err(MidiThruError::InvalidRoute("a globally enabled MIDI input is not connected".into()));
        }
        let any_solo = session.tracks.iter().any(|track| track.mixer.solo && !track.inactive);
        let mut routes = Vec::new();
        for track in &session.tracks {
            if !audible(session, track, any_solo) {
                continue;
            }
            let Some(output_route) = &track.midi_output else { continue };
            if output_route.port.trim() != output_route.port || output_route.port.is_empty() {
                return Err(MidiThruError::InvalidRoute(format!("track {} has an invalid MIDI output name", track.id.0)));
            }
            if output_route.channel.is_some_and(|channel| !(1..=16).contains(&channel))
                || track.midi_input.channel.is_some_and(|channel| !(1..=16).contains(&channel))
            {
                return Err(MidiThruError::InvalidRoute(format!("track {} has an invalid MIDI channel", track.id.0)));
            }
            let Some(output) = connected_outputs.iter().position(|name| name == &output_route.port) else {
                return Err(MidiThruError::InvalidRoute(format!("track {} MIDI output {:?} is not connected", track.id.0, output_route.port)));
            };
            let input = match &track.midi_input.port {
                None => None,
                Some(name) => Some(
                    enabled_inputs
                        .iter()
                        .position(|candidate| candidate == name)
                        .ok_or_else(|| MidiThruError::InvalidRoute(format!("track {} MIDI input {name:?} is not enabled", track.id.0)))?,
                ),
            };
            if routes.len() == MAX_THRU_ROUTES {
                return Err(MidiThruError::InvalidRoute(format!("more than {MAX_THRU_ROUTES} active MIDI thru routes")));
            }
            routes.push(ThruRoute {
                input,
                input_channel: track.midi_input.channel.map(|channel| channel - 1),
                output,
                output_channel: output_route.channel.map(|channel| channel - 1),
            });
        }
        if routes.is_empty() {
            return Err(MidiThruError::NoRoutes);
        }
        Ok(Self {
            inputs: connected_inputs.to_vec(),
            outputs: connected_outputs.to_vec(),
            routes,
            held: Vec::with_capacity(MAX_THRU_HELD_NOTES),
            pending: Vec::with_capacity(MAX_THRU_OUTPUT_PORTS),
            transport_generation,
            phase: MidiThruPhase::Enabled,
            sent_packets: 0,
            input_drops: 0,
            last_error: None,
        })
    }

    pub fn status(&self) -> MidiThruStatus {
        MidiThruStatus {
            phase: self.phase,
            routes: self.routes.len(),
            held_notes: self.held.len(),
            sent_packets: self.sent_packets,
            input_drops: self.input_drops,
            cleanup_pending: !self.pending.is_empty(),
            last_error: self.last_error.clone(),
        }
    }

    fn request_cleanup(&mut self, sink: &mut impl MidiThruSink) {
        self.held.clear();
        self.pending.clear();
        for (output, name) in self.outputs.iter().enumerate() {
            match sink.cancel_and_release(name) {
                Ok(generation) => self.pending.push(PendingRelease { output, generation }),
                Err(reason) => {
                    self.phase = MidiThruPhase::Failed;
                    if self.last_error.is_none() {
                        self.last_error = Some(format!("MIDI output {name:?} could not request note release: {reason}"));
                    }
                }
            }
        }
        if self.pending.is_empty() && self.phase == MidiThruPhase::CleaningUp {
            self.phase = MidiThruPhase::Disabled;
        }
    }

    fn fail_closed(&mut self, sink: &mut impl MidiThruSink, error: MidiThruError) -> MidiThruError {
        self.phase = MidiThruPhase::Failed;
        self.last_error = Some(error.to_string());
        self.request_cleanup(sink);
        error
    }

    fn send_packet(&mut self, sink: &mut impl MidiThruSink, output: usize, bytes: &[u8]) -> Result<(), MidiThruError> {
        let Some(name) = self.outputs.get(output) else { return Err(self.fail_closed(sink, MidiThruError::InvalidMessage)) };
        match sink.try_send_now(name, bytes) {
            Ok(()) => {
                self.sent_packets = self.sent_packets.saturating_add(1);
                Ok(())
            }
            Err(reason) => {
                let error = MidiThruError::Output { port: name.clone(), reason };
                Err(self.fail_closed(sink, error))
            }
        }
    }

    fn note_on(
        &mut self,
        sink: &mut impl MidiThruSink,
        route_index: usize,
        source: usize,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), MidiThruError> {
        let route = self.routes[route_index];
        let output_channel = route.output_channel.unwrap_or(channel);
        if self.held.iter().any(|held| held.route == route_index && held.input == source && held.input_channel == channel && held.pitch == pitch) {
            // Repeated NoteOn without an intervening NoteOff must never grow
            // an unbounded refcount or make a single NoteOff insufficient.
            return Ok(());
        }
        if self.held.len() == MAX_THRU_HELD_NOTES {
            return Err(self.fail_closed(sink, MidiThruError::TooManyHeldNotes(MAX_THRU_HELD_NOTES)));
        }
        // MIDI wire format has no source identifier. Merge coincident held
        // notes on the same physical destination until the *last* source ends,
        // otherwise one track's NoteOff can silence another track's note.
        let already_sounding = self
            .held
            .iter()
            .any(|held| self.routes[held.route].output == route.output && held.output_channel == output_channel && held.pitch == pitch);
        if !already_sounding {
            self.send_packet(sink, route.output, &[0x90 | output_channel, pitch, velocity])?;
        }
        self.held.push(HeldNote { route: route_index, input: source, input_channel: channel, pitch, output_channel });
        Ok(())
    }

    fn note_off(
        &mut self,
        sink: &mut impl MidiThruSink,
        route_index: usize,
        source: usize,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), MidiThruError> {
        let Some(index) = self
            .held
            .iter()
            .position(|held| held.route == route_index && held.input == source && held.input_channel == channel && held.pitch == pitch)
        else {
            return Ok(()); // Never turn off another input/track's held note.
        };
        let note = self.held.swap_remove(index);
        let output = self.routes[route_index].output;
        if !self.held.iter().any(|held| self.routes[held.route].output == output && held.output_channel == note.output_channel && held.pitch == pitch)
        {
            self.send_packet(sink, output, &[0x80 | note.output_channel, pitch, velocity])?;
        }
        Ok(())
    }

    fn dispatch_one(&mut self, event: MidiThruEvent<'_>, sink: &mut impl MidiThruSink) -> Result<(), MidiThruError> {
        let Some(source) = self.inputs.iter().position(|name| name == event.source_port) else {
            return Err(self.fail_closed(sink, MidiThruError::UnknownInput(event.source_port.to_owned())));
        };
        let Some((channel, message)) = encode(event.message) else {
            return Err(self.fail_closed(sink, MidiThruError::InvalidMessage));
        };
        let Some(channel) = channel else { return Ok(()) }; // No MIDI clock/transport/system reset thru.
        for index in 0..self.routes.len() {
            let route = self.routes[index];
            if route.input.is_some_and(|input| input != source) || route.input_channel.is_some_and(|filtered| filtered != channel) {
                continue;
            }
            let out_channel = route.output_channel.unwrap_or(channel);
            match message {
                Encoded::NoteOn(pitch, velocity) => self.note_on(sink, index, source, channel, pitch, velocity)?,
                Encoded::NoteOff(pitch, velocity) => self.note_off(sink, index, source, channel, pitch, velocity)?,
                Encoded::Voice(mut bytes, len) => {
                    bytes[0] = (bytes[0] & 0xf0) | out_channel;
                    self.send_packet(sink, route.output, &bytes[..len])?;
                    // Hardware channel-mode panic overrides all source/track
                    // claims on the same physical port and channel.
                    if bytes[0] & 0xf0 == 0xb0 && (bytes[1] == 120 || bytes[1] == 123) {
                        self.held.retain(|held| self.routes[held.route].output != route.output || held.output_channel != out_channel);
                    }
                }
                Encoded::Realtime => {}
            }
        }
        Ok(())
    }

    /// Called only on a non-audio thread, with a finite batch drained from an
    /// already bounded MIDI input queue. Sends are nonblocking. A partial batch
    /// followed by a failure triggers generation-based cancellation and held
    /// note release for **every** connected output, not a silent partial state.
    pub fn dispatch_batch(&mut self, events: &[MidiThruEvent<'_>], sink: &mut impl MidiThruSink) -> Result<(), MidiThruError> {
        if self.phase != MidiThruPhase::Enabled {
            return Err(MidiThruError::Disabled);
        }
        if events.len() > MAX_THRU_EVENTS_PER_BATCH {
            let dropped = (events.len() - MAX_THRU_EVENTS_PER_BATCH) as u64;
            self.input_drops = self.input_drops.saturating_add(dropped);
            return Err(self.fail_closed(sink, MidiThruError::InputOverflow(dropped)));
        }
        for event in events {
            self.dispatch_one(*event, sink)?;
        }
        Ok(())
    }

    /// Any input callback queue overflow may have lost a NoteOff: fail closed.
    pub fn report_input_loss(&mut self, lost: u64, sink: &mut impl MidiThruSink) -> Result<(), MidiThruError> {
        if self.phase != MidiThruPhase::Enabled {
            return Err(MidiThruError::Disabled);
        }
        if lost == 0 {
            return Ok(());
        }
        self.input_drops = self.input_drops.saturating_add(lost);
        Err(self.fail_closed(sink, MidiThruError::InputOverflow(lost)))
    }

    /// Caller periodically verifies its actual open input feeds and output
    /// workers. A disappearance fails closed even when no notes are arriving.
    /// This function performs no discovery or OS I/O itself.
    pub fn verify_connections(&mut self, connected_inputs: &[String], sink: &mut impl MidiThruSink) -> Result<(), MidiThruError> {
        if self.phase != MidiThruPhase::Enabled {
            return Err(MidiThruError::Disabled);
        }
        if let Some(missing) = self.inputs.iter().find(|name| !connected_inputs.contains(name)) {
            let error = MidiThruError::UnknownInput(missing.clone());
            return Err(self.fail_closed(sink, error));
        }
        if let Some(missing) = self.outputs.iter().find(|name| !sink.port_connected(name)) {
            let error = MidiThruError::Output { port: missing.clone(), reason: "disconnected".into() };
            return Err(self.fail_closed(sink, error));
        }
        Ok(())
    }

    /// Stop, Seek, Loop or session/tempo/speed replacement invalidates held
    /// notes. A changed observer generation stops thru until **new consent**;
    /// it is never automatically re-enabled after transport discontinuity.
    pub fn observe_transport_generation(&mut self, generation: u64, sink: &mut impl MidiThruSink) {
        if generation != self.transport_generation {
            self.transport_generation = generation;
            self.stop(sink);
        }
    }

    pub fn stop(&mut self, sink: &mut impl MidiThruSink) {
        if self.phase == MidiThruPhase::Enabled {
            self.phase = MidiThruPhase::CleaningUp;
            self.request_cleanup(sink);
        }
    }

    /// Read only worker-generation acknowledgements. When a device disappears
    /// during cleanup, preserve `Failed` instead of claiming notes were freed.
    pub fn poll_cleanup(&mut self, sink: &impl MidiThruSink) {
        if self.pending.is_empty() {
            return;
        }
        let mut failed_port = None;
        self.pending.retain(|release| {
            let port = &self.outputs[release.output];
            if !sink.port_connected(port) {
                failed_port = Some(port.clone());
                return false;
            }
            !sink.cleanup_acknowledged(port, release.generation)
        });
        if let Some(port) = failed_port {
            self.phase = MidiThruPhase::Failed;
            self.last_error = Some(format!("MIDI output {port:?} disconnected before note release could be confirmed"));
        }
        if self.pending.is_empty() && self.phase == MidiThruPhase::CleaningUp {
            self.phase = MidiThruPhase::Disabled;
        }
    }
}

#[derive(Clone, Copy)]
enum Encoded {
    NoteOn(u8, u8),
    NoteOff(u8, u8),
    Voice([u8; 3], usize),
    Realtime,
}

fn encode(message: LiveMidiMessage) -> Option<(Option<u8>, Encoded)> {
    let (channel, encoded) = match message {
        LiveMidiMessage::NoteOn { channel, pitch, velocity } if pitch < 128 && velocity < 128 => {
            (channel, if velocity == 0 { Encoded::NoteOff(pitch, 0) } else { Encoded::NoteOn(pitch, velocity) })
        }
        LiveMidiMessage::NoteOff { channel, pitch, velocity } if pitch < 128 && velocity < 128 => (channel, Encoded::NoteOff(pitch, velocity)),
        LiveMidiMessage::PolyPressure { channel, pitch, pressure } if pitch < 128 && pressure < 128 => {
            (channel, Encoded::Voice([0xa0 | channel, pitch, pressure], 3))
        }
        LiveMidiMessage::ControlChange { channel, controller, value } if controller < 128 && value < 128 => {
            (channel, Encoded::Voice([0xb0 | channel, controller, value], 3))
        }
        LiveMidiMessage::ProgramChange { channel, program } if program < 128 => (channel, Encoded::Voice([0xc0 | channel, program, 0], 2)),
        LiveMidiMessage::ChannelPressure { channel, pressure } if pressure < 128 => (channel, Encoded::Voice([0xd0 | channel, pressure, 0], 2)),
        LiveMidiMessage::PitchBend { channel, value } if (-8_192..=8_191).contains(&value) => {
            let unsigned = i32::from(value) + 8_192;
            (channel, Encoded::Voice([0xe0 | channel, (unsigned & 0x7f) as u8, ((unsigned >> 7) & 0x7f) as u8], 3))
        }
        LiveMidiMessage::Realtime(0xf8 | 0xfa | 0xfb | 0xfc | 0xfe | 0xff) => {
            return Some((None, Encoded::Realtime));
        }
        _ => return None,
    };
    (channel < 16).then_some((Some(channel), encoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, MidiInputRoute, MidiOutputRoute, TrackKind};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct FakeSink {
        micros: u64,
        packets: Vec<(String, u64, Vec<u8>)>,
        release_requested: BTreeMap<String, u64>,
        acknowledged: BTreeMap<String, u64>,
        disconnected: BTreeSet<String>,
        fail_next_send: bool,
        fail_next_cancel: bool,
    }

    impl MidiThruSink for FakeSink {
        fn try_send_now(&mut self, port: &str, packet: &[u8]) -> Result<(), String> {
            if self.disconnected.contains(port) {
                return Err("disconnected".into());
            }
            if std::mem::take(&mut self.fail_next_send) {
                return Err("queue full".into());
            }
            self.packets.push((port.into(), self.micros, packet.to_vec()));
            Ok(())
        }

        fn cancel_and_release(&mut self, port: &str) -> Result<u64, String> {
            if self.disconnected.contains(port) || std::mem::take(&mut self.fail_next_cancel) {
                return Err("release unavailable".into());
            }
            let generation = self.release_requested.get(port).copied().unwrap_or(0) + 1;
            self.release_requested.insert(port.into(), generation);
            Ok(generation)
        }

        fn cleanup_acknowledged(&self, port: &str, generation: u64) -> bool {
            self.acknowledged.get(port).copied().unwrap_or(0) >= generation
        }

        fn port_connected(&self, port: &str) -> bool {
            !self.disconnected.contains(port)
        }
    }

    fn session() -> Session {
        let mut session = Session::default();
        let first = session.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("A"));
        let second = session.add_track(TrackKind::Midi, ChannelFormat::Mono, Some("B"));
        for (id, input, channel, output, out_channel) in [(first, "Keyboard A", 1, "Synth A", 6), (second, "Keyboard B", 16, "Synth B", 1)] {
            let track = session.track_mut(id).expect("track exists");
            track.mixer.record_arm = true;
            track.midi_input = MidiInputRoute { port: Some(input.into()), channel: Some(channel) };
            track.midi_output = Some(MidiOutputRoute { port: output.into(), channel: Some(out_channel) });
        }
        session
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|name| (*name).into()).collect()
    }

    fn enable(s: &Session) -> MidiThruRouter {
        MidiThruRouter::enable(s, &names(&["Keyboard A", "Keyboard B"]), &names(&["Keyboard A", "Keyboard B"]), &names(&["Synth A", "Synth B"]), 7)
            .expect("explicit valid routed setup")
    }

    fn event(source: &str, message: LiveMidiMessage) -> MidiThruEvent<'_> {
        MidiThruEvent { source_port: source, message }
    }

    fn on(channel: u8, pitch: u8) -> LiveMidiMessage {
        LiveMidiMessage::NoteOn { channel, pitch, velocity: 100 }
    }

    fn off(channel: u8, pitch: u8) -> LiveMidiMessage {
        LiveMidiMessage::NoteOff { channel, pitch, velocity: 64 }
    }

    #[test]
    fn explicit_route_and_channel_preflight_rejects_stale_duplicate_and_unarmed() {
        let s = session();
        assert!(matches!(
            MidiThruRouter::enable(&s, &names(&["Keyboard A"]), &names(&["Keyboard A"]), &names(&["Synth A", "Synth B"]), 0),
            Err(MidiThruError::InvalidRoute(_))
        ));
        assert!(matches!(
            MidiThruRouter::enable(
                &s,
                &names(&["Keyboard A", "Keyboard B"]),
                &names(&["Keyboard A", "Keyboard B"]),
                &names(&["Synth A", "synth a"]),
                0
            ),
            Err(MidiThruError::InvalidRoute(_))
        ));
        let mut silent = s.clone();
        silent.tracks.iter_mut().for_each(|track| track.mixer.record_arm = false);
        assert!(matches!(
            MidiThruRouter::enable(
                &silent,
                &names(&["Keyboard A", "Keyboard B"]),
                &names(&["Keyboard A", "Keyboard B"]),
                &names(&["Synth A", "Synth B"]),
                0
            ),
            Err(MidiThruError::NoRoutes)
        ));
    }

    #[test]
    fn routes_source_and_channel_with_best_effort_immediate_send_and_accurate_short_packets() {
        let mut thru = enable(&session());
        let mut sink = FakeSink { micros: 12_345, ..Default::default() };
        thru.dispatch_batch(
            &[
                event("Keyboard A", on(0, 60)),
                event("Keyboard B", on(0, 59)), // Wrong B channel: must not leak to A.
                event("Keyboard B", on(15, 65)),
                event("Keyboard A", LiveMidiMessage::ProgramChange { channel: 0, program: 7 }),
                event("Keyboard A", LiveMidiMessage::ControlChange { channel: 0, controller: 1, value: 11 }),
                event("Keyboard A", LiveMidiMessage::PitchBend { channel: 0, value: -100 }),
                event("Keyboard A", LiveMidiMessage::Realtime(0xff)), // Never forward device reset/system clock.
                event("Keyboard A", off(0, 60)),
                event("Keyboard B", off(15, 65)),
            ],
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.packets.iter().map(|(port, time, bytes)| (port.as_str(), *time, bytes.as_slice())).collect::<Vec<_>>(),
            vec![
                ("Synth A", 12_345, &[0x95, 60, 100][..]),
                ("Synth B", 12_345, &[0x90, 65, 100]),
                ("Synth A", 12_345, &[0xc5, 7]),
                ("Synth A", 12_345, &[0xb5, 1, 11]),
                ("Synth A", 12_345, &[0xe5, 28, 63]),
                ("Synth A", 12_345, &[0x85, 60, 64]),
                ("Synth B", 12_345, &[0x80, 65, 64]),
            ]
        );
        assert_eq!(thru.status().held_notes, 0);
    }

    #[test]
    fn coincident_notes_reference_count_by_physical_port_channel_pitch() {
        let mut s = session();
        let second = s.tracks.get_mut(1).unwrap();
        second.midi_input = MidiInputRoute { port: Some("Keyboard A".into()), channel: Some(1) };
        second.midi_output = Some(MidiOutputRoute { port: "Synth A".into(), channel: Some(6) });
        let mut thru = enable(&s);
        let mut sink = FakeSink::default();
        thru.dispatch_batch(&[event("Keyboard A", on(0, 60))], &mut sink).unwrap();
        assert_eq!(thru.status().held_notes, 2);
        assert_eq!(sink.packets.len(), 1, "only one physical NoteOn despite two routes");
        thru.dispatch_batch(&[event("Keyboard B", off(0, 60))], &mut sink).unwrap();
        assert_eq!(thru.status().held_notes, 2, "different source must not release keys");
        thru.dispatch_batch(&[event("Keyboard A", off(0, 60))], &mut sink).unwrap();
        assert_eq!(sink.packets.len(), 2, "one physical NoteOff when both routes finish");
        assert_eq!(sink.packets.last().unwrap().2, vec![0x85, 60, 64]);
    }

    #[test]
    fn zero_velocity_note_off_and_channel_mode_panic_do_not_leave_held_state() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        thru.dispatch_batch(
            &[event("Keyboard A", on(0, 60)), event("Keyboard A", LiveMidiMessage::NoteOn { channel: 0, pitch: 60, velocity: 0 })],
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.packets.last().unwrap().2, vec![0x85, 60, 0]);
        thru.dispatch_batch(
            &[event("Keyboard A", on(0, 62)), event("Keyboard A", LiveMidiMessage::ControlChange { channel: 0, controller: 123, value: 0 })],
            &mut sink,
        )
        .unwrap();
        assert_eq!(thru.status().held_notes, 0);
    }

    #[test]
    fn stop_seek_loop_generation_requests_priority_release_until_ack() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        thru.dispatch_batch(&[event("Keyboard A", on(0, 60))], &mut sink).unwrap();
        thru.observe_transport_generation(7, &mut sink);
        assert_eq!(thru.status().phase, MidiThruPhase::Enabled);
        thru.observe_transport_generation(8, &mut sink);
        assert_eq!(thru.status().phase, MidiThruPhase::CleaningUp);
        assert!(thru.status().cleanup_pending);
        assert_eq!(thru.status().held_notes, 0);
        assert_eq!(sink.release_requested.len(), 2);
        assert_eq!(thru.dispatch_batch(&[event("Keyboard A", on(0, 61))], &mut sink), Err(MidiThruError::Disabled));
        thru.poll_cleanup(&sink);
        assert_eq!(thru.status().phase, MidiThruPhase::CleaningUp, "release requires ACK, not just a queued generation bump");
        sink.acknowledged = sink.release_requested.clone();
        thru.poll_cleanup(&sink);
        assert_eq!(thru.status().phase, MidiThruPhase::Disabled);
    }

    #[test]
    fn queue_full_and_input_loss_fail_closed_and_cancel_all_outputs() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        thru.dispatch_batch(&[event("Keyboard A", on(0, 60))], &mut sink).unwrap();
        sink.fail_next_send = true;
        assert!(matches!(thru.dispatch_batch(&[event("Keyboard B", on(15, 65))], &mut sink), Err(MidiThruError::Output { .. })));
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert_eq!(thru.status().held_notes, 0);
        assert_eq!(sink.release_requested.len(), 2);

        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        assert_eq!(thru.report_input_loss(3, &mut sink), Err(MidiThruError::InputOverflow(3)));
        assert_eq!(thru.status().input_drops, 3);
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert_eq!(sink.release_requested.len(), 2);
    }

    #[test]
    fn unknown_port_and_bounded_batch_fail_closed_before_later_packets() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        assert_eq!(
            thru.dispatch_batch(&[event("Unplugged Keyboard", on(0, 60))], &mut sink),
            Err(MidiThruError::UnknownInput("Unplugged Keyboard".into()))
        );
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert!(sink.packets.is_empty());

        let mut thru = enable(&session());
        let events = vec![event("Keyboard A", on(0, 60)); MAX_THRU_EVENTS_PER_BATCH + 1];
        let mut sink = FakeSink::default();
        assert_eq!(thru.dispatch_batch(&events, &mut sink), Err(MidiThruError::InputOverflow(1)));
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert!(sink.packets.is_empty());
    }

    #[test]
    fn cleanup_unplug_or_failed_release_remains_failed_not_falsely_acknowledged() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        thru.stop(&mut sink);
        sink.disconnected.insert("Synth A".into());
        thru.poll_cleanup(&sink);
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert!(thru.status().cleanup_pending, "another output still needs its independent ACK");
        sink.acknowledged = sink.release_requested.clone();
        thru.poll_cleanup(&sink);
        assert!(!thru.status().cleanup_pending);

        let mut thru = enable(&session());
        let mut sink = FakeSink { fail_next_cancel: true, ..Default::default() };
        thru.stop(&mut sink);
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert_eq!(sink.release_requested.len(), 1, "other port still receives priority cancel even if first fails");
    }

    #[test]
    fn held_note_safety_limit_always_cancels_outputs_instead_of_allocating_unboundedly() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        // Seed the exact hard ceiling using a synthetic device backlog; no real
        // hardware can reach these fabricated same-source distinct key claims.
        thru.held = (0..MAX_THRU_HELD_NOTES)
            .map(|index| HeldNote { route: 0, input: 0, input_channel: 0, pitch: (index % 128) as u8, output_channel: 5 })
            .collect();
        assert_eq!(thru.dispatch_batch(&[event("Keyboard A", on(0, 125))], &mut sink), Ok(()), "already-held pitch is idempotent");
        assert_eq!(thru.dispatch_batch(&[event("Keyboard B", on(15, 65))], &mut sink), Err(MidiThruError::TooManyHeldNotes(MAX_THRU_HELD_NOTES)));
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
        assert_eq!(thru.status().held_notes, 0);
        assert_eq!(sink.release_requested.len(), 2);
    }

    #[test]
    fn hot_unplug_with_silent_held_note_fails_closed_without_waiting_for_an_event() {
        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        thru.dispatch_batch(&[event("Keyboard A", on(0, 60))], &mut sink).unwrap();
        assert!(matches!(thru.verify_connections(&names(&["Keyboard B"]), &mut sink), Err(MidiThruError::UnknownInput(_))));
        assert_eq!(thru.status().held_notes, 0);
        assert_eq!(sink.release_requested.len(), 2);

        let mut thru = enable(&session());
        let mut sink = FakeSink::default();
        sink.disconnected.insert("Synth A".into());
        assert!(matches!(thru.verify_connections(&names(&["Keyboard A", "Keyboard B"]), &mut sink), Err(MidiThruError::Output { .. })));
        assert_eq!(thru.status().phase, MidiThruPhase::Failed);
    }
}
