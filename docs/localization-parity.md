# Localization parity

> **Last reviewed:** 2026-10-19 · **Last updated:** 2026-10-19 · **Change:** minor (review fixes) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft's source language is English. Menu labels now have a path-keyed Simplified Chinese
table; dialogs, panels, tooltips and status messages are still English literals in Rust
(`crates/engine/src/cmd/*`, `crates/ui-egui/src/*`). Pro Tools ships its UI in **8 languages**
(bundle `.lproj` folders: English, German, Spanish, French, Japanese, Korean, Simplified Chinese,
Traditional Chinese).

**Localization: ~18 %** (English complete; Simplified Chinese covers the whole menu bar, ≈ 42 % of
the visible strings, with dialogs and panels untranslated). Remaining ≈ 55–90 h: the full string
catalog and extraction pass (PR #143, ≈ 15–25 h), CJK/Devanagari/Arabic font fallbacks for the web
build and complex-script shaping (≈ 25–40 h), then ≈ 3–5 h of translation per language and native
review (human).

Open work: PR #143 (a `tr`/`trf` string catalog and French/Spanish, pending), PR #141 (bundled
multilingual fonts for the web/offline builds, pending), issue #67 (Ukrainian).

## How the Chinese menus work

- `crates/ui-egui/src/i18n/` owns the language choice. By default it follows the operating
  system: `sys-locale` reads `AppleLanguages` on macOS, `GetUserPreferredUILanguages` on Windows,
  and `LC_*`/`LANG` on Linux and BSD. A locale such as `zh-Hans-CN`, `zh_CN.UTF-8` or `zh-SG`
  selects Simplified Chinese; English and all other locales keep English.
- **Setup › Appearance › Language** (or `ui.language {"lang":"system"|"en"|"zh-Hans"}`) pins a
  language and saves it in the UI preferences. On macOS the native menu bar is rebuilt on a
  change. `System` is the default, so a Chinese system gets Chinese menus on first launch.
- `SOUNDCRAFT_LANG=en` / `SOUNDCRAFT_LANG=zh-Hans` (or `zh-CN`, `zh_CN`, `简体中文`) overrides the
  system choice, for screenshots, tests and users whose OS language is not the one they want.
- Only presentation is translated. A menu node keeps its English `path` (for example
  `File > Save Session`); the Chinese label is looked up by that path at draw time. Command ids,
  shortcut dispatch, saved sessions and the control protocol are byte-for-byte unchanged, so
  scripts and agents behave the same in every language.
- Traditional Chinese locales (`zh-Hant`, `zh-TW`, `zh-HK`, `zh-MO`) fall back to English rather
  than showing Simplified labels; there is no Traditional table yet.
- The menu is drawn in the egui bar on Windows/Linux and in the native macOS menu bar; both read
  the same translation table.
- Chinese glyphs come from an operating-system font loaded at runtime (Hiragino Sans GB, STHeiti
  or Songti on macOS; Microsoft YaHei on Windows; Noto Sans CJK SC, Source Han Sans SC or WenQuanYi
  on Linux/FreeBSD). The Han collection is large (Hiragino Sans GB is 23 MB), so it is read only
  when the interface language is Simplified Chinese; English launches do not pay that memory cost.
  Chinese track or clip names in an English interface are the separate multilingual-name problem
  tracked in PR #141. No font is redistributed, so `ATTRIBUTION.md` is unchanged. The web build has
  no CJK font yet and deliberately stays English instead of drawing empty boxes.
- Product and plug-in names (Pro Tools, AudioSuite, HEAT, QuickPunch, D-Verb, Melodyne, …) stay in
  English, as in other DAWs.

## Status

| Language | Code | UI strings translated | Dialogs / tooltips / help | Script support | Native review | Status | Est. to full (h) | In Pro Tools |
|---|---|---|---|---|---|---|---:|---|
| English | en | all (source) | ✅ | Latin ✅ | — | **full** | 0 | ✅ |
| Simplified Chinese | zh-Hans | menus 584/584; dialogs/panels 0 | ❌ English | ✅ CJK system font fallback; IME/editing untested | no | **menus** | 20–35 | ✅ |
| Spanish | es | 0 (0 %) — PR #143 pending | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Hindi | hi | 0 (0 %) | ❌ | ❌ no Devanagari font or shaping | no | none | 8–15 | ❌ |
| Arabic | ar | 0 (0 %) | ❌ | ❌ no RTL layout, no shaping | no | none | 15–25 | ❌ |
| French | fr | 0 (0 %) — PR #143 pending | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Portuguese | pt | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Indonesian | id | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Japanese | ja | 0 (0 %) | ❌ | ❌ no CJK font fallback, no IME testing | no | none | 6–10 | ✅ |
| German | de | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Korean | ko | 0 (0 %) | ❌ | ✅ system Hangul fallback | no | none | 6–10 | ✅ |
| Vietnamese | vi | 0 (0 %) | ❌ | 🟡 Latin with stacked diacritics, untested | no | none | 3–5 | ❌ |

Other languages shipped: **0**. (Pro Tools also ships Traditional Chinese, zh-Hant.)

Per-language hours exclude the shared catalog and script work above. Simplified Chinese has no
native-speaker review yet: the menu wording follows common Chinese DAW usage, but a Chinese
engineer should read it before it is called complete.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-19 | minor | Locale script subtag wins over region (`zh-Hans-HK`/`MO` are Simplified); Han font loaded only for a Chinese interface (saves 23 MB on English launches); saved **Setup › Appearance › Language** setting and `ui.language` command, with macOS native-menu rebuild; native macOS menu bar checked with the real NSMenu |
| 2026-10-18 | minor | Simplified Chinese menus: system-language detection, 584 path-keyed labels, `SOUNDCRAFT_LANG`, CJK system-font fallback (native only); dialogs and panels still English |
| 2026-10-10 | major | Created: English only, no catalog; the twelve key languages at 0 %; Pro Tools' 8 languages listed from its bundle |
