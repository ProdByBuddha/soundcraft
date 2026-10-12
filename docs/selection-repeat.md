# Selection Duplicate and Repeat

Updated 2026-10-12. This is a bounded arrangement engine arc within ED-56; broader manual parity remains partial. It is original clean-room behavior derived from public feature documentation and SoundCraft's source, without proprietary code or assets.

`edit.duplicate`, `edit.repeat` and `edit.duplicate_extend` use one immutable plan. Targets are deduplicated audio/MIDI/Instrument tracks in their arrangement master views. The selected time range is captured once; Repeat never reads copies it has just placed. Repeat accepts an integral number from 1 through 999, including integral JSON floats sent by the existing dialog. Wrong types, fractions and out-of-range values fail before document mutation.

In Slip, the whole copied destination interval is replaced, including leading, interior and trailing silence. Uncovered destination material cannot leak into copied silence. Outside material is retained, with safe plain-audio fragments where needed. In Shuffle, the original selection end is one insertion point; a crossing plain-audio clip is split and later selected-track material moves once by the full inserted duration. Unselected tracks and alternate playlists remain unchanged.

With Automation Follows Edit on, each nonempty lane copies interpolated start and final-included-sample values and interior breakpoints. A preceding anchor and a restored final destination anchor preserve surrounding curves. Each iteration starts from the original captured curve, and Mute/SendMute remain stepped. Later automation ripples with Shuffle material. Empty lanes remain empty and lane enabled state is preserved. With the option off, lane objects remain unchanged in both modes.

Whole clip copies retain their stored fades, gain, envelopes, stretch, names, rating, color and sync point. Fresh IDs receive exact-delimiter copies of `clip_fx.<id>.*`, bypass flags, `elastic.<id>.*`, clip tempo/length/pitch metadata and original MIDI performances. Destination material removed completely loses its known per-ID metadata; other playlists and similarly named IDs remain intact. This preserves stored processing settings, not live effect history. Partial processed clips, partial MIDI, locks/groups and unsupported restoration fragments fail atomically; they are not flattened or partly applied.

Duplicate selects its new copy range and IDs. Duplicate and Extend includes the original time range and copy, keeping retained selected IDs and adding copied IDs. Repeat preserves the previous edit selection and retained selected IDs. Linked timeline selection follows the resulting edit selection. Each successful operation uses one Undo; errors preserve the original document Arc, revision, dirty status, undo/redo, journal and clipboard. The clipboard is never used as temporary repeat state.

## Musical placement

`timebase: "samples"` requests exact sample spacing. `timebase: "ticks"`, a Bars|Beats main counter, or explicit bar-formatted boundaries requests musical placement. Musical iterations are calculated from absolute original tick boundaries, rather than by repeatedly adding one rounded sample duration.

This first arc accepts only exactly round-tripping musical boundaries at a constant tempo where every converted iteration has the same sample width. A fractional-width sequence or tempo change fails before mutation. Whole MIDI content must fit its positive clip tick span; notes/controllers and saved original performances remain intact. MIDI across tempo changes or Grabber note selection requires a separate policy and is refused. UI note indices are not interpreted by these arrangement commands.

## Bounds and residual work

The planner caps 4096 targets/tracks, 100000 document/plan clips or playlists, 1000000 aggregate automation/MIDI/envelope events, 65536 metadata entries, 128 MiB of estimated document payload, and two million clip/metadata comparisons per planning phase. Destination fragments and repeated deep MIDI data contribute to the plan limits. Timeline results must fit the supported nonnegative command range; all duration, identity and placement reservations are checked. Existing metadata/performance IDs also prevent fresh-ID collisions. Stored sample rate, tempo maps, automation interpolation and parameter dimensions are checked before using timing or label helpers. These conservative limits do not bound total process memory, the original session, or the Engine's copy-on-write snapshot.

Remaining ED-56 acceptance includes fractional-sample musical-width material mapping, variable-tempo audio/MIDI transformations, automation-only views, Time Grabber note merging, processed partial fragments/crossfades, richer locks/groups, marker/tempo/meter event repetition and a Pro Tools output comparison. General Cut/Paste/Clear, Paste Repeat to Fill and global song-time insertion use independent paths. Mixer regression evidence uses synthetic stereo DC, a nonconstant volume ramp, both edit modes and a 16-sample block. It compares each copy with independently calculated PCM and later preserved/rippled material. The existing mixer interpolates gain between each block's endpoints and samples Mute/Pan/Send/Plugin control at block boundaries; this arc does not change that timing policy or establish sample-exact incumbent behavior for arbitrary breakpoints or hosted effects.

No readiness percentage, hours estimate or project stage is changed by this arc.

## Combined fork validation

Synthetic stereo headroom material with copied per-clip gain/bypass metadata survives native and bundled strict Copy with its clip identities, metadata and saved-source identity intact. Original and repeated Track Bounce Float32 WAVs decode to the same 2-channel, 64-frame PCM as each expected source range. Heal refuses the processed copied seam while preserving document identity, dirty state, Undo history and revision-bound Copy/Track previews. Successful no-op Heal retains its existing macro-journal entry; previews and exports add none. This is bounded synthetic acceptance, not hosted-effect history or Pro Tools fidelity.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Added combined Copy/Track Bounce/Heal metadata and decoded-output regression coverage |
| 2026-10-12 | minor | Added immutable bounded arrangement Duplicate/Repeat/Extend planning and explicit residual scope |
