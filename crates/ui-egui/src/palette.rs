//! Search: a command palette over every engine and UI command (Window › Search, Cmd+Ctrl+S).

use crate::SoundApp;
use crate::theme::{Tokens, mono};
use egui::{Align2, Key, vec2};

#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub query: String,
    pub selected: usize,
    last_query: String,
}

impl PaletteState {
    /// Detect changes from typing, pasting, or programmatic query updates.
    fn sync_query(&mut self) -> bool {
        if self.query == self.last_query {
            return false;
        }
        self.last_query.clone_from(&self.query);
        true
    }

    /// A changed query always selects its first match; stale indices never carry over.
    fn navigate(&mut self, count: usize, query_changed: bool, down: bool, up: bool) {
        if count == 0 || query_changed {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(count - 1);
        match (down, up) {
            (true, false) => self.selected = self.selected.saturating_add(1).min(count - 1),
            (false, true) => self.selected = self.selected.saturating_sub(1),
            _ => {}
        }
    }

    /// Never execute a stale result, including when typing and Enter arrive in one frame.
    fn submit_index(&self, count: usize, enter: bool, query_changed: bool) -> Option<usize> {
        (enter && !query_changed && self.selected < count).then_some(self.selected)
    }
}

struct Entry {
    id: String,
    label: String,
    path: String,
    shortcut: String,
}

fn entries() -> Vec<Entry> {
    let mut v: Vec<Entry> = soundcraft_engine::command_specs()
        .iter()
        .map(|c| Entry {
            id: c.id.to_string(), label: c.label.to_string(), path: c.menu.join(" › "), shortcut: c.shortcut.unwrap_or("").to_string()
        })
        .collect();
    for (id, label, path, sc) in crate::menus::UI_COMMANDS {
        v.push(Entry { id: id.to_string(), label: label.to_string(), path: path.replace(" > ", " › "), shortcut: sc.unwrap_or("").to_string() });
    }
    v
}

/// Subsequence match score (higher is better), or None.
fn score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if let Some(i) = h.find(&n) {
        return Some(1000 - i32::try_from(i).unwrap_or(999));
    }
    let mut it = h.chars();
    let mut gaps = 0;
    for c in n.chars() {
        let mut found = false;
        for hc in it.by_ref() {
            if hc == c {
                found = true;
                break;
            }
            gaps += 1;
        }
        if !found {
            return None;
        }
    }
    Some(500 - gaps)
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.ui.show_search {
        return;
    }
    let t = Tokens::current();
    let mut run: Option<String> = None;
    let mut close = ctx.input(|i| i.key_pressed(Key::Escape));
    egui::Window::new("Search")
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .fixed_size(vec2(560.0, 380.0))
        .show(ctx, |ui| {
            let r = ui
                .add(egui::TextEdit::singleline(&mut app.palette.query).hint_text("Search commands…").desired_width(f32::INFINITY).font(mono(15.0)));
            r.request_focus();
            let query_changed = app.palette.sync_query();
            let q = app.palette.query.trim().to_string();
            let mut hits: Vec<(i32, Entry)> =
                entries().into_iter().filter_map(|e| score(&format!("{} {} {}", e.label, e.path, e.id), &q).map(|s| (s, e))).collect();
            hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.label.cmp(&b.1.label)));
            hits.truncate(14);
            let (down, up, enter) = ctx.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
            app.palette.navigate(hits.len(), query_changed, down, up);
            ui.separator();
            if hits.is_empty() {
                ui.label(egui::RichText::new("No matching commands").color(t.text_dim));
            }
            for (i, (_, e)) in hits.iter().enumerate() {
                let sel = i == app.palette.selected;
                let resp = ui.horizontal(|ui| {
                    let l = ui.selectable_label(sel, &e.label);
                    ui.label(egui::RichText::new(&e.path).small().color(t.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let sc = crate::shortcuts::shortcut_label(&e.shortcut, ctx.os().is_mac());
                        ui.label(egui::RichText::new(sc).font(mono(10.0)).color(t.text_dim));
                    });
                    l
                });
                if resp.inner.clicked() {
                    run = Some(e.id.clone());
                }
            }
            if let Some(index) = app.palette.submit_index(hits.len(), enter, query_changed)
                && let Some((_, e)) = hits.get(index)
            {
                run = Some(e.id.clone());
            }
        });
    if let Some(id) = run {
        close = true;
        let path = entries().into_iter().find(|e| e.id == id).map(|e| e.path.replace(" › ", " > ")).unwrap_or_default();
        crate::menus::invoke_menu(app, &id, &path);
    }
    if close {
        app.ui.show_search = false;
        app.palette = PaletteState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::PaletteState;

    #[test]
    fn scoring_prefers_substrings() {
        assert!(super::score("Duplicate", "dup") > super::score("Duplicate", "dpl"));
        assert!(super::score("Bounce Mix", "xyz").is_none());
        assert_eq!(super::score("anything", ""), Some(0));
    }

    #[test]
    fn changing_query_resets_selection_and_blocks_same_frame_enter() {
        let mut state = PaletteState { selected: 8, ..PaletteState::default() };
        state.query = "bounce".into();
        let changed = state.sync_query();
        assert!(changed);
        state.navigate(11, changed, true, false);
        assert_eq!(state.selected, 0);
        assert_eq!(state.submit_index(11, true, changed), None);
        assert!(!state.sync_query());
        assert_eq!(state.submit_index(11, true, false), Some(0));

        state.selected = 5;
        state.query = "export".into();
        let changed = state.sync_query();
        state.navigate(9, changed, false, false);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn empty_results_cannot_be_navigated_or_executed() {
        let mut state = PaletteState { selected: usize::MAX, ..PaletteState::default() };
        state.navigate(0, false, true, false);
        assert_eq!(state.selected, 0);
        assert_eq!(state.submit_index(0, true, false), None);
        state.navigate(0, false, false, true);
        assert_eq!(state.selected, 0);
        assert_eq!(state.submit_index(0, true, false), None);
    }

    #[test]
    fn navigation_clamps_to_visible_results_and_ignores_opposing_arrows() {
        let mut state = PaletteState { selected: usize::MAX, ..PaletteState::default() };
        state.navigate(3, false, true, false);
        assert_eq!(state.selected, 2);
        state.navigate(3, false, false, true);
        assert_eq!(state.selected, 1);
        state.navigate(3, false, true, true);
        assert_eq!(state.selected, 1);
        state.navigate(3, false, false, true);
        state.navigate(3, false, false, true);
        assert_eq!(state.selected, 0);
        assert_eq!(state.submit_index(3, true, false), Some(0));
        assert_eq!(state.submit_index(3, false, false), None);
        state.selected = 5;
        assert_eq!(state.submit_index(3, true, false), None);
    }
}
