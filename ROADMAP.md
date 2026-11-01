# SoundCraft roadmap

**Stage: pre-alpha** · next: alpha, ~5% and ~110–180 h away (the alpha gate's three failing workflows)

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (per-track MIDI input routes and independent A1/A2 EDL cuts; alpha blockers remain) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft is a clean-room, pure-Rust digital audio workstation that aims at full parity with
Avid Pro Tools, and then beyond it: faster, open, scriptable and agent-controllable. This page is
the one-page summary; the detail is in [`docs/target-app-parity.md`](docs/target-app-parity.md)
(assessment), [`docs/gaps.md`](docs/gaps.md) (ranked work list), [`docs/roadmap.md`](docs/roadmap.md)
(milestones and beta plan) and [`docs/parity-checklist.md`](docs/parity-checklist.md) (generated
menu checklist).

## Headline numbers

| Number | Value | Kind |
|---|---|---|
| Menu breadth | **482 / 512 Pro Tools menu leaves (94.1 %)**; engine alone 397 / 512 | measured (`cargo xtask parity` + UI-layer aliases) |
| Feature breadth | **~70 %** | estimated (menus plus plugins, hardware, formats, immersive, cloud) |
| **Ready for real work** | **~40 %** | estimated, weighted by dimension |
| Mainstream practitioner (tracking/mixing engineer) | **~36 %** | estimated: 50.6 % weekly-area depth × 0.90 interaction × 0.92 stability × 0.85 file exchange ([how](docs/target-app-parity.md#mainstream-practitioner-and-essentials-user)) |
| Essentials user (song or podcast, defaults) | **~57 %** | estimated: 72.4 % core-feature depth × 0.90 launch/stability × 0.90 discoverability × 0.97 files ([how](docs/target-app-parity.md#mainstream-practitioner-and-essentials-user)) |
| Remaining to alpha | **~110–180 Opus 5.5 agent-hours** (the failing gate rows) | estimated, calibrated on PR history |
| Remaining to beta | **~350–570 Opus 5.5 agent-hours** (includes the alpha work; ~60–70 % parallelizable) | estimated |
| Remaining to full parity | **~950–1,450 Opus 5.5 agent-hours** | estimated |

| Audience | Ready % | Agent-hours to ~95 % | Work that dominates |
|---|---:|---:|---|
| Full target (ready for real work) | ~40 % | ~950–1,450 h | Hardware, the stock plugin catalogue, AAF/OMF and immersive, localization, fidelity |
| Mainstream practitioner | ~36 % | ~380–600 h | Interface and MIDI recording, AAF/OMF, stock-plugin depth, core-path bugs |
| Essentials user | ~57 % | ~110–200 h | Launch and platform fixes, onboarding, effect panels and presets, MP3/AAC export |

Hours are Opus 5.5 agent wall-clock hours, calibrated on this repo's PRs; 60–70 % parallelizes.
Detail: [target-app-parity.md](docs/target-app-parity.md#readiness-by-audience).

**Why pre-alpha:** the core-workflow gate in [docs/roadmap.md](docs/roadmap.md#alpha-gate) fails.
Editing, mixing, bouncing, and saving and reopening work end to end. Audio device, rate and
buffer selection exist, WAV takes stream to disk, and per-track input selections are bounded
rather than silently wrapping to the wrong mic. A single-clock duplex path, complete I/O matrix
and measured latency compensation remain incomplete; a bounded *manual* record offset exists.
Native MIDI input capture code and an undoable armed-track take bridge are connected to desktop
Record/Stop. Saved, named per-track input ports and all/1–16 channel filters are selectable in
Mix/Edit and pass synthetic tests; physical keyboard capture and live instrument audition remain
unverified. Audio-only OpenTimelineIO and restricted mono `A`, linked-stereo `AA`, and two
independently edited mono `A1`/`A2` CMX3600 EDL lanes work; AAF, OMF and `.ptx` interchange
with Pro Tools remain absent.
Readiness (~40 %) sits right at the alpha bar, but a gate failure keeps the app pre-alpha. The
2026-10-07 estimate ("~65 % overall, 110–150 h to 100 %") counted feature presence. This one
measures readiness and adds hardware, localization and field bug reports.

## By dimension

| Dimension | Ready | Remaining (h) | Doc |
|---|---:|---:|---|
| Features | ~45 % | 520–810 | [target-app-parity.md](docs/target-app-parity.md#feature-areas) |
| UI/UX fidelity | ~55 % | 60–100 | [ui-parity.md](docs/ui-parity.md) |
| File formats | ~40 % | 70–120 | [file-format-parity.md](docs/file-format-parity.md) |
| Hardware (interfaces, MIDI, surfaces, sync) | ~12 % | 110–180 | [hardware-parity.md](docs/hardware-parity.md) |
| Localization | ~12 % (1 of Pro Tools' 8 languages) | 70–110 | [localization-parity.md](docs/localization-parity.md) |
| Performance | ~35 % (unmeasured) | 40–70 | [hardware-parity.md](docs/hardware-parity.md#performance) |
| Stability | ~45 % | 50–80 | [gaps.md](docs/gaps.md) |
| Platforms | ~65 % (5 platforms built, macOS exercised) | 20–40 | [gaps.md](docs/gaps.md) |
| Ecosystem / plugins | ~30 % | in Features + Hardware | [plugin-parity.md](docs/plugin-parity.md) |
| AI features | ~5 % | 30–50 | [gaps.md](docs/gaps.md) |
| Agent control | ahead of Pro Tools | 3 | [control-protocol.md](docs/control-protocol.md), [mcp.md](docs/mcp.md) |

## Features

| Area | Breadth | Ready | Remaining (h) |
|---|---:|---:|---:|
| Recording and monitoring | 75 % | 40 % | 40–60 |
| Editing | 92 % | 65 % | 40–60 |
| Mixing and routing | 85 % | 60 % | 35–50 |
| Automation | 80 % | 55 % | 20–30 |
| Built-in plugins and AudioSuite | 40 % | 35 % | 80–120 |
| Third-party plugin hosting | 70 % | 45 % | 40–60 |
| MIDI and instruments | 70 % | 30 % | 50–80 |
| Score and notation | 60 % | 35 % | 20–30 |
| Elastic Audio, TCE, Beat Detective | 70 % | 35 % | 40–60 |
| Surround and immersive | 55 % | 35 % | 50–80 |
| Video and post | 60 % | 40 % | 40–60 |
| Session, file management, interchange | 55 % | 35 % | 60–100 |
| Collaboration and cloud (Avid services) | 0 % | 0 % | owner decision |
| Agent control | 100 % | 100 % | 3 |

Weights and evidence per row: [target-app-parity.md](docs/target-app-parity.md#feature-areas).

## Languages

| Language | Code | Status | Translated |
|---|---|---|---:|
| English | en | full | 100 % |
| Simplified Chinese | zh-Hans | none | 0 % |
| Spanish | es | none (PR #143 open) | 0 % |
| Hindi | hi | none | 0 % |
| Arabic | ar | none | 0 % |
| French | fr | none (PR #143 open) | 0 % |
| Portuguese | pt | none | 0 % |
| Indonesian | id | none | 0 % |
| Japanese | ja | none | 0 % |
| German | de | none | 0 % |
| Korean | ko | none | 0 % |
| Vietnamese | vi | none | 0 % |

Other languages shipped: 0. Detail: [localization-parity.md](docs/localization-parity.md).

## Upcoming

Ranked; the alpha gate, the alpha checklist and the beta plan are in [docs/roadmap.md](docs/roadmap.md).

1. **Alpha blocker:** audio interfaces: full physical-channel I/O matrix, one-clock duplex monitoring, measured latency compensation and hardware testing (G-01); manual record offset, per-track input bounds and take-to-disk now work, source streaming/recovery remain separate (G-06). Estimated gate 35–55 h, not re-calibrated.
2. **Alpha blocker:** verify native MIDI Record/Stop and per-track port/channel input routing with a physical keyboard, then add live instrument playback, MIDI output, thru and clock (G-02). 20–35 h, not re-calibrated.
3. **Alpha blocker:** AAF/OMF import and export (G-03). 50–90 h.
4. Obtain explicit owner approval for macOS third-party native-plugin library-validation entitlement (#153), then AU editors (G-05). 15–25 h plus the security decision.
5. Land the open bug-fix PRs and clear the correctness backlog (G-04). 15–25 h.
6. Enable and verify the three-OS PR CI in this fork; GitHub still reports no Actions runs. A headless audio import/bounce/save/reopen CLI smoke test now passes on Linux; macOS/Windows startup and hardware checks remain (G-14).
7. Fidelity harness against Pro Tools (G-07). 20–30 h.

## Robustness

Property tests drive random edit sequences (with full undo), mutated session files and hostile
command parameters through the engine and mixer; they have caught fade, overflow and
unbounded-allocation bugs. Every command also runs with empty parameters on empty and demo
sessions. Hosted plugins live in isolated unsafe crates (`clap-host`, `vst3-host`, `au-host`) and
are created, loaded and destroyed off the audio thread. The video decoders survive hundreds of
mutated and truncated movies. Field reports from the first public week are tracked in
[gaps.md](docs/gaps.md) (G-04).

## Progress log

- **2026-11-01:** Named per-track MIDI input ports and channel filters are saved and selectable in Mix/Edit, with synthetic routing and fail-closed port-preflight tests. CMX3600 additionally supports two independently edited mono `A1`/`A2` lanes with overlapping record times across lanes and strict processing validation; this is not arbitrary multitrack interchange. Hardware controller/live audition and AAF/OMF/PTX remain unverified or absent; the alpha gate and estimates are unchanged.
- **2026-11-01:** A third integration slice connects desktop MIDI Record/Stop to armed-track undoable takes in synthetic tests, adds audio Stop callback draining, manual ±500 ms recording offset, and restricted frame-exact CMX3600 mono `A`/linked stereo `AA` EDL import/export. A public CLI stereo EDL waveform roundtrip passes. The manual offset is not a measured duplex correction, live instrument audition and physical keyboard capture remain unverified, and EDL does not replace AAF/OMF/PTX. Readiness and alpha gate unchanged.
- **2026-11-01:** A second integration slice adds an audio-only OpenTimelineIO importer (official Clip.2 fixture, media/source bounds, atomic undo, File menu path), native MIDI device selection in Setup, and a transport-aware capture-to-armed-track take bridge with synthetic tests. Multichannel audio recording now refuses invalid physical input routes before WAV creation and silences unavailable monitor channels. CLAP/VST3 scans fail closed on ambiguous bundle executables. A hardware-free CLI import→bounce→save→reopen→bounce test passes. The complete Linux `cargo xtask ci` gate, including WebAssembly, passed on the exact GitHub tree at that point; there are still no GitHub Actions runs and no new hardware/macOS/Windows validation. Readiness and alpha gate unchanged.
- **2026-10-31:** First stabilization/integration slice: direct-to-disk recording reports writer-thread failures and retries input stream startup with the device-default buffer when needed. CLAP/VST3 note releases survive saturated queues and transport stops. MIDI 1.0 decoding/take assembly and a native bounded MIDI input-port capture adapter are available, but not wired to instruments/armed tracks. `file.export_otio` exports eligible multitrack audio timelines through the desktop File menu, compatible with the official OTIO reader. Exact keyboard shortcuts and command-palette selection handling prevent unintended commands. Three-OS CI configuration now targets this fork's default branch; GitHub runner and macOS/Windows/hardware checks remain outstanding. No readiness percentage change.
- **2026-10-25:** Auto Pan moves the balance of a stereo track: the left channel's level falls as the position moves right and the right channel's as it moves left, at constant power, and each channel stays on its own side. At the center the track passes unchanged, so a wide or out-of-phase stereo image keeps its width, and the envelope follower follows the louder channel. No readiness percentage change.
- **2026-10-24:** Clip List › Auto Rename (`clip.auto_rename`) sets the names that editing gives the clips it separates from the selected clips: a name, a starting number, a number of places (zeros before the number) and a suffix. The piece after each cut takes the next name, the piece before it keeps its name, and all the pieces count up one number sequence. Pieces that Strip Silence names with its own Rename fields, or removes, take no Auto Rename name. Copied clips keep their scheme when they are pasted, saved sessions hold the schemes, and Import Session Data brings them along. No readiness percentage change.
- **2026-10-24:** While the Strip Silence window is open, the Edit window draws a rectangle around each range that Strip would keep on the selected tracks, padded by Clip Start Pad and Clip End Pad, and the rectangles follow the sliders as they move. They are removed when the window closes. `edit.strip_silence_preview` returns the same ranges without changing the session. No readiness percentage change.
- **2026-10-24:** The Strip Silence window has a Rename button. It opens Rename Selected Clips, where a name, a starting number, a count of zeros and a suffix set the names of the clips that Strip, Extract and Separate leave in the selection (for example SFX023.Reel1, SFX024.Reel1). The app keeps the scheme for later uses of Strip Silence until the app quits, or until Clear and then OK empty it. No readiness percentage change.
- **2026-10-24:** The Strip Silence window has Extract and Separate buttons. Extract removes the audio and keeps the silent parts of the selection, and Separate separates the clips at the edges of the silence without removing anything. Minimum Strip Duration goes up to 4,000 ms. No readiness percentage change.
- **2026-10-24:** Clip › Loop… (Cmd+Alt+L) opens the Clip Looping dialog: Number of Loops, Loop Length in the main counter's time format, or Loop Until End of Session or Next Clip. A loop length that is not a whole number of loops cuts the last loop short, and Unloop removes that short loop too. No readiness percentage change.
- **2026-10-24:** AudioSuite ▸ Pitch Shift ▸ Vari-Fi slows the selected clips down to a complete stop or speeds them up from one, and the pitch follows the speed. Fit To keeps each clip's length and plays two-thirds of it; Extend plays all of it and makes the clip 150 % long. Fades On fades the audio out with the slowdown or in with the speedup. Before, the menu item resampled the clip at a constant speed. No readiness percentage change.
- **2026-10-24:** A built-in Auto Pan plugin pans the mono sum of a stereo track between left and right. An LFO with seven waveforms moves it at a rate in BPM or at the session tempo, running free, once, in step with the transport, or when the level crosses a threshold or a manual trigger changes; an envelope follower can move it by level instead. Width, Manual and Path set the field, and Output sets the level. No readiness percentage change.
- **2026-10-24:** A built-in Subharmonic (`subharmonic`) adds the octave below a lower and an upper band of the input, chosen from four frequency ranges, with a direct bass band, a gain and a solo for each band, Drive with six characters, HP and LP filters, and a Mix that keeps one path at full level. No readiness percentage change.
- **2026-10-23:** A built-in Ring Modulator (`ring_mod`) applies ring modulation, frequency modulation or a positive or negative resonator, and an LFO, an envelope follower or sample and hold moves its frequency. AudioSuite › Modulation › Sci-Fi now runs it instead of the Flanger. No readiness percentage change.
- **2026-10-19:** Float32 WAV and AIFF-C encoding preserve finite samples above full scale. Integer PCM still clips at full scale. Tests cover whole-file and streaming encoders, non-finite samples, and bounce output with normalization on and off. No readiness percentage change.
- **2026-10-18:** Mono audio devices receive the average of the stereo channels before output clipping, retaining right-only audio. Surround mixes use the same average after their ITU stereo fold-down. Regression tests cover channel balance, polarity cancellation, clipping, centered mono tracks, each 5.1 channel and several callback sizes. No readiness percentage change.
- **2026-10-16:** Master fader volume automation uses each render block's actual last sample, matching track faders. Breakpoints in the next block no longer pull the preceding block's level toward them, and ramps reach their last-sample value. Regression tests cover nonzero starts, one-sample callbacks and several block sizes. No readiness percentage change.
- **2026-10-16:** Aux, folder and master latency calculations exclude inserts already taken out for producing NaN or infinity, matching audio tracks. Subsequent blocks no longer compensate for an insert that is bypassed after failure. Regression tests check impulse timing, healthy insert latency and reset/retry at 64- and 512-sample blocks. No readiness percentage change.
- **2026-10-16:** Split into Mono extracts the correct source channel for each mono track instead of playing channel zero on every track. Clip offsets, gain, fades and stretch, source timestamps, Undo/Redo and saved audio are preserved; repeated clips share each extracted mono source. Unavailable media returns an error without partial edits. No readiness percentage change.
- **2026-10-14:** MCP `batch` rejects more than 1000 calls before executing any command, instead of silently running only the first 1000 (#460). No readiness percentage change.
- **2026-10-14:** `soundcraft-cli convert` dithers only when the output loses resolution (fewer bits than the source, a float or lossy source, or resampled/normalized samples); a same-depth, wider or 32-bit float conversion copies the samples exactly and digital silence stays silent. `--dither` and `--no-dither` override the choice (#548)
- **2026-10-13:** WAV: an empty data chunk followed by metadata (LIST, bext) reads as no audio; a recording that was never finalised (sizes left at 0) still gives what was written (#466)
- **2026-10-13:** Sampler: a ping-pong loop keeps its phase and direction when a step is wider than the loop (a narrow loop with high transposition), instead of reflecting once and dropping the rest of the travel (#463)
- **2026-10-13 (VST3 host messages):** The VST3 host creates `IMessage` and `IAttributeList` objects for plugins (`IHostApplication::createInstance`), so plugins whose controller and processor talk through `IConnectionPoint::notify` work: Line 6 Helix Stadium Native now loads its content database (it reported "Failed to get content database recovery info (Out of memory condition)") and exposes its parameters. No readiness percentage change.
- **2026-10-13:** `mix.instrument` takes the built-in instrument's parameters in `params` (validated, clamped) and refuses any other field, so a synth setting written beside `plugin` is an error instead of being accepted and ignored (#265)
- **2026-10-13:** A bounce of a bus that an aux returns onto lines up with the timeline: the aux insert's lookahead is compensated like the main mix's (it left the bus late and cut the end off), and tracks routed to that bus wait for it too (#246)
- **2026-10-13:** Delay compensation covers the inserts of the master faders: a bounce of the main mix with a Maximizer on the master lines up with the same mix without it (it came out 71 samples late) (#351)
- **2026-10-12 (audio device settings):** Setup › Playback Engine chooses the audio system, output and input device, sample rate and buffer size, kept per machine with the preferences; 16/32-bit integer devices are converted; ASIO is available behind the opt-in `asio` build feature (verified with an SSL 2+ at 128 samples). Channel routing, one duplex clock and latency compensation remain (G-01). No readiness percentage change.
- **2026-10-12 (Mix in a separate window):** Window › Mix in Separate Window shows the Mix window in its own native window, so Edit and Mix can be on screen at once (e.g. one per monitor), as in Pro Tools. No readiness percentage change.
- **2026-10-12 (plugin editors on Windows):** VST3 editors and embedded CLAP editors open in a SoundCraft host window on Windows (owned by the app window, resized on request, closed from its title bar); verified with a JUCE VST3 compressor and a CLAP effect. No readiness percentage change.
- **2026-10-12:** `soundcraft-cli convert` rejects an unparsable or zero `--rate` and an unknown `--bit-depth` (exit 1, nothing written) instead of converting with a default (#256)
- **2026-10-12:** Timecode labels with huge fields no longer overflow: frame counts are computed in 128 bits and clamp, so `setup.current_timecode` and `session.format_time` saturate instead of failing (#248)
- **2026-10-12:** `session.format_time` with the smallest bar number no longer fails with an internal error: the bar offset saturates (#249)
- **2026-10-12 (plugin editors on Windows):** VST3 editors and embedded CLAP editors open in a SoundCraft host window on Windows (owned by the app window, resized on request, closed from its title bar); verified with a JUCE VST3 compressor and a CLAP effect. No readiness percentage change.
- **2026-10-12 (Mix in a separate window):** Window › Mix in Separate Window shows the Mix window in its own native window, so Edit and Mix can be on screen at once (e.g. one per monitor), as in Pro Tools. No readiness percentage change.
- **2026-10-12 (audio device settings):** Setup › Playback Engine chooses the audio system, output and input device, sample rate and buffer size, kept per machine with the preferences; 16/32-bit integer devices are converted; ASIO is available behind the opt-in `asio` build feature (verified with an SSL 2+ at 128 samples). Channel routing, one duplex clock and latency compensation remain (G-01). No readiness percentage change.
- **2026-10-10 (UI docking):** Tracks and Groups, Clips, MIDI Editor, Memory Locations, and Undo History use movable panels. Edit and Mix retain separate arrangements; Clips and MIDI Editor belong to Edit. Transport, plug-in, and specialist utility windows retain their existing hosts. Panel moves and reset are available as UI commands, and saved UI preferences retain arrangements. Common toolbar and track record-enable buttons share interaction, accessible labels and keyboard focus through `craft-ui`, retaining blue tool selection and red record-arm states. No readiness percentage change.
- **2026-10-10** — Stage re-normalized to **pre-alpha** by the core-workflow gate (interface recording, MIDI recording and session interchange fail). Full re-measure against Pro Tools Ultimate 2026.4.1 and the standard
  progress-docs set (this page, target-app parity, gaps, roadmap, UI/format/hardware/plugin/
  localization parity). v0.4.0 released. Community fixes: Cut All Automation, snap to neighbours,
  tempo stretch, I/O Setup, clip export names, menu-only dialogs, playback stop on new session.
- **2026-10-09** — Light theme and live system appearance, clip-gain fader and nudge, missing-media
  warnings, session audio health, malformed-JSON and encoder-extension checks in the CLI, macOS
  system menu bar, Windows DX12 default, Linux fontconfig, metronome click in playback.
- **2026-10-08** — v0.2.x–v0.3.0 released (Flatpak, AppImage zsync); VST3 quit crash fixed;
  Ctrl-as-Cmd shortcuts on Windows/Linux; plugin menus by vendor, AU inserts, instrument picker;
  rotating log file; branded DMG.
- **2026-10-07** — Repository published; About window credits; release CI on macOS runners.
- **2026-10-06** — Video track, Audio Unit hosting, plugin state and editors, Renderer; Mix
  window views; score export and print; surround mixing; VST3 hosting; property tests.
- **2026-10-05** — Foundation crates, Edit and Mix windows, playback, CLI/MCP, parity report,
  recording and punch, CLAP hosting, Elastic Audio, Beat Detective, release pipeline.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | Desktop MIDI Record/Stop, manual recording offset, callback-drained audio Stop, frame-exact mono/linked-stereo EDL and hardware-free CLI EDL roundtrip; alpha/hardware blockers remain |
| 2026-11-01 | minor | Audio-only OTIO import/export, MIDI recording bridge and Setup selector, bounded input routing, plugin scan safety and Linux full-gate/smoke evidence; pre-alpha gate still fails |
| 2026-10-31 | minor | Stabilization and interchange work logged; corrected stale assertions about device selection and recording-to-disk; alpha gate still fails |
| 2026-10-25 | trivial | Progress log: Auto Pan stereo balance; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Clip List Auto Rename; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Strip Silence rectangles; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Strip Silence Rename; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Strip Silence Extract and Separate; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Clip Looping dialog; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: AudioSuite Vari-Fi speed ramps; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Auto Pan plugin; readiness estimates unchanged |
| 2026-10-24 | trivial | Progress log: Subharmonic plugin; readiness estimates unchanged |
| 2026-10-23 | trivial | Progress log: Ring Modulator plugin; readiness estimates unchanged |
| 2026-10-19 | trivial | Float32 file encoding preserves finite headroom (#791); readiness estimates unchanged |
| 2026-10-18 | trivial | Mono device playback retains both stereo sides and surround fold-down channels; readiness estimates unchanged |
| 2026-10-16 | trivial | Master volume automation block boundaries corrected; readiness estimates unchanged |
| 2026-10-16 | trivial | Failed inserts no longer contribute aux, folder or master latency after containment; readiness estimates unchanged |
| 2026-10-16 | trivial | Split into Mono preserves each source channel, clip edits and saved audio; readiness estimates unchanged |
| 2026-10-14 | trivial | Progress log: oversized MCP batches rejected before execution (#460) |
| 2026-10-13 | trivial | Progress log: empty WAV data chunk followed by metadata (#466) |
| 2026-10-13 | trivial | Progress log: sampler ping-pong phase for wide steps (#463) |
| 2026-10-13 | trivial | Progress log: mix.instrument params, unknown fields refused (#265) |
| 2026-10-13 | trivial | Progress log: bus bounce compensates aux returns (#246) |
| 2026-10-13 | trivial | Progress log: master insert latency compensated (#351) |
| 2026-10-12 | trivial | Progress log: convert option validation (#256) |
| 2026-10-12 | trivial | Progress log: timecode overflow (#248) |
| 2026-10-12 | trivial | Progress log: bar offset saturates (#249) |
| 2026-10-10 | minor | Readiness-by-audience table with hours: full 950–1,450 h, mainstream 380–600 h, essentials 110–200 h |
| 2026-10-10 | minor | Added mainstream practitioner ~36 % and essentials user ~57 % to the headline numbers |
| 2026-10-10 | minor | Stage alpha → pre-alpha: the core-workflow gate fails on interface recording, MIDI recording and session interchange; alpha distance added |
| 2026-10-10 | major | Rewritten to the progress-docs standard: stage alpha (re-normalized the same day), two numbers (breadth ~70 %, ready ~40 %), dimension/feature/language tables, hours re-calibrated; area table moved to docs/target-app-parity.md, milestones to docs/roadmap.md |
| 2026-10-07 | major | Menu parity 94 %, ~65 % estimated feature parity, alpha list |
| 2026-10-06 | minor | Alpha and parity estimates |
