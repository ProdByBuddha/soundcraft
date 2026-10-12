# Fork integration scope

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (combined fork scope and remaining validation)

## Fork integration, 2026-10-12

The first fork checkpoint combined eight published changes while retaining each source commit and its history.
The next wave adds reviewed Track Bounce file delivery on that verified head.
This is source integration with synthetic regression coverage; hardware recording, installed
licensed plug-ins, platform runtime acceptance and Pro Tools fidelity remain separate checks.
Stage, readiness percentages and remaining-hours estimates have not been remeasured.

The fork's `main` branch is our ongoing integration target. Start new feature branches from
its verified current head. The coordinating agent reviews exact source, evidence and combined
interactions before a history-preserving merge and push to fork `main`. Upstream contributions
remain a separate workflow with their own review and acceptance state.

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
| [Bounce Mix delivery rate](bounce-sample-rate.md) | Independent bounded output rate, conversion and output-rate measurements | Dither/noise shaping, layout-aware loudness weighting and shared renderer workspace limits |

The recorder keeps the isolated recovery directory and propagates allocation errors. Session
saving retains byte-validated recovery reuse plus identities of the actual saved PCM. Relink
fixtures retain finite 1.25 headroom. Relink and batch naming each keep their own errors/retry
state and close only on successful document replacement; batch previews keep their revision
checks and MIDI editing guards. Bounce retains its own delivery controls in the combined host. Track Bounce retains its independent
preview and cancellation policy. Its tail source-stop and volume-bypass flags do not change later
ordinary Mix or exact-layout Commit renders.

Required `cargo xtask ci` was attempted before every merge. Its unchanged video-codec Clippy
error stops the workspace check before tests, assets, layers and WASM. Separate scoped tests,
strict touched-crate lint, format, assets, layers and WASM checks are recorded in the local
integration evidence; no complete workspace CI pass or physical-device acceptance is claimed.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Added Track Bounce integration scope and verified Float32/rendering boundaries |
| 2026-10-12 | minor | Combined the eight published workflows and their residual limits; retained the existing readiness estimates |
