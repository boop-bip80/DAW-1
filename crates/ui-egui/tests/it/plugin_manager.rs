use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, menus};

/// SoundCraft's own Audio Plug-in Manager sits in the Setup menu beside the audio engine items it
/// manages (Pro Tools has no such item), right after I/O... rather than after Preferences.
#[test]
fn the_setup_menu_lists_the_audio_plugin_manager_after_io_setup() {
    let tree = menus::tree();
    let setup = tree.iter().find(|n| n.label == "Setup").expect("no Setup menu");
    let paths: Vec<&str> = setup.children.iter().map(|n| n.path.as_str()).collect();
    let at = paths.iter().position(|p| *p == "Setup > Audio Plug-in Manager...").expect("the Setup menu has no Audio Plug-in Manager");
    assert_eq!(paths.get(at.wrapping_sub(1)), Some(&"Setup > I/O..."), "{paths:?}");
    let shortcuts = paths.iter().position(|p| *p == "Setup > Keyboard Shortcuts...").expect("no Keyboard Shortcuts");
    assert!(at < shortcuts, "{paths:?}");
    assert_eq!(menus::command_for_path("Setup > Audio Plug-in Manager...", &[]).as_deref(), Some("window.plugin_manager"));
}

/// The menu item opens the window as a command, so scripts and the control channel reach it too.
#[test]
fn the_plugin_manager_command_opens_and_closes_its_window() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("window.plugin_manager", json!({"value": true})).expect("window.plugin_manager");
    assert!(app.ui.show_plugin_manager);
    app.run("window.plugin_manager", json!({"value": false})).expect("window.plugin_manager");
    assert!(!app.ui.show_plugin_manager);
    // No value: a menu click toggles.
    app.run("window.plugin_manager", json!({})).expect("window.plugin_manager");
    assert!(app.ui.show_plugin_manager);
}

/// The window draws its placeholder, inside the screen.
#[test]
fn the_plugin_manager_window_draws() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("window.plugin_manager", json!({"value": true})).expect("window.plugin_manager");
    let id = egui::Id::unique("Audio Plug-in Manager");
    let mut h = Harness::builder().with_size(vec2(1280.0, 900.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    assert!(h.state().ui.show_plugin_manager);
    let rect = h.ctx.memory(|m| m.area_rect(id)).expect("the window was not drawn");
    assert!(rect.right() <= 1280.0 && rect.bottom() <= 900.0, "the window hangs off the screen: {rect:?}");
    h.get_by_label(soundcraft_ui_egui::i18n::t("Plug-in management arrives in a later update."));
}
