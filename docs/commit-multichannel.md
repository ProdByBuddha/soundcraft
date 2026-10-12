# Track Commit channel-format integrity

**Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12

Track Commit retains the source track's exact `ChannelFormat`, including surround, height and
ambisonic layouts. Channel count alone cannot identify a layout: for example, 5.1 and 6.0 both
have six channels, while Quad, LCRS and first-order ambisonics all have four.

The shared pre-fader renderer already returns multichannel audio in the track's own layout.
Commit now copies that layout instead of labeling every multichannel result as stereo, and
checks the complete rendered channel count and duration before adding a source or replacing
the original track. Requests whose two-second tail overflows or whose render would exceed
the current offline limit return an error. A command committing several tracks rolls back all
of them if any selected track fails.

Regression coverage uses different bipolar signals on every channel across all 30 declared
formats the current 16-channel renderer can process, and applies an insert before committing.
Ambisonics orders four through seven exceed that limit and are explicitly rejected before
rendering; the source track remains unchanged. Additional cases compare routed bus/send
audio before and after commit, undo/redo, session save/reopen, and float WAV export, including
the output speaker mask. Mono is checked against the renderer's temporary stereo allocation
limit as well as the multichannel limits.

The shared mono renderer's existing -3 dB center-pan gain followed by `SQRT_2` compensation
produces approximately +0.0103 dB gain. The signal test accounts for that baseline explicitly
with the same tolerance as other formats. Exact mono amplitude parity remains a separate
renderer gap; this change leaves its processing behavior intact.

This closes a channel-format correctness gap in the existing commit workflow. It does not add
multiple-output commit options, pan-automation render controls, plugin auxiliary-output
selection, configurable tails, or render-bit-depth preferences. Session track metadata retains
the exact layout; this change does not alter the channel masks inferred for raw source WAV
files during session saves. The behavior reference is the
Pro Tools Reference Guide 2026.4, printed page 1367 (PDF page 1469), which describes committed
output widths and assignments. No runtime comparison with Pro Tools is claimed.

## Revision history

| Date | Change |
|---|---|
| 2026-10-12 | Preserve exact committed formats and reject incomplete or unsupported-width renders; add channel-signal, routing, persistence/export and rollback coverage. |
