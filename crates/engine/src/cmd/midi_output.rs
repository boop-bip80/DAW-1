//! Undoable external MIDI destination preference. This module never opens or
//! enumerates devices: a named route is not permission to send on its own.

use super::{CommandSpec, always, bad};
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};
use soundcraft_model::{MidiOutputRoute, TrackId};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "track.midi_output",
            "MIDI Track Output",
            [],
            None,
            "{track, port?: name|null, channel?: 1..16|\"original\"} — explicit external MIDI destination; omitted fields preserve, undoable",
            always,
            set_route
        ),
        cmd!(query "midi.output_route", "Read MIDI Track Output", [], None, "{track} — read-only external MIDI destination", always, read_route),
    ]
}

fn target(e: &Engine, params: &Value, command: &str) -> Result<TrackId> {
    let id = TrackId(params.get("track").and_then(Value::as_u64).ok_or_else(|| bad(command, "track id required"))?);
    let track = e.session().track(id).ok_or_else(|| bad(command, "MIDI track not found"))?;
    if !track.kind.is_midi() {
        return Err(bad(command, "track must be a MIDI or instrument track"));
    }
    Ok(id)
}

fn route_json(route: Option<&MidiOutputRoute>) -> Value {
    match route {
        Some(route) => json!({"port": route.port, "channel": route.channel.map_or_else(|| json!("original"), |channel| json!(channel))}),
        None => json!({"port": null, "channel": "original"}),
    }
}

fn read_route(e: &mut Engine, params: &Value) -> Result<Value> {
    let id = target(e, params, "midi.output_route")?;
    let route = e.session().track(id).ok_or_else(|| bad("midi.output_route", "MIDI track not found"))?.midi_output.as_ref();
    Ok(route_json(route))
}

fn parse_channel(value: &Value) -> Result<Option<u8>> {
    match value {
        Value::String(value) if value == "original" => Ok(None),
        Value::Number(_) => match value.as_u64() {
            Some(value @ 1..=16) => Ok(Some(u8::try_from(value).unwrap_or(1))),
            _ => Err(bad("track.midi_output", "channel must be 1..16 or 'original'")),
        },
        _ => Err(bad("track.midi_output", "channel must be 1..16 or 'original'")),
    }
}

fn set_route(e: &mut Engine, params: &Value) -> Result<Value> {
    const CMD: &str = "track.midi_output";
    if e.transport.playing || e.transport.recording {
        return Err(EngineError::Disabled(CMD.into(), "stop the transport before changing an external MIDI output route".into()));
    }
    let id = target(e, params, CMD)?;
    if params.get("port").is_none() && params.get("channel").is_none() {
        return Err(bad(CMD, "port or channel update required; use midi.output_route to query"));
    }
    let mut route = e.session().track(id).and_then(|track| track.midi_output.clone());
    if let Some(value) = params.get("port") {
        route = match value {
            Value::Null => None,
            Value::String(name) => {
                // Keep exact OS port spelling. Silently normalizing driver names can
                // select a different device when the runtime opens the port.
                if name.is_empty() || name.len() > 512 || name.trim() != name || name.chars().any(char::is_control) {
                    return Err(bad(CMD, "port name must be 1..512 bytes, unpadded and free of control characters"));
                }
                Some(MidiOutputRoute { port: name.clone(), channel: route.as_ref().and_then(|previous| previous.channel) })
            }
            _ => return Err(bad(CMD, "port must be an exact device name or null to disable external output")),
        };
    }
    if let Some(value) = params.get("channel") {
        let channel = parse_channel(value)?;
        if let Some(route) = route.as_mut() {
            route.channel = channel;
        } else if channel.is_some() {
            return Err(bad(CMD, "select an external MIDI output port before overriding its channel"));
        }
    }
    // A disconnected/ambiguous port can remain in a reopened session. Discovery
    // and duplicate-name rejection belong to the later native send preflight,
    // never to an undoable document mutation or a headless engine command.
    if e.session().track(id).is_some_and(|track| track.midi_output == route) {
        return Ok(route_json(route.as_ref()));
    }
    let track = e.session_mut().track_mut(id).ok_or_else(|| bad(CMD, "MIDI track not found"))?;
    track.midi_output = route.clone();
    Ok(route_json(route.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, Session, TrackKind};

    #[test]
    fn output_route_defaults_to_off_is_undoable_and_survives_roundtrip() {
        let mut e = Engine::default();
        let midi = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, Some("External"));
        let inst = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Instrument"));
        let audio = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Audio"));
        let first_journal = e.journal.len();
        assert_eq!(e.execute("midi.output_route", &json!({"track": midi.0})).unwrap(), json!({"port": null, "channel": "original"}));
        assert_eq!(e.journal.len(), first_journal, "read-only output query must never journal");
        assert!(e.execute("track.midi_output", &json!({"track": audio.0, "port": "Synth"})).is_err());
        assert!(e.execute("track.midi_output", &json!({"track": midi.0})).is_err());

        let route = e.execute("track.midi_output", &json!({"track": midi.0, "port": "Offline Synth", "channel": 12})).unwrap();
        assert_eq!(route, json!({"port": "Offline Synth", "channel": 12}));
        assert_eq!(e.session().track(midi).unwrap().midi_output, Some(MidiOutputRoute { port: "Offline Synth".into(), channel: Some(12) }));
        let restored = Session::from_json(&e.session().to_json().unwrap()).unwrap();
        assert_eq!(restored.track(midi).unwrap().midi_output, e.session().track(midi).unwrap().midi_output);
        assert!(restored.track(inst).unwrap().midi_output.is_none());
        assert_eq!(e.journal.last().map(|(id, _)| id.as_str()), Some("track.midi_output"));
        assert!(e.undo());
        assert!(e.session().track(midi).unwrap().midi_output.is_none());
        assert!(e.redo());
        assert_eq!(e.execute("midi.output_route", &json!({"track": midi.0})).unwrap(), route);

        assert_eq!(
            e.execute("track.midi_output", &json!({"track": midi.0, "channel": "original"})).unwrap(),
            json!({"port": "Offline Synth", "channel": "original"})
        );
        assert_eq!(e.execute("track.midi_output", &json!({"track": midi.0, "port": null})).unwrap(), json!({"port": null, "channel": "original"}));
        assert!(e.session().track(midi).unwrap().midi_output.is_none());
    }

    #[test]
    fn rejects_invalid_ports_and_channels_atomically() {
        let mut e = Engine::default();
        let id = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, None);
        for params in [
            json!({"track": id.0, "channel": 1}),
            json!({"track": id.0, "port": " "}),
            json!({"track": id.0, "port": "\nSynth"}),
            json!({"track": id.0, "port": 123}),
            json!({"track": id.0, "port": "Synth", "channel": 0}),
            json!({"track": id.0, "port": "Synth", "channel": 17}),
            json!({"track": id.0, "port": "Synth", "channel": -1}),
            json!({"track": id.0, "port": "Synth", "channel": 1.5}),
            json!({"track": id.0, "port": "Synth", "channel": "all"}),
            json!({"track": id.0, "port": null, "channel": 3}),
        ] {
            assert!(e.execute("track.midi_output", &params).is_err(), "unexpectedly accepted: {params}");
            assert!(e.session().track(id).unwrap().midi_output.is_none(), "failed mutation must preserve session");
        }
        assert_eq!(e.journal.len(), 0);
        e.transport.playing = true;
        assert!(e.execute("track.midi_output", &json!({"track": id.0, "port": "Synth"})).is_err());
    }
}
