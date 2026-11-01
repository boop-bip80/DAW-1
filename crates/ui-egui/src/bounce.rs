//! Bounce Mix renders a slice per frame behind a progress window, so the app keeps drawing and the
//! bounce can be cancelled.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use soundcraft_audio_io::EncodeOptions;
#[cfg(not(target_arch = "wasm32"))]
use soundcraft_time::Range;

/// Starts the bounce `file.bounce_mix` runs for `params`; [`step`] renders it.
pub fn start(app: &mut SoundApp, params: &Value) {
    if app.bounce.is_some() || stem_running(app) {
        app.ui.status = "A bounce is already running".into();
        return;
    }
    // Third-party plugins keep state the session can't see: store it first.
    app.capture_plugin_states();
    match soundcraft_engine::cmd::bounce_job(&app.engine, params) {
        Ok(job) => app.bounce = Some(job),
        Err(e) => app.ui.status = e.to_string(),
    }
}

/// Start an incremental native stem export only after the user confirms the export dialog.
/// Merely opening or painting that dialog never touches the filesystem or command journal.
#[cfg(not(target_arch = "wasm32"))]
pub fn start_stems(app: &mut SoundApp, dir: &str, range: Range, options: &EncodeOptions, fold_stereo: bool) {
    if app.bounce.is_some() || stem_running(app) {
        app.ui.status = "Finish or cancel the current bounce first".into();
        return;
    }
    if app.engine.transport.recording {
        app.ui.status = "Stop recording before exporting stems".into();
        return;
    }
    // Plugin state is captured only on an explicit Start click, never on dialog paint.
    app.capture_plugin_states();
    match soundcraft_engine::stem_bounce::StemBounceJob::new(&app.engine, dir, range, options, fold_stereo) {
        Ok(job) => {
            let count = job.total_stems();
            app.dialogs.stem_bounce = Some(job);
            app.ui.status = format!("Exporting {count} stems…");
        }
        Err(error) => app.ui.status = error.to_string(),
    }
}

pub(crate) fn stem_running(app: &SoundApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.dialogs.stem_bounce.is_some()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        false
    }
}

/// Renders the running bounce for a moment. Called from `logic`, which eframe keeps calling while
/// the window is hidden as long as a repaint is requested, so a bounce doesn't pause then.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    if let Some(job) = &mut app.bounce {
        app.ui.status = match job.step(slice()) {
            Ok(None) => {
                ctx.request_repaint();
                return;
            }
            Ok(Some(done)) => {
                format!(
                    "Bounced {} (peak {:.1} dBFS, {:.1} LUFS)",
                    job.path().file_name().unwrap_or_default().to_string_lossy(),
                    done.peak_db,
                    done.lufs
                )
            }
            Err(e) => e.to_string(),
        };
        app.bounce = None;
    }
    #[cfg(not(target_arch = "wasm32"))]
    step_stems(app, ctx);
}

#[cfg(not(target_arch = "wasm32"))]
fn step_stems(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = app.dialogs.stem_bounce.as_mut() else { return };
    match job.step(slice()) {
        Ok(None) => ctx.request_repaint(),
        Ok(Some(_)) => {
            // The engine-owned completion method checks the final commit and consumes the job:
            // one journal event, no second export and no undo/document change.
            if let Some(job) = app.dialogs.stem_bounce.take() {
                app.ui.status = match job.record_completed(&mut app.engine) {
                    Ok(files) => format!("Exported {} stems", files.len()),
                    Err(error) => error.to_string(),
                };
            }
        }
        Err(error) => {
            app.dialogs.stem_bounce = None;
            app.ui.status = error.to_string();
        }
    }
}

/// Refuses to replace the session while a bounce holds its own copy: two large sessions could run
/// out of memory. True when refused; the status line says why.
pub fn refuses_session_switch(app: &mut SoundApp) -> bool {
    let running = app.bounce.is_some() || stem_running(app);
    if running {
        app.ui.status = "Finish or cancel the bounce first".into();
    }
    running
}

/// Whether to render another block in this call: for 30 ms.
#[cfg(not(target_arch = "wasm32"))]
fn slice() -> impl FnMut() -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(30);
    move || std::time::Instant::now() < until
}

/// The browser has no `Instant` (it panics there), so a call renders 32 blocks.
#[cfg(target_arch = "wasm32")]
fn slice() -> impl FnMut() -> bool {
    let mut blocks = 32;
    move || {
        blocks -= 1;
        blocks > 0
    }
}

/// Shows how far the running bounce got, with Cancel (or Esc).
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    if let Some(job) = &app.bounce {
        let mut cancel = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Modal::new(egui::Id::unique("bounce_progress")).show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            ui.set_width(320.0);
            let name = job.path().file_name().unwrap_or_default().to_string_lossy();
            ui.label(egui::RichText::new(format!("Bouncing {name}")).font(bold(13.0)));
            ui.add(egui::ProgressBar::new(job.progress()));
            ui.horizontal(|ui| {
                cancel |= ui.button("Cancel").clicked();
                ui.label(egui::RichText::new(format!("{:.0}%", job.progress() * 100.0)).color(Tokens::current().text_dim));
            });
        });
        if cancel {
            app.bounce = None;
            app.ui.status = "Bounce cancelled".into();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    show_stems(app, ctx);
}

#[cfg(not(target_arch = "wasm32"))]
fn show_stems(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = app.dialogs.stem_bounce.as_ref() else { return };
    let mut cancel = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    egui::Modal::new(egui::Id::unique("stem_bounce_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(350.0);
        ui.label(egui::RichText::new("Exporting Stems").font(bold(13.0)));
        let progress = job.progress();
        let activity = if let Some(name) = job.current_stem() {
            format!("Rendering stem {} of {}: {name}", job.completed_stems() + 1, job.total_stems())
        } else {
            format!("Publishing {} stem files without overwriting existing files…", job.total_stems())
        };
        ui.label(activity);
        ui.add(egui::ProgressBar::new(progress).text(format!("{:.0}% complete", progress * 100.0)));
        ui.horizontal(|ui| {
            cancel |= ui.button("Cancel stem export").clicked();
            ui.label(egui::RichText::new("Cancel removes only unfinished export files").small().color(Tokens::current().text_dim));
        });
    });
    if cancel {
        app.dialogs.stem_bounce = None;
        app.ui.status = "Stem export cancelled; unfinished export files removed".into();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use soundcraft_model::{ChannelFormat, TrackKind};

    struct TestFolder(std::path::PathBuf);

    impl TestFolder {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path =
                std::env::temp_dir().join(format!("soundcraft-ui-stem-bounce-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&path).expect("isolated stem destination");
            Self(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }

        fn contents(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .expect("read stem folder")
                .map(|entry| entry.expect("directory entry").file_name().to_string_lossy().into_owned())
                .collect()
        }
    }

    impl Drop for TestFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app() -> SoundApp {
        let mut engine = soundcraft_engine::Engine::default();
        engine.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Guitar"));
        engine.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        engine.mark_clean();
        SoundApp::new(engine, None, crate::Services::default())
    }

    #[test]
    fn stem_export_advances_in_frame_slices_and_journals_once_without_undo_or_dirty() {
        let folder = TestFolder::new();
        let mut app = app();
        let undo_before = app.engine.undo_history().len();
        start_stems(&mut app, &folder.path(), Range::new(0, 4_800), &EncodeOptions::default(), false);
        assert!(app.dialogs.stem_bounce.is_some());
        assert!(app.engine.journal.is_empty(), "Start only allocates a job; publication has not completed");
        let ctx = egui::Context::default();
        let mut slices = 0;
        while app.dialogs.stem_bounce.is_some() {
            slices += 1;
            assert!(slices < 100, "frame-budgeted stem export never finished");
            step(&mut app, &ctx);
        }
        assert_eq!(app.engine.journal.len(), 1);
        assert_eq!(app.engine.journal[0].0, "file.bounce_stems");
        assert_eq!(app.engine.journal[0].1["dir"], folder.path());
        assert_eq!(app.engine.journal[0].1["start"], 0);
        assert_eq!(app.engine.journal[0].1["end"], 4_800);
        assert_eq!(folder.contents().len(), 2, "two completed stems remain; no staging sidecars");
        assert!(!app.engine.is_dirty());
        assert_eq!(app.engine.undo_history().len(), undo_before, "export cannot create an undo step");
        step(&mut app, &ctx);
        assert_eq!(app.engine.journal.len(), 1, "additional paint/logic cannot journal again");
    }

    #[test]
    fn session_replacement_and_concurrent_bounce_are_refused_during_stems() {
        let folder = TestFolder::new();
        let mut app = app();
        let previous = app.engine.session().to_json();
        start_stems(&mut app, &folder.path(), Range::new(0, 4_800), &EncodeOptions::default(), false);
        let error = app.run("session.new", json!({"name": "New Session"})).expect_err("cannot replace session during stem export");
        assert!(error.contains("Finish or cancel"), "{error}");
        assert_eq!(app.engine.session().to_json(), previous);

        start(&mut app, &json!({"path": folder.0.join("other.wav").to_string_lossy()}));
        assert!(app.bounce.is_none(), "a mix bounce cannot start while stems own the render budget");
        assert!(app.dialogs.stem_bounce.is_some());
        app.dialogs.stem_bounce = None;
        assert!(folder.contents().is_empty(), "cancel cleans the partially reserved stem batch");
        assert!(app.engine.journal.is_empty());
    }

    #[test]
    fn recording_refuses_stem_start_before_allocating_or_journaling() {
        let folder = TestFolder::new();
        let mut app = app();
        app.engine.transport.recording = true;
        start_stems(&mut app, &folder.path(), Range::new(0, 4_800), &EncodeOptions::default(), false);
        assert!(app.dialogs.stem_bounce.is_none());
        assert!(folder.contents().is_empty());
        assert!(app.engine.journal.is_empty());
        assert!(app.ui.status.contains("Stop recording"));
    }
}
