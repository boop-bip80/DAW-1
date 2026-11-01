# Hardware parity

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (per-launch opt-in native transport MIDI scheduler and control/UI bridge verified synthetically; physical-device timing and duplex remain unverified) · **Target:** Avid Pro Tools Ultimate 2026.4.1

A DAW is judged by how it drives audio interfaces, MIDI gear, control surfaces and sync. This is
SoundCraft's weakest dimension. Ours is measured from `crates/playback` (cpal), the MIDI input-capture adapter and
per-track input routing and bounded native output scheduler, and the gaps in verified physical MIDI
playback, sync and control-surface support; Pro Tools' from its documentation and Setup menus.

**Hardware: ~12 % ready** (estimated). **Performance: ~35 %** (estimated; nothing benchmarked
against Pro Tools). Remaining ≈ 110–180 h for hardware, 40–70 h for performance.

Legend: ✅ works · 🟡 partial · ❌ missing.

## Audio interfaces

| Feature | Pro Tools (macOS / Windows) | SoundCraft macOS | Windows | Linux | FreeBSD | Web |
|---|---|---|---|---|---|---|
| Driver API | Core Audio / ASIO | 🟡 Core Audio via cpal | 🟡 WASAPI shared via cpal; ASIO with the opt-in `asio` build feature (verified with an SSL 2+) | 🟡 ALSA/Pulse via cpal; no JACK | 🟡 cpal (ALSA layer) | 🟡 Web Audio |
| Choose playback device | ✅ Playback Engine | 🟡 Setup › Playback Engine (not yet verified on macOS) | ✅ Setup › Playback Engine (WASAPI, ASIO) | 🟡 (not yet verified) | 🟡 (not yet verified) | ❌ |
| Choose input device | ✅ | 🟡 Setup › Playback Engine (not yet verified on macOS) | ✅ (WASAPI, ASIO) | 🟡 (not yet verified) | ❌ no recording | ❌ |
| Buffer size (32–2048) | ✅ | 🟡 device buffer when the driver allows it (not yet verified); mix in 512-sample blocks | 🟡 device buffer 16–8192 (verified 128 on ASIO, 256 on WASAPI); mix in 512-sample blocks | 🟡 (not yet verified) | 🟡 | ❌ |
| Duplex stream on one clock | ✅ | ❌ separate input/output streams | ❌ | ❌ | ❌ | ❌ |
| Hardware I/O mapping (I/O Setup to physical channels) | ✅ | 🟡 per-track recording input start up to 64 channels and ordered multichannel output; no I/O matrix | 🟡 | 🟡 | 🟡 | ❌ |
| Low-latency monitoring | ✅ | ❌ (`options.low_latency_monitoring` is a stored flag) | ❌ | ❌ | ❌ | ❌ |
| Latency reporting and record offset compensation | ✅ | 🟡 manual ±500 ms offset only; no measurement | 🟡 manual offset only | 🟡 manual offset only | ❌ | ❌ |
| Input/output clock-rate diagnostics | — not assessed | 🟡 independent per-CPAL-stream ppm estimates; no shared timestamp origin or measured loopback latency | 🟡 diagnostic-only | 🟡 diagnostic-only | ❌ | ❌ |
| Sample rate follows the session; resample when it can't | ✅ (switches device) | 🟡 resamples (input pitch bug fixed, #28) | 🟡 | 🟡 | 🟡 | 🟡 |
| Avid HDX / HD Native DSP, Hybrid Engine DSP mode | ✅ | ❌ (out of reach: Avid hardware) | ❌ | ❌ | ❌ | ❌ |
| Disk allocation / record to disk | ✅ | 🟡 WAV takes stream to disk through a background writer; hardware unverified | 🟡 same writer | 🟡 same writer | ❌ | ❌ |

## MIDI

A bounded MIDI 1.0 decoder, timestamped take assembler and native `midir` input queue exist (`soundcraft-midi::live` and `device`, Linux/macOS/Windows). Setup › Playback Engine can rescan and save native MIDI input selections. Desktop Record/Stop opens selected native inputs, drains callbacks and commits tempo-mapped, undoable takes to armed MIDI/instrument tracks; synthetic tests cover MIDI-only recording, timestamp alignment and Stop draining. Per-track named input ports and all/1–16 channel filters are persisted and selectable in Mix/Edit; disconnected or ambiguous ports fail recording preflight. Setup › MIDI › MIDI Input Filter exposes eight saved event-kind flags read at Record start, though SysEx is not decoded. Mix/Edit also save explicit external MIDI output routes/channels. The **separate per-launch, disabled-by-default** Playback Engine output toggle can now connect DAW transport windows to a bounded off-audio-thread native MIDI scheduler. Fake-port tests cover generation cancellation, queue saturation, cleanup acknowledgment, held-note/sustain release, stale port identity and session changes. Physical timing, scheduling fidelity and real controller/device compatibility are **unverified**; independent audio/MIDI clocks are not calibrated. Live instrument audition, MIDI thru and sync remain missing; Web has no native output adapter. Hardware readiness percentages remain unchanged.

| Feature | Pro Tools | SoundCraft (all platforms) |
|---|---|---|
| MIDI input devices (keyboards, pads) | ✅ | 🟡 desktop Record/Stop plus per-track named input-port/channel selectors and filtering tested synthetically; no physical keyboard or live audition verified |
| MIDI output to external gear | ✅ | 🟡 user-enabled transport-to-native-output path with saved named routes and fake-sink-tested cancellation/note cleanup; **no physical output or latency validation** |
| MIDI Thru | ✅ | ❌ no routed MIDI thru |
| MIDI input filter | ✅ | 🟡 Setup › MIDI › MIDI Input Filter stores per-kind flags sampled by Record; SysEx not decoded, UI tested synthetically without hardware |
| MIDI Beat Clock out, MTC generate/chase | ✅ | ❌ |
| MIDI learn / controller mapping | ✅ (via surfaces and plug-in maps) | ❌ |
| Virtual MIDI keyboard | ✅ (MIDI Keyboard window) | 🟡 window exists, no live audition path |

## Control surfaces

| Feature | Pro Tools | SoundCraft |
|---|---|---|
| EUCON (S1/S3/S4/S6, Dock, Control app) | ✅ | ❌ Avid-proprietary SDK; owner decision |
| HUI | ✅ | ❌ |
| Mackie Control Universal | ✅ (via HUI emulation and personalities) | ❌ |
| Generic MIDI controller personalities | ✅ | ❌ |

## Sync and video hardware

| Feature | Pro Tools | SoundCraft |
|---|---|---|
| LTC / MTC chase, 9-pin machine control | ✅ (Ultimate) | ❌ |
| Word clock / video reference awareness | ✅ via interface | ❌ |
| Video output to a second display or video card | ✅ | ❌ (video window only) |
| Satellite linking of systems | ✅ | ❌ |

## Performance

| Feature | Pro Tools | SoundCraft | Evidence |
|---|---|---|---|
| Parallel mixing across cores | ✅ | ✅ | `soundcraft-mix` renders independent strips in parallel (rayon) |
| Plugin delay compensation | ✅ | ✅ | Bounces of a bus that an aux returns onto are compensated like the main mix (#246). |
| Disk streaming of long sources | ✅ | ❌ | Whole files decoded into the `SourcePool` |
| Track count (Ultimate: 2,048 voices) | ✅ | unmeasured | No benchmark exists; add `cargo xtask bench` |
| Real-time audio thread discipline | ✅ | ✅ | No blocking/allocation in steady state, audio-thread-aware logger |
| Freeze / commit to save CPU | ✅ | 🟡 | `track.commit` renders; `track.freeze` only sets a flag (no CPU saved) |
| GPU-accelerated UI | — | ✅ (wgpu/glow via eframe) | llvmpipe X11 issue #107 |

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | Playback Engine adds a per-launch OFF-by-default native external MIDI output switch; SoundApp bridges the transport to a bounded generation-safe worker, all fake-port and UI tests pass; physical output, sample-accurate clocks, MIDI thru and duplex remain unverified |
| 2026-11-01 | minor | Saved external MIDI output routes/channels, pure clip-event collector and generation-safe worker note cleanup verified synthetically; independent CPAL stream-rate diagnostics added. No real transport output, single-clock duplex or hardware validation |
| 2026-11-01 | minor | Native MIDI output scheduler/discovery has bounded fake-sink tests but no track/transport playback; Setup MIDI Input Filter exposes persisted flags, with no physical controller verification |
| 2026-11-01 | minor | Persistent per-track MIDI named-port/channel input routing and Mix/Edit selectors passed synthetic tests; disconnected/ambiguous-port preflight fails closed, but physical devices/live audition remain unverified |
| 2026-11-01 | minor | Desktop Record/Stop now drives native MIDI input and undoable armed-track takes in synthetic tests; manual ±500 ms recording offset and callback-drain safeguards, no measured duplex latency or physical keyboard evidence |
| 2026-11-01 | minor | Bounded recording routes avoid wrong-channel capture; native MIDI selector and synthetic recording bridge added, still not wired to app transport; no physical or duplex validation |
| 2026-10-31 | minor | Native MIDI port discovery and capture API added but not wired to playback or armed tracks; corrected WAV take-to-disk evidence; hardware and duplex tests remain open |
| 2026-10-13 | trivial | bounces of a bus fed by an aux compensate its latency (#246) |
| 2026-10-12 | minor | Audio device settings: system, output, input, sample rate and buffer size; opt-in ASIO |
| 2026-10-10 | major | Created: audit of audio interface, MIDI, control-surface, sync and performance support against Pro Tools 2026.4.1 |
