//! Window › Mix in Separate Window: Edit stays in the main window, and closing the Mix window
//! brings Mix back into it. It is an item of the Window menu, and keys typed into either window
//! act there.
use egui::{Key, Modifiers, vec2};
use egui_kittest::Harness;
use serde_json::json;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp, mix_window};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

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

/// A main window and the native Mix window, as a multi-window backend drives them: each window's
/// pass gets only its own input (`set_immediate_viewport_renderer` is how eframe does it).
struct TwoWindows {
    ctx: egui::Context,
    /// Input for the Mix window's next pass, and what it reported.
    mix_events: Rc<RefCell<Vec<egui::Event>>>,
    mix_info: Rc<RefCell<egui::ViewportInfo>>,
    mix_focused: Rc<Cell<bool>>,
    mix_commands: Rc<RefCell<Vec<egui::ViewportCommand>>>,
    mix_builder: Rc<RefCell<Option<egui::ViewportBuilder>>>,
    /// A widget in the Mix window holds the keyboard focus (a text field being typed into).
    mix_field_focused: Rc<Cell<bool>>,
}

impl TwoWindows {
    fn new() -> Self {
        let ctx = egui::Context::default();
        ctx.set_os(egui::os::OperatingSystem::Mac);
        ctx.set_embed_viewports(false);
        let w = TwoWindows {
            ctx,
            mix_events: Rc::default(),
            mix_info: Rc::default(),
            mix_focused: Rc::new(Cell::new(true)),
            mix_commands: Rc::default(),
            mix_builder: Rc::default(),
            mix_field_focused: Rc::default(),
        };
        let (events, info, focused, builder) = (w.mix_events.clone(), w.mix_info.clone(), w.mix_focused.clone(), w.mix_builder.clone());
        let field = w.mix_field_focused.clone();
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let id = viewport.ids.this;
            *builder.borrow_mut() = Some(viewport.builder.clone());
            let input = egui::RawInput {
                viewport_id: id,
                viewports: std::iter::once((id, info.borrow().clone())).collect(),
                events: events.take(),
                focused: focused.get(),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                if field.get() {
                    ui.memory_mut(|m| m.request_focus(egui::Id::new("mix text field")));
                }
                (viewport.viewport_ui_cb)(ui)
            })
            .textures_delta
            .clear();
        });
        w
    }

    /// One frame of the main window, with `events` typed into it; returns the main window's commands.
    fn frame(&self, app: &mut SoundApp, events: Vec<egui::Event>) -> Vec<egui::ViewportCommand> {
        let root =
            egui::ViewportInfo { outer_rect: Some(egui::Rect::from_min_size(egui::pos2(100.0, 80.0), vec2(1600.0, 1000.0))), ..Default::default() };
        let input = egui::RawInput {
            events,
            viewports: std::iter::once((egui::ViewportId::ROOT, root)).collect(),
            focused: !self.mix_focused.get(),
            ..Default::default()
        };
        let mut out = self.ctx.run_ui(input, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        // The main window's pass carries the commands of the windows drawn inside it.
        if let Some(mix) = out.viewport_output.remove(&mix_window::viewport_id()) {
            self.mix_commands.borrow_mut().extend(mix.commands);
        }
        out.viewport_output.remove(&egui::ViewportId::ROOT).map(|v| v.commands).unwrap_or_default()
    }

    fn type_in_mix(&self, app: &mut SoundApp, key: Key, modifiers: Modifiers) -> Vec<egui::ViewportCommand> {
        self.mix_focused.set(true);
        *self.mix_events.borrow_mut() = key_press(key, modifiers);
        self.frame(app, Vec::new())
    }

    fn type_in_main(&self, app: &mut SoundApp, key: Key, modifiers: Modifiers) -> Vec<egui::ViewportCommand> {
        self.mix_focused.set(false);
        self.frame(app, key_press(key, modifiers))
    }
}

fn key_press(key: Key, modifiers: Modifiers) -> Vec<egui::Event> {
    [true, false].map(|pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers }).to_vec()
}

fn separate_app() -> SoundApp {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.run("window.mix_separate", json!({"value": true})).unwrap();
    app
}

#[test]
fn shortcuts_work_in_the_window_that_has_the_keys_and_run_once() {
    let w = TwoWindows::new();
    let mut app = separate_app();
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    let cmd = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };
    // Cmd+1 (Transport) typed into the Mix window toggles it exactly once; so does the main window.
    assert!(!app.ui.show_transport);
    w.type_in_mix(&mut app, Key::Num1, cmd);
    assert!(app.ui.show_transport, "a shortcut typed into the separate Mix window runs");
    w.type_in_main(&mut app, Key::Num1, cmd);
    assert!(!app.ui.show_transport, "and one typed into the main window runs once, not twice");

    // Nudge (bare =) is an Edit window key: not from the Mix window, but from the main window,
    // which shows Edit even after Window › Mix brought the Mix window forward.
    let kick = app.engine.session().track_by_name("Kick").unwrap().clips()[0].id;
    app.engine.execute("edit.select", &json!({"clips": [kick.0]})).unwrap();
    let start = |app: &SoundApp| app.engine.session().find_clip(kick).unwrap().1.start;
    let original = start(&app);
    app.run("window.mix", json!({})).unwrap();
    w.type_in_mix(&mut app, Key::Equals, Modifiers::NONE);
    assert_eq!(start(&app), original, "Edit-only keys don't act from the Mix window");
    w.type_in_main(&mut app, Key::Equals, Modifiers::NONE);
    assert!(start(&app) > original, "the main window shows Edit, so its nudge key works");

    // Cmd+= goes to the other window: Edit from the Mix window, Mix from the main one.
    let focus = |c: &[egui::ViewportCommand]| c.contains(&egui::ViewportCommand::Focus);
    w.mix_commands.borrow_mut().clear();
    assert!(focus(&w.type_in_mix(&mut app, Key::Equals, cmd)), "Cmd+= in the Mix window brings Edit forward");
    w.type_in_main(&mut app, Key::Equals, cmd);
    w.frame(&mut app, Vec::new());
    assert!(focus(&w.mix_commands.borrow()), "Cmd+= in the main window brings Mix forward");
    assert!(app.ui.mix_separate);
}

#[test]
fn a_text_field_in_the_mix_window_keeps_its_keys() {
    let w = TwoWindows::new();
    let mut app = separate_app();
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    let cmd = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };
    w.mix_field_focused.set(true);
    w.type_in_mix(&mut app, Key::Num1, cmd);
    assert!(!app.ui.show_transport, "typing into a Mix window field runs no shortcut");
    // The field goes away (egui drops the focus of a widget no longer drawn).
    w.mix_field_focused.set(false);
    for _ in 0..2 {
        w.frame(&mut app, Vec::new());
    }
    w.type_in_mix(&mut app, Key::Num1, cmd);
    assert!(app.ui.show_transport);
}

#[test]
fn a_mix_window_drawn_inside_the_main_one_handles_its_keys_once() {
    // Backends without native windows draw it as a window inside the main one, which shares the
    // main window's input: the main window's pass handles the keys, the Mix window's must not.
    let mut app = separate_app();
    let ctx = egui::Context::default();
    ctx.set_os(egui::os::OperatingSystem::Mac);
    let frame = |app: &mut SoundApp, events: Vec<egui::Event>| {
        ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        })
        .textures_delta
        .clear();
    };
    for _ in 0..3 {
        frame(&mut app, Vec::new());
    }
    frame(&mut app, key_press(Key::Num1, Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE }));
    assert!(app.ui.show_transport, "Cmd+1 toggled Transport once, not on and off again");
}
