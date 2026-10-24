//! Window › Mix in Separate Window: Edit stays in the main window, and closing the Mix window
//! brings Mix back into it. Keys typed into either window act there, and the Mix window reopens
//! where it was left.
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
    /// The labelled widgets and windows each window drew last (with `ctx.enable_accesskit()`).
    mix_nodes: Rc<RefCell<Vec<(String, egui::Rect)>>>,
    main_nodes: RefCell<Vec<(String, egui::Rect)>>,
}

/// The labels (and values) of the accessibility nodes a pass output, with their bounds.
fn labelled(update: Option<egui::accesskit::TreeUpdate>) -> Vec<(String, egui::Rect)> {
    let nodes = update.map(|u| u.nodes).unwrap_or_default();
    nodes
        .iter()
        .filter_map(|(_, n)| {
            let text = n.label().or(n.value())?.to_string();
            let b = n.bounds()?;
            Some((text, egui::Rect::from_min_max(egui::pos2(b.x0 as f32, b.y0 as f32), egui::pos2(b.x1 as f32, b.y1 as f32))))
        })
        .collect()
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
            mix_nodes: Rc::default(),
            main_nodes: RefCell::default(),
        };
        let (events, info, focused, builder) = (w.mix_events.clone(), w.mix_info.clone(), w.mix_focused.clone(), w.mix_builder.clone());
        let (field, nodes) = (w.mix_field_focused.clone(), w.mix_nodes.clone());
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
            let mut out = ctx.run_ui(input, |ui| {
                if field.get() {
                    ui.memory_mut(|m| m.request_focus(egui::Id::new("mix text field")));
                }
                (viewport.viewport_ui_cb)(ui)
            });
            out.textures_delta.clear();
            *nodes.borrow_mut() = labelled(out.platform_output.accesskit_update);
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
        *self.main_nodes.borrow_mut() = labelled(out.platform_output.accesskit_update.take());
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

    /// A click at `at` in the Mix window (`in_mix`) or the main window, over two frames.
    fn click(&self, app: &mut SoundApp, in_mix: bool, at: egui::Pos2) {
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        self.mix_focused.set(in_mix);
        for events in [vec![egui::Event::PointerMoved(at), button(true)], vec![button(false)]] {
            if in_mix {
                *self.mix_events.borrow_mut() = events;
                self.frame(app, Vec::new());
            } else {
                self.frame(app, events);
            }
        }
        self.frame(app, Vec::new());
    }

    /// Where the leftmost (then topmost) widget or window labelled `label` is in the Mix window or the
    /// main one: the first track's, in the Mix strips or the Edit window's rows.
    fn find(&self, in_mix: bool, label: impl Fn(&str) -> bool) -> Option<egui::Pos2> {
        let nodes = if in_mix { self.mix_nodes.borrow().clone() } else { self.main_nodes.borrow().clone() };
        nodes.iter().filter(|(l, _)| label(l)).map(|(_, r)| r.center()).min_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)))
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

#[test]
fn the_mix_window_reopens_where_it_was_left() {
    reopens_where_it_was_left(1.0);
    reopens_where_it_was_left(1.5);
}

fn reopens_where_it_was_left(zoom: f32) {
    let w = TwoWindows::new();
    w.ctx.set_zoom_factor(zoom);
    let mut app = separate_app();
    assert_eq!(app.ui.mix_window_pos, None);
    *w.mix_info.borrow_mut() = egui::ViewportInfo {
        outer_rect: Some(egui::Rect::from_min_size(egui::pos2(1800.0, 120.0), vec2(900.0, 640.0))),
        inner_rect: Some(egui::Rect::from_min_size(egui::pos2(1800.0, 148.0), vec2(900.0, 612.0))),
        monitor_size: Some(vec2(1920.0, 1080.0)),
        ..Default::default()
    };
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    // Saved in logical pixels: points × zoom.
    assert_eq!(app.ui.mix_window_pos, Some([1800.0 * zoom, 120.0 * zoom]));
    assert_eq!(app.ui.mix_window_size, [900.0 * zoom, 612.0 * zoom]);
    assert!(!w.mix_commands.borrow().iter().any(|c| matches!(c, egui::ViewportCommand::OuterPosition(_))), "an on-screen window stays put");

    // Saved with the preferences, restored at the next launch.
    let ui: soundcraft_ui_egui::UiState = serde_json::from_value(serde_json::to_value(&app.ui).unwrap()).unwrap();
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.ui = ui;
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    let builder = w.mix_builder.borrow().clone().unwrap();
    assert_eq!(builder.position, Some(egui::pos2(1800.0, 120.0)));
    assert_eq!(builder.inner_size, Some(vec2(900.0, 612.0)));
}

#[test]
fn a_mix_window_on_a_display_that_is_gone_moves_next_to_the_main_window() {
    let w = TwoWindows::new();
    let mut app = separate_app();
    app.ui.mix_window_pos = Some([5000.0, 300.0]);
    // The system puts the window where it was, but on no monitor.
    *w.mix_info.borrow_mut() = egui::ViewportInfo {
        outer_rect: Some(egui::Rect::from_min_size(egui::pos2(5000.0, 300.0), vec2(900.0, 640.0))),
        inner_rect: Some(egui::Rect::from_min_size(egui::pos2(5000.0, 328.0), vec2(900.0, 612.0))),
        monitor_size: None,
        ..Default::default()
    };
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    let moves: Vec<_> = w.mix_commands.borrow().iter().filter(|c| matches!(c, egui::ViewportCommand::OuterPosition(_))).cloned().collect();
    assert_eq!(moves, [egui::ViewportCommand::OuterPosition(egui::pos2(148.0, 128.0))]);
}

#[test]
fn nonsense_mix_window_placements_fall_back_to_the_defaults() {
    assert_eq!(mix_window::sane_placement(Some([10.0, 20.0]), [900.0, 600.0]), (Some([10.0, 20.0]), [900.0, 600.0]));
    assert_eq!(mix_window::sane_placement(Some([f32::NAN, 0.0]), [f32::INFINITY, 600.0]), (None, mix_window::DEFAULT_SIZE));
    assert_eq!(mix_window::sane_placement(Some([1e9, 0.0]), [10.0, -5.0]), (None, mix_window::DEFAULT_SIZE));
    assert_eq!(mix_window::sane_placement(None, [1e9, 1e9]), (None, [16_384.0, 16_384.0]));
    // Preferences from before the Mix window was remembered.
    let ui: soundcraft_ui_egui::UiState = serde_json::from_value(json!({"mix_separate": true})).unwrap();
    assert_eq!((ui.mix_window_pos, ui.mix_window_size), (None, mix_window::DEFAULT_SIZE));
}

#[test]
fn saved_numbers_too_big_for_the_preferences_get_their_defaults() {
    // JSON reads 1e39 into an f32 as infinity, which would be written back as `null` and make
    // the whole preferences file unreadable.
    let saved =
        json!({"mix_window_pos": [1e39, 0], "mix_window_size": [1e39, 600], "track_tint": 1e39, "window_size": [1e39, 1e39], "ui_scale": -1e39});
    let ui: soundcraft_ui_egui::UiState = serde_json::from_value(saved.clone()).unwrap();
    assert_eq!((ui.mix_window_pos, ui.mix_window_size), (None, mix_window::DEFAULT_SIZE));
    assert_eq!((ui.track_tint, ui.window_size, ui.ui_scale), (0.5, [1600.0, 1000.0], 1.0));
    let again: soundcraft_ui_egui::UiState = serde_json::from_value(serde_json::to_value(&ui).unwrap()).unwrap();
    assert_eq!(again.mix_window_size, mix_window::DEFAULT_SIZE, "the preferences round-trip");

    // The control channel's ui.set goes through the same reading.
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.run("ui.set", saved).unwrap();
    assert_eq!((app.ui.mix_window_pos, app.ui.mix_window_size, app.ui.track_tint), (None, mix_window::DEFAULT_SIZE, 0.5));
    app.run("ui.set", json!({"track_tint": 2.0, "mix_window_pos": [-32000, -32000]})).unwrap();
    assert_eq!((app.ui.track_tint, app.ui.mix_window_pos), (1.0, None), "out of range: clamped, or the minimized-window sentinel dropped");
    app.run("ui.set", json!({"show_transport": true})).expect("later ui.set calls still round-trip");
}

#[test]
fn plugin_and_send_windows_open_in_the_window_they_were_asked_from() {
    let w = TwoWindows::new();
    w.ctx.enable_accesskit();
    let mut app = separate_app();
    app.run("view.edit_inserts_ae", json!({})).unwrap();
    let first = app.engine.session().tracks.first().map(|t| (t.id, t.name.clone())).unwrap();
    app.run("mix.insert", json!({"track": first.0.0, "slot": 0, "plugin": "eq_7band"})).unwrap();
    for _ in 0..3 {
        w.frame(&mut app, Vec::new());
    }
    let title = format!("{} · a · ", first.1);
    let window = |l: &str| l.starts_with(&title);

    // Clicking an insert in the Mix window opens its plugin window there, not in the main window.
    let insert = w.find(true, |l| l == "Insert A").expect("the Mix window shows inserts");
    w.click(&mut app, true, insert);
    assert_eq!(app.ui.plugin_windows, [(first.0, 0)]);
    assert!(w.find(true, window).is_some(), "the plugin window opened in the Mix window");
    assert!(w.find(false, window).is_none(), "and not in the main window");

    // With the Mix window back inside the main one, so is the plugin window; turned on again,
    // the plugin window goes back with it.
    app.run("window.mix_separate", json!({"value": false})).unwrap();
    w.frame(&mut app, Vec::new());
    assert!(w.find(false, window).is_some(), "Mix in the main window: the plugin window is there");
    app.run("window.mix_separate", json!({"value": true})).unwrap();
    // A window reopened in a new native window takes a few frames to find its size.
    for _ in 0..4 {
        w.frame(&mut app, Vec::new());
    }
    assert!(w.find(true, window).is_some() && w.find(false, window).is_none(), "back in the separate Mix window");

    // Its close button works there.
    let close = w.find(true, |l| l == "Close window").expect("a close button");
    w.click(&mut app, true, close);
    assert!(app.ui.plugin_windows.is_empty() && app.ui.mix_plugin_windows.is_empty(), "closed from the Mix window");

    // The same insert clicked in the Edit window opens it in the main window; clicked in the Mix
    // window again, the open window moves there.
    let insert_in_edit = w.find(false, |l| l == "Insert A").expect("the Edit window shows inserts");
    w.click(&mut app, false, insert_in_edit);
    assert!(w.find(false, window).is_some() && w.find(true, window).is_none(), "opened from Edit: in the main window");
    w.click(&mut app, true, insert);
    assert!(w.find(true, window).is_some() && w.find(false, window).is_none(), "asked for from Mix: moved there");
    let close = w.find(true, |l| l == "Close window").unwrap();
    w.click(&mut app, true, close);

    // A send's level opens the Send window in the window it was clicked in, too.
    let send = w.find(true, |l| l == "Send A level").expect("a send");
    w.click(&mut app, true, send);
    let send_window = |l: &str| l.contains(" · Send A");
    assert!(app.ui.send_window.is_some());
    assert!(w.find(true, send_window).is_some() && w.find(false, send_window).is_none(), "the Send window opened in the Mix window");
    w.click(&mut app, true, send);
    assert!(app.ui.send_window.is_none(), "clicking the send again closes it");
}

#[test]
fn plugin_windows_of_a_mix_window_inside_the_main_one_are_drawn_there() {
    // Backends without native windows: the Mix window and its plugin windows are in the main one.
    let mut app = separate_app();
    let kick = app.engine.session().track_by_name("Kick").unwrap().id;
    app.ui.plugin_windows = vec![(kick, 0)];
    app.ui.mix_plugin_windows = vec![(kick, 0)];
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut nodes = Vec::new();
    for _ in 0..3 {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        nodes = labelled(out.platform_output.accesskit_update);
    }
    assert!(nodes.iter().any(|(l, _)| l.starts_with("Kick · a · ")), "the plugin window is drawn");
    assert_eq!(app.ui.plugin_windows, [(kick, 0)], "and stays open");
}
