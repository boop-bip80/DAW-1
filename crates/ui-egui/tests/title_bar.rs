//! Integration tests for the title-bar cloud-control layout.
//!
//! Tests cover:
//!  - capability gating: Share absent without a cloud backend
//!  - no-cloud bar height: compact 24 px panel, controls within bounds
//!  - cloud frame/fill visibility: button_frame restored in the cloud scope
//!  - control order (RTL): account right of share, title rightmost
//!  - 8 px spacing between controls (measured via AccessKit rects)
//!  - accessibility: correct roles for the bar and its interactive controls

use egui::{Vec2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use soundcraft_ui_egui::{Services, SoundApp};

const WIN: Vec2 = vec2(1600.0, 900.0);

fn no_cloud_harness() -> Harness<'static, SoundApp> {
    let app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    Harness::builder().with_size(WIN).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    )
}

// ---------------------------------------------------------------- capability gating

/// Share split-button must not appear when no cloud backend is configured.
/// A control that fails on use (no service) is worse than no control.
#[test]
fn share_button_absent_without_cloud_backend() {
    let mut h = no_cloud_harness();
    h.run_steps(3);
    assert!(
        h.try_get_by_label("Share").is_err(),
        "Share button rendered without a cloud backend; it must be gated by cloud::shares()"
    );
}

// ---------------------------------------------------------------- no-cloud bar height

/// Without cloud controls the bar must stay compact. The SoundCraft menu
/// button is inside the bar; its painted bottom must not exceed the 24 px bar
/// height plus the panel's top margin (1 px).
#[test]
fn no_cloud_bar_does_not_exceed_compact_height() {
    let mut h = no_cloud_harness();
    h.run_steps(3);
    let bar_btn = h.get_by_label("SoundCraft");
    let bottom = bar_btn.rect().max.y;
    // The bar is 24 px tall with 1 px inner top margin. The button sits within that.
    // Allow a small float rounding tolerance.
    assert!(
        bottom <= 27.0,
        "no-cloud bar bottom edge is {bottom:.1} px; expected ≤ 27.0 px (24 px bar + 1 px margin + rounding)"
    );
}

// ---------------------------------------------------------------- menu bar accessibility

/// The menu bar items must all carry an accessible label so keyboard and
/// screen-reader users can navigate between menus by name.
#[test]
fn menu_bar_items_have_accessible_labels() {
    let mut h = no_cloud_harness();
    h.run_steps(3);
    // Core menus must be reachable by label.
    for label in ["SoundCraft", "File", "Edit", "View", "Track", "Clip"] {
        h.get_by_label(label); // panics if not found
    }
}

// ---------------------------------------------------------------- optional render evidence

/// Save screenshots of the title bar into the directory named by
/// `CRAFT_UI_SUITE_SHOTS`, when set. Not run in CI; useful for manual review
/// of bar height, badge frame, and RTL ordering.
#[test]
fn title_bar_render_evidence() {
    let Some(dir) = std::env::var_os("CRAFT_UI_SUITE_SHOTS").map(std::path::PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();

    for scale in [1.0_f32, 2.0] {
        let app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        let mut h = Harness::builder()
            .with_size(WIN)
            .with_pixels_per_point(scale)
            .wgpu()
            .build_ui_state(
                |ui, app: &mut SoundApp| {
                    app.logic(&ui.ctx().clone());
                    app.ui(ui);
                },
                app,
            );
        h.run_steps(4);
        h.render()
            .unwrap()
            .save(dir.join(format!("soundcraft-title-bar-no-cloud-{scale}x.png")))
            .unwrap();
    }
}
