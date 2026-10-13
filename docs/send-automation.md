# Send automation playback

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor · **Workflow:** SC-MX11, partial

**Historical send-only checkpoint:** the policy, test scope and residual table below describe the earlier mono-send implementation. The current [shared prepared automation](automation-playback.md) adds per-sample Volume/Pan/Mute/SendLevel/Plugin consumers, extends ordinary Bus mute/level matrices and introduces a shared static/automated stereo-send balance law. It also replaces per-callback lane scans with prepared curves, sorts imported time points and rejects nonfinite preparation. Its focused references and integration requirements are recorded there; neither arc closes the original SC-MX11 acceptance.

A nonempty SendPan lane now controls each sample of a mono send into an ordinary stereo Bus. A nonempty SendMute lane gates samples at the exact stored breakpoint on ordinary Bus routes, for supported widths up to 16 channels. The shared mix engine supplies playback and offline rendering; this change has synthetic rendering evidence, with no physical audio-device or browser observation.

The original MX-11 acceptance remains: **For every exposed AutoParam, render an automated change and compare the output against a static reference; include send pan and send mute plus seek/loop boundaries.** This feature covers two bounded consumers and does not close that acceptance.

## Initial value and routing policy (historical)

- In a mode for which `AutomationMode::reads()` is true, a valid nonempty lane overrides the send's static pan/mute. A zero SendMute value can unmute a statically muted send. Track mute, solo suppression and VCA mute still suppress the strip, including pre-fader sends.
- Off, absent and empty lanes use the static control. Read, Touch, Latch, TouchLatch, Write and Trim retain the existing reader-mode policy. Recording those gestures is a separate workflow.
- Existing mixer readers ignore `AutomationLane::enabled`. These new readers follow that policy, including old saved lanes with a missing field that deserialize to false. This does **not** implement individual or global automation suspension; a consistent migration across all readers remains open.
- Pan holds the first/last endpoint and interpolates between knots. Mute holds endpoints and steps at a knot; values at or above 0.5 mute. Finite values clamp to each control's range. Duplicate timestamps use the last knot at that timestamp. Nonfinite values or decreasing-time knots cause the whole lane to fall back to its static control, without changing stored data. This is deterministic rendering protection, not full malformed-session diagnostics.
- Only mono-to-stereo Bus pan is newly read. Stereo/multichannel sources, mono/multichannel destinations and Ambisonics retain their static send-pan routing. Send mute is newly read for non-Ambisonic source/destination Bus routes; Main/hardware sends and Ambisonics remain outside this arc. Surround/Follow Main Pan geometry is preserved in the static matrix and is not a new automation consumer.
- Pre/post-fader source selection, send-level law and its existing block-start automation timing, strip/solo/VCA handling, unrelated sends and direct outputs stay consistent. Static sends keep the existing matrix fast path.

`crates/mix/src/send_automation.rs` contains the local reader and two routing helpers. It rebuilds the cursor from each callback's absolute position, so seek and loop wrap cannot carry a stale cursor. Timestamp differences widen to i128 before f64 interpolation; sample advances saturate. Sparse static-matrix taps use stack storage, and the new steady routing helpers allocate, block and log nothing. Lane validation scans its points once per callback; worst-case callback CPU cost for very large lanes and end-to-end allocation/performance measurements remain to be accepted separately. No shared `AutomationLane::value_at` semantics changed.

## Independent verification

Original synthetic signed PCM isolates a source's send to a Bus/Aux return. The expected pan transfer uses elementary trigonometry and the declared pan law, and mute uses fixture-knot lookup; neither expected-value path calls the production lane evaluator or pan helper. Static sessions calibrate pan values. Tests compare every sample, with knots deliberately away from typical callback/grid boundaries.

The mixer suite covers requested chunks 1, 37, 64, 100, 512 and 1024 plus irregular chunks, all four pan laws, already-used engine seeks, loop wraps and tiny final chunks. Separate tests cover static/empty/Off fallback, the explicit enabled compatibility policy, mute threshold, mono/stereo/5.1/7.1.4/16-channel routing, preserved wider static pan, pre/post-fader and fixed level, direct output, strip/solo/VCA suppression, A/J/absent-slot isolation, and nonfinite/unsorted lane fallback. Local unit tests cover extreme i64 interpolation, duplicate/clamped endpoints and short buffers. Engine tests cover existing `automation.set_point`, `automation.write_range`, `mix.send_level` and mode commands, Undo/Redo with a reused renderer, document Arc/revision/dirty/history immutability, folder/bundle save/reopen PCM, and missing enabled-field compatibility.

These are independent SoundCraft references; no proprietary PCM, code, binary, artwork or manual prose is included. Touched-crate checks, full CI attempts and platform limits are reported in the external task evidence, separately from source coverage. No UI pixels or new command IDs were needed.

## Combined fork validation

Whole mono arrangement Repeat with Automation Follows Edit retains the captured SendPan/SendMute curve and audible stereo bus output. The combined regression preserves outside clips/PCM, static send/bus/Aux routing and selection, adds one Undo, and reopens native/bundled strict Copy with the same copied curves and PCM across different chunk sizes. Exports preserve the original document Arc, revision, dirty state, history, macro journal, clipboard and path. This synthetic interaction does not expand the bounded route/layout or platform scope.

## Initial acceptance residuals (historical)

| AutoParam family | Remaining requirement |
|---|---|
| Volume | Full knot/reference timing across ordinary/VCA/Master paths, Write/Trim policies and gain ordering |
| Pan(0/1) | Main/bus block timing, both stereo panners, supported layouts and mode references; upstream PR295 overlaps the main route |
| Mute | Exact ordinary-strip steps and static/lane precedence beyond this send-only arc |
| SendLevel(0..9) | Continuous ramps and mid-block knots, all mode/suspension references; current reader samples block start |
| SendPan(0..9) | Stereo/multichannel sources/destinations, surround/XYZ, Follow Main Pan, Ambisonics, non-Bus routes and truthful unsupported-context validation |
| SendMute(0..9) | Non-Bus/Ambisonic routes, global/type suspension, safing and actual UI/control capture |
| Plugin {slot,param} | Missing/unknown keys/slots, mid-block timing, instruments, trim sentinel, hosted parameter/state/thread behavior and complete built-in references |

The table records the requirements left by that initial checkpoint. Current shared-consumer coverage and the retained EX01–EX10 exposure residuals are tracked in [automation playback](automation-playback.md). All exposed route/control contexts still need the complete original acceptance. Automation writing/prime/preview/safing, global enable/suspend migration, malformed-lane import diagnostics and capability validation remain open. Device latency, realtime callback budgets, hosted plugins, browser playback and Windows/Linux/FreeBSD acceptance are separate; installed-plugin or hardware inventory is outside this feature’s acceptance.

Fresh exact upstream overlap review retained PR295 (main-pan grid in the same route function), PR308 (writer/history), PR362 (static send UI), PR214 (static mute shortcuts), PR290 (editing), PR354 (thinning) and PR326 (offline render bounds). PR295/308 were rechecked immediately before implementation; no upstream code was imported. Future changes must reconcile those scopes explicitly.

## Reference provenance

Behavior was paraphrased from the existing 2026.4 Reference Guide corpus: physical PDF pages 1645/1646/1655/1672, printed pages 1543/1544/1553/1570. The corpus identifies exposed automation families, width-dependent controls, write safing and separate control suspension. Its provenance hashes and exact upstream base/head diffs remain in the external SC-MX11 preflight evidence. No manual text or assets were copied into the repository.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Marked initial mono-send policy/timing as historical and linked prepared shared-consumer follow-up; full acceptance remains partial |
| 2026-10-12 | minor | Added combined Repeat/strict Copy audible-curve regression and retained timing/runtime limits |
| 2026-10-12 | minor | Added bounded send pan/mute consumers with explicit precedence, malformed-lane and suspension policy; full MX11 acceptance remains open |
