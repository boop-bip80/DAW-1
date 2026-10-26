use egui::accesskit::Role;
use egui::{Event, Key, Modifiers, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{Services, SoundApp, menus};

/// Loops the Snare clip three times, selects it and opens Clip > Unloop... from the menu.
fn unloop_dialog() -> (Harness<'static, SoundApp>, TrackId, usize) {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let track = app.engine.session().track_by_name("Snare").unwrap().id;
    let clip = app.engine.session().track(track).unwrap().clips()[0].id;
    app.run("clip.loop", json!({"clip": clip.0, "count": 3})).unwrap();
    app.run("edit.select", json!({"clips": [clip.0]})).unwrap();
    let looped = app.engine.session().track(track).unwrap().clips().len();
    menus::invoke_menu(&mut app, "clip.unloop", "Clip > Unloop...");
    assert_eq!(app.dialogs.open_name(), Some("unloop"));
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    (h, track, looped)
}

fn clips(h: &Harness<'_, SoundApp>, track: TrackId) -> Vec<Option<u64>> {
    h.state().engine.session().track(track).map(|t| t.clips().iter().map(|c| c.loop_id).collect()).unwrap_or_default()
}

#[test]
fn unloop_dialogs_flatten_keeps_each_iteration_as_its_own_clip() {
    let (mut h, track, looped) = unloop_dialog();
    assert_eq!(clips(&h, track).len(), looped, "opening the dialog does not unloop yet");
    h.get_by_role_and_label(Role::Button, "Flatten").click();
    h.run_steps(2);
    assert_eq!(h.state().dialogs.open_name(), None);
    assert_eq!(clips(&h, track), vec![None; looped]);
}

#[test]
fn enter_on_a_focused_flatten_flattens() {
    let (mut h, track, looped) = unloop_dialog();
    h.get_by_role_and_label(Role::Button, "Flatten").focus();
    h.run_steps(2);
    h.event(Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
    assert_eq!(h.state().dialogs.open_name(), None);
    assert_eq!(clips(&h, track), vec![None; looped]);
}

#[test]
fn unloop_dialogs_remove_keeps_only_the_source_clip() {
    let (mut h, track, looped) = unloop_dialog();
    h.get_by_role_and_label(Role::Button, "Remove").click();
    h.run_steps(2);
    assert_eq!(h.state().dialogs.open_name(), None);
    assert_eq!(clips(&h, track).len(), looped - 2);
}
