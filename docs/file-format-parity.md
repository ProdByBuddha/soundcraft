# File-format parity

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-12 · **Change:** minor (partial Track Bounce delivery evidence; assessment unchanged) · **Target:** Avid Pro Tools Ultimate 2026.4.1

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
| ADM BWF (Dolby Atmos master) | import/export | ❌ | ❌ | G-12 |
| SoundCraft session `.scraft` + `Audio Files/` | — | ✅ | ✅ | JSON (serde); mutated-file property test loads or fails cleanly; plugin state embedded |
| Import Session Data (from another session) | Pro Tools sessions | 🟡 | — | From `.scraft` only |
| Session info as text | export | — | ✅ | `file.export_session_text` |
| Clip groups `.rgrp` / ours `.scgrp` | read/write | 🟡 | 🟡 | Our own `.scgrp` format; Pro Tools' clip-group files not read |
| Track presets, fade presets, I/O settings, workspace, sync settings | read/write | ❌ | ❌ | Their equivalents don't exist yet (G-16) |

## Audio

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| WAV / BWF (PCM 16/24/32, float 32), `bext` | native | ✅ | ✅ | Native reader/writer, TPDF dither option; also 8-bit and float 64 read |
| RF64 / BW64 (> 4 GiB) | read/write | ✅ | ✅ | `encode_wav_rf64` |
| WAV `WAVE_FORMAT_EXTENSIBLE` multichannel with channel mask | read/write | ✅ | ✅ | `encode_with_channel_mask`; default 5.1 import creates a stereo track (#89, PR #128) |
| AIFF / AIFF-C | native | ✅ | ✅ (`fl32`) | Native |
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

**SC-MX37 — Track Bounce delivery: partial.** [Selected Audio/built-in Instrument exports](track-bounce-files.md) use explicit destinations and existing session-rate WAV/AIFF/FLAC codecs without editing the document. WAV/AIFF support 16/24-bit integer and 32-bit float; FLAC supports 16/24-bit integer, up to eight channels and 655350 Hz. WAV layout masks, decoded frames and widths up to 16 channels are tested. A delivered mono/48 kHz PCM24 WAV was independently decoded. Native publication currently requires Unix and a hard-link-capable filesystem; single-file web delivery compiles but has no observed browser download. Windows native delivery, hosted/Aux/Master/real-time/multipath paths, sample-rate conversion, mono-file grouping and other manual options remain open. No format readiness or hour estimate changes.

Standalone Track Bounce Float32 output inherits the baseline codec's over-range clipping. Own PR #345 integration and a separate over-range delivery round-trip are required before removing that limitation; this feature does not change the codecs.

## MIDI and notation

| Format | Pro Tools | Read | Write | Tests / notes |
|---|---|---|---|---|
| Standard MIDI File (type 0/1) | import/export | ✅ | 🟡 | Exported MIDI omits imported controllers and ignores the tracks filter (#87, #98; PR #125) |
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
| 2026-10-12 | minor | Recorded partial SC-MX37 selected-track format/delivery evidence and Float32/platform residuals; assessment values unchanged |
| 2026-10-10 | major | Created: format inventory against Pro Tools 2026.4.1's declared document types and import/bounce options |
