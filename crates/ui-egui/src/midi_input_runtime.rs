//! Native MIDI recording resources owned by the desktop UI/control thread.
//!
//! A midir callback only decodes and enqueues into a fixed-capacity queue. This module drains
//! queues on the UI thread, normalizes each port's arbitrary timestamp epoch, and hands the
//! resulting events to the engine's undoable recording bridge. No plugin/audio callback work
//! occurs here. Timestamping is approximate: first-event calibration uses the transport sample
//! observed at the next UI poll, so timing can lag by a polling interval, not sample accuracy.

use soundcraft_engine::{
    Engine,
    midi_record::{MAX_MIDI_DRAIN_PER_POLL, MidiRecordBridge},
};
use soundcraft_midi::{
    device::{self, MidiCaptureQueue, MidiInputPortInfo, MidiInputStream},
    live::{LiveMidiMessage, TimedMidiMessage},
};
use soundcraft_time::Samples;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Keep multiple input-device selections bounded, even for hostile session preferences.
const MAX_OPEN_PORTS: usize = 16;
/// At stop, preserve up to 16 control-thread polls before closing each bounded hardware queue.
const MAX_STOP_DRAINS: usize = 16;
const PORT_RESCAN_INTERVAL: Duration = Duration::from_secs(1);
const FILTER_KEYS: [&str; 8] = ["notes", "pitch_bend", "mono_aftertouch", "poly_aftertouch", "program", "cc", "sysex", "realtime"];

#[derive(Debug, Clone, Copy)]
struct MidiInputFilter([bool; FILTER_KEYS.len()]);

impl MidiInputFilter {
    fn from_engine(engine: &Engine) -> Self {
        Self(FILTER_KEYS.map(|name| engine.session().edit.flag(&format!("midi.filter.{name}"))))
    }

    fn rejects(self, message: LiveMidiMessage) -> bool {
        let index = match message {
            LiveMidiMessage::NoteOn { .. } | LiveMidiMessage::NoteOff { .. } => 0,
            LiveMidiMessage::PitchBend { .. } => 1,
            LiveMidiMessage::ChannelPressure { .. } => 2,
            LiveMidiMessage::PolyPressure { .. } => 3,
            LiveMidiMessage::ProgramChange { .. } => 4,
            LiveMidiMessage::ControlChange { .. } => 5,
            LiveMidiMessage::Realtime(_) => 7,
        };
        self.0[index]
    }
}

/// Read-only connection information for Setup > Playback Engine and other UI panels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MidiInputStatus {
    pub enabled_ports: Vec<String>,
    pub connected_ports: Vec<String>,
    pub recording: bool,
    pub dropped_events: u64,
    pub truncated_packets: u64,
    pub error: Option<String>,
}

/// `setup.midi_input_devices` stores normalized names as `midi.input.*` session flags.
pub(crate) fn selected_names(engine: &Engine) -> Vec<String> {
    engine.session().edit.flags.iter().filter_map(|flag| flag.strip_prefix("midi.input.")).take(64).map(ToString::to_string).collect()
}

fn port_key(name: &str) -> String {
    name.chars().filter(|c| !c.is_control() && *c != '=').take(64).collect::<String>().trim().to_owned()
}

/// Resolve preference names against a fresh native scan. A port index is *never* persisted:
/// unplugging or adding a device can reorder the OS-provided list between sessions.
fn matching_ports(selected: &[String], available: &[MidiInputPortInfo]) -> Result<Vec<MidiInputPortInfo>, String> {
    if selected.is_empty() {
        return Err("no MIDI input selected; enable a port under Setup > Playback Engine > MIDI Inputs".into());
    }
    let mut matching = Vec::new();
    for selected_name in selected {
        let ports: Vec<_> = available.iter().filter(|port| port_key(&port.name) == *selected_name).cloned().collect();
        if ports.is_empty() {
            return Err(format!("selected MIDI input '{selected_name}' is unavailable; rescan MIDI Inputs in Setup > Playback Engine"));
        }
        if ports.len() > 1 {
            return Err(format!(
                "MIDI input '{selected_name}' is ambiguous: multiple ports share this name; disambiguate the devices before recording"
            ));
        }
        matching.extend(ports);
        if matching.len() > MAX_OPEN_PORTS {
            return Err(format!("at most {MAX_OPEN_PORTS} simultaneous MIDI input ports can be recorded"));
        }
    }
    Ok(matching)
}

fn validate_track_routes(engine: &Engine, selected: &[String]) -> Result<(), String> {
    for track in engine.session().tracks.iter().filter(|track| track.kind.is_midi() && track.mixer.record_arm && !track.inactive) {
        let route = &track.midi_input;
        if route.channel.is_some_and(|channel| !(1..=16).contains(&channel)) {
            return Err(format!("track '{}' has an invalid MIDI channel; choose All or 1..16", track.name));
        }
        if let Some(port) = &route.port {
            if port.is_empty() || port_key(port) != *port {
                return Err(format!("track '{}' has an invalid saved MIDI port name", track.name));
            }
            if !selected.contains(port) {
                return Err(format!("track '{}' uses MIDI input '{port}', which is not enabled under Setup > Playback Engine", track.name));
            }
        }
    }
    Ok(())
}

enum InputSource {
    Native(MidiInputStream),
    #[cfg(test)]
    Fake {
        queue: MidiCaptureQueue,
        present: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
}

impl InputSource {
    fn queue(&self) -> &MidiCaptureQueue {
        match self {
            Self::Native(stream) => stream.capture_queue(),
            #[cfg(test)]
            Self::Fake { queue, .. } => queue,
        }
    }

    fn still_present(&self) -> Result<bool, String> {
        match self {
            Self::Native(stream) => stream.port_still_present().map_err(|e| e.to_string()),
            #[cfg(test)]
            Self::Fake { present, .. } => Ok(present.load(std::sync::atomic::Ordering::Relaxed)),
        }
    }
}

struct MidiPortFeed {
    /// Full OS name for connection/status reporting.
    name: String,
    /// Stable normalized name for matching the saved track route.
    key: String,
    source: InputSource,
    /// First event timestamp on this source, and the transport-relative microseconds when
    /// it was first observed. Each input connection has its own unspecified timestamp epoch.
    anchor: Option<(u64, u64)>,
}

/// Open input streams before enabling the recording transport, so an unavailable selected
/// keyboard cannot cause half an audio/MIDI pass to be armed.
pub(crate) struct MidiInputSources {
    feeds: Vec<MidiPortFeed>,
}

impl MidiInputSources {
    pub(crate) fn open_selected(engine: &Engine) -> Result<Self, String> {
        let selected = selected_names(engine);
        if selected.is_empty() {
            return Err("no MIDI input selected; enable a port under Setup > Playback Engine > MIDI Inputs".into());
        }
        validate_track_routes(engine, &selected)?;
        let available = device::input_ports().map_err(|e| format!("scan MIDI inputs: {e}"))?;
        let matching = matching_ports(&selected, &available)?;
        let feeds = matching
            .into_iter()
            .map(|port| {
                let stream = device::open_input(port.index, device::MAX_MIDI_QUEUE_EVENTS).map_err(|e| format!("open '{}': {e}", port.name))?;
                // A device can disappear or change index between scan and open.
                if port_key(&stream.port().name) != port_key(&port.name) {
                    return Err(format!("MIDI input '{}' changed during opening; rescan and retry", port.name));
                }
                Ok(MidiPortFeed { name: port.name.clone(), key: port_key(&port.name), source: InputSource::Native(stream), anchor: None })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self { feeds })
    }

    #[cfg(test)]
    pub(crate) fn fake(name: &str, queue: MidiCaptureQueue, present: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { feeds: vec![MidiPortFeed { name: name.to_owned(), key: port_key(name), source: InputSource::Fake { queue, present }, anchor: None }] }
    }

    #[cfg(test)]
    pub(crate) fn fake_many(feeds: Vec<(&str, MidiCaptureQueue)>) -> Self {
        let feeds = feeds
            .into_iter()
            .map(|(name, queue)| MidiPortFeed {
                name: name.to_owned(),
                key: port_key(name),
                source: InputSource::Fake { queue, present: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)) },
                anchor: None,
            })
            .collect();
        Self { feeds }
    }
}

#[derive(Clone, Copy)]
struct SourcedMidiEvent {
    source_index: usize,
    event: TimedMidiMessage,
}

pub(crate) struct MidiInputRuntime {
    bridge: MidiRecordBridge,
    feeds: Vec<MidiPortFeed>,
    input_filter: MidiInputFilter,
    sample_rate_hz: u64,
    last_emitted_micros: u64,
    last_rescan: Instant,
    batch: Vec<SourcedMidiEvent>,
}

impl MidiInputRuntime {
    pub(crate) fn begin(engine: &Engine, sources: MidiInputSources) -> Result<Self, String> {
        if sources.feeds.is_empty() || sources.feeds.len() > MAX_OPEN_PORTS {
            return Err("MIDI recording requires 1 to 16 selected input ports".into());
        }
        let selected = selected_names(engine);
        validate_track_routes(engine, &selected)?;
        let mut opened = HashSet::new();
        for feed in &sources.feeds {
            if feed.key.is_empty() || !opened.insert(feed.key.as_str()) {
                return Err(format!("MIDI input '{}' is ambiguous or has a blank name", feed.name));
            }
            if !selected.contains(&feed.key) {
                return Err(format!("MIDI input '{}' is not globally enabled", feed.name));
            }
        }
        for track in engine.session().tracks.iter().filter(|track| track.kind.is_midi() && track.mixer.record_arm && !track.inactive) {
            if let Some(port) = &track.midi_input.port
                && !opened.contains(port.as_str())
            {
                return Err(format!("track '{}' MIDI input '{port}' is not connected", track.name));
            }
        }
        let mut bridge = MidiRecordBridge::begin(engine).map_err(|e| e.to_string())?;
        bridge.set_anchor(0, bridge.start_sample()).map_err(|e| e.to_string())?;
        Ok(Self {
            bridge,
            feeds: sources.feeds,
            input_filter: MidiInputFilter::from_engine(engine),
            sample_rate_hz: u64::from(engine.session().sample_rate.hz()),
            last_emitted_micros: 0,
            last_rescan: Instant::now(),
            batch: Vec::with_capacity(MAX_MIDI_DRAIN_PER_POLL),
        })
    }

    pub(crate) fn connected_ports(&self) -> Vec<String> {
        self.feeds.iter().map(|feed| feed.name.clone()).collect()
    }

    pub(crate) fn input_loss(&self) -> (u64, u64) {
        self.bridge.input_loss()
    }

    fn observed_micros(&self, transport_sample: Samples) -> u64 {
        let elapsed_samples = transport_sample.saturating_sub(self.bridge.start_sample()).max(0) as u128;
        let elapsed_micros = elapsed_samples.saturating_mul(1_000_000) / u128::from(self.sample_rate_hz.max(1));
        u64::try_from(elapsed_micros).unwrap_or(u64::MAX)
    }

    /// Nonblocking drain from each selected port. Events from different port-specific timestamp
    /// epochs are aligned on first receipt, sorted within this poll and monotonically clamped
    /// across polls; the initial poll delay remains an explicit timing accuracy limitation.
    fn drain(&mut self, at_sample: Samples, limit: usize, check_presence: bool) -> Result<usize, String> {
        let base_micros = self.observed_micros(at_sample);
        self.batch.clear();
        for feed in &self.feeds {
            self.bridge.add_input_loss(feed.source.queue().take_dropped_events(), feed.source.queue().take_truncated_packets());
        }
        let mut consumed = 0;
        while consumed < limit {
            let mut received = false;
            for (source_index, feed) in self.feeds.iter_mut().enumerate() {
                if consumed >= limit {
                    break;
                }
                let Some(mut event) = feed.source.queue().try_recv() else { continue };
                received = true;
                consumed += 1;
                if self.input_filter.rejects(event.message) {
                    continue;
                }
                let (origin, at_micros) = *feed.anchor.get_or_insert((event.timestamp_micros, base_micros));
                if event.timestamp_micros < origin {
                    return Err(format!("MIDI input '{}' timestamp moved backwards", feed.name));
                }
                event.timestamp_micros = at_micros.saturating_add(event.timestamp_micros - origin);
                self.batch.push(SourcedMidiEvent { source_index, event });
            }
            if !received {
                break;
            }
        }
        self.batch.sort_by_key(|sourced| sourced.event.timestamp_micros);
        for sourced in &mut self.batch {
            sourced.event.timestamp_micros = sourced.event.timestamp_micros.max(self.last_emitted_micros);
            let name = &self.feeds.get(sourced.source_index).ok_or_else(|| "MIDI input source disappeared during queue drain".to_owned())?.key;
            self.bridge.record_sourced_event(name, sourced.event).map_err(|e| e.to_string())?;
            self.last_emitted_micros = sourced.event.timestamp_micros;
        }
        if check_presence && self.last_rescan.elapsed() >= PORT_RESCAN_INTERVAL {
            self.last_rescan = Instant::now();
            for feed in &self.feeds {
                if !feed.source.still_present()? {
                    return Err(format!("MIDI input '{}' disconnected; the partial take will be saved", feed.name));
                }
            }
        }
        Ok(consumed)
    }

    pub(crate) fn poll(&mut self, at_sample: Samples) -> Result<usize, String> {
        self.drain(at_sample, MAX_MIDI_DRAIN_PER_POLL, true)
    }

    /// Drain pending callbacks once more at Stop, then close all hardware connections before
    /// committing. A pathological backlog beyond this limit is explicitly marked as lost.
    pub(crate) fn finish(&mut self, engine: &mut Engine, at_sample: Samples) -> Result<serde_json::Value, String> {
        let mut drain_warning = None;
        for _ in 0..MAX_STOP_DRAINS {
            if self.feeds.iter().all(|feed| feed.source.queue().pending() == 0) {
                break;
            }
            if let Err(err) = self.drain(at_sample, MAX_MIDI_DRAIN_PER_POLL, false) {
                drain_warning = Some(err);
                break;
            }
        }
        for feed in &self.feeds {
            self.bridge.add_input_loss(feed.source.queue().take_dropped_events(), feed.source.queue().take_truncated_packets());
        }
        let discarded = self.feeds.iter().map(|feed| feed.source.queue().pending() as u64).sum();
        self.bridge.add_input_loss(discarded, 0);
        self.feeds.clear(); // Drop midir connections before session changes or a retry.
        let mut result = self.bridge.finish(engine, at_sample).map_err(|e| e.to_string())?;
        if let Some(warning) = drain_warning {
            engine.message(format!("MIDI input interrupted while stopping; the partial take was kept: {warning}"));
            if let serde_json::Value::Object(ref mut map) = result {
                map.insert("capture_warning".into(), serde_json::Value::String(warning));
            }
        }
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn force_presence_check(&mut self) {
        self.last_rescan = Instant::now() - PORT_RESCAN_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Services, SoundApp};
    use serde_json::json;
    use soundcraft_model::{ChannelFormat, ClipContent, TrackId, TrackKind};
    use soundcraft_time::Range;
    use std::sync::{Arc, atomic::AtomicBool};

    fn armed_midi_app() -> (SoundApp, TrackId) {
        let mut app = SoundApp::new(Engine::default(), None, Services::default());
        let id = app.engine.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        app.engine.execute("mix.record_arm", &json!({"tracks": [id.0], "value": true})).unwrap();
        app.engine.session_mut().edit.selection = Range::point(48_000);
        app.engine.execute("setup.midi_input_devices", &json!({"enable": ["Fake Keyboard"]})).unwrap();
        (app, id)
    }

    fn request_record(app: &mut SoundApp) {
        app.run("transport.record", json!({})).unwrap();
        app.handle_transport(0.0);
        assert!(app.engine.transport.recording, "record command must activate the real app transport: {}", app.ui.status);
    }

    fn request_stop(app: &mut SoundApp, at_sample: Samples) {
        app.engine.transport.position = at_sample;
        app.run("transport.stop", json!({})).unwrap();
        app.handle_transport(0.0);
        assert!(!app.engine.transport.recording);
    }

    #[test]
    fn native_record_stop_uses_fake_device_to_make_undoable_instrument_take_without_audio_input() {
        let (mut app, track) = armed_midi_app();
        let (mut producer, queue) = device::capture_channel(16).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", queue)]));
        request_record(&mut app);
        assert!(app.recorder.is_none(), "a MIDI-only pass must never require a physical microphone");
        assert!(app.take_plan.is_none(), "MIDI notes do not need an audio-file take plan");
        assert_eq!(app.midi_input_status().connected_ports, ["Fake Keyboard"]);
        assert!(app.run("session.new", json!({})).is_err(), "a session switch must not destroy an uncommitted MIDI pass");
        producer.push_packet(1_000, &[0x90, 60, 100, 0xB0, 1, 75]);
        app.poll_midi_input();
        producer.push_packet(501_000, &[0x80, 60, 55]);
        app.engine.transport.position = 72_000;
        app.poll_midi_input();
        request_stop(&mut app, 96_000);
        let status = app.midi_input_status();
        assert!(!status.recording);
        assert!(status.connected_ports.is_empty());
        let clips = app.engine.session().track(track).unwrap().clips();
        assert_eq!(clips.len(), 1);
        assert_eq!((clips[0].start, clips[0].length), (48_000, 48_000));
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI take expected") };
        assert_eq!((sequence.notes.len(), sequence.ctrls.len()), (1, 1));
        assert_eq!((sequence.notes[0].pitch, sequence.notes[0].release_velocity), (60, 55));
        assert_eq!(sequence.notes[0].length, 960);
        assert!(app.engine.undo());
        assert!(app.engine.session().track(track).unwrap().clips().is_empty());
    }

    #[test]
    fn two_input_ports_with_distinct_timestamp_epochs_merge_into_one_take() {
        let (mut app, track) = armed_midi_app();
        app.engine.execute("setup.midi_input_devices", &json!({"enable": ["Second Keyboard"]})).unwrap();
        let (mut first, first_queue) = device::capture_channel(8).unwrap();
        let (mut second, second_queue) = device::capture_channel(8).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", first_queue), ("Second Keyboard", second_queue)]));
        request_record(&mut app);
        first.push_packet(100, &[0x90, 60, 90]);
        second.push_packet(4_000_000_000, &[0x90, 64, 85]);
        app.poll_midi_input();
        first.push_packet(500_100, &[0x80, 60, 64]);
        second.push_packet(4_000_500_000, &[0x80, 64, 64]);
        app.engine.transport.position = 72_000;
        app.poll_midi_input();
        request_stop(&mut app, 96_000);
        let clips = app.engine.session().track(track).unwrap().clips();
        assert_eq!(clips.len(), 1, "two ports should combine into one undoable track take");
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI take expected") };
        assert_eq!(sequence.notes.len(), 2);
        assert_eq!(sequence.notes.iter().map(|n| n.pitch).collect::<Vec<_>>(), vec![60, 64]);
    }

    #[test]
    fn stopped_transport_drains_queued_note_off_and_counts_queue_overflow() {
        let (mut app, track) = armed_midi_app();
        let (mut producer, queue) = device::capture_channel(1).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", queue)]));
        request_record(&mut app);
        producer.push_packet(10, &[0x90, 61, 80]);
        app.poll_midi_input();
        producer.push_packet(500_010, &[0x80, 61, 64, 0x90, 63, 80]);
        request_stop(&mut app, 96_000);
        assert_eq!(app.midi_input_status().dropped_events, 1);
        assert!(app.engine.messages.iter().any(|m| m.contains("MIDI input lost")));
        let clips = app.engine.session().track(track).unwrap().clips();
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI take expected") };
        assert_eq!(sequence.notes.len(), 1);
        assert_eq!(sequence.notes[0].length, 960, "the queued note-off must be drained at Stop");
    }

    #[test]
    fn simulated_port_unplug_stops_transport_and_closes_hanging_note() {
        let (mut app, track) = armed_midi_app();
        let (mut producer, queue) = device::capture_channel(8).unwrap();
        let present = Arc::new(AtomicBool::new(true));
        app.midi_test_sources = Some(MidiInputSources::fake("Fake Keyboard", queue, Arc::clone(&present)));
        request_record(&mut app);
        producer.push_packet(100, &[0x90, 60, 90]);
        app.poll_midi_input();
        app.engine.transport.position = 96_000;
        present.store(false, std::sync::atomic::Ordering::Relaxed);
        app.midi_capture.as_mut().unwrap().force_presence_check();
        app.poll_midi_input();
        assert!(!app.engine.transport.recording);
        assert!(app.midi_input_status().error.as_ref().is_some_and(|err| err.contains("disconnected")));
        let clips = app.engine.session().track(track).unwrap().clips();
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI take expected") };
        assert_eq!(sequence.notes.len(), 1, "hanging note must end at transport stop");
        assert!(sequence.notes[0].length > 0);
    }

    #[test]
    fn missing_input_and_unsupported_punch_fail_without_starting_transport() {
        let (mut app, _) = armed_midi_app();
        app.engine.execute("setup.midi_input_devices", &json!({"disable": ["Fake Keyboard"]})).unwrap();
        app.start_recording();
        assert!(!app.engine.transport.recording);
        assert!(app.ui.status.contains("no MIDI input selected"));
        app.engine.session_mut().edit.selection = Range::new(48_000, 96_000);
        app.start_recording();
        assert!(!app.engine.transport.recording);
        assert!(app.ui.status.contains("MIDI time-range punch"));
    }

    #[test]
    fn stale_device_preference_does_not_open_an_unrelated_port_index() {
        let available = vec![MidiInputPortInfo { name: "Keys A".into(), index: 0 }, MidiInputPortInfo { name: "Keys B".into(), index: 1 }];
        assert_eq!(matching_ports(&["Keys B".into()], &available).unwrap(), vec![available[1].clone()]);
        assert!(matching_ports(&["Disconnected Keys".into()], &available).unwrap_err().contains("unavailable"));
        assert!(matching_ports(&[], &available).unwrap_err().contains("no MIDI input selected"));
        let duplicate_names = vec![MidiInputPortInfo { name: "Keys= A".into(), index: 0 }, MidiInputPortInfo { name: "Keys A".into(), index: 1 }];
        assert!(matching_ports(&["Keys A".into()], &duplicate_names).unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn native_record_stop_routes_by_port_and_one_based_channel() {
        let (mut app, all) = armed_midi_app();
        let routed_a = app.engine.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, Some("Only A ch1"));
        let routed_b = app.engine.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Only B ch16"));
        app.engine.execute("mix.record_arm", &json!({"tracks": [routed_a.0, routed_b.0], "value": true})).unwrap();
        app.engine.execute("setup.midi_input_devices", &json!({"enable": ["Second Keyboard"]})).unwrap();
        app.engine.execute("track.midi_input", &json!({"track": routed_a.0, "port": "Fake Keyboard", "channel": 1})).unwrap();
        app.engine.execute("track.midi_input", &json!({"track": routed_b.0, "port": "Second Keyboard", "channel": 16})).unwrap();
        let (mut first, q1) = device::capture_channel(16).unwrap();
        let (mut second, q2) = device::capture_channel(16).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", q1), ("Second Keyboard", q2)]));
        request_record(&mut app);
        first.push_packet(100, &[0x90, 60, 100, 0xB0, 7, 90]);
        second.push_packet(5_000_000, &[0x9F, 65, 96, 0xBF, 7, 75]);
        app.poll_midi_input();
        first.push_packet(500_100, &[0x80, 60, 64]);
        second.push_packet(5_500_000, &[0x8F, 65, 64]);
        app.engine.transport.position = 72_000;
        app.poll_midi_input();
        request_stop(&mut app, 96_000);
        let summary = |id| {
            let clips = app.engine.session().track(id).unwrap().clips();
            let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI clip expected") };
            (sequence.notes.iter().map(|n| n.pitch).collect::<Vec<_>>(), sequence.ctrls.len())
        };
        assert_eq!(summary(all), (vec![60, 65], 2));
        assert_eq!(summary(routed_a), (vec![60], 1));
        assert_eq!(summary(routed_b), (vec![65], 1));
    }

    #[test]
    fn stale_disabled_named_route_and_ambiguous_fake_ports_fail_preflight() {
        let (mut app, track) = armed_midi_app();
        app.engine.session_mut().track_mut(track).unwrap().midi_input.port = Some("Unplugged Keys".into());
        let selected = selected_names(&app.engine);
        assert!(validate_track_routes(&app.engine, &selected).unwrap_err().contains("not enabled"));
        app.engine.session_mut().track_mut(track).unwrap().midi_input.port = None;
        let (_, first) = device::capture_channel(8).unwrap();
        let (_, second) = device::capture_channel(8).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", first), ("Fake Keyboard", second)]));
        app.start_recording();
        assert!(!app.engine.transport.recording);
        assert!(app.ui.status.contains("ambiguous"));
        assert!(app.engine.session().track(track).unwrap().clips().is_empty());
    }

    #[test]
    fn setup_midi_input_filter_is_honored_during_native_recording() {
        let (mut app, track) = armed_midi_app();
        app.engine.execute("setup.midi_input_filter", &json!({"filter": ["notes"]})).unwrap();
        let (mut producer, queue) = device::capture_channel(8).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", queue)]));
        request_record(&mut app);
        producer.push_packet(100, &[0x90, 60, 90, 0xB0, 7, 100]);
        app.poll_midi_input();
        producer.push_packet(500_100, &[0x80, 60, 64]);
        request_stop(&mut app, 96_000);
        let clips = app.engine.session().track(track).unwrap().clips();
        assert_eq!(clips.len(), 1);
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("MIDI take expected") };
        assert!(sequence.notes.is_empty(), "note messages were filtered at capture");
        assert_eq!(sequence.ctrls.len(), 1);
        assert_eq!(app.midi_input_status().dropped_events, 0, "intentional input filtering is not device loss");
    }

    #[test]
    fn invalidated_target_keeps_completed_take_in_memory_for_retry() {
        let (mut app, track) = armed_midi_app();
        let (mut producer, queue) = device::capture_channel(8).unwrap();
        app.midi_test_sources = Some(MidiInputSources::fake_many(vec![("Fake Keyboard", queue)]));
        request_record(&mut app);
        producer.push_packet(100, &[0x90, 60, 100]);
        app.poll_midi_input();
        // A track can be made inactive while the transport runs; the commit must reject it
        // atomically and keep all completed MIDI data until the user repairs the target.
        app.engine.session_mut().track_mut(track).unwrap().inactive = true;
        request_stop(&mut app, 96_000);
        assert!(app.midi_capture.is_some());
        assert!(app.ui.status.contains("kept in memory"));
        assert!(app.run("session.new", json!({})).is_err());
        app.engine.session_mut().track_mut(track).unwrap().inactive = false;
        let result = app.retry_midi_commit().unwrap();
        assert_eq!(result["clips"].as_array().map(Vec::len), Some(1));
        assert!(app.midi_capture.is_none());
        assert_eq!(app.engine.session().track(track).unwrap().clips().len(), 1);
    }
}
