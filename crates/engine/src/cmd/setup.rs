use super::*;
use crate::cmd;
use crate::cmd::more_util::{set_text, text};
use crate::{Engine, Result, TransportRequest};
use serde_json::{Value, json};
use soundcraft_model::{BusId, ChannelFormat, OutputPath, Route, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            noundo "setup.hardware",
            "Hardware...",
            ["Setup"],
            None,
            "{device?, input_device?, buffer_size?: 16..8192} — audio interface settings (sample rate is the session's)",
            always,
            |e, p| engine_settings(e, p)
        ),
        cmd!(
            noundo "setup.playback_engine",
            "Playback Engine...",
            ["Setup"],
            None,
            "{device?, input_device?, buffer_size?: 16..8192, delay_compensation?: bool}",
            always,
            |e, p| {
                if let Some(v) = p.get("delay_compensation").and_then(Value::as_bool) {
                    e.session_mut().edit.delay_compensation = v;
                }
                engine_settings(e, p)
            }
        ),
        cmd!(
            "setup.io",
            "I/O...",
            ["Setup"],
            None,
            "{action?: list|create_bus|rename_bus|delete_bus|create_output|rename_output|delete_output, name?, new_name?, format?: stereo, first_channel?: 0}",
            always,
            io_setup
        ),
    ]
}

pub fn engine_settings(e: &mut Engine, p: &Value) -> Result<Value> {
    let mut buf_sz = None;
    if let Some(b) = p.get("buffer_size").and_then(Value::as_u64) {
        let b = b.clamp(16, 8192).next_power_of_two().min(8192) as u32;
        e.session_mut().edit.values.insert("engine.buffer_size".into(), b as f64);
        buf_sz = Some(b);
    }
    let mut dev = None;
    if let Some(d) = str_param(p, "device") {
        set_text(&mut e.session_mut().edit, "engine.device", Some(d));
        dev = Some(d.to_string());
    }
    let mut in_dev = None;
    if let Some(d) = str_param(p, "input_device") {
        set_text(&mut e.session_mut().edit, "engine.input_device", Some(d));
        in_dev = Some(d.to_string());
    }

    if buf_sz.is_some() || dev.is_some() || in_dev.is_some() {
        e.transport_requests.push(TransportRequest::ReconfigureAudio {
            device: dev.or_else(|| text(&e.session().edit, "engine.device")),
            input_device: in_dev.or_else(|| text(&e.session().edit, "engine.input_device")),
            buffer_size: buf_sz.or_else(|| Some(e.session().edit.value("engine.buffer_size", 512.0) as u32)),
        });
    }

    let s = e.session();
    Ok(json!({
        "device": text(&s.edit, "engine.device"),
        "input_device": text(&s.edit, "engine.input_device"),
        "buffer_size": s.edit.value("engine.buffer_size", 512.0),
        "sample_rate": s.sample_rate.hz(),
        "bit_depth": s.bit_depth,
        "delay_compensation": s.edit.delay_compensation,
    }))
}

pub fn io_state(s: &Session) -> Value {
    json!({
        "busses": s.busses,
        "outputs": s.outputs,
    })
}

pub fn io_setup(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = "setup.io";
    let action = str_param(p, "action").unwrap_or("list");
    if action == "list" {
        return Ok(io_state(e.session()));
    }
    let name = str_param(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(id, "`name` required"))?.to_string();
    let new_name = str_param(p, "new_name").map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    let format = match str_param(p, "format") {
        Some(f) => ChannelFormat::from_id(f).ok_or_else(|| bad(id, format!("unknown format `{f}`")))?,
        None => ChannelFormat::Stereo,
    };
    let s = e.session_mut();
    match action {
        "create_bus" => {
            if s.bus_by_name(&name).is_some() {
                return Err(bad(id, format!("a bus named `{name}` exists")));
            }
            s.add_bus(&name, format);
        }
        "rename_bus" => {
            let to = new_name.ok_or_else(|| bad(id, "`new_name` required"))?;
            if s.bus_by_name(&to).is_some() {
                return Err(bad(id, format!("a bus named `{to}` exists")));
            }
            let b = s.busses.iter_mut().find(|b| b.name.eq_ignore_ascii_case(&name)).ok_or_else(|| bad(id, format!("no bus `{name}`")))?;
            b.name = to;
        }
        "delete_bus" => {
            let bid: BusId = s.bus_by_name(&name).map(|b| b.id).ok_or_else(|| bad(id, format!("no bus `{name}`")))?;
            s.busses.retain(|b| b.id != bid);
            for t in &mut s.tracks {
                if t.mixer.input == Route::Bus(bid) {
                    t.mixer.input = Route::None;
                }
                if t.mixer.output == Route::Bus(bid) {
                    t.mixer.output = Route::None;
                }
                for slot in &mut t.mixer.sends {
                    if slot.as_ref().is_some_and(|x| x.target == Route::Bus(bid)) {
                        *slot = None;
                    }
                }
            }
        }
        "create_output" => {
            if s.outputs.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
                return Err(bad(id, format!("an output named `{name}` exists")));
            }
            let first = i64_or(p, "first_channel", 0).clamp(0, 1023) as u16;
            s.outputs.push(OutputPath { name, first_channel: first, format });
        }
        "rename_output" => {
            let to = new_name.ok_or_else(|| bad(id, "`new_name` required"))?;
            let o = s.outputs.iter_mut().find(|o| o.name.eq_ignore_ascii_case(&name)).ok_or_else(|| bad(id, format!("no output `{name}`")))?;
            let old = std::mem::replace(&mut o.name, to.clone());
            for t in &mut s.tracks {
                if t.mixer.output == Route::Hardware(old.clone()) {
                    t.mixer.output = Route::Hardware(to.clone());
                }
            }
        }
        "delete_output" => {
            let before = s.outputs.len();
            s.outputs.retain(|o| !o.name.eq_ignore_ascii_case(&name));
            if s.outputs.len() == before {
                return Err(bad(id, format!("no output `{name}`")));
            }
        }
        other => return Err(bad(id, format!("unknown action `{other}`"))),
    }
    Ok(io_state(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_settings_triggers_reconfigure() {
        let mut e = Engine::default();
        let r = e.execute("setup.playback_engine", &json!({"buffer_size": 256, "device": "USB Interface"})).unwrap();
        assert_eq!(r["buffer_size"], 256.0);
        assert_eq!(r["device"], "USB Interface");
        assert!(e.transport_requests.iter().any(|req| matches!(
            req,
            TransportRequest::ReconfigureAudio {
                device: Some(d),
                buffer_size: Some(256),
                ..
            } if d == "USB Interface"
        )));
    }

    #[test]
    fn io_setup_manages_outputs_and_busses() {
        let mut e = Engine::default();
        let r = e.execute("setup.io", &json!({"action": "create_output", "name": "Monitors", "first_channel": 0})).unwrap();
        assert!(r["outputs"].as_array().unwrap().iter().any(|o| o["name"] == "Monitors"));
    }
}
