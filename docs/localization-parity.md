# Localization parity

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-10 · **Change:** minor (Ukrainian UI catalog and language selection) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft supports English and Ukrainian. The UI-only catalog in
`crates/ui-egui/src/i18n/uk.tsv` translates menus, command search, dialogs and built-in parameter
captions. Desktop and web follow the system language; desktop also persists an explicit choice.
Untranslated labels fall back to English, while engine and external diagnostics retain their
original wording. Pro Tools ships its UI in
**8 languages** (bundle `.lproj` folders: English, German, Spanish, French, Japanese, Korean,
Simplified Chinese, Traditional Chinese).

**Localization: ~12 %** (measured: 1 of the target's 8 UI languages, and English is the only one
complete; Ukrainian is outside that target-language count). Remaining ≈ 70–110 h: additional
language catalogs and coverage, a font fallback for
CJK/Devanagari/Arabic (PR #141 open), RTL layout and complex-script shaping (egui has neither:
≈ 25–40 h, or ship Arabic/Hindi menus-only), then ≈ 3–5 h of agent translation per language, plus
native-speaker review (human).

Open work: PR #143 (French and Spanish, a Language setting), PR #141 (bundled multilingual font
fallbacks). Ukrainian addresses issue #67.

| Language | Code | UI strings translated | Dialogs / tooltips / help | Script support | Native review | Status | Est. to full (h) | In Pro Tools |
|---|---|---|---|---|---|---|---:|---|
| English | en | all (source) | ✅ | Latin ✅ | — | **full** | 0 | ✅ |
| Ukrainian | uk | UI catalog; command/menu/built-in parameter coverage checked by tests | ✅ UI captions; diagnostics retain original wording | Cyrillic ✅ bundled fonts tested | no | available; English fallback | — | ❌ |
| Simplified Chinese | zh-Hans | 0 (0 %) | ❌ | ❌ no CJK font fallback, no IME testing | no | none | 6–10 | ✅ |
| Spanish | es | 0 (0 %) — PR #143 pending | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Hindi | hi | 0 (0 %) | ❌ | ❌ no Devanagari font or shaping | no | none | 8–15 | ❌ |
| Arabic | ar | 0 (0 %) | ❌ | ❌ no RTL layout, no shaping | no | none | 15–25 | ❌ |
| French | fr | 0 (0 %) — PR #143 pending | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Portuguese | pt | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Indonesian | id | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Japanese | ja | 0 (0 %) | ❌ | ❌ no CJK font fallback, no IME testing | no | none | 6–10 | ✅ |
| German | de | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Korean | ko | 0 (0 %) | ❌ | ❌ no Hangul font fallback | no | none | 6–10 | ✅ |
| Vietnamese | vi | 0 (0 %) | ❌ | 🟡 Latin with stacked diacritics, untested | no | none | 3–5 | ❌ |

Other languages shipped: **1 (Ukrainian)**. (Pro Tools also ships Traditional Chinese, zh-Hant.)

Per-language hours exclude the shared catalog and script work above.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-10 | minor | Added Ukrainian catalog, language selection, English fallback and bundled-font coverage; the Pro Tools language count stays at 1 of 8 |
| 2026-10-10 | major | Created: English only, no catalog; the twelve key languages at 0 %; Pro Tools' 8 languages listed from its bundle |
