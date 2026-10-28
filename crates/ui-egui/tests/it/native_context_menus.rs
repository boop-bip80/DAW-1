//! Native presentation owns context menus and left-click dropdowns without an egui popup.
#![cfg(not(target_arch = "wasm32"))]

use egui::{Event, Modifiers, PointerButton, Pos2, pos2, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_ui_egui::{
    MainWindow, Services, SoundApp,
    context_menu::{Presenter, Request, Selection, Target},
    native_host::NativeUiHost,
};
use std::{cell::RefCell, rc::Rc};

struct Recorder(Rc<RefCell<Vec<Request>>>);
impl Presenter for Recorder {
    fn present(&self, request: &Request) -> Result<Option<Selection>, String> {
        self.0.borrow_mut().push(request.clone());
        Ok(None)
    }
}

fn harness() -> (Harness<'static, NativeUiHost>, Rc<RefCell<Vec<Request>>>) {
    let mut engine = soundcraft_engine::Engine::default();
    engine.execute("track.new", &json!({"name": "Voice"})).unwrap();
    engine.execute("mix.new_bus", &json!({"name": "Verb"})).unwrap();
    engine.execute("mix.send", &json!({"track": "Voice", "slot": 0, "bus": "Verb"})).unwrap();
    let host = NativeUiHost::new(SoundApp::new(engine, None, Services::default())).unwrap();
    let requests = Rc::new(RefCell::new(vec![]));
    host.set_context_menu_presenter(Rc::new(Recorder(requests.clone()))).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        |ui, host: &mut NativeUiHost| {
            host.state.borrow_mut().logic(ui.ctx());
            host.ui(ui);
        },
        host,
    );
    h.run_steps(5);
    (h, requests)
}

fn right(h: &mut Harness<NativeUiHost>, at: Pos2) {
    h.hover_at(at);
    for pressed in [true, false] {
        h.event(Event::PointerButton { pos: at, button: PointerButton::Secondary, pressed, modifiers: Modifiers::NONE });
    }
    h.run_steps(3);
}

#[test]
fn edit_and_mix_track_names_open_one_native_menu_without_an_egui_popup() {
    let (mut h, requests) = harness();
    let row = h.state().state.borrow().edit_layout.rows[0].1;
    right(&mut h, pos2(row[0] + 60.0, row[1] + 10.0));
    assert_eq!(requests.borrow().len(), 1);
    assert!(matches!(requests.borrow()[0].target, Target::Track(_)));
    assert!(h.query_by_label("Make Inactive").is_none());
    h.state().state.borrow_mut().ui.window = MainWindow::Mix;
    h.run_steps(4);
    let strip = h.get_by_label("Record enable for Voice").rect();
    right(&mut h, pos2(strip.center().x, 900.0 - 17.0));
    assert_eq!(requests.borrow().len(), 2);
    assert!(matches!(requests.borrow()[1].target, Target::Track(_)));
    assert!(h.query_by_label("Make Inactive").is_none());
}

#[test]
fn inserts_and_sends_use_native_context_menus_and_left_click_dropdowns() {
    let (mut h, requests) = harness();
    h.state().state.borrow_mut().ui.window = MainWindow::Mix;
    h.run_steps(4);
    let insert = h.get_by_label("Insert A for Voice").rect().center();
    right(&mut h, insert);
    assert!(matches!(requests.borrow().last().unwrap().target, Target::Insert { slot: 0, .. }));
    assert!(h.query_by_label("Search plugins…").is_none());
    let send = h.get_by_label("Send A level for Voice").rect().center();
    right(&mut h, send);
    assert!(matches!(requests.borrow().last().unwrap().target, Target::Send { slot: 0, .. }));
    assert!(h.query_by_label("Pre-fader").is_none());
    let n = requests.borrow().len();
    h.get_by_label("Insert A for Voice").click();
    h.run_steps(3);
    assert_eq!(requests.borrow().len(), n + 1);
    assert!(matches!(requests.borrow().last().unwrap().target, Target::Insert { slot: 0, .. }));
    assert!(matches!(requests.borrow().last().unwrap().anchor, soundcraft_ui_egui::context_menu::Anchor::Below { .. }));
    assert!(h.query_by_label("Search plugins…").is_none());
}

#[test]
fn track_view_dropdown_requests_a_native_menu() {
    let (mut h, requests) = harness();
    let row = h.state().state.borrow().edit_layout.rows[0].1;
    let point = pos2(row[0] + 70.0, row[1] + 47.0);
    h.hover_at(point);
    for pressed in [true, false] {
        h.event(Event::PointerButton { pos: point, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
    }
    h.run_steps(3);
    let requests = requests.borrow();
    assert!(matches!(requests.last().unwrap().target, Target::TrackView(_)));
    assert!(matches!(requests.last().unwrap().anchor, soundcraft_ui_egui::context_menu::Anchor::Below { .. }));
    assert!(h.query_by_label("blocks").is_none(), "the egui menu must not open behind the native menu");
}

fn grouped_locations(h: &mut Harness<NativeUiHost>) {
    {
        let host = h.state();
        let mut app = host.state.borrow_mut();
        app.ui.show_midi_editor = true;
        app.ui.show_memory_locations = true;
        app.ui.show_undo_history = true;
    }
    h.run_steps(4);
    for panel in ["memory", "history"] {
        h.state().state.borrow_mut().run("ui.dock", json!({"operation":"move", "panel":panel, "target":"midi", "zone":"tab"})).unwrap();
    }
    h.run_steps(4);
}

fn action(entries: &[soundcraft_ui_egui::context_menu::Entry], name: &str) -> soundcraft_ui_egui::context_menu::Action {
    entries
        .iter()
        .find_map(|entry| match entry {
            soundcraft_ui_egui::context_menu::Entry::Item { label, action, .. } if label == name => Some(action.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing menu item {name}"))
}

#[test]
fn docking_tabs_use_one_native_menu_and_keep_the_originating_workspace() {
    use soundcraft_ui_egui::{
        context_menu,
        docking::{Panel, native_menus::MenuKind},
    };
    let (mut h, requests) = harness();
    grouped_locations(&mut h);
    let at = h.get_by_role_and_label(egui::accesskit::Role::Tab, "Memory Locations").rect().center();
    right(&mut h, at);
    assert_eq!(requests.borrow().len(), 1, "discarded egui passes must not duplicate a native request");
    let request = requests.borrow().last().unwrap().clone();
    assert!(matches!(request.target, Target::Dock { window: MainWindow::Edit, panel: Panel::Memory, kind: MenuKind::Panel }));
    assert!(h.query_by_label("Float Panel").is_none(), "no egui menu remains under AppKit");
    assert!(!egui::Popup::is_any_open(&h.ctx));
    assert!(request.entries.iter().any(|entry| matches!(entry, context_menu::Entry::Submenu { label, .. } if label == "Move Group To")));
    let selected = action(&request.entries, "Float Panel");
    let host = h.state();
    let mut app = host.state.borrow_mut();
    let session = serde_json::to_value(app.engine.session()).unwrap();
    let mix = app.ui.mix_docking.clone();
    app.ui.window = MainWindow::Mix;
    context_menu::execute(&mut app, selected, Modifiers::NONE, request.viewport);
    assert!(app.ui.docking.layout.floating.iter().any(|floating| floating.contains(&Panel::Memory)));
    assert_eq!(app.ui.mix_docking, mix);
    assert_eq!(app.ui.window, MainWindow::Mix);
    assert_eq!(serde_json::to_value(app.engine.session()).unwrap(), session);
    let saved = serde_json::to_string(&app.ui).unwrap();
    let reloaded: soundcraft_ui_egui::UiState = serde_json::from_str(&saved).unwrap();
    soundcraft_ui_egui::docking::validate(&reloaded.docking).unwrap();
}

#[test]
fn group_drag_bar_and_column_handle_use_native_menus() {
    use soundcraft_ui_egui::docking::{Panel, area_id, native_menus::MenuKind};
    let (mut h, requests) = harness();
    grouped_locations(&mut h);
    let id = area_id(MainWindow::Edit).with(("drag-bar", Some(&Panel::Midi)));
    let at = h.ctx.read_response(id).unwrap().rect.center();
    right(&mut h, at);
    assert!(matches!(requests.borrow().last().unwrap().target, Target::Dock { kind: MenuKind::Group, .. }));
    assert!(matches!(requests.borrow().last().unwrap().anchor, soundcraft_ui_egui::context_menu::Anchor::Below { .. }));
    assert!(h.query_by_label("Float Tab Group").is_none());
    let id = area_id(MainWindow::Edit).with(("column-bar", Some(&Panel::Tracks)));
    let at = h.ctx.read_response(id).unwrap().rect.center();
    right(&mut h, at);
    assert!(matches!(requests.borrow().last().unwrap().target, Target::Dock { kind: MenuKind::Column, .. }));
    assert!(matches!(requests.borrow().last().unwrap().anchor, soundcraft_ui_egui::context_menu::Anchor::Below { .. }));
    assert!(!egui::Popup::is_any_open(&h.ctx));
}

#[test]
fn docking_overflow_uses_native_choices_and_a_stale_layout_cannot_apply() {
    use soundcraft_ui_egui::{
        context_menu,
        docking::{Panel, native_menus::MenuKind},
    };
    let (mut h, requests) = harness();
    {
        let mut app = h.state().state.borrow_mut();
        app.ui.show_memory_locations = true;
    }
    h.run_steps(3);
    for panel in ["clips", "memory"] {
        h.state().state.borrow_mut().run("ui.dock", json!({"operation":"move", "panel":panel, "target":"tracks", "zone":"tab"})).unwrap();
    }
    h.run_steps(4);
    h.get_by_label("Panels").click();
    h.run_steps(3);
    let request = requests.borrow().last().unwrap().clone();
    assert!(matches!(request.target, Target::Dock { kind: MenuKind::Overflow, .. }));
    assert!(!egui::Popup::is_any_open(&h.ctx));
    let selected = action(&request.entries, "Tracks and Groups");
    let mut app = h.state().state.borrow_mut();
    context_menu::execute(&mut app, selected, Modifiers::NONE, request.viewport);
    assert_eq!(app.ui.docking.layout.panel_state(&Panel::Tracks), craft_ui::docking::PanelState::Front);
    let entries = soundcraft_ui_egui::docking::native_menus::describe(&app, MainWindow::Edit, Panel::Memory, MenuKind::Panel);
    let stale = action(&entries, "Close Tab Group");
    app.run("ui.dock", json!({"operation":"float", "panel":"clips", "rect":[600,100,250,300]})).unwrap();
    let before = app.ui.docking.clone();
    context_menu::execute(&mut app, stale, Modifiers::NONE, request.viewport);
    assert_eq!(app.ui.docking, before, "a stale group identity must not close a different group");
    assert!(app.ui.status.contains("layout changed"));
    let entries = soundcraft_ui_egui::docking::native_menus::describe(&app, MainWindow::Edit, Panel::Canvas, MenuKind::Panel);
    assert!(entries.is_empty(), "the protected session view has no destructive menu actions");
}

#[test]
fn floating_panel_menus_in_mix_cancel_without_changes_and_close_the_right_panel() {
    use soundcraft_ui_egui::{
        context_menu,
        docking::{Panel, native_menus::MenuKind},
    };
    let (mut h, requests) = harness();
    {
        let mut app = h.state().state.borrow_mut();
        app.ui.window = MainWindow::Mix;
        app.ui.show_memory_locations = true;
    }
    h.run_steps(5);
    let before = h.state().state.borrow().ui.mix_docking.clone();
    let at = h.get_by_role_and_label(egui::accesskit::Role::Tab, "Memory Locations").rect().center();
    right(&mut h, at);
    assert_eq!(requests.borrow().len(), 1);
    let request = requests.borrow().last().unwrap().clone();
    assert!(matches!(request.target, Target::Dock { window: MainWindow::Mix, panel: Panel::Memory, kind: MenuKind::Panel }));
    assert_eq!(h.state().state.borrow().ui.mix_docking, before, "canceling AppKit must not apply a layout action");
    assert!(!egui::Popup::is_any_open(&h.ctx));
    assert!(!request.entries.iter().any(|entry| matches!(entry, context_menu::Entry::Item { label, .. } if label == "Float Tab Group")));
    let selected = action(&request.entries, "Close Panel");
    let mut app = h.state().state.borrow_mut();
    context_menu::execute(&mut app, selected, Modifiers::NONE, request.viewport);
    assert!(!app.ui.mix_docking.layout.contains(&Panel::Memory));
    assert!(!app.ui.show_memory_locations);
    assert!(app.ui.mix_docking.layout.contains(&Panel::Canvas));
}
