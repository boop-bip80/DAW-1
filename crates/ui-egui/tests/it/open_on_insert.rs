//! #486: picking a plugin for an insert slot opens it straight away, as DAWs do, from the Mix
//! window's slots and the Edit window's Inserts column alike.

use egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_model::TrackId;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

fn harness(engine: soundcraft_engine::Engine, window: MainWindow) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = window;
    app.run("view.edit_inserts_ae", json!({})).unwrap();
    app.run("track.height", json!({"tracks": ["Vox"], "height": "medium"})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

fn vox() -> soundcraft_engine::Engine {
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Vox"})).unwrap();
    engine
}

fn vox_id(h: &Harness<SoundApp>) -> TrackId {
    h.state().engine.session().tracks[0].id
}

/// Two built-in effects from one category, and the category's menu label.
fn two_effects() -> (String, &'static soundcraft_dsp::PluginInfo, &'static soundcraft_dsp::PluginInfo) {
    let first = soundcraft_dsp::plugins().iter().find(|p| !p.is_instrument).unwrap();
    let other = soundcraft_dsp::plugins().iter().find(|p| p.category == first.category && p.id != first.id).unwrap();
    (format!("{:?} ⏵", first.category), first, other)
}

/// Opens `slot`'s menu (a click on an empty slot, a right-click on a filled one) and picks
/// `plugin` from its category.
fn pick(h: &mut Harness<SoundApp>, slot: &str, filled: bool, category: &str, plugin: &str) {
    let node = h.get_by_label(slot);
    if filled {
        node.click_secondary();
    } else {
        node.click();
    }
    h.run_steps(3);
    h.get_by_label(category).hover();
    h.run_steps(3);
    h.get_by_label(plugin).click();
    h.run_steps(3);
}

fn inserted(h: &Harness<SoundApp>) -> Option<String> {
    h.state().engine.session().tracks[0].mixer.inserts[0].as_ref().map(|i| i.plugin.clone())
}

#[test]
fn picking_a_plugin_for_an_empty_insert_opens_it() {
    let (category, plugin, _) = two_effects();
    let mut h = harness(vox(), MainWindow::Mix);
    pick(&mut h, "Insert A", false, &category, plugin.name);
    assert_eq!(inserted(&h).as_deref(), Some(plugin.id));
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)], "the new insert's window opened");
}

#[test]
fn picking_from_the_search_opens_it_too() {
    let mut h = harness(vox(), MainWindow::Mix);
    h.get_by_label("Insert A").click();
    h.run_steps(3);
    h.event(egui::Event::Text("reverb".into()));
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert!(inserted(&h).is_some(), "Enter inserted a reverb");
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
}

#[test]
fn replacing_a_filled_insert_opens_the_new_plugin() {
    let (category, first, second) = two_effects();
    let mut engine = vox();
    let track = engine.session().tracks[0].id;
    engine.execute("mix.insert", &json!({"track": track.0, "slot": 0, "plugin": first.id})).unwrap();
    let mut h = harness(engine, MainWindow::Mix);
    assert!(h.state().ui.plugin_windows.is_empty(), "nothing opens before a pick");
    pick(&mut h, "Insert A", true, &category, second.name);
    assert_eq!(inserted(&h).as_deref(), Some(second.id));
    assert_eq!(h.state().ui.plugin_windows, vec![(track, 0)]);
}

#[test]
fn the_edit_windows_inserts_column_opens_it_too() {
    let (category, plugin, _) = two_effects();
    let mut h = harness(vox(), MainWindow::Edit);
    pick(&mut h, "Insert A", false, &category, plugin.name);
    assert_eq!(inserted(&h).as_deref(), Some(plugin.id));
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
}

/// A filled slot of `plugin` on Vox's insert A, set straight into the session.
fn vox_with(plugin: &str) -> soundcraft_engine::Engine {
    let mut engine = vox();
    engine.session_mut().tracks[0].mixer.inserts[0] = Some(soundcraft_model::Insert::new(plugin));
    engine
}

#[test]
fn clicking_a_built_in_insert_opens_its_parameter_window() {
    let (_, plugin, _) = two_effects();
    let mut h = harness(vox_with(plugin.id), MainWindow::Mix);
    h.get_by_label("Insert A").click();
    h.run_steps(3);
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
}

/// The synthetic CLAP effect, installed so its parameters are known.
fn hosted() -> &'static str {
    crate::common::fixture_dir();
    "clap:org.soundcraft.test.gain"
}

/// With no audio engine nothing is hosted, so a click on a hosted insert can't open the plugin's
/// own editor: it opens the parameter window instead, through the slot itself.
#[test]
fn clicking_a_hosted_insert_without_an_audio_engine_opens_its_parameter_window() {
    let mut h = harness(vox_with(hosted()), MainWindow::Mix);
    h.get_by_label("Insert A").click();
    h.run_steps(3);
    let id = vox_id(&h);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
}

/// An insert's right-click menu, on its own.
fn menu(engine: soundcraft_engine::Engine) -> (Harness<'static, SoundApp>, TrackId) {
    let id = engine.session().tracks[0].id;
    let app = SoundApp::new(engine, None, Services::default());
    let mut h = Harness::builder()
        .with_size(vec2(400.0, 600.0))
        .build_ui_state(move |ui, app: &mut SoundApp| soundcraft_ui_egui::mix_window::plugin_menu(app, ui, id, 0, true), app);
    h.step();
    (h, id)
}

#[test]
fn a_hosted_inserts_menu_edits_its_parameters_when_it_is_installed() {
    let (mut h, id) = menu(vox_with(hosted()));
    h.get_by_label("Edit Parameters…").click();
    h.step();
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
}

#[test]
fn a_missing_hosted_plugin_offers_no_parameter_window() {
    let (h, _) = menu(vox_with("clap:org.soundcraft.test.not.installed"));
    assert!(h.query_by_label("Edit Parameters…").is_none(), "it would open nothing");
    assert!(h.query_by_label("Bypass").is_some());
}

/// A hosted instrument's parameter window has nothing to edit (its sounds live in its own
/// editor), so its menu doesn't offer one; a built-in's does (`sampler_window.rs`).
#[test]
fn a_hosted_instruments_menu_offers_no_parameter_window() {
    let synth = {
        crate::common::fixture_dir();
        "clap:org.soundcraft.test.synth"
    };
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Keys", "kind": "instrument"})).unwrap();
    let keys = engine.session().tracks[0].id;
    engine.session_mut().tracks[0].instrument = Some(soundcraft_model::Insert::new(synth));
    let app = SoundApp::new(engine, None, Services::default());
    let mut h = Harness::builder()
        .with_size(vec2(400.0, 600.0))
        .build_ui_state(move |ui, app: &mut SoundApp| soundcraft_ui_egui::mix_window::instrument_menu(app, ui, keys), app);
    h.step();
    assert!(h.query_by_label("Fixture Synth").is_some(), "the instrument's name heads the menu");
    assert!(h.query_by_label("Edit Parameters…").is_none());
}

/// `window.plugin` opens a slot as a click does, or the view it names, and closes it again.
#[test]
fn window_plugin_opens_and_closes_a_slots_windows() {
    let (_, plugin, _) = two_effects();
    let mut engine = vox_with(plugin.id);
    engine.session_mut().tracks[0].mixer.inserts[1] = Some(soundcraft_model::Insert::new(hosted()));
    let id = engine.session().tracks[0].id;
    let mut app = SoundApp::new(engine, None, Services::default());
    let r = app.run("window.plugin", json!({"track": "Vox", "slot": "a"})).unwrap();
    assert_eq!(r, json!({"track": id.0, "slot": 0, "view": "parameters", "value": true}), "a built-in opens its parameter window");
    assert_eq!(app.ui.plugin_windows, vec![(id, 0)]);
    let e = app.run("window.plugin", json!({"track": id.0, "slot": 1, "view": "editor"})).unwrap_err();
    assert!(e.contains("no audio engine"), "{e}");
    assert!(app.ui.status.starts_with(&format!("{}: ", soundcraft_ui_egui::mix_window::plugin_info(hosted()).unwrap().name)), "{}", app.ui.status);
    assert_eq!(app.run("window.plugin", json!({"track": id.0, "slot": 1, "value": true})).unwrap()["view"], "parameters");
    assert_eq!(app.ui.plugin_windows, vec![(id, 0), (id, 1)]);
    let r = app.run("window.plugin", json!({"track": id.0, "slot": 0, "value": false})).unwrap();
    assert_eq!(r, json!({"track": id.0, "slot": 0, "view": null, "value": false}));
    assert_eq!(app.ui.plugin_windows, vec![(id, 1)]);
    let r = app.run("window.plugin", json!({"track": id.0, "slot": "instrument", "view": "editor", "value": false})).unwrap();
    assert_eq!(r, json!({"track": id.0, "slot": "instrument", "view": "editor", "value": false}), "closing an empty slot is a no-op");
    for bad in
        [json!({"value": "false"}), json!({"value": 0}), json!({"view": "gui"}), json!({"view": 1}), json!({"slot": 10}), json!({"track": null})]
    {
        let mut p = json!({"track": id.0, "slot": 0});
        for (k, v) in bad.as_object().unwrap() {
            p[k] = v.clone();
        }
        assert!(app.run("window.plugin", p.clone()).is_err(), "{p} is refused");
    }
    assert_eq!(app.ui.plugin_windows, vec![(id, 1)], "refused calls open nothing");
    assert!(app.run("window.plugin", json!({"track": id.0, "slot": 2})).unwrap_err().contains("empty"));
    app.run("window.close", json!({})).unwrap();
    assert!(app.ui.plugin_windows.is_empty());
}

/// The top colour of the raised key painted under `label`: an open slot's is lit, a closed
/// one's is grey. Read from the shapes the real Mix and Edit drawing emits.
fn face_under(h: &Harness<SoundApp>, label: &str) -> egui::Color32 {
    use egui::Shape;
    let shapes = &h.output().shapes;
    // The label may appear elsewhere too (a parameter called the same): take the one on a key.
    let key_under = |center: egui::Pos2| {
        shapes
            .iter()
            .filter_map(|s| match &s.shape {
                Shape::Mesh(m) if m.calc_bounds().contains(center) && m.calc_bounds().height() <= 18.0 => Some(m),
                _ => None,
            })
            .min_by(|a, b| a.calc_bounds().area().total_cmp(&b.calc_bounds().area()))
    };
    let face = shapes
        .iter()
        .filter_map(|s| match &s.shape {
            Shape::Text(t) if t.galley.job.text == label => key_under(t.visual_bounding_rect().center()),
            _ => None,
        })
        .next()
        .unwrap_or_else(|| panic!("no slot named {label}"));
    let top = face.vertices.iter().filter(|v| v.color.a() == 255).min_by(|a, b| a.pos.y.total_cmp(&b.pos.y)).unwrap();
    top.color
}

fn near(a: egui::Color32, b: egui::Color32) -> bool {
    a.to_array().iter().zip(b.to_array()).all(|(p, q)| p.abs_diff(q) <= 2)
}

/// As in Pro Tools, an insert or instrument slot whose window is open is drawn lit, in the Mix
/// window and the Edit window's Inserts column; the others stay grey.
#[test]
fn a_slot_with_its_window_open_is_drawn_lit() {
    let t = soundcraft_ui_egui::theme::Tokens::DARK;
    let (_, a, b) = two_effects();
    let mut engine = vox();
    engine.execute("mix.insert", &json!({"track": "Vox", "slot": 0, "plugin": a.id})).unwrap();
    engine.execute("mix.insert", &json!({"track": "Vox", "slot": 1, "plugin": b.id})).unwrap();
    engine.execute("track.new", &json!({"kind": "instrument", "name": "Keys"})).unwrap();
    let synth = soundcraft_mix_plugin_name(&engine);
    let mut h = harness(engine, MainWindow::Mix);
    h.state_mut().run("view.mix_section", json!({"section": "instruments"})).unwrap();
    h.run_steps(3);
    assert!(near(face_under(&h, a.short_name), t.slot_face[0]) && near(face_under(&h, synth), t.slot_face[0]), "closed slots are grey");
    h.state_mut().run("window.plugin", json!({"track": "Vox", "slot": 0})).unwrap();
    h.state_mut().run("window.plugin", json!({"track": "Keys", "slot": "instrument", "view": "parameters"})).unwrap();
    h.run_steps(3);
    assert!(near(face_under(&h, a.short_name), t.slot_open[0]), "Mix: the open insert is lit");
    assert!(near(face_under(&h, b.short_name), t.slot_face[0]), "Mix: the other insert stays grey");
    assert!(near(face_under(&h, synth), t.slot_open[0]), "Mix: the open instrument is lit");
    h.state_mut().run("window.edit", json!({})).unwrap();
    h.run_steps(3);
    assert!(near(face_under(&h, a.short_name), t.slot_open[0]), "Edit: the open insert is lit");
    assert!(near(face_under(&h, b.short_name), t.slot_face[0]), "Edit: the other insert stays grey");
    h.state_mut().run("window.plugin", json!({"track": "Vox", "slot": 0, "value": false})).unwrap();
    h.run_steps(3);
    assert!(near(face_under(&h, a.short_name), t.slot_face[0]), "closing it greys it again");
}

/// The short name the Keys track's instrument slot shows.
fn soundcraft_mix_plugin_name(engine: &soundcraft_engine::Engine) -> &'static str {
    let keys = engine.session().track_by_name("Keys").unwrap();
    soundcraft_ui_egui::mix_window::plugin_info(&keys.instrument.as_ref().unwrap().plugin).unwrap().short_name
}
