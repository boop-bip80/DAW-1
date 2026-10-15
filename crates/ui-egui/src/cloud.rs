//! SoundCraft's half of Open and Save against cloud storage.
//!
//! The pages themselves live in `craft_ui::cloud`. Every Craft app offers the
//! same ones. What is here is what only SoundCraft can answer. A document is a
//! session bundle. These extensions and content types are what that means.
//! Bytes become a session through [`soundcraft_engine::io::open_session_bytes`]
//! and go back through [`soundcraft_engine::io::session_bundle`].
//!
//! SoundCraft holds no provider credential and speaks no provider protocol.
//! Every call goes to the CollabCraft service. That service is the single place
//! a provider is configured and permissions are decided. Replacing one provider
//! with another changes nothing here.
//!
//! Nothing in this file opens a socket. The transport half is a [`CloudBackend`]
//! the host application supplies. See `apps/soundcraft/src/cloud.rs` for the
//! desktop build and `apps/soundcraft-web/src/cloud.rs` for the browser build.
//! Every CollabCraft call blocks on the desktop (worker thread) and is async in
//! the browser. The answer arrives on a channel that [`pump`] drains on a later
//! frame.
//!
//! ## Large-file constraints
//!
//! A SoundCraft session bundle (`.scraftz`) packs the `.scraft` session file
//! **and every audio source the session references** as 32-bit float WAV into a
//! zip archive. A session with dozens of recorded tracks and hours of audio can
//! easily reach tens of gigabytes. Cloud upload is therefore unsuitable for
//! large production sessions.
//!
//! - Only `.scraftz` bundles are written. A bare `.scraft` (JSON only) would
//!   arrive on the other machine with all audio missing and no path to recover
//!   it. Refusing to write it is the honest answer.
//! - The bundle is assembled entirely in memory before upload. Sessions whose
//!   audio does not fit in RAM cannot be bundled at all.
//! - [`BUNDLE_SIZE_WARNING_BYTES`] is the soft threshold at which the
//!   interface logs a warning before upload. There is no hard limit enforced
//!   here; the service and the provider impose their own.
//! - Audio sources marked `unsaved` (recorded but not yet flushed to disk)
//!   are included in the bundle exactly as every other source is.
//! - After a cloud open the session's audio lives in memory. A subsequent
//!   Save or Save As to a local path writes the audio out to `Audio Files/`
//!   beside the session file on disk.

use std::sync::Arc;

// Re-exported so a host application can name these types without depending on
// the crates they come from. A test needs them for the same reason.
pub use craft_cloud::{Capability, Entry, EntryKind};
pub use craft_ui::cloud::{
    Account, Busy, CloudApp, CloudBackend, CloudFiles, Conflict, CopyFormat, Destination, DocumentKind, Event, Fault, Folder, Job, Link, Origin,
    Post, Source, UNAVAILABLE, available, browse, can_converse, conflict_modal, destination_name, has_target, link, load_shared, open_entry,
    open_page, origin, parent_of, prefers_cloud, progress, pump, reload_active, save, save_as, save_page, share_button, shares, sign_in, sign_out,
    start, who, work_together,
};
use egui::{Color32, Painter, Rect, Ui};
use serde_json::Value as Json;
use soundcraft_engine::io::{BUNDLE_EXTENSION, open_session_bytes, session_bundle};
use soundcraft_model::Session;

use crate::SoundApp;
use crate::theme::{self, Tokens};

/// Session formats SoundCraft opens from the cloud.
///
/// Only bundles are listed. A bare `.scraft` is excluded because it would
/// arrive without its audio; opening it would leave the session unrecoverable.
const OPENS: [&str; 1] = [BUNDLE_EXTENSION];

/// Content types SoundCraft opens.
///
/// CollabCraft typically reports a bundle as `application/zip` (the correct
/// MIME for a ZIP archive). Bare JSON is excluded for the same reason as above.
const MIMES: [&str; 1] = ["application/zip"];

/// What a SoundCraft session is, as a cloud file list needs to know.
pub const SESSION: DocumentKind = DocumentKind { extensions: &OPENS, mimes: &MIMES, noun: "session", default_extension: BUNDLE_EXTENSION };

/// The only format a cloud save writes: the session bundle.
///
/// Uploading a bare `.scraft` would hand the user a session whose audio is
/// permanently gone. One format is all there can be.
const SAVES_AS: &str = BUNDLE_EXTENSION;

/// Sessions larger than this (uncompressed audio estimate) get a warning log
/// before upload begins. The check is approximate: exact bundle size is only
/// known after `session_bundle` assembles it, which may take many seconds.
///
/// Value: 2 GiB. Large by typical cloud limits; small by professional-DAW
/// standards. A 30-track session of 10 minutes at 96 kHz is roughly 1.4 GiB.
pub const BUNDLE_SIZE_WARNING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Share menu copy formats. SoundCraft produces only bundles; there is no
/// "Save a Copy as PDF" equivalent here.
const COPIES: [(&str, &str); 0] = [];

/// The uid used for SoundCraft's single session in cloud state maps.
///
/// SoundCraft holds exactly one session at a time, like WordCraft holds one
/// document. A constant uid satisfies `CloudApp`'s requirements without
/// needing a per-session identifier in the model.
const SESSION_UID: u64 = 1;

/// The session snapshot a save in flight captured.
///
/// Only SoundCraft knows what a version of its session is. A save that succeeds
/// clears the dirty flag for exactly the snapshot that reached the server. An
/// edit made while the bytes were on the wire stays unsaved.
pub type Sent = Option<Arc<Session>>;

/// Whether SoundCraft can open this entry.
pub fn opens(entry: &Entry) -> bool {
    craft_ui::cloud::opens(entry, SESSION)
}

/// A typed name as a session bundle file name. Folder separators are dropped;
/// a missing or unrecognised extension becomes `.scraftz`.
pub fn session_name(typed: &str) -> String {
    craft_ui::cloud::document_name(typed, SESSION)
}

/// The whole of the cloud state, for the control channel and for tests.
pub fn status(app: &SoundApp) -> Json {
    craft_ui::cloud::report(app)
}

/// The Share menu's copy formats — empty for SoundCraft (bundles only).
pub fn copies() -> Vec<CopyFormat> {
    COPIES.iter().map(|(id, label)| CopyFormat::new(*id, *label)).collect()
}

/// The host part of the service address, for the account menu's first line.
///
/// Read from the environment and compiled in nowhere. The service is
/// deployment-specific. Somebody with an account on two deployments needs to
/// know which one a session is about to land in.
fn service_host() -> String {
    ["SOUNDCRAFT_CLOUD_URL", "CRAFT_CLOUD_URL", "COLLABCRAFT_URL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .find(|value| !value.is_empty())
        .map(|value| value.trim_start_matches("https://").trim_start_matches("http://").to_owned())
        .unwrap_or_default()
}

/// The format a cloud save should write.
///
/// Only the bundle format is supported. A destination that names a non-bundle
/// file is refused with an explanation the user can act on.
fn writes_as(into: Destination<'_>) -> Result<&'static str, String> {
    let ext = into.extension();
    let asked = if ext.is_empty() {
        match into.mime.map(|m| m.split(';').next().unwrap_or(m).trim()) {
            Some("application/zip") | Some("application/x-zip-compressed") => BUNDLE_EXTENSION.to_owned(),
            _ => BUNDLE_EXTENSION.to_owned(),
        }
    } else {
        ext
    };
    if asked == BUNDLE_EXTENSION {
        Ok(SAVES_AS)
    } else {
        Err(format!(
            "SoundCraft cannot write a .{asked} to the cloud. \
             Only .{BUNDLE_EXTENSION} bundles are supported. \
             A bare .scraft file would arrive without its audio."
        ))
    }
}

/// Approximate uncompressed audio size of the session (bytes).
///
/// Used for the pre-upload size warning. The estimate counts in-memory sources
/// only; missing-file sources contribute zero. It is an approximation, not a
/// guarantee about the final bundle size.
fn estimated_bundle_bytes(s: &Session) -> u64 {
    s.sources.iter().fold(0u64, |acc, src| {
        // 32-bit float × channels × frames (the bundle writes f32 PCM WAV)
        let bytes = u64::from(src.channels).saturating_mul(src.frames).saturating_mul(4);
        acc.saturating_add(bytes)
    })
}

impl CloudApp for SoundApp {
    fn cloud(&self) -> &CloudFiles {
        &self.cloud
    }

    fn cloud_mut(&mut self) -> &mut CloudFiles {
        &mut self.cloud
    }

    fn kind(&self) -> DocumentKind {
        SESSION
    }

    /// SoundCraft holds exactly one session at all times: the uid is always 1.
    fn active(&self) -> Option<u64> {
        Some(SESSION_UID)
    }

    fn title(&self, uid: u64) -> String {
        if uid == SESSION_UID { self.engine.session().name.clone() } else { String::new() }
    }

    fn open_bytes(&mut self, name: &str, bytes: Vec<u8>) -> Result<Option<u64>, String> {
        // Stop playback before replacing the session.
        self.engine.transport_requests.push(soundcraft_engine::TransportRequest::ResetSession);
        let missing = open_session_bytes(&mut self.engine, name, &bytes).map_err(|e| e.to_string())?;
        if !missing.is_empty() {
            self.ui.status = format!("Opened {name} ({} audio file{} missing in bundle)", missing.len(), if missing.len() == 1 { "" } else { "s" });
        }
        self.ui.show_cloud = false;
        Ok(Some(SESSION_UID))
    }

    fn encode(&mut self, uid: u64, into: Destination<'_>) -> Result<Vec<u8>, String> {
        if uid != SESSION_UID {
            return Err("that session is no longer open".to_owned());
        }
        writes_as(into)?;
        let est = estimated_bundle_bytes(self.engine.session());
        if est > BUNDLE_SIZE_WARNING_BYTES {
            log::warn!(
                "cloud save: session estimated at {:.1} GiB; \
                 upload may be slow or rejected by the service",
                est as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        }
        let snapshot = self.engine.session().clone();
        let (bytes, _fresh) = session_bundle(&snapshot).map_err(|e| e.to_string())?;
        self.cloud_sent = Some(Arc::new(snapshot));
        Ok(bytes)
    }

    fn close(&mut self, _uid: u64) {
        // SoundCraft holds one session. A reload replaces it through open_bytes.
    }

    fn mark_saved(&mut self, uid: u64) {
        let Some(_snapshot) = self.cloud_sent.take() else { return };
        if uid == SESSION_UID {
            // Mark the session clean so File > Save does not re-upload it.
            self.engine.mark_clean();
        }
    }

    fn drop_pending(&mut self, uid: u64) {
        if uid == SESSION_UID {
            self.cloud_sent = None;
        }
    }

    fn sharing(&mut self) -> &mut craft_ui::share::ShareDialog {
        &mut self.share
    }

    fn presence(&mut self) -> &mut craft_ui::presence::PresenceBar {
        &mut self.presence
    }

    fn paint_icon(&self, painter: &Painter, rect: Rect, kind: EntryKind, colour: Color32) {
        let name = if kind == EntryKind::Folder { "folder" } else { "wave" };
        crate::icons::draw(painter, rect, name, colour);
    }

    fn open_url(&self, url: &str) {
        if let Some(f) = &self.services.open_url {
            f(url);
        }
    }

    fn say(&mut self, text: String) {
        self.ui.status = text;
    }

    fn toast(&mut self, text: String) {
        self.ui.status = text;
    }

    fn go_to_save_as(&mut self) {
        self.ui.show_cloud = true;
        self.ui.cloud_page = CloudPage::SaveAs;
    }

    fn app_name(&self) -> &str {
        "SoundCraft"
    }

    fn service(&self) -> String {
        service_host()
    }

    fn save_note(&self) -> Option<String> {
        Some(
            "Sessions are saved as SoundCraft bundles (.scraftz) that include \
             their audio. Large sessions may be slow to upload."
                .to_owned(),
        )
    }

    fn style(&self, _ui: &Ui) -> craft_ui::cloud::CloudStyle {
        let t = Tokens::current();
        craft_ui::cloud::CloudStyle {
            surface: t.panel_bg,
            border: t.border,
            text: t.text,
            dim: t.text_dim,
            hover: t.button_hi,
            soft: t.accent.gamma_multiply(0.14),
            family: egui::FontFamily::Proportional,
            bold: theme::bold(13.0).family,
        }
    }
}

/// Which cloud page to show when the cloud overlay is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum CloudPage {
    #[default]
    Open,
    SaveAs,
}

/// Draw the cloud overlay when it is active.
///
/// Rendered as a centred modal area so it never overlaps the transport,
/// timeline, or mixer panels — only the menu bar stays visible above it.
/// Width and height are capped so the overlay is readable on small screens.
pub fn show_cloud_overlay(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.ui.show_cloud {
        return;
    }

    let screen = ctx.content_rect();
    let w = (screen.width() - 32.0).min(920.0).max(480.0);
    let h = (screen.height() - 80.0).min(740.0).max(320.0);
    let top = screen.min.y + 48.0; // clear the menu bar

    // Dim the content behind the overlay.
    ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("cloud_dim"))).rect_filled(
        screen,
        0.0,
        egui::Color32::from_black_alpha(160),
    );

    egui::Area::new(egui::Id::new("cloud_overlay"))
        .fixed_pos(egui::pos2(screen.min.x + (screen.width() - w) * 0.5, top))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let t = Tokens::current();
            let frame = egui::Frame::window(ui.style()).fill(t.panel_bg).stroke(egui::Stroke::new(1.0, t.border)).inner_margin(egui::Margin::same(0));
            frame.show(ui, |ui| {
                ui.set_min_size(egui::vec2(w, h));
                ui.set_max_size(egui::vec2(w, h));
                match app.ui.cloud_page {
                    CloudPage::Open => open_page(app, ui),
                    CloudPage::SaveAs => {
                        let _ = save_page(app, ui);
                    }
                }
                progress(app, ui);
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    app.ui.show_cloud = false;
                }
            });
        });

    conflict_modal(app, ctx);
}

#[cfg(test)]
#[path = "cloud_tests.rs"]
mod tests;
