//! Alt+Shift on a selected track's fader moves every selected track's fader by the same amount, as
//! Alt+Shift applies a change to the selected channel strips in Pro Tools; a plain drag moves one.

use egui::{Event, Modifiers, PointerButton, Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

const START: [f32; 4] = [-10.0, -4.0, -20.0, -7.0];

/// Kick, Snare and Bass selected (Bass hidden), Keys not; every track at a different level.
fn harness() -> (Harness<'static, SoundApp>, [TrackId; 4]) {
    let mut engine = soundcraft_engine::Engine::default();
    let mut ids = Vec::new();
    for (name, db) in ["Kick", "Snare", "Bass", "Keys"].iter().zip(START) {
        engine.execute("track.new", &json!({"name": name})).unwrap();
        let id = engine.session().tracks.last().unwrap().id;
        engine.execute("mix.volume", &json!({"track": id.0, "db": db})).unwrap();
        ids.push(id);
    }
    let ids: [TrackId; 4] = ids.try_into().unwrap();
    engine.session_mut().track_mut(ids[2]).unwrap().hidden = true;
    engine.session_mut().edit.selected_tracks = vec![ids[0], ids[1], ids[2]];
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = MainWindow::Mix;
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    (h, ids)
}

fn volumes(h: &Harness<SoundApp>, ids: &[TrackId; 4]) -> [f32; 4] {
    ids.map(|id| h.state().engine.session().track(id).unwrap().mixer.volume_db)
}

fn press(h: &mut Harness<SoundApp>, at: Pos2, pressed: bool, modifiers: Modifiers) {
    h.event(Event::ModifiersChanged(modifiers));
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers });
    h.step();
}

/// Drags Kick's fader cap up by `dy` points with `modifiers` held, in a few moves.
fn drag_kick(h: &mut Harness<SoundApp>, dy: f32, modifiers: Modifiers) {
    let fader = h.get_by_label("Kick volume").rect();
    let from = fader.center() - vec2(0.0, fader.height() * 0.25);
    h.hover_at(from);
    h.step();
    press(h, from, true, modifiers);
    for f in [0.25, 0.5, 1.0] {
        h.hover_at(from - vec2(0.0, dy * f));
        h.step();
    }
    press(h, from - vec2(0.0, dy), false, modifiers);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.run_steps(2);
}

/// Drags Kick's fader with Alt+Shift through `path` (points above where the drag starts, in one
/// gesture) and returns the volumes after each move.
fn drag_kick_through(h: &mut Harness<SoundApp>, ids: &[TrackId; 4], path: &[f32]) -> Vec<[f32; 4]> {
    let both = Modifiers::ALT | Modifiers::SHIFT;
    let fader = h.get_by_label("Kick volume").rect();
    let from = fader.center() - vec2(0.0, fader.height() * 0.25);
    h.hover_at(from);
    h.step();
    press(h, from, true, both);
    let mut seen = Vec::new();
    for dy in path {
        h.hover_at(from - vec2(0.0, *dy));
        h.step();
        seen.push(volumes(h, ids));
    }
    press(h, from - vec2(0.0, *path.last().unwrap_or(&0.0)), false, both);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.run_steps(2);
    seen
}

#[test]
fn a_selected_fader_held_at_the_top_gets_its_distance_back_on_the_way_down() {
    let (mut h, ids) = harness();
    let seen = drag_kick_through(&mut h, &ids, &[40.0, 80.0, 110.0, 130.0, 150.0, 130.0, 80.0, 40.0, 0.0]);
    assert!(seen.iter().any(|v| v[1] == 12.0 && v[0] < 12.0), "Snare stopped at the top while Kick went on: {seen:?}");
    let v = volumes(&h, &ids);
    assert!((v[1] - v[0] - (START[1] - START[0])).abs() < 1e-3, "back down, Snare is {} dB above Kick again: {v:?}", START[1] - START[0]);
    assert_eq!(v[3], START[3]);
}

#[test]
fn a_drag_up_from_minus_infinity_and_back_returns_the_selected_faders() {
    let (mut h, ids) = harness();
    h.state_mut().engine.session_mut().track_mut(ids[0]).unwrap().mixer.volume_db = -144.0;
    h.run_steps(2);
    let seen = drag_kick_through(&mut h, &ids, &[12.0, 24.0, 12.0, 0.0]);
    for v in &seen {
        let want = (START[1] + v[0] + 144.0).min(12.0);
        assert!((v[1] - want).abs() < 1e-3, "Snare follows Kick's dB change from where it began: {v:?}");
    }
    let v = volumes(&h, &ids);
    assert_eq!((v[0], v[1], v[3]), (-144.0, START[1], START[3]), "back at the bottom, every fader is where it began: {v:?}");
}

#[test]
fn alt_shift_drag_moves_the_selected_faders_by_the_same_amount_in_one_undo_step() {
    let (mut h, ids) = harness();
    drag_kick(&mut h, 40.0, Modifiers::ALT | Modifiers::SHIFT);
    let v = volumes(&h, &ids);
    let d = v[0] - START[0];
    assert!(d > 1.0, "the dragged fader moved: {v:?}");
    assert!((v[1] - (START[1] + d)).abs() < 1e-3, "the other selected strip keeps its distance: {v:?}");
    assert_eq!(v[2], START[2], "a hidden track has no strip, so it stays where it was");
    assert_eq!(v[3], START[3], "an unselected track stays where it was");

    assert!(h.state_mut().engine.undo());
    assert_eq!(volumes(&h, &ids), START, "one undo puts every fader back");
}

#[test]
fn a_plain_drag_moves_only_its_own_fader() {
    let (mut h, ids) = harness();
    drag_kick(&mut h, 40.0, Modifiers::NONE);
    let v = volumes(&h, &ids);
    assert!(v[0] - START[0] > 1.0, "the dragged fader moved: {v:?}");
    assert_eq!(v[1..], START[1..]);
}

#[test]
fn alt_shift_click_sets_the_selected_faders_to_zero() {
    let (mut h, ids) = harness();
    let fader = h.get_by_label("Kick volume").rect();
    h.hover_at(fader.center());
    h.step();
    let both = Modifiers::ALT | Modifiers::SHIFT;
    press(&mut h, fader.center(), true, both);
    press(&mut h, fader.center(), false, both);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.run_steps(2);
    assert_eq!(volumes(&h, &ids), [0.0, 0.0, START[2], START[3]]);
}
