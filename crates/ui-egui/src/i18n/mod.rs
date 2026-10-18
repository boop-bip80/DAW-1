//! The interface language, used for the menu bar and the items in it.
//!
//! English is the source language: menu paths and their labels stay English in the catalog, the
//! command registry and the control protocol. Only what is drawn is translated, by looking the
//! English menu path up in the [`zh`] table. Command ids and `MenuNode::path` therefore stay
//! stable in every language, so shortcuts, scripts and agents keep working.
//!
//! At startup the language follows the operating system ([`sys_locale`]); setting
//! `SOUNDCRAFT_LANG` overrides it, which also makes the UI reproducible in tests and screenshots:
//! `SOUNDCRAFT_LANG=en` for English, `SOUNDCRAFT_LANG=zh-Hans` for Simplified Chinese.
//!
//! Only Simplified Chinese is shipped today. Other locales fall back to English, including
//! Traditional Chinese (`zh-Hant`, `zh-TW`, `zh-HK`, `zh-MO`), which has no translation yet.
//! The web build also stays English until a CJK font is bundled: canvas text cannot use the
//! browser's fonts, so Chinese labels would draw as empty boxes there.

mod zh;

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;

/// A language the interface can be drawn in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    SimplifiedChinese,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::SimplifiedChinese];

    /// The id used in `SOUNDCRAFT_LANG`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-Hans",
        }
    }

    /// The language's own name, so it can be recognized without reading English.
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }

    /// Parse an explicit `SOUNDCRAFT_LANG` value, or `None` when it names no shipped language.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if value.eq_ignore_ascii_case("en") || value.eq_ignore_ascii_case("english") || value == "英文" {
            return Some(Self::English);
        }
        let normalized = value.replace('_', "-").to_ascii_lowercase();
        if normalized == "zh"
            || normalized.starts_with("zh-hans")
            || normalized.starts_with("zh-cn")
            || normalized.starts_with("zh-sg")
            || value == "简体中文"
            || value == "中文"
        {
            return Some(Self::SimplifiedChinese);
        }
        None
    }

    /// The language a BCP 47 system locale asks for. Unknown and Traditional Chinese locales
    /// fall back to English rather than showing Simplified labels to a Traditional reader.
    ///
    /// An explicit script subtag wins over the region: `zh-Hans-HK` is Simplified Chinese even
    /// though Hong Kong defaults to Traditional, and `zh-Hant-CN` is Traditional. Only a locale
    /// without `Hans`/`Hant` uses the region (`TW`, `HK`, `MO` = Traditional).
    pub fn from_locale(locale: &str) -> Self {
        let locale = locale.split(['.', '@']).next().unwrap_or("").replace('_', "-").to_ascii_lowercase();
        if locale == "zh" {
            return Self::SimplifiedChinese;
        }
        if !locale.starts_with("zh-") {
            return Self::English;
        }
        let mut has_hans = false;
        let mut has_hant = false;
        let mut traditional_region = false;
        for part in locale.split('-').skip(1) {
            match part {
                "hans" => has_hans = true,
                "hant" => has_hant = true,
                "tw" | "hk" | "mo" => traditional_region = true,
                _ => {}
            }
        }
        if has_hans {
            Self::SimplifiedChinese
        } else if has_hant || traditional_region {
            Self::English
        } else {
            Self::SimplifiedChinese
        }
    }
}

/// The saved Language setting (Setup › Appearance › Language). [`Language`] is the resolved
/// language used for drawing; this is the user's choice, which may mean "follow the system".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LanguageSetting {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-Hans")]
    SimplifiedChinese,
}

impl LanguageSetting {
    pub const ALL: [Self; 3] = [Self::System, Self::English, Self::SimplifiedChinese];

    /// The id used in `ui.language`, `UiState` and `SOUNDCRAFT_LANG`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::SimplifiedChinese => "zh-Hans",
        }
    }

    /// Parse an explicit setting. Region and script aliases are accepted for the command line;
    /// Traditional Chinese names no shipped interface, so it is not a valid setting.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if value.eq_ignore_ascii_case("system") || value.eq_ignore_ascii_case("auto") || value == "跟随系统" {
            return Some(Self::System);
        }
        if value.eq_ignore_ascii_case("en") || value.eq_ignore_ascii_case("english") {
            return Some(Self::English);
        }
        let normalized = value.replace('_', "-").to_ascii_lowercase();
        if normalized == "zh"
            || normalized.starts_with("zh-hans")
            || normalized.starts_with("zh-cn")
            || normalized.starts_with("zh-sg")
            || value == "简体中文"
            || value == "中文"
        {
            return Some(Self::SimplifiedChinese);
        }
        None
    }

    /// The name to show in the picker. "System" is translated only when the interface itself is
    /// Chinese; the language names stay in their own script so anyone can find theirs.
    pub fn label(self, current: Language) -> &'static str {
        match self {
            Self::System => {
                if current == Language::SimplifiedChinese {
                    "跟随系统"
                } else {
                    "System"
                }
            }
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }
}

thread_local! {
    // Per thread, like the rest of the UI state: tests can pick a language without racing each
    // other, and the desktop app resolves the system locale on its UI thread.
    static OVERRIDE: Cell<Option<Language>> = const { Cell::new(None) };
    static SETTING: Cell<LanguageSetting> = const { Cell::new(LanguageSetting::System) };
}

/// The language the interface is drawn in: a test/screenshot [`set`] override, else the saved
/// [`LanguageSetting`], else the system locale.
pub fn current() -> Language {
    if let Some(language) = OVERRIDE.with(Cell::get) {
        return language;
    }
    match setting() {
        LanguageSetting::English => Language::English,
        LanguageSetting::SimplifiedChinese => Language::SimplifiedChinese,
        LanguageSetting::System => detected(),
    }
}

/// The saved setting (System by default).
pub fn setting() -> LanguageSetting {
    SETTING.with(Cell::get)
}

/// Save the user's choice and draw with it. An explicit setting replaces a test/screenshot
/// override; [`set`] itself leaves the setting alone.
pub fn set_setting(setting: LanguageSetting) {
    OVERRIDE.with(|choice| choice.set(None));
    SETTING.with(|saved| saved.set(setting));
}

/// Draw interface strings in `language` from now on, ignoring the system locale (tests,
/// screenshots). The macOS system menu bar is built once at startup, so a host that lets the user
/// switch at run time must rebuild it; this build decides the language once, at launch.
pub fn set(language: Language) {
    OVERRIDE.with(|choice| choice.set(Some(language)));
}

/// Forget an explicit [`set`] and follow the system again (used between tests).
#[cfg(test)]
fn clear() {
    OVERRIDE.with(|choice| choice.set(None));
}

fn detected() -> Language {
    static DETECTED: OnceLock<Language> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        if let Ok(value) = std::env::var("SOUNDCRAFT_LANG") {
            if let Some(language) = Language::parse(&value) {
                return language;
            }
            // "zh-Hant", "fr", …: a locale we do not ship. English, not the nearest table.
            return Language::from_locale(&value);
        }
        language_from_system()
    })
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn language_from_system() -> Language {
    sys_locale::get_locale().map_or(Language::English, |locale| Language::from_locale(&locale))
}

/// Unit tests and the web build start in English. Tests that exercise a translation call
/// [`set`]; the web build has no bundled CJK font yet (see the module docs).
#[cfg(any(test, target_arch = "wasm32"))]
fn language_from_system() -> Language {
    Language::English
}

fn menu_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| zh::MENU.iter().copied().collect())
}

fn ui_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| zh::UI.iter().copied().collect())
}

/// The label for a menu node, looked up by its stable English path. Falls back to the English
/// label when the node has no translation, so adding a menu item never leaves a blank item.
pub fn menu_label<'a>(path: &str, english: &'a str) -> &'a str {
    if current() == Language::English {
        return english;
    }
    menu_map().get(path).copied().unwrap_or(english)
}

/// A standalone interface string (application menu, dialogs) in the current language.
pub fn tr(english: &str) -> &str {
    if current() == Language::English {
        return english;
    }
    ui_map().get(english).copied().unwrap_or(english)
}

/// Translate a template, then fill its `{}` placeholders in order (`{{` and `}}` are literal).
pub fn trf(template: &str, args: &[&dyn Display]) -> String {
    fill(tr(template), args)
}

fn fill(template: &str, args: &[&dyn Display]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(template.len() + 8 * args.len());
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('{', Some('}')) => {
                chars.next();
                if let Some(a) = args.next() {
                    let _ = write!(out, "{a}");
                }
            }
            ('{', Some('{')) | ('}', Some('}')) => {
                chars.next();
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplified_chinese_locales_are_recognized() {
        for locale in ["zh-Hans", "zh-Hans-CN", "zh_CN.UTF-8", "zh-CN", "zh-SG", "zh", "ZH-hans"] {
            assert_eq!(Language::from_locale(locale), Language::SimplifiedChinese, "{locale}");
        }
    }

    #[test]
    fn other_locales_stay_english() {
        for locale in ["en-US", "en_GB.UTF-8", "fr-FR", "ja-JP", "", "zh-Hant", "zh-TW", "zh-HK", "zh-MO", "zh-Hant-CN", "zh-Hant-SG"] {
            assert_eq!(Language::from_locale(locale), Language::English, "{locale}");
        }
    }

    #[test]
    fn an_explicit_script_subtag_wins_over_the_region() {
        for locale in ["zh-Hans-HK", "zh-Hans-MO", "zh-Hans-TW", "zh-Hans-Hant"] {
            assert_eq!(Language::from_locale(locale), Language::SimplifiedChinese, "{locale}");
        }
        assert_eq!(Language::from_locale("zh-Hant-CN"), Language::English);
        assert_eq!(Language::from_locale("zh-Hant-SG"), Language::English);
    }

    #[test]
    fn explicit_language_values_parse() {
        assert_eq!(Language::parse("en"), Some(Language::English));
        assert_eq!(Language::parse("English"), Some(Language::English));
        assert_eq!(Language::parse("zh-Hans"), Some(Language::SimplifiedChinese));
        assert_eq!(Language::parse("zh_cn"), Some(Language::SimplifiedChinese));
        assert_eq!(Language::parse("简体中文"), Some(Language::SimplifiedChinese));
        assert_eq!(Language::parse("zh-Hant"), None);
        assert_eq!(Language::parse("fr"), None);
        assert_eq!(Language::parse(""), None);
    }

    #[test]
    fn chinese_labels_differ_from_english_and_still_fall_back() {
        set(Language::SimplifiedChinese);
        assert_eq!(menu_label("File", "File"), "文件");
        assert_eq!(menu_label("File > Save Session", "Save Session"), "保存工程");
        assert_eq!(menu_label("Not > A Menu", "A Menu"), "A Menu");
        assert_eq!(tr("Open Recent"), "打开最近");
        assert_eq!(tr("Not a UI string"), "Not a UI string");
        assert_eq!(trf("Undo {}", &[&"Mute"]), "撤销 Mute");
        clear();
    }

    #[test]
    fn every_menu_node_has_a_translation() {
        let mut missing = Vec::new();
        fn walk(nodes: &[crate::menus::MenuNode], missing: &mut Vec<String>) {
            for node in nodes {
                if !zh::MENU.iter().any(|(path, _)| *path == node.path) {
                    missing.push(node.path.clone());
                }
                walk(&node.children, missing);
            }
        }
        walk(&crate::menus::tree(), &mut missing);
        assert!(missing.is_empty(), "menu paths without a Simplified Chinese label: {missing:?}");
    }

    #[test]
    fn translations_are_not_empty_and_keep_placeholders() {
        for (english, chinese) in zh::MENU.iter().chain(zh::UI) {
            assert!(!english.is_empty(), "empty English key");
            assert!(!chinese.trim().is_empty(), "{english}: empty translation");
            assert_eq!(english.matches("{}").count(), chinese.matches("{}").count(), "{english} -> {chinese}: placeholders must match");
        }
    }

    #[test]
    fn menu_paths_are_not_duplicated() {
        let mut seen = std::collections::HashSet::new();
        for (path, _) in zh::MENU {
            assert!(seen.insert(*path), "duplicate translation for {path}");
        }
        let mut seen = std::collections::HashSet::new();
        for (key, _) in zh::UI {
            assert!(seen.insert(*key), "duplicate UI translation for {key}");
        }
    }

    #[test]
    fn filling_a_template_handles_escaped_braces() {
        assert_eq!(fill("{} and {}", &[&1, &2]), "1 and 2");
        assert_eq!(fill("{{}}", &[]), "{}");
        assert_eq!(fill("no placeholders", &[&1]), "no placeholders");
    }

    #[test]
    fn the_saved_setting_beats_the_system_but_a_test_override_beats_both() {
        set_setting(LanguageSetting::English);
        assert_eq!(current(), Language::English);
        set(Language::SimplifiedChinese);
        assert_eq!(current(), Language::SimplifiedChinese);
        clear();
        assert_eq!(current(), Language::English, "the setting is still English");
        set_setting(LanguageSetting::System);
        clear();
    }

    #[test]
    fn language_setting_parses_and_serializes_its_documented_ids() {
        assert_eq!(LanguageSetting::parse("system"), Some(LanguageSetting::System));
        assert_eq!(LanguageSetting::parse("跟随系统"), Some(LanguageSetting::System));
        assert_eq!(LanguageSetting::parse("en"), Some(LanguageSetting::English));
        assert_eq!(LanguageSetting::parse("简体中文"), Some(LanguageSetting::SimplifiedChinese));
        assert_eq!(LanguageSetting::parse("zh-Hant"), None);
        assert_eq!(LanguageSetting::parse("fr"), None);
        assert_eq!(serde_json::to_string(&LanguageSetting::System).ok().as_deref(), Some("\"system\""));
        assert_eq!(serde_json::to_string(&LanguageSetting::English).ok().as_deref(), Some("\"en\""));
        assert_eq!(serde_json::to_string(&LanguageSetting::SimplifiedChinese).ok().as_deref(), Some("\"zh-Hans\""));
    }

    #[test]
    fn the_picker_labels_follow_the_current_interface() {
        assert_eq!(LanguageSetting::System.label(Language::English), "System");
        assert_eq!(LanguageSetting::System.label(Language::SimplifiedChinese), "跟随系统");
        assert_eq!(LanguageSetting::English.label(Language::SimplifiedChinese), "English");
        assert_eq!(LanguageSetting::SimplifiedChinese.label(Language::English), "简体中文");
    }

    #[test]
    fn old_ui_json_gets_the_system_language_setting() {
        let ui: crate::UiState = serde_json::from_str("{}").expect("older prefs load");
        assert_eq!(ui.language, LanguageSetting::System);
        let json = serde_json::to_string(&ui).expect("ui serializes");
        assert!(json.contains("\"language\":\"system\""), "{json}");
    }
}
