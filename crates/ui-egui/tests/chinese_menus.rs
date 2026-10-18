//! The menu bar follows the system language: Simplified Chinese labels over the same English
//! paths, so a Chinese machine shows Chinese menus and scripts dispatch exactly as before.

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_ui_egui::{ControlRequest, Services, SoundApp, control, i18n, menus};
use std::sync::mpsc;

fn harness() -> Harness<'static, SoundApp> {
    let app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(6);
    h
}

#[test]
fn chinese_menus_render_and_keep_english_dispatch_paths() {
    i18n::set(i18n::Language::SimplifiedChinese);
    let mut h = harness();
    for root in ["文件", "编辑", "轨道", "片段", "事件", "AudioSuite", "选项", "设置", "视图", "窗口"] {
        assert!(h.query_by_label(root).is_some(), "menu bar is missing {root}");
    }

    h.get_by_label("文件").click();
    h.run_steps(4);
    // Menu items carry their shortcut in the accessibility label ("保存工程 Ctrl+S").
    assert!(h.query_by_label_contains("保存工程").is_some(), "File menu is not translated");
    assert!(h.query_by_label_contains("新建…").is_some(), "File ▸ New is not translated");
    assert!(h.query_by_label_contains("Save Session").is_none(), "English menu item is still drawn");

    // The label changed, the path did not: the same id is found from the English menu path.
    let extra = menus::ui_aliases();
    assert_eq!(menus::command_for_path("File > Save Session", &extra).as_deref(), Some("session.save"));
    assert_eq!(menus::command_for_path("File > Import > Audio...", &extra).as_deref(), Some("file.import_audio"));
}

#[test]
fn undo_and_redo_labels_follow_the_language() {
    i18n::set(i18n::Language::SimplifiedChinese);
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    assert_eq!(menus::live_label(&app, Some("edit.undo")).as_deref(), Some("无法撤销"));
    app.run("mix.mute", serde_json::json!({"track": "Kick"})).unwrap();
    assert_eq!(menus::live_label(&app, Some("edit.undo")).as_deref(), Some("撤销 Mute"));
    i18n::set(i18n::Language::English);
    assert_eq!(menus::live_label(&app, Some("edit.undo")).as_deref(), Some("Undo Mute"));
}

#[test]
fn the_language_command_switches_saves_and_reports() {
    i18n::set_setting(i18n::LanguageSetting::System);
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    assert_eq!(app.ui.language, i18n::LanguageSetting::System);

    let reply = app.run("ui.language", json!({"lang": "zh-Hans"})).expect("set language");
    assert_eq!(reply["lang"], "zh-Hans");
    assert_eq!(app.ui.language, i18n::LanguageSetting::SimplifiedChinese);
    assert_eq!(i18n::current(), i18n::Language::SimplifiedChinese);
    assert!(app.take_language_changed(), "the host must save the preference");

    assert_eq!(app.run("ui.language", json!({})).expect("read language")["lang"], "zh-Hans");
    assert!(app.run("ui.language", json!({"lang": "fr"})).is_err());
    assert!(app.run("ui.language", json!({"lang": "zh-Hant"})).is_err());
}

#[test]
fn the_control_channel_can_set_the_language() {
    i18n::set_setting(i18n::LanguageSetting::System);
    let (tx, rx) = mpsc::channel();
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default()).with_control(rx);
    let (request, reply) = ControlRequest::new("ui.language", json!({"lang": "zh-Hans"}));
    tx.send(request).expect("control request");
    control::drain(&mut app, &egui::Context::default());
    let response = reply.try_recv().expect("control reply");
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["lang"], "zh-Hans");
    assert_eq!(app.ui.language, i18n::LanguageSetting::SimplifiedChinese);
    assert_eq!(i18n::current(), i18n::Language::SimplifiedChinese);
}

#[test]
fn ui_set_can_change_the_saved_language_too() {
    i18n::set_setting(i18n::LanguageSetting::System);
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.run("ui.set", json!({"language": "en"})).expect("ui.set language");
    assert_eq!(app.ui.language, i18n::LanguageSetting::English);
    assert_eq!(i18n::current(), i18n::Language::English);
    assert!(app.take_language_changed());
}

#[test]
fn language_parsing_covers_the_documented_overrides() {
    // The detection itself is unit-tested; this pins the documented spelling of the override.
    assert_eq!(i18n::Language::parse("zh-Hans"), Some(i18n::Language::SimplifiedChinese));
    assert_eq!(i18n::Language::parse("en"), Some(i18n::Language::English));
    assert_eq!(i18n::Language::from_locale("zh-Hans-CN"), i18n::Language::SimplifiedChinese);
    assert_eq!(i18n::Language::from_locale("en-US"), i18n::Language::English);
}
