# UI parity

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (combined relink, batch naming and Bounce dialogs) · **Target:** Avid Pro Tools Ultimate 2026.4.1

How SoundCraft feels in the hands of a Pro Tools user: tools, modes, modifiers, shortcuts,
windows, meters. Measured from `crates/model/src/session.rs` (tools, modes), the command
registry (shortcuts), `crates/ui-egui/src` (windows, gestures) and user issues; compared with
reference screenshots kept locally under `plan/protools/screenshots/` (16, never committed).

**UI/UX fidelity: ~55 % ready** (estimated). Remaining ≈ 60–100 h.

Legend: ✅ matches · 🟡 partial · ❌ missing.

## Tools and modes

| Feature | Status | Notes |
|---|---|---|
| Tools: Zoom, Trim, Selector, Grabber, Scrubber, Pencil, Smart (7) | ✅ present | Trim variants (TCE, loop, scrub trim) and Grabber variants (separation, object) partial; Multitool fade/trim handles don't adjust audio (#5) |
| Edit modes: Shuffle, Slip, Spot, Grid (absolute/relative) | ✅ | Spot dialog present |
| Grid and nudge values, all timebases | ✅ | Bars/Beats, Min:Secs, Timecode, Feet+Frames, Samples |
| Keyboard focus modes (Commands, Clip List, Groups) | 🟡 | Commands focus single-key editing ✅; list focus partial |
| Link timeline/edit, link track/edit, Tab to transient, insertion follows playback | ✅ | |
| Mirrored MIDI, MIDI merge, layered editing | 🟡 | Stored and partly honoured |

## Keyboard and mouse

| Feature | Status | Notes |
|---|---|---|
| Default shortcuts | 🟡 146 bound (measured: distinct `Some("…")` shortcuts in the registry) | Pro Tools' default set is several hundred incl. numeric keypad and focus keys. Collisions: Mix/Edit toggle vs zoom (#144), Heal (#140), Export (#7) |
| Ctrl as Cmd on Windows/Linux | ✅ since #15, #41 still open in some builds | #62 shows Ctrl in labels |
| Numeric keypad transport and memory-location recall | 🟡 | Classic / Transport / Shuttle modes not all present |
| Modifier gestures (Alt for all tracks, Shift-Alt for selected, Cmd-click fine faders, Ctrl-click defaults) | 🟡 | Main ones on faders and pan; not across every control |
| Scroll/zoom: wheel, trackpad pinch | 🟡 | Anchored trackpad zoom in open PRs #112, #137 |
| Drag and drop: Clip List → tracks, files → tracks | ✅ macOS/Windows; 🟡 Wayland (PR #150) | |
| Context menus (clip, track name, ruler, Clip List) | 🟡 | Clip and track menus exist; depth lower than Pro Tools'; Clip List delete (#4) |

## Windows and panels

| Window | Status | Notes |
|---|---|---|
| Edit window: rulers, track headers, view columns, Clip List, Groups, Tracks list | ✅ | Track Pin (new in 2026.4) ✅ (`track.pin`) |
| Mix window: strips, views (inserts, sends, I/O, comments, EQ curve, preamps, instruments, object) | ✅ | Narrow Mix ✅; Delay Compensation view and HEAT missing |
| MIDI Editor, Score Editor, Event List | 🟡 | Dock height bug fixed (#113); notation editing shallow |
| Transport, Big Counter, Memory Locations, Automation, Video, Universe, Renderer, UI Customization | ✅ present | |
| Workspace / sound browser with previews | ❌ | G-16 |
| Missing audio relink | 🟡 | Native Session Audio Health action, file picker/path entry, content validation, explicit legacy confirmation and retry after errors; automatic/batch relinking remains G-16 |
| Batch track naming | 🟡 | Selected-track literal stages, preview and one Undo; stale revisions reject Apply; advanced controls and clip/file naming remain |
| Bounce delivery sample rate | 🟡 | Common/custom independent delivery-rate controls; broader delivery options remain |
| I/O Setup | 🟡 | Busses and paths; no hardware channel matrix (G-01) |
| Preferences | 🟡 | Many settings stored, fewer honoured |
| Window configurations, Tile/Cascade | 🟡 | Configurations ✅; Tile/Cascade in PR #151 |
| Plugin windows | 🟡 | Generic parameter sliders; Pro Tools plugins have bespoke GUIs (#138 asks for skeuomorphic design); multi-page plugin views (#6) |
| Command palette (Search) | ✅ | Over every command |
| Track Bounce dialog (SC-MX37) | 🟡 partial | Track menu, clicked-track header menu and Cmd/Ctrl+Alt+Shift+B share an immutable source/range/revision preview. Explicit destination, format/depth/dither, volume/pan and tail controls; stale previews disable delivery, document replacement closes the dialog, and Cancel/Escape plus Enter cannot export |

The [Track Bounce subset](track-bounce-files.md) has five original UI tests and an inspected 3200×2000 dialog image. Unix native delivery requires hard-link-capable destinations; web supports one file through the existing hook, with compilation evidence only. Aux/Master, hosted/latency-producing processing, real-time/multipath delivery and Windows native delivery remain unsupported. Audio effects start cold and clip effects have no independent postselection tails; integrated Float32 delivery retains finite headroom and rejects nonfinite rendered PCM without delivering a file. Rendering has no progress display or mid-render cancellation. This scoped update does not remeasure shortcut counts, UI fidelity or hour estimates.

## Look and meters

| Feature | Status | Notes |
|---|---|---|
| Dark theme close to Pro Tools' | ✅ | Light theme (#32) and live system appearance (#85) beyond Pro Tools |
| Interface scaling | 🟡 | Slider in PR #147 |
| Meter types and ballistics (peak, RMS, VU, K-scales, gain reduction) | 🟡 | Peak/RMS; meter type choice and GR meters partial |
| Clip indicators, track colours, waveform views (peak/power/rectified/outlines) | ✅ | |
| Fader resolution, fine adjust, numeric entry | 🟡 | |
| macOS system menu bar | ✅ | #10 |

Integrated workflow details and remaining validation: [fork integration](fork-integration.md). Existing percentages and hours remain prior estimates.
Save Session Copy In has a dedicated media preview, blocked-source reasons, explicit external-video limits, and stale-preview refresh that retains the typed destination. [Behavior and residual scope](session-copy.md).

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Combined native dialogs retain independent lifecycle/retry behavior and sample-rate controls; broad UI estimates unchanged |
| 2026-10-12 | minor | Added native manual audio relink dialog and health-report refresh; no general UI parity claim |
| 2026-10-12 | minor | Added partial SC-MX37 dialog interaction evidence and bounded delivery/format limits; assessment values unchanged |
| 2026-10-12 | minor | Strict audio-complete Save Copy and media preview; conversion, selected-time, video and full external-asset portability remain open |
| 2026-10-10 | major | Created: tools, modes, shortcuts, windows and meters checklist against Pro Tools 2026.4.1 |
