# Track Bounce file delivery

Updated: 2026-10-12. Implementation scope: SC-MX37, based on the public [Pro Tools Reference Guide 2026.4](https://resources.avid.com/SupportFiles/PT/Pro_Tools_Reference_Guide_2026.4.pdf#page=1474) Track Bounce workflow (PDF pages 1474–1476, printed pages 1372–1374). This is a bounded SoundCraft implementation, not a full manual parity claim.

Track > Bounce…, Cmd+Alt+Shift+B, and the track-header Bounce… item open an original SoundCraft dialog. The header item targets the clicked track. The menu and shortcut snapshot selected tracks, the timeline selection and session revision. A stale dialog cannot export; replacing the document closes it. Preview lists planned destination, layout, channels, capture/output range, delivered frames and Instrument preroll without creating plugins, rendering or writing files.

`track.bounce` now delivers files instead of calling Commit. It does not add sources or tracks, hide/deactivate tracks, alter arrangement/activity/mixer state, change transport, or enter undo/redo or the recovery journal. `track.commit` retains its separate document-editing behavior. Programmatic exports require an explicit destination and share the same planner as the dialog.

## Supported sources and rendering

- Active Audio tracks and active built-in Instrument tracks, without record arm or input monitoring.
- Active built-in effects with zero insert latency. Hosted plugins, Pitch Shifter, Maximizer, nonzero compressor lookahead and lookahead automation are rejected before rendering. Bypassed/inactive slots are not instantiated.
- Exact declared source width, up to 16 channels, with Render Pan off for Audio or stereo/multichannel Instrument tracks. Mono Instrument Pan off is rejected because the built-in instrument internally produces stereo and no new fold policy is implied.
- Render Pan on applies the existing SoundCraft matrix and pan automation to one Main or internal Bus output width. It captures the selected strip, before downstream Aux/Bus/Master processing. Main width is validated before the existing main-format fallback. Hardware/None outputs are rejected with Pan on.
- Render Volume on uses the fader, automation read policy, trim automation and referenced VCA fader/read automation. Turning it off bypasses those gain contributions. Static trim, phase inversion, track/automation mute and VCA mute still apply. Other tracks' solo state is ignored. Sends, Aux returns, Master processing, metronome and live inputs are excluded.
- Clip gain/fades/stretch and enabled Clip Effects use the existing Audio strip renderer. Built-in Instrument note scheduling uses the existing tempo and MIDI real-time properties; controller events and MIDI clip stretch are explicitly unsupported.

Only selected strip plugin instances are created. Other tracks, even inactive hosted plugins and VCA plugin slots, never enter mixer synchronization. VCA numeric/mute metadata is read immutably.

## Range and tail

An explicit start/end takes priority. Otherwise a nonempty timeline selection is used (edit selection when linked, separate timeline selection when unlinked). With no selection, each source starts at session zero and ends at its last unmuted Audio clip or rendered note-off, constrained by its MIDI clip boundary.

Extra tail extends the delivered file by the requested duration. Source input stops at capture end. Held Instrument notes are released there; no later note or clip can enter the tail. Track inserts and instrument release continue against silence. Per-clip effects are no longer processed once source input stops; their independent tails are excluded. Blocks split at capture end, including an end between normal 1024-sample block boundaries.

Instruments start from fresh state at session zero, preroll through selection start, and discard that earlier audio. Preview exposes this processing span. Audio effects start cold at selection start, as in the existing offline renderer; this subset does not reconstruct earlier Audio effect history. Processing is synchronous without cancellation or progress reporting.

## Destinations and formats

Native publication currently supports Unix systems and destinations on hard-link-capable filesystems. A single source uses `path`; a native batch uses an existing `dir`. Batch names are sanitized, truncated, and suffixed with the stable track ID. Direct path extension must match the format. Existing files and symlinks are rejected, including collisions after preview. Directories are not created automatically.

Files are encoded to exclusive staging files in the destination directory, flushed, and published with exclusive hard links. Unsupported filesystems fail rather than using an overwrite fallback. Staging and newly linked finals are checked against the expected device/inode before success. Cleanup checks identity again before unlinking. These standard-library checks are best-effort handling of concurrent changes, not an atomic race-free ownership primitive. Failures clean up only paths whose checked device/inode identity matches this invocation; external replacements are preserved and retained paths are reported. Staging cleanup failure after publication reports the delivered destinations. This is not an atomic whole-batch transaction; external observers may see files as they are published.

Web supports one named file using the existing browser download hook; directories/batches are rejected. Web preview checks format/source/range policy, not browser filesystem existence or native publication guarantees. Non-Unix native publication, including Windows, is explicitly unsupported before rendering or writing.

WAV and AIFF support 16-bit/24-bit integer and 32-bit float. FLAC supports 16/24-bit integer, up to eight channels and 655350 Hz. Rendering remains at session sample rate (8000–768000 Hz supported); no new sample-rate conversion or quality control is claimed. Integer dither is optional, ignored for float. WAV writes the declared layout channel mask. In the integrated fork, Float32 WAV/AIFF delivery retains finite 1.25/−2.5 headroom exactly through decoding. Track Bounce rejects nonfinite rendered PCM before delivering any file; it does not invoke the codec’s nonfinite sanitization policy. Both outcomes retain the original source PCM, saved identity and session state.

## Bounded policy

All parameters, shape/rates, routing, selected plugin IDs/parameters, ranges and metadata are checked before plugin/render/codec allocation. The source policy caps 128 requested tracks, 10000 active-playlist clips per source, 1024 lanes per source/VCA, 100000 automation points per source/VCA, 100000 clip-gain points per source and 128 Instrument notes per source. The note cap conservatively fits the current fixed 512-event built-in queue, including note edges/boundary release. Tempo maps have at most 1024 valid, ordered events. Clip Effects metadata is capped per clip. Unknown options, incomplete/reversed ranges, missing/unloaded/malformed audio, MIDI controllers, nonfinite metadata and oversized work are errors.

The checked 512 MiB aggregate policy estimate charges processed frames × output width × 12 bytes, selected plugin workspace, Clip Effects state and eight block buffers (including pre-fader-send scratch). Instrument preroll is charged even though discarded. The existing offline sample cap also bounds processing span. This aggregate estimate limits repeated preroll work; it does not guarantee a wall-clock CPU duration. Selected effects are processed without delay compensation; unsupported latency is rejected rather than extending allocations.

Workspace policy is a conservative estimate of current original DSP code, not a formal allocator or physical-memory guarantee. Current Mod Delay reserves 2.01 seconds per channel; time shift reserves approximately one second plus 10000 samples per channel; reverb has two 250 ms predelays, eight room lines and eight diffusers; built-in instruments use fixed voice/event arrays. The estimate reserves four seconds per channel at at least 48 kHz plus 1 MiB per active plugin, covering factory preparation and re-preparation. Native files are rendered/encoded one at a time, but the sum is capped to limit repeated preroll/encoding workload. Any built-in DSP allocation change requires this policy to be reconsidered.

## Command example

```json
{
  "tracks": [1], "path": "/existing/folder/Voice.wav",
  "format": "wav", "bit_depth": "24", "dither": true,
  "render_volume": true, "render_pan": false,
  "start": 48000, "end": 96000, "tail_seconds": 0.5,
  "expected_revision": 7
}
```

Use the same parameters with `track.bounce_preview` for a metadata-only manifest. Export/preview are registered `journal=false`, `undoable=false`.

## Remaining manual options

Aux/Master sources, hosted/licensed processing, real-time/hardware paths, multiple output paths, Windows native delivery, dual-mono/multi-mono grouping, sample-rate conversion/quality, other file/container formats, prefix/suffix and preset workflows, import-after-bounce, automatic Bounce Files placement, and progress/cancellation remain outside this implementation. Audio effect prehistory and latency-producing effects remain unresolved. Existing downstream matrix behavior and voice stealing are shared SoundCraft behavior, not Pro Tools equivalence evidence.

## Revision history

- 2026-10-12: Integrated with Float32 preservation, Mix delivery rate, exact Commit layouts and the combined dialog host. Verified finite WAV/AIFF headroom, nonfinite no-delivery behavior and unchanged subsequent Mix/Commit rendering.
- 2026-10-12: Defined separate immutable Track Bounce file delivery, supported source/routing/range/format policy, preview/revision gate, safety limits and explicit residuals.
