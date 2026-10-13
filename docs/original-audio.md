# Imported originals and playback audio

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (ED-31 original preservation)

Native Unix audio imports record encoded original bytes separately from the decoded audio used for
playback. Original provenance includes SHA-256, byte count, original sample rate, frame and
channel counts, sample format and parsed BWF metadata. `audio_identity` continues to describe
the session-rate playback PCM; it does not identify the original encoded file.

## Import choices

File > Import > Audio and the Workspace browser open the same policy dialog. Choose:

- **Copy:** snapshot the original encoded file into managed storage before accepting the import.
- **Reference:** retain the external original's path and identity. Keep that file available and unchanged.
- **Convert:** preserve original bytes and create session-rate playback audio.

Copy and Reference require **Allow session-rate playback conversion** for a file whose rate
differs from the session. Convert supplies that choice. Playback conversion never replaces the
original. The clip list's source tooltip shows original and playback properties separately.

The command interface uses the same policies:

```json
{"command":"file.import_audio","params":{"paths":["/takes/voice-44100.wav","/takes/room-96000.wav"],"policy":"convert","new_tracks":true,"at":0}}
{"command":"file.import_audio","params":{"path":"/takes/voice-44100.wav","policy":"reference","rate_conversion":true}}
```

`policy` defaults to `copy`; `rate_conversion` defaults to false. Optional `use_timestamp`
places BWF files at their original timestamp converted to session samples, relative to the
session's timecode start. Missing timestamps, negative session-relative positions and overflowing
placements fail that request. The raw unsigned original timestamp and its original rate remain
recorded.
The original-time clip display uses that recorded rate; legacy sources do not invent one.

All files in a batch are prepared before the document changes. Invalid files, refused conversion,
cancellation and resource failures do not add tracks, clips, history or journal entries. A successful
batch is one undoable import. Preparation binds the current document; stale plans cannot apply.
Receipts report the selected policy and separate original and derivative identities, rates and paths.

## Save and portable copies

Native Save retains owned original encoded files under `Original Audio/`, separate from playback
Float32 WAV files under `Audio Files/`. Referenced originals remain external and are checked
against their recorded identity. Opening a session checks original identity separately from the
playback PCM identity. Original bytes are never reconstructed by re-encoding playback audio.
Ordinary Save can retain unresolved original references and emit original-media warnings.
Recovery retains unresolved provenance as well, so an unavailable original does not prevent
preserving recent edits. Those saves do not claim verified original preservation. Strict portable copies require every recorded original.

Strict Save Session Copy includes verified originals, including external Reference files, and
rewrites paths in the copied document. Missing or changed originals refuse a preservation-complete
copy. The Copy dialog checks availability and size without hashing during UI rendering; it
labels originals `available_unverified`. The explicit `session.save_copy_preview` command
reads and verifies their encoded identity and metadata. Copy repeats verification before output.
Native bundles contain distinct original and playback entries and retain the existing
1 GiB uncompressed-data and 10,000-entry limits. These audio checks do not establish portability
of video, external plugin files or every other session asset.

Older sessions without original provenance continue to open. Their originals are explicitly
unrecorded; copying available playback PCM does not create an original-preservation claim.

## Resource and validation boundaries

Native imports copy encoded originals with bounded buffers and decode from seekable readers.
Decoded PCM and waveform overviews still live in memory: this is not disk-streaming playback.
Preparation checks aggregate memory and managed-original byte budgets. Defaults are 1 GiB
memory and 8 GiB originals; the native import API exposes those budgets. Disk I/O can still fail,
including for insufficient space. The budgets are not measurements of available disk capacity.

Lightweight original-asset leases survive undo, redo and held session snapshots. Collecting expired
temporary originals is an explicit control-side operation; lease destruction performs no filesystem
I/O. A missing playback derivative does not release its original's lease. Published session
originals are excluded from temporary collection. Concurrent hostile
filesystem replacement and arbitrary codec allocation behavior remain separate limits.

Temporary snapshot cleanup currently proves ownership with Unix device/inode identity. On
non-Unix platforms it refuses cleanup rather than deleting a file it cannot prove it owns.
Windows Reference imports and subsequent staging after expired snapshots are therefore not
ready; a safe non-Unix ownership backend remains open. Native Unix acceptance does not close
that platform gap.

Acceptance tests exercise independent mixed-rate BWF fixtures, opaque chunks and unsigned
timestamps above 32 bits, byte-exact original retention, explicit policies, batch atomicity,
checked conversion, reference tampering, leases and portable reopen. UI tests select each policy
through the actual controls and verify refusal and cancellation. Native folder and offscreen UI
checks do not establish live browser storage, physical device acceptance or conversion fidelity
equivalent to another DAW. New original-preservation bundles require native Unix storage; browser
support for that provenance is an explicit remaining gap.

## Revision history

| Date | Change | Notes |
|---|---|---|
| 2026-10-12 | minor | ED-31 ownership, original provenance, atomic import and separate original/playback preservation contract on native Unix; non-Unix ownership, browser and broader session portability remain open |
