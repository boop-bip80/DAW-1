//! Meter ballistics, scale and preferences, in one place.
//!
//! The audio thread hands the UI the highest reading of every block since the last poll
//! (`soundcraft_playback::Player::take_meters`): the session's meter type (sample or true peak,
//! RMS, VU, K-System; `options.meter_type`, measured by `soundcraft_dsp::meter::MeterBank`), the
//! sample peak and the gain reduction. This module turns those into what the meters show: for
//! peak types instant attack and a release in dB per second (averaging types bring their own
//! ballistics), a peak hold line, a clip light and the highest peak as a number, each following
//! the meter preferences in [`MeterPrefs`] (`ui.meters`), on the meter type's [`Scale`]. Clicking
//! a meter resets its holds and clip light (`ui.meters.reset`); Alt-click resets every meter,
//! right-click picks the meter type and the options.
//!
//! Every number below is a named constant so it can be checked against Pro Tools and changed in
//! one place. Where a value comes from a published standard rather than from observing Pro Tools,
//! its comment says so.

use crate::{MeterDisplay, SoundApp};
use serde_json::{Value, json};
use soundcraft_dsp::meter::MeterKind;
use soundcraft_model::TrackId;

/// Readings at or above this linear sample peak (0 dBFS) light the clip indicator.
pub const CLIP_LEVEL: f32 = 1.0;

/// Seconds a peak hold line or clip light stays lit with the "timed" setting. Pro Tools' Display
/// preferences offer a 3 second hold (assumed equal for peak hold and clip indication).
pub const HOLD_SECS: f32 = 3.0;

/// Levels below this (linear, about -100 dBFS) read as silence, so a decaying bar ends.
pub const SILENCE: f32 = 1e-5;

/// Release (fall-back) rates, dB per second. `fast` is SoundCraft's original rate, kept as the
/// default until it is checked against Pro Tools' sample-peak meter. The others are the decay
/// rates of the IEC 60268-10 peak programme meters: Type I (DIN/Nordic, 20 dB in 1.7 s) and
/// Type II (BBC, 24 dB in 2.8 s).
pub const RELEASE_FAST_DB_S: f32 = 26.0;
pub const RELEASE_MEDIUM_DB_S: f32 = 20.0 / 1.7;
pub const RELEASE_SLOW_DB_S: f32 = 24.0 / 2.8;

/// The sample-peak (dBFS) scale: marks (numbered and ruled across the bar), from the top down.
pub const SCALE_MARKS: [f32; 8] = [0.0, -5.0, -10.0, -15.0, -20.0, -30.0, -40.0, -60.0];
/// The bottom of the dBFS scale.
pub const SCALE_FLOOR_DB: f32 = -80.0;
/// The dBFS scale's segments: `(dB at the top of the segment, position there)`, from the bottom
/// up, linear in dB within each: -80..-40 dB fills the bottom 10 %, -40..-20 the next 20 %,
/// -20..0 the top 70 %.
pub const SCALE_KNEES: [(f32, f32); 3] = [(-40.0, 0.1), (-20.0, 0.3), (0.0, 1.0)];
/// Where the dBFS bar turns from green to yellow, and from yellow to red.
pub const YELLOW_FROM_DB: f32 = -12.0;
pub const RED_FROM_DB: f32 = -3.0;

/// The VU scale (VU, 0 at [`soundcraft_dsp::meter::VU_REFERENCE_DBFS`]): the classic -20..+3
/// marks, linear in dB, red above 0 VU.
pub const VU_MARKS: [f32; 11] = [3.0, 2.0, 1.0, 0.0, -1.0, -2.0, -3.0, -5.0, -7.0, -10.0, -20.0];
const VU_KNEES: [(f32, f32); 1] = [(3.0, 1.0)];

/// K-System scales (0 at -12, -14 or -20 dBFS; full scale at the top): green to 0, yellow to +4,
/// red above, as the K-System specifies. The bottom is 40 dB below the 0 mark.
pub const K_FLOOR: f32 = -40.0;
pub const K_YELLOW_FROM: f32 = 0.0;
pub const K_RED_FROM: f32 = 4.0;
const K12_MARKS: [f32; 10] = [12.0, 8.0, 4.0, 0.0, -4.0, -8.0, -12.0, -20.0, -30.0, -40.0];
const K14_MARKS: [f32; 10] = [14.0, 8.0, 4.0, 0.0, -4.0, -8.0, -12.0, -20.0, -30.0, -40.0];
const K20_MARKS: [f32; 11] = [20.0, 12.0, 8.0, 4.0, 0.0, -4.0, -8.0, -12.0, -20.0, -30.0, -40.0];
const K12_KNEES: [(f32, f32); 2] = [(-20.0, 0.3), (12.0, 1.0)];
const K14_KNEES: [(f32, f32); 2] = [(-20.0, 0.3), (14.0, 1.0)];
const K20_KNEES: [(f32, f32); 2] = [(-20.0, 0.3), (20.0, 1.0)];

/// A meter's scale: where a reading sits on the bar, the marks beside it and the colour zones.
/// Scale units are dB relative to `zero_dbfs` (dBFS for peak and RMS meters, VU, K units).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    pub kind: MeterKind,
    /// The dBFS of the scale's 0.
    pub zero_dbfs: f32,
    /// The bottom (scale units).
    pub floor: f32,
    /// `(scale units at the top of a segment, bar position there)`, bottom up; the last is the top.
    pub knees: &'static [(f32, f32)],
    /// Marks, top down (scale units).
    pub marks: &'static [f32],
    pub yellow_from: f32,
    pub red_from: f32,
}

impl Scale {
    pub fn of(kind: MeterKind) -> Scale {
        let dbfs = Scale {
            kind,
            zero_dbfs: 0.0,
            floor: SCALE_FLOOR_DB,
            knees: &SCALE_KNEES,
            marks: &SCALE_MARKS,
            yellow_from: YELLOW_FROM_DB,
            red_from: RED_FROM_DB,
        };
        let k = |knees: &'static [(f32, f32)], marks: &'static [f32]| Scale {
            kind,
            zero_dbfs: kind.k_zero_dbfs().unwrap_or(0.0),
            floor: K_FLOOR,
            knees,
            marks,
            yellow_from: K_YELLOW_FROM,
            red_from: K_RED_FROM,
        };
        match kind {
            MeterKind::SamplePeak | MeterKind::TruePeak | MeterKind::Rms => dbfs,
            MeterKind::Vu => Scale {
                kind,
                zero_dbfs: soundcraft_dsp::meter::VU_REFERENCE_DBFS,
                floor: -20.0,
                knees: &VU_KNEES,
                marks: &VU_MARKS,
                yellow_from: 0.0,
                red_from: 0.0,
            },
            MeterKind::K12 => k(&K12_KNEES, &K12_MARKS),
            MeterKind::K14 => k(&K14_KNEES, &K14_MARKS),
            MeterKind::K20 => k(&K20_KNEES, &K20_MARKS),
        }
    }

    /// The scale of the session's meter type.
    pub fn of_session(s: &soundcraft_model::Session) -> Scale {
        Scale::of(MeterKind::parse(&s.edit.meter_type).unwrap_or_default())
    }

    /// Bar position (0..1) of a mark (scale units): piecewise linear between the knees.
    pub fn mark_pos(&self, u: f32) -> f32 {
        if u.is_nan() || u <= self.floor {
            return 0.0;
        }
        let (mut lo_u, mut lo_pos) = (self.floor, 0.0);
        for &(hi_u, hi_pos) in self.knees {
            if u < hi_u {
                return lo_pos + (u - lo_u) / (hi_u - lo_u).max(1e-6) * (hi_pos - lo_pos);
            }
            (lo_u, lo_pos) = (hi_u, hi_pos);
        }
        1.0
    }

    /// Bar position (0..1) of a dBFS value.
    pub fn db_pos(&self, dbfs: f32) -> f32 {
        self.mark_pos(dbfs - self.zero_dbfs)
    }

    /// Bar position (0..1) of a linear reading.
    pub fn level_pos(&self, v: f32) -> f32 {
        if v.is_nan() || v <= 1e-6 {
            return 0.0;
        }
        self.db_pos(20.0 * v.log10())
    }

    /// Bar positions where yellow and red begin.
    pub fn zones(&self) -> (f32, f32) {
        (self.mark_pos(self.yellow_from), self.mark_pos(self.red_from))
    }

    /// A mark's number: negative marks without their sign (as Pro Tools prints them), positive
    /// ones (VU, K) with a plus.
    pub fn mark_label(u: f32) -> String {
        if u > 0.0 { format!("+{u}") } else { format!("{}", u.abs()) }
    }
}

/// How long a peak hold line or a clip light stays lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// Not held: the hold line follows the bar; the clip light is lit only while the signal clips.
    None,
    /// [`HOLD_SECS`] after the last peak or clip.
    Timed,
    /// Until reset: click the meter or `ui.meters.reset` (hold line, clip light and peak
    /// readout), or Track › Clear All Clip Indicators (Alt+C; clip lights only).
    Infinite,
}

impl Hold {
    const ALL: [Hold; 3] = [Hold::None, Hold::Timed, Hold::Infinite];

    pub fn id(self) -> &'static str {
        match self {
            Hold::None => "none",
            Hold::Timed => "timed",
            Hold::Infinite => "infinite",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Hold::None => "None",
            Hold::Timed => "3 Seconds",
            Hold::Infinite => "Infinite",
        }
    }

    fn parse(s: &str) -> Option<Hold> {
        Hold::ALL.into_iter().find(|h| h.id() == s)
    }
}

/// How fast a meter falls back after a peak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Release {
    #[default]
    Fast,
    Medium,
    Slow,
}

impl Release {
    const ALL: [Release; 3] = [Release::Fast, Release::Medium, Release::Slow];

    pub fn id(self) -> &'static str {
        match self {
            Release::Fast => "fast",
            Release::Medium => "medium",
            Release::Slow => "slow",
        }
    }

    pub fn db_per_s(self) -> f32 {
        match self {
            Release::Fast => RELEASE_FAST_DB_S,
            Release::Medium => RELEASE_MEDIUM_DB_S,
            Release::Slow => RELEASE_SLOW_DB_S,
        }
    }

    pub fn label(self) -> String {
        let name = match self {
            Release::Fast => "Fast",
            Release::Medium => "Medium",
            Release::Slow => "Slow",
        };
        format!("{name} ({:.1} dB/s)", self.db_per_s())
    }

    fn parse(s: &str) -> Option<Release> {
        Release::ALL.into_iter().find(|r| r.id() == s)
    }

    /// The linear factor a level falls by over `dt` seconds.
    pub fn factor(self, dt: f32) -> f32 {
        10f32.powf(-self.db_per_s() * dt.max(0.0) / 20.0)
    }
}

/// Meter preferences (per machine, saved with the UI state; `ui.meters`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MeterPrefs {
    /// 3 seconds by default.
    pub peak_hold: Hold,
    /// Clip lights latch until cleared by default, as they always have.
    pub clip_hold: Hold,
    pub release: Release,
}

impl Default for MeterPrefs {
    fn default() -> Self {
        MeterPrefs { peak_hold: Hold::Timed, clip_hold: Hold::Infinite, release: Release::default() }
    }
}

/// Which meter a click or `ui.meters.reset` acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterTarget {
    Track(TrackId),
    /// The main output meters (Transport, toolbar).
    Main,
}

/// One poll's reading of a meter (linear; gain reduction in dB).
#[derive(Debug, Clone, Copy, Default)]
pub struct Reading {
    /// The meter type's reading of the two sides the bars show (mono strips repeat channel 0).
    pub level: [f32; 2],
    /// The sample peaks of those two sides.
    pub peak: [f32; 2],
    /// The highest peak over every channel of the strip (surround strips have more than the
    /// two bars of a stereo meter; on a true-peak meter, true peaks), for the clip light and
    /// the peak readout.
    pub peak_all: f32,
    pub gr: f32,
    pub kind: MeterKind,
}

impl Reading {
    /// `m` read on a `kind` meter. A reading measured as another type (the type just changed and
    /// the audio thread has not caught up) shows nothing rather than landing on the wrong scale;
    /// its sample peaks still count.
    pub fn of(m: &soundcraft_mix::StripMeter, kind: MeterKind) -> Reading {
        let level = if m.kind == kind { m.level } else { [0.0; 2] };
        Reading { level, peak: m.peak, peak_all: m.peak_all, gr: m.gain_reduction_db, kind }
    }

    /// What the clip light and peak readout look at: the highest sample peak of any channel,
    /// or true peak on a true-peak meter.
    fn loudest(&self) -> f32 {
        let tp = if self.kind == MeterKind::TruePeak { self.level } else { [0.0; 2] };
        self.peak.iter().chain(&tp).copied().map(Some).map(sane).fold(sane(Some(self.peak_all)), f32::max)
    }
}

fn sane(v: Option<f32>) -> f32 {
    v.filter(|v| v.is_finite()).unwrap_or(0.0).max(0.0)
}

/// Feed one poll's reading into a meter's display state, `dt` seconds after the previous frame.
/// Peak types attack instantly and fall back at the release rate; averaging types (RMS, VU, K)
/// carry their own ballistics from the audio thread and are shown as read.
pub fn feed(d: &mut MeterDisplay, r: &Reading, dt: f32, prefs: &MeterPrefs) {
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
    let fall = if r.kind.is_peak() { prefs.release.factor(dt) } else { 0.0 };
    let all = r.loudest();
    d.max_peak = d.max_peak.max(all);
    let clipping = all >= CLIP_LEVEL;
    for c in 0..2 {
        let v = sane(r.level.get(c).copied());
        let lvl = d.level.get(c).copied().unwrap_or(0.0);
        let nv = if v >= lvl { v } else { (lvl * fall).max(v) };
        if let Some(x) = d.level.get_mut(c) {
            *x = if nv < SILENCE { 0.0 } else { nv };
        }
        // Each side holds on its own: a silent side (a hard-panned source, a stopped transport)
        // must not keep restarting the other side's hold time.
        let (Some(h), Some(age)) = (d.hold.get_mut(c), d.hold_age.get_mut(c)) else { continue };
        if v > 0.0 && v >= *h {
            *h = v;
            *age = 0.0;
        } else {
            *age += dt;
        }
        let expired = match prefs.peak_hold {
            Hold::None => true,
            Hold::Timed => *age > HOLD_SECS,
            Hold::Infinite => false,
        };
        if expired {
            *h = d.level.get(c).copied().unwrap_or(0.0);
        }
    }
    if clipping {
        d.clip = true;
        d.clip_age = 0.0;
    } else {
        d.clip_age += dt;
        let clip_expired = match prefs.clip_hold {
            Hold::None => true,
            Hold::Timed => d.clip_age > HOLD_SECS,
            Hold::Infinite => false,
        };
        if clip_expired {
            d.clip = false;
        }
    }
    let gr = if r.gr.is_finite() { r.gr } else { 0.0 };
    d.gr = if gr > d.gr { gr } else { d.gr * prefs.release.factor(dt) };
}

/// Clear a meter's peak hold, clip light and highest-peak readout (the bar keeps falling; the
/// readout starts again from the next reading).
pub fn reset(d: &mut MeterDisplay) {
    d.hold = d.level;
    d.hold_age = [0.0; 2];
    d.clip = false;
    d.clip_age = 0.0;
    d.max_peak = 0.0;
}

/// A linear peak as the readout shows it: dBFS with one decimal, or "-inf".
pub fn peak_text(v: f32) -> String {
    if !v.is_finite() || v <= SILENCE {
        return "-inf".into();
    }
    format!("{:.1}", 20.0 * v.log10())
}

/// `ui.meters`: read or set the meter preferences. `{peak_hold?, clip_hold?: none|timed|infinite,
/// release?: fast|medium|slow}`; nothing changes unless every given value is valid.
pub fn prefs_command(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let mut next = app.ui.meters;
    for (key, slot) in [("peak_hold", &mut next.peak_hold), ("clip_hold", &mut next.clip_hold)] {
        if let Some(v) = p.get(key) {
            *slot = v.as_str().and_then(Hold::parse).ok_or_else(|| format!("`{key}` must be none, timed or infinite"))?;
        }
    }
    if let Some(v) = p.get("release") {
        next.release = v.as_str().and_then(Release::parse).ok_or("`release` must be fast, medium or slow")?;
    }
    app.ui.meters = next;
    Ok(prefs_json(&next))
}

fn prefs_json(m: &MeterPrefs) -> Value {
    json!({
        "peak_hold": m.peak_hold.id(),
        "clip_hold": m.clip_hold.id(),
        "release": m.release.id(),
        "release_db_per_s": m.release.db_per_s(),
        "hold_seconds": HOLD_SECS,
    })
}

/// `ui.meters.reset`: clear peak holds, clip lights and peak readouts. `{track?: id | name}` or
/// `{main: true}` (the output meters); with neither, every meter. `{track: "main"}` also means the
/// output meters unless a track is called that.
pub fn reset_command(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let session = app.engine.session();
    let by_name = |name: &str| session.track_by_name(name).map(|t| MeterTarget::Track(t.id));
    let target = match p.get("track") {
        _ if p.get("main").and_then(Value::as_bool) == Some(true) => Some(MeterTarget::Main),
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(match by_name(name) {
            Some(t) => t,
            None if name == "main" => MeterTarget::Main,
            None => return Err(format!("no track named `{name}`")),
        }),
        Some(v) => {
            let id = TrackId(v.as_u64().ok_or("`track` must be a track id or name")?);
            if session.track(id).is_none() {
                return Err(format!("no track {}", id.0));
            }
            Some(MeterTarget::Track(id))
        }
    };
    match target {
        None => app.meters.values_mut().chain([&mut app.main_meter]).for_each(reset),
        Some(MeterTarget::Main) => reset(&mut app.main_meter),
        Some(MeterTarget::Track(id)) => reset(app.meters.entry(id).or_default()),
    }
    Ok(json!({"reset": target.map_or_else(|| "all".to_string(), |t| match t {
        MeterTarget::Main => "main".into(),
        MeterTarget::Track(id) => id.0.to_string(),
    })}))
}

/// Make a drawn meter clickable: click resets it, Alt-click resets every meter, right-click opens
/// the meter options.
pub fn interact(app: &mut SoundApp, ui: &mut egui::Ui, r: egui::Rect, target: MeterTarget) {
    let key = match target {
        MeterTarget::Track(id) => ("meter", id.0),
        MeterTarget::Main => ("meter", u64::MAX),
    };
    let resp = ui.interact(r, ui.id().with(key), egui::Sense::click());
    if resp.clicked() {
        let all = ui.input(|i| i.modifiers.alt);
        let params = match target {
            _ if all => json!({}),
            MeterTarget::Main => json!({"main": true}),
            MeterTarget::Track(id) => json!({"track": id.0}),
        };
        let _ = app.run("ui.meters.reset", params);
    }
    let kind = Scale::of_session(app.engine.session()).kind;
    let fader = if app.engine.session().edit.pre_fader_metering { "pre-fader" } else { "post-fader" };
    let resp = resp.on_hover_text(format!(
        "{} meter, {fader}\nClick: reset peak hold and clip · Alt-click: reset all meters · right-click: meter type and options",
        kind.label()
    ));
    resp.context_menu(|ui| options_menu(app, ui));
}

/// The meter options (right-click a meter).
pub fn options_menu(app: &mut SoundApp, ui: &mut egui::Ui) {
    let kind = Scale::of_session(app.engine.session()).kind;
    ui.menu_button("Meter Type", |ui| {
        for k in MeterKind::ALL {
            if ui.radio(kind == k, k.label()).clicked() {
                let _ = app.run("options.meter_type", json!({"type": k.id()}));
                ui.close();
            }
        }
        ui.separator();
        if ui.button(format!("Next ({})", kind.next().label())).clicked() {
            let _ = app.run("options.meter_type", json!({}));
            ui.close();
        }
    });
    let mut pre = app.engine.session().edit.pre_fader_metering;
    if ui.checkbox(&mut pre, "Pre-Fader Metering").clicked() {
        let _ = app.run("options.pre_fader_metering", json!({"value": pre}));
        ui.close();
    }
    ui.separator();
    let cur = app.ui.meters;
    for (key, title, now) in [("peak_hold", "Peak Hold", cur.peak_hold), ("clip_hold", "Clip Indication", cur.clip_hold)] {
        ui.menu_button(title, |ui| {
            for h in Hold::ALL {
                if ui.radio(now == h, h.label()).clicked() {
                    let _ = app.run("ui.meters", json!({ key: h.id() }));
                    ui.close();
                }
            }
        });
    }
    ui.menu_button("Release", |ui| {
        for r in Release::ALL {
            if ui.radio(cur.release == r, r.label()).clicked() {
                let _ = app.run("ui.meters", json!({"release": r.id()}));
                ui.close();
            }
        }
    });
    ui.separator();
    if ui.button("Reset All Meters").clicked() {
        let _ = app.run("ui.meters.reset", json!({}));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;

    const FRAME: f32 = 1.0 / 60.0;

    fn pk(v: [f32; 2]) -> Reading {
        Reading { level: v, peak: v, peak_all: v[0].max(v[1]), gr: 0.0, kind: MeterKind::SamplePeak }
    }

    fn run_for_each(d: &mut MeterDisplay, secs: f32, peak: [f32; 2], prefs: &MeterPrefs) {
        let n = (secs / FRAME).round() as usize;
        for _ in 0..n {
            feed(d, &pk(peak), FRAME, prefs);
        }
    }

    fn run_for(d: &mut MeterDisplay, secs: f32, peak: f32, prefs: &MeterPrefs) {
        let n = (secs / FRAME).round() as usize;
        for _ in 0..n {
            feed(d, &pk([peak; 2]), FRAME, prefs);
        }
    }

    #[test]
    fn timed_hold_lasts_hold_secs_and_infinite_until_reset() {
        let timed = MeterPrefs::default();
        let mut d = MeterDisplay::default();
        feed(&mut d, &pk([0.5; 2]), FRAME, &timed);
        run_for(&mut d, HOLD_SECS - 0.1, 0.0, &timed);
        assert_eq!(d.hold[0], 0.5, "still held");
        run_for(&mut d, 0.2, 0.0, &timed);
        assert!(d.hold[0] < 0.5, "released after the hold time");
        let inf = MeterPrefs { peak_hold: Hold::Infinite, ..MeterPrefs::default() };
        let mut d = MeterDisplay::default();
        feed(&mut d, &pk([0.5; 2]), FRAME, &inf);
        run_for(&mut d, 60.0, 0.0, &inf);
        assert_eq!(d.hold[0], 0.5);
        assert_eq!(d.level[0], 0.0, "the bar fell back");
        reset(&mut d);
        assert_eq!(d.hold[0], 0.0);
        let none = MeterPrefs { peak_hold: Hold::None, ..MeterPrefs::default() };
        feed(&mut d, &pk([0.5; 2]), FRAME, &none);
        run_for(&mut d, 0.5, 0.0, &none);
        assert_eq!(d.hold, d.level);
    }

    #[test]
    fn a_silent_side_does_not_keep_the_other_sides_hold() {
        // Hard-panned mono on a stereo meter: the right side reads nothing at all.
        let prefs = MeterPrefs::default();
        let mut d = MeterDisplay::default();
        feed(&mut d, &pk([0.5, 0.0]), FRAME, &prefs);
        run_for_each(&mut d, HOLD_SECS + 0.2, [0.01, 0.0], &prefs);
        assert!(d.hold[0] < 0.5, "the left hold released: {d:?}");
    }

    #[test]
    fn clip_light_follows_clip_indication() {
        for (hold, lit_after_10s) in [(Hold::Infinite, true), (Hold::Timed, false), (Hold::None, false)] {
            let prefs = MeterPrefs { clip_hold: hold, ..MeterPrefs::default() };
            let mut d = MeterDisplay::default();
            feed(&mut d, &pk([1.2, 0.1]), FRAME, &prefs);
            assert!(d.clip);
            feed(&mut d, &pk([0.1; 2]), FRAME, &prefs);
            assert_eq!(d.clip, hold != Hold::None, "{hold:?} right after");
            run_for(&mut d, 10.0, 0.1, &prefs);
            assert_eq!(d.clip, lit_after_10s, "{hold:?}");
        }
    }

    #[test]
    fn release_rates_fall_by_their_db_per_second() {
        for r in Release::ALL {
            let prefs = MeterPrefs { release: r, ..MeterPrefs::default() };
            let mut d = MeterDisplay::default();
            feed(&mut d, &pk([1.0; 2]), FRAME, &prefs);
            run_for(&mut d, 1.0, 0.0, &prefs);
            let fell = -20.0 * d.level[0].log10();
            assert!((fell - r.db_per_s()).abs() < 0.5, "{r:?}: {fell}");
        }
    }

    #[test]
    fn hostile_readings_do_not_stick() {
        let mut d = MeterDisplay::default();
        let bad = [f32::NAN, f32::INFINITY];
        feed(
            &mut d,
            &Reading { level: bad, peak: bad, peak_all: f32::NAN, gr: f32::NAN, kind: MeterKind::TruePeak },
            f32::NAN,
            &MeterPrefs::default(),
        );
        assert!(d.level.iter().chain(&d.hold).all(|x| x.is_finite()) && d.gr.is_finite() && d.max_peak.is_finite());
    }

    #[test]
    fn a_clip_on_any_channel_lights_the_clip_and_the_readout() {
        // A 5.1 strip whose bars (L, R) are quiet while the LFE clips.
        let mut d = MeterDisplay::default();
        feed(&mut d, &Reading { level: [0.1; 2], peak: [0.1; 2], peak_all: 1.2, ..Reading::default() }, FRAME, &MeterPrefs::default());
        assert!(d.clip && d.max_peak == 1.2 && d.level == [0.1; 2], "{d:?}");
    }

    #[test]
    fn clear_all_clip_indicators_leaves_holds_and_readouts() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let kick = app.engine.session().track_by_name("Kick").unwrap().id;
        let d = app.meters.entry(kick).or_default();
        (d.clip, d.hold, d.max_peak) = (true, [0.9; 2], 1.1);
        app.run("track.clear_clip_indicators", json!({})).unwrap();
        let d = app.meters[&kick];
        assert!(!d.clip && d.hold == [0.9; 2] && d.max_peak == 1.1, "{d:?}");
    }

    #[test]
    fn defaults_latch_clips_and_hold_peaks_for_three_seconds() {
        let prefs = MeterPrefs::default();
        assert_eq!((prefs.peak_hold, prefs.clip_hold), (Hold::Timed, Hold::Infinite));
        let ui: crate::UiState = serde_json::from_value(json!({"meters": {"release": "slow"}})).unwrap();
        assert_eq!(ui.meters.clip_hold, Hold::Infinite, "missing fields take the defaults");
    }

    #[test]
    fn averaging_meters_show_their_reading_and_clip_on_sample_peaks() {
        let prefs = MeterPrefs::default();
        let mut d = MeterDisplay::default();
        let rms = |level: f32, peak: f32| Reading { level: [level; 2], peak: [peak; 2], peak_all: peak, gr: 0.0, kind: MeterKind::Rms };
        feed(&mut d, &rms(0.5, 1.0), FRAME, &prefs);
        assert!(d.clip && d.max_peak == 1.0 && d.level[0] == 0.5);
        feed(&mut d, &rms(0.2, 0.3), FRAME, &prefs);
        assert_eq!(d.level[0], 0.2, "no display release on top of the RMS ballistics");
        // True peak: an inter-sample over lights the clip light though no sample reached 0 dBFS.
        let mut d = MeterDisplay::default();
        feed(&mut d, &Reading { level: [1.1; 2], peak: [0.9; 2], peak_all: 0.9, gr: 0.0, kind: MeterKind::TruePeak }, FRAME, &prefs);
        assert!(d.clip);
    }

    #[test]
    fn scales_place_their_zero_and_marks() {
        let peak = Scale::of(MeterKind::SamplePeak);
        assert_eq!((peak.db_pos(0.0), peak.db_pos(-80.0), peak.db_pos(-20.0)), (1.0, 0.0, 0.3));
        let k14 = Scale::of(MeterKind::K14);
        assert_eq!(k14.db_pos(0.0), 1.0, "full scale is the top of a K meter");
        assert!((k14.db_pos(-14.0) - k14.mark_pos(0.0)).abs() < 1e-6 && k14.mark_pos(0.0) > 0.7);
        let vu = Scale::of(MeterKind::Vu);
        assert_eq!(vu.db_pos(soundcraft_dsp::meter::VU_REFERENCE_DBFS + 3.0), 1.0);
        assert!((vu.db_pos(soundcraft_dsp::meter::VU_REFERENCE_DBFS) - 20.0 / 23.0).abs() < 1e-5);
        for k in MeterKind::ALL {
            let s = Scale::of(k);
            assert!(s.marks.windows(2).all(|w| w[0] > w[1]), "{k:?} marks run top down");
            assert!(s.marks.iter().all(|m| (0.0..=1.0).contains(&s.mark_pos(*m))));
            assert_eq!(s.level_pos(f32::NAN), 0.0);
        }
        assert_eq!((Scale::mark_label(-10.0), Scale::mark_label(4.0), Scale::mark_label(0.0)), ("10".into(), "+4".into(), "0".into()));
    }

    #[test]
    fn peak_readout_keeps_the_highest_peak_until_reset() {
        let prefs = MeterPrefs::default();
        let mut d = MeterDisplay::default();
        feed(&mut d, &pk([0.5, 0.25]), FRAME, &prefs);
        run_for(&mut d, 10.0, 0.01, &prefs);
        assert_eq!(peak_text(d.max_peak), "-6.0");
        reset(&mut d);
        assert_eq!(peak_text(d.max_peak), "-inf", "cleared");
        feed(&mut d, &pk([0.01; 2]), FRAME, &prefs);
        assert_eq!(peak_text(d.max_peak), "-40.0", "and counting again");
        assert_eq!(peak_text(0.0), "-inf");
    }

    #[test]
    fn a_reading_of_another_meter_type_does_not_reach_the_scale() {
        let sample = soundcraft_mix::StripMeter { level: [0.5; 2], peak: [0.5; 2], peak_all: 0.5, ..Default::default() };
        let r = Reading::of(&sample, MeterKind::Vu);
        assert_eq!((r.level, r.peak_all), ([0.0; 2], 0.5));
        let mut d = MeterDisplay::default();
        feed(&mut d, &r, FRAME, &MeterPrefs::default());
        assert_eq!((d.level, d.hold), ([0.0; 2], [0.0; 2]), "no +12 VU flash latched in the hold");
        let vu = soundcraft_mix::StripMeter { kind: MeterKind::Vu, ..sample };
        assert_eq!(Reading::of(&vu, MeterKind::Vu).level, [0.5; 2]);
    }

    #[test]
    fn changing_the_meter_type_resets_every_meter() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let kick = app.engine.session().track_by_name("Kick").unwrap().id;
        let d = app.meters.entry(kick).or_default();
        (d.clip, d.hold, d.max_peak) = (true, [0.9; 2], 1.1);
        app.main_meter.max_peak = 0.7;
        assert_eq!(app.run("options.meter_type", json!({"type": "vu"})).unwrap()["type"], "vu");
        assert!(!app.meters[&kick].clip && app.meters[&kick].max_peak == 0.0 && app.main_meter.max_peak == 0.0);
        assert_eq!(Scale::of_session(app.engine.session()).kind, MeterKind::Vu);
    }

    #[test]
    fn commands_set_prefs_and_reset_meters() {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
        let r = app.run("ui.meters", json!({"peak_hold": "infinite", "release": "slow"})).unwrap();
        assert_eq!((r["peak_hold"].as_str(), r["clip_hold"].as_str(), r["release"].as_str()), (Some("infinite"), Some("infinite"), Some("slow")));
        assert!(app.run("ui.meters", json!({"clip_hold": "forever", "peak_hold": "none"})).is_err());
        assert_eq!(app.ui.meters.peak_hold, Hold::Infinite, "a refused call changes nothing");
        let kick = app.engine.session().track_by_name("Kick").unwrap().id;
        let snare = app.engine.session().track_by_name("Snare").unwrap().id;
        for id in [kick, snare] {
            let d = app.meters.entry(id).or_default();
            (d.clip, d.hold, d.max_peak) = (true, [0.9; 2], 1.1);
        }
        app.main_meter.clip = true;
        app.run("ui.meters.reset", json!({"track": kick.0})).unwrap();
        assert!(!app.meters[&kick].clip && app.meters[&kick].hold == [0.0; 2] && app.meters[&kick].max_peak == 0.0);
        assert!(app.meters[&snare].clip && app.main_meter.clip, "only that meter");
        app.run("ui.meters.reset", json!({"track": "main"})).unwrap();
        app.meters.entry(kick).or_default().clip = true;
        app.run("ui.meters.reset", json!({"track": "Kick"})).unwrap();
        assert!(!app.meters[&kick].clip, "by name");
        assert!(app.run("ui.meters.reset", json!({"track": "Nobody"})).is_err());
        app.main_meter.clip = true;
        app.run("ui.meters.reset", json!({"main": true})).unwrap();
        assert!(!app.main_meter.clip);
        // A track called "main" is reachable by name; {"main": true} still means the output.
        let main_track = app.engine.session().track_by_name("Bass").unwrap().id;
        app.engine.execute("track.rename", &json!({"track": main_track.0, "name": "main"})).unwrap();
        (app.meters.entry(main_track).or_default().clip, app.main_meter.clip) = (true, true);
        app.run("ui.meters.reset", json!({"track": "main"})).unwrap();
        assert!(!app.meters[&main_track].clip && app.main_meter.clip);
        assert!(app.meters[&snare].clip, "the others stay lit");
        assert!(app.run("ui.meters.reset", json!({"track": 999_999})).is_err());
        app.run("ui.meters.reset", json!({})).unwrap();
        assert!(!app.meters[&snare].clip);
        // Saved with the UI state, and older prefs without it get the defaults.
        let ui: crate::UiState = serde_json::from_value(json!({})).unwrap();
        assert_eq!(ui.meters, MeterPrefs::default());
        let back: crate::UiState = serde_json::from_value(serde_json::to_value(&app.ui).unwrap()).unwrap();
        assert_eq!(back.meters, app.ui.meters);
    }
}
