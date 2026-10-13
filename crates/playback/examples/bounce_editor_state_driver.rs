//! Main-thread acceptance executable; shares the exact private production implementation.
//! Cargo also checks its cfg(test) build through the ordinary libtest harness.

include!("../src/runtime.rs");

#[cfg(all(not(test), not(target_arch = "wasm32")))]
#[path = "../src/bounce_editor_cases.rs"]
mod bounce_editor_cases;

#[cfg(all(not(test), not(target_arch = "wasm32")))]
#[path = "support/bounce_editor_sources.rs"]
mod source_snapshot;

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--workspace-root")) {
        return Err("editor acceptance driver requires --workspace-root from its integration launcher".into());
    }
    let expected_root = arguments.next().ok_or("missing expected workspace root")?;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--ready-file")) {
        return Err("editor acceptance driver requires --ready-file from its integration launcher".into());
    }
    let ready_file = arguments.next().ok_or("missing owned-fixture ready file")?;
    if arguments.next().is_some() {
        return Err("unexpected editor acceptance driver argument".into());
    }
    source_snapshot::verify(std::path::Path::new(&expected_root))?;
    bounce_editor_cases::prepare_owned_fixtures();
    // Publish readiness only after the deadline-free owned Cargo fixture builds and
    // descriptor assertions finish. Atomic rename prevents a partially read marker.
    use std::io::Write;
    let ready_file = std::path::Path::new(&ready_file);
    let preparing = ready_file.with_extension("preparing");
    let mut ready = std::fs::OpenOptions::new().write(true).create_new(true).open(&preparing)?;
    ready.write_all(b"SC-MX38 owned fixtures prepared\n")?;
    drop(ready);
    std::fs::rename(preparing, ready_file)?;
    println!("bounce_editor_state::owned_fixtures_ready: prepared");
    bounce_editor_cases::editor_main();
    Ok(())
}

#[cfg(all(not(test), target_arch = "wasm32"))]
fn main() {}
