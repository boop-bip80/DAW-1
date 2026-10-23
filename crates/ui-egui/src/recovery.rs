//! The offer to recover unsaved work an earlier run left in the autosave folder.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use egui::{Align2, vec2};

/// Offer the newest leftover autosave, one at a time.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(left) = app.leftovers.first().cloned() else { return };
    let mut open = true;
    let mut recover = None;
    egui::Window::new(egui::RichText::new(crate::i18n::tr("Recover Unsaved Work")).font(bold(13.0)))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, vec2(0.0, -80.0))
        .show(ctx, |ui| {
            let age = left.saved.elapsed().map_or_else(|_| crate::i18n::tr("recently").into(), |d| ago(d.as_secs()));
            ui.label(recovery_message(&left.name, left.recording, &age));
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(crate::i18n::tr("Discard")).clicked() {
                    recover = Some(false);
                }
                let recover_button = egui::Button::new(egui::RichText::new(crate::i18n::tr("Recover")).strong()).fill(Tokens::current().accent);
                if ui.add_enabled(!app.engine.transport.recording, recover_button).on_disabled_hover_text(crate::i18n::tr("Stop recording first")).clicked() {
                    recover = Some(true);
                }
            });
        });
    if !open || recover.is_some() {
        app.leftovers.retain(|l| *l != left);
    }
    match recover {
        Some(true) => app.recover(&left),
        Some(false) => soundcraft_engine::autosave::discard(&left),
        None => {}
    }
}

fn ago(secs: u64) -> String {
    let (n, unit) = match secs {
        0..60 => return crate::i18n::tr("less than a minute ago").into(),
        60..3_600 => (secs / 60, "minute"),
        3_600..86_400 => (secs / 3_600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    let suffix = if n == 1 { "" } else { "s" };
    if crate::i18n::is_korean() {
        crate::i18n::format_fields("{n} {unit}{} ago", &[
            ("n", n.to_string()),
            ("unit", crate::i18n::tr(unit).to_owned()),
            ("", crate::i18n::english_ending(suffix).to_owned()),
        ])
    } else {
        format!("{n} {unit}{} ago", suffix)
    }
}

fn recovery_message(name: &str, recording: bool, age: &str) -> String {
    let what = crate::i18n::tr(if recording { "unsaved changes and an unfinished recording" } else { "unsaved changes" });
    if crate::i18n::is_korean() {
        crate::i18n::format_fields("“{}” has {what} from an earlier run, autosaved {age}.", &[
            ("", name.to_owned()), ("what", what.to_owned()), ("age", age.to_owned()),
        ])
    } else {
        format!("“{name}” has {what} from an earlier run, autosaved {age}.")
    }
}

#[cfg(test)]
mod korean_recovery_tests {
    use super::{ago, recovery_message};
    use crate::i18n::{Language, set_current, tr};

    #[test]
    fn recovery_fragments_form_complete_sentences_in_both_languages() {
        set_current(Language::Korean);
        for (secs, expected) in [(0, "1분 미만 전"), (59, "1분 미만 전"), (60, "1분 전"), (120, "2분 전"), (3600, "1시간 전"), (7200, "2시간 전"), (86400, "1일 전"), (172800, "2일 전")] {
            assert_eq!(ago(secs), expected);
        }
        assert_eq!(recovery_message("Session {name}", false, &ago(120)), "“Session {name}”에 이전 실행의 저장되지 않은 변경 내용이(가) 남아 있습니다. 2분 전 자동 저장되었습니다.");
        assert_eq!(recovery_message("Session {name}", true, tr("recently")), "“Session {name}”에 이전 실행의 저장되지 않은 변경 내용과 완료되지 않은 녹음이(가) 남아 있습니다. 최근 자동 저장되었습니다.");
        set_current(Language::English);
        assert_eq!(ago(59), "less than a minute ago");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(120), "2 minutes ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(86400), "1 day ago");
        assert_eq!(recovery_message("Session {name}", true, "recently"), "“Session {name}” has unsaved changes and an unfinished recording from an earlier run, autosaved recently.");
    }
}
