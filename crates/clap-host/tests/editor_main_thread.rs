//! Plugin editors must be driven from the process main thread, which the standard test harness
//! never uses, so this test binary has its own `main` (`harness = false`). The gain fixture's
//! floating GUI is window-less; the embedded fixture's GUI lives in a real host window (macOS and
//! Windows), which briefly appears on screen. Set `SOUNDCRAFT_SKIP_GUI_TESTS=1` to skip that part
//! (it is skipped anyway on macOS without a screen, e.g. outside a login session).

mod common;

use soundcraft_dsp::Plugin;

const GAIN: &str = "clap:org.soundcraft.test.gain";
const EMBEDDED: &str = "clap:org.soundcraft.test.embedded";

fn block(p: &mut Box<dyn Plugin>) -> Vec<Vec<f32>> {
    let mut io = vec![vec![1.0f32; 64]; 2];
    p.process(&mut io, 64);
    io
}

fn main() {
    common::fixture_dir();
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    p.prepare(48_000.0, 64, 2);
    let mut ed = p.editor().expect("editor handle");
    assert!(!ed.is_open());
    ed.open().expect("open on the main thread");
    assert!(ed.is_open());
    ed.open().expect("opening again just raises it");
    // The fixture "edits" the gain in its GUI; the edit arrives as an output event of the next
    // block and is reported (in SoundCraft units) by idle.
    let io = block(&mut p);
    assert!((io[0][10] - 0.25).abs() < 1e-6, "{}", io[0][10]);
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)]);
    assert!(ed.idle().is_empty(), "drained");
    ed.close();
    assert!(!ed.is_open());
    // Through the Plugin trait as well.
    p.open_editor().expect("open via the trait");
    assert!(ed.is_open(), "the handle sees the same editor");
    p.close_editor();
    assert!(!ed.is_open());
    // Dropping the instance with its editor open destroys the GUI first (the fixture asserts
    // that); the handle then refuses politely.
    ed.open().expect("reopen");
    drop(p);
    assert!(!ed.is_open());
    assert!(ed.open().is_err());
    assert!(ed.idle().is_empty());
    ed.close();
    embedded();
    println!("editor_main_thread: ok");
}

/// The visible host window of the embedded fixture, its content size and its child view's size.
#[cfg(target_os = "macos")]
fn host_window() -> Option<(objc2::rc::Retained<objc2_app_kit::NSWindow>, (f64, f64), (f64, f64))> {
    let mtm = objc2::MainThreadMarker::new().expect("main thread");
    let w = objc2_app_kit::NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .find(|w| w.isVisible() && w.title().to_string() == "Fixture Embedded")?;
    let content = w.contentView()?;
    let child = content.subviews().firstObject()?;
    let (c, v) = (content.frame().size, child.frame().size);
    Some((w, (c.width, c.height), (v.width, v.height)))
}

/// Real windows can be opened: not opted out, and (macOS) a screen to put them on.
fn gui_available() -> bool {
    if std::env::var_os("SOUNDCRAFT_SKIP_GUI_TESTS").is_some_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let mtm = objc2::MainThreadMarker::new().expect("main thread");
        objc2_app_kit::NSScreen::mainScreen(mtm).is_some()
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// An embedded-only CLAP editor (no floating window): hosted in a window of ours on macOS and
/// Windows, refused elsewhere.
fn embedded() {
    let mut p = soundcraft_clap_host::create(EMBEDDED).expect("embedded fixture");
    p.prepare(48_000.0, 64, 2);
    let mut ed = p.editor().expect("editor handle");
    if !cfg!(any(target_os = "macos", windows)) {
        assert!(ed.open().is_err(), "embedded editors are not hosted here");
        return;
    }
    if !gui_available() {
        println!("editor_main_thread: no GUI session; embedded editor windows skipped");
        return;
    }
    objc_pool(|| {
        ed.open().expect("embedded editor opens");
        assert!(ed.is_open());
        #[cfg(target_os = "macos")]
        {
            use objc2_app_kit::NSWindowStyleMask;
            use objc2_foundation::NSSize;
            let (w, content, child) = host_window().expect("host window with the plugin's view");
            assert_eq!((content, child), ((400.0, 300.0), (400.0, 300.0)), "sized by get_size");
            assert!(w.styleMask().contains(NSWindowStyleMask::Resizable), "can_resize gives a resize handle");
            // The fixture's `show` asked for 300×200 (`request_resize`): applied by idle.
            let io = block(&mut p);
            assert!((io[0][10] - 0.25).abs() < 1e-6);
            assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)]);
            let (_, content, child) = host_window().expect("still open");
            assert_eq!((content, child), ((300.0, 200.0), (300.0, 200.0)), "request_resize applied");
            // A user resize is negotiated: adjust_size clamps it, the window follows, set_size
            // resizes the plugin's view.
            w.setContentSize(NSSize::new(1000.0, 1000.0));
            let (_, content, child) = host_window().expect("still open");
            assert_eq!((content, child), ((800.0, 600.0), (800.0, 600.0)), "user resize adjusted");
            // Hidden (as AppKit does when the app deactivates) is not closed.
            w.orderOut(None);
            assert!(ed.idle().is_empty());
            assert!(ed.is_open(), "a hidden editor stays open");
            ed.open().expect("raise");
            assert!(host_window().is_some(), "raised again");
            // The user closes the window: the editor notices and frees the GUI in idle.
            w.performClose(None);
            assert!(!ed.is_open());
            assert!(ed.idle().is_empty());
            assert!(host_window().is_none());
            // Closed by the user and reopened before the next idle: a fresh editor, not the
            // closed window brought back (and then freed by idle).
            ed.open().expect("reopen");
            let (w, _, _) = host_window().expect("reopened");
            w.performClose(None);
            assert!(!ed.is_open());
            ed.open().expect("reopen before idle");
            assert!(ed.idle().is_empty());
            assert!(ed.is_open(), "the reopened editor survives idle");
            assert!(host_window().is_some());
        }
        ed.close();
        assert!(!ed.is_open());
        // Reopen, then drop the instance with the editor open: the GUI goes first (the fixture
        // asserts that), and the window with it.
        ed.open().expect("reopen");
        assert!(ed.is_open());
        drop(p);
        assert!(!ed.is_open());
        #[cfg(target_os = "macos")]
        assert!(host_window().is_none(), "the host window closed with the instance");
        assert!(ed.open().is_err());
    });
}

/// Runs `f` inside an autorelease pool (the app's event loop provides one; this test has none).
#[cfg(target_os = "macos")]
fn objc_pool(f: impl FnOnce()) {
    objc2::rc::autoreleasepool(|_| f());
}

#[cfg(not(target_os = "macos"))]
fn objc_pool(f: impl FnOnce()) {
    f();
}
