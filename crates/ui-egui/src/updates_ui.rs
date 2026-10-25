//! Software updates, background check at startup, and startup prompt modal.

use crate::SoundApp;

pub(crate) struct Runtime {
    startup_checked: bool,
    manual_checked: bool,
    pub(crate) manual_window_open: bool,
    pub(crate) status_message: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) prompt: Option<craft_update::StartupPrompt>,
    #[cfg(not(target_arch = "wasm32"))]
    rx: Option<std::sync::mpsc::Receiver<Result<Option<craft_update::Release>, String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    checking: bool,
    #[cfg(not(target_arch = "wasm32"))]
    update_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            startup_checked: false,
            manual_checked: false,
            manual_window_open: false,
            status_message: None,
            #[cfg(not(target_arch = "wasm32"))]
            prompt: None,
            #[cfg(not(target_arch = "wasm32"))]
            rx: None,
            #[cfg(not(target_arch = "wasm32"))]
            checking: false,
            #[cfg(not(target_arch = "wasm32"))]
            update_rx: None,
        }
    }
}

pub(crate) fn check(app: &mut SoundApp) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.updates_rt.manual_checked = true;
        app.updates_rt.manual_window_open = false;
        app.updates_rt.status_message = None;
        start_check(app);
    }
    #[cfg(target_arch = "wasm32")]
    {
        app.updates_rt.manual_window_open = true;
        app.updates_rt.status_message = Some("Update check is not supported in the web browser.".into());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn start_check(app: &mut SoundApp) {
    if app.updates_rt.checking {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.updates_rt.rx = Some(rx);
    app.updates_rt.checking = true;

    std::thread::spawn(move || {
        let http = match craft_update::Http::new("SoundCraft/Update") {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let res = craft_update::check_for_update(&craft_update::suite::SOUNDCRAFT, &http, env!("CARGO_PKG_VERSION"));
        let _ = tx.send(res);
    });
}

pub(crate) fn tick(app: &mut SoundApp, ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !app.updates_rt.startup_checked {
            app.updates_rt.startup_checked = true;
            start_check(app);
        }

        if app.updates_rt.checking {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let mut completed = None;
        if let Some(rx) = &app.updates_rt.rx {
            if let Ok(res) = rx.try_recv() {
                completed = Some(res);
            }
        }

        if let Some(res) = completed {
            app.updates_rt.rx = None;
            app.updates_rt.checking = false;
            match res {
                Ok(Some(release)) => {
                    let mut prompt = craft_update::StartupPrompt::new(&craft_update::suite::SOUNDCRAFT, release, env!("CARGO_PKG_VERSION"));
                    prompt.open = true;
                    app.updates_rt.prompt = Some(prompt);
                    ctx.request_repaint();
                }
                Ok(None) => {
                    if app.updates_rt.manual_checked {
                        app.updates_rt.manual_checked = false;
                        app.updates_rt.status_message =
                            Some(format!("SoundCraft {} is currently the newest version available.", env!("CARGO_PKG_VERSION")));
                        app.updates_rt.manual_window_open = true;
                        ctx.request_repaint();
                    }
                }
                Err(e) => {
                    if app.updates_rt.manual_checked {
                        app.updates_rt.manual_checked = false;
                        app.updates_rt.status_message = Some(format!("Could not check for updates:\n{e}"));
                        app.updates_rt.manual_window_open = true;
                        ctx.request_repaint();
                    }
                }
            }
        }
    }
}

pub(crate) fn show(app: &mut SoundApp, ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut download_url = None;
        if let Some(prompt) = &mut app.updates_rt.prompt {
            if let Some(result) = app.updates_rt.update_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                app.updates_rt.update_rx = None;
                match result {
                    // The app's close path asks about unsaved work; the restart helper relaunches once the process exits.
                    Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => {
                        prompt.status_message = Some(e);
                        prompt.is_installing = false;
                    }
                }
            }
            match prompt.show(ctx) {
                craft_update::StartupPromptResponse::Pending => {}
                craft_update::StartupPromptResponse::UpdateAndRestart => {
                    prompt.is_installing = true;
                    if let Some(install) = craft_update::self_update::current(&craft_update::suite::SOUNDCRAFT) {
                        let mut release = prompt.release.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        app.updates_rt.update_rx = Some(rx);
                        let ctx = ctx.clone();
                        std::thread::spawn(move || {
                            let mut run = || -> Result<(), String> {
                                let http = craft_update::Http::new("SoundCraft/Update").map_err(|e| format!("could not initialize HTTP: {e}"))?;
                                if release.assets.is_empty()
                                    && let Some(full) =
                                        craft_update::check_for_update(&craft_update::suite::SOUNDCRAFT, &http, env!("CARGO_PKG_VERSION"))
                                            .map_err(|e| format!("could not fetch release assets: {e}"))?
                                {
                                    release = full;
                                }
                                let paths = craft_update::Paths::for_app(&craft_update::suite::SOUNDCRAFT)
                                    .map_err(|e| format!("could not find paths: {e}"))?;
                                let platform = craft_update::Platform::current().ok_or("unsupported platform for self update")?;
                                craft_update::self_update::update(
                                    &craft_update::suite::SOUNDCRAFT,
                                    &install,
                                    &release,
                                    platform,
                                    &http,
                                    &paths,
                                    &craft_update::Stop::default(),
                                    &mut |_| {},
                                    &|| Ok(()),
                                    Some(env!("CARGO_PKG_VERSION")),
                                )
                            };
                            let result = run();
                            if let Err(e) = &result {
                                log::warn!("self-update failed: {e}");
                            }
                            let _ = tx.send(result);
                            ctx.request_repaint();
                        });
                    } else {
                        prompt.status_message = Some("Cannot auto-update this installation type. Please download manually.".to_string());
                        prompt.is_installing = false;
                    }
                }
                craft_update::StartupPromptResponse::Download => {
                    download_url = Some(prompt.release.html_url.clone());
                    prompt.open = false;
                }
                craft_update::StartupPromptResponse::Later => {
                    prompt.open = false;
                }
            }
            if !prompt.open {
                app.updates_rt.prompt = None;
            }
        }

        if let Some(url) = download_url {
            app.updates_rt.prompt = None;
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
    }

    if app.updates_rt.manual_window_open {
        let mut open = true;
        let mut close = false;
        egui::Window::new("Check for Updates")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if let Some(msg) = &app.updates_rt.status_message {
                    ui.label(msg);
                } else {
                    ui.label("Checking for updates…");
                }
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        if !open || close {
            app.updates_rt.manual_window_open = false;
            app.updates_rt.status_message = None;
        }
    }
}
