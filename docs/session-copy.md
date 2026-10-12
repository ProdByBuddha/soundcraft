# Audio-portable session copies

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor (combined Float32 identity and Copy lifecycle verification)

`session.save_copy` packages every declared audio source into a new SoundCraft session without
changing the open document, its saved path, dirty state, selection, Undo or Redo. Copy and its
preview are excluded from the recovery journal. Ordinary Save, Save As and autosave remain able
to preserve documents with missing audio.

Missing decoded audio blocks Copy before creating output directories or files. Restore or relink
the audio first. The copy includes alternate playlists and unused source rows retained in the
document. Audio clip references, mixer insert samples and instrument samples must resolve to
declared, available sources. Duplicate IDs, mismatched decoded dimensions, ragged channels,
unsupported rates and oversized metadata fail preflight. Same names and original paths are
allowed; copied media is distinguished by its actual content.

## Commands

```json
{"command":"session.save_copy_preview","params":{"path":"/sessions/Copy/Copy.scraft"}}
{"command":"session.save_copy","params":{"path":"/sessions/Copy/Copy.scraft","expected_revision":12}}
```

Preview accepts optional `path` and `expected_revision`; Copy requires `path` and additionally
accepts boolean `replace` (default false). Parameters must be an object with correctly typed,
known keys. A stale `expected_revision` fails before output. `replace: true` permits replacing
another session file; it does not permit replacing the open session's own file or its canonical aliases.

Preview returns `ready`, audio ready/missing/invalid counts, issues, and media rows. Successful
Copy retains `path` and `audio_files_written` and adds the effective `output_path`,
`audio_sources_packaged`, `audio_complete: true`, and `media`. Native audio rows say `written`
or `reused`; browser audio rows say `packaged`. Each gives its actual relative output path.
Video rows always say `external_unverified` and retain their external reference. All reports
state `full_session_portability: "not_assessed"`: plugin files and other external assets have
not been inventoried, even when there is no video.

## Native and browser behavior

Native Copy writes Float32 WAV at the current session rate, with byte-validated reuse of existing
media. It creates collision suffixes instead of replacing another session's audio. Symlink
session destinations, an Audio Files symlink directory, and symlinks in that media folder are
rejected. Session publication tries at most 100 unique staging names, creating its file exclusively,
writing and syncing that open file before renaming it to the authorized destination. Existing staging
paths are never opened for writing; an old fixed `.scraft.tmp` file or link is left untouched.
On a publication failure, best-effort cleanup removes staging only when its filesystem identity still matches
the file created by Copy; platforms without that identity check retain the error-stage artifact.
Unix hard-link aliases of the open session are also rejected.
A later encoding or disk I/O failure may leave additional audio without a committed session; a
whole-folder transaction is not implemented. Concurrent hostile filesystem replacement is not
covered by these preflight checks.

Browser Copy builds one `.scraftz` download through the existing delivery hook. The complete
bundle is validated before delivery; building it never changes the open document. Successful
delivery means the browser hook accepted the bytes, not that the download was retained. The
reader's 1 GiB uncompressed-data and 10,000-entry limits constrain Copy. Both routes allow at
most 9,999 declared audio sources and 9,999 external video references, source name/path fields
of at most 4,096 bytes, and at most 16 MiB of bounded serialized metadata. Source/buffer/session
rates must agree within 8,000–768,000 Hz. Float32 WAV channel/block/byte-rate fields and sample
counts are checked before output.

The dedicated Copy dialog shows the audio policy, blocked sources and external-video limit.
Edits preserve its typed destination but make the preview stale until Refresh Preview is
pressed. Successful New/Open/Close/Revert dismisses only the Copy dialog. A failed open leaves
it available. Escape or Cancel suppresses copying, including when Enter is pressed in the same frame.
Available live plugin state is captured only into a detached export document; preview never
captures state or opens plugins.

## Verification and remaining scope

Synthetic tests move the copied folder, remove original media, reopen all declared sources, and
compare decoded audio and playlists. Tests also cover incomplete/malformed sessions, altered
same-size destination media, missing-media ordinary Save/recovery, Undo/Redo/journal preservation,
native destination aliases, strict bundles, delivery failure, and the actual dialog/UI dispatcher.

This is the audio-completeness part of manual-analysis row ED-03. Target-rate/depth/type conversion,
selected-time export and timing remap, exact original-file bytes, BWF metadata, video copying,
external plugin-file inventory and whole-copy rollback remain separate work. It does not provide
Pro Tools session interchange.

Fork integration retains per-document recovery, byte-validated media reuse and relink identity
checks. Native and bundled Copy assign each source's identity from the encoded and decoded saved
PCM. Combined regressions preserve finite above-unity samples and signed zero, sanitize NaN and
infinities to zero, and reopen both forms without identity warnings. They also retain opaque
insert/instrument state and the original document, source identity, Undo/Redo and recovery journal.
These are synthetic audio checks; installed plugin-state capture, browser download retention and
physical-device acceptance remain separate. No proprietary resources or reference-manual wording
are bundled.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Integrated saved-PCM identities, Float32 headroom and nonfinite handling; verified immutable export and dialog cancellation |
| 2026-10-12 | minor | Added strict audio-completeness preflight, native/bundled Copy and dedicated preview dialog |
