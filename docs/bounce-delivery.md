# Bounce Mix fidelity and file delivery

Last reviewed: 2026-10-13. Last updated: 2026-10-13.

Manual-analysis row MX-38 remains partial. Its acceptance criterion is unchanged:

> Compare exported fixtures against live capture for built-ins and representative stateful hosted plugins, checking selection, compensation, tails, channel metadata and saved editor state. Require exactly the requested frame count, or reject oversize requests before writing; validate the channel-dependent allocation boundary and truthful reported duration.

## Rendering and capture contract

`file.bounce_mix` has a strict shared parser and pure allocation/work preflight. It captures
the edit selection, or the session when none is selected, plus explicit `tail_samples` in
session-rate samples. An omitted `preroll_start` retains the existing cold-selection API
behavior by starting at the capture start. An explicit earlier start renders effect history.
The dialog's checked history control explicitly starts at session zero, or at the capture
start for a selection before zero; turning it off requests cold effect state. History and tails consume the
same checked processing budget; oversized or malformed requests fail before writing.
The delivered rate can differ from the processing/session rate, with rounded exact frame
counts and truthful `seconds = frames / sample_rate`.

Inactive or bypassed instruments produce no output. A disable transition delivers held-note
releases through the prepared channel shape before resetting; a rejected release stays
silent and blocks re-enable until delivery succeeds. Re-enable schedules future note-ons,
without inventing a chase of earlier notes. Active bypassed slots still retain opaque state
and missing static parameter defaults in export snapshots.

The selected source ends at the selection boundary. Tail rendering continues processor
history with source audio and new MIDI notes stopped. Delay compensation must account for
the actual selected route, including instruments, serial Aux paths, sends and the Main
Master chain. The output's finite PCM shape and encoded rate/channel/frame header are
checked before publication. These are adapter bounds, not a total vendor/codec memory or
processing-time guarantee.

Native UI export requests a fallible snapshot bound to the exact prepared document and
generation. It clones opaque states, immutable prepared defaults for absent static keys,
and actual pending editor gestures into a detached engine. Explicit stored parameters take
precedence over defaults, then actual editor gestures apply. A transient Read automation
value must not become a fresh instance's static baseline. Failed export polling preserves
valid drained editor gestures for normal UI handling. Export does not update the original
document, Undo/Redo, revision, dirty state, transport, or command/recovery journal.

During playback, capture waits for the next naturally rendered block, then polls its editor
events before returning the detached snapshot. It never forces DSP or advances the transport
itself. A failed render, stale binding, interrupted synchronization or busy request is an
explicit error. Stopped capture reads committed processor state; unprocessed future editor
events and general vendor parameter-flush behavior remain separate acceptance work.

Opaque state requests call vendor serialization between audio blocks. They can allocate
and take vendor-defined time; this is not a steady-state callback latency guarantee.
Missing state, stale prepared identity, missing workers and unanswered requests are explicit
errors. CLI calls use the stored document. Browser builds cannot synchronously capture live
hosted state and report that limitation; builtin browser export uses the stored document.

## Destination contract

Native export requires a fresh output file. Existing files, dangling links and recorded
original/persisted-playback locators are protected, including recorded locators whose file
has gone missing. A canonical-parent plan is checked again before staging and publication.
Output is exclusively staged in that directory, synced and verified, then exclusively
published through a hard link or a create-new copy fallback. No destructor deletes files.

Cleanup removes only files with a demonstrated ownership identity. Where that proof is
unavailable, the receipt/error reports retained paths. Concurrent arbitrary path replacement
and filesystem-specific Unicode aliasing need separate acceptance; conservative ASCII
case comparisons can refuse additional names. Windows naming validation and compilation
are distinct from observed Windows publication. Browser receipts establish only that the
existing download hook accepted the request, not an observed downloaded file.

WAV/AIFF retain 16/24-bit integer and Float32 delivery; FLAC retains its integer channel/rate
bounds and typed Int32 support. Float32 FLAC requests preserve the encoder's unsupported-option
error before rendering; the command's `32f` option requests float, not integer audio.
The direct byte/delivery API accepts explicit BWF metadata; ordinary Bounce Mix
does not invent a source timestamp for a mixed output. Loudness metrics describe the final
output-rate PCM before quantization. Channel-layout loudness weights remain a separate gap.

## Acceptance boundaries

Acceptance work includes independent literal PCM/BWF wire fixtures, the actual production
callback path, stateful owned host fixtures, a real editor interface, variable callback
sizes, history/cutoff/tail/PDC routing, source/destination failure cases, retained artifacts,
functional Redo, and actual dialog dispatch/rendering. Test definitions and formatting alone
do not establish these outcomes; the tested source revision and full CI results must be
reported together.

An invoked production callback fixture is distinct from audio recorded from a physical
device. Real device capture, arbitrary third-party modules, cross-platform runtime,
browser downloads and Pro Tools fidelity remain separate gates. Track Bounce, stems,
real-time delivery, progress/cancellation, noise shaping and the broader manual mixdown
options are not closed by this arc. SoundCraft stays pre-alpha; no readiness percentage or
hour estimate is remeasured.

## Revision history

- 2026-10-13: Defined full MX-38 rendering/state/delivery acceptance, detached export and
  explicit filesystem/runtime limits; retained the original criterion and partial status.
- 2026-10-13: Retained cold selection for existing API calls, made the dialog's history
  request explicit, and preserved typed integer FLAC options and encoder error context.
