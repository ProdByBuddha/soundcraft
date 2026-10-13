# Automation playback

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor · **Workflow:** SC-MX11, partial

The shared renderer evaluates Volume, Pan, Mute, SendLevel, SendPan, SendMute and populated insert Plugin curves from the absolute sample position. Preparation pairs an immutable session with its compiled curves and processor bindings. Playback and the supported offline paths use that pair, including seek and loop boundaries.

The original 202-feature audit acceptance remains: **For every exposed AutoParam, render an automated change and compare the output against a static reference; include send pan and send mute plus seek/loop boundaries.** Supporting all seven families in ordinary audible paths does not close every exposed track, route, width or control context. SC-MX11 remains partial; the exposure residuals below remain open. Stage, readiness figures, milestone completion and effort estimates are unchanged.

## Sample and value policy

| Family | Shared consumer and boundary |
|---|---|
| Volume | Evaluate linear dB at each sample on ordinary strips, linked VCA gain and active Main Master Faders. Master gain remains before serial inserts; ordinary inserts remain before their fader. |
| Pan | Sample the existing scalar panners for actual one- or two-channel strips where the output matrix consumes them. Both stereo panner indices are distinct. Wider/spatial overrides retain their own matrix policy. |
| Mute | Apply held steps at the knot on ordinary strips and linked VCA suppression. Static track mute, solo suppression and static VCA mute remain safety gates. |
| SendLevel | Sample linear dB on existing Bus send routes, including their existing wide/Ambisonic matrices. Pre/post-fader source selection is retained. |
| SendPan | For actual mono sources, sample the existing panner into the destination matrix. For actual stereo sources, use a center-preserving balance before the existing matrix/fold: center retains both channels, each endpoint suppresses the opposite channel. Static and automated stereo sends share this intentional extension. Source widths above two and spatial overrides are not generalized scalar pan consumers. |
| SendMute | Apply held steps on existing Bus send routes. An unmuted lane value can override a statically muted send; strip/solo/VCA suppression still applies, including pre-fader sends. |
| Plugin | Bind exact registered parameter IDs in populated A–J insert slots, including parameters absent from the stored value map. Continuous curves request one value per ID per sample span where needed; active non-bypassed processors retain their own state and smoothing. |

Continuous curves hold endpoints and interpolate in their plain control units. Mute and SendMute hold their value until the next knot. The last duplicate point at a timestamp wins. Time differences use widened arithmetic and absolute sample advances saturate. Imported decreasing-time points are sorted in the prepared copy without rewriting the document; nonfinite points reject preparation and preserve the last published valid pair. Unknown imported addresses remain stored without becoming audible consumers.

Plugin Toggle and Choice curves remain linear in plain units, followed by registered setter quantization. They are not converted into held discrete curves: Repeat/write/thin boundary semantics remain the existing generic Plugin contract. Independent controlled quantization references do not establish every vendor's mapping or smoothing behavior.

The existing reader compatibility is explicit: Off uses static values; readable modes keep their current behavior, with Volume using the static fader in Write while other readable controls retain their existing policy. `AutomationLane::enabled` is still ignored by these readers, including old saves that deserialize it as false. This is not global/type suspension, safing or a complete gesture-writing workflow. Internal Trim and dedicated instrument addressing retain the restrictions below.

## Static baselines and processor lifetime

A processor's initial parameter seed is captured once after state restoration and preparation, before document or automated targets. Each binding uses a present explicit stored value first, otherwise that original seed, with registered default fallback. Clear or Off restores that static baseline rather than recapturing the most recently automated value. Exact-case IDs and same-length key changes rebuild the appropriate bindings.

Ordinary edits retain matching live processors, seeds, editor handles and appropriate processing history. A changed document lifetime, effective block size, sample rate, plugin or channel identity prepares a replacement off the callback. Editor handles are closed and removed for removed/replaced instances even when the replacement has no editor. A failed preparation does not publish a half-updated session/plan pair.

Typed prepared and fallible offline delivery paths reject unavailable processors. The existing infallible `render_range` and `render_bus` helpers preserve dry passthrough for missing instances, using the same compatibility policy for their latency probe and final renderer. Capacity, catalog, target and processing failures retain their error handling.

A static or Off-mode built-in compressor lookahead binding supplies Aux latency before first-block routing, including retained-instance edits. This reads the prepared value without extra parameter setters or delivery acknowledgments. Readable dynamic lookahead and generic dynamic PDC remain in EX09.

Locate and loop wrap resample absolute curve targets while retaining insert state. Explicit Play/Stop/reset applies the processor's declared reset behavior; the controlled Gain reference models its existing 20 ms recurrence and reset to the last target. This is not a generic vendor reset or tail guarantee. The dedicated instrument remains unsplit so its whole-block note scheduling is preserved; it is not aliased to insert A.

Prepared CLAP parameter/note storage uses checked P+N capacity; VST3 pending and FFI parameter storage cover P, with separate bounded note storage. The supported envelopes are 4096 registered parameters and 1024 span-local notes. Actual processing receipts distinguish requested values from successful delivery; failed delivery does not advance the core's applied-value cache and retries current absolute targets. AU setter/render results are checked, but controlled AU failure injection and arbitrary external components remain separate acceptance.

## Ownership and limits

Preparation checks a conservative 128 MiB budget per adapter workspace before controlled allocations and processor creation; actual bounded latency adds PDC charges before reservation. This estimate includes compiled point/container storage, parameter bindings/caches, scratch buffers and hosted queue estimates. It excludes immutable document/pool memory, processor DSP state, registry/vendor allocations, allocator bookkeeping, control-side command envelopes and multiple queued/retired workspaces. It is not measured total process memory.

Playback circulates one preallocated boxed audio state through a capacity-one try-only exchange. Prepared commands use envelopes allocated on the control thread. The callback moves retired workspace fields into that same envelope and returns it intact to the control owner for disposal. Failed retirement retains ownership and silences further processing. Control teardown requests shutdown, pauses/drops the stream, joins its owned native silent clock, reclaims the state, closes editors and disposes active/queued resources while the retirement receiver remains alive. A failed stream-build producer stays disabled; a WASM player using the unavailable silent fallback rejects publication.

Pinned WebAudio teardown closes asynchronously, so the small empty callback guard may outlive control reclamation. Native shutdown waits for an already running callback. Vendor/backend hangs, panics, physical-device behavior and browser closure release are not established by headless tests. Existing message transitions, capture paths and backend/controller code remain separately qualified; this is not a universal allocation-free, lock-free or audio-deadline guarantee.

## Verification boundary

Focused native suites `automation_prepared`, `automation_prepared_contract` and playback `automation_tests` cover paired snapshots, static seeds, mode compatibility, transport and controlled ownership. Mix and Engine `automation_seven` define the seven-family PCM and command/persistence matrix. Synthetic references use original linear/step/pan/dB math and the independent Gain recurrence, with channel/frame shapes, finite output and audible contrasts against wrong behavior. Private playback tests exercise actual AudioState transport and native silent-clock ownership without opening an audio device.

The owned CLAP/VST3 `automation_delivery` suites and Mix `automation_hosted` use actual processor receipts, independently calculated affine stereo PCM and injected processing failures. The Mix regression verifies all 4096 parameter deliveries, finite silence on refusal, unchanged processor state, resend of failed current targets and retention across a document-only reprepare. Host receipts also check note counts/times. The VST3 fixture proves one point at offset zero per span, not multi-point queue timing; controlled AU failure injection remains open.

Standalone allocator-helper tests and controlled callback counters have independent positive/zero controls. They cover the test binary's calling-thread Rust allocator on measured controlled spans, not arbitrary vendor/FFI allocations or audio deadlines. These native references establish SoundCraft behavior within the stated scope; no proprietary processing code, PCM or manual assets are used.

Integration requires all six gates of `cargo +1.95.0 xtask ci`: format, strict workspace Clippy, unfiltered workspace tests, assets, dependency layers and WASM compilation. The feature PR records the actual full-CI result, exact source/test freeze, focused runs and loader/provenance evidence. Focused native references do not substitute for that integration gate or establish other-platform, browser, external-plugin or physical-device acceptance.

## Original exposed-context residuals

| Audit ID | Remaining scope |
|---|---|
| EX01 | Master Pan/Mute/send lanes and alternate target/routing. Main Master Volume/insert order does not make these controls audible. |
| EX02 | VCA Pan, sends and insert graph, plus inactive VCA ownership policy. Linked sample gain/mute does not implement an ordinary VCA audio strip. |
| EX03 | Pan1 on actual mono, scalar panners on wider sources, SurroundPan overrides and stereo-to-Ambisonic spatial semantics. Nominal mono instruments may have actual stereo processing width; UI/exposure policy must reflect the actual consumer. Main output currently has a two-channel minimum; true one-channel Main remains open. |
| EX04 | General spatial SendPan, source widths above two, SurroundPan/XYZ/Follow Main Pan linking and remaining width policies. Sample SendLevel/SendMute and the mono/stereo balance extension do not close every exposed pan context. |
| EX05 | Main/None/Hardware send targets, hardware/multi-output direct routing and missing Bus handling. Existing Bus consumers do not establish device routing. |
| EX06 | A–Z/numeric slots outside A–J, empty or unknown populated targets, future imported addresses and hosted Edit-menu/range capability. New atomic refusal and inert round-trip data are not audible acceptance. |
| EX07 | Video model/UI/audio-path disagreement and Midi mixer controls without an audible source; external MIDI automation/device paths. Silent output is not positive consumer proof. |
| EX08 | Internal `Plugin(slot255,trim)`, typed Trim and dedicated-instrument automation addressing, plus Master Trim. The instrument remains a separate unsplit processor. |
| EX09 | Enabled/suspend/safe/write migration, complete gesture capture, generic processor smoothing, dynamic latency/PDC, hosted mapping and real-time/vendor/platform acceptance. |
| EX10 | Save while automation is active with no canonical stored static key: opaque state can contain the last automated value. A live lifetime seed and Save after clear/Off do not solve that persisted-baseline policy. |

These residuals preserve the original acceptance rather than narrowing it through hidden controls or validation refusals. Independent Pro Tools fidelity, all native platforms, browser playback, external plugins and physical audio devices remain separate gates. The earlier send-only checkpoint is recorded in [send automation](send-automation.md), and the historical signal-order acceptance in [Master post-fader inserts](master-post-fader.md).

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Recorded shared prepared seven-family consumer policy, lifecycle, explicit missing-instance compatibility and static Aux latency boundaries, focused native references and full integration requirement; exposed-context residuals and stage/estimates retained |
