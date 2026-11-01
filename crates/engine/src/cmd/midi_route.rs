//! Persistent, undoable MIDI input source and channel routing for MIDI/instrument tracks.

use super::{CommandSpec, always, bad};
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};
use soundcraft_model::{MidiInputRoute, TrackId};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "track.midi_input",
            "MIDI Track Input",
            [],
            None,
            "{track, port?: name|null, channel?: 1..16|\"all\"} — change MIDI/instrument input, undoable; omitted fields preserve",
            always,
            set_midi_input
        ),
        cmd!(query "midi.input_route", "Read MIDI Track Input", [], None, "{track} — read-only MIDI port/channel route", always, input_route),
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

fn route_json(route: &MidiInputRoute) -> Value {
    json!({"port": route.port, "channel": route.channel.map_or_else(|| json!("all"), |channel| json!(channel))})
}

fn input_route(e: &mut Engine, params: &Value) -> Result<Value> {
    let id = target(e, params, "midi.input_route")?;
    let route = &e.session().track(id).ok_or_else(|| bad("midi.input_route", "MIDI track not found"))?.midi_input;
    Ok(route_json(route))
}

fn set_midi_input(e: &mut Engine, params: &Value) -> Result<Value> {
    const CMD: &str = "track.midi_input";
    if e.transport.recording {
        return Err(EngineError::Disabled(CMD.into(), "stop recording before changing a MIDI input route".into()));
    }
    let id = target(e, params, CMD)?;
    let has_port = params.get("port").is_some();
    let has_channel = params.get("channel").is_some();
    if !has_port && !has_channel {
        return Err(bad(CMD, "port or channel update required; use midi.input_route to query"));
    }
    let mut route = e.session().track(id).ok_or_else(|| bad(CMD, "MIDI track not found"))?.midi_input.clone();
    if let Some(value) = params.get("port") {
        route.port = match value {
            Value::Null => None,
            Value::String(name) => {
                if name.len() > 512 {
                    return Err(bad(CMD, "MIDI input port name is too long"));
                }
                let normalized: String = name.chars().filter(|c| !c.is_control() && *c != '=').take(64).collect();
                let normalized = normalized.trim().to_owned();
                if normalized.is_empty() {
                    return Err(bad(CMD, "MIDI input port name must not be blank"));
                }
                Some(normalized)
            }
            _ => return Err(bad(CMD, "port must be a device name or null for All Enabled")),
        };
    }
    if let Some(value) = params.get("channel") {
        route.channel = match value {
            Value::String(name) if name == "all" => None,
            Value::Number(_) => match value.as_u64() {
                Some(channel @ 1..=16) => Some(channel as u8),
                _ => return Err(bad(CMD, "MIDI input channel must be one of 1..16 or 'all'")),
            },
            _ => return Err(bad(CMD, "MIDI input channel must be one of 1..16 or 'all'")),
        };
    }
    if let Some(port) = &route.port
        && !e.session().edit.flags.contains(&format!("midi.input.{port}"))
    {
        return Err(bad(CMD, format!("MIDI input '{port}' is not enabled in Setup > Playback Engine")));
    }
    if e.session().track(id).is_some_and(|track| track.midi_input == route) {
        return Ok(route_json(&route));
    }
    let track = e.session_mut().track_mut(id).ok_or_else(|| bad(CMD, "MIDI track not found"))?;
    track.midi_input = route.clone();
    Ok(route_json(&route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, Session, TrackKind};

    #[test]
    fn undoable_input_route_and_readonly_query_survive_session_roundtrip() {
        let mut e = Engine::default();
        let keys = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        let midi = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, Some("MIDI"));
        let audio = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        assert_eq!(e.execute("midi.input_route", &json!({"track": keys.0})).unwrap(), json!({"port": null, "channel": "all"}));
        assert!(e.execute("track.midi_input", &json!({"track": audio.0, "port": "Keyboard"})).is_err());
        assert!(e.execute("track.midi_input", &json!({"track": midi.0})).is_err(), "setter must reject a query-shaped mutation");
        e.execute("setup.midi_input_devices", &json!({"enable": ["Keyboard"]})).unwrap();
        let route = e.execute("track.midi_input", &json!({"track": keys.0, "port": "Keyboard", "channel": 3})).unwrap();
        assert_eq!(route, json!({"port": "Keyboard", "channel": 3}));
        assert_eq!(e.execute("midi.input_route", &json!({"track": keys.0})).unwrap(), route);
        let changed = e.session().to_json().unwrap();
        let loaded = Session::from_json(&changed).unwrap();
        assert_eq!(loaded.track(keys).unwrap().midi_input, MidiInputRoute { port: Some("Keyboard".into()), channel: Some(3) });
        assert!(loaded.track(midi).unwrap().midi_input.is_all_enabled());
        assert!(e.undo(), "route edit must be one undoable command");
        assert_eq!(e.session().track(keys).unwrap().midi_input, MidiInputRoute::default());
        assert!(e.redo());
        assert_eq!(e.session().track(keys).unwrap().midi_input.channel, Some(3));
        let reset = e.execute("track.midi_input", &json!({"track": keys.0, "port": null})).unwrap();
        assert_eq!(reset, json!({"port": null, "channel": 3}), "omitted channel must be preserved");
        let reset = e.execute("track.midi_input", &json!({"track": keys.0, "channel": "all"})).unwrap();
        assert_eq!(reset, json!({"port": null, "channel": "all"}));
    }

    #[test]
    fn validates_port_and_one_based_channels_without_mutating_session() {
        let mut e = Engine::default();
        let keys = e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, None);
        for params in [
            json!({"track": keys.0, "channel": 0}),
            json!({"track": keys.0, "channel": 17}),
            json!({"track": keys.0, "channel": -1}),
            json!({"track": keys.0, "channel": 1.5}),
            json!({"track": keys.0, "channel": "omni"}),
            json!({"track": keys.0, "port": "Not Enabled"}),
            json!({"track": keys.0, "port": "  "}),
        ] {
            assert!(e.execute("track.midi_input", &params).is_err(), "invalid params: {params}");
        }
        assert!(e.session().track(keys).unwrap().midi_input.is_all_enabled());
        e.transport.recording = true;
        assert!(e.execute("track.midi_input", &json!({"track": keys.0, "channel": 1})).is_err());
    }
}
