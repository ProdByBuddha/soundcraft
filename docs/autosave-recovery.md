# Autosave recovery isolation

Last updated: 2026-10-12.

Desktop autosave reserves a separate directory for each open document with an atomic filesystem
operation. Two documents named `Untitled`, even when their source names and numeric source IDs
are identical, do not share a recovery file or audio folder. Unsaved recordings use the same
reserved directory, falling back to a unique temporary directory when the host has not enabled
autosave.

The directory stays with the open document through edits, undo/redo, Save As and Save Copy.
Successful New, Open, Close and Revert operations start another document lifetime and reserve a
fresh directory. A failed open/create does not change the current lifetime. This identity is
host state, not a new session field, so existing `.scraft` files require no migration and copied
session files do not inherit a shared recovery namespace.

Each directory contains `Recovery.scraft`; its actual session name remains inside the document.
Recovery discovery still scans one directory below the configured autosave root, including the
older `<session name>/<session name>.scraft` layout. Prior recovery directories remain available
after an application restart.

Recovery audio is reused only when its encoded WAV bytes match. An existing filename or source
ID alone is insufficient: different material gets a new filename, and the recovery document is
atomically replaced only after the media is stored. Original files and previous recovery audio
are retained. Save Copy does not change the open document or mark it clean.

This correctness check re-encodes available pooled media and compares existing candidate files
on each autosave. It avoids repeated writes for unchanged audio but can consume CPU and disk I/O
on large sessions. Autosave remains synchronous, every two minutes while dirty and outside
recording. Configurable interval/retention, asynchronous saving, automatic removal of old
recovery directories and unavailable-media recovery are separate work.

## Revision history

| Date | Change |
|---|---|
| 2026-10-12 | Documented per-open-document recovery directories and exact-byte media reuse. |
