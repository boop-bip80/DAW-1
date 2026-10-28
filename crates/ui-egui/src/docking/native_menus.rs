//! Bridge the pinned craft-ui renderer's stock menus to the app's deferred native presenter.
//! The renderer owns tab geometry and gestures. Only its open menu is intercepted; a discarded
//! pass removes its egui drawing before the host presents AppKit, outside the SoundApp borrow.
use super::*;
use crate::context_menu::{self, Anchor, Entry, Request};
use egui::{Context, Popup, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Panel,
    Group,
    Overflow,
    Column,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub window: crate::MainWindow,
    pub expected: Box<Workspace>,
    pub action: Action<Panel>,
}

#[derive(Clone, Copy)]
enum Kind {
    Frame,
    Dock { minimized: bool, column: Panel, iconic: bool },
    Floating { minimized: bool, whole: bool },
    Stack,
}

struct PanelGroup {
    members: Vec<Panel>,
    active: Panel,
    kind: Kind,
}

fn groups(layout: &Layout<Panel>) -> Vec<PanelGroup> {
    let mut out = Vec::new();
    let mut todo: Vec<_> = layout.root.iter().map(|node| (node, Kind::Frame)).collect();
    for floating in &layout.floating {
        todo.push((&floating.root, Kind::Floating { minimized: floating.minimized, whole: matches!(floating.root, Node::Tabs { .. }) }));
    }
    while let Some((node, kind)) = todo.pop() {
        match node {
            Node::Split { first, second, .. } => {
                todo.push((second, kind));
                todo.push((first, kind));
            }
            Node::Tabs { panels, active } => {
                if let Some(active) = panels.get(*active) {
                    out.push(PanelGroup { members: panels.clone(), active: *active, kind });
                }
            }
            Node::Stack { entries } => {
                for entry in entries {
                    out.push(PanelGroup { members: vec![entry.panel], active: entry.panel, kind: Kind::Stack });
                }
            }
            Node::Dock { columns, .. } => {
                for column in columns {
                    let Some(first) = column.groups.iter().find_map(|g| g.panels.first()).copied() else { continue };
                    for group in &column.groups {
                        if let Some(active) = group.panels.get(group.active) {
                            out.push(PanelGroup {
                                members: group.panels.clone(),
                                active: *active,
                                kind: Kind::Dock { minimized: group.minimized, column: first, iconic: column.iconic },
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn workspace(app: &crate::SoundApp, window: crate::MainWindow) -> &Workspace {
    if window == crate::MainWindow::Edit { &app.ui.docking } else { &app.ui.mix_docking }
}

pub(super) fn record_body(ctx: &Context, window: crate::MainWindow, panel: Panel, rect: Rect) {
    ctx.data_mut(|data| data.insert_temp(area_id(window).with(("native-menu-body", panel)), rect));
}

/// Stock widget identities are confined here and covered by renderer-driven integration tests.
/// Revisit this adapter with the pinned craft-ui renderer when updating that dependency.
pub(super) fn collect(app: &mut crate::SoundApp, ui: &egui::Ui, window: crate::MainWindow) {
    if !app.context_menus.enabled || app.context_menus.active || app.context_menus.pending.is_some() {
        return;
    }
    let ctx = ui.ctx();
    let area = area_id(window);
    for group in groups(&workspace(app, window).layout) {
        let mut controls: Vec<_> = group.members.iter().map(|panel| (area.with(("tab", panel)), *panel, MenuKind::Panel)).collect();
        controls.push((area.with(("drag-bar", group.members.first())), group.active, MenuKind::Group));
        controls.push((area.with(("overflow", group.active)), group.active, MenuKind::Overflow));
        if let Kind::Dock { column, .. } = group.kind {
            controls.push((area.with(("column-bar", Some(&column))), column, MenuKind::Column));
        }
        for (id, panel, kind) in controls {
            let Some(response) = ctx.read_response(id) else { continue };
            let popup = Popup::default_response_id(&response);
            if !Popup::is_id_open(ctx, popup) {
                continue;
            }
            Popup::close_id(ctx, popup);
            ctx.request_discard("present docking menu natively");
            let target = context_menu::Target::Dock { window, panel, kind };
            let entries = describe(app, window, panel, kind);
            if !entries.is_empty() {
                let anchor = if kind != MenuKind::Panel { Anchor::Below { rect: response.rect, zoom: ctx.zoom_factor() } } else { Anchor::Pointer };
                app.context_menus.pending = Some(Request { target, entries, viewport: ctx.viewport_id(), epoch: app.context_menus.epoch, anchor });
                ctx.request_repaint();
            }
            return;
        }
    }
}

pub fn valid(app: &crate::SoundApp, window: crate::MainWindow, panel: Panel) -> bool {
    workspace(app, window).layout.contains(&panel)
}

pub fn execute(app: &mut crate::SoundApp, selection: Selection) -> Result<(), String> {
    let current = workspace(app, selection.window);
    if *current != *selection.expected {
        return Err("Panel layout changed while the menu was open; open the menu again".into());
    }
    let previous = current.clone();
    let edit = selection.window == crate::MainWindow::Edit;
    command_for(app, &DockCommand::Layout { action: selection.action }, edit)?;
    cancel_replaced_workspace(app, &previous, workspace(app, selection.window), edit);
    Ok(())
}

pub fn describe(app: &crate::SoundApp, window: crate::MainWindow, panel: Panel, kind: MenuKind) -> Vec<Entry> {
    let state = workspace(app, window);
    let all = groups(&state.layout);
    let Some(group) = all.iter().find(|group| group.members.contains(&panel)) else { return vec![] };
    let item = |label: &str, action: Action<Panel>| -> Option<Entry> {
        let mut probe = state.clone();
        if apply(&mut probe, action.clone()).is_err() || probe == *state {
            return None;
        }
        Some(Entry::item(label, context_menu::Action::Dock(Selection { window, expected: Box::new(state.clone()), action })))
    };
    if kind == MenuKind::Overflow {
        return group
            .members
            .iter()
            .map(|member| {
                Entry::item(
                    member.label(),
                    context_menu::Action::Dock(Selection { window, expected: Box::new(state.clone()), action: Action::Activate { panel: *member } }),
                )
                .checked(*member == group.active)
            })
            .collect();
    }
    if kind == MenuKind::Column {
        return if let Kind::Dock { iconic, .. } = group.kind {
            item(if iconic { "Expand column" } else { "Collapse column to icons" }, Action::SetIconic { panel, iconic: !iconic })
                .into_iter()
                .collect()
        } else {
            vec![]
        };
    }
    let body =
        app.docking_context.as_ref().and_then(|ctx| ctx.data(|data| data.get_temp::<Rect>(area_id(window).with(("native-menu-body", group.active)))));
    let min = panel_limits(&panel).min;
    let rect = body.unwrap_or_else(|| Rect::from_min_size(egui::pos2(80.0, 80.0), min.max(egui::vec2(320.0, 240.0))));
    let size = rect.size().max(min).min(egui::vec2(1600.0, 1000.0));
    let float = [rect.left() + 24.0, (rect.top() - MIDI_TAB_HEIGHT).max(0.0) + 24.0, size.x, size.y];
    let mut entries = Vec::new();
    if kind == MenuKind::Panel {
        entries.extend(item("Float Panel", Action::Float { panel, rect: float }));
        let targets: Vec<_> = state
            .layout
            .move_targets(&panel, permissions)
            .into_iter()
            .filter_map(|target| item(target.label(), Action::Move { panel, anchor: *target, placement: Placement::Tab { before: None } }))
            .collect();
        if !targets.is_empty() {
            entries.push(Entry::Submenu { label: "Move Panel to Group".into(), entries: targets });
        }
        entries.extend(item("Close Panel", Action::Close { panel }));
        if !matches!(group.kind, Kind::Stack) && !entries.is_empty() {
            entries.push(Entry::Separator);
        }
    }
    if !matches!(group.kind, Kind::Stack) {
        entries.extend(item("Close Tab Group", Action::CloseGroup { panel }));
        let minimized = match group.kind {
            Kind::Dock { minimized, .. } | Kind::Floating { minimized, whole: true } => Some(minimized),
            _ => None,
        };
        if let Some(minimized) = minimized {
            entries.extend(item(if minimized { "Expand" } else { "Minimize" }, Action::SetMinimized { panel, minimized: !minimized }));
        }
        if let Kind::Dock { .. } = group.kind {
            entries.extend(item("Collapse to Icons", Action::SetIconic { panel, iconic: true }));
        }
        if !matches!(group.kind, Kind::Floating { whole: true, .. }) {
            entries.extend(item("Float Tab Group", Action::MoveGroup { panel, to: Target::Float { rect: Some(float) } }));
        }
        let mut seen = Vec::new();
        let targets: Vec<_> = state
            .layout
            .move_targets(&panel, permissions)
            .into_iter()
            .filter_map(|target| {
                let index = all.iter().position(|g| g.members.contains(target))?;
                if seen.contains(&index) {
                    return None;
                }
                seen.push(index);
                item(target.label(), Action::MoveGroup { panel, to: Target::At { anchor: *target, placement: Placement::Tab { before: None } } })
            })
            .collect();
        if !targets.is_empty() {
            entries.push(Entry::Submenu { label: "Move Group To".into(), entries: targets });
        }
    }
    if matches!(entries.last(), Some(Entry::Separator)) {
        entries.pop();
    }
    entries
}
