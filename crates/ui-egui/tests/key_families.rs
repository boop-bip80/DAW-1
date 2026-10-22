//! Shortcuts written as key families ("Cmd+Ctrl+1-5", "Cmd+Alt+0-5", "Cmd+Alt+[ / ]") fire, and Commands Focus
//! has Pro Tools' 1–5 zoom presets and N for Insertion Follows Playback.

use egui::os::OperatingSystem;
use egui::{Key, Modifiers};
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, shortcuts};

const MAC_CMD: Modifiers = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };
const PC_CTRL: Modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
const OSES: [(OperatingSystem, Modifiers); 3] =
    [(OperatingSystem::Mac, MAC_CMD), (OperatingSystem::Windows, PC_CTRL), (OperatingSystem::Nix, PC_CTRL)];

fn press(ctx: &egui::Context, app: &mut SoundApp, key: Key, modifiers: Modifiers) {
    let events = [true, false].map(|pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers }).to_vec();
    ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
}

fn tool_chord(ctx: &egui::Context, app: &mut SoundApp, keys: &[Key]) {
    for &key in keys {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        ctx.run_ui(egui::RawInput { events: vec![event], ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
    }
    let events = keys
        .iter()
        .rev()
        .map(|&key| egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE })
        .collect();
    ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
}

fn key_event(key: Key, pressed: bool) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE }
}

fn tool_events(ctx: &egui::Context, app: &mut SoundApp, events: Vec<egui::Event>) {
    ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
}

/// Send tool-key transitions at an explicit input time so chord grace periods can be tested.
fn tool_events_at(ctx: &egui::Context, app: &mut SoundApp, time: f64, events: Vec<egui::Event>) {
    ctx.run_ui(egui::RawInput { time: Some(time), events, ..Default::default() }, |ui| shortcuts::handle(app, ui.ctx())).textures_delta.clear();
}

fn rig(os: OperatingSystem) -> (egui::Context, SoundApp) {
    let ctx = egui::Context::default();
    ctx.set_os(os);
    (ctx, SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default()))
}

#[test]
fn cmd_alt_brackets_zoom_waveforms_and_cmd_alt_digits_rate_clips() {
    for (os, cmd) in OSES {
        let (ctx, mut app) = rig(os);
        let cmd_alt = Modifiers { alt: true, ..cmd };
        press(&ctx, &mut app, Key::CloseBracket, cmd_alt);
        press(&ctx, &mut app, Key::CloseBracket, cmd_alt);
        press(&ctx, &mut app, Key::OpenBracket, cmd_alt);
        assert_eq!(app.engine.session().edit.zoom.waveform_zoom, 2.0, "{os:?}");
        let kick = app.engine.session().track_by_name("Kick").unwrap().clips()[0].id;
        app.engine.execute("edit.select", &json!({"clips": [kick.0]})).unwrap();
        press(&ctx, &mut app, Key::Num4, cmd_alt);
        assert_eq!(app.engine.session().find_clip(kick).unwrap().1.rating, 4, "{os:?}");
        press(&ctx, &mut app, Key::Num9, cmd_alt);
        assert_eq!(app.engine.session().find_clip(kick).unwrap().1.rating, 4, "{os:?}: 9 is outside 0-5");
    }
}

#[test]
fn cmd_ctrl_digits_recall_zoom_presets_on_macos() {
    let (ctx, mut app) = rig(OperatingSystem::Mac);
    press(&ctx, &mut app, Key::Num2, Modifiers { ctrl: true, ..MAC_CMD });
    assert_eq!(app.engine.session().edit.zoom.samples_per_px, 256.0);
    press(&ctx, &mut app, Key::Num6, Modifiers { ctrl: true, ..MAC_CMD });
    assert_eq!(app.engine.session().edit.zoom.samples_per_px, 256.0, "6 is outside 1-5");
}

#[test]
fn commands_focus_digits_recall_zoom_presets_and_n_toggles_insertion_follows_playback() {
    for (os, _) in OSES {
        let (ctx, mut app) = rig(os);
        app.engine.execute("options.keyboard_focus", &json!({"focus": "commands"})).unwrap();
        let follows = app.engine.session().edit.insertion_follows_playback;
        press(&ctx, &mut app, Key::Num5, Modifiers::NONE);
        press(&ctx, &mut app, Key::N, Modifiers::NONE);
        let edit = &app.engine.session().edit;
        assert_eq!((edit.zoom.samples_per_px, edit.insertion_follows_playback), (16384.0, !follows), "{os:?}");
    }
}

#[test]
fn f6_f7_f8_choose_individual_tools_and_any_held_pair_selects_smart() {
    use soundcraft_model::Tool;

    for (os, _) in OSES {
        for (key, expected) in [(Key::F6, Tool::Trim), (Key::F7, Tool::Selector), (Key::F8, Tool::Grabber)] {
            let (ctx, mut app) = rig(os);
            press(&ctx, &mut app, key, Modifiers::NONE);
            assert_eq!(app.engine.session().edit.tool, expected, "{os:?}");
        }

        for keys in [
            vec![Key::F6, Key::F7],
            vec![Key::F7, Key::F6],
            vec![Key::F6, Key::F8],
            vec![Key::F8, Key::F6],
            vec![Key::F7, Key::F8],
            vec![Key::F8, Key::F7],
            vec![Key::F6, Key::F7, Key::F8],
        ] {
            let (ctx, mut app) = rig(os);
            tool_chord(&ctx, &mut app, &keys);
            assert_eq!(app.engine.session().edit.tool, Tool::Smart, "{os:?}: {keys:?}");
        }
    }
}

#[test]
/// The first held key waits long enough for a chord instead of briefly selecting its own tool.
fn smart_tool_chord_does_not_flash_its_first_member() {
    use soundcraft_model::Tool;

    let (ctx, mut app) = rig(OperatingSystem::Mac);
    app.engine.execute("edit.tool", &json!({"tool": "pencil"})).unwrap();
    tool_events_at(&ctx, &mut app, 1.0, vec![key_event(Key::F6, true)]);
    assert_eq!(app.engine.session().edit.tool, Tool::Pencil);
    tool_events_at(&ctx, &mut app, 1.02, vec![key_event(Key::F7, true)]);
    assert_eq!(app.engine.session().edit.tool, Tool::Smart);

    let (ctx, mut app) = rig(OperatingSystem::Mac);
    app.engine.execute("edit.tool", &json!({"tool": "pencil"})).unwrap();
    tool_events_at(&ctx, &mut app, 1.0, vec![key_event(Key::F6, true)]);
    tool_events_at(&ctx, &mut app, 1.05, Vec::new());
    assert_eq!(app.engine.session().edit.tool, Tool::Trim);
}

#[test]
/// Event ordering still distinguishes an overlapping chord from sequential tool-key presses.
fn smart_tool_chord_preserves_a_same_frame_press_before_release() {
    use soundcraft_model::Tool;

    for (os, _) in OSES {
        let (ctx, mut app) = rig(os);
        tool_events(&ctx, &mut app, vec![key_event(Key::F6, true)]);
        tool_events(&ctx, &mut app, vec![key_event(Key::F7, true), key_event(Key::F6, false)]);
        assert_eq!(app.engine.session().edit.tool, Tool::Smart, "{os:?}");

        let (ctx, mut app) = rig(os);
        tool_events(&ctx, &mut app, vec![key_event(Key::F6, true)]);
        tool_events(&ctx, &mut app, vec![key_event(Key::F6, false), key_event(Key::F7, true)]);
        assert_eq!(app.engine.session().edit.tool, Tool::Selector, "{os:?}: release before press is not a chord");
    }
}
