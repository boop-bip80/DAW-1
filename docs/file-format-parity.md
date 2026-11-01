# File-format parity

> **Last reviewed:** 2026-11-01 · **Last updated:** 2026-11-01 · **Change:** minor (portable audio-only OTIOZ import/export and public MIDI SMF roundtrip evidence; no AAF/OMF/PTX parity change) · **Target:** Avid Pro Tools Ultimate 2026.4.1

Every format Pro Tools reads or writes, and ours. The Pro Tools list comes from the document types
its bundle declares (`CFBundleDocumentTypes` in Info.plist: names only, nothing else read) and its
documented import/bounce options. Ours comes from `crates/audio-io` (`detect_format`, `decode`,
`ENCODE_EXTENSIONS`), `crates/engine/src/cmd/file.rs`, `crates/engine/src/score.rs`,
`crates/engine/src/clip_group_file.rs` and `crates/video`.

**File formats: ~40 % ready** (estimated: weighted by how often a Pro Tools user hits each row;
session interchange carries a third of the weight and is at 0 %). Remaining ≈ 70–120 h.

Legend: ✅ supported and tested · 🟡 partial · ❌ missing · — not applicable.

## Sessions and interchange

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| Pro Tools session `.ptx` (and `.ptf` from 7–9) | native | ❌ | ❌ | Proprietary, undocumented. Owner decision needed on black-box format study (G-03) |
| Session templates `.ptxt` | read/write | ❌ | ❌ | We have no templates at all (G-16) |
| AAF (Advanced Authoring Format) | import/export | ❌ | ❌ | #40. The post-production handoff from picture editors (G-03) |
| OMF | import/export | ❌ | ❌ | Legacy, still delivered by some editors |
| Media Composer–compatible session | export | ❌ | ❌ | AAF variant |
| OpenTimelineIO `.otio` audio timeline | not a native Pro Tools session format | 🟡 | 🟡 | `file.import_otio` reads local audio tracks, gaps, clips and trims, resamples sources and commits atomically with undo; Clip.2 official fixture tested. `file.export_otio` writes eligible audio; unsupported effects, transitions, video, offline/unsaved/stretched media fail explicitly |
| Portable OpenTimelineIO `.otioz` bundle (audio only) | media-inclusive OTIO ZIP, not a Pro Tools session | 🟡 supported audio subset | 🟡 supported audio subset | `file.import_otioz` / `file.export_otioz` and File-menu Import/Export read/write `content.otio`, `version.txt` and deduplicated local media with 256/512 MiB media caps and strict ZIP traversal/CRC checks. Import needs a saved target session and is undoable; CLI tests preserve stereo/trims after exporter-media and archive deletion. Unsupported gain/fades/mixer/video fail explicitly; not AAF, OMF or `.ptx` compatibility |
| CMX3600 `.edl` audio cuts | edit decision list interchange, not a full Pro Tools session | 🟡 | 🟡 | `file.import_edl` / `file.export_edl` handle mono `A`, linked stereo `AA`, or two independently edited mono `A1`/`A2` cut lanes; source/record trims are frame aligned, record cuts may overlap across mono lanes, and unsupported formats/processing fail closed. Engine fixtures include an official OTIO parser check; mono/AA CLI roundtrip tested. Not arbitrary multitrack EDL, AAF, OMF or `.ptx` interchange |
| ADM BWF (Dolby Atmos master) | import/export | ❌ | ❌ | G-12 |
| SoundCraft session `.scraft` + `Audio Files/` | — | ✅ | ✅ | JSON (serde); mutated-file property test loads or fails cleanly; plugin state embedded. Fades save optional `curvature` (-1..1); missing values default to 0 and preserve legacy envelopes |
| Import Session Data (from another session) | Pro Tools sessions | 🟡 | — | From `.scraft` only |
| Session info as text | export | — | ✅ | `file.export_session_text` |
| Clip groups `.rgrp` / ours `.scgrp` | read/write | 🟡 | 🟡 | Our own `.scgrp` format; Pro Tools' clip-group files not read |
| Track presets, fade presets, I/O settings, workspace, sync settings | read/write | ❌ | ❌ | Their equivalents don't exist yet (G-16) |

## Audio

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| WAV / BWF (PCM 16/24/32, float 32), `bext` | native | ✅ | ✅ | Native reader/writer, TPDF dither option; Float32 retains finite headroom (#791); also 8-bit and float 64 read. An empty data chunk followed by metadata reads as no audio; an unfinalised recording's audio is still read (#466). |
| RF64 / BW64 (> 4 GiB) | read/write | ✅ | ✅ | `encode_wav_rf64` |
| WAV `WAVE_FORMAT_EXTENSIBLE` multichannel with channel mask | read/write | ✅ | ✅ | `encode_with_channel_mask`; default 5.1 import creates a stereo track (#89, PR #128) |
| AIFF / AIFF-C | native | ✅ | ✅ (`fl32`) | Native; `fl32` retains finite headroom (#791) |
| FLAC | import | ✅ | ✅ | Own encoder |
| MP3 | import, bounce | ✅ (symphonia) | ❌ | G-21 |
| AAC / M4A / MP4 audio | import, bounce | ✅ (symphonia) | ❌ | G-21 |
| ALAC | import | ✅ | ❌ | |
| Ogg Vorbis | — | ✅ | ❌ | Beyond Pro Tools |
| CAF | — | ✅ | ❌ | |
| Sound Designer / SD II | import (legacy) | ❌ | — | Rare today |
| ReCycle / REX | import | ❌ | — | Loops |
| MXF audio (OP-Atom, OP1a) | import | ❌ | ❌ | Avid media; FilmCraft has an MXF reader to learn from |
| iXML / polyphonic field-recorder metadata | read | ❌ | ❌ | G-17 |
| Peak overviews | `.wfm` cache | ✅ (own, in memory) | — | `audio-io::peaks`; not cached on disk |

## MIDI and notation

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| Standard MIDI File (type 0/1) | import/export | ✅ | 🟡 | A public Type-1 CLI import → save/reopen → export → fresh-import test verifies note pitch/velocity/duration/channel, CC7, program, pitch bend, tempo and meter. Broader MIDI fidelity and requested track filtering remain incomplete (#98); no physical playback evidence |
| MusicXML (to Sibelius) | export | — | ✅ | `file.export_sibelius`, `score.rs` |
| Printable score | print | — | ✅ (SVG) | |

## Video

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| QuickTime / MP4, H.264 | import, bounce | ✅ | ❌ | Own pure-Rust decoder (Baseline/Main/High); mutated-movie fuzzing |
| ProRes (all flavours) | import | ✅ | ❌ | Own decoder |
| Motion JPEG | import | ✅ | ❌ | |
| HEVC, AV1 | import | ❌ | ❌ | G-17 |
| DNxHD / DNxHR, MXF video | import | ❌ | ❌ | G-17 |
| Bounce with video | export | — | ❌ | G-17 |

## Plugin and preset files

| Format | Pro Tools | Ours |
|---|---|---|
| AAX plug-ins | the only format it hosts | ❌ (cannot: Avid SDK licence, PACE signing) |
| VST3 / Audio Units / CLAP | — (AAX only) | ✅ hosted (ahead of Pro Tools) |
| Plug-in settings `.tfx` | read/write | ❌ (our own JSON presets per plugin) |

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-11-01 | minor | Added secure, undoable audio-only `.otioz` import alongside export; CLI regression survives deletion of exporter/archive. Public Type-1 MIDI roundtrip verifies controller/program/pitch and tempo/meter semantics; AAF/OMF/PTX still missing |
| 2026-11-01 | minor | Added File-menu audio-only `.otioz` portable bundle export with bounded deduplicated media; import remains unsupported, AAF/OMF/PTX gap unchanged |
| 2026-11-01 | minor | CMX3600 import/export now accepts two independently edited mono A1/A2 audio lanes with overlapping record cuts, source trims and strict fail-closed checks; not arbitrary multitrack or AAF/OMF/PTX interchange |
| 2026-11-01 | minor | Added frame-aligned CMX3600 audio EDL import/export for single mono `A` and linked stereo `AA` cuts, with strict rejection and waveform round-trip tests; not a substitute for AAF/OMF/PTX |
| 2026-11-01 | minor | Added bounded, undoable OTIO audio import with official Clip.2 fixture and source-rate conversion; AAF, OMF and PTX still unsupported |
| 2026-10-31 | minor | Added OTIO audio-only export; AAF, OMF and Pro Tools sessions remain unsupported |
| 2026-10-19 | trivial | WAV and AIFF-C Float32 encoders retain finite headroom (#791); whole-file, streaming and bounce regression tests |
| 2026-10-13 | trivial | WAV: empty data chunk with trailing metadata (#466) |
| 2026-10-13 | minor | Fade curvature round-trips in sessions with a zero default for older files. Format version and readiness estimate unchanged |
| 2026-10-10 | major | Created: format inventory against Pro Tools 2026.4.1's declared document types and import/bounce options |
