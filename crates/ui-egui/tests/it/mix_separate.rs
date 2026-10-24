//! Window › Mix in Separate Window: Edit stays in the main window, and closing the Mix window
//! brings Mix back into it. It is an item of the Window menu.
use egui::vec2;
use egui_kittest::Harness;
use serde_json::json;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp, mix_window};

#[test]
fn mix_in_a_separate_window_toggles_and_closing_it_returns_to_the_main_window() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    assert!(!app.ui.mix_separate);
    app.run("window.mix_separate", json!({})).unwrap();
    assert!(app.ui.mix_separate);
    app.run("window.toggle_mix_edit", json!({})).unwrap();
    assert_eq!(app.ui.window, MainWindow::Edit, "Mix is already on screen: the main window keeps Edit");

    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    let app = h.state_mut();
    assert!(app.ui.mix_separate, "drawing both windows keeps them both");

    mix_window::separate_closed(app);
    assert!(!app.ui.mix_separate);
    assert_eq!(app.ui.window, MainWindow::Mix, "closing the Mix window shows Mix in the main one");
    h.run_steps(3);

    let app = h.state_mut();
    app.run("window.mix_separate", json!({"value": true})).unwrap();
    app.run("window.mix_separate", json!({"value": false})).unwrap();
    assert!(!app.ui.mix_separate);
}

#[test]
fn mix_in_separate_window_is_a_checked_item_of_the_window_menu() {
    use soundcraft_ui_egui::menus;
    let path = "Window > Mix in Separate Window";
    let tree = menus::tree();
    let window = tree.iter().find(|n| n.label == "Window").unwrap();
    let at = window.children.iter().position(|n| n.path == path).expect("in the Window menu (native and in-window)");
    assert_eq!(window.children.get(at.wrapping_sub(1)).map(|n| n.path.as_str()), Some("Window > Edit"));
    assert_eq!(menus::command_for_path(path, &menus::ui_aliases()).as_deref(), Some("window.mix_separate"));
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    assert_eq!(menus::item_state(&app, path, Some("window.mix_separate")), (true, false));
    menus::invoke_menu(&mut app, "window.mix_separate", path);
    assert!(app.ui.mix_separate);
    assert_eq!(menus::item_state(&app, path, Some("window.mix_separate")), (true, true));
    menus::invoke_menu(&mut app, "window.mix_separate", path);
    assert!(!app.ui.mix_separate);
}
