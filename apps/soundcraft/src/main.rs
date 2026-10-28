//! SoundCraft desktop app.
//!
//! Usage: `soundcraft [--demo] [--control <port>] [--no-audio] [session.scraft | audio files…]`; `soundcraft --help` lists the flags and `SOUNDCRAFT_CONTROL_PORT`.
//!
//! `--control <port>` (or `SOUNDCRAFT_CONTROL_PORT`) starts a localhost JSON-lines control server
//! (`0` picks a free port and prints it): `{"id":1,"method":"ui.inspect","params":{}}` →
//! `{"id":1,"ok":true,"result":…}`. See `soundcraft_ui_egui::control` for the methods.
//!
//! `log` records go to standard error and `<settings dir>/logs/soundcraft.log` (`RUST_LOG` sets the
//! levels); see [`logging`].
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod config;
mod control_server;
mod env;
#[cfg(any(target_os = "windows", test))]
mod graphics;
mod launch_args;
mod logging;
#[cfg(target_os = "macos")]
mod native_menu;
mod switch;

use env::Env;
use soundcraft_audio_io::FileFormat;
use soundcraft_engine::Engine;
use soundcraft_ui_egui::{Services, SoundApp, UiState, dialogs};
use std::sync::Arc;

struct App {
    app: SoundApp,
    logger: Option<&'static logging::AppLogger>,
    /// The process environment the app started with: preferences switch, system fonts, folders.
    env: Env,
    #[cfg(target_os = "macos")]
    menu: Option<native_menu::NativeMenu>,
    /// The engine preferences revision last written to `preferences.json`; `None` while a
    /// migration that changed the preferences still has to be written by the frame loop.
    saved_prefs_revision: Option<u64>,
    /// When the frame loop last tried to save a preference change (egui seconds); a failed save
    /// is retried at most once a second instead of on every frame.
    prefs_save_attempt: Option<f64>,
    /// The last save failure: logged once, and again only when it changes.
    prefs_save_error: Option<String>,
}

/// Sends the title only when it changes, remembered here as X11 and Wayland can't read it back.
fn set_title(ctx: &egui::Context, title: String) {
    let id = egui::Id::unique("window_title");
    if ctx.data(|d| d.get_temp::<String>(id)).as_deref() != Some(title.as_str()) {
        ctx.data_mut(|d| d.insert_temp(id, title.clone()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
    }
}

impl eframe::App for App {
    /// Per-frame app logic: report audio-thread logs, run the shared app logic with the macOS menu driven around it,
    /// then handle dropped paths, quit/close requests, window geometry, recent-list persistence and the window title.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // What the realtime audio thread logged is written here, off that thread.
        if let Some(logger) = self.logger {
            logger.report_audio_thread();
        }
        #[cfg(target_os = "macos")]
        if let Some(menu) = &mut self.menu {
            menu.process(&mut self.app);
        }
        self.app.guarded(|app| app.logic(ctx));
        #[cfg(target_os = "macos")]
        if let Some(menu) = &mut self.menu {
            menu.sync(&self.app);
        }
        // Files dropped on the window: audio on an instrument track or a Sampler window loads
        // the sampler; otherwise sessions open and audio/MIDI import.
        for f in self.app.take_dropped_files() {
            if !f.path().as_os_str().is_empty() {
                dialogs::open_path(&mut self.app, f.path());
            }
        }
        if self.app.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if ctx.input(|i| i.viewport().close_requested()) && dialogs::ask_to_save(&mut self.app, "app.quit", &serde_json::json!({})) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        soundcraft_ui_egui::fit_window_to_screen(ctx);
        let (inner, maximized) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().maximized.unwrap_or(false)));
        self.app.ui.window_maximized = maximized;
        if let Some(r) = inner.filter(|_| !maximized) {
            self.app.ui.window_size = (r.size() * ctx.zoom_factor()).round().into();
        }
        self.app.ui.ui_scale = self.app.extra.ui_scale;
        // Saved now rather than at exit: the recent list matters most after a crash.
        if self.app.take_recent_changed() {
            save_prefs(&self.env, &self.app.ui);
        }
        // Preferences change rarely (the Appearance window, agents): save them when they do, and
        // only record the revision once the file actually kept them. A failure (read-only settings
        // folder, full disk) is retried at most once a second — not on every frame, which would
        // write a temp file and log a warning sixty times a second.
        let prefs_revision = self.app.engine.prefs_revision();
        if self.saved_prefs_revision != Some(prefs_revision) {
            let now = ctx.input(|i| i.time);
            if self.prefs_save_attempt.is_none_or(|at| now - at >= 1.0) {
                self.prefs_save_attempt = Some(now);
                match save_engine_prefs(&self.env, &self.app.engine) {
                    Ok(()) => {
                        self.saved_prefs_revision = Some(prefs_revision);
                        self.prefs_save_error = None;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if self.prefs_save_error.as_deref() != Some(message.as_str()) {
                            log::warn!("preferences: {message}");
                        }
                        self.prefs_save_error = Some(message);
                        ctx.request_repaint_after(std::time::Duration::from_secs(1));
                    }
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
        }
        // With the system menu bar there is no egui bar showing the window and dirty state, so the
        // title carries them; elsewhere the egui menu bar already does.
        let title = if self.app.native_menu_bar {
            format!(
                "{}{} — SoundCraft — {}",
                self.app.engine.session().name,
                if self.app.engine.is_dirty() { " *" } else { "" },
                if self.app.ui.window == soundcraft_ui_egui::MainWindow::Edit { "Edit" } else { "Mix" }
            )
        } else {
            format!("{} — SoundCraft", self.app.engine.session().name)
        };
        set_title(ctx, title);
    }
    /// Forward raw input to the shared app hook so scripted gestures and host-key protection behave like the desktop
    /// shell.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.guarded(|app| app.ui(ui));
    }
    fn on_exit(&mut self) {
        self.app.close_autosave();
        save_prefs(&self.env, &self.app.ui);
        // Only write when a preference actually changed: `load_prefs_json` is lenient, so an
        // unconditional save would replace a hand-edited (or damaged) file with defaults, and an
        // older build would drop the keys a newer one wrote.
        if self.saved_prefs_revision != Some(self.app.engine.prefs_revision())
            && let Err(error) = save_engine_prefs(&self.env, &self.app.engine)
        {
            log::warn!("preferences: {error}");
        }
        if let Some(logger) = self.logger {
            logger.report_audio_thread();
        }
        // The system Quit (⌘Q) ends the process from inside the event loop, so `run_native` never
        // returns: release the player's plugin instances and shut plugin hosting down here.
        self.app.player = None;
        soundcraft_engine::shutdown_plugin_hosts();
    }
}

fn prefs_path(env: &Env) -> Option<std::path::PathBuf> {
    env.config_dir.as_ref().map(|b| b.join("ui.json"))
}

/// Runs without preferences (`SOUNDCRAFT_NO_PREFS`, agents' test runs) neither read nor write them.
fn load_prefs(env: &Env) -> Option<UiState> {
    if !env.prefs_enabled() {
        return None;
    }
    let path = prefs_path(env)?;
    match config::load_ui(&path) {
        Ok(state) => state,
        Err(error) => {
            log::warn!("{error}");
            None
        }
    }
}

fn save_prefs(env: &Env, ui: &UiState) {
    if !env.prefs_enabled() {
        return;
    }
    if let Some(path) = prefs_path(env)
        && let Err(error) = config::save_ui(&path, ui)
    {
        log::warn!("{error}");
    }
}

/// The engine-owned preference document (`interface.language`, …) beside `ui.json`.
fn preferences_path(env: &Env) -> Option<std::path::PathBuf> {
    env.config_dir.as_ref().map(|b| b.join("preferences.json"))
}

/// Load the preference document leniently (a damaged value keeps its default, with a log line)
/// and, on the first run with it, adopt the language older builds saved in `ui.json`.
///
/// Returns true when that migration changed the preferences but could not be written yet: the
/// frame loop must retry the save instead of recording the revision as already persisted.
fn load_engine_prefs(env: &Env, engine: &mut Engine) -> bool {
    if !env.prefs_enabled() {
        return false;
    }
    let Some(path) = preferences_path(env) else { return false };
    match config::load_preferences(&path) {
        Ok(Some(bytes)) => {
            for warning in engine.load_prefs_json(&bytes) {
                log::warn!("preferences: {warning}");
            }
            false
        }
        Ok(None) => {
            let mut pending = false;
            if let Some(ui_path) = prefs_path(env) {
                match config::migrate_language(engine, &ui_path, &path) {
                    Ok(true) => log::info!("preferences: adopted the interface language saved in ui.json"),
                    Ok(false) => {}
                    Err(error) => {
                        log::warn!("preferences: {error}");
                        pending = true;
                    }
                }
            }
            pending
        }
        Err(error) => {
            log::warn!("preferences: {error}");
            false
        }
    }
}

/// Persist the engine preferences. Disabled or absent paths are a successful no-op; a failed
/// write is reported so the frame loop can retry it.
fn save_engine_prefs(env: &Env, engine: &Engine) -> std::io::Result<()> {
    if !env.prefs_enabled() {
        return Ok(());
    }
    let Some(path) = preferences_path(env) else { return Ok(()) };
    let json = engine.prefs_json().map_err(|error| std::io::Error::other(error.to_string()))?;
    config::save_preferences(&path, &json).map_err(std::io::Error::other)
}

fn services() -> Services {
    Services {
        pick_open: Some(Box::new(|purpose: &str, _exts: &[&str]| {
            let d = rfd::FileDialog::new();
            let d = match purpose {
                "session.open" => d.add_filter("SoundCraft Session", &["scraft"]),
                "file.import_midi" => d.add_filter("MIDI", &["mid", "midi", "smf"]),
                _ => d.add_filter("Audio", &FileFormat::readable_extensions()),
            };
            d.pick_file()
        })),
        pick_save: Some(Box::new(|purpose: &str, path: &std::path::Path| {
            let mut d = match purpose {
                "file.bounce_mix" => rfd::FileDialog::new().add_filter("Audio", &FileFormat::writable_extensions()),
                _ => rfd::FileDialog::new().add_filter("SoundCraft Session", &["scraft"]),
            };
            if let Some(dir) = path.parent().filter(|dir| dir.is_dir()) {
                d = d.set_directory(dir);
            }
            if let Some(n) = path.file_name().and_then(std::ffi::OsStr::to_str) {
                d = d.set_file_name(n);
            }
            d.save_file()
        })),
        pick_folder: Some(Box::new(|start: &std::path::Path| rfd::FileDialog::new().set_directory(start).pick_folder())),
    }
}

/// The window, Dock, taskbar and Alt-Tab icon. macOS gets Apple's icon grid (transparent margin);
/// elsewhere the full tile. Regenerate with `packaging/icons.sh`.
fn app_icon() -> Option<egui::IconData> {
    #[cfg(target_os = "macos")]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/soundcraft-macos-512.png");
    #[cfg(not(target_os = "macos"))]
    let png: &[u8] = include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.soundcraft.png");
    eframe::icon_data::from_png_bytes(png).map_err(|e| log::warn!("app icon: {e}")).ok()
}

fn main() -> eframe::Result {
    // Windows: the update helper is a copy of this program started with `--finish-update`.
    #[cfg(windows)]
    if let Some(result) = craft_update::self_update::finish(&craft_update::suite::SOUNDCRAFT, &std::env::args_os().skip(1).collect::<Vec<_>>()) {
        std::process::exit(i32::from(result.is_err()));
    }
    // First, so every start-up record (and the engine's panic hook, installed with the first
    // Engine) is captured; see `logging`.
    let logger = logging::install(env::rust_log().as_deref());
    // Help, version and usage errors print and exit here, before a window or a log file exists.
    let launch = launch_args::parse(launch_args::command(), std::env::args_os().skip(1)).unwrap_or_else(|e| e.exit());
    // After the arguments and the logger: reading resolves the settings folder, which creates it,
    // and a malformed switch is logged.
    let env = Env::read();
    let (demo, no_audio, control_port) = (launch.demo, launch.no_audio, launch.control_port);
    let files = launch.files;
    log::info!("SoundCraft {}", soundcraft_automation::args::long_version());
    // The log file lives in the settings directory; opened after the arguments, so `--version`
    // leaves no file behind. Records logged until now are written to it first. Runs without
    // preferences (agents' test runs) log to standard error only, so they don't rotate away the
    // user's own logs.
    if let Some(logger) = logger {
        match env.config_dir.as_deref() {
            _ if !env.prefs_enabled() => logger.no_file(),
            Some(dir) => match logger.attach_dir(&dir.join(logging::LOG_DIR)) {
                Ok(path) => log::info!("log file {}", path.display()),
                Err(e) => log::warn!("no log file: {e}"),
            },
            None => {
                logger.no_file();
                log::warn!("no log file: no settings directory (profile override is empty or platform configuration is unavailable)");
            }
        }
    }

    let mut engine = if demo { soundcraft_engine::demo::demo_engine() } else { Engine::default() };
    let migration_needs_save = load_engine_prefs(&env, &mut engine);
    let prefs = load_prefs(&env).unwrap_or_default();
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundCraft")
            .with_inner_size(sane_window_size(prefs.window_size))
            .with_maximized(prefs.window_maximized)
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true)
            .with_app_id("ai.storyteller.soundcraft"),
        ..Default::default()
    };
    if let Some(icon) = app_icon() {
        options.viewport = options.viewport.with_icon(icon);
    }
    // Before eframe creates the wgpu instance: default Windows to DirectX 12 with FXC (see graphics.rs).
    #[cfg(target_os = "windows")]
    graphics::configure(&mut options, eframe::wgpu::Backends::from_env(), eframe::wgpu::Dx12Compiler::from_env());
    #[cfg(all(unix, not(target_os = "macos")))]
    // winit has no file drag-and-drop on Wayland (only on X11), so dropping files from the file
    // manager showed a "no" cursor. Run through XWayland when it's there; SOUNDCRAFT_WAYLAND=1
    // keeps the native Wayland backend.
    if env.use_xwayland(std::env::var_os("DISPLAY").as_ref()) {
        options.event_loop_builder = Some(Box::new(|b| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            b.with_x11();
        }));
    }
    // Read the desktop's UI language once, before the first frame (Auto follows it).
    soundcraft_ui_egui::i18n::init_system_locale();
    eframe::run_native(
        "SoundCraft",
        options,
        Box::new(move |cc| {
            let player =
                if no_audio { None } else { Some(soundcraft_playback::Player::with_config(Arc::new(engine.session().clone()), &prefs.audio)) };
            let mut app = SoundApp::new(engine, player, services());
            app.set_startup_update_check(true);
            app.set_system_fonts(env.system_fonts());
            app.autosave_dir = env.config_dir.as_ref().map(|d| d.join("Autosave"));
            app.leftovers = app.autosave_dir.as_deref().map(soundcraft_engine::autosave::leftovers).unwrap_or_default();
            app.preset_dir = env.config_dir.as_ref().map(|d| d.join("Presets"));
            app.extra.ui_scale = prefs.ui_scale;
            app.ui = prefs;
            if let Some(port) = control_port {
                let rx = control_server::start(port, cc.egui_ctx.clone());
                app = app.with_control(rx);
            }
            for f in &files {
                dialogs::open_path(&mut app, f);
            }
            // If the system menu bar cannot be built, keep the egui menu bar rather than fail to start.
            #[cfg(target_os = "macos")]
            let menu = match native_menu::NativeMenu::new(&app, &cc.egui_ctx) {
                Ok(m) => {
                    app.native_menu_bar = true;
                    Some(m)
                }
                Err(e) => {
                    log::warn!("system menu bar unavailable, using the in-window menu bar: {e}");
                    None
                }
            };
            let prefs_revision = app.engine.prefs_revision();
            Ok(Box::new(App {
                saved_prefs_revision: (!migration_needs_save).then_some(prefs_revision),
                prefs_save_attempt: None,
                prefs_save_error: None,
                app,
                logger,
                env,
                #[cfg(target_os = "macos")]
                menu,
            }))
        }),
    )
}

/// A saved window size, or the default when the prefs file holds nonsense (NaN, negative, tiny).
fn sane_window_size([w, h]: [f32; 2]) -> [f32; 2] {
    if w.is_finite() && h.is_finite() && w >= 960.0 && h >= 600.0 { [w.min(16_384.0), h.min(16_384.0)] } else { [1600.0, 1000.0] }
}

#[cfg(test)]
mod tests {
    use super::set_title;

    #[test]
    fn the_title_is_sent_only_when_it_changes() {
        let ctx = egui::Context::default();
        let titles = |title: &str| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| set_title(ui.ctx(), title.to_string()));
            out.textures_delta.clear();
            out.viewport_output.values().flat_map(|v| &v.commands).filter(|c| matches!(c, egui::ViewportCommand::Title(_))).count()
        };
        assert_eq!(titles("Song — SoundCraft"), 1);
        assert_eq!(titles("Song — SoundCraft"), 0);
        assert_eq!(titles("Song * — SoundCraft"), 1);
    }
}
