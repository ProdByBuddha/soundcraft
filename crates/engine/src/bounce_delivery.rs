//! Exclusive native bounce publication. Rendering and playback comparison are separate callers.
//!
//! These control-side phases never change a Session and never overwrite an existing destination.
//! No destructor deletes files. Explicit publication/discard cleans only identity-proven files;
//! a platform without that proof reports retained paths rather than blindly unlinking them.

use crate::{EngineError, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use soundcraft_model::Session;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Matches the whole-render sample envelope, allowing bounded encoded headers.
pub const MAX_DELIVERY_BYTES: u64 = (2 << 30) + (1 << 20);
const MAX_PATH_BYTES: usize = 4096;
const CHUNK: usize = 64 << 10;
const STAGE_ATTEMPTS: usize = 64;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

fn fail(message: impl std::fmt::Display) -> EngineError {
    EngineError::Io(format!("bounce delivery: {message}"))
}
fn path_text(path: &Path) -> Result<&str> {
    let text = path.to_str().ok_or_else(|| fail("destination must be UTF-8"))?;
    if text.is_empty() || text.len() > MAX_PATH_BYTES || text.contains('\0') {
        return Err(fail("path must be nonempty, without NUL, and at most 4096 bytes"));
    }
    Ok(text)
}
fn require_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(fail(format!("destination already exists: {}", path.display()))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(fail(format!("{}: {err}", path.display()))),
    }
}
fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() { Ok(path.to_owned()) } else { Ok(std::env::current_dir().map_err(fail)?.join(path)) }
}
// Resolve the deepest existing ancestor, then normalize missing suffixes. This also protects
// recorded files which have gone missing and aliases through existing symlink parents.
fn resolved(path: &Path) -> Result<PathBuf> {
    let full = absolute(path)?;
    let mut ancestor = full.as_path();
    loop {
        match ancestor.canonicalize() {
            Ok(root) => {
                let suffix = full.strip_prefix(ancestor).map_err(fail)?;
                return Ok(normalized(&root.join(suffix)));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(fail(format!("cannot resolve {}: {err}", path.display()))),
        }
        ancestor = ancestor.parent().ok_or_else(|| fail("path has no resolvable ancestor"))?;
    }
}
fn source_path(session: &Session, path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    path_text(path)?;
    Ok(if path.is_absolute() { path.to_owned() } else { session.folder.as_deref().unwrap_or(Path::new(".")).join(path) })
}

// A conservative ASCII case check protects missing recorded leaves on common case-insensitive
// filesystems. It may also refuse case variants on a case-sensitive filesystem. This is not a
// universal Unicode/filesystem equivalence test; exact canonical resolution remains separate.
fn same_recorded_path(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.to_str(), b.to_str()), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
}

#[cfg(any(windows, test))]
fn windows_file_name_safe(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with([' ', '.'])
        || name.chars().any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|number| matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    })
}

// Keep normal disk and UNC syntax, without interpreting device/verbatim namespaces or alternate
// data streams. Pure parsing can be tested on every platform; it is not Windows runtime proof.
#[cfg(any(windows, test))]
fn windows_disk_path_safe(text: &str) -> bool {
    let slashes = text.replace('/', "\\");
    if slashes.starts_with("\\\\?\\") || slashes.starts_with("\\\\.\\") || !text.rsplit(['/', '\\']).next().is_some_and(windows_file_name_safe) {
        return false;
    }
    let mut components = text.split(['/', '\\']).filter(|part| !part.is_empty()).enumerate();
    components.all(|(index, name)| {
        if index == 0 && name.len() == 2 && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && name.ends_with(':') {
            true
        } else {
            matches!(name, "." | "..") || windows_file_name_safe(name)
        }
    })
}

/// A validated fresh-file destination. Accessors expose actual paths for independent callers/tests.
pub struct DestinationPlan {
    requested: PathBuf,
    destination: PathBuf,
    parent: PathBuf,
    parent_metadata: Metadata,
}
impl DestinationPlan {
    pub fn destination(&self) -> &Path {
        &self.destination
    }
    pub fn requested_path(&self) -> &Path {
        &self.requested
    }
}

/// Validate names and protect every recorded original and persisted playback locator, even
/// when missing. Virtual unsaved playback paths are not treated as existing media files.
pub fn prepare_destination(session: &Session, path: &Path) -> Result<DestinationPlan> {
    if cfg!(target_arch = "wasm32") {
        return Err(EngineError::Unsupported("native bounce publication requires a filesystem".into()));
    }
    let text = path_text(path)?;
    if text.ends_with('/') || (cfg!(windows) && text.ends_with('\\')) {
        return Err(fail("destination must name a file"));
    }
    let name = path.file_name().ok_or_else(|| fail("destination must name a file"))?;
    #[cfg(windows)]
    if !windows_disk_path_safe(text) {
        return Err(fail("destination must use regular disk/UNC names, without device namespaces, reserved names or alternate streams"));
    }
    require_absent(path)?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = parent.canonicalize().map_err(|err| fail(format!("destination directory: {err}")))?;
    let parent_metadata = std::fs::metadata(&parent).map_err(fail)?;
    if !parent_metadata.is_dir() {
        return Err(fail("destination parent must be a directory"));
    }
    let destination = parent.join(name);
    path_text(&destination)?;
    require_absent(&destination)?;
    let lexical = normalized(&absolute(path)?);
    for source in &session.sources {
        let locators =
            source.original.as_ref().map(|original| original.path.as_str()).into_iter().chain((!source.unsaved).then_some(source.path.as_str()));
        for locator in locators {
            let protected = source_path(session, locator)?;
            if same_recorded_path(&normalized(&absolute(&protected)?), &lexical) || same_recorded_path(&resolved(&protected)?, &destination) {
                return Err(fail(format!("destination is a recorded media locator: {locator}")));
            }
        }
    }
    Ok(DestinationPlan { requested: path.to_owned(), destination, parent, parent_metadata })
}

struct OwnedFile {
    path: PathBuf,
    file: File,
    metadata: Metadata,
}

/// Completed, synced staging bytes. Dropping this closes the handle but does not unlink its path.
pub struct StagedBounce {
    plan: DestinationPlan,
    owned: OwnedFile,
    bytes: u64,
    sha256: String,
}
impl StagedBounce {
    pub fn destination(&self) -> &Path {
        &self.plan.destination
    }
    pub fn staging_path(&self) -> &Path {
        &self.owned.path
    }
}

#[derive(Debug, Serialize)]
pub struct DeliveryReceipt {
    pub path: String,
    pub output_path: String,
    pub bytes: u64,
    pub sha256: String,
    /// True only after final bytes are checked and the output is synced.
    pub complete: bool,
    /// Cleanup lacks an ownership proof or failed. These paths must not be blindly removed.
    pub retained_paths: Vec<String>,
}

fn same_identity(metadata: &Metadata, current: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.dev() == current.dev() && metadata.ino() == current.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, current);
        false
    }
}
fn verify_parent(plan: &DestinationPlan) -> Result<()> {
    let current = std::fs::symlink_metadata(&plan.parent).map_err(fail)?;
    if !current.is_dir() || current.file_type().is_symlink() {
        return Err(fail("destination directory changed"));
    }
    #[cfg(unix)]
    if !same_identity(&plan.parent_metadata, &current) {
        return Err(fail("destination directory identity changed"));
    }
    #[cfg(not(unix))]
    let _ = &plan.parent_metadata;
    Ok(())
}
fn remove_proven(path: &Path, metadata: &Metadata) -> std::io::Result<()> {
    let current = match std::fs::symlink_metadata(path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::other("owned file no longer at recorded path; cleanup unverified"));
        }
        Err(err) => return Err(err),
    };
    if current.is_file() && !current.file_type().is_symlink() && same_identity(metadata, &current) {
        std::fs::remove_file(path)
    } else {
        Err(std::io::Error::other("file ownership cannot be proved; retained"))
    }
}
fn cleanup(owned: OwnedFile, retained: &mut Vec<String>) {
    drop(owned.file);
    if remove_proven(&owned.path, &owned.metadata).is_err() {
        retained.push(owned.path.to_string_lossy().into_owned());
    }
}
fn hash_file(file: &mut File, expected_bytes: u64) -> Result<String> {
    let metadata = file.metadata().map_err(fail)?;
    if !metadata.is_file() || metadata.len() != expected_bytes {
        return Err(fail("output file/length differs from staged bytes"));
    }
    file.seek(SeekFrom::Start(0)).map_err(fail)?;
    let mut buffer = [0u8; CHUNK];
    let mut count = 0u64;
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut buffer).map_err(fail)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).filter(|count| *count <= expected_bytes).ok_or_else(|| fail("output grew during validation"))?;
        let chunk = buffer.get(..n).ok_or_else(|| fail("invalid output reader chunk"))?;
        hash.update(chunk);
    }
    if count != expected_bytes {
        return Err(fail("output shortened during validation"));
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn validate_path(path: &Path, expected: &Metadata, bytes: u64, sha256: &str) -> Result<()> {
    let current = std::fs::symlink_metadata(path).map_err(fail)?;
    if !current.is_file() || current.file_type().is_symlink() {
        return Err(fail("output path is no longer a regular file"));
    }
    #[cfg(unix)]
    if !same_identity(expected, &current) {
        return Err(fail("output file identity changed"));
    }
    #[cfg(not(unix))]
    let _ = expected;
    let mut file = File::open(path).map_err(fail)?;
    #[cfg(unix)]
    if !same_identity(expected, &file.metadata().map_err(fail)?) {
        return Err(fail("output changed while opening for validation"));
    }
    if hash_file(&mut file, bytes)? != sha256 {
        return Err(fail("output bytes differ from staged content"));
    }
    Ok(())
}

pub fn stage(plan: DestinationPlan, bytes: &[u8]) -> Result<StagedBounce> {
    let length = bytes.len() as u64;
    if length == 0 || length > MAX_DELIVERY_BYTES {
        return Err(fail("encoded output exceeds delivery byte limit or is empty"));
    }
    verify_parent(&plan)?;
    require_absent(&plan.destination)?;
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(fail)?.as_nanos();
    for _ in 0..STAGE_ATTEMPTS {
        let nonce = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let path = plan.parent.join(format!(".soundcraft-bounce-{}-{time}-{nonce}.tmp", std::process::id()));
        let mut file = match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(fail(format!("cannot create staging file: {err}"))),
        };
        let metadata = file.metadata().map_err(|err| fail(format!("staging identity unavailable; retained {}: {err}", path.display())))?;
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        let owned = OwnedFile { path, file, metadata };
        if let Err(err) = result {
            let mut retained = Vec::new();
            cleanup(owned, &mut retained);
            return Err(fail(format!("cannot stage output: {err}; retained paths: {}", retained.join(", "))));
        }
        return Ok(StagedBounce { plan, owned, bytes: length, sha256 });
    }
    Err(fail("no unused staging path after 64 attempts"))
}

fn create_copy_output(staged: &StagedBounce, final_owned: &mut Option<OwnedFile>) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&staged.plan.destination).map_err(fail)?;
    let metadata =
        file.metadata().map_err(|err| fail(format!("final identity unavailable; retained {}: {err}", staged.plan.destination.display())))?;
    *final_owned = Some(OwnedFile { path: staged.plan.destination.clone(), file, metadata });
    Ok(())
}

// The writer is private: production forwards File::write_all, while tests can measure a real
// prefix followed by an explicitly injected error without a public filesystem test toggle.
fn copy_staged_output(
    staged: &mut StagedBounce,
    final_owned: &mut OwnedFile,
    mut write_chunk: impl FnMut(&mut File, &[u8]) -> std::io::Result<()>,
) -> Result<()> {
    staged.owned.file.seek(SeekFrom::Start(0)).map_err(fail)?;
    let mut buffer = [0u8; CHUNK];
    let mut written = 0u64;
    loop {
        let n = staged.owned.file.read(&mut buffer).map_err(fail)?;
        if n == 0 {
            break;
        }
        written = written.checked_add(n as u64).filter(|n| *n <= staged.bytes).ok_or_else(|| fail("staging file grew during publication"))?;
        write_chunk(&mut final_owned.file, buffer.get(..n).ok_or_else(|| fail("invalid staging chunk"))?).map_err(fail)?;
    }
    if written != staged.bytes {
        return Err(fail("staging file shortened during publication"));
    }
    final_owned.file.sync_all().map_err(fail)?;
    if hash_file(&mut final_owned.file, staged.bytes)? != staged.sha256 {
        return Err(fail("output bytes differ from staged content"));
    }
    validate_path(&final_owned.path, &final_owned.metadata, staged.bytes, &staged.sha256)
}

fn copy_to_new_file(staged: &mut StagedBounce, final_owned: &mut Option<OwnedFile>) -> Result<()> {
    create_copy_output(staged, final_owned)?;
    copy_staged_output(staged, final_owned.as_mut().ok_or_else(|| fail("missing output handle"))?, File::write_all)
}

fn publish_file(staged: &mut StagedBounce, final_owned: &mut Option<OwnedFile>) -> Result<()> {
    verify_parent(&staged.plan)?;
    require_absent(&staged.plan.destination)?;
    if hash_file(&mut staged.owned.file, staged.bytes)? != staged.sha256 {
        return Err(fail("staging handle content changed"));
    }
    validate_path(&staged.owned.path, &staged.owned.metadata, staged.bytes, &staged.sha256)?;
    match std::fs::hard_link(&staged.owned.path, &staged.plan.destination) {
        Ok(()) => {
            // Track the identity of OUR staged inode, not an unverified replacement linked by a race.
            let mut file = OpenOptions::new().read(true).write(true).open(&staged.plan.destination).map_err(fail)?;
            file.sync_all().map_err(fail)?;
            #[cfg(unix)]
            if !same_identity(&staged.owned.metadata, &file.metadata().map_err(fail)?) {
                return Err(fail("published file identity changed"));
            }
            if hash_file(&mut file, staged.bytes)? != staged.sha256 {
                return Err(fail("published bytes differ from staged content"));
            }
            *final_owned = Some(OwnedFile { path: staged.plan.destination.clone(), file, metadata: staged.owned.metadata.clone() });
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Err(fail(format!("late destination collision: {err}"))),
        Err(_) => {
            // Some filesystems do not support hard links. Exclusive creation still cannot overwrite
            // a late winner; on a write error an unverifiable partial path is reported, not deleted.
            copy_to_new_file(staged, final_owned)?;
        }
    }
    let owned = final_owned.as_ref().ok_or_else(|| fail("missing published ownership"))?;
    validate_path(&owned.path, &owned.metadata, staged.bytes, &staged.sha256)
}

pub fn publish(mut staged: StagedBounce) -> Result<DeliveryReceipt> {
    let mut final_owned = None;
    let result = publish_file(&mut staged, &mut final_owned);
    finish_publication(staged, final_owned, result)
}

fn finish_publication(staged: StagedBounce, final_owned: Option<OwnedFile>, result: Result<()>) -> Result<DeliveryReceipt> {
    let mut retained = Vec::new();
    if let Some(owned) = final_owned {
        if result.is_err() {
            cleanup(owned, &mut retained);
        } else {
            drop(owned.file);
        }
    } else if result.is_err() && std::fs::symlink_metadata(&staged.plan.destination).is_ok() {
        // No ownership was established. Preserve a collision winner or an unverifiable final.
        retained.push(staged.plan.destination.to_string_lossy().into_owned());
    }
    cleanup(staged.owned, &mut retained);
    result.map_err(|err| fail(format!("{err}; retained paths: {}", retained.join(", "))))?;
    crate::io::sync_dir(&staged.plan.parent);
    Ok(DeliveryReceipt {
        path: staged.plan.requested.to_string_lossy().into_owned(),
        output_path: staged.plan.destination.to_string_lossy().into_owned(),
        bytes: staged.bytes,
        sha256: staged.sha256,
        complete: true,
        retained_paths: retained,
    })
}

/// Explicit control-side disposal for a completed stage which the caller chooses not to publish.
pub fn discard(staged: StagedBounce) -> Result<Vec<String>> {
    let mut retained = Vec::new();
    cleanup(staged.owned, &mut retained);
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn under(parent: &Path) -> Self {
            let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let nonce = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".soundcraft-bounce-test-{}-{time}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            // This guard owns only a successfully created, unique test directory.
            Self(path)
        }
        fn new() -> Self {
            Self::under(&std::env::temp_dir())
        }
        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn literal() -> Vec<u8> {
        b"\0MX38: owned exclusive copy fallback\xff\r\n".repeat(4001)
    }
    fn staged(dir: &Directory, bytes: &[u8]) -> StagedBounce {
        stage(prepare_destination(&Session::default(), &dir.path("output.bin")).unwrap(), bytes).unwrap()
    }
    fn assert_cleaned_or_reported(path: &Path, report: &str) {
        if cfg!(unix) {
            assert!(!path.exists(), "identity-proven owned file must be cleaned: {}", path.display());
        } else {
            assert!(path.exists(), "unproved owned file must be retained: {}", path.display());
            assert!(report.contains(path.to_str().unwrap()), "retained path must be reported: {report}");
        }
    }

    #[test]
    fn exclusive_copy_fallback_writes_literal_bytes_and_uses_shared_finish() {
        let dir = Directory::new();
        let bytes = literal();
        let mut stage = staged(&dir, &bytes);
        let stage_path = stage.staging_path().to_owned();
        let mut final_owned = None;
        let result = copy_to_new_file(&mut stage, &mut final_owned);
        assert!(result.is_ok(), "{result:?}");
        #[cfg(unix)]
        assert!(!same_identity(&stage.owned.metadata, &final_owned.as_ref().unwrap().metadata), "fallback must copy to a distinct inode");
        let receipt = finish_publication(stage, final_owned, result).unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.bytes, bytes.len() as u64);
        // Literal reference hash was calculated independently of production hash_file.
        assert_eq!(receipt.sha256, "53aeeecb68a5c311e1c278a6aab946d1440c3a4055e19a5851a12fdb37172098");
        assert_eq!(std::fs::read(&receipt.output_path).unwrap(), bytes);
        assert_cleaned_or_reported(&stage_path, &receipt.retained_paths.join(", "));
    }

    #[test]
    fn exclusive_copy_fallback_preserves_a_late_winner() {
        let dir = Directory::new();
        let mut stage = staged(&dir, &literal());
        let stage_path = stage.staging_path().to_owned();
        let destination = stage.destination().to_owned();
        let winner = b"a late winner must never be overwritten";
        std::fs::write(&destination, winner).unwrap();
        let mut final_owned = None;
        let result = copy_to_new_file(&mut stage, &mut final_owned);
        assert!(result.is_err());
        assert!(final_owned.is_none(), "no ownership of the existing winner");
        let error = finish_publication(stage, final_owned, result).unwrap_err().to_string();
        assert_eq!(std::fs::read(&destination).unwrap(), winner);
        assert!(error.contains(destination.to_str().unwrap()));
        assert_cleaned_or_reported(&stage_path, &error);
    }

    #[test]
    fn a_real_read_only_output_write_error_uses_shared_cleanup() {
        let dir = Directory::new();
        let mut stage = staged(&dir, &literal());
        let stage_path = stage.staging_path().to_owned();
        let destination = stage.destination().to_owned();
        let mut final_owned = None;
        create_copy_output(&stage, &mut final_owned).unwrap();
        let output = final_owned.as_mut().unwrap();
        // An actual File opened without write permission makes the OS reject write_all.
        output.file = File::open(&destination).unwrap();
        let result = copy_staged_output(&mut stage, output, File::write_all);
        assert!(result.is_err());
        assert_eq!(std::fs::metadata(&destination).unwrap().len(), 0);
        let error = finish_publication(stage, final_owned, result).unwrap_err().to_string();
        assert_cleaned_or_reported(&destination, &error);
        assert_cleaned_or_reported(&stage_path, &error);
    }

    #[test]
    fn an_explicitly_injected_failure_after_a_real_prefix_uses_shared_cleanup() {
        let dir = Directory::new();
        let bytes = literal();
        assert!(bytes.len() > CHUNK);
        let mut stage = staged(&dir, &bytes);
        let stage_path = stage.staging_path().to_owned();
        let destination = stage.destination().to_owned();
        let mut final_owned = None;
        create_copy_output(&stage, &mut final_owned).unwrap();
        let mut calls = 0usize;
        let result = copy_staged_output(&mut stage, final_owned.as_mut().unwrap(), |file, chunk| {
            calls += 1;
            file.write_all(chunk)?;
            Err(std::io::Error::other("injected after successful prefix write"))
        });
        assert_eq!(calls, 1);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), bytes[..CHUNK]);
        let error = finish_publication(stage, final_owned, result).unwrap_err().to_string();
        assert!(error.contains("injected after successful prefix write"));
        assert_cleaned_or_reported(&destination, &error);
        assert_cleaned_or_reported(&stage_path, &error);
    }

    #[cfg(unix)]
    #[test]
    fn shared_finish_retains_a_foreign_replacement_of_the_copied_output() {
        let dir = Directory::new();
        let mut stage = staged(&dir, &literal());
        let stage_path = stage.staging_path().to_owned();
        let destination = stage.destination().to_owned();
        let mut final_owned = None;
        copy_to_new_file(&mut stage, &mut final_owned).unwrap();
        std::fs::rename(&destination, dir.path("moved-owned-output.bin")).unwrap();
        let foreign = b"foreign replacement must survive failure cleanup";
        std::fs::write(&destination, foreign).unwrap();
        let output = final_owned.as_ref().unwrap();
        let result = validate_path(&output.path, &output.metadata, stage.bytes, &stage.sha256);
        assert!(result.is_err());
        let error = finish_publication(stage, final_owned, result).unwrap_err().to_string();
        assert_eq!(std::fs::read(&destination).unwrap(), foreign);
        assert!(error.contains(destination.to_str().unwrap()));
        assert!(!stage_path.exists());
    }

    #[test]
    fn pure_windows_names_reject_streams_devices_and_non_disk_namespaces() {
        for invalid in [
            "out.wav:alternate",
            "out.wav.",
            "out.wav ",
            "CON",
            "nul.wav",
            "AUX .wav",
            "COM1.wav",
            "com¹.wav",
            "COM²",
            "LPT³.wav",
            "LPT9",
            "CONIN$",
            "bad<name.wav",
            "bad>name.wav",
            "bad|name.wav",
            "bad?name.wav",
            "bad*name.wav",
            "bad\"name.wav",
            "bad\u{1f}name.wav",
            "",
            ".",
            "..",
        ] {
            assert!(!windows_file_name_safe(invalid), "{invalid:?}");
        }
        for valid in ["Mix.wav", "COM0.wav", "COM10.wav", "LPT0.wav", "Console.wav", "曲.wav", "a b.wav"] {
            assert!(windows_file_name_safe(valid), "{valid:?}");
        }
        for invalid in [
            r"C:\Mix.wav:ads",
            r"C:\bad.\Mix.wav",
            r"C:\COM¹\Mix.wav",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\Mix.wav",
            r"\\?\C:\Mix.wav",
            r"\\?\UNC\server\share\Mix.wav",
            "C:",
            "C:Mix.wav",
            "",
        ] {
            assert!(!windows_disk_path_safe(invalid), "{invalid:?}");
        }
        for valid in [r"C:\Audio Files\Mix.wav", r"\\server\share\Mix.wav", r"relative\Mix.wav", "../Audio Files/Mix.wav", "C:/Audio Files/Mix.wav"] {
            assert!(windows_disk_path_safe(valid), "{valid:?}");
        }
    }

    #[test]
    fn missing_ascii_case_aliases_protect_absolute_relative_and_session_folder_locators() {
        use soundcraft_audio_io::{AudioBuffer, AudioInfo, FileFormat, SampleFormat};
        use soundcraft_model::{ImportPolicy, OriginalProvenance};
        let cwd = std::env::current_dir().unwrap();
        let dir = Directory::under(&cwd);
        let folder = dir.path("session");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(folder.join("Original Audio")).unwrap();
        std::fs::create_dir(folder.join("Audio Files")).unwrap();
        for relative_folder in [false, true] {
            for absolute_locator in [false, true] {
                let mut session = Session {
                    folder: Some(if relative_folder { folder.strip_prefix(&cwd).unwrap().to_owned() } else { folder.clone() }),
                    ..Session::default()
                };
                crate::io::add_source(
                    &mut session,
                    "fixture",
                    AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.125, -0.25]] },
                    None,
                    FileFormat::Wav,
                );
                let source = &mut session.sources[0];
                let locator = |name: &str| {
                    if absolute_locator { folder.join(name).to_str().unwrap().to_owned() } else { name.to_owned() }
                };
                source.path = locator("Audio Files/Missing-Pcm.wav");
                source.unsaved = false;
                source.original = Some(OriginalProvenance {
                    version: 1,
                    policy: ImportPolicy::Copy,
                    path: locator("Original Audio/Missing-Original.wav"),
                    sha256: "0".repeat(64),
                    bytes: 1,
                    info: AudioInfo {
                        format: FileFormat::Wav,
                        sample_format: SampleFormat::Int16,
                        sample_rate: 48_000,
                        channels: 1,
                        frames: 2,
                        bwf: None,
                    },
                });
                for leaf in ["Original Audio/missing-original.WAV", "Audio Files/missing-pcm.WAV"] {
                    let destination = folder.join(leaf);
                    for relative_request in [false, true] {
                        let request = if relative_request { destination.strip_prefix(&cwd).unwrap().to_owned() } else { destination.clone() };
                        let error = prepare_destination(&session, &request).err().unwrap().to_string();
                        assert!(error.contains("recorded media locator"), "{error}");
                        assert!(!destination.exists());
                    }
                }
                assert!(prepare_destination(&session, &folder.join("Unrelated.wav")).is_ok(), "do not refuse the whole parent directory");
            }
        }
    }

    #[test]
    fn ascii_path_comparison_does_not_claim_unicode_case_equivalence() {
        assert!(same_recorded_path(Path::new("Audio/Missing.WAV"), Path::new("audio/missing.wav")));
        assert!(!same_recorded_path(Path::new("Audio/É.wav"), Path::new("Audio/é.wav")));
        assert!(!same_recorded_path(Path::new("Audio/Missing.wav"), Path::new("Audio/Other.wav")));
    }
}
