# Safe audio Heal

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (bounded ED-48 correctness slice)

`edit.heal` joins ordinary neighbouring audio clips on selected tracks' active playlists when
their timeline and source windows are exactly contiguous. The source file, gain and mute state
must match, and stretch must be exactly 1. The combined timeline/source bounds must fit in a
signed sample count. It retains the left clip's ID, name, rating, colour and sync point and
extends its length; removed right IDs in the selection map to the final left survivor. Duplicate
selected IDs are collapsed in their existing order. Other tracks, alternate playlists and
selected sources retain their state.

The existing command accepts `track` or `tracks`, and `start`/`end`, falling back to the selected
tracks and edit range. A seam on either range endpoint is included. For compatibility, an
insertion point includes seams at that sample and one sample on either side; expansion saturates
at signed limits. The operation plans all selected tracks before mutable document access and
creates one Undo step when at least one seam heals. A command with no eligible seam leaves the
document Arc, revision, dirty state, Undo and Redo unchanged. Normal command journaling continues.

The returned object retains `healed` and adds `skipped`:

```json
{"healed": 1, "skipped": [{"track": 7, "left": 12, "right": 13, "reason": "unsupported_fade"}]}
```

Skipped pairs have stable reason codes: `unsupported_content`, `invalid_clip`, `duplicate_id`,
`arithmetic_overflow`, `not_contiguous`, `different_source`, `gain_mismatch`, `mute_mismatch`,
`unsupported_stretch`, `unsupported_fade`, `unsupported_envelope`, `locked`, `grouped`, or
`clip_metadata`. A mixed selection can heal supported seams while retaining unsupported pairs.
No candidate boundary produces an empty `skipped` array. Playlist order is preserved; Heal
does not sort or repair malformed playlists.

Any nonzero fade length, gain envelope, clip effects/bypass, edit/time lock, group or per-ID
elastic/pitch/BPM/original-length history is refused. Unexpected MIDI performance history
attached to either audio ID is refused too. Even outer fades are outside this slice: the current
renderer approximates gain in clip-relative segments, so removing an unaligned seam can change
their rendered samples. Refusal keeps the original audio and metadata. Ordinary right-clip
presentation metadata disappears with that merged identity; retaining several sync points is
not supported.

`crates/engine/tests/heal_safety.rs` exercises synthetic mono/stereo output across unaligned
splits, nonzero gain/source offset, muted clips, selection remapping, alternate and unselected
playlists, mixed selections, one Undo/Redo, persisted clip bounds, processing/metadata refusal,
complete-ID key matching, malformed/overflowing arithmetic and range/point boundaries. These
are engine and renderer regressions; they do not establish native shortcut behavior or physical
Pro Tools fidelity.

The behavior reference is [Reference Guide PDF1018 / printed916](https://resources.avid.com/SupportFiles/PT/Pro_Tools_Reference_Guide_2026.4.pdf#page=1018),
which describes healing separated material from the same audio file while its boundaries remain
in place. [PDF1318 / printed1216](https://resources.avid.com/SupportFiles/PT/Pro_Tools_Reference_Guide_2026.4.pdf#page=1318)
also discusses restoring stripped material. This implementation is independently written from
that behavioral description, with original synthetic test audio and no proprietary assets.

Remaining scope: faded/enveloped/effected/stretched material, MIDI/video Heal, restoring silence
removed between clips, richer sync/presentation preservation, reusable captured clip definitions,
strict selection-spans-both-clips parity and the existing Heal shortcut issue. ED-48 and G-04
remain partial; stage, readiness percentages and hours estimates are unchanged.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Added bounded safe audio Heal, no-op guarantees and advanced workflow residuals |
