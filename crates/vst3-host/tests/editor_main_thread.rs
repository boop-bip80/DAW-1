//! VST3 editors on the process main thread (`harness = false`: the standard harness never runs
//! tests there). Opening a real window would flash on screen, so this only checks what the
//! main thread changes without one: the view probe and the parameter round trip through the
//! controller. On macOS one real window opens briefly to check that a hidden editor (as AppKit
//! hides it while the app is inactive) stays open and a closed one doesn't; set
//! `SOUNDCRAFT_SKIP_GUI_TESTS=1` to skip that (it is skipped anyway without a screen). The window
//! is otherwise exercised manually (see `installed_plugins_smoke`).

mod common;

fn main() {
    let dir = common::fixture_dir();
    let gain = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
    let mut p = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
    // Editor windows are hosted on macOS and Windows; elsewhere the probe reports "unsupported".
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(p.editor_size(), Ok((320, 240)));
    } else {
        assert!(p.editor_size().is_err());
    }
    // A resizable view: user resizes of the host window are negotiated with the view.
    assert_eq!(p.editor_resize(640, 480), Ok(Some((640, 480))), "a size the view accepts stands");
    assert_eq!(p.editor_resize(50, 2000), Ok(Some((200, 900))), "checkSizeConstraint clamps the request");
    let mut ed = soundcraft_dsp::Plugin::editor(&mut p).expect("editor handle");
    assert!(!ed.is_open());
    assert_eq!(ed.idle().len(), 1, "the fixture's setup edit");
    // Session → editor: the controller follows (seen through the component handler round trip
    // below and through the editor's own value).
    ed.set_param("7", 1.5);
    ed.set_param("nope", 1.0);
    assert!(ed.idle().is_empty(), "set_param is not reported back as an edit");
    // #599: an identity-mapped (JUCE-style) parameter shows the plugin's own text, verbatim, with
    // its units appended once; correctly mapped parameters and choices keep SoundCraft's format.
    assert_eq!(ed.value_text("13", 0.375).as_deref(), Some("\u{2212}6.0 dB"));
    assert_eq!(ed.value_text("13", 1.0).as_deref(), Some("24.0 dB"));
    assert_eq!(ed.value_text("14", 0.25).as_deref(), Some("3.0 dB"), "units already in the text");
    assert_eq!(ed.value_text("7", 1.5), None);
    assert_eq!(ed.value_text("3", 1.0), None);
    assert_eq!(ed.value_text("nope", 0.5), None);
    #[cfg(target_os = "macos")]
    hidden_is_not_closed(&mut *ed);
    drop(p);
    assert_eq!(ed.value_text("13", 0.375), None, "the plugin is gone");
    assert!(!ed.is_open());
    assert!(ed.open().is_err(), "the plugin is gone");
    println!("editor_main_thread: ok");
}

/// Hiding the window (AppKit does when the app deactivates) keeps the editor; the user closing it
/// frees it.
#[cfg(target_os = "macos")]
fn hidden_is_not_closed(ed: &mut dyn soundcraft_dsp::PluginEditor) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSScreen};
    if std::env::var_os("SOUNDCRAFT_SKIP_GUI_TESTS").is_some_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    if NSScreen::mainScreen(mtm).is_none() {
        println!("editor_main_thread: no screen; window checks skipped");
        return;
    }
    objc2::rc::autoreleasepool(|_| {
        let window = || NSApplication::sharedApplication(mtm).windows().iter().find(|w| w.isVisible() && w.title().to_string() == "Fixture Gain");
        ed.open().expect("open");
        let w = window().expect("the editor window");
        w.orderOut(None);
        let _ = ed.idle();
        assert!(ed.is_open(), "a hidden editor stays open");
        ed.open().expect("raise");
        assert!(window().is_some(), "shown again");
        w.performClose(None);
        assert!(!ed.is_open(), "closed by the user");
        let _ = ed.idle();
        assert!(window().is_none());
        ed.open().expect("reopen");
        assert!(ed.is_open());
        ed.close();
        assert!(!ed.is_open());
    });
}
