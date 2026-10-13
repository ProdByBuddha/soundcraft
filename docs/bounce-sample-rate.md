# Bounce delivery sample rate

Last reviewed: 2026-10-13. Last updated: 2026-10-13.

`file.bounce_mix` accepts optional `sample_rate`, an integer JSON number from 8,000 through
768,000 Hz. Omitting it retains the session rate. The Bounce Mix dialog starts at the session
rate, offers common rates, and accepts a custom integer rate within the same bounds. The
session rate, source audio, clip positions, routing and saved plugin settings do not change.

The mixer and plugins run at the session rate. The selected main mix or bus is optionally
folded to stereo, then converted once with the existing windowed-sinc converter. Conversion
precedes normalization, output-rate metering, encoding, integer quantization and existing
TPDF dither. The WAV channel mask still describes the delivered format. Equal-rate bounces
skip conversion and retain the prior encoded bytes and measurements. Existing `bounce_source`
and main-mix byte helpers retain their signatures and default behavior.

Results include actual `sample_rate`, `frames`, and `seconds = frames / sample_rate`, alongside
file bytes, peak, LUFS and true peak. Measurements describe the output-rate signal before
final quantization; they do not claim a post-encoding analyzer. Existing meter weights are
inferred from channel count (six channels use 5.1 weights, affecting 6.0); layout-aware LUFS
weighting remains separate. This change selects the measurement rate only. Conversion length is rounded
to the nearest whole frame, with a half frame rounded upward. A request that would round to
zero frames fails rather than producing an empty file.

Before rendering, checked arithmetic verifies the range length and the existing four-hour
limit. Source buffers are bounded by the renderer's 2^29 total-sample limit, using its actual
width and at least stereo for its working path. Fold/conversion buffers must also fit that
aggregate limit while coexisting. Converted delivery is additionally bounded to 2^28 total
samples, matching the new fallible explicit-length conversion helper. Oversized requests
fail before rendering or writing a destination; returned frame/channel shapes are checked
again to prevent silently truncated delivery. A declared Main output or named bus wider than
16 channels is rejected, even when a stereo fold is requested, before the model can silently
substitute stereo for an unsupported Main format. Supported Main formats, including the
existing mono/missing-output stereo fallback, retain their behavior. These limits cover whole planar bounce buffers;
shared-renderer plugin latency/tail workspaces have their own preparation bounds. The
[MX-38 bounce delivery arc](bounce-delivery.md) adds explicit history/tails, detached live
state and exclusive publication; the full acceptance criterion remains open. PR #326 is a
separate upstream proposal and is not treated as merged runtime evidence.

Tests cover default byte/stat equivalence, 44.1/48/96 kHz delivery in WAV/AIFF/FLAC, distinct
multichannel signals, bus and stereo-fold routing, exact frames and speaker masks, output-rate
metrics, one-frame half-rate rounding, malformed/huge requests with no writes or document
changes, UI selection dispatch and an original offscreen PNG. Existing DSP SRC tests remain
unchanged. UI file-browser changes in PR #300 and CLI validation in PR #261 are separate.

Reference: Pro Tools Reference Guide 2026.4, printed page 1625 (PDF page 1727), documents a
bounce sample-rate choice independently of bit depth. This scoped feature does not close
stems, SRC quality selection, noise shaping, master-fader dither placement, pull-up/down or
other MX42 mixdown gaps. No Pro Tools resources or implementation code were used.

## Revision history

- 2026-10-13: Linked full MX-38 state/history/publication acceptance without broadening the
  sample-rate quality or runtime claims.
- 2026-10-12: Added independently selected bounce delivery rate, bounded conversion and scoped
  command/UI regression coverage. No global parity/readiness estimate was changed.
