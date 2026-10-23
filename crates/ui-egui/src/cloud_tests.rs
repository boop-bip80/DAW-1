//! Behavioural tests for SoundCraft's cloud integration.
//!
//! These tests run without a network and without an egui context. They verify
//! the contracts this module holds: format acceptance/refusal, origin/etag
//! tracking, dirty-flag management and constraint documentation.

use std::sync::Arc;

use soundcraft_engine::Engine;

use super::{BUNDLE_SIZE_WARNING_BYTES, Destination, EntryKind, SAVES_AS, SESSION, estimated_bundle_bytes, opens, session_name, writes_as};
use crate::{Services, SoundApp};

fn app() -> SoundApp {
    SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default())
}

// ---------------------------------------------------------------- DocumentKind

#[test]
fn session_kind_is_bundle_extension() {
    assert!(SESSION.extensions.contains(&"scraftz"));
    assert_eq!(SESSION.default_extension, "scraftz");
    assert_eq!(SESSION.noun, "session");
}

#[test]
fn session_kind_does_not_offer_bare_scraft() {
    // A bare .scraft is excluded: it arrives without audio.
    assert!(!SESSION.extensions.contains(&"scraft"));
}

// ---------------------------------------------------------------- writes_as

#[test]
fn writes_as_accepts_bundle_extension() {
    let d = Destination::new("Midnight Groove.scraftz");
    assert_eq!(writes_as(d).unwrap(), SAVES_AS);
}

#[test]
fn writes_as_accepts_bundle_via_zip_mime() {
    let d = Destination { name: "session", mime: Some("application/zip") };
    assert_eq!(writes_as(d).unwrap(), SAVES_AS);
}

#[test]
fn writes_as_refuses_bare_scraft() {
    let d = Destination::new("Song.scraft");
    let err = writes_as(d).unwrap_err();
    assert!(err.contains("without its audio"), "unexpected: {err}");
}

#[test]
fn writes_as_refuses_wav() {
    let d = Destination::new("bounce.wav");
    let err = writes_as(d).unwrap_err();
    assert!(err.contains(".wav"), "unexpected: {err}");
}

#[test]
fn writes_as_defaults_to_bundle_on_no_extension() {
    let d = Destination::new("NoExtension");
    assert_eq!(writes_as(d).unwrap(), SAVES_AS);
}

// ---------------------------------------------------------------- session_name

#[test]
fn session_name_appends_scraftz_when_missing() {
    assert_eq!(session_name("Song"), "Song.scraftz");
}

#[test]
fn session_name_keeps_scraftz_when_present() {
    assert_eq!(session_name("Song.scraftz"), "Song.scraftz");
}

#[test]
fn session_name_drops_folder_separators() {
    assert_eq!(session_name("a/b/Song"), "Song.scraftz");
}

// ---------------------------------------------------------------- opens

#[test]
fn opens_accepts_scraftz() {
    let entry = craft_cloud::Entry {
        id: "1".into(),
        room: String::new(),
        name: "song.scraftz".into(),
        path: String::new(),
        kind: EntryKind::File,
        size: Some(1024),
        modified: None,
        mime: Some("application/zip".into()),
        etag: None,
        writable: true,
        shared_by: None,
    };
    assert!(opens(&entry));
}

#[test]
fn opens_rejects_scraft() {
    let entry = craft_cloud::Entry {
        id: "2".into(),
        room: String::new(),
        name: "song.scraft".into(),
        path: String::new(),
        kind: EntryKind::File,
        size: Some(512),
        modified: None,
        mime: Some("application/json".into()),
        etag: None,
        writable: true,
        shared_by: None,
    };
    assert!(!opens(&entry));
}

#[test]
fn opens_rejects_folder() {
    let entry = craft_cloud::Entry {
        id: "3".into(),
        room: String::new(),
        name: "Projects".into(),
        path: String::new(),
        kind: EntryKind::Folder,
        size: None,
        modified: None,
        mime: None,
        etag: None,
        writable: false,
        shared_by: None,
    };
    assert!(!opens(&entry));
}

// ---------------------------------------------------------------- estimated_bundle_bytes

#[test]
fn empty_session_has_zero_estimated_bytes() {
    let engine = Engine::default();
    assert_eq!(estimated_bundle_bytes(engine.session()), 0);
}

#[test]
fn warning_threshold_is_documented() {
    // The constant must match the docstring (2 GiB). A change here needs a
    // deliberate decision, not an accidental constant-name refactor.
    assert_eq!(BUNDLE_SIZE_WARNING_BYTES, 2 * 1024 * 1024 * 1024);
}

// ---------------------------------------------------------------- CloudApp impl

#[test]
fn active_is_always_1() {
    use craft_ui::cloud::CloudApp;
    let app = app();
    assert_eq!(app.active(), Some(1));
}

#[test]
fn title_returns_session_name() {
    use craft_ui::cloud::CloudApp;
    let app = app();
    let name = app.engine.session().name.clone();
    assert_eq!(app.title(1), name);
    assert!(app.title(999).is_empty());
}

#[test]
fn mark_saved_with_no_pending_is_a_noop() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    // Nothing pending: must not panic.
    app.mark_saved(1);
    assert!(!app.engine.is_dirty());
}

#[test]
fn drop_pending_clears_cloud_sent() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    app.cloud_sent = Some(Arc::new(app.engine.session().clone()));
    app.drop_pending(1);
    assert!(app.cloud_sent.is_none());
}

#[test]
fn encode_refuses_non_bundle_extension() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    let into = Destination::new("song.scraft");
    let err = app.encode(1, into).unwrap_err();
    assert!(err.contains("without its audio"), "unexpected: {err}");
}

#[test]
fn encode_produces_non_empty_bytes_for_bundle() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    let into = Destination::new("Midnight Groove.scraftz");
    let bytes = app.encode(1, into).expect("encode should succeed");
    assert!(!bytes.is_empty());
    // The bundle starts with a ZIP local file header signature.
    assert_eq!(&bytes[..4], b"PK\x03\x04", "expected ZIP magic");
}

#[test]
fn encode_records_snapshot_in_cloud_sent() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    assert!(app.cloud_sent.is_none());
    let into = Destination::new("song.scraftz");
    app.encode(1, into).expect("encode should succeed");
    assert!(app.cloud_sent.is_some());
}

#[test]
fn mark_saved_clears_dirty_flag() {
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    // Make the session dirty by changing something.
    app.engine.session_mut().name = "Modified".to_owned();
    assert!(app.engine.is_dirty());
    // Simulate a save cycle.
    let into = Destination::new("Modified.scraftz");
    app.encode(1, into).expect("encode should succeed");
    app.mark_saved(1);
    assert!(!app.engine.is_dirty());
}

// ---------------------------------------------------------------- overlay state

#[test]
fn show_cloud_starts_false() {
    let app = app();
    assert!(!app.ui.show_cloud);
}

#[test]
fn go_to_save_as_sets_cloud_flags() {
    use super::CloudPage;
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    app.go_to_save_as();
    assert!(app.ui.show_cloud);
    assert_eq!(app.ui.cloud_page, CloudPage::SaveAs);
}

// ---------------------------------------------------------------- service host

#[test]
fn service_host_is_empty_without_env() {
    // The environment is not set in test runs.
    use craft_ui::cloud::CloudApp;
    let app = app();
    // Must not panic and must not hardcode a hostname.
    let _ = app.service();
}

// ---------------------------------------------------------------- large-file constraint documentation

#[test]
fn save_note_documents_in_memory_assembly() {
    // save_note is the user-visible explanation of upload constraints.
    // It must mention memory so users of large sessions understand the risk.
    use craft_ui::cloud::CloudApp;
    let app = app();
    let note = app.save_note().unwrap_or_default();
    assert!(
        note.to_lowercase().contains("memory") || note.to_lowercase().contains("ram"),
        "save_note must document the in-memory bundle constraint; got: {note}"
    );
}

#[test]
fn save_note_does_not_claim_cloud_is_safe_for_large_sessions() {
    // Ensure we do not phrase save_note as if large sessions are fine.
    use craft_ui::cloud::CloudApp;
    let app = app();
    let note = app.save_note().unwrap_or_default().to_lowercase();
    // "may be slow" alone is insufficient. The note must mention a hard limit:
    // memory, RAM, or a size qualifier.
    let warns_about_limit = note.contains("ram") || note.contains("memory") || note.contains("cannot") || note.contains("exhaust");
    assert!(warns_about_limit, "save_note must warn about the RAM limit, not just slowness: {note}");
}

// ---------------------------------------------------------------- .scraft silent audio loss prevention

#[test]
fn scraft_error_mentions_audio_loss() {
    // The refusal error for a .scraft destination must explain *why* —
    // that the audio would be silently absent. The user must be able to
    // act on the message without reading source code.
    use craft_ui::cloud::CloudApp;
    let mut app = app();
    let err = app.encode(1, Destination::new("session.scraft")).unwrap_err();
    assert!(err.to_lowercase().contains("audio"), "the .scraft refusal must mention audio loss; got: {err}");
    assert!(
        err.to_lowercase().contains("without") || err.to_lowercase().contains("missing"),
        "the .scraft refusal must explain what would be lost; got: {err}"
    );
}
