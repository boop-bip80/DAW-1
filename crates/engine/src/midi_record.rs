//! Transport-aware MIDI capture, from a bounded input queue to undoable session clips.
//!
//! The hardware driver uses its own monotonic microsecond timestamp origin, which is
//! *not* the session's audio clock. A caller must explicitly pair one device timestamp
//! with an observed transport sample position via set_anchor. Later events use the
//! driver's timestamp deltas and the captured session tempo map. Anchoring at the first
//! event's UI-poll position is a functional fallback, but introduces up to one poll of
//! timing error; sample-accurate tracking requires a shared host/audio clock.
//!
//! This bridge runs entirely outside the realtime callback. Driver threads must only
//! enqueue through the fixed-capacity soundcraft-midi device capture queue.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use soundcraft_midi::{
    Sequence,
    live::{LiveMidiMessage, LiveMidiTake, TimedMidiMessage},
};
use soundcraft_model::TrackId;
use soundcraft_time::{MAX_POSITION, SampleRate, Samples, TempoMap};

/// Maximum number of MIDI input events handled in one UI/control-thread poll.
pub const MAX_MIDI_DRAIN_PER_POLL: usize = 4096;
/// One recording pass may not span more than 24 hours of device timestamp deltas.
const MAX_RECORD_MICROS: u64 = 24 * 60 * 60 * 1_000_000;

struct TrackCapture {
    id: TrackId,
    recorder: Option<LiveMidiTake>,
    completed: Option<Sequence>,
}

/// One MIDI recording pass, broadcast to the MIDI/instrument tracks armed at its start.
/// Input port/channel filtering and external MIDI output remain separate future work.
pub struct MidiRecordBridge {
    start_sample: Samples,
    start_tick: i64,
    sample_rate: SampleRate,
    tempo: TempoMap,
    tracks: Vec<TrackCapture>,
    /// Device timestamp paired with a transport position at the same observed moment.
    anchor: Option<(u64, Samples)>,
    last_timestamp: Option<u64>,
    last_event_sample: Samples,
    dropped_events: u64,
    truncated_packets: u64,
    commit_end: Option<Samples>,
    committed: bool,
}

fn problem(reason: impl Into<String>) -> EngineError {
    EngineError::BadParams("midi.record_take".into(), reason.into())
}

impl MidiRecordBridge {
    /// Start a pass using the current playing/recording transport, retaining the armed track
    /// selection and tempo map so later UI edits cannot move events during the pass.
    pub fn begin(engine: &Engine) -> Result<Self> {
        if !engine.transport.playing || !engine.transport.recording {
            return Err(EngineError::Disabled("midi.record_take".into(), "start the recording transport first".into()));
        }
        let session = engine.session();
        let tracks: Vec<_> = session
            .tracks
            .iter()
            .filter(|t| t.mixer.record_arm && t.kind.is_midi() && !t.inactive)
            .map(|t| TrackCapture { id: t.id, recorder: Some(LiveMidiTake::default()), completed: None })
            .collect();
        if tracks.is_empty() {
            return Err(problem("record-enable at least one MIDI or instrument track"));
        }
        if tracks.len() > 64 {
            return Err(problem("no more than 64 MIDI or instrument tracks can be recorded in one pass"));
        }
        let start_sample = engine.transport.position.clamp(0, MAX_POSITION - 1);
        Ok(Self {
            start_tick: session.tempo.samples_to_ticks(start_sample, session.sample_rate),
            sample_rate: session.sample_rate,
            tempo: session.tempo.clone(),
            tracks,
            start_sample,
            anchor: None,
            last_timestamp: None,
            last_event_sample: start_sample,
            dropped_events: 0,
            truncated_packets: 0,
            commit_end: None,
            committed: false,
        })
    }

    pub fn start_sample(&self) -> Samples {
        self.start_sample
    }

    pub fn armed_tracks(&self) -> impl Iterator<Item = TrackId> + '_ {
        self.tracks.iter().map(|capture| capture.id)
    }

    /// Pair a device timestamp with the playhead's sample position. The values must represent
    /// the same observed instant, not arbitrary clock epochs. Set exactly once before ingesting.
    pub fn set_anchor(&mut self, timestamp_micros: u64, transport_sample: Samples) -> Result<()> {
        if self.anchor.is_some() || self.commit_end.is_some() {
            return Err(problem("MIDI timestamp anchor is already set or the pass has finished"));
        }
        if !(self.start_sample..=MAX_POSITION).contains(&transport_sample) {
            return Err(problem("MIDI anchor must be within the recorded transport range"));
        }
        self.anchor = Some((timestamp_micros, transport_sample));
        Ok(())
    }

    fn sample_for(&self, timestamp_micros: u64) -> Result<Samples> {
        let (origin, at_sample) = self.anchor.ok_or_else(|| problem("calibrate MIDI timestamp against transport before recording events"))?;
        if timestamp_micros < origin || self.last_timestamp.is_some_and(|last| timestamp_micros < last) {
            return Err(problem("MIDI timestamp moved backwards or predates the recording anchor"));
        }
        let elapsed = timestamp_micros - origin;
        if elapsed > MAX_RECORD_MICROS {
            return Err(problem("MIDI input timestamp exceeds the maximum 24-hour recording span"));
        }
        let delta_samples = u128::from(elapsed).saturating_mul(u128::from(self.sample_rate.hz())) / 1_000_000;
        let delta_samples = i64::try_from(delta_samples).unwrap_or(i64::MAX);
        Ok(at_sample.saturating_add(delta_samples).min(MAX_POSITION))
    }

    /// Consume one decoded device message off the realtime thread. The same message reaches
    /// every armed MIDI/instrument track, as when multiple tracks listen to one input port.
    pub fn record_event(&mut self, event: TimedMidiMessage) -> Result<()> {
        if self.commit_end.is_some() {
            return Err(problem("cannot append MIDI events after finishing a take"));
        }
        let at_sample = self.sample_for(event.timestamp_micros)?;
        if !matches!(event.message, LiveMidiMessage::Realtime(_)) {
            let tick = self.tempo.samples_to_ticks(at_sample, self.sample_rate).saturating_sub(self.start_tick).max(0);
            for track in &mut self.tracks {
                if let Some(recorder) = &mut track.recorder {
                    recorder.record(tick, event.message).map_err(|err| problem(format!("record MIDI event: {err}")))?;
                }
            }
            self.last_event_sample = self.last_event_sample.max(at_sample);
        }
        self.last_timestamp = Some(event.timestamp_micros);
        Ok(())
    }

    /// Drain a bounded number of captured input events. A full/oversized queue is recorded in
    /// sticky counters and reported on finish: partial recordings remain recoverable.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn consume_queue(&mut self, queue: &soundcraft_midi::device::MidiCaptureQueue) -> Result<usize> {
        self.dropped_events = self.dropped_events.saturating_add(queue.take_dropped_events());
        self.truncated_packets = self.truncated_packets.saturating_add(queue.take_truncated_packets());
        let mut count = 0;
        while count < MAX_MIDI_DRAIN_PER_POLL {
            let Some(event) = queue.try_recv() else { break };
            self.record_event(event)?;
            count += 1;
        }
        Ok(count)
    }

    pub fn input_loss(&self) -> (u64, u64) {
        (self.dropped_events, self.truncated_packets)
    }

    /// Account for lost events from multiple independent input ports after merging their
    /// timestamp domains on the control thread. Never call this from a driver callback.
    pub fn add_input_loss(&mut self, dropped_events: u64, truncated_packets: u64) {
        self.dropped_events = self.dropped_events.saturating_add(dropped_events);
        self.truncated_packets = self.truncated_packets.saturating_add(truncated_packets);
    }

    /// Finish note-offs at the actual transport stop (or the last observed event, whichever
    /// is later) and commit all completed takes in one undoable engine command.
    ///
    /// The bridge retains completed sequences if command validation fails, so a caller can
    /// correct the target session and retry instead of losing the take on a failed commit.
    pub fn finish(&mut self, engine: &mut Engine, transport_end: Samples) -> Result<Value> {
        if self.committed {
            return Err(problem("MIDI take was already committed"));
        }
        if engine.session().sample_rate != self.sample_rate || engine.session().tempo != self.tempo {
            return Err(problem("session sample rate or tempo changed during the MIDI recording; preserve the take before retry"));
        }
        let end = match self.commit_end {
            Some(end) => end,
            None => {
                let end = transport_end.max(self.start_sample.saturating_add(1)).max(self.last_event_sample.saturating_add(1)).clamp(1, MAX_POSITION);
                self.commit_end = Some(end);
                end
            }
        };
        let end_tick = self.tempo.samples_to_ticks(end, self.sample_rate).saturating_sub(self.start_tick).max(1);
        for track in &mut self.tracks {
            if let Some(recorder) = track.recorder.take() {
                track.completed = Some(recorder.finish(end_tick).map_err(|err| problem(format!("finish MIDI take: {err}")))?);
            }
        }
        let takes: Vec<Value> = self
            .tracks
            .iter()
            .filter_map(|capture| {
                let seq = capture.completed.as_ref()?;
                (!seq.notes.is_empty() || !seq.ctrls.is_empty()).then_some(json!({"track": capture.id.0, "sequence": seq}))
            })
            .collect();
        if takes.is_empty() {
            self.report_input_loss(engine);
            self.committed = true;
            return Ok(
                json!({"clips": [], "start": self.start_sample, "end": end, "dropped_events": self.dropped_events, "truncated_packets": self.truncated_packets}),
            );
        }
        let mut result = engine.execute("midi.record_take", &json!({"start": self.start_sample, "end": end, "takes": takes}))?;
        self.report_input_loss(engine);
        if let Value::Object(ref mut map) = result {
            map.insert("dropped_events".into(), json!(self.dropped_events));
            map.insert("truncated_packets".into(), json!(self.truncated_packets));
        }
        self.committed = true;
        Ok(result)
    }

    fn report_input_loss(&self, engine: &mut Engine) {
        if self.dropped_events > 0 || self.truncated_packets > 0 {
            engine.message(format!(
                "MIDI input lost {} event(s) and truncated {} packet(s); inspect this take before use.",
                self.dropped_events, self.truncated_packets
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_midi::live::LiveMidiMessage;
    use soundcraft_model::{ChannelFormat, ClipContent, TrackKind};

    fn engine_with_armed_instrument() -> (Engine, TrackId) {
        let mut engine = Engine::default();
        let track = engine.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let arm = engine.execute("mix.record_arm", &json!({"tracks": [track.0], "value": true}));
        assert!(arm.is_ok());
        engine.transport.playing = true;
        engine.transport.recording = true;
        engine.transport.position = 48_000;
        (engine, track)
    }

    #[test]
    fn fake_device_to_armed_instrument_clip_with_undo() {
        let (mut e, track) = engine_with_armed_instrument();
        let mut bridge = MidiRecordBridge::begin(&e).unwrap();
        bridge.set_anchor(1_000, 48_000).unwrap();
        let (mut producer, queue) = soundcraft_midi::device::capture_channel(8).unwrap();
        producer.push_packet(1_000, &[0x90, 60, 100, 0xB0, 1, 64]);
        producer.push_packet(501_000, &[0x80, 60, 50]);
        assert_eq!(bridge.consume_queue(&queue), Ok(3));
        let r = bridge.finish(&mut e, 96_000).unwrap();
        assert_eq!(r["clips"].as_array().map(Vec::len), Some(1));
        let clips = e.session().track(track).map(|t| t.clips()).unwrap_or_default();
        assert_eq!(clips.len(), 1);
        assert_eq!((clips[0].start, clips[0].length), (48_000, 48_000));
        let ClipContent::Midi { sequence } = &clips[0].content else { panic!("expected MIDI clip") };
        assert_eq!(sequence.notes.len(), 1);
        assert_eq!((sequence.notes[0].start, sequence.notes[0].length), (0, 960));
        assert_eq!(sequence.notes[0].release_velocity, 50);
        assert_eq!(sequence.ctrls.len(), 1);
        assert_eq!(sequence.ctrls[0].tick, 0);
        assert!(e.undo());
        assert!(e.session().track(track).is_some_and(|t| t.clips().is_empty()));
    }

    #[test]
    fn only_armed_midi_tracks_receive_input() {
        let (mut e, first) = engine_with_armed_instrument();
        let second = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Stereo, Some("Second"));
        let audio = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Audio"));
        e.execute("mix.record_arm", &json!({"tracks": [second.0, audio.0], "value": true})).unwrap();
        let mut bridge = MidiRecordBridge::begin(&e).unwrap();
        assert_eq!(bridge.armed_tracks().collect::<Vec<_>>(), vec![first, second]);
        bridge.set_anchor(5, 48_000).unwrap();
        bridge
            .record_event(TimedMidiMessage { timestamp_micros: 5, message: LiveMidiMessage::NoteOn { channel: 0, pitch: 64, velocity: 80 } })
            .unwrap();
        bridge
            .record_event(TimedMidiMessage { timestamp_micros: 505, message: LiveMidiMessage::NoteOff { channel: 0, pitch: 64, velocity: 64 } })
            .unwrap();
        let result = bridge.finish(&mut e, 96_000).unwrap();
        assert_eq!(result["clips"].as_array().map(Vec::len), Some(2));
        assert!(e.session().track(audio).is_some_and(|t| t.clips().is_empty()));
    }

    #[test]
    fn tempo_map_changes_are_accounted_for_and_timestamps_checked() {
        let (mut e, _) = engine_with_armed_instrument();
        assert!(e.session_mut().tempo.set_tempo(960, 60.0).is_ok());
        let mut bridge = MidiRecordBridge::begin(&e).unwrap();
        assert!(bridge.record_event(TimedMidiMessage { timestamp_micros: 100, message: LiveMidiMessage::Realtime(0xF8) }).is_err());
        bridge.set_anchor(100, 48_000).unwrap();
        bridge
            .record_event(TimedMidiMessage { timestamp_micros: 100, message: LiveMidiMessage::NoteOn { channel: 0, pitch: 60, velocity: 100 } })
            .unwrap();
        bridge
            .record_event(TimedMidiMessage { timestamp_micros: 500_100, message: LiveMidiMessage::NoteOff { channel: 0, pitch: 60, velocity: 64 } })
            .unwrap();
        assert!(bridge.record_event(TimedMidiMessage { timestamp_micros: 90, message: LiveMidiMessage::Realtime(0xF8) }).is_err());
        assert!(bridge.finish(&mut e, 97_000).is_ok());
    }

    #[test]
    fn full_input_queue_is_reported_but_a_partial_take_is_preserved() {
        let (mut e, _) = engine_with_armed_instrument();
        let mut bridge = MidiRecordBridge::begin(&e).unwrap();
        bridge.set_anchor(0, 48_000).unwrap();
        let (mut producer, queue) = soundcraft_midi::device::capture_channel(1).unwrap();
        producer.push_packet(0, &[0x90, 60, 90, 61, 91, 62, 92]);
        assert_eq!(bridge.consume_queue(&queue), Ok(1));
        assert_eq!(bridge.input_loss(), (2, 0));
        let result = bridge.finish(&mut e, 96_000).unwrap();
        assert_eq!(result["dropped_events"], 2);
        assert!(e.messages.iter().any(|m| m.contains("MIDI input lost")));
    }

    #[test]
    fn input_loss_is_reported_even_without_any_recordable_events() {
        let (mut e, _) = engine_with_armed_instrument();
        let mut bridge = MidiRecordBridge::begin(&e).unwrap();
        bridge.set_anchor(0, 48_000).unwrap();
        let (mut producer, queue) = soundcraft_midi::device::capture_channel(1).unwrap();
        producer.push_packet(0, &[0xF8, 0xF8, 0xF8]);
        assert_eq!(bridge.consume_queue(&queue), Ok(1));
        let result = bridge.finish(&mut e, 96_000).unwrap();
        assert_eq!(result["clips"], json!([]));
        assert_eq!(result["dropped_events"], 2);
        assert!(e.messages.iter().any(|m| m.contains("MIDI input lost")));
    }

    #[test]
    fn cannot_begin_without_armed_recording_transport() {
        let mut e = Engine::default();
        e.transport.playing = true;
        e.transport.recording = true;
        assert!(MidiRecordBridge::begin(&e).is_err());
        let t = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Stereo, None);
        e.execute("mix.record_arm", &json!({"tracks": [t.0], "value": true})).unwrap();
        e.transport.recording = false;
        assert!(MidiRecordBridge::begin(&e).is_err());
    }
}
