//! Audio Unit editors must be driven from the process main thread, which the standard test
//! harness never uses, so this test binary has its own `main` (`harness = false`). It opens real
//! windows (briefly on screen) for Apple's built-in units only. Set `SOUNDCRAFT_SKIP_GUI_TESTS=1`
//! to skip it (it is skipped anyway without a screen, e.g. outside a login session).
fn main() {
    #[cfg(target_os = "macos")]
    if std::env::var_os("SOUNDCRAFT_SKIP_GUI_TESTS").is_some_and(|v| !v.is_empty() && v != "0") {
        println!("editor_main_thread: SOUNDCRAFT_SKIP_GUI_TESTS set; skipped");
    } else {
        mac::run();
    }
    println!("editor_main_thread: ok");
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::MainThreadMarker;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_app_kit::{NSAccessibilitySlider, NSApplication, NSScreen, NSSlider, NSView, NSWindow};
    use objc2_foundation::{NSDate, NSRunLoop};
    use soundcraft_dsp::Plugin;
    use std::time::{Duration, Instant};

    /// Has its own Cocoa view (`kAudioUnitProperty_CocoaUI`).
    const DELAY: &str = "au:aufx:dely:appl";
    /// Has none: CoreAudioKit's generic view.
    const SAMPLE_DELAY: &str = "au:aufx:sdly:appl";

    /// Creates a unit by id without a scan of the system's components.
    fn create(id: &str) -> Option<Box<dyn Plugin>> {
        let p = soundcraft_au_host::create(id);
        if p.is_none() {
            eprintln!("{id} unavailable; skipping");
        }
        p
    }

    /// The visible editor window titled `title`, with its content view's first subview.
    fn editor_window(title: &str) -> Option<(Retained<NSWindow>, Retained<NSView>)> {
        let mtm = MainThreadMarker::new().expect("main thread");
        let w = NSApplication::sharedApplication(mtm).windows().iter().find(|w| w.isVisible() && w.title().to_string() == title)?;
        let view = w.contentView()?.subviews().firstObject()?;
        Some((w, view))
    }

    /// Every slider in `view`'s subtree.
    fn sliders(view: &NSView) -> Vec<Retained<NSSlider>> {
        let mut out = Vec::new();
        for v in view.subviews().iter() {
            match v.clone().downcast::<NSSlider>() {
                Ok(s) => out.push(s),
                Err(v) => out.extend(sliders(&v)),
            }
        }
        out
    }

    /// Runs the main run loop briefly (delivers the listener's notifications).
    fn pump() {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
    }

    /// Runs the run loop until `done` holds, for at most 2 s; whether it held.
    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if done() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            pump();
        }
    }

    fn block(p: &mut Box<dyn Plugin>) {
        let mut io = vec![vec![0.1f32; 512]; 2];
        p.process(&mut io, 512);
    }

    /// Open, idle, set a parameter from the host, close by the window's button and reopen, drop
    /// the instance with the editor open.
    fn exercise(id: &str, generic: bool) {
        let Some(mut p) = create(id) else { return };
        p.prepare(48_000.0, 512, 2);
        let title = p.info().name;
        let mut ed = p.editor().expect("every Audio Unit has an editor handle");
        assert!(!ed.is_open());
        autoreleasepool(|_| {
            ed.open().expect("open on the main thread");
            assert!(ed.is_open());
            let (w, view) = editor_window(title).expect("editor window with the unit's view");
            let class = view.class().name().to_string_lossy().into_owned();
            assert_eq!(class == "AUGenericView", generic, "{id}: view class {class}");
            let (content, size) = (w.contentView().expect("content").frame().size, view.frame().size);
            assert!(size.width >= 1.0 && size.height >= 1.0, "{id}: view size {size:?}");
            assert_eq!(content, size, "{id}: the window is sized to the view");
            ed.open().expect("opening again raises it");
            block(&mut p);
            pump();
            assert!(ed.idle().is_empty(), "{id}: no edits made in the view");
            let param = p.info().params.first().expect("a parameter");
            let v = param.min + 0.73 * (param.max - param.min);
            if generic {
                // An edit in the view (a slider stepped, as VoiceOver or the keyboard does)
                // reaches idle as (SoundCraft id, value).
                let s = sliders(&view).into_iter().next().expect("the generic view has sliders");
                // (NSSlider steps but answers `false` here; the edit is what counts.)
                let before = s.doubleValue();
                let _ = s.accessibilityPerformIncrement();
                assert!(s.doubleValue() > before, "{id}: the slider moved");
                let mut edits = Vec::new();
                assert!(
                    wait_until(|| {
                        edits.extend(ed.idle());
                        !edits.is_empty()
                    }),
                    "{id}: the view's edit is reported"
                );
                assert_eq!(edits.len(), 1, "{id}: one edit reported: {edits:?}");
                let (pid, ev) = &edits[0];
                let info = p.info().param(pid).expect("a known parameter");
                assert!(ev.is_finite() && (info.min..=info.max).contains(ev), "{id}: {pid} = {ev}");
                // A host-side change reaches the view (a slider moves) and is not echoed back.
                let values = |v: &NSView| sliders(v).iter().map(|s| s.doubleValue()).collect::<Vec<_>>();
                let shown = values(&view);
                ed.set_param(param.id, v);
                assert!(wait_until(|| values(&view) != shown), "{id}: the view shows the host's value");
                assert!(!wait_until(|| !ed.idle().is_empty()), "{id}: no echo");
            } else {
                // The unit's own view: the call must not disturb it (what it shows is up to it).
                ed.set_param(param.id, v);
                pump();
                assert!(ed.idle().is_empty(), "{id}: no echo");
            }
            // Hidden (as AppKit does when the app deactivates) is not closed.
            w.orderOut(None);
            pump();
            assert!(ed.idle().is_empty());
            assert!(ed.is_open(), "{id}: a hidden editor stays open");
            ed.open().expect("raise");
            // The user closes the window: idle frees the view.
            w.performClose(None);
            assert!(!ed.is_open());
            assert!(ed.idle().is_empty());
            pump();
            assert!(editor_window(title).is_none(), "{id}: closed");
            // Closed and reopened before the next idle: a fresh editor that survives idle.
            ed.open().expect("reopen");
            let (w, _) = editor_window(title).expect("reopened");
            w.performClose(None);
            ed.open().expect("reopen before idle");
            assert!(ed.idle().is_empty());
            assert!(ed.is_open() && editor_window(title).is_some(), "{id}: the reopened editor survives idle");
            ed.close();
            // Through the Plugin trait as well.
            p.open_editor().expect("open via the trait");
            assert!(ed.is_open(), "the handle sees the same editor");
            p.close_editor();
            assert!(!ed.is_open());
            // Reopen, process, then drop the instance with the editor open: the view goes first.
            ed.open().expect("reopen");
            block(&mut p);
            pump();
            drop(p);
            assert!(!ed.is_open());
            assert!(editor_window(title).is_none(), "{id}: the window closed with the instance");
            assert!(ed.open().is_err(), "the instance is gone");
            assert!(ed.idle().is_empty());
            ed.close();
        });
        pump();
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("main thread");
        if NSScreen::mainScreen(mtm).is_none() {
            println!("editor_main_thread: no screen; skipped");
            return;
        }
        exercise(DELAY, false);
        exercise(SAMPLE_DELAY, true);
    }
}
