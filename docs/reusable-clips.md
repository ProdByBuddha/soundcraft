# Persistent reusable clips

Reviewed 2026-10-13. ED-46's acceptance criterion is: capture a middle range, leave the original
timeline clip unchanged, remove every timeline instance, and place the retained definition
elsewhere. Native acceptance and publication are separate gates; the implementation is under
verification until those gates pass.

`clip.capture` creates session-owned definitions rather than renaming or replacing timeline
clips. Each intersecting selected clip produces its own definition; a selection spanning gaps
does not become a rendered composite. Definitions have typed identities and detached payloads,
without a timeline start, playlist, group or instance identity.

The command interface is:

```json
{"command":"clip.capture","params":{"clips":[3],"start":248,"end":515,"name":"Middle"}}
{"command":"clip.definitions","params":{}}
{"command":"clip.clear","params":{"clips":[3]}}
{"command":"clip.place_definition","params":{"definition":4,"track":2,"at":1003}}
{"command":"clip.clear_definitions","params":{"definitions":[4]}}
```

IDs above are illustrative; use the IDs returned by the commands. Capture returns a
`definitions` array and a count. Placement returns the definition, fresh clip identity, track,
position and resolved length. Explicit track targets must match Audio, MIDI/Instrument or Video
content. Omitted targets create a suitable track. Placement uses the existing explicit
source-placement overwrite convention; searching, drag/drop and additional edit-mode policies
belong to the separate ED-20 Clip List work.

Capture and placement preflight their inputs, references, document budgets and fresh identities
before applying one undoable transaction. Capture leaves original timeline objects and source
assets intact. Instance edits do not change the stored definition. Clearing a definition retains
existing instances and media, clearing their provenance links; Undo restores the definition and
links. Clearing timeline instances retains the definition.

## Media coordinates

An audio definition retains the source identity and stretch ratio. A captured cut keeps its
timeline-relative source origin, including a fractional source position after division by
stretch. Playback reads `offset + phase + (origin + local_sample) / stretch`; the separately
stored phase also preserves fractional unwarped coordinates. Rounding a cut to an integer
file offset would lose that phase.

Gain envelopes retain interpolated values at both cuts and all interior points. Incoming fades
keep the original curve denominator and elapsed phase; outgoing fades keep the original
denominator and distance beyond the captured right edge. A cut inside a fade therefore neither
restarts it nor shortens its curve. Further inward trims preserve these coordinates. Outward
audio handles continue the envelope's edge value and retained fade law, subject to valid source
coverage; they cannot reconstruct discarded envelope points or MIDI events.

Audio rendering evaluates the actual gain law per sample. Constant clips use a flat fast path;
curves use binary-search envelope evaluation and the strip's preallocated scratch buffer shared
across channels. The old block-relative 32-frame approximation could change a curve after a
cut, relocation or different block partition. Callback allocation and native timing need their
own measured evidence; compiling a renderer alone does not establish realtime readiness.

Video definitions retain a movie reference and a cut-adjusted movie offset. They use the
existing linear picture clock; this feature does not add video varispeed. Picture lookup and
thumbnail drawing use the same placed source coordinates.

MIDI definitions crop notes at both selection edges, rebase notes and controllers, preserve
same-tick controller order, and retain an independently cropped saved performance. A crossing
note becomes a note-on at the new beginning: this is rearticulation, without recovery of a
synth's earlier envelope state. MIDI duration is retained in ticks and resolved at the
destination tempo. Controller payload preservation is distinct from controller/channel/release
velocity dispatch, which the current renderer does not fully implement. No implicit controller
chase is promised. Notes ending exactly on a render-block boundary must release at frame zero
of the next block.

Recognized clip-effect and elastic metadata are retained under detached keys and remapped to
each fresh instance. This preserves configuration, without promising the previous instance's
processor history. Unwarped sources remain explicit dependencies, with a source phase and
rendered-coordinate anchor so nested capture and inward trims do not apply a cut twice. Removing
warp restores the retained visible unwarped window; conversion from fractional duration to a
sample length uses explicit rounding. Track inserts, routing,
automation and instrument identity belong to tracks and are not captured into a clip definition.

## Persistence and ownership

Session format version 2 persists definitions, slice coordinates and optional instance
provenance. Valid version-1 sessions load with an empty definition store and upgrade in memory.
Older version-1 applications must reject version 2 instead of silently discarding definitions
when saving. Loading validates definition identities, timebases, geometry, metadata and bounded
payloads; missing media is reported separately from invalid document structure.

Referenced audio remains declared in `Session.sources`, including media retained only by a
definition. Save, strict audio copy and recovery enumerate those declarations. Original-asset
leases remain in the source pool and document/history/prepared snapshots. A source retained by
a definition cannot be cleared through `clip.clear`, even with `with_clips`; clear the definition
explicitly first. Dropping a definition or snapshot performs no filesystem operation. Managed
original collection remains an explicit control-side action.

## Acceptance boundaries

The native acceptance suite uses original synthetic media and independent analytic references:
nonzero multi-channel PCM, fractional stretched cuts, nonlinear fade phases, gain-envelope
boundaries, multiple block partitions and destinations; structural MIDI crops and saved
performance; movie offsets; fresh identities; failure atomicity; real Undo/Redo; save/open,
portable audio copy, recovery and original retention after all instances are removed.

Visual review must cover the placed window's waveform and retained fade geometry. ED-20 search,
browser/platform runtime, physical audio/video devices, arbitrary hosted processor histories and
full MIDI dispatch remain separate acceptance scopes. The project-wide stage and readiness
estimates are not changed by a native clip-capture check.

## Revision history

| Date | Change |
|---|---|
| 2026-10-13 | Defined ED-46 persistent identities, faithful media windows, commands, ownership and acceptance boundaries |
