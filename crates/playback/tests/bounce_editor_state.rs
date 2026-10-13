//! Run the real editor cases on a separate executable's process main thread.

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn actual_clap_effect_and_instrument_editor_states_on_process_main_thread() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Cargo builds normal examples for `cargo test`; acceptance also explicitly builds
    // this target before all-targets tests. Never search old hashed artifacts.
    let current = std::env::current_exe()?;
    let profile = current.parent().and_then(std::path::Path::parent).ok_or("test executable has no Cargo profile directory")?;
    let executable = profile.join("examples").join(format!("bounce_editor_state_driver{}", std::env::consts::EXE_SUFFIX));
    assert!(executable.is_file(), "Cargo must build the normal example: {}", executable.display());
    let expected_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ready = ReadyFlag::new()?;
    let mut command = Command::new(&executable);
    command.arg("--workspace-root").arg(expected_root).arg("--ready-file").arg(&ready.path).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    // Fixture compilation has no deadline. Bound only the editor/DSP phase, once
    // this driver's owned, atomically published marker proves preparation completed.
    let mut runtime_deadline = None;
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let cleanup = stop_driver(&mut child);
                return Err(format!("editor acceptance status failed: {error}; {cleanup}").into());
            }
        };
        if let Some(status) = status {
            if !status.success() {
                let cleanup = stop_driver(&mut child);
                return Err(format!("main-thread editor acceptance failed: {status}; {cleanup}").into());
            }
            assert!(ready.prepared()?, "driver exited without proving owned-fixture preparation");
            return Ok(());
        }
        if runtime_deadline.is_none() {
            match ready.prepared() {
                Ok(true) => runtime_deadline = Some(Instant::now() + Duration::from_secs(120)),
                Ok(false) => {}
                Err(error) => {
                    let cleanup = stop_driver(&mut child);
                    return Err(format!("owned-fixture readiness failed: {error}; {cleanup}").into());
                }
            }
        }
        if runtime_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let cleanup = stop_driver(&mut child);
            return Err(format!("main-thread editor acceptance exceeded 120 runtime seconds after owned-fixture preparation; {cleanup}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn stop_driver(child: &mut std::process::Child) -> String {
    // Owned fixture Cargo/rustc descendants inherit this isolated group. End the whole
    // tree before reaping the driver so a timeout cannot leave fixture builds running.
    #[cfg(unix)]
    let descendants = std::process::Command::new("/bin/kill").args(["-KILL", "--"]).arg(format!("-{}", child.id())).status();
    #[cfg(windows)]
    let descendants = std::process::Command::new("taskkill").args(["/T", "/F", "/PID"]).arg(child.id().to_string()).status();
    #[cfg(not(any(unix, windows)))]
    let descendants = "no platform subtree terminator";
    let killed = child.kill();
    let reaped = child.wait();
    format!("descendants={descendants:?}, kill={killed:?}, wait={reaped:?}")
}

#[cfg(not(target_arch = "wasm32"))]
struct ReadyFlag {
    directory: std::path::PathBuf,
    path: std::path::PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReadyFlag {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!("soundcraft-bounce-editor-ready-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory)?;
        let path = directory.join("owned-fixtures-ready");
        Ok(Self { directory, path })
    }

    fn prepared(&self) -> std::io::Result<bool> {
        match std::fs::read(&self.path) {
            Ok(bytes) if bytes == b"SC-MX38 owned fixtures prepared\n" => Ok(true),
            Ok(_) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid owned-fixture ready marker")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for ReadyFlag {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
