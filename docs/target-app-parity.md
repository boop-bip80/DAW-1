# Target-app parity: SoundCraft vs Avid Pro Tools

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-10 · **Change:** minor (stage re-normalized to pre-alpha by the core-workflow gate) · **Target:** Avid Pro Tools Ultimate 2026.4.1 (bundle 26.4.1.179, macOS)

The authoritative assessment of how close SoundCraft is to Pro Tools. [`ROADMAP.md`](../ROADMAP.md)
summarizes it, [`gaps.md`](gaps.md) itemizes every shortfall, and the per-dimension docs go deeper:
[`ui-parity.md`](ui-parity.md), [`file-format-parity.md`](file-format-parity.md),
[`hardware-parity.md`](hardware-parity.md), [`plugin-parity.md`](plugin-parity.md),
[`localization-parity.md`](localization-parity.md). The generated menu checklist is
[`parity-checklist.md`](parity-checklist.md).

## Headline

| Number | Value | Kind |
|---|---|---|
| Menu breadth (engine + UI layer) | **482 / 512 menu leaves (94.1 %)** | **measured**: the 512-leaf catalog `crates/engine/catalog/menus.txt` against the command registry; engine alone 397 / 512 (77.5 %) from `cargo xtask parity` ([`parity-checklist.md`](parity-checklist.md)); the 85 UI-layer matches re-counted on 2026-10-10 from `crates/ui-egui/src/menus.rs` (`UI_COMMANDS`, `AUDIOSUITE`, `MENU_WINDOWS`, Mix Window Views) by script |
| Feature breadth (menus plus everything not in a menu: plugins, hardware, formats, immersive, cloud) | **~70 %** | estimated (area table below, presence only) |
| **Ready for real work** | **~40 %** | estimated (dimension table below) |
| Stage | **pre-alpha** (core-workflow gate fails) | see [Stage](#stage) |
| Remaining effort to alpha | **~110–180 Opus 5.5 agent-hours** | estimated: the failing gate rows |
| Remaining effort to beta | **~350–570 Opus 5.5 agent-hours** | estimated, calibrated (below) |
| Remaining effort to full parity | **~950–1,450 Opus 5.5 agent-hours** | estimated, calibrated (below) |

The previous figure in ROADMAP.md (2026-10-07: "~65 % overall feature parity", "110–150 h to 100 %")
measured feature *presence*. This pass measures readiness, and found new evidence that pulls the
numbers down: 37 open GitHub issues from the first public week (about 25 of them correctness bugs
in features counted as done), no MIDI hardware input or output at all, no audio device or buffer
size selection, no localization, and a macOS release that cannot load signed third-party VST3s
(#153). The estimates did not drift up; the scope of what is measured widened.

## How this was measured

- **The target.** `/Applications/Pro Tools.app` on the owner's Mac: Info.plist reports version
  26.4.1.179 (Pro Tools Ultimate 2026.4.1). Only bundle metadata was read (version,
  `CFBundleDocumentTypes` for the file types it opens, the `.lproj` folder names for its UI
  languages: en, de, es, fr, ja, ko, zh-Hans, zh-Hant). Nothing inside the bundle's resources,
  plug-ins or code was read or copied (clean-room rule, AGENTS.md).
- **The menu tree.** `plan/protools/menus-clean.txt` (observed black-box, local only) became the
  committed 512-leaf catalog; `cargo xtask parity` and the UI layer's `engine.parity` score it.
- **Avid's own documentation.** *What's New in Pro Tools 2026.4* (Track Pin, MPEG-H renderer,
  Dolby headphone personalization, speech-to-text upgrades, batch file rename) and earlier
  release notes for features outside menus (Dolby Atmos renderer integration, ADM BWF, Sketch,
  cloud collaboration, Splice, ARA, HEAT, control surfaces, I/O setup).
- **The code on origin/main (3bc2d92, 2026-10-10).** Read and counted with grep: ~70,900 lines of
  Rust; 524 `#[test]` functions plus proptest suites; ~350 macro-registered engine command ids;
  26 built-in processors and 2 instruments in `crates/dsp/src/plugins`; audio I/O in
  `crates/audio-io`; device I/O in `crates/playback` (cpal default output and default input only);
  no MIDI device crate (no `midir`/CoreMIDI anywhere); no string catalog.
- **Users.** The 37 open and 21 closed GitHub issues, and the 42 merged and 29 open pull
  requests up to 2026-10-10, are the only field evidence. Nothing has been benchmarked against
  Pro Tools.
- **Not done:** running Pro Tools side by side on the same material (null tests of mixes, plugin
  responses, fades, elastic audio). That is gap G-01 in [`gaps.md`](gaps.md).

## By dimension

Weights are how much each dimension decides whether a Pro Tools user (music production,
post-production, tracking) can switch. "Ready" is depth, correctness and fidelity, not presence.

| Dimension | Weight | Ready | Kind | Remaining (h) | Doc | Evidence |
|---|---:|---:|---|---:|---|---|
| Features | 45 % | **~45 %** | estimated | 520–810 | [area table](#feature-areas) | Menu breadth 94 % measured, but depth is thin: see the area table |
| UI/UX fidelity | 10 % | **~55 %** | estimated | 60–100 | [ui-parity.md](ui-parity.md) | Edit/Mix windows, every menu, floating windows; open UX bugs #3 #5 #6 #7 #140 #144; shortcuts only recently right on Windows/Linux (#15, #41); generic plugin UIs |
| File formats | 10 % | **~40 %** | estimated | 70–120 | [file-format-parity.md](file-format-parity.md) | WAV/BWF/RF64/AIFF/FLAC read+write, compressed formats read; no `.ptx`, AAF, OMF, ADM BWF, MP3/AAC export, video export |
| Hardware | 10 % | **~12 %** | estimated | 110–180 | [hardware-parity.md](hardware-parity.md) | Default output and default input device only, fixed 512-sample block, no ASIO, no MIDI devices, no control surfaces, no sync |
| Stability | 10 % | **~45 %** | estimated | 50–80 | [gaps.md](gaps.md) | Never-crash rules and proptests (random edits, mutated sessions, hostile params); but ~25 open correctness bugs, a launch failure (#142), X11 llvmpipe failure (#107) |
| Performance | 5 % | **~35 %** | estimated (unmeasured) | 40–70 | [hardware-parity.md](hardware-parity.md) | Parallel strip rendering and PDC exist; whole files decoded into RAM, recording into RAM (#84 open), no track-count or latency benchmarks |
| Ecosystem / plugins | 4 % | **~30 %** | estimated | in Features + Hardware | [plugin-parity.md](plugin-parity.md) | CLAP/VST3/AU hosting; no AAX (Pro Tools' only format), no ARA, no sidechain, no control-surface ecosystem, no content library |
| Platforms | 3 % | **~65 %** | estimated | 20–40 | [ROADMAP.md](../ROADMAP.md) | Builds for macOS, Windows x64/x86/arm64, Linux, FreeBSD, Web (Pro Tools: macOS, Windows), but only macOS is exercised at runtime; Web lacks file pickers (#63, #64) |
| Localization | 2 % | **~12 %** | measured (catalog count) | 70–110 | [localization-parity.md](localization-parity.md) | English only, no string catalog; Pro Tools ships 8 UI languages; FR/ES PR #143 open |
| AI features | 1 % | **~5 %** | estimated | 30–50 | [gaps.md](gaps.md) | Pro Tools has speech-to-text transcription (upgraded in 2026.4); we have only an audio-to-MIDI pitch tracker |
| **Weighted** | 100 % | **~40 %** | | **~950–1,450** (overlaps removed) | | |

Agent control is not weighted (Pro Tools has only its scripting SDK); SoundCraft is ahead there:
every command is reachable from the CLI, the JSON control channel and MCP.

## Feature areas

Weights by what Pro Tools users actually spend time on. **Breadth** = the feature exists;
**Ready** = it works deeply and correctly enough for paid work.

| Area | Weight | Breadth | Ready | Remaining (h) | What's there / what's missing |
|---|---:|---:|---:|---:|---|
| Recording and monitoring | 12 % | 75 % | **40 %** | 40–60 | Punch in/out, QuickPunch-style punch on the fly, pre/post-roll, loop record into playlists, input monitoring through the strip, autosave/recovery. Missing: input/output device choice, buffer size, duplex low-latency path, record straight to disk (#84 open), per-channel hardware input routing, TrackPunch/destructive punch beyond flags, monitoring opt-out (#42) |
| Editing | 15 % | 92 % | **65 %** | 40–60 | Slip/Shuffle/Spot/Grid, every Edit menu command, fades/crossfades, playlists and comping, clip groups, nudge, Tab to transient, separate/heal/consolidate/strip silence. Open bugs: #3, #5 (Multitool handles), #140 (Heal shortcut), #86, #91, #92; no batch fade presets, no field-recorder workflows |
| Mixing and routing | 12 % | 85 % | **60 %** | 35–50 | Faders, pan, sends, busses, aux, masters, VCAs, folders, 10 inserts, PDC, solo modes, I/O Setup. Missing: sidechain inputs, hardware-channel I/O mapping, bus/AFL/PFL paths verified, HEAT; bugs #59, #103 |
| Automation | 7 % | 80 % | **55 %** | 20–30 | Lanes for volume/pan/mute/sends/plugin params, trim, Write/Touch/Latch, glide/thin/coalesce. Missing: surround-pan and object automation, Touch/Latch prime and preview modes, automation on control surfaces; bug #104 |
| Built-in plugins and AudioSuite | 8 % | 40 % | **35 %** | 80–120 | 26 processors + 2 instruments covering every AudioSuite category; Pro Tools bundles roughly three times as many plugins plus virtual instruments. No fidelity comparison, generic parameter UIs (#139). See [plugin-parity.md](plugin-parity.md) |
| Third-party plugin hosting | 8 % | 70 % | **45 %** | 40–60 | CLAP, VST3, AU: audio, params, latency, notes, state, presets, off-thread lifecycle, CLAP floating GUIs, VST3 editors on macOS. Missing: AU editors, VST3 editors on Windows/Linux, sidechain buses, ARA, out-of-process sandboxing, macOS library-validation entitlement (#153, #149). AAX cannot be hosted (Avid SDK licence, PACE signing) |
| MIDI and instruments | 8 % | 70 % | **30 %** | 50–80 | Piano roll, event list, step input, quantize/transpose, real-time properties, SMF import/export. Missing: **any MIDI device input/output** (no keyboard recording, no external synths, no MIDI clock/MTC out), MIDI learn, instrument depth (Sampler PR #108 open), bug #98 |
| Score and notation | 2 % | 60 % | **35 %** | 20–30 | Grand-staff Score Editor, MusicXML export, printable SVG. Missing: notation editing depth (tuplets, articulations, lyrics, parts layout) |
| Elastic Audio, TCE, Beat Detective, pitch | 5 % | 70 % | **35 %** | 40–60 | Pitch-preserving warp on Elastic tracks, TCE, conform to tempo, Beat Detective and Identify Beat, audio-to-MIDI. Missing: per-algorithm choice (polyphonic/rhythmic/monophonic/varispeed/X-Form), warp markers UI depth, Elastic Pitch, quality vs Pro Tools unmeasured |
| Surround and immersive | 5 % | 55 % | **35 %** | 50–80 | Formats stereo → 9.1.6 and Ambisonics, surround panner, fold-down, multichannel bounce, object/bed routing. Missing: Dolby Atmos renderer integration and ADM BWF, MPEG-H (new in 2026.4), binaural/headphone monitoring, surround automation; bug #89 |
| Video and post | 5 % | 60 % | **40 %** | 40–60 | Video track, H.264/ProRes/MJPEG decoders, sync offset, relinking, timecode rulers. Missing: HEVC/DNx/AV1, video bounce/export, timecode sync to external machines, speech-to-text transcript, field-recorder/iXML conform |
| Session, file management, interchange | 10 % | 55 % | **35 %** | 60–100 | `.scraft` + Audio Files, Save Copy In, Import Session Data, clip groups, session text export, audio-health report. Missing: `.ptx` read/write, AAF/OMF (#40), session templates, track presets, workspace/Soundbase browser, disk streaming of long sources, batch rename; bugs #86, #91, #136 |
| Collaboration, cloud, Sketch, Splice | 2 % | 0 % | **0 %** | not estimated | Avid services; out of scope unless the owner decides otherwise |
| Agent control (CLI, control channel, MCP) | 1 % | 100 % | **100 %** | 3 | Ahead of Pro Tools' scripting SDK; one open bug (#145) |
| **Weighted** | 100 % | **~70 %** | **~45 %** | **~520–810** | |

## Stage

**Pre-alpha.** Readiness (~40 %) is at the alpha bar, but the core-workflow gate
([`roadmap.md`](roadmap.md#alpha-gate)) fails three of six Pro Tools workflows on macOS, the main
platform:

- Tracking from a chosen interface at a chosen buffer size: **no**. Only the default devices are
  used, the mix runs in fixed 512-sample blocks, input and output are separate streams, and
  takes are held in RAM.
- MIDI recording: **no**. There is no MIDI device layer.
- Session interchange with Pro Tools or picture editors: **no**. No AAF/OMF/`.ptx`.

Editing, mixing with plugins, bouncing, and saving and reopening pass (with known bugs).

**Distance to alpha: ~5 points of readiness and ~110–180 agent-hours** (G-01, G-02, G-03, part
of G-06, and the #153 entitlement). **Distance to beta: ~35 points and ~350–570 agent-hours**
(beta also needs ~75 % ready). The beta plan is in [`roadmap.md`](roadmap.md).

## Effort and calibration

Hours are **Opus 5.5 agent wall-clock hours**: one agent session working sequentially, including
tests and verification.

**Calibrated against this repo's history** (`git log`, `gh pr list`):

- The 10-05 bulk commits (`first commit` through `Video track, Audio Unit hosting…`, ~70k lines)
  carry batch-commit times, not authoring times, so they don't calibrate.
- The public week (2026-10-07 → 10-10, 42 merged PRs) does. Focused bug fixes with a regression
  test, +20 to +120 lines (#116–#122, #25, #28, #37, #61), took **~0.5–1.5 h** each. Small
  features of +200 to +1,000 lines (#32 light theme +516, #11 log file +981, #8 credits +666, #23
  plugin menus +259, #76 clip-gain fader +198) took **~2–5 h**: about **200–300 tested lines per
  agent-hour**.
- Areas were sized by the code they need at that rate (for example, MIDI device I/O plus MIDI
  recording ≈ 3–5k lines ≈ 12–25 h), plus a 1.3–1.5× tail factor for verification against Pro
  Tools behaviour, as FilmCraft observed for its long tail.

**Parallelism.** About 60–70 % of the work parallelizes across 4–5 agents by crate (dsp plugins,
hardware/playback, file interchange, UI, localization). The audio-thread and hardware work
serializes more, because it needs hands-on testing.

**Needs a human:**
- Owner decisions: AAX (requires Avid's SDK licence and PACE signing; probably never), `.ptx`
  reverse engineering (clean-room implications), Avid cloud features (out of scope by default).
- Hardware we don't have in CI: multichannel interfaces, ASIO drivers, control surfaces (HUI/MCU,
  EUCON is Avid-proprietary), MIDI keyboards, sync generators.
- Native-speaker review for every language beyond English.
- Ears: a mix engineer's A/B of plugins, fades and elastic audio.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-10 | minor | Stage alpha → pre-alpha under the core-workflow gate; alpha distance ~110–180 h |
| 2026-10-10 | major | Full re-measure against Pro Tools Ultimate 2026.4.1: two numbers (breadth ~70 %, ready ~40 %), dimension and area tables with hours, stage alpha; hours re-calibrated from PR history (110–150 h → 950–1,450 h to full parity: the old figure counted presence only) |
| 2026-10-07 | major | Area table and "~65 % overall, 110–150 h" estimate (then in ROADMAP.md) |
