# SoundCraft — DAW-1

SoundCraft is a cross-platform, open-source digital audio workstation written in Rust. This repository (`boop-bip80/DAW-1`) is a development copy of the SoundCraft codebase. Its long-term goal is a capable, scriptable alternative to professional DAWs such as Pro Tools—not a proprietary-code or asset clone.

> **Status: pre-alpha. Not yet safe for irreplaceable recording sessions.**
> The session editor, mixer, effects, basic recording, offline export, desktop application and headless command tools exist, but key professional workflows remain incomplete. Save backups and test with non-critical material.

## What is implemented

- Multitrack session model with audio and MIDI clips, playlists, editing modes, fades, markers, tempo and automation.
- Realtime mixing with tracks, busses, sends, inserts, metering and delay compensation.
- Built-in DSP and instruments, plus CLAP/VST3/Audio Unit host code (support varies by platform and plugin).
- Playback, basic recording, WAV/AIFF/FLAC media workflows, offline bounce and stems.
- Desktop Edit/Mix interface, command-line tools and a JSON control channel/MCP interface for automation.

These describe existing subsystems, **not a guarantee that every workflow is production-ready**. For test status and limitations, start with [the ranked gaps](docs/gaps.md), [alpha gate](docs/roadmap.md#alpha-gate), and [architecture](docs/architecture.md).

## Newly integrated (November 1, 2026)

- **Audio-only timeline interchange:** File → Import/Export → OpenTimelineIO Audio reads and writes eligible multitrack timelines with clips, gaps and source trims; import validates media, resamples when needed and commits one undoable edit. CMX3600 EDL import/export additionally supports frame-aligned straight cuts on one mono `A` lane, one linked stereo `AA` lane, or two independent mono `A1`/`A2` lanes with overlapping record times across lanes. Unsupported transitions, processing and advanced structures fail explicitly. Neither format replaces AAF/OMF/PTX session compatibility.
- **Portable OTIOZ import/export:** File → Import/Export → OpenTimelineIO Bundle reads and writes eligible audio-only `.otioz` ZIPs with bounded, deduplicated local media. Import requires a saved recipient session, validates archive paths, CRCs and sizes, and commits an undoable edit. Compatibility tests include official OpenTimelineIO Clip.2 media references with signed timecode origins and literal `%` filenames. A public-CLI regression reopens and rebounces after deleting the archive and original sources. Advanced processing/video and AAF/OMF/PTX exchange remain unsupported.
- **Desktop MIDI recording:** Setup can discover and select native MIDI input ports. Desktop Record/Stop now connects captured events to armed MIDI/instrument tracks and commits undoable takes, with tests for multiple ports, timestamp alignment, stop draining and MIDI-only recording. Saved per-track named MIDI input ports and all/1–16 channel filters are now selectable in Mix/Edit, with synthetic route/filter tests. **No physical keyboard or live instrument audition has been verified**; MIDI track output, thru and sync remain incomplete.
- **External MIDI playback (hardware-unverified):** Setup → MIDI → MIDI Input Filter has eight persisted event-kind switches used at Record start (SysEx still not decoded). Mix/Edit save named MIDI output routes and channels; Playback Engine now offers a separate **disabled-by-default, this-launch-only** external MIDI transmission switch. A native off-audio-thread scheduler follows transport generations, cancels held notes and reports disconnection/queue failures in fake-device tests. **Physical MIDI output, timing/latency, live audition, thru and sync remain unverified or incomplete.**
- **Recording safeguards:** Recording rejects missing physical input channels rather than capturing from the wrong microphone. Tests cover device-buffer fallback, background writer failures, rapid Stop→re-arm isolation and Stop draining the last callback before finalizing WAV takes. Errored partial WAVs remain available for recovery and bounded notices identify preserved files; automatic disk-failure UI notice retention still needs work. Independent CPAL stream-rate diagnostics and a bounded manual offset do **not** measure hardware latency or create one-clock duplex monitoring.
- **Plugin reliability:** CLAP/VST3 note releases survive queue overload and transport stop; failures discard unsafe parameter edits, and VST3 host ranges and event-buffer contention fail closed. CLAP stops processing on its audio thread before retirement, duplicate public parameter IDs are filtered, and restart requests are observable to playback/control code. **Automatic CLAP deactivate/reactivate is not yet implemented.** Native plug-ins still execute in-process; platform-specific editor and signing gaps remain.
- **Hardware-free integration tests:** Public CLI regressions cover audio import/bounce/save/reopen, CMX3600 EDL, portable OTIOZ deletion/reopen/rebounce, and Standard MIDI File import/export persistence. The current merged source tree passed local Linux `cargo xtask ci`, including strict linting and WebAssembly. The Release workflow depends on three-OS quality jobs, but GitHub Actions still has **no observed hosted runs**; macOS/Windows/hardware testing remains unverified.

## Major blockers to professional use

| Area | Current limitation |
| --- | --- |
| Audio interfaces | Basic physical input-channel selection and manual record offset exist; the full I/O matrix, measured latency, unified low-latency duplex monitoring and hardware verification remain incomplete. |
| MIDI hardware | Synthetic native MIDI Record/Stop, per-track input/output routes and an explicit opt-in transport-driven external output path exist. Physical controller/output timing and live instrument audition are unverified; MIDI thru, clock and sync remain incomplete. |
| Session interchange | Native `.scraft` sessions exist; AAF, OMF and Pro Tools `.ptx` interchange are not implemented. |
| Plugins | Hosting, editors and signing/entitlements have OS- and format-specific gaps. |
| Reliability | Recording recovery, real hardware stress tests, cross-platform runtime checks and regression coverage need further work. |

See [ROADMAP.md](ROADMAP.md) for the long-term parity target and [docs/target-app-parity.md](docs/target-app-parity.md) for the estimates. Menu counts and estimated feature percentages are not proof of production readiness.

## Run the desktop app from source

Requirements:
- **Rust 1.95 or newer for the desktop UI** (the workspace core declares 1.90, but `soundcraft-ui-egui` requires 1.95).
- Native platform build tools. On Ubuntu/Debian, audio and UI packages include `libasound2-dev`, `pkg-config`, `libxkbcommon-dev`, `libwayland-dev`, `libx11-dev`, `libfontconfig1-dev`, and Vulkan/OpenGL development libraries.
- An audio output device for realtime playback.

```sh
git clone https://github.com/boop-bip80/DAW-1.git
cd DAW-1
cargo run --release -p soundcraft -- --demo --control 0
```

The `--demo` option starts a generated example session. The control port is printed at startup. Omit `--demo` to launch without it. Your OS may ask for microphone permission if you record.

The repository's current default development branch is **`build/craft-update-495e597`**; it is not named `main`.

## CLI and automation

```sh
cargo run -p soundcraft-cli -- run --demo --cmd 'mix.volume={"track":"Kick","db":-6}' --bounce out.wav
cargo run -p soundcraft-cli -- mcp
```

The CLI shares the engine command registry with the UI. See the [control protocol](docs/control-protocol.md) and [MCP guide](docs/mcp.md). Demo audio is synthesised in code rather than copied from proprietary recordings.

## Development and verification

```sh
cargo xtask ci
```

The full quality gate checks formatting, Clippy, tests, assets, crate layers and the web build. Targeted crate tests are useful during development but do not replace the full gate.

For changes: reproduce a defect; add a regression test; keep realtime audio code bounded and non-blocking; run the gate and report unverified hardware/platform cases; commit focused changes; update the appropriate gap/parity documents when a milestone actually changes.

Read [AGENTS.md](AGENTS.md) before contributing. Never copy proprietary Pro Tools/Avid source code or assets. See [NOTICE](NOTICE), [ATTRIBUTION.md](ATTRIBUTION.md) and the project license files for obligations and credits.

## Further reading

- [Milestones and alpha acceptance gate](docs/roadmap.md)
- [Ranked gaps and defects](docs/gaps.md)
- [File-format support](docs/file-format-parity.md)
- [Hardware limitations](docs/hardware-parity.md)
- [Plugin capabilities](docs/plugin-parity.md)
- [Release process](docs/releasing.md)
