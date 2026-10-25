//! Plugin editors must be driven from the process main thread, which the standard test harness
//! never uses, so this test binary has its own `main` (`harness = false`). The fixture's GUI is
//! window-less: nothing appears on screen.

mod common;

use soundcraft_dsp::Plugin;

const GAIN: &str = "clap:org.soundcraft.test.gain";

fn block(p: &mut Box<dyn Plugin>) -> Vec<Vec<f32>> {
    let mut io = vec![vec![1.0f32; 64]; 2];
    p.process(&mut io, 64);
    io
}

fn main() {
    // nextest lists custom-harness tests with `--list`; each binary here is one test.
    if std::env::args().any(|a| a == "--list") {
        println!("editor_main_thread: test");
        return;
    }
    common::fixture_dir();
    let mut p = soundcraft_clap_host::create(GAIN).expect("fixture");
    let info = soundcraft_clap_host::plugin_info(GAIN).expect("info");
    assert_eq!(info.param("3").expect("mode").choices, &["A", "B", "C"], "the main thread reads the plugin's choice text");
    assert_eq!(p.value_text("3", 1.0).as_deref(), Some("B"));
    assert_eq!(p.value_from_text("3", "b"), Some(1.0));
    assert_eq!(p.value_from_text("3", "not a mode"), None);
    assert_eq!(p.value_from_text("3", &"x".repeat(257)), None);
    p.prepare(48_000.0, 64, 2);
    let mut ed = p.editor().expect("editor handle");
    assert!(!ed.is_open());
    // Text and the live value go through the same main-thread handle as the window, before any
    // GUI edit.
    assert_eq!(ed.value_text("3", 1.0).as_deref(), Some("B"));
    assert_eq!(ed.value_from_text("3", "b"), Some(1.0));
    assert_eq!(ed.value_from_text("3", "nope"), None);
    assert!((ed.param("7").unwrap_or(-1.0) - 1.0).abs() < 1e-6);
    ed.open().expect("open on the main thread");
    assert!(ed.is_open());
    ed.open().expect("opening again just raises it");
    // The fixture "edits" the gain in its GUI; the edit arrives as an output event of the next
    // block and is reported (in SoundCraft units) by idle.
    let io = block(&mut p);
    assert!((io[0][10] - 0.25).abs() < 1e-6, "{}", io[0][10]);
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.25)]);
    assert!((ed.param("7").unwrap_or(-1.0) - 0.25).abs() < 1e-6, "the live value follows the edit");
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
    assert_eq!(ed.param("7"), None, "the plugin is gone");
    assert_eq!(ed.value_from_text("3", "b"), None, "the plugin is gone");
    assert!(ed.open().is_err());
    assert!(ed.idle().is_empty());
    ed.close();
    println!("editor_main_thread: ok");
}
