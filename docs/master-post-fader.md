# Master Fader post-fader inserts

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (SC-MX02 native reference acceptance) · **Target:** Avid Pro Tools Ultimate 2026.4.1

The original SC-MX02 acceptance is: “Use a nonlinear limiter or dither on a Master Fader and change the fader; verify the chosen post-fader signal order and compare an independently calculated reference.” The native regression uses an original nonlinear limiter reference and synthetic audio; no proprietary processing code or assets are used.

The manual places ordinary track inserts before the fader and Master Fader inserts after it ([Reference Guide 2026.4, printed page 1487, PDF page 1589](https://resources.avid.com/SupportFiles/PT/Pro_Tools_Reference_Guide_2026.4.pdf#page=1589)). Mixdown and dither placement are described on [printed page 1612, PDF page 1714](https://resources.avid.com/SupportFiles/PT/Pro_Tools_Reference_Guide_2026.4.pdf#page=1714) and [Plugins Guide 2024.10, page 373](https://resources.avid.com/SupportFiles/PT/Audio_and_MIDI_Plugins_Guide_2024.10.pdf#page=373). These references specify order; dither fidelity is a separate acceptance.

## Signal order

Active Master Faders now apply their Volume gain while copying main audio into the prepared strip, process active non-bypassed slots A through J in sequence, then copy back at unity. Output meters still observe the final result. Ordinary Audio and Aux inserts keep their pre-fader order. Multiple active Master Faders retain the existing main-output cascade and main channel width.

The original SC-MX02 checkpoint used a saturating last-included-frame endpoint and a linear gain ramp over each block; its endpoint-dependent tests below are historical evidence of that law. The [shared prepared automation follow-up](automation-playback.md) evaluates readable Master Volume at each absolute sample while preserving gain-before-inserts order. Volume in Write still uses the static fader, and the enabled/suspend policy is unchanged. Its focused references and full integration requirement are recorded in the linked workflow; the original nonlinear signal-order acceptance does not by itself establish the new automation law or complete SC-MX11.

At the original SC-MX02 checkpoint, only existing prepared buffers were used and the Master callback added no allocation, lock or logging path. Callback allocation and CPU performance were not measured. The prepared follow-up has the separate ownership and measurement qualifications recorded in [automation playback](automation-playback.md).

## Original SC-MX02 native evidence

At the SC-MX02 checkpoint, eight Mix tests and three Engine tests passed on the native Rust 1.95 toolchain with serial test execution. Output/reference comparisons assert exact channel and frame shapes and finite PCM. Non-silent static limiter cases require audible signal; the -144 dB case requires exact silence. The independent reference derives limiter gain reduction from linked peak level, threshold, ratio and attack/release recurrence, without calling mixer or DSP expectation helpers. The built-in compressor uses limiter mode with zero lookahead, so latency cannot mask signal order.

The Mix tests cover:

- Static Master fader changes from -144 to +6 dB over five block sizes, compared with gain-before-limiter references and an audible contrast against the former limiter-before-gain order.
- Serial slots, bypass, inactive Masters, ordinary Audio/Aux pre-fader inserts, multiple-Master cascades, stereo/5.1/16-channel main outputs and final peak meters.
- Next-block knots, zero/one-frame and saturating endpoints, declared block-ramp references over regular and irregular schedules, retained limiter state across seek/loop, reset behavior and offline/callback agreement for the same static schedule.

The Engine tests cover merged fader gestures with one Undo, Redo and exact document/render restoration; bypass and slot-move commands; and native Save/Open plus strict native Copy and bundle reopen. Reopened Volume curves, insert parameters, track IDs, outputs and audible PCM match. Rendering and Copy retain the open document, revision, dirty state, history, journal, clipboard and path.

The initial Engine test attempt failed during fixture setup because the reserved name `Master` is stored as `Master 1`. The corrected fixture uses a distinct name and asserts the command's returned identity before configuring inserts; the independent reference and production code were unchanged. The failed attempt is retained separately from passing evidence.

## Remaining scope

This establishes the original nonlinear-limiter signal-order acceptance on synthetic native PCM. It does not establish hosted or hardware insert behavior, live device playback, other native platforms, or browser audio runtime. Master latency/PDC, lookahead/tails, independent output/bus assignment, mute/trim/VCA/pan/send semantics and other main widths remain separate work.

SC-MX11 retains its original seven-family automation acceptance: Volume, Pan, Mute, SendLevel, SendPan, SendMute and Plugin, including independent PCM/static references and seek/loop behavior. Generic malformed Volume-lane handling, global enabled/suspend policy and writing/safing also remain. SC-MX42 dither depth, noise shaping, spectral behavior, double-dither and resampling references are unverified here.

Stage, readiness values, milestone completion and effort estimates are unchanged. Full CI and publication are separate gates from these focused native tests.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Marked original block-endpoint reference law as historical; shared prepared sample-Volume follow-up preserves Master insert order and has separate verification |
| 2026-10-12 | minor | Added Master gain-before-inserts order, last-included endpoint and independent nonlinear native command/persistence acceptance; broader runtime and automation residuals retained |
