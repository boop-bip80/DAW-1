//! Double-clicking a fade opens the Fades dialog aimed at that fade; double-clicking a crossfade
//! aims it at the pair. OK applies the chosen shape to what was aimed at, in one undo step.

use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{Clip, ClipId, Fade, FadeShape, Source, SourceId, TrackId};
use soundcraft_ui_egui::dialogs::{Dialog, FadeTarget};
use soundcraft_ui_egui::{Services, SoundApp, edit_window};

const SEC: i64 = 48_000;
/// Every fade here is 20 000 samples long (50 px at the zoom used).
const FADE: i64 = 20_000;

struct Rig {
    ctx: egui::Context,
    app: SoundApp,
    track: TrackId,
}

impl Rig {
    /// One track with `clips` (start, length, fade-in shape, fade-out shape; a shape means a `FADE`-long fade).
    fn new(tool: &str, clips: &[(i64, i64, Option<FadeShape>, Option<FadeShape>)]) -> (Rig, Vec<ClipId>) {
        let mut e = Engine::default();
        let track = TrackId(e.execute("track.new", &json!({"count": 1})).unwrap()["tracks"][0].as_u64().unwrap());
        let s = e.session_mut();
        s.sources.push(Source {
            id: SourceId(7),
            name: "src".into(),
            path: "src.wav".into(),
            channels: 1,
            frames: u64::try_from(30 * SEC).unwrap(),
            sample_rate: 48_000,
            format: soundcraft_audio_io::FileFormat::Wav,
            time_reference: 0,
            unsaved: false,
        });
        let mut ids = Vec::new();
        for &(start, len, fade_in, fade_out) in clips {
            let id = s.new_clip_id();
            let mut c = Clip::audio(id, "c", SourceId(7), SEC + start, start, len);
            let fade = |shape: Option<FadeShape>| shape.map_or(Fade::default(), |shape| Fade { len: FADE, shape, curvature: 0.0 });
            c.fade_in = fade(fade_in);
            c.fade_out = fade(fade_out);
            s.track_mut(track).and_then(|t| t.playlist_mut()).unwrap().clips.push(c);
            ids.push(id);
        }
        e.execute("edit.mode", &json!({"mode": "slip"})).unwrap();
        e.execute("edit.tool", &json!({"tool": tool})).unwrap();
        e.execute("view.zoom_set", &json!({"samples_per_px": 400.0})).unwrap();
        let mut r = Rig { ctx: egui::Context::default(), app: SoundApp::new(e, None, Services::default()), track };
        for _ in 0..4 {
            r.frame(vec![]);
        }
        (r, ids)
    }

    fn frame(&mut self, events: Vec<Event>) {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
        self.ctx
            .run_ui(raw, |ui| {
                self.app.logic(ui.ctx());
                self.app.ui(ui);
            })
            .textures_delta
            .clear();
    }

    /// The point at sample `at` on the track, `down` of the way from the top of its row to the bottom.
    fn point(&self, at: i64, down: f32) -> Pos2 {
        let s = self.app.engine.session();
        let [x0, y0, x1, y1] = self.app.edit_layout.timeline;
        let row = self.app.edit_layout.rows.iter().find(|(id, _)| *id == self.track.0).unwrap().1;
        let x = edit_window::x_of(s, Rect::from_min_max(pos2(x0, y0), pos2(x1, y1)), at);
        pos2(x, row[1] + s.track(self.track).unwrap().height.points() * down)
    }

    fn double_click(&mut self, at: Pos2) {
        // A second of frames first: egui counts a click within 0.6 s of the one before last as a triple-click.
        for _ in 0..60 {
            self.frame(vec![]);
        }
        for pressed in [true, false, true, false] {
            self.frame(vec![
                Event::PointerMoved(at),
                Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE },
            ]);
        }
        self.frame(vec![]);
    }

    fn target(&self) -> Option<FadeTarget> {
        match &self.app.dialogs.open {
            Some(Dialog::Fades { target, .. }) => *target,
            _ => None,
        }
    }

    fn choose(&mut self, shape: &str) {
        if let Some(Dialog::Fades { shape: chosen, .. }) = &mut self.app.dialogs.open {
            *chosen = shape.into();
        }
    }

    fn ok(&mut self) {
        for pressed in [true, false] {
            self.frame(vec![Event::Key { key: Key::Enter, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE }]);
        }
        self.frame(vec![]);
    }

    fn clip(&self, id: ClipId) -> Clip {
        self.app.engine.session().find_clip(id).unwrap().1.clone()
    }
}

fn shaped(shape: FadeShape) -> Fade {
    Fade { len: FADE, shape, curvature: 0.0 }
}

#[test]
fn double_clicking_a_fade_aims_the_dialog_at_that_fade_and_ok_changes_only_it() {
    for tool in ["grabber", "selector"] {
        for fade_in in [true, false] {
            let (mut r, ids) = Rig::new(tool, &[(0, 2 * SEC, Some(FadeShape::EqualPower), Some(FadeShape::Linear))]);
            let x = ids[0];
            let at = if fade_in { FADE / 2 } else { 2 * SEC - FADE / 2 };
            r.double_click(r.point(at, 0.75));
            assert_eq!(r.target(), Some(FadeTarget::Single { clip: x, fade_in }), "{tool} {fade_in}");
            assert_eq!(r.app.engine.session().edit.selected_clips, vec![x], "the double-click still selects the clip");
            assert!(matches!(&r.app.dialogs.open, Some(Dialog::Fades { shape, .. }) if shape == if fade_in { "equal power" } else { "linear" }));
            // OK changes the shape of that fade, and only that fade, in one undo step.
            r.choose("s-curve");
            let before = r.app.engine.session().clone();
            let steps = r.app.engine.undo_history().len();
            r.ok();
            let c = r.clip(x);
            let (hit, other) = if fade_in { (c.fade_in, c.fade_out) } else { (c.fade_out, c.fade_in) };
            assert_eq!(hit, shaped(FadeShape::SCurve), "{tool} {fade_in}");
            assert_eq!(other, if fade_in { shaped(FadeShape::Linear) } else { shaped(FadeShape::EqualPower) });
            assert_eq!(r.app.engine.undo_history().len(), steps + 1);
            assert!(r.app.dialogs.open.is_none());
            assert!(r.app.engine.undo());
            assert_eq!(r.app.engine.session(), &before);
        }
    }
}

#[test]
fn double_clicking_a_crossfade_aims_the_dialog_at_the_pair_and_ok_changes_both_in_one_step() {
    for tool in ["grabber", "smart"] {
        // B starts 20 000 before A ends: a crossfade over 76 000..96 000.
        let b_start = 2 * SEC - FADE;
        let spans = [(0, 2 * SEC, None, Some(FadeShape::Linear)), (b_start, 2 * SEC, Some(FadeShape::Linear), None)];
        let (mut r, ids) = Rig::new(tool, &spans);
        let (a, b) = (ids[0], ids[1]);
        r.double_click(r.point(b_start + FADE / 2, 0.75));
        assert_eq!(r.target(), Some(FadeTarget::Crossfade { later: b }), "{tool}");
        r.choose("exponential");
        let before = r.app.engine.session().clone();
        let steps = r.app.engine.undo_history().len();
        r.ok();
        assert_eq!((r.clip(a).fade_out, r.clip(b).fade_in), (shaped(FadeShape::Exponential), shaped(FadeShape::Exponential)), "{tool}");
        assert_eq!((r.clip(a).end() - r.clip(b).start, r.clip(a).start, r.clip(b).end()), (FADE, 0, b_start + 2 * SEC));
        assert_eq!(r.app.engine.undo_history().len(), steps + 1, "both fades in one undo step");
        assert!(r.app.engine.undo());
        assert_eq!(r.app.engine.session(), &before);
    }
}

#[test]
fn the_smart_tool_opens_the_fade_dialog_from_both_halves_of_the_clip() {
    for down in [0.2, 0.75] {
        let (mut r, ids) = Rig::new("smart", &[(0, 2 * SEC, Some(FadeShape::SCurve), None)]);
        r.double_click(r.point(FADE / 2, down));
        assert_eq!(r.target(), Some(FadeTarget::Single { clip: ids[0], fade_in: true }), "down {down}");
    }
}

#[test]
fn double_clicking_a_clip_outside_its_fades_still_only_selects_it() {
    let (mut r, ids) = Rig::new("grabber", &[(0, 2 * SEC, Some(FadeShape::EqualPower), Some(FadeShape::EqualPower))]);
    r.double_click(r.point(SEC, 0.75));
    assert_eq!(r.app.dialogs.open_name(), None);
    assert_eq!(r.app.engine.session().edit.selected_clips, vec![ids[0]]);
    // A clip without fades has no fade to aim at, at its edges either.
    let (mut r, _) = Rig::new("grabber", &[(0, 2 * SEC, None, None)]);
    r.double_click(r.point(FADE / 2, 0.75));
    assert_eq!(r.app.dialogs.open_name(), None);
}

#[test]
fn the_menu_still_opens_the_fades_dialog_for_the_selection() {
    let (mut r, _) = Rig::new("grabber", &[(0, 2 * SEC, None, None)]);
    soundcraft_ui_egui::menus::invoke_menu(&mut r.app, "edit.fades_create", "Edit > Fades > Create...");
    r.frame(vec![]);
    assert!(matches!(&r.app.dialogs.open, Some(Dialog::Fades { target: None, shape }) if shape == "equal power"));
}

#[test]
fn ok_without_a_new_shape_changes_nothing_and_keeps_the_bend() {
    let (mut r, ids) = Rig::new("grabber", &[(0, 2 * SEC, Some(FadeShape::EqualPower), None)]);
    r.app.engine.session_mut().find_clip_mut(ids[0]).unwrap().fade_in.curvature = 0.35;
    r.frame(vec![]);
    r.double_click(r.point(FADE / 2, 0.75));
    assert!(r.target().is_some());
    let before = r.app.engine.session().clone();
    let steps = r.app.engine.undo_history().len();
    r.ok();
    assert!(r.app.dialogs.open.is_none());
    assert_eq!(r.app.engine.session(), &before);
    assert_eq!(r.app.engine.undo_history().len(), steps);
}

#[test]
fn only_the_tools_that_select_open_the_dialog_and_the_gain_icon_keeps_its_own_double_click() {
    for tool in ["zoom", "scrubber", "pencil"] {
        let (mut r, _) = Rig::new(tool, &[(0, 2 * SEC, Some(FadeShape::EqualPower), None)]);
        r.double_click(r.point(FADE / 2, 0.75));
        assert_eq!(r.app.dialogs.open_name(), None, "{tool}");
    }
    // The clip-gain icon sits at the bottom left of the clip, inside its fade-in.
    let (mut r, ids) = Rig::new("grabber", &[(0, 2 * SEC, Some(FadeShape::EqualPower), None)]);
    r.app.engine.session_mut().find_clip_mut(ids[0]).unwrap().gain_db = 3.0;
    r.frame(vec![]);
    let beside_the_icon = r.point(0, 0.5) + vec2(8.0, 0.0);
    let on_the_icon = r.point(0, 1.0) + vec2(8.0, -9.0);
    r.double_click(on_the_icon);
    assert_eq!(r.app.dialogs.open_name(), None);
    r.double_click(beside_the_icon);
    assert_eq!(r.target(), Some(FadeTarget::Single { clip: ids[0], fade_in: true }), "the same stretch of fade opens the dialog elsewhere");
}
