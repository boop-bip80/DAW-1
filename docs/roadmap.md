# SoundCraft roadmap: milestones and what's next

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (portable audio-only OTIOZ import/export, saved MIDI output routing and worker safety, independent clock diagnostics and CLI regressions; alpha gate still fails) · **Target:** Avid Pro Tools Ultimate 2026.4.1

Forward-looking plan. Status and numbers: [`../ROADMAP.md`](../ROADMAP.md) and
[`target-app-parity.md`](target-app-parity.md); the ranked work list: [`gaps.md`](gaps.md).
Hours are Opus 5.5 agent-hours.

## Current focus

1. **Merge or close the open bug-fix PRs** (#123–#134, #146, #148, #75) and fix the rest of the
   correctness backlog (G-04). ≈ 15–25 h.
2. **Audio interface completion** (G-01): full physical channel matrix, one-clock duplex monitoring, measured latency/automatic offsets and hardware verification. Manual offset, callback drain and bounded per-track input choice exist; device/rate/buffer choice exists. ≈ 15–25 h estimated.
3. **MIDI devices** (G-02): validate synthetic desktop Record/Stop and saved per-track input-port/channel routing on a physical controller; opt-in external output-port/channel routes and an off-callback worker with cancellation now exist, but track/transport scheduling, live instrument monitoring, thru and clock still need wiring. ≈ 20–35 h estimate not re-calibrated.
4. **macOS plugin hosting** (G-05): obtain explicit owner approval for relaxing signed-app library validation to host third-party native plugins; then address AU editors. ≈ 15–25 h, plus a security decision.
5. **Verify CI execution** (G-14): local Linux `cargo xtask ci` and public CLI audio/OTIOZ/MIDI import-export-reopen smoke tests pass; workflows target the default branch on three OSes, and release packaging depends on a three-OS quality job, but GitHub reports no Actions runs. Enable/verify runners and add hardware/platform smoke tests.

## Alpha gate

The core workflows a Pro Tools professional does every day, checked end to end on macOS (the
main platform). Any **no** keeps SoundCraft **pre-alpha**, and those rows are the alpha checklist
(craftrules `standards/progress-docs.md`, Stages).

| Workflow | Works end to end? | Evidence | Hours to pass |
|---|---|---|---:|
| Record audio from a chosen interface at a chosen buffer size, monitor, punch, keep the takes | **no** | Playback Engine selects devices, rate, buffer and a bounded manual record offset; WAV takes stream to disk and Stop drains callback audio. Invalid input channels are rejected before take creation. Still separate input/output streams, 512-frame mix block, no full physical I/O matrix or measured duplex latency compensation; needs interface tests. | 35–55 |
| Record MIDI from a controller and play it back through an instrument | **no** | Drawn/imported MIDI works. Native port selection, persistent per-track port/channel routes and desktop Record/Stop-to-armed-track undoable takes pass synthetic tests, but live instrument audition is not connected and no physical keyboard end-to-end test exists (G-02). | 20–35 |
| Edit: select, trim, fade, comp playlists, Slip/Shuffle/Grid | yes (rough) | Every Edit menu command; property-tested with undo; open bugs #3, #5, #140 don't block the workflow | — |
| Mix with built-in and third-party plugins, sends, busses, automation | partial (passes on CLAP/AU) | 27 built-in processors; CLAP and AU host on the signed macOS build, but its missing entitlement blocks third-party VST3 (#153). No AU plugin windows yet; VST3 windows on macOS only | 1–2 (entitlement) |
| Bounce mix and stems; save and reopen the session | yes (rough) | WAV/BWF/AIFF/FLAC bounce, stems, `.scraft` save/reopen with plugin state; bugs #59, #103 in bus bounce and aux latency (PRs open) | — |
| Exchange a session with Pro Tools or a picture editor | **no** | Audio-only `.otio` import/export, portable `.otioz` bundle import/export (saved recipient, supported audio subset), and restricted mono/stereo or independent A1/A2 CMX3600 straight-cut EDL interchange work, but no `.ptx`, AAF or OMF interchange (#40, G-03) | 50–90 |

**Result: pre-alpha.** Three workflows fail. Passing them is ~110–180 h, about 70 % of it
parallelizable (interfaces, MIDI and AAF are independent crates).

## Milestones

Status: ✅ done · 🟡 in progress · ⬜ not started.

| # | Milestone | Status | Remaining (h) |
|---|---|---|---:|
| M0–M4 | Workspace, gates, model, I/O, DSP, mix engine, command engine, Edit/Mix UI, playback, metering, plugins, bounce | ✅ | — |
| M5 | Editing depth (tools, modes, playlists, fades, comping, clip groups) | ✅ (bugs open, G-04) | in G-04 |
| M6 | Recording, punch, loop record, monitoring | 🟡 device/rate/buffer choice and WAV disk writer exist; duplex routing, latency and hardware reliability still incomplete | 40–60 |
| M7 | MIDI and notation editing | 🟡 | 50–80 |
| M8 | CLI, control channel, MCP, agent acceptance test | ✅ | 3 |
| M9 | AudioSuite breadth ✅, Elastic Audio 🟡, Beat Detective 🟡 | 🟡 | 40–60 |
| M10 | Public releases on all platforms | ✅ v0.3.0 (2026-10-08) and v0.4.0 (2026-10-10) released for every platform | — |
| M11 | Third-party plugin hosting (CLAP, VST3, AU), video, surround | 🟡 mostly done | 40–60 |
| M12 | **Hardware**: audio interfaces, MIDI devices, sync, control surfaces | ⬜ | 110–180 |
| M13 | **Interchange**: AAF/OMF import/export and source streaming/recovery (OTIO audio, `.otioz` bundle import/export, mono/linked-stereo and independent two-mono-lane CMX3600 EDL cuts, WAV take writing exist) | 🟡 foundation only | 65–115 |
| M14 | **Fidelity harness** against Pro Tools (null tests, plugin responses) | ⬜ | 20–30 |
| M15 | **Localization**: catalog, fonts/RTL, the 8 Pro Tools languages first | ⬜ (PRs #141, #143 open) | 70–110 |
| M16 | Built-in plugin depth, sidechain, ARA | ⬜ | 110–170 |
| M17 | Immersive delivery (ADM BWF, object renderer, binaural) and video depth | ⬜ | 80–130 |

M8 follow-up (2026-10-14): MCP batches above 1000 calls are rejected before execution (#460).
The limit is advertised in the tool schema and description; stage and readiness estimates are unchanged.

## To beta (~350–570 h, about 60–70 % parallelizable)

Beta needs ~75 % ready for real work and no blocking gap in session interchange.

| Step | Gaps | Est. (h) |
|---|---|---:|
| Correctness backlog and a PR CI | G-04, G-14 | 40–65 |
| Audio interfaces, MIDI devices, sync | G-01, G-02, G-11 | 60–100 |
| AAF/OMF interchange, playback-source streaming, crash recovery | G-03, G-06 | 65–115 |
| Plugin hosting completeness on every OS, sidechain | G-05, G-13 | 35–60 |
| Fidelity harness and the fixes it finds | G-07 | 40–60 |
| Built-in plugin essentials (EQ/dynamics/reverb/delay depth) | G-08 (half) | 40–60 |
| UI feel pass: shortcuts, plugin windows, meters | G-19 (half) | 30–50 |
| Localization of the 8 Pro Tools languages (catalog, CJK fonts) | G-09 (part) | 40–60 |

Needs a human: hardware for G-01/G-02/G-10/G-11, native-speaker review, and owner decisions on
AAX, `.ptx` and Avid cloud features.

## After beta (to full parity, a further ~600–900 h)

Control surfaces (G-10), immersive delivery (G-12), the rest of the built-in plugin catalogue
(G-08), Elastic Audio depth (G-15), session templates and browser (G-16), video depth (G-17),
speech-to-text (G-18), notation and instrument depth (G-20), compressed export (G-21), and the
remaining languages.

There is no `cargo xtask scorecard` in this repo; the measured number is `cargo xtask parity`
([`parity-checklist.md`](parity-checklist.md)).

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | OTIOZ audio bundle import/export, opt-in MIDI output routing/worker safety, CPAL drift diagnostics, Transport roll controls and public CLI OTIOZ/SMF roundtrips verified locally; AAF/OMF/PTX, physical tests and hosted CI still block alpha |
| 2026-11-01 | minor | OTIOZ portable audio export-only and MIDI Input Filter/output worker foundations recognized; release quality dependency configured, hosted CI and physical MIDI/playback still unverified, alpha gate unchanged |
| 2026-11-01 | minor | Per-track MIDI named-port/channel selectors and synthetic route filtering landed; independent two-mono-lane A1/A2 CMX3600 EDL cuts landed; physical MIDI/live audition and AAF/OMF/PTX alpha gate unchanged |
| 2026-11-01 | minor | Desktop native MIDI Record/Stop integrated in synthetic tests; manual record offset, callback-drained audio Stop and restricted CMX3600 mono/stereo audio EDL; physical controller and AAF/OMF/PTX alpha blockers remain |
| 2026-11-01 | minor | Native MIDI take bridge (not app-wired), audio-only OTIO import, bounded audio channel routes, CLI media round-trip and full Linux gate; alpha/physical/CI runner verification remain open |
| 2026-10-31 | minor | Native MIDI input capture foundation (no track routing), audio-only OTIO export, recording/command-palette safeguards and three-OS CI configuration; alpha gate and physical/runner verification remain open |
| 2026-10-24 | trivial | 27 built-in processors, as the plugin registry holds; the sampler (#108) is no longer listed under plugin essentials |
| 2026-10-14 | trivial | M8: reject oversized MCP batches before execution (#460) |
| 2026-10-10 | minor | Added the alpha gate (3 of 6 core workflows fail → pre-alpha) |
| 2026-10-10 | major | Created from ROADMAP.md's milestones and alpha list; added M12–M17 and the beta plan with estimates |
