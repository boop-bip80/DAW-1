# Where SoundCraft falls short of Pro Tools

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (separate per-launch idle MIDI thru, safe multi-stem publication, monitoring diagnostics, cumulative OTIO timecode correction and constrained plugin retirement; alpha gate remains) · **Target:** Avid Pro Tools Ultimate 2026.4.1

Every known shortfall, one entry each, ranked by how much it stops a Pro Tools user from doing
real work in SoundCraft. This is the work list: pick from the top. Rows marked **alpha blocker**
fail the core-workflow gate in [`roadmap.md`](roadmap.md#alpha-gate) and keep SoundCraft pre-alpha. Numbers and weights live in
[`target-app-parity.md`](target-app-parity.md); the plan that orders these into milestones is
[`roadmap.md`](roadmap.md). Hours are Opus 5.5 agent-hours (calibration in target-app-parity.md).
Estimates marked "≈" are judgements; issue numbers are `storytold/soundcraft` GitHub issues.

When you close a gap, delete its entry (or shrink it) in the same PR, and bump the timestamp.

## Ranked gaps

| # | Gap | Dimension | Doc | Est. (h) |
|---|---|---|---|---:|
| G-01 | **Alpha blocker.** Bounded audio input routes and manual offset exist; no complete I/O matrix, duplex clock or measured latency compensation | Hardware | [hardware-parity.md](hardware-parity.md) | 15–25 |
| G-02 | **Alpha blocker.** Native MIDI Record/Stop and named per-track input routing pass synthetic tests; live instrument audition, MIDI track output and physical keyboard verification remain | Hardware / MIDI | [hardware-parity.md](hardware-parity.md) | 20–35 |
| G-03 | **Alpha blocker.** Cannot exchange sessions with Pro Tools (no AAF/OMF, no `.ptx`) | File formats | [file-format-parity.md](file-format-parity.md) | 50–90 |
| G-04 | Open correctness bugs in features counted as done (~25 issues) | Stability | this file | 25–40 |
| G-05 | (Entitlement part: alpha blocker for the mix row) Third-party VST3s blocked in the signed macOS build; AU editors missing | Ecosystem | [plugin-parity.md](plugin-parity.md) | 15–25 |
| G-06 | Sources still decoded whole into RAM; active WAV takes already stream to disk, but crash recovery and long-session memory use need work | Performance | [hardware-parity.md](hardware-parity.md) | 15–25 |
| G-07 | Nothing is compared against Pro Tools' output (no fidelity harness) | Features | this file | 20–30 |
| G-08 | Built-in plugin catalogue thin and unverified | Features | [plugin-parity.md](plugin-parity.md) | 80–120 |
| G-09 | English only, no string catalog, no CJK/RTL rendering | Localization | [localization-parity.md](localization-parity.md) | 70–110 |
| G-10 | No control surfaces (HUI/MCU, EUCON) | Hardware | [hardware-parity.md](hardware-parity.md) | 40–70 |
| G-11 | No external sync (MTC, LTC, MIDI clock, video reference) | Hardware | [hardware-parity.md](hardware-parity.md) | 15–25 |
| G-12 | Immersive delivery: no Dolby Atmos/ADM BWF, MPEG-H, binaural | Features | [file-format-parity.md](file-format-parity.md) | 50–80 |
| G-13 | Sidechain inputs, ARA, out-of-process plugin sandboxing | Ecosystem | [plugin-parity.md](plugin-parity.md) | 30–50 |
| G-14 | Linux full gate and headless audio round-trip pass; no GitHub Actions runs or macOS/Windows/hardware runtime evidence | Platforms / Stability | this file | 20–40 |
| G-15 | Elastic Audio depth: one algorithm, no Elastic Pitch | Features | [target-app-parity.md](target-app-parity.md) | 30–45 |
| G-16 | Session templates, track presets, workspace browser, batch rename | Features | [target-app-parity.md](target-app-parity.md) | 25–40 |
| G-17 | Video: HEVC/DNx decode, video export, external timecode | Features / Formats | [file-format-parity.md](file-format-parity.md) | 30–50 |
| G-18 | Speech-to-text transcription | AI | this file | 30–50 |
| G-19 | UI feel: generic plugin windows, shortcut collisions, pro-grade meters and scaling | UI/UX | [ui-parity.md](ui-parity.md) | 60–100 |
| G-20 | MIDI/instrument depth: MIDI learn, notation editing | Features | [target-app-parity.md](target-app-parity.md) | 40–60 |
| G-21 | Compressed export (MP3, AAC/M4A, Ogg/Opus) | File formats | [file-format-parity.md](file-format-parity.md) | 10–20 |
| G-22 | Avid cloud services (collaboration, Sketch, Splice, sign-in) | Ecosystem | — | not estimated (owner decision) |

## Details

### G-01. Audio interface support

- **Done (2026-10-12):** Setup › Playback Engine chooses the audio system, output and input
  device, sample rate and device buffer size (kept with the preferences, per machine; `ui.audio_device`
  for agents); f32 and 16/32-bit integer devices; ASIO behind the opt-in `asio` build feature
  (verified on Windows with an SSL 2+ at 128 samples).
- **Partial (2026-11-01):** The Mix input selector now offers up to 64 physical channels when the device exposes them; recording rejects invalid routes before creating takes, and monitoring silences missing channels instead of wrapping to another microphone. A manual ±500 ms record placement offset is bounded and exposed in Playback Engine; Stop drains in-flight callbacks before WAV finalization. The recorder counts exact affected frames from input-buffer dropouts and warns at capture FIFO pressure; the separate monitor FIFO tracks lock-busy loss, oldest-sample eviction, occupancy and output-silence frames without conflating them with WAV capture loss (its monitoring counters are not yet surfaced in the GUI). The monitor FIFO discards stale samples on resume, and per-stream CPAL timestamps provide diagnostic clock-rate deviation without assuming shared origins. None of this measures actual hardware latency, synchronizes two device clocks or supplies a full physical I/O matrix.
- **Missing:** complete I/O Setup channel mapping, one duplex stream on one clock (input and
  output are still separate cpal streams), measured hardware latency reporting/automatic compensation, ASIO
  in official builds (owner decision: Steinberg's SDK is proprietary or GPLv3), JACK, and
  verification on macOS and Linux. The mix still runs in fixed 512-sample blocks.
  `setup.playback_engine` still stores `device`/`buffer_size` in the session, unused.
- **Evidence:** code above; Playback Engine window says "Mix block size: 512 samples"; issue #42
  (no input device selection, forced monitoring feedback with virtual routing).
- **Impact:** blocks tracking with any multichannel interface; latency makes overdubs hard.
- **Estimate:** ≈ 25–40 h (device enumeration and selection, buffer size, duplex stream with one
  clock, ASIO feature build, per-channel I/O mapping into I/O Setup, latency reporting and
  record-offset compensation). Needs hands-on hardware testing.

### G-02. MIDI devices

- **Partial foundation (2026-11-01):** `soundcraft-midi::live` decodes timestamped MIDI, `soundcraft-midi::device` captures native input through bounded queues, and Setup › Playback Engine selects enabled inputs. Desktop Record/Stop commits tempo-mapped, undoable takes to armed MIDI/instrument tracks. `track.midi_input` persists named ports and all/1–16 channel filters; Mix/Edit expose selectors, with synthetic tests for routing, filtering, preflight and Stop draining. Setup › MIDI › MIDI Input Filter exposes eight saved event kinds (SysEx not decoded). Tracks save explicit external MIDI output ports/channels. A separate per-launch OFF-by-default Playback Engine toggle enables native transport-driven MIDI output through an off-callback scheduler. A **second, independent per-launch OFF-by-default checkbox** now permits idle external MIDI thru to named physical output routes via bounded input workers; transport changes disarm it and wait for prioritized note-off cleanup acknowledgement. Device queues, unplug, saturation and UI consent/status behavior pass synthetic tests without opening hardware on paint.
- **Missing:** **real physical MIDI controller/output and timing validation**, live **software-instrument** audition/monitoring and hardware MIDI clock/MTC. The implemented idle MIDI thru is for external gear, is disabled during playback/record, and has **not** been exercised on physical ports. Audio/device and MIDI clocks have no measured common origin or sample-accurate timing guarantee. Saved routes alone never transmit.
- **Impact:** A hardware-free MIDI recording pathway exists, but it is not verified for musical performance using a controller and instrument.
- **Estimate:** ≈ 20–35 h (device layer, realtime input to armed tracks and instruments with
  timestamps, MIDI record/merge/loop, thru, output tracks, clock out).

### G-03. Session interchange with Pro Tools

- **Missing:** opening or saving `.ptx`/`.ptf`, AAF and OMF import and export (#40), Media
  Composer–compatible sessions. Our session format is `.scraft` JSON.
- **Partial interchange (2026-11-01):** `file.export_otio` and `file.import_otio` write/read eligible audio-only OpenTimelineIO timelines (multiple tracks, gaps, trims, local media and sample-rate conversion). Portable `.otioz` export/import validates deduplicated size-capped media and strict ZIP paths/CRCs; import requires a saved recipient and is undoable, with a public CLI deletion/reopen/rebounce regression. Official Clip.2 fixtures exercise available-range timecode origins (including negative) and literal-percent media names; new tests prove cumulative record positions after 100 one-frame 29.97 fps gaps land at **160,160** 48 kHz samples rather than 160,200, while relative media origins still round correctly. Processing/video/unsafe structures still fail closed. CMX3600 `file.export_edl` / `file.import_edl` cover mono `A`, linked stereo `AA` or two independently edited mono `A1`/`A2` lanes with frame-exact straight cuts, not arbitrary multitrack/fades/mixer processing. MIDI and EDL exports refuse unrequested overwrites. None substitutes for AAF, OMF or `.ptx`.
- **Impact:** the standard's main-format bar for beta. A Pro Tools user cannot bring a project in
  or hand one back; post-production (picture editors deliver AAF) is blocked.
- **Estimate:** AAF/OMF import + export ≈ 50–90 h (FilmCraft already writes AAF/OMF; we can learn
  from it but not share code). `.ptx` is proprietary and undocumented: owner decision on whether
  black-box format study is in the clean-room rules; not included in the estimate.

### G-04. Correctness bugs in "done" features

**Fixed (2026-11-01):** Recording validates every selected input route before WAV creation; monitor channels no longer wrap to unavailable microphones. CLAP/VST3 plugin scans do not guess among ambiguous bundle executables. A CLI test covers import → bounce → save → reopen → bounce, surviving removal of the original import path.

**Further narrowed (2026-11-01, PRs #103–#116):** Mix and stem bounce staging files now use exclusive ownership so preexisting `.part` files/symlinks survive failed exports. Native multi-stem bounces preflight all outputs, render into a private same-volume directory and publish via no-replace links, rolling back only files just published when a recoverable later publication fails. This is **not crash-atomic** across multiple files and cannot guarantee outcomes under hostile concurrent pathname swaps. Recording reports sub-millisecond capture loss and warning pressure; monitor-only loss is separately counted. Silent-clock native plugin state is joined and destroyed on the UI/control thread, and hardware retired-mailbox receiver teardown order is protected; **active hardware CPAL callbacks can still destroy plugin-bearing state on a render worker**, so fully safe native plugin shutdown/restart remains open. Project-save backup pruning on failed `.scraft.tmp` staging is separately under investigation.

**Fixed (2026-10-31):** A panicked recording writer is reported rather than silently treated as an empty successful take. Recording input startup retries the driver's default buffer when stream build or play fails. CLAP and VST3 note-off queues preserve releases under overload and after transport stop. Exact-modifier shortcuts avoid stealing unrelated keys; command-palette changes reset selection to avoid invoking stale results. These are focused regressions, not closure of the broader G-04 backlog.

**Fixed (2026-10-19, #791):** Float32 WAV and AIFF-C encoding retain finite samples above full scale. Whole-file and streaming tests cover both formats, integer clipping, and the existing NaN/infinity behavior. A bounce regression checks preserved headroom and file/report agreement with normalization off, then checks normalized output.

**Fixed (2026-10-18):** Mono device playback averages the stereo channels before output clipping,
**Fixed (2026-10-16):** Master fader volume automation uses the first and last samples of each
**Fixed (2026-10-16):** Aux, folder and master latency calculations now skip inserts already
render block, matching track faders. A breakpoint at the next block no longer ramps the preceding
block, and ramps reach their last-sample value. Regression tests cover nonzero render starts,
one-sample callbacks and several block sizes. Interior automation breakpoints remain block-dependent.

disabled for producing NaN or infinity. Subsequent blocks compensate only for healthy inserts;
regression tests verify rendered impulse timing, retained healthy latency and reset/retry at
64- and 512-sample blocks, with audio tracks as the control.

instead of discarding the right side. Surround mixes average both sides of the ITU stereo
fold-down. Regression tests cover right-only and left-only audio, equal and opposite-polarity
channels, clipping order, centered mono tracks, each 5.1 channel and 1-, 37- and 512-frame callbacks.
**Fixed (2026-10-16):** Split into Mono now gives each output track its own source channel,
including surround channels, instead of repeating channel zero. Clip edits, source timestamps,
Undo/Redo and save/reopen are covered by regression tests; missing or empty media fails atomically.

**Open (2026-10-25):** the Clip Looping dialog (#1030) has no option to crossfade the loops into
each other, and its Number of Loops field stops at 512, while Loop Length and Loop Until End of
Session or Next Clip make up to 1000 loops.

Open on 2026-10-10 (several have PRs open):
editing #3, #5, #140, #144; clip/session #86 (PR #123), #88 (PR #130), #91 (PR #124), #92 (PR #132);
automation #104 (PR #134); mix/bounce #59 (PR #131), #103 (PR #133), #152; playback #101 (PR #129);
MIDI #87, #98 (PR #125); import #89 (PR #128); CLI #95, #96 (PR #126, #127); MCP #145 (PR #146);
web #63, #64, #136; Clip List #4 (PR #75); plugin view #6; shortcuts #7, #41;
launch #142; Linux #45, #107.
- **Impact:** each one is small, but together they say "presence is not parity": an outside
  contributor found seven in one sitting (#116–#122).
- **Estimate:** ≈ 25–40 h at 0.5–1.5 h each, plus review of the open PRs.

### G-05. Plugin hosting on macOS

- **Missing:** the `com.apple.security.cs.disable-library-validation` entitlement in the signed
  build, so hardened runtime refuses third-party VST3 bundles (#153, #149). AU editor views
  (`crates/au-host/src/lib.rs` header: "Not hosted yet: Audio Unit editor views"). Embedded CLAP
  GUIs; VST3 editors on Windows/Linux.
- **Estimate:** ≈ 15–25 h (entitlement ≈ 1 h; AU Cocoa views and VST3 HWND/X11 views most of it).

### G-06. Memory-bound media

- **Missing:** disk streaming for decoded playback sources and stronger crash/recovery handling. Every
  source is decoded whole into the `SourcePool`, which raises long-session memory consumption.
- **Already present:** the recorder writes active takes into WAV files on a background thread and
  refreshes headers periodically; this is not an in-RAM-only take recorder. Writer-thread failure
  detection was tightened on 2026-10-31, but power-loss recovery still needs end-to-end testing.
- **Impact:** long multitrack playback sessions can exhaust memory; recovery of interrupted takes
  and recording behavior under disk or device failure still require verification.
- **Estimate:** ≈ 15–25 h.

### G-07. Fidelity harness against Pro Tools

- **Missing:** any measured comparison: null tests of bounces of the same material, fade curve
  and pan-law checks, plugin frequency/dynamics responses, elastic-audio quality, automation
  timing. Pro Tools is installed; it can be driven black-box with synthetic sessions (outputs kept
  under `plan/`, never committed).
- **Estimate:** ≈ 20–30 h for the harness and the first 20 scored cases; turns many "≈" in
  target-app-parity.md into measurements.

### G-08. Built-in plugins

See [`plugin-parity.md`](plugin-parity.md): 27 processors + 3 instruments against Pro Tools'
roughly 80 bundled processors and instruments; no fidelity checks; generic parameter UIs; EQ
parametric request #139. ≈ 80–120 h.

### G-09. Localization

See [`localization-parity.md`](localization-parity.md): no catalog, every string literal in code;
egui has no complex-script shaping or RTL; no CJK font fallback (PR #141 open). ≈ 70–110 h plus
native-speaker review.

### G-10. Control surfaces

None: no HUI or Mackie Control, no EUCON (Avid-proprietary, needs Avid's SDK: owner decision),
no generic MIDI controller mapping. Pro Tools users on S1/S3/S4/S6/Dock or HUI devices can't
switch. ≈ 40–70 h for HUI/MCU plus a mapping layer; hardware needed to verify.

### G-11. Sync

No MTC/LTC chase or generation, no MIDI Beat Clock (`setup.midi_beat_clock` stores a flag only),
no video reference or word clock awareness, no Satellite-style linking. ≈ 15–25 h after G-01/G-02.

### G-12. Immersive delivery

No Dolby Atmos renderer integration, ADM BWF import/export, re-renders, binaural monitoring,
MPEG-H (new in Pro Tools 2026.4), surround/object automation. ≈ 50–80 h; Dolby's renderer and
SDKs are licensed, so we'd write our own ADM and object renderer.

### G-13. Plugin ecosystem depth

No sidechain (key input) buses for any host; no ARA (Melodyne-style editing is a menu in Pro
Tools); plugins run in-process, so a crashing plugin can take the session down. ≈ 30–50 h. AAX
cannot be hosted (#109): Avid's AAX SDK licence and PACE signing; owner decision, likely never.

### G-14. Platforms and CI

The default-branch and PR CI workflows target this fork's active branch and request
`cargo xtask ci` on Linux, macOS and Windows. Release packaging now has a `quality` dependency
that must pass the same three-OS gate before the artifact workflow starts (configuration only). On 2026-11-01 a local Linux
`cargo xtask ci` run passed fmt, strict Clippy, workspace tests, doctests, assets, layers and
WebAssembly, with an additional headless CLI media round-trip regression. However, GitHub still
reports **zero workflow runs**, so runner enablement/permissions and three-OS results remain unverified. Runtime
smoke tests are also outstanding; Windows/Linux startup reports include #41, #45, #107 and #142.
≈ 20–40 h for runner verification, smoke tests and any platform fixes.

### G-15. Elastic Audio depth

One warp algorithm; Pro Tools offers several per track (polyphonic, rhythmic, monophonic,
varispeed, high-quality offline) plus Elastic Pitch and warp-marker editing depth. ≈ 30–45 h.

### G-16. Session management

Session templates, track-preset recall UI and browser, workspace/sound browser with previews,
batch rename (new in 2026.4), disk-usage and relink depth remain. Track-preset saves capture the
current hosted-plugin state before writing (#661); recall UI, partial recall and preset browsing remain
tracked in #639. ≈ 25–40 h.

### G-17. Video and post

No HEVC, DNxHD/HR or AV1 decode; no bounce with video; no external timecode/9-pin; no iXML
field-recorder conform. ≈ 30–50 h.

### G-18. Speech-to-text

Pro Tools transcribes dialogue to a searchable transcript (upgraded in 2026.4). We have a
settings command only. Needs an openly licensed local speech model. ≈ 30–50 h.

### G-19. UI feel

See [`ui-parity.md`](ui-parity.md). ≈ 60–100 h.

### G-20. MIDI and instrument depth

MIDI learn, drum machine depth, notation editing (tuplets, lyrics,
articulations, parts). ≈ 40–60 h.

### G-21. Compressed export

Bounce writes WAV/BWF/RF64, AIFF and FLAC; Pro Tools also writes MP3 and AAC/M4A in Bounce Mix.
≈ 10–20 h (pure-Rust encoders).

### G-22. Avid cloud services

Collaboration, cloud projects, Sketch, Splice, Learn, sign-in: 11 of the 30 menu leaves still
missing. Out of scope unless the owner decides otherwise.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | PRs #103–#116: exact NTSC OTIO boundary rounding and media-relative trims; multi-stem staging with no-replace publication/rollback; independent live monitoring and WAV capture-loss telemetry; separate opt-in idle external MIDI thru and UI with fake-port ACK safety; mixer and Paste Special reachability; narrower silent-clock/hardware retired-plugin ownership. Local Linux full CI and pushed source-tree identity verified, but physical controllers, one-clock duplex, active hardware-plugin teardown and AAF/OMF/PTX remain alpha blockers |
| 2026-11-01 | minor | Native per-launch opt-in MIDI transport output/UI tested with fake ports (not hardware), official OTIO signed timecode and literal-% names supported, MIDI/EDL no-clobber and stem-collision guards, failed-WAV recovery notices and CLAP observation added; alpha gates/readiness unchanged |
| 2026-11-01 | minor | Portable audio-only OTIOZ import/export (saved recipient and hostile-archive guards), opt-in MIDI output routes with cancellation-safe worker but no playback, independent CPAL rate diagnostics and CLI OTIOZ/SMF tests; alpha blockers unchanged |
| 2026-11-01 | minor | OTIOZ media-inclusive audio export-only, MIDI Input Filter GUI and device-level output worker documented; release requires three-OS quality before packaging but still has zero hosted run evidence; alpha blockers unchanged |
| 2026-11-01 | minor | Per-track named MIDI port/channel input routing is saved, exposed in Mix/Edit and synthetically tested; independent mono A1/A2 CMX3600 EDL cuts now interchange; physical keyboard, AAF/OMF/PTX and alpha blockers remain |
| 2026-11-01 | minor | Desktop MIDI recording integrated for synthetic tests, manual bounded audio record offset, mono/stereo straight-cut CMX3600 EDL and full callback drain; still no hardware or AAF/OMF/PTX validation |
| 2026-11-01 | minor | Added OTIO audio import, MIDI synthetic take bridge, bounded hardware input routing, safe plugin scans and Linux smoke/full-gate evidence; no hardware or GitHub Actions evidence, alpha gaps remain |
| 2026-10-31 | minor | Documented bounded native MIDI input capture and its missing routing; OTIO audio export; record fallback, plugin and palette safeguards; CI trigger without runner evidence. Alpha gaps remain |
| 2026-10-25 | trivial | G-04: Clip Looping (#1030) lacks the loop crossfade option, and Number of Loops stops at 512 while length and fill make up to 1000 loops |
| 2026-10-24 | trivial | G-08: 27 processors + 3 instruments, as the plugin registry holds; G-20: the sampler shipped (#108), so it is no longer listed as missing |
| 2026-10-19 | trivial | G-04: Float32 encoding retains finite headroom (#791); integer clipping and non-finite handling retained |
| 2026-10-18 | trivial | G-04: mono device stereo/surround fold-down fixed and regression-tested; estimates unchanged |
| 2026-10-16 | trivial | G-04: master volume automation block boundaries fixed and regression-tested; estimates unchanged |
| 2026-10-16 | trivial | G-04: failed aux/folder/master inserts excluded from latency compensation after containment; estimates unchanged |
| 2026-10-16 | trivial | G-04: Split into Mono channel preservation fixed and regression-tested; estimates unchanged |
| 2026-10-16 | minor | G-16: capture current hosted-plugin state before saving a track preset (#661); broader preset work remains (#639) |
| 2026-10-12 | minor | G-01 shrunk: audio device, rate and buffer choice; opt-in ASIO |
| 2026-10-10 | minor | Marked the alpha blockers (G-01, G-02, G-03; parts of G-05 and G-06) from the core-workflow gate |
| 2026-10-10 | major | Created from the full re-measure against Pro Tools Ultimate 2026.4.1: 22 ranked gaps with evidence and estimates |
