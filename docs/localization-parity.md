# Localization parity

> **Last reviewed:** 2026-10-26 · **Last updated:** 2026-10-26 · **Change:** major (French, Spanish and Russian catalog) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft is English only. There is no string catalog: every menu label, dialog and tooltip is a
literal in Rust (`crates/engine/src/cmd/*`, `crates/ui-egui/src/*`). Pro Tools ships its UI in
**8 languages** (bundle `.lproj` folders: English, German, Spanish, French, Japanese, Korean,
Simplified Chinese, Traditional Chinese).

**Localization: ~50 %** (measured: 4 of the target's 8 UI languages — English complete, French,
Spanish and Russian complete in review). Remaining ≈ 70–110 h: a catalog and extraction pass (≈ 15–25 h), a font fallback for
CJK/Devanagari/Arabic (PR #141 open), RTL layout and complex-script shaping (egui has neither:
≈ 25–40 h, or ship Arabic/Hindi menus-only), then ≈ 3–5 h of agent translation per language, plus
native-speaker review (human).

Open work: PR #143 (French, Spanish and Russian, a Language setting — in review), PR #141 (bundled
multilingual font fallbacks), issue #67 (Ukrainian).

| Language | Code | UI strings translated | Dialogs / tooltips / help | Script support | Native review | Status | Est. to full (h) | In Pro Tools |
|---|---|---|---|---|---|---|---:|---|
| English | en | all (source) | ✅ | Latin ✅ | — | **full** | 0 | ✅ |
| Russian | ru | all — PR #143 in review | ✅ | Cyrillic ✅ | no | review | 1–2 (review) | ✅ |
| Simplified Chinese | zh-Hans | 0 (0 %) | ❌ | ❌ no CJK font fallback, no IME testing | no | none | 6–10 | ✅ |
| Spanish | es | all — PR #143 in review | ✅ | Latin ✅ | no | review | 1–2 (review) | ✅ |
| Hindi | hi | 0 (0 %) | ❌ | ❌ no Devanagari font or shaping | no | none | 8–15 | ❌ |
| Arabic | ar | 0 (0 %) | ❌ | ❌ no RTL layout, no shaping | no | none | 15–25 | ❌ |
| French | fr | all — PR #143 in review | ✅ | Latin ✅ | no | review | 1–2 (review) | ✅ |
| Portuguese | pt | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Indonesian | id | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ❌ |
| Japanese | ja | 0 (0 %) | ❌ | ❌ no CJK font fallback, no IME testing | no | none | 6–10 | ✅ |
| German | de | 0 (0 %) | ❌ | Latin ✅ | no | none | 3–5 | ✅ |
| Korean | ko | 0 (0 %) | ❌ | ❌ no Hangul font fallback | no | none | 6–10 | ✅ |
| Vietnamese | vi | 0 (0 %) | ❌ | 🟡 Latin with stacked diacritics, untested | no | none | 3–5 | ❌ |

Other languages shipped: **0**. (Pro Tools also ships Traditional Chinese, zh-Hant.)

Per-language hours exclude the shared catalog and script work above.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-10 | major | Created: English only, no catalog; the twelve key languages at 0 %; Pro Tools' 8 languages listed from its bundle |
| 2026-10-26 | major | PR #143: a string catalog (tr/trf) and a Language setting; French, Spanish and Russian complete (1 441 strings each, machine-translated, native review pending) |
