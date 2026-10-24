//! Metering options: the meter type of the track and main meters. (Pre-Fader Metering is an
//! Options toggle in `options`.)
use super::{CommandSpec, always, bad, str_param};

use crate::{Engine, Result, cmd};
use serde_json::{Value, json};
use soundcraft_dsp::meter::MeterKind;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        noundo "options.meter_type",
        "Meter Type",
        [],
        None,
        "{type?: sample_peak|true_peak|rms|vu|k12|k14|k20} — the track and main meters' type; without `type`, the next one",
        always,
        meter_type
    )]
}

fn meter_type(e: &mut Engine, p: &Value) -> Result<Value> {
    let current = MeterKind::parse(&e.session().edit.meter_type).unwrap_or_default();
    let kind = match str_param(p, "type") {
        Some(t) => MeterKind::parse(t).ok_or_else(|| {
            let ids: Vec<&str> = MeterKind::ALL.iter().map(|k| k.id()).collect();
            bad("options.meter_type", format!("unknown meter type `{t}` ({})", ids.join(", ")))
        })?,
        None => current.next(),
    };
    e.session_mut().edit.meter_type = kind.id().into();
    Ok(json!({"type": kind.id(), "label": kind.label()}))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn sets_and_cycles_the_meter_type() {
        let mut e = Engine::default();
        assert_eq!(e.session().edit.meter_type, "sample_peak");
        assert_eq!(e.execute("options.meter_type", &json!({})).unwrap()["type"], "true_peak");
        assert_eq!(e.execute("options.meter_type", &json!({"type": "k14"})).unwrap()["label"], "K-14");
        assert_eq!(e.session().edit.meter_type, "k14");
        assert!(e.execute("options.meter_type", &json!({"type": "ppm"})).is_err());
        assert_eq!(e.session().edit.meter_type, "k14", "a refused type changes nothing");
        e.execute("options.meter_type", &json!({"type": "k20"})).unwrap();
        assert_eq!(e.execute("options.meter_type", &json!({})).unwrap()["type"], "sample_peak", "and round again");
        // Saved with the session; older sessions without it meter sample peaks.
        let mut v = serde_json::to_value(&e.session().edit).unwrap();
        v.as_object_mut().unwrap().remove("meter_type");
        let old: soundcraft_model::EditState = serde_json::from_value(v).unwrap();
        assert_eq!(old.meter_type, "sample_peak");
    }
}
