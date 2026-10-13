//! Encoded originals, control-side ownership and atomic audio-import preparation.
//!
//! A lease only retains a path. Filesystem collection is explicit; dropping a session, history
//! snapshot or playback state never reads or deletes an original file.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use soundcraft_audio_io::{AudioInfo, FileFormat, ReaderOptions};
use soundcraft_model::{
    ChannelFormat, Clip, ImportPolicy, OriginalAssetLease, OriginalProvenance, Session, Source, SourceAudio, SourceId, TrackId, TrackKind,
};
use soundcraft_time::{SampleRate, Samples};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

pub const DEFAULT_MEMORY_BYTES: u64 = 1 << 30;
pub const DEFAULT_ORIGINAL_BYTES: u64 = 8 << 30;
const MAX_BATCH: usize = 1_000;
const MAX_VALUES: u64 = 1 << 30;
const MAX_PATH: usize = 4_096;
const CHUNK: usize = 64 << 10;
const MAX_ASSETS: usize = 10_000;

fn original_error(message: impl std::fmt::Display) -> EngineError {
    EngineError::Io(format!("original media: {message}"))
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) { Err(original_error("import cancelled")) } else { Ok(()) }
}
fn text_path(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| original_error("path is not UTF-8"))?;
    if text.is_empty() || text.len() > MAX_PATH || text.contains('\0') {
        return Err(original_error("path must be nonempty, without NUL, and at most 4096 bytes"));
    }
    Ok(text.to_owned())
}

struct TrackedAsset {
    path: PathBuf,
    lease: Weak<OriginalAssetLease>,
    bytes: u64,
    owned: Metadata,
}

/// An exclusively created temporary store. No filesystem action occurs in its destructor.
pub struct OriginalStore {
    root: PathBuf,
    max_bytes: u64,
    next: u64,
    assets: Vec<TrackedAsset>,
}

impl OriginalStore {
    pub fn new(root: PathBuf, max_bytes: u64) -> Result<Self> {
        if cfg!(target_arch = "wasm32") {
            return Err(EngineError::Unsupported("encoded-original preservation requires native storage".into()));
        }
        text_path(&root)?;
        std::fs::create_dir(&root).map_err(original_error)?;
        Ok(Self { root, max_bytes, next: 0, assets: Vec::new() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Remove only expired files whose current identity still matches our exclusively created file.
    pub fn collect_expired(&mut self) -> Result<usize> {
        let mut removed = 0;
        let mut i = 0;
        while i < self.assets.len() {
            let Some(asset) = self.assets.get(i) else { break };
            if asset.lease.strong_count() != 0 {
                i += 1;
                continue;
            }
            remove_owned(&asset.path, &asset.owned)?;
            self.assets.remove(i);
            removed += 1;
        }
        Ok(removed)
    }

    fn stage<R: Read>(&mut self, mut input: R, bound: u64, cancel: &AtomicBool) -> Result<(Arc<OriginalAssetLease>, String, u64)> {
        self.collect_expired()?;
        let retained = self.assets.iter().try_fold(0u64, |n, a| n.checked_add(a.bytes)).ok_or_else(|| original_error("store size overflow"))?;
        if self.assets.len() >= MAX_ASSETS || bound > self.max_bytes.saturating_sub(retained) {
            return Err(original_error("managed-original disk budget exceeded"));
        }
        self.next = self.next.checked_add(1).ok_or_else(|| original_error("store identity exhausted"))?;
        let path = self.root.join(format!("original-{}.asset", self.next));
        let mut output = OpenOptions::new().write(true).create_new(true).open(&path).map_err(original_error)?;
        let owned = output.metadata().map_err(original_error)?;
        let result = stream_copy(&mut input, &mut output, bound, cancel).and_then(|receipt| {
            output.sync_all().map_err(original_error)?;
            Ok(receipt)
        });
        drop(output);
        let (hash, bytes) = match result {
            Ok(receipt) => receipt,
            Err(err) => {
                remove_owned(&path, &owned)?;
                return Err(err);
            }
        };
        let mut permissions = owned.permissions();
        permissions.set_readonly(true);
        if let Err(err) = std::fs::set_permissions(&path, permissions) {
            remove_owned(&path, &owned)?;
            return Err(original_error(err));
        }
        let lease = Arc::new(OriginalAssetLease { path: path.clone() });
        self.assets.push(TrackedAsset { path, lease: Arc::downgrade(&lease), bytes, owned });
        Ok((lease, hash, bytes))
    }
}

fn remove_owned(path: &Path, owned: &Metadata) -> Result<()> {
    let current = match std::fs::symlink_metadata(path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(original_error(err)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !current.is_file() || current.file_type().is_symlink() || current.dev() != owned.dev() || current.ino() != owned.ino() {
            return Err(original_error("owned snapshot path was replaced; refusing cleanup"));
        }
        std::fs::remove_file(path).map_err(original_error)
    }
    #[cfg(not(unix))]
    {
        let _ = (current, owned);
        Err(original_error("this platform cannot prove snapshot ownership for cleanup"))
    }
}

fn stream_copy(input: &mut impl Read, output: &mut impl Write, bound: u64, cancel: &AtomicBool) -> Result<(String, u64)> {
    let mut buffer = [0u8; CHUNK];
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    loop {
        cancelled(cancel)?;
        let n = input.read(&mut buffer).map_err(original_error)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).filter(|n| *n <= bound).ok_or_else(|| original_error("encoded-byte bound exceeded"))?;
        let chunk = buffer.get(..n).ok_or_else(|| original_error("invalid reader chunk"))?;
        output.write_all(chunk).map_err(original_error)?;
        hash.update(chunk);
    }
    cancelled(cancel)?;
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn file_hash(path: &Path, bound: u64, cancel: &AtomicBool) -> Result<(String, u64)> {
    let mut file = File::open(path).map_err(|err| original_error(format!("{}: {err}", path.display())))?;
    if !file.metadata().map_err(original_error)?.is_file() {
        return Err(original_error("original must be a regular file"));
    }
    stream_copy(&mut file, &mut std::io::sink(), bound, cancel)
}

fn store(e: &mut Engine) -> Result<&mut OriginalStore> {
    if e.original_store.is_none() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(original_error)?.as_nanos();
        let root = std::env::temp_dir().join(format!("soundcraft-originals-{}-{nonce}", std::process::id()));
        e.original_store = Some(OriginalStore::new(root, DEFAULT_ORIGINAL_BYTES)?);
    }
    e.original_store.as_mut().ok_or_else(|| original_error("original store unavailable"))
}

/// Whole-batch admission and cancellation controls. Budgets are checked limits, not free-space estimates.
#[derive(Clone)]
pub struct ImportOptions {
    pub policy: ImportPolicy,
    pub rate_conversion: bool,
    pub use_timestamp: bool,
    pub target: Option<TrackId>,
    pub at: Samples,
    pub cancel: Arc<AtomicBool>,
    pub max_memory_bytes: u64,
    pub max_original_bytes: u64,
}
impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            policy: ImportPolicy::Copy,
            rate_conversion: false,
            use_timestamp: false,
            target: None,
            at: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            max_memory_bytes: DEFAULT_MEMORY_BYTES,
            max_original_bytes: DEFAULT_ORIGINAL_BYTES,
        }
    }
}

/// An unpublished result. Dropping it performs no filesystem work; collect expired assets explicitly.
pub struct ImportPlan {
    before: Arc<Session>,
    revision: u64,
    generation: u64,
    session: Session,
    receipt: Value,
    cancel: Arc<AtomicBool>,
}
impl ImportPlan {
    pub fn receipt(&self) -> &Value {
        &self.receipt
    }
}

fn converted_frames(frames: u64, from: u32, to: u32) -> Result<u64> {
    if from == 0 || to == 0 {
        return Err(original_error("zero sample rate"));
    }
    let result = (u128::from(frames) * u128::from(to) + u128::from(from / 2)) / u128::from(from);
    u64::try_from(result).map_err(original_error)
}
fn dimensions(info: &AudioInfo, allow_unknown: bool) -> Result<()> {
    SampleRate::new(info.sample_rate).map_err(original_error)?;
    if info.channels == 0
        || usize::from(info.channels) > 64
        || (!allow_unknown && info.frames == 0)
        || info.frames.checked_mul(u64::from(info.channels)).is_none_or(|n| n > MAX_VALUES)
    {
        return Err(original_error("empty audio or unsupported channel/frame dimensions"));
    }
    Ok(())
}
fn retained_memory(session: &Session) -> Result<u64> {
    // Eight bytes per sample bounds PCM plus the min/max peak hierarchy and its construction.
    session
        .pool
        .ids()
        .try_fold(0u64, |total, id| {
            let values = session.pool.get(id).map_or(Some(0), |audio| (audio.buffer.frames() as u64).checked_mul(audio.buffer.num_channels() as u64));
            total.checked_add(values?.checked_mul(8)?)
        })
        .ok_or_else(|| original_error("decoded-memory size overflow"))
}

pub fn prepare_audio_import(e: &mut Engine, paths: &[PathBuf], options: ImportOptions) -> Result<ImportPlan> {
    let result = prepare_inner(e, paths, options);
    if result.is_err() {
        e.collect_original_assets()?;
    }
    result
}
fn prepare_inner(e: &mut Engine, paths: &[PathBuf], options: ImportOptions) -> Result<ImportPlan> {
    if cfg!(target_arch = "wasm32") {
        return Err(EngineError::Unsupported("encoded-original import requires native storage".into()));
    }
    cancelled(&options.cancel)?;
    if paths.is_empty() || paths.len() > MAX_BATCH || options.at < 0 {
        return Err(original_error("import needs 1..1000 paths and a nonnegative position"));
    }
    let before = e.session_arc();
    let revision = e.revision;
    let generation = e.session_generation();
    let mut s = (*before).clone();
    if let Some(target) = options.target
        && s.track(target).is_none_or(|t| t.kind != TrackKind::Audio || t.playlist().is_none())
    {
        return Err(original_error("target must be an audio track with a playlist"));
    }
    let ids = (paths.len() as u64).checked_mul(if options.target.is_some() { 2 } else { 3 }).ok_or_else(|| original_error("ID count overflow"))?;
    s.next_id.max(1).checked_add(ids).filter(|next| *next < u64::MAX).ok_or_else(|| original_error("session ID capacity exhausted"))?;
    let sr = s.sample_rate.hz();
    SampleRate::new(sr).map_err(original_error)?;
    let mut memory = retained_memory(&s)?;
    let mut encoded = 0u64;
    let mut rows = Vec::with_capacity(paths.len());
    for input in paths {
        cancelled(&options.cancel)?;
        text_path(input)?;
        let external = input.canonicalize().map_err(original_error)?;
        let external_text = text_path(&external)?;
        let path_metadata = std::fs::metadata(&external).map_err(original_error)?;
        if !path_metadata.is_file() || path_metadata.len() == 0 {
            return Err(original_error("original must be a nonempty regular file"));
        }
        let original_file = File::open(&external).map_err(original_error)?;
        let metadata = original_file.metadata().map_err(original_error)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(original_error("original must be a nonempty regular file"));
        }
        encoded = encoded
            .checked_add(metadata.len())
            .filter(|n| *n <= options.max_original_bytes)
            .ok_or_else(|| original_error("batch original-byte budget exceeded"))?;
        let (lease, hash, bytes) = store(e)?.stage(original_file, metadata.len(), &options.cancel)?;
        // A second pass detects ordinary file changes; no atomic snapshot of arbitrary concurrent writers is promised.
        let (external_hash, external_bytes) = file_hash(&external, metadata.len(), &options.cancel)?;
        if external_hash != hash || external_bytes != bytes {
            return Err(original_error("original changed while importing"));
        }
        let ext = external.extension().and_then(|x| x.to_str());
        let available = options.max_memory_bytes.checked_sub(memory).ok_or_else(|| original_error("decoded-memory budget exceeded"))?;
        let max_values = (available / 16).min(MAX_VALUES);
        if max_values == 0 {
            return Err(original_error("decoded-memory budget exceeded"));
        }
        let reader_options = || ReaderOptions { max_bytes: bytes, max_sample_values: max_values, cancel: Arc::clone(&options.cancel) };
        let probe =
            soundcraft_audio_io::probe_reader(File::open(&lease.path).map_err(original_error)?, ext, reader_options()).map_err(original_error)?;
        dimensions(&probe, true)?;
        if probe.sample_rate != sr && options.policy != ImportPolicy::Convert && !options.rate_conversion {
            return Err(original_error("sample rates differ; choose Convert or explicitly allow rate_conversion"));
        }
        if probe.frames > 0 {
            let values = probe.frames.checked_mul(u64::from(probe.channels)).ok_or_else(|| original_error("sample count overflow"))?;
            let out = converted_frames(probe.frames, probe.sample_rate, sr)?
                .checked_mul(u64::from(probe.channels))
                .ok_or_else(|| original_error("converted sample count overflow"))?;
            if values
                .checked_mul(4)
                .and_then(|n| out.checked_mul(12).and_then(|out| n.checked_add(out)))
                .and_then(|n| n.checked_add(if probe.sample_rate != sr { 8 << 20 } else { 0 }))
                .is_none_or(|n| n > available)
            {
                return Err(original_error("decode/conversion peak memory budget exceeded"));
            }
        }
        let (info, mut buf) =
            soundcraft_audio_io::decode_reader(File::open(&lease.path).map_err(original_error)?, ext, reader_options()).map_err(original_error)?;
        dimensions(&info, false)?;
        if buf.frames() as u64 != info.frames
            || buf.num_channels() != usize::from(info.channels)
            || buf.sample_rate != info.sample_rate
            || buf.channels.iter().any(|c| c.len() != buf.frames())
        {
            return Err(original_error("decoder dimensions differ from original metadata"));
        }
        let frames = converted_frames(info.frames, info.sample_rate, sr)?;
        let out_values = frames
            .checked_mul(u64::from(info.channels))
            .filter(|n| *n > 0 && *n <= MAX_VALUES)
            .ok_or_else(|| original_error("conversion dimensions exceed allocation limit"))?;
        let in_values = info.frames.checked_mul(u64::from(info.channels)).ok_or_else(|| original_error("sample count overflow"))?;
        if in_values
            .checked_mul(4)
            .and_then(|n| out_values.checked_mul(12).and_then(|out| n.checked_add(out)))
            .and_then(|n| n.checked_add(if info.sample_rate != sr { 8 << 20 } else { 0 }))
            .is_none_or(|n| n > available)
        {
            return Err(original_error("decode/conversion peak memory budget exceeded"));
        }
        let converted = info.sample_rate != sr;
        if converted {
            cancelled(&options.cancel)?;
            let count = usize::try_from(frames).map_err(original_error)?;
            buf.channels = soundcraft_dsp::offline::resample_to_frames_cancellable(&buf.channels, info.sample_rate, sr, count, || {
                options.cancel.load(Ordering::Acquire)
            })
            .map_err(original_error)?;
            buf.sample_rate = sr;
            cancelled(&options.cancel)?;
        }
        memory = memory
            .checked_add(out_values.checked_mul(8).ok_or_else(|| original_error("memory overflow"))?)
            .ok_or_else(|| original_error("memory overflow"))?;
        let original = OriginalProvenance {
            version: 1,
            policy: options.policy,
            path: if options.policy == ImportPolicy::Reference { external_text } else { text_path(&lease.path)? },
            sha256: hash,
            bytes,
            info,
        };
        let at = if options.use_timestamp {
            original
                .timeline_time_reference(sr)
                .map_err(original_error)?
                .ok_or_else(|| original_error("original has no BWF timestamp"))?
                .checked_sub(s.timecode_start)
                .filter(|n| *n >= 0)
                .ok_or_else(|| original_error("BWF timestamp lies before session start or overflows"))?
        } else {
            options.at
        };
        let length = i64::try_from(frames).map_err(original_error)?;
        at.checked_add(length).ok_or_else(|| original_error("clip timeline overflow"))?;
        let name = external.file_stem().and_then(|n| n.to_str()).unwrap_or("Audio").to_owned();
        let source = SourceId(s.alloc());
        let path = format!("Audio Files/{}_{}.wav", crate::io::sanitize_name(&name), source.0);
        let identity = crate::media::audio_identity(&buf);
        s.sources.push(Source {
            id: source,
            name: name.clone(),
            path: path.clone(),
            channels: original.info.channels,
            frames,
            sample_rate: sr,
            format: FileFormat::Wav,
            time_reference: original.info.bwf.as_ref().map_or(0, |b| b.time_reference),
            unsaved: true,
            audio_identity: Some(identity.clone()),
            original: Some(original.clone()),
        });
        let audio =
            if options.policy == ImportPolicy::Reference { SourceAudio::new(buf) } else { SourceAudio::with_original_asset(buf, Arc::clone(&lease)) };
        s.pool.insert(source, Arc::new(audio));
        let track = options
            .target
            .unwrap_or_else(|| s.add_track(TrackKind::Audio, ChannelFormat::for_channels(usize::from(original.info.channels)), Some(&name)));
        let clip = s.new_clip_id();
        crate::edit::place_clip(&mut s, track, Clip::audio(clip, name, source, 0, at, length));
        rows.push(json!({"source":source,"track":track,"clip":clip,"frames":frames,"channels":original.info.channels,"at":at,"policy":options.policy,
            "original":{"path":original.path,"sha256":original.sha256,"bytes":original.bytes,"sample_rate":original.info.sample_rate,
                "frames":original.info.frames,"channels":original.info.channels,"time_reference":original.info.bwf.as_ref().map(|b|b.time_reference),"bwf":original.info.bwf},
            "derivative":{"path":path,"audio_identity":identity,"sample_rate":sr,"frames":frames,"channels":original.info.channels,"converted":converted}}));
    }
    cancelled(&options.cancel)?;
    e.collect_original_assets()?;
    Ok(ImportPlan {
        before,
        revision,
        generation,
        session: s,
        receipt: json!({"imported":rows,"policy":options.policy,"rate_conversion":options.rate_conversion || options.policy == ImportPolicy::Convert}),
        cancel: options.cancel,
    })
}

pub fn apply_audio_import(e: &mut Engine, plan: ImportPlan) -> Result<Value> {
    let result = apply_inner(e, plan);
    if result.is_err() {
        e.collect_original_assets()?;
    }
    result
}
fn apply_inner(e: &mut Engine, plan: ImportPlan) -> Result<Value> {
    cancelled(&plan.cancel)?;
    if e.revision != plan.revision || e.session_generation() != plan.generation || !Arc::ptr_eq(&plan.before, &e.session_arc()) {
        return Err(original_error("session changed after import preparation"));
    }
    for src in plan.session.sources.iter().skip(plan.before.sources.len()) {
        if let Some(original) = &src.original {
            verify_original(original, plan.session.folder.as_deref(), &plan.cancel)?;
        }
    }
    cancelled(&plan.cancel)?;
    *e.session_mut() = plan.session;
    Ok(plan.receipt)
}

/// Validate encoded original identity independently of the playback PCM identity.
pub fn original_availability(original: &OriginalProvenance, folder: Option<&Path>) -> Result<PathBuf> {
    if original.version != 1 {
        return Err(EngineError::Unsupported("original media provenance version".into()));
    }
    dimensions(&original.info, false)?;
    if original.bytes == 0 || original.sha256.len() != 64 || !original.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err(original_error("invalid encoded-original identity"));
    }
    text_path(Path::new(&original.path))?;
    let path = if Path::new(&original.path).is_absolute() {
        PathBuf::from(&original.path)
    } else {
        folder.ok_or_else(|| original_error("relative original has no session folder"))?.join(&original.path)
    };
    let metadata = std::fs::metadata(&path).map_err(|err| original_error(format!("{}: {err}", path.display())))?;
    if !metadata.is_file() || metadata.len() != original.bytes {
        return Err(original_error("encoded-original file/length unavailable"));
    }
    Ok(path)
}

pub fn verify_original(original: &OriginalProvenance, folder: Option<&Path>, cancel: &Arc<AtomicBool>) -> Result<PathBuf> {
    let path = original_availability(original, folder)?;
    let (hash, bytes) = file_hash(&path, original.bytes, cancel)?;
    if hash != original.sha256 || bytes != original.bytes {
        return Err(original_error(format!("{}: encoded-original hash/length mismatch", path.display())));
    }
    let options = ReaderOptions { max_bytes: original.bytes, max_sample_values: MAX_VALUES, cancel: Arc::clone(cancel) };
    let hint = if original.info.format == FileFormat::Aac { Some("m4a") } else { None };
    let mut info = soundcraft_audio_io::probe_reader(File::open(&path).map_err(original_error)?, hint, options.clone()).map_err(original_error)?;
    if info != original.info {
        // Container counts can include padding that a gapless decoder removes. Independently
        // decode with a bound, then require exact actual metadata rather than trusting the record.
        let bounded = ReaderOptions { max_sample_values: DEFAULT_MEMORY_BYTES / 16, ..options };
        info = soundcraft_audio_io::decode_reader(File::open(&path).map_err(original_error)?, hint, bounded).map_err(original_error)?.0;
    }
    if info != original.info {
        return Err(original_error("encoded-original metadata differs from recorded provenance"));
    }
    cancelled(cancel)?;
    Ok(path)
}

/// Ordinary saves remain available with missing media; these warnings are independent of PCM.
pub fn original_issues(session: &Session) -> Vec<String> {
    let cancel = Arc::new(AtomicBool::new(false));
    session
        .sources
        .iter()
        .filter_map(|src| {
            src.original
                .as_ref()
                .and_then(|original| verify_original(original, session.folder.as_deref(), &cancel).err().map(|err| format!("{}: {err}", src.name)))
        })
        .collect()
}

/// Copy verified originals into a native folder. Reference locators remain external on ordinary
/// Save; portable copies materialize them and update only the returned session clone.
pub(crate) fn persist_originals(s: &mut Session, destination: &Path, portable: bool) -> Result<usize> {
    let cancel = Arc::new(AtomicBool::new(false));
    let from = s.folder.clone();
    if s.sources.iter().filter(|src| src.original.is_some()).count() > MAX_ASSETS {
        return Err(original_error("too many original sources"));
    }
    let verified: Vec<_> = s.sources.iter().filter_map(|src| src.original.as_ref()).map(|o| verify_original(o, from.as_deref(), &cancel)).collect();
    let dir = destination.join("Original Audio");
    if let Ok(meta) = std::fs::symlink_metadata(&dir)
        && (!meta.is_dir() || meta.file_type().is_symlink())
    {
        return Err(original_error("Original Audio must be a regular directory, not a symlink"));
    }
    let mut written = 0;
    let mut paths = verified.into_iter();
    for src in &mut s.sources {
        let Some(original) = &mut src.original else { continue };
        let input = match paths.next().ok_or_else(|| original_error("original plan changed"))? {
            Ok(path) => path,
            Err(err) => {
                if portable {
                    return Err(err);
                }
                if Path::new(&original.path).is_relative()
                    && let Some(from) = &from
                {
                    original.path = text_path(&from.join(&original.path))?;
                }
                continue;
            }
        };
        let managed = Path::new(&original.path).is_relative() || s.pool.original_asset(src.id).is_some();
        if original.policy == ImportPolicy::Reference && !portable && !managed {
            original.path = text_path(&input.canonicalize().map_err(original_error)?)?;
            continue;
        }
        std::fs::create_dir_all(&dir).map_err(original_error)?;
        let name = original_name(src.id, original);
        let output = dir.join(&name);
        if output.exists() {
            let metadata = std::fs::symlink_metadata(&output).map_err(original_error)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(original_error("original destination is not a regular file"));
            }
            let (hash, bytes) = file_hash(&output, original.bytes, &cancel)?;
            if hash != original.sha256 || bytes != original.bytes {
                return Err(original_error("original destination contains different bytes"));
            }
        } else {
            let mut handle = OpenOptions::new().write(true).create_new(true).open(&output).map_err(original_error)?;
            let owned = handle.metadata().map_err(original_error)?;
            let result = File::open(&input)
                .map_err(original_error)
                .and_then(|mut input| stream_copy(&mut input, &mut handle, original.bytes, &cancel))
                .and_then(|receipt| {
                    handle.sync_all().map_err(original_error)?;
                    Ok(receipt)
                });
            drop(handle);
            match result {
                Ok((hash, bytes)) if hash == original.sha256 && bytes == original.bytes => {
                    let mut permissions = owned.permissions();
                    permissions.set_readonly(true);
                    std::fs::set_permissions(&output, permissions).map_err(original_error)?;
                    written += 1;
                }
                Ok(_) => {
                    remove_owned(&output, &owned)?;
                    return Err(original_error("original changed while publishing"));
                }
                Err(err) => {
                    remove_owned(&output, &owned)?;
                    return Err(err);
                }
            }
        }
        original.path = format!("Original Audio/{name}");
    }
    if written > 0 {
        crate::io::sync_dir(&dir);
    }
    Ok(written)
}
fn original_name(id: SourceId, original: &OriginalProvenance) -> String {
    // The encoded format is unchanged; extension identifies its format rather than a derivative WAV.
    let extension = format_hint(original.info.format);
    format!("original_{}_{}.{}", id.0, original.sha256, extension)
}

fn format_hint(format: FileFormat) -> &'static str {
    match format {
        FileFormat::Wav => "wav",
        FileFormat::Aiff => "aiff",
        FileFormat::Flac => "flac",
        FileFormat::Mp3 => "mp3",
        FileFormat::Ogg => "ogg",
        FileFormat::Aac => "aac",
        FileFormat::Alac => "m4a",
        FileFormat::Caf => "caf",
        FileFormat::Other => "audio",
    }
}

/// Stream originals directly into a bounded portable archive; never buffer an encoded original
/// separately from the already bounded archive output.
pub(crate) fn bundle_originals<W: Write + std::io::Seek>(s: &mut Session, writer: &mut zip::ZipWriter<W>) -> Result<(u64, usize)> {
    if cfg!(target_arch = "wasm32") && s.sources.iter().any(|src| src.original.is_some()) {
        return Err(EngineError::Unsupported("browser encoded-original retention is not implemented".into()));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let from = s.folder.clone();
    let mut bytes = 0u64;
    let mut entries = 0usize;
    for src in &mut s.sources {
        let Some(original) = &mut src.original else { continue };
        let path = verify_original(original, from.as_deref(), &cancel)?;
        bytes = bytes.checked_add(original.bytes).filter(|n| *n <= 1 << 30).ok_or_else(|| original_error("originals exceed bundle byte limit"))?;
        let name = format!("Original Audio/{}", original_name(src.id, original));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
        writer.start_file(&name, opts).map_err(original_error)?;
        let (hash, size) = stream_copy(&mut File::open(path).map_err(original_error)?, writer, original.bytes, &cancel)?;
        if hash != original.sha256 || size != original.bytes {
            return Err(original_error("original changed while bundling"));
        }
        original.path = name;
        entries += 1;
    }
    Ok((bytes, entries))
}

/// Admission before archive/encoded-WAV allocation. The existing bundle budget includes originals,
/// derivatives and rewritten JSON; codec internals remain outside this adapter's bound.
pub(crate) fn check_bundle_limits(s: &Session) -> Result<()> {
    let originals = s.sources.iter().filter(|src| src.original.is_some()).count();
    if s.sources.len().checked_add(originals).and_then(|n| n.checked_add(1)).is_none_or(|n| n > 10_000) {
        return Err(original_error("bundle entry limit exceeded"));
    }
    struct Count(u64);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= 1 << 30)
                .ok_or_else(|| std::io::Error::other("bundle metadata limit exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer_pretty(&mut count, s).map_err(original_error)?;
    let mut total = count.0.checked_add((s.sources.len() as u64).saturating_mul(512)).ok_or_else(|| original_error("bundle size overflow"))?;
    for src in &s.sources {
        if let Some(original) = &src.original {
            total = total.checked_add(original.bytes).ok_or_else(|| original_error("bundle size overflow"))?;
        }
        if let Some(audio) = s.pool.get(src.id) {
            let bytes = (audio.buffer.frames() as u64)
                .checked_mul(audio.buffer.num_channels() as u64)
                .and_then(|n| n.checked_mul(4))
                .and_then(|n| n.checked_add(128))
                .ok_or_else(|| original_error("bundle size overflow"))?;
            total = total.checked_add(bytes).ok_or_else(|| original_error("bundle size overflow"))?;
        }
    }
    if total > 1 << 30 {
        return Err(original_error("originals, playback audio and metadata exceed bundle byte limit"));
    }
    Ok(())
}

pub(crate) enum BundleOriginalFailure {
    Invalid(EngineError),
    Storage(EngineError),
}

/// Restore a verified original archive entry into an exclusively owned managed snapshot.
pub(crate) fn restore_bundle_original(
    e: &mut Engine,
    original: &mut OriginalProvenance,
    bytes: &[u8],
) -> std::result::Result<Arc<OriginalAssetLease>, BundleOriginalFailure> {
    if original.version != 1 {
        return Err(BundleOriginalFailure::Storage(EngineError::Unsupported("original media provenance version".into())));
    }
    dimensions(&original.info, false).map_err(BundleOriginalFailure::Invalid)?;
    if bytes.len() as u64 != original.bytes || format!("{:x}", Sha256::digest(bytes)) != original.sha256 {
        return Err(BundleOriginalFailure::Invalid(original_error("bundle encoded-original hash/length mismatch")));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (lease, hash, size) = store(e)
        .map_err(BundleOriginalFailure::Storage)?
        .stage(std::io::Cursor::new(bytes), original.bytes, &cancel)
        .map_err(BundleOriginalFailure::Storage)?;
    if hash != original.sha256 || size != original.bytes {
        return Err(BundleOriginalFailure::Invalid(original_error("bundle encoded-original hash/length mismatch")));
    }
    let mut candidate = original.clone();
    candidate.path = text_path(&lease.path).map_err(BundleOriginalFailure::Storage)?;
    verify_original(&candidate, None, &cancel).map_err(BundleOriginalFailure::Invalid)?;
    *original = candidate;
    Ok(lease)
}
