//! Keyboard shortcuts. Most come from the command registry's `shortcut` strings, which people can
//! change in Setup › Keyboard Shortcuts; a few context-sensitive keys (Space, Enter, F-keys, nudge,
//! Tab) are handled here.

use crate::{MainWindow, SoundApp};
use egui::{Key, Modifiers};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// People's own shortcuts: command id → shortcut, "" for none.
pub type Custom = BTreeMap<String, String>;

/// Parse "Cmd+Shift+X" style strings. Returns None for descriptive ones ("F1-F4", "Plus/Minus").
pub fn parse(s: &str) -> Option<(Modifiers, Key)> {
    let mut m = Modifiers::NONE;
    let mut key = None;
    for part in s.split('+') {
        match part.trim() {
            "Cmd" => m.command = true,
            "Shift" => m.shift = true,
            "Alt" => m.alt = true,
            "Ctrl" => m.ctrl = true,
            "" => {}
            k => {
                if key.is_some() {
                    return None;
                }
                key = Some(match k {
                    "Space" => Key::Space,
                    "Enter" => Key::Enter,
                    "Home" => Key::Home,
                    "End" => Key::End,
                    "Tab" => Key::Tab,
                    "[" => Key::OpenBracket,
                    "]" => Key::CloseBracket,
                    "," => Key::Comma,
                    "." => Key::Period,
                    "/" => Key::Slash,
                    "'" => Key::Quote,
                    "=" => Key::Equals,
                    other => Key::from_name(other)?,
                });
            }
        }
    }
    key.map(|k| (m, k))
}

/// egui names the shifted symbol ("?" for Shift+/); shortcuts name the key under it.
fn unshifted(key: Key) -> Key {
    match key {
        Key::Questionmark => Key::Slash,
        Key::Colon => Key::Semicolon,
        Key::Pipe => Key::Backslash,
        Key::OpenCurlyBracket => Key::OpenBracket,
        Key::CloseCurlyBracket => Key::CloseBracket,
        Key::Exclamationmark => Key::Num1,
        k => k,
    }
}

/// The shortcut string of a key press ("Cmd+Shift+K"). Off macOS the Ctrl key is the shortcut's Cmd.
pub fn chord_text(key: Key, m: Modifiers, mac: bool) -> String {
    let name = match key {
        Key::Minus => "-",
        Key::Quote => "'",
        Key::Plus | Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight => key.name(),
        k => k.symbol_or_name(),
    };
    let mods = [(m.command, "Cmd"), (mac && m.ctrl, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift")];
    mods.iter().filter(|(held, _)| *held).map(|(_, n)| *n).chain([name]).collect::<Vec<_>>().join("+")
}

/// Whether two shortcut strings name the same keys.
fn same(a: &str, b: &str) -> bool {
    parse(a).is_some_and(|x| parse(b) == Some(x))
}

/// A command people can give a shortcut in Setup › Keyboard Shortcuts.
#[derive(Debug)]
pub struct Bindable {
    pub id: &'static str,
    pub label: &'static str,
    /// The menu it is in, or Markers, Mix and Transport for commands outside the menus.
    pub category: &'static str,
    /// Where it is: "Edit › Cut Special", or the category outside the menus.
    pub menu: String,
    pub default: Option<&'static str>,
    /// False for built-in key families ("F1-F4").
    pub editable: bool,
}

/// The categories in list order: the menus as in the menu bar, then the commands outside them.
pub const CATEGORIES: [&str; 12] = ["File", "Edit", "View", "Track", "Clip", "Event", "Options", "Setup", "Window", "Markers", "Mix", "Transport"];

/// Commands outside the menus that deserve keys of their own.
const KEY_ONLY: &[&str] = &[
    "transport.pause",
    "transport.rewind",
    "transport.fast_forward",
    "markers.next",
    "markers.previous",
    "mix.solo_safe",
    "mix.phase_invert",
    "mix.clear_solos",
    "mix.clear_mutes",
    "track.hide",
    "track.playlist_new",
    "track.playlist_duplicate",
    "options.keyboard_focus",
];

/// Every command a shortcut can run, by category: the menu items in menu order, then the commands outside the menus.
pub fn bindable() -> &'static [Bindable] {
    static LIST: OnceLock<Vec<Bindable>> = OnceLock::new();
    LIST.get_or_init(|| {
        let aliases = crate::menus::ui_aliases();
        let in_menus =
            soundcraft_engine::catalog::catalog().into_iter().filter_map(|path| Some((crate::menus::command_for_path(path, &aliases)?, path)));
        let keyed =
            soundcraft_engine::command_specs().iter().filter(|c| c.shortcut.is_some() || KEY_ONLY.contains(&c.id)).map(|c| (c.id.to_string(), ""));
        let windows = crate::menus::UI_COMMANDS.iter().map(|c| (c.0.to_string(), c.2));
        let mut list: Vec<Bindable> = Vec::new();
        for (id, path) in in_menus.chain(keyed).chain(windows) {
            if list.iter().all(|b| b.id != id)
                && let Some(b) = bindable_entry(&id, path)
            {
                list.push(b);
            }
        }
        list.sort_by_key(|b| CATEGORIES.iter().position(|c| *c == b.category));
        list
    })
}

/// `path` is the menu item that runs `id`, if one does. Queries and menu items that need their item's parameters are left out.
fn bindable_entry(id: &str, path: &'static str) -> Option<Bindable> {
    let (id, label, default) = match soundcraft_engine::find_command(id) {
        Some(c) if c.journal => (c.id, c.label, c.shortcut),
        Some(_) => return None,
        None => crate::menus::UI_COMMANDS.iter().find(|c| c.0 == id && c.0.starts_with("window.")).map(|c| (c.0, c.1, c.3))?,
    };
    if default.is_none() && crate::menus::params_for("", id) != json!({}) {
        return None;
    }
    let category = match path.split_once(" > ") {
        Some((menu, _)) => menu,
        None => CATEGORIES.into_iter().find(|c| id.split_once('.').is_some_and(|(p, _)| c.eq_ignore_ascii_case(p))).unwrap_or("File"),
    };
    let menu = path.rsplit_once(" > ").map_or(category.to_string(), |(parents, _)| parents.replace(" > ", " › "));
    Some(Bindable { id, label, category, menu, default, editable: default.is_none_or(|d| parse(d).is_some()) })
}

fn default_shortcut(id: &str) -> Option<&'static str> {
    match soundcraft_engine::find_command(id) {
        Some(c) => c.shortcut,
        None => crate::menus::UI_COMMANDS.iter().find(|c| c.0 == id).and_then(|c| c.3),
    }
}

/// A command's shortcut: the one people chose, else its built-in one.
pub fn shortcut_for<'a>(custom: &'a Custom, id: &str) -> Option<&'a str> {
    match custom.get(id) {
        Some(s) => Some(s.as_str()).filter(|s| !s.is_empty()),
        None => default_shortcut(id),
    }
}

/// The other commands whose shortcut is `chord`.
pub fn clashes(custom: &Custom, chord: &str, id: &str) -> Vec<&'static str> {
    bindable().iter().filter(|b| b.id != id && shortcut_for(custom, b.id).is_some_and(|s| same(s, chord))).map(|b| b.id).collect()
}

/// Gives `id` the keys `chord` ("" for none) and takes them from the commands that had them, which it returns.
/// `custom` keeps only shortcuts that differ from the built-in ones.
pub fn assign(custom: &mut Custom, id: &'static str, chord: &str) -> Vec<&'static str> {
    let taken = clashes(custom, chord, id);
    for (cmd, keys) in taken.iter().map(|t| (*t, "")).chain([(id, chord)]) {
        if default_shortcut(cmd).map_or(keys.is_empty(), |d| same(d, keys)) {
            custom.remove(cmd);
        } else {
            custom.insert(cmd.to_string(), keys.to_string());
        }
    }
    taken
}

/// `ui.shortcuts`: every listed command with its shortcut, or a change. `{command, shortcut}` gives the command those keys
/// ("" or null: none) and takes them from other commands, `{command, reset: true}` gives it back its built-in keys the same
/// way, and `{reset: true}` restores every command's.
pub fn configure(custom: &mut Custom, p: &Value) -> Result<Value, String> {
    let reset = p.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = p.get("command").and_then(Value::as_str) else {
        if reset {
            custom.clear();
        }
        let list: Vec<Value> = bindable()
            .iter()
            .map(|b| {
                json!({"id": b.id, "label": b.label, "menu": b.menu, "shortcut": shortcut_for(custom, b.id), "default": b.default,
                    "custom": custom.contains_key(b.id), "editable": b.editable})
            })
            .collect();
        return Ok(json!({"shortcuts": list}));
    };
    let b = bindable().iter().find(|b| b.id == id && b.editable).ok_or_else(|| format!("`{id}` can't have a shortcut of its own"))?;
    let chord = match p.get("shortcut") {
        _ if reset => b.default.unwrap_or("").to_string(),
        Some(Value::String(s)) if !s.is_empty() => {
            let (m, k) = parse(s).ok_or_else(|| format!("can't read the shortcut `{s}`"))?;
            chord_text(k, m, true)
        }
        Some(Value::String(_) | Value::Null) => String::new(),
        _ => return Err("`shortcut` (such as \"Cmd+Shift+K\", or null for none) or `reset` required".into()),
    };
    let taken = assign(custom, b.id, &chord);
    Ok(json!({"command": b.id, "shortcut": shortcut_for(custom, b.id), "taken_from": taken}))
}

/// The command this key press runs: among those with keys people chose (`chosen`), or among the rest.
fn command_for(custom: &Custom, key: Key, mods: Modifiers, mac: bool, chosen: bool) -> Option<&'static str> {
    let hit = |id: &str| shortcut_for(custom, id).and_then(parse).is_some_and(|(m, k)| k == key && mods_match(m, mods, mac));
    bindable().iter().map(|b| b.id).find(|id| custom.contains_key(*id) == chosen && hit(id))
}

/// Whether the held modifiers `got` fire a shortcut that asks for `want`.
fn mods_match(want: Modifiers, got: Modifiers, mac: bool) -> bool {
    // Off macOS egui reports Ctrl as both `ctrl` and `command`: Ctrl is the shortcut's Cmd, and a shortcut's
    // Ctrl (the Mac Control key) has no key of its own there, so Ctrl+X is Cut, not Cmd+Ctrl+X.
    let ctrl = if mac { want.ctrl == got.ctrl } else { !want.ctrl };
    want.command == got.command && want.shift == got.shift && want.alt == got.alt && ctrl
}

/// Cmd+Ctrl+S opens Search. Off macOS Ctrl+S is Save (Cmd+S), so Search is reached from the Window menu.
fn search_chord(m: Modifiers, mac: bool) -> bool {
    mac && m.command && m.ctrl
}

pub fn handle(app: &mut SoundApp, ctx: &egui::Context) {
    // Cmd+= and Cmd+0 are Mix/Edit and Quantize to Grid, not egui's built-in interface zoom.
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    let events = key_presses(ctx, &mut app.v_pressed);
    if ctx.egui_wants_keyboard_input() || app.dialogs.open.is_some() {
        return;
    }
    // Cmd and Ctrl are separate keys only on macOS (asked at runtime so the web build in a Mac browser agrees).
    let mac = ctx.os() == egui::os::OperatingSystem::Mac;
    if app.shortcut_editor.capturing.is_some() {
        if let Some((key, mods)) = events.into_iter().find(|(k, _)| !is_modifier(*k)) {
            crate::shortcut_editor::captured(app, key, mods, mac);
        }
        ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Key { .. })));
        return;
    }
    for (key, mods) in events {
        let chosen = command_for(&app.ui.shortcuts, key, mods, mac, true);
        if chosen.is_none() && fixed(app, key, mods, mac) {
            continue;
        }
        let Some(id) = chosen.or_else(|| command_for(&app.ui.shortcuts, key, mods, mac, false)) else { continue };
        if !crate::menus::invoke_shortcut_dialog(app, id) {
            let _ = app.run(id, json!({}));
        }
    }
}

/// The desktop sends Cmd+X/C/V (and Windows' Shift+Delete, Ctrl+Insert, Shift+Insert) only as clipboard events, and
/// drops Cmd+V's press while the clipboard holds no text, so a V release whose press never came counts as Cmd+V.
fn key_presses(ctx: &egui::Context, v_pressed: &mut bool) -> Vec<(Key, Modifiers)> {
    ctx.input(|i| {
        let clipboard = |key| (!cfg!(target_arch = "wasm32")).then_some((key, if i.modifiers.command { i.modifiers } else { Modifiers::COMMAND }));
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => {
                    *v_pressed |= *key == Key::V;
                    Some((if modifiers.shift { unshifted(*key) } else { *key }, *modifiers))
                }
                egui::Event::Key { key: Key::V, pressed: false, .. } if !std::mem::take(v_pressed) => clipboard(Key::V),
                egui::Event::Cut => clipboard(Key::X),
                egui::Event::Copy => clipboard(Key::C),
                egui::Event::Paste(_) => {
                    *v_pressed |= i.modifiers.command;
                    clipboard(Key::V)
                }
                _ => None,
            })
            .collect()
    })
}

pub fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft | Key::ShiftRight | Key::ControlLeft | Key::ControlRight | Key::AltLeft | Key::AltRight | Key::SuperLeft | Key::SuperRight
    )
}

/// Runs a built-in key's command, unless people gave that command other keys.
fn run_unless_moved(app: &mut SoundApp, id: &str) {
    if !app.ui.shortcuts.contains_key(id) {
        let _ = app.run(id, json!({}));
    }
}

/// Keys with context-dependent meaning. Returns true when handled.
fn fixed(app: &mut SoundApp, key: Key, m: Modifiers, mac: bool) -> bool {
    let plain = !m.command && !m.alt && !m.ctrl && !m.shift;
    match key {
        Key::Space if !m.command && !m.alt => {
            run_unless_moved(app, if m.shift { "transport.half_speed" } else { "transport.toggle" });
            true
        }
        Key::Space if m.command => {
            run_unless_moved(app, "transport.record");
            true
        }
        Key::S if search_chord(m, mac) => {
            run_unless_moved(app, "window.search");
            true
        }
        Key::Equals if m.command && !(mac && m.ctrl) => {
            run_unless_moved(app, "window.toggle_mix_edit");
            true
        }
        Key::Enter if plain => {
            run_unless_moved(app, "markers.add");
            true
        }
        Key::F1 | Key::F2 | Key::F3 | Key::F4 if plain => {
            let mode = match key {
                Key::F1 => "shuffle",
                Key::F2 => "slip",
                Key::F3 => "spot",
                _ => "grid",
            };
            let _ = app.run("edit.mode", json!({"mode": mode}));
            true
        }
        Key::F5 | Key::F6 | Key::F7 | Key::F8 | Key::F9 | Key::F10 if plain => {
            let tool = match key {
                Key::F5 => "zoom",
                Key::F6 => "trim",
                Key::F7 => "selector",
                Key::F8 => "grabber",
                Key::F9 => "scrubber",
                _ => "pencil",
            };
            let _ = app.run("edit.tool", json!({"tool": tool}));
            true
        }
        // Selected notes in an open MIDI editor take the key.
        Key::Delete | Key::Backspace if plain && app.ui.window == MainWindow::Edit && (app.midi.selected.is_empty() || !app.ui.show_midi_editor) => {
            let _ = app.run("edit.clear", json!({}));
            true
        }
        Key::Plus | Key::Minus if !m.command && app.ui.window == MainWindow::Edit => {
            let dir = if key == Key::Plus { 1 } else { -1 };
            let _ = app.run("edit.nudge", json!({"direction": dir}));
            true
        }
        Key::Tab if plain || m.alt => {
            // Tab to next clip boundary (or transient when enabled).
            let s = app.engine.session();
            let at = s.edit.selection.start;
            let back = m.alt;
            if s.edit.tab_to_transient
                && !back
                && let Some(t) = s.edit.selected_tracks.first().copied()
            {
                let r = soundcraft_time::Range::new(at + 1, s.content_end());
                if let Some(n) = soundcraft_engine::io::transients_in(s, t, r, 0.5).into_iter().find(|x| *x > at + 64) {
                    let _ = app.run("transport.locate", json!({"at": n}));
                    return true;
                }
            }
            let mut edges: Vec<i64> = s
                .tracks
                .iter()
                .filter(|t| s.edit.selected_tracks.contains(&t.id))
                .flat_map(|t| t.clips().iter().flat_map(|c| [c.start, c.end()]))
                .collect();
            edges.sort_unstable();
            let next = if back { edges.iter().rev().find(|e| **e < at).copied() } else { edges.iter().find(|e| **e > at).copied() };
            if let Some(n) = next {
                let _ = app.run("transport.locate", json!({"at": n}));
            }
            true
        }
        Key::ArrowUp | Key::ArrowDown if m.ctrl && !m.shift => {
            let id = if key == Key::ArrowUp { "edit.extend_selection_up" } else { "edit.extend_selection_down" };
            let _ = app.run(id, json!({}));
            true
        }
        // Ctrl+Shift+Up/Down nudge clip gain off macOS too (the registry's Ctrl is the Mac Control key).
        Key::ArrowUp | Key::ArrowDown if !mac && m.ctrl && m.shift && !m.alt => {
            run_unless_moved(app, if key == Key::ArrowUp { "clip.gain_nudge_up" } else { "clip.gain_nudge_down" });
            true
        }
        // Commands Keyboard Focus: single-key editing (Edit window only).
        k if plain && app.ui.window == MainWindow::Edit && app.engine.session().edit.keyboard_focus == "commands" && commands_focus(app, k) => true,
        Key::R | Key::T if plain => {
            // Zoom out / in on the timeline (Commands Focus style).
            let id = if key == Key::T { "view.zoom_in" } else { "view.zoom_out" };
            let _ = app.run(id, json!({}));
            true
        }
        _ => false,
    }
}

/// Single-key commands. Returns true when `key` was handled.
fn commands_focus(app: &mut SoundApp, key: Key) -> bool {
    let id = match key {
        Key::A => "edit.trim_start_to_insertion",
        Key::S => "edit.trim_end_to_insertion",
        Key::D => "edit.fade_to_start",
        Key::G => "edit.fade_to_end",
        Key::F => "edit.fades_create",
        Key::B => "edit.separate",
        Key::X => "edit.cut",
        Key::C => "edit.copy",
        Key::V => "edit.paste",
        Key::Z => "edit.undo",
        Key::P => {
            move_selection_track(app, -1);
            return true;
        }
        Key::Semicolon => {
            move_selection_track(app, 1);
            return true;
        }
        Key::L | Key::Quote => {
            tab_clip(app, key == Key::L);
            return true;
        }
        _ => return false,
    };
    let _ = app.run(id, json!({}));
    true
}

/// Move the edit selection to the track above/below (P / ;).
fn move_selection_track(app: &mut SoundApp, dir: i64) {
    let s = app.engine.session();
    let visible: Vec<u64> = s.tracks.iter().filter(|t| !t.hidden).map(|t| t.id.0).collect();
    let Some(cur) = s.edit.selected_tracks.first().map(|t| t.0) else { return };
    let Some(i) = visible.iter().position(|t| *t == cur) else { return };
    let j = (i as i64 + dir).clamp(0, visible.len() as i64 - 1) as usize;
    if let Some(t) = visible.get(j) {
        let sel = s.edit.selection;
        let _ = app.run("edit.select", json!({"tracks": [t], "start": sel.start, "end": sel.end, "exact": true}));
    }
}

/// Tab to the previous/next clip boundary on the selected tracks (L / ').
fn tab_clip(app: &mut SoundApp, back: bool) {
    let s = app.engine.session();
    let at = s.edit.selection.start;
    let mut edges: Vec<i64> =
        s.tracks.iter().filter(|t| s.edit.selected_tracks.contains(&t.id)).flat_map(|t| t.clips().iter().flat_map(|c| [c.start, c.end()])).collect();
    edges.sort_unstable();
    let next = if back { edges.iter().rev().find(|e| **e < at).copied() } else { edges.iter().find(|e| **e > at).copied() };
    if let Some(n) = next {
        let _ = app.run("transport.locate", json!({"at": n}));
    }
}

/// Shortcut text for display. The registry stores macOS-style strings ("Cmd+N"); off macOS the Cmd
/// modifier is the Ctrl key. Mac-only Control chords ("Cmd+Ctrl+S") have no key there, so they show nothing.
pub fn shortcut_label(s: &str, mac: bool) -> String {
    if mac || !s.split('+').any(|p| p == "Cmd") {
        return s.to_string();
    }
    if s.split('+').any(|p| p == "Ctrl") {
        return String::new();
    }
    s.split('+').map(|p| if p == "Cmd" { "Ctrl" } else { p }).collect::<Vec<_>>().join("+")
}

#[cfg(test)]
mod label_tests {
    use super::shortcut_label;

    #[test]
    fn cmd_becomes_ctrl_off_mac() {
        assert_eq!(shortcut_label("Cmd+N", false), "Ctrl+N");
        assert_eq!(shortcut_label("Cmd+Shift+W", false), "Ctrl+Shift+W");
        assert_eq!(shortcut_label("Cmd+Alt+B", false), "Ctrl+Alt+B");
        assert_eq!(shortcut_label("Cmd+,", false), "Ctrl+,");
        assert_eq!(shortcut_label("Shift+R", false), "Shift+R");
        assert_eq!(shortcut_label("Cmd+Ctrl+S", false), "");
        assert_eq!(shortcut_label("", false), "");
    }

    #[test]
    fn unchanged_on_mac() {
        for s in ["Cmd+N", "Cmd+Shift+W", "Cmd+Alt+B", "Shift+R", "Cmd+Ctrl+S"] {
            assert_eq!(shortcut_label(s, true), s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Custom, command_for, mods_match, parse, search_chord};
    use egui::{Key, Modifiers};

    /// Ctrl held on Windows or Linux, as egui reports it.
    const PC_CTRL: Modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
    /// Cmd held on macOS.
    const MAC_CMD: Modifiers = Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE };

    fn want(s: &str) -> Modifiers {
        parse(s).map(|(m, _)| m).unwrap()
    }

    #[test]
    fn ctrl_is_cmd_off_macos() {
        assert!(mods_match(want("Cmd+S"), PC_CTRL, false));
        assert!(mods_match(want("Cmd+Shift+N"), Modifiers { shift: true, ..PC_CTRL }, false));
        assert!(mods_match(want("Cmd+X"), PC_CTRL, false));
        assert!(!mods_match(want("Cmd+Ctrl+X"), PC_CTRL, false));
        assert!(!mods_match(want("Cmd+S"), Modifiers::NONE, false));
        assert!(!search_chord(PC_CTRL, false));
    }

    #[test]
    fn cmd_and_ctrl_stay_apart_on_macos() {
        assert!(mods_match(want("Cmd+S"), MAC_CMD, true));
        assert!(!mods_match(want("Cmd+S"), Modifiers::CTRL, true));
        assert!(mods_match(want("Cmd+Ctrl+X"), Modifiers { ctrl: true, ..MAC_CMD }, true));
        assert!(!mods_match(want("Cmd+X"), Modifiers { ctrl: true, ..MAC_CMD }, true), "Cmd+Ctrl+X is not Cut");
        assert!(!search_chord(MAC_CMD, true));
        assert!(search_chord(Modifiers { ctrl: true, ..MAC_CMD }, true));
    }

    #[test]
    fn shift_cmd_k_prompts_for_a_folder_to_export_clips() {
        assert_eq!(command_for(&Custom::new(), Key::K, Modifiers { shift: true, ..MAC_CMD }, true, false), Some("file.export_clips"));
        assert_eq!(command_for(&Custom::new(), Key::K, Modifiers { shift: true, ..PC_CTRL }, false, false), Some("file.export_clips"));
        assert!(crate::dialogs::takes_path("file.export_clips"));
    }
}
