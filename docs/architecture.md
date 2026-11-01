# Architecture

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (MIDI recording bridge, bounded input channel routing and audio-only OTIO import) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft is a Cargo workspace of small crates with enforced layering (`cargo xtask layers`).
Nothing below the UI knows about egui, so the interface can be replaced.

The UI package pins `craft-ui` for common icon-button and track record-enable interactions.
SoundCraft supplies each state's colors, border, radius and icon painter; selection and command
dispatch remain in the toolbar and track controls. The single-shape frame option preserves the
existing fill/border blending when controls are disabled. Specialized transport Play/Stop/Record styling,
nonlinear meters, faders, pan gestures, fonts and the live theme resolver remain local. Shared adapters read the current palette. The UI
requires Rust 1.95 or later without raising the core workspace's Rust 1.90 declaration.

```
L0  time        audio-io        midi        video   (standalone: no workspace deps)
L1  dsp         clap-host   vst3-host   au-host     (plugins; the three hosts are the only crates
                                                     allowed `unsafe`, confined to their FFI modules)
L2  model                                       (the session document)
L3  mix                                         (the mix engine)
L4  engine      playback                        (commands, undo, I/O · audio devices)
L5  automation                                  (MCP server, control-channel client)
L6  ui-egui                                     (the user interface)
    apps: soundcraft (desktop) · soundcraft-cli · soundcraft-web (wasm)
```

## The document

`soundcraft_model::Session` is plain data (serde): tracks with playlists of clips, mixer state
(inserts, sends, routing, automation lanes), tempo and meter maps, memory locations, groups,
busses, and `EditState` (selection, tools, modes, view flags). Decoded audio lives in a
`SourcePool` of `Arc`s that is not serialised; cloning a session is cheap. Sessions save as
`.scraft` JSON plus an `Audio Files/` folder.

## Commands and undo

Every user action is a `CommandSpec` (id, label, menu path, shortcut, params doc, enabled, run)
registered in `crates/engine/src/cmd/`. `Engine::execute(id, params)` runs it, catches any escaped
panic, and pushes an undo snapshot (the previous `Arc<Session>`) when the document changed.
Continuous gestures use `execute_merged` so a fader drag is one undo step. Menus are built from
the incumbent's menu catalog (`crates/engine/catalog/menus.txt`); a menu item lights up when a
command has the same menu path and label (or an alias maps it), which also drives the parity
report.

## Audio

`soundcraft_mix::MixEngine::render(session, pos, frames, out)` renders one block: clips (gain,
fades, clip effects) → trim → inserts → pre-fader sends → fader/mute (automation, VCA, trim
automation) → post-fader sends → pan → busses → aux inputs → master faders. Independent strips
process in parallel; plugin delay compensation aligns every path, the master faders' inserts included. The same engine renders
bounces offline (`RangeRender`, streamed to the file block by block) and realtime playback
(`soundcraft_playback::Player`, which owns it on the audio thread and receives new session
snapshots through a channel).

Device I/O uses selectable output and input devices, sample rate and buffer size, still opened
as separate cpal streams (without a shared duplex clock or full physical-channel routing). The
mix renders 512-sample blocks and resamples when the device rate differs; ASIO is opt-in on
Windows. Recording input startup retries with the device-default buffer if a requested one
fails during stream build or play. Per-track physical input routes are validated before take
creation, and unavailable monitor inputs remain silent. The `soundcraft-midi::device` native
adapter discovers and opens input ports, forwarding timestamped events through a bounded queue.
`MidiRecordBridge` translates an explicitly anchored device timestamp to session ticks and can
commit one undoable armed-track take, but desktop record/stop and instrument monitoring are not
connected to it (see [`hardware-parity.md`](hardware-parity.md)). Decoded playback sources live
in `SourcePool`; active recording takes stream through a bounded ring to a WAV writer thread,
with periodic headers and writer-failure reporting. `file.import_otio` validates and decodes
local audio-only OTIO media before an atomic, undoable session edit.

`soundcraft-video` holds our own H.264, ProRes and Motion JPEG decoders and the ISO-BMFF demuxer
for the video track. Hosted plugins (`clap-host`, `vst3-host`, `au-host`) are scanned, created and
destroyed off the audio thread; only `process` runs on it.

The cpal callbacks mark their thread (`soundcraft_playback::mark_audio_thread`). Code that may run
there can ask `on_audio_thread()` before doing anything that blocks; the desktop app's logger does,
so a `log::` record from the audio thread (a stream error, a full synth event queue, a hosted
plugin's failed `process`, a CLAP plugin's own log call) is kept without waiting and written later
by the UI thread (see README › Logs).

## Agent control

The desktop app's control channel (JSON lines over TCP) exposes engine commands, inspection,
synthetic input and screenshots; `soundcraft-cli mcp` wraps either a headless engine or a running
app as an MCP server. See `control-protocol.md` and `mcp.md`.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | Added bounded physical input-route preflight, native MIDI selector/undoable take bridge and atomic OTIO audio import; desktop MIDI record lifecycle still missing |
| 2026-10-31 | minor | Corrected device and direct-to-disk recording architecture, added input buffer fallback and MIDI native capture API; DAW routing remains missing |
| 2026-10-13 | trivial | delay compensation includes the master faders' inserts (#351) |
| 2026-10-10 | minor | Added video, vst3-host and au-host to the layer diagram; documented device I/O limits and in-memory media |
| 2026-10-05 | major | First architecture overview |
