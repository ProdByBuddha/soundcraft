# Plugin discovery in CI

> **Last reviewed:** 2026-10-12 · **Last updated:** 2026-10-12 · **Change:** minor

`cargo xtask ci` runs the complete workspace test command with controlled plugin discovery.
It sets empty exclusive VST3 and CLAP automatic search roots and restricts Audio Unit discovery
to Apple's `appl` manufacturer before macOS component enumeration. Owned CLAP/VST3 fixtures,
their real host processing/state/editor/shutdown tests and the existing Apple system-AU tests
remain in the workspace run. Installed third-party plugins are not required for CI.

The production host defaults are unchanged when these settings are absent:

| Environment setting | Automatic discovery policy |
|---|---|
| `SOUNDCRAFT_VST3_SEARCH_PATHS` | Present: only its platform-separated paths; empty: no roots. Absent: standard locations plus additive `VST3_PATH`. |
| `SOUNDCRAFT_CLAP_SEARCH_PATHS` | Present: only its platform-separated paths; empty: no roots. Absent: standard locations plus additive `CLAP_PATH`. |
| `SOUNDCRAFT_AU_MANUFACTURERS` | Present: comma-separated valid nonzero four-char or `#XXXXXXXX` codes, deduplicated across the first 64 fields; empty/invalid: no discovery. Absent: all registered manufacturers. CI supplies `appl`. |

Set the policy before starting the process. Registries still cache discovery and plugin info;
changing environment values after discovery is not a cache-reset API. Explicit `scan_paths`,
`add_search_dir` and `load_bundle` calls remain available for deliberate paths, including the
owned test fixtures. These settings isolate automatic discovery, not arbitrary explicit loads.

The CI test child overrides inherited exclusive paths and removes `SOUNDCRAFT_VST3_SMOKE`.
Direct `cargo test --workspace` retains the caller's discovery settings. Optional installed-VST3
smoke coverage is a separate owner-controlled run, enabled with `SOUNDCRAFT_VST3_SMOKE=1`;
it may require plugin activation or other interaction and is not part of required CI.

Pure policy and child-command tests cover default compatibility, exclusive/empty paths,
manufacturer selection before enumeration and no discovery callback for an empty policy.
Fixture rescan assertions check that registry cache seeding cannot conceal fallback to system
roots. Passing CI does not establish third-party plugin, audio-device or platform runtime acceptance.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-12 | minor | Documented explicit automatic-discovery policies and controlled workspace CI hosting |
