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

- **Audio-only timeline interchange:** File → Import/Export → OpenTimelineIO Audio reads and writes eligible multitrack timelines with clips, gaps and source trims; import validates media, resamples when needed and commits one undoable edit. CMX3600 EDL import/export additionally supports frame-aligned straight cuts on one mono `A` lane or one linked stereo `AA` lane. Unsupported transitions, processing and advanced structures fail explicitly. Neither format replaces AAF/OMF/PTX session compatibility.
- **Desktop MIDI recording:** Setup can discover and select native MIDI input ports. Desktop Record/Stop now connects captured events to armed MIDI/instrument tracks and commits undoable takes, with tests for multiple ports, timestamp alignment, stop draining and MIDI-only recording. **No physical keyboard or live instrument audition has been verified**; track-specific port/channel routing, external output and sync remain incomplete.
- **Recording safeguards:** Recording rejects missing physical input channels rather than capturing from the wrong microphone. Tests cover device-buffer fallback, background writer failures and Stop draining the last callback before finalizing WAV takes. A bounded manual recording offset is available for aligning overdubs, but it does **not** measure hardware latency or create a single-clock duplex stream.
- **Plugin reliability:** CLAP/VST3 note releases survive queue overload and transport stop. Bundle scans reject ambiguous executables, and VST3 state restore protects pending edits on failure. Native third-party plug-ins still execute in-process; platform-specific host/editor and signing gaps remain.
- **Hardware-free integration tests:** CLI regressions cover import, bounce, save/reopen after removing the source path, mono CMX3600 EDL interchange and linked-stereo EDL waveforms. An earlier exact merged snapshot passed the full local Linux `cargo xtask ci` gate, including WebAssembly; current changes must still clear that gate. GitHub Actions reports no runs, and macOS/Windows/hardware testing remains unverified.

## Major blockers to professional use

| Area | Current limitation |
| --- | --- |
| Audio interfaces | Basic physical input-channel selection and manual record offset exist; the full I/O matrix, measured latency, unified low-latency duplex monitoring and hardware verification remain incomplete. |
| MIDI hardware | Synthetic native MIDI Record/Stop is implemented; physical controller recording, live instrument audition, track-specific input routing, external output and clock remain unverified or incomplete. |
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
