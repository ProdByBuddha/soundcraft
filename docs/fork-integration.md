# Fork integration scope

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (combined Send/Repeat playback, verified CI boundary and DCO workflow)

## Fork integration, 2026-10-12

The first fork checkpoint combined eight published changes while retaining each source commit and its history.
Later waves add reviewed Track Bounce file delivery, strict Save Session Copy In, safe audio Heal,
bounded selection Repeat and send pan/mute playback on that verified head.
This is source integration with synthetic regression coverage; hardware recording, installed
plug-ins, platform runtime acceptance and Pro Tools fidelity remain separate checks.
Stage, readiness percentages and remaining-hours estimates have not been remeasured.

The fork's `main` branch is our ongoing integration target. Start new feature branches from
its verified current head. The coordinating agent reviews exact source, evidence and combined
interactions before a history-preserving merge and push to fork `main`. Upstream contributions
remain a separate workflow with their own review and acceptance state. Every new topic, merge and documentation
commit uses `git commit -s` with the actual author identity. Audit new PR commits for matching
`Signed-off-by` trailers before merging. For an unmerged branch with no downstream work, a
message-only amendment may use an explicit old-head force-with-lease; verify the tree is unchanged.

| Workflow | Integrated behavior | Remaining scope |
|---|---|---|
| [Recording allocation](recording-disk-allocation.md) | Session/per-track paths reach fallible native take planning; unavailable absolute folders fail explicitly | Physical multi-volume/full-volume tests, volume rotation/roles and allocation editor |
| [Track Commit](commit-multichannel.md) | Exact channel formats and complete render dimensions retained through the supported 16-channel mixer | Wider renders, mono gain parity, raw-source WAV masks and additional Commit policies |
| [Recovery](autosave-recovery.md) | Separate per-open-document directories and exact-byte reuse preserve prior source-ID recordings | Interval/retention controls, asynchronous saving, cleanup and unavailable media |
| [Manual audio relink](media-relink.md) | PCM identity/metadata checks, explicit legacy consent and document-scoped retry behavior | Automatic/batch search, video/interchange relink and raw file-byte fidelity |
| [Float32 files](float32-file-preservation.md) | Finite headroom preserved through files, recordings and generated-source persistence | Broader depth/dither choices and delivery analysis |
| Stored track names | Single rename checks uniqueness after 64-character truncation and returns the stored name | Folder/output-bus label synchronization is a separate workflow |
| [Batch track naming](batch-track-rename.md) | Literal replace/trim/add/number stages preview and apply atomically with one Undo | Advanced transforms, presets, clip/file naming and Score/MIDI context entries |
| [Track Bounce files](track-bounce-files.md) | Immutable selected Audio/built-in Instrument export, exact finite Float32 headroom, nonfinite no-delivery policy and source-stop tails | Unix hard-link publication and one-file web subset; hosted/Aux/Master/real-time/Windows paths, rate conversion and further manual options |
| [Save Session Copy In](session-copy.md) | Complete declared audio, validated references and destinations, actual saved-PCM identities and detached export snapshots preserve the open document | Audio conversion/timing options, original file bytes/BWF, video and external plugin files, whole-copy rollback and browser download retention |
| [Safe audio Heal](heal-safety.md) | Contiguous ordinary audio splits preserve rendered samples, remap selection and use one Undo; unsupported processing returns skipped reasons without consuming history | Faded/enveloped/effected/stretched clips, MIDI/video, removed-silence restoration, richer presentation/sync points and selection/shortcut fidelity |
| [Selection Duplicate and Repeat](selection-repeat.md) | Immutable full-range material/automation planning, exact copied per-clip metadata, silence replacement and aggregate Shuffle ripple | Fractional-width or variable-tempo remapping, partial processed clips, note-selection and broader manual policies |
| [Send automation playback](send-automation.md) | Per-sample mono-to-stereo ordinary-Bus SendPan and stepped ordinary-Bus SendMute; repeated curves retain audible output and strict Copy persistence | Full AutoParam/reference matrix, wider pan/non-Bus/Ambisonic contexts, suspension/writing, callback budgets and platform/plugin acceptance |
| [Bounce Mix delivery rate](bounce-sample-rate.md) | Independent bounded output rate, conversion and output-rate measurements | Dither/noise shaping, layout-aware loudness weighting and shared renderer workspace limits |

The recorder keeps the isolated recovery directory and propagates allocation errors. Session
saving retains byte-validated recovery reuse plus identities of the actual saved PCM. Relink
fixtures retain finite 1.25 headroom. Relink and batch naming each keep their own errors/retry
state and close only on successful document replacement; batch previews keep their revision
checks and MIDI editing guards. Bounce retains its own delivery controls in the combined host. Track Bounce retains its independent
preview and cancellation policy. Its tail source-stop and volume-bypass flags do not change later
ordinary Mix or exact-layout Commit renders.
Strict Copy blocks missing or invalid audio before output while ordinary Save and recovery retain
their permissive missing-media policy. Native and bundled copies retain finite Float32 headroom
and signed zero, sanitize nonfinite samples, and derive saved identities from the actual saved PCM.
Copy preserves original source identities, document revision, saved path, dirty state, Undo/Redo,
journal and opaque plugin snapshots. Its preview revision, document replacement and same-frame
cancellation guards coexist with Track Bounce, Batch Rename and Relink in the combined host.
Heal preserves Copy's source identities and exact Track Bounce bytes for supported headroom
material. Native and bundled copies of the healed playlist reopen with the surviving clip selection
and unchanged PCM bits. A refused Heal keeps Copy and Track Bounce revision-bound previews current.
Repeat gives copied clip gain/bypass metadata fresh identities. Native and bundled Copy retain that
metadata and saved-source identity, while Track Bounce renders the repeated range like the original.
Heal refuses those processed copied seams without changing the document, Undo history or revision-bound
Copy/Track previews. Successful no-op Heal is still recorded in the existing macro-command journal;
previews and exports add no journal entries.
With Automation Follows Edit enabled, whole mono repeats retain SendPan/SendMute curves and
the original audible bus output, while outside material and static routing remain unchanged.
Native and bundled strict Copy reopen with the copied curves and the same stereo PCM. Strip Mute,
main Pan, SendLevel and Plugin controls retain their existing block timing; this bounded send
consumer coverage remains separate from full automation and incumbent-fidelity acceptance.

Required `cargo xtask ci` was attempted before every merge. The first checkpoints stopped at
the video-codec Clippy error. An equivalent Mp3 match guard now removes that error, and the
combined native workspace passes all-target Clippy with warnings denied. Required CI attempts
are stopped after Clippy and before broad native installed-plugin inventory tests. An earlier
installed VST3 scan stalled and emitted a PACE diagnostic; the exact plugin or blocking condition
was not confirmed. Full workspace tests and a complete CI pass
remain unverified. Scoped synthetic tests plus separate format, assets, layers and WASM checks
are recorded in the local evidence; physical-device acceptance remains separate.

When transferring a build cache between checkouts, verify the `xtask` runner's compiled repository
root and rebuild that runner from the current checkout if needed. Its root is fixed at compilation;
a stale runner can check the previous checkout even when Cargo is invoked from the new one.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Added Send/Repeat/strict Copy curve and audible-output regressions; qualified timing and installed-plugin evidence |
| 2026-10-12 | minor | Added bounded selection Repeat and copied-metadata Copy/Track/Heal interaction coverage; estimates unchanged |
| 2026-10-12 | minor | Recorded verified video lint maintenance, the native-test runtime boundary and required DCO signoffs |
| 2026-10-12 | minor | Added safe audio Heal integration and cross-feature Copy/Track Bounce identity, output and no-op preview regressions |
| 2026-10-12 | minor | Added strict Copy integration, saved-PCM identities and combined immutable-state/dialog regressions |
| 2026-10-12 | minor | Added Track Bounce integration scope and verified Float32/rendering boundaries |
| 2026-10-12 | minor | Combined the eight published workflows and their residual limits; retained the existing readiness estimates |
