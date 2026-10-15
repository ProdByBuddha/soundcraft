//! Independent encoded-byte/destination acceptance. Playback fidelity is tested separately.
//! No production codec or renderer constructs the expected Float32/BWF wire bytes below.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use soundcraft_audio_io::{AudioBuffer, AudioInfo, BwfInfo, FileFormat, SampleFormat};
use soundcraft_engine::{Engine, Result, bounce_delivery, io};
use soundcraft_model::{ChannelFormat, Clip, ImportPolicy, OriginalProvenance, Route, Session, TrackKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const FRAMES: usize = 97;
const TIMESTAMP: u64 = (1 << 40) + 12_345;
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("soundcraft-bounce-safety-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample(channel: usize, frame: usize) -> f32 {
    const LEFT: [f32; 4] = [0.25, -0.125, 1.25, -0.5];
    const RIGHT: [f32; 4] = [0.5, 0.125, -2.5, -0.25];
    [LEFT, RIGHT][channel][frame % 4]
}
fn pcm_bytes(start: usize, end: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in start..end {
        for channel in 0..2 {
            out.extend_from_slice(&sample(channel, frame).to_le_bytes());
        }
    }
    out
}
fn bwf() -> BwfInfo {
    BwfInfo {
        description: "Independent delivery fixture".into(),
        originator: "SoundCraft test".into(),
        originator_reference: "SC-MX38-wire-v1".into(),
        origination_date: "2026-10-13".into(),
        origination_time: "04:12:23".into(),
        time_reference: TIMESTAMP,
    }
}
fn fixed(out: &mut [u8], at: usize, text: &str) {
    out[at..at + text.len()].copy_from_slice(text.as_bytes());
}
fn chunk(out: &mut Vec<u8>, name: &[u8; 4], bytes: &[u8]) {
    out.extend_from_slice(name);
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
    out.extend_from_slice(bytes);
    if !bytes.len().is_multiple_of(2) {
        out.push(0);
    }
}
fn wire(rate: u32) -> Vec<u8> {
    // Literal WAVE_FORMAT_EXTENSIBLE: two Float32 channels, L/R mask, eight bytes/frame.
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xfffe_u16.to_le_bytes());
    fmt.extend_from_slice(&2_u16.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * 8).to_le_bytes());
    fmt.extend_from_slice(&8_u16.to_le_bytes());
    fmt.extend_from_slice(&32_u16.to_le_bytes());
    fmt.extend_from_slice(&22_u16.to_le_bytes());
    fmt.extend_from_slice(&32_u16.to_le_bytes());
    fmt.extend_from_slice(&3_u32.to_le_bytes());
    fmt.extend_from_slice(&[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
    assert_eq!(fmt.len(), 40);
    let mut bext = vec![0_u8; 602];
    let metadata = bwf();
    fixed(&mut bext, 0, &metadata.description);
    fixed(&mut bext, 256, &metadata.originator);
    fixed(&mut bext, 288, &metadata.originator_reference);
    fixed(&mut bext, 320, &metadata.origination_date);
    fixed(&mut bext, 330, &metadata.origination_time);
    bext[338..346].copy_from_slice(&TIMESTAMP.to_le_bytes());
    bext[346..348].copy_from_slice(&1_u16.to_le_bytes());
    let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
    chunk(&mut out, b"fmt ", &fmt);
    chunk(&mut out, b"bext", &bext);
    chunk(&mut out, b"JvOr", &[9, 8, 7, 6, 5]);
    chunk(&mut out, b"data", &pcm_bytes(0, FRAMES));
    let riff_bytes = u32::try_from(out.len() - 8).unwrap();
    out[4..8].copy_from_slice(&riff_bytes.to_le_bytes());
    out
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn chunks(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    assert!(bytes.len() >= 12);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
    let mut out = Vec::new();
    let mut at = 12_usize;
    while at < bytes.len() {
        assert!(at + 8 <= bytes.len());
        let name: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let end = (at + 8).checked_add(len).unwrap();
        assert!(end <= bytes.len());
        assert!(!out.iter().any(|(previous, _)| *previous == name), "duplicate chunk");
        out.push((name, &bytes[at + 8..end]));
        at = end.checked_add(len % 2).unwrap();
    }
    assert_eq!(at, bytes.len());
    out
}
fn payload<'a>(chunks: &[([u8; 4], &'a [u8])], name: &[u8; 4]) -> &'a [u8] {
    chunks.iter().find(|(tag, _)| tag == name).unwrap().1
}
fn assert_float_wave(bytes: &[u8], rate: u32, start: usize, end: usize) {
    assert!(start < end);
    let chunks = chunks(bytes);
    let fmt = payload(&chunks, b"fmt ");
    assert!(fmt.len() >= 40);
    assert_eq!(u16::from_le_bytes(fmt[0..2].try_into().unwrap()), 0xfffe);
    assert_eq!(u16::from_le_bytes(fmt[2..4].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(fmt[4..8].try_into().unwrap()), rate);
    assert_eq!(u32::from_le_bytes(fmt[8..12].try_into().unwrap()), rate * 8);
    assert_eq!(u16::from_le_bytes(fmt[12..14].try_into().unwrap()), 8);
    assert_eq!(u16::from_le_bytes(fmt[14..16].try_into().unwrap()), 32);
    assert_eq!(u16::from_le_bytes(fmt[18..20].try_into().unwrap()), 32);
    assert_eq!(u32::from_le_bytes(fmt[20..24].try_into().unwrap()), 3);
    assert_eq!(&fmt[24..40], &[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
    let data = payload(&chunks, b"data");
    assert_eq!(data.len(), (end - start) * 2 * 4);
    assert_eq!(data, pcm_bytes(start, end));
    for (index, encoded) in data.chunks_exact(4).enumerate() {
        let actual = f32::from_le_bytes(encoded.try_into().unwrap());
        assert!(actual.is_finite());
        assert_eq!(actual.to_bits(), sample(index % 2, start + index / 2).to_bits());
    }
    assert!(data.chunks_exact(4).any(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).abs() > 1.0));
}
fn assert_bwf(bytes: &[u8]) {
    let chunks = chunks(bytes);
    let bext = payload(&chunks, b"bext");
    assert_eq!(bext.len(), 602);
    for (at, width, expected) in [
        (0, 256, "Independent delivery fixture"),
        (256, 32, "SoundCraft test"),
        (288, 32, "SC-MX38-wire-v1"),
        (320, 10, "2026-10-13"),
        (330, 8, "04:12:23"),
    ] {
        assert_eq!(&bext[at..at + expected.len()], expected.as_bytes());
        assert!(bext[at + expected.len()..at + width].iter().all(|byte| *byte == 0));
    }
    assert_eq!(u64::from_le_bytes(bext[338..346].try_into().unwrap()), TIMESTAMP);
    assert!(TIMESTAMP > u64::from(u32::MAX));
    assert_eq!(u16::from_le_bytes(bext[346..348].try_into().unwrap()), 1);
    assert!(bext[348..].iter().all(|byte| *byte == 0));
    assert_eq!(payload(&chunks, b"JvOr"), [9, 8, 7, 6, 5]);
}
fn info() -> AudioInfo {
    AudioInfo {
        format: FileFormat::Wav,
        sample_format: SampleFormat::Float32,
        sample_rate: 48_000,
        channels: 2,
        frames: FRAMES as u64,
        bwf: Some(bwf()),
    }
}

struct State {
    session: Arc<Session>,
    revision: u64,
    dirty: bool,
    path: Option<String>,
    undo: Vec<String>,
    redo: Option<String>,
    journal: Vec<(String, Value)>,
    generation: u64,
    transport: Value,
}
impl State {
    fn capture(engine: &Engine) -> Self {
        Self {
            session: engine.session_arc(),
            revision: engine.revision,
            dirty: engine.is_dirty(),
            path: engine.path.clone(),
            undo: engine.undo_history(),
            redo: engine.redo_label().map(str::to_string),
            journal: engine.journal.clone(),
            generation: engine.session_generation(),
            transport: serde_json::to_value(engine.transport).unwrap(),
        }
    }
    fn unchanged(&self, engine: &Engine) {
        assert!(Arc::ptr_eq(&self.session, &engine.session_arc()));
        assert_eq!(engine.revision, self.revision);
        assert_eq!(engine.is_dirty(), self.dirty);
        assert_eq!(engine.path, self.path);
        assert_eq!(engine.undo_history(), self.undo);
        assert_eq!(engine.redo_label(), self.redo.as_deref());
        assert_eq!(engine.journal, self.journal);
        assert_eq!(engine.session_generation(), self.generation);
        assert_eq!(serde_json::to_value(engine.transport).unwrap(), self.transport);
    }
    fn unchanged_and_redo_works(&self, engine: &mut Engine) {
        self.unchanged(engine);
        assert!(engine.redo(), "delivery must preserve a functional seeded Redo");
        assert_eq!(engine.session().tracks.iter().filter(|track| track.name == "Delivery Redo").count(), 1);
    }
}
fn seeded(mut engine: Engine) -> Engine {
    engine.execute("track.new", &json!({"name":"Delivery Redo"})).unwrap();
    assert!(engine.undo());
    assert_eq!(engine.redo_label(), Some("New..."));
    engine
}
fn engine() -> Engine {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Wire source"));
    session.track_mut(track).unwrap().mixer.pan = vec![-1.0, 1.0];
    let buffer =
        AudioBuffer { sample_rate: 48_000, channels: (0..2).map(|channel| (0..FRAMES).map(|frame| sample(channel, frame)).collect()).collect() };
    let source = io::add_source(&mut session, "Wire source", buffer, None, FileFormat::Wav);
    let id = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "Wire source", source, 0, 0, FRAMES as i64));
    seeded(Engine::new(session))
}
fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("operation must refuse"),
        Err(error) => error.to_string(),
    }
}
fn assert_stage_cleanup(path: &Path, reported: &[String]) {
    if cfg!(unix) {
        assert!(!path.exists(), "proven-owned staging must be removed");
        assert!(!reported.iter().any(|item| item == &path.to_string_lossy()));
    } else {
        assert!(path.exists(), "unproved staging must be preserved");
        assert!(reported.iter().any(|item| item == &path.to_string_lossy()));
    }
}
fn protected_session(dir: &Directory, policy: ImportPolicy) -> Session {
    let mut session = engine().session().clone();
    let folder = dir.path("session");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(folder.join("Audio Files")).unwrap();
    std::fs::create_dir(folder.join("Original Audio")).unwrap();
    session.folder = Some(folder);
    let source = &mut session.sources[0];
    source.path = "Audio Files/recorded.wav".into();
    source.unsaved = false;
    source.original = Some(OriginalProvenance {
        version: 1,
        policy,
        path: "Original Audio/original.wav".into(),
        sha256: sha(&wire(48_000)),
        bytes: wire(48_000).len() as u64,
        info: info(),
    });
    session
}

#[test]
fn literal_float_bwf_bytes_publish_exactly_at_each_rate_without_document_changes() {
    for rate in [44_100, 48_000, 96_000] {
        let dir = Directory::new();
        let bytes = wire(rate);
        let golden = match rate {
            44_100 => "c7081caa58fa7de6d56781c8079120c2f86ead1f962654b1dfface5eb2c8cb66",
            48_000 => "c61f9bfe5fc1cc1855a914f98a5840a9563acb825d5458438b46fdebcc2592bf",
            96_000 => "71b418a93ba54345f2135e0d9a055821bb0301ad0a09a638fe37e6ec5d48a267",
            _ => unreachable!(),
        };
        assert_eq!(bytes.len(), 1468);
        assert_eq!(sha(&bytes), golden, "independently fixed fixture identity");
        assert_float_wave(&bytes, rate, 0, FRAMES);
        assert_bwf(&bytes);
        let mut engine = engine();
        let before = State::capture(&engine);
        let path = dir.path("complete.wav");
        let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
        assert_eq!(plan.requested_path(), path);
        assert_eq!(plan.destination(), dir.0.canonicalize().unwrap().join("complete.wav"));
        assert!(!path.exists());
        let stage = bounce_delivery::stage(plan, &bytes).unwrap();
        let stage_path = stage.staging_path().to_owned();
        assert_eq!(std::fs::read(&stage_path).unwrap(), bytes);
        assert!(!path.exists());
        let receipt = bounce_delivery::publish(stage).unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.path, path.to_string_lossy());
        assert_eq!(Path::new(&receipt.output_path), path.canonicalize().unwrap());
        assert_eq!(receipt.bytes, bytes.len() as u64);
        assert_eq!(receipt.sha256, sha(&bytes));
        assert_stage_cleanup(&stage_path, &receipt.retained_paths);
        let delivered = std::fs::read(&path).unwrap();
        assert_eq!(delivered, bytes);
        assert_float_wave(&delivered, rate, 0, FRAMES);
        assert_bwf(&delivered);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[test]
fn actual_copy_reference_convert_originals_cannot_be_bounce_destinations() {
    for policy in ["copy", "reference", "convert"] {
        let dir = Directory::new();
        let external = dir.path("external.wav");
        let bytes = wire(48_000);
        std::fs::write(&external, &bytes).unwrap();
        let mut engine = Engine::default();
        engine.configure_original_store(dir.path("store"), 8 << 20).unwrap();
        engine.execute("file.import_audio", &json!({"path":external,"policy":policy})).unwrap();
        engine = seeded(engine);
        let before = State::capture(&engine);
        let original = engine.session().sources[0].original.as_ref().unwrap().clone();
        let original_path = PathBuf::from(&original.path);
        assert_eq!(
            original.policy,
            match policy {
                "copy" => ImportPolicy::Copy,
                "reference" => ImportPolicy::Reference,
                "convert" => ImportPolicy::Convert,
                _ => unreachable!(),
            }
        );
        assert_eq!(original.sha256, sha(&bytes));
        assert_eq!(original.bytes, bytes.len() as u64);
        assert_eq!(original.info, info());
        for path in [&external, &original_path] {
            let error = failure(bounce_delivery::prepare_destination(engine.session(), path));
            assert!(error.contains("exists"), "{error}");
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            before.unchanged(&engine);
        }
        assert_eq!(std::fs::read(&external).unwrap(), bytes);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[test]
fn missing_relative_original_and_persisted_pcm_locators_are_reserved_for_all_policies() {
    for policy in [ImportPolicy::Copy, ImportPolicy::Reference, ImportPolicy::Convert] {
        let dir = Directory::new();
        let session = protected_session(&dir, policy);
        let folder = session.folder.clone().unwrap();
        let mut engine = seeded(Engine::new(session));
        let before = State::capture(&engine);
        for relative in ["Original Audio/original.wav", "Audio Files/recorded.wav", "Audio Files/../Original Audio/original.wav"] {
            let path = folder.join(relative);
            assert!(!path.exists());
            let error = failure(bounce_delivery::prepare_destination(engine.session(), &path));
            assert!(error.contains("recorded media locator"), "{relative}: {error}");
            assert!(!path.exists());
            before.unchanged(&engine);
        }
        assert_eq!(std::fs::read_dir(folder.join("Original Audio")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(folder.join("Audio Files")).unwrap().count(), 0);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[test]
fn an_unsaved_virtual_pcm_name_does_not_reserve_a_missing_filesystem_destination() {
    let dir = Directory::new();
    let mut session = engine().session().clone();
    session.folder = Some(dir.0.clone());
    session.sources[0].path = "virtual.wav".into();
    assert!(session.sources[0].unsaved);
    assert!(session.sources[0].original.is_none());
    let mut engine = seeded(Engine::new(session));
    let before = State::capture(&engine);
    let path = dir.path("virtual.wav");
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    let receipt = bounce_delivery::publish(bounce_delivery::stage(plan, &wire(48_000)).unwrap()).unwrap();
    assert!(receipt.complete);
    assert_eq!(std::fs::read(&path).unwrap(), wire(48_000));
    before.unchanged_and_redo_works(&mut engine);
}

fn video_session(folder: Option<PathBuf>, locator: String) -> Session {
    let mut session = engine().session().clone();
    session.folder = folder;
    let id = soundcraft_model::SourceId(session.alloc());
    session.videos.push(soundcraft_model::VideoSource {
        id,
        name: "Unreferenced movie".into(),
        path: locator,
        width: 0,
        height: 0,
        frame_rate: 0.0,
        duration: 0.0,
        codec: String::new(),
    });
    session
}

fn video_refusal(session: Session, destination: &Path, expected: &str) {
    let mut engine = seeded(Engine::new(session));
    let before = State::capture(&engine);
    let parent = destination.parent().unwrap();
    let entries = std::fs::read_dir(parent).unwrap().count();
    let error = failure(bounce_delivery::prepare_destination(engine.session(), destination));
    assert!(error.contains(expected), "{error}");
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(parent).unwrap().count(), entries, "no output or stage on refusal");
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn missing_unreferenced_video_names_reserve_absolute_relative_and_cwd_destinations() {
    let dir = Directory::new();
    let destination = dir.path("MissingMovie.wav");
    for locator in [destination.to_str().unwrap().to_owned(), "MissingMovie.wav".into(), "missingmovie.WAV".into()] {
        video_refusal(video_session(Some(dir.0.clone()), locator), &destination, "current video locator");
    }
    // Exercise CWD fallback without changing the process-global directory during parallel tests.
    let cwd = std::env::current_dir().unwrap();
    let target = dir.0.canonicalize().unwrap().join("cwd-movie.wav");
    let cwd_parts: Vec<_> = cwd.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let common = cwd_parts.iter().zip(&target_parts).take_while(|(a, b)| a == b).count();
    // On different Windows drives, an absolute locator is required instead.
    if common > 0 {
        let mut relative = PathBuf::new();
        for _ in common..cwd_parts.len() {
            relative.push("..");
        }
        for component in target_parts.iter().skip(common) {
            relative.push(component.as_os_str());
        }
        video_refusal(video_session(None, relative.to_str().unwrap().into()), &target, "current video locator");
    }
}

#[cfg(unix)]
#[test]
fn deleted_cwd_keeps_absolute_video_anchors_independent_of_process_directory() {
    const CHILD_ROOT: &str = "SOUNDCRAFT_VIDEO_DELETED_CWD_CHILD_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let deleted = root.join("deleted-cwd");
        std::fs::create_dir(&deleted).unwrap();
        std::env::set_current_dir(&deleted).unwrap();
        std::fs::remove_dir(&deleted).unwrap();
        assert!(std::env::current_dir().is_err(), "fixture must actually remove CWD");
        for relative in [false, true] {
            let reserved = root.join(if relative { "relative-movie.wav" } else { "absolute-movie.wav" });
            let locator = if relative { "relative-movie.wav".to_owned() } else { reserved.to_str().unwrap().to_owned() };
            let session = video_session(relative.then(|| root.clone()), locator);
            video_refusal(session.clone(), &reserved, "current video locator");
            let mut engine = seeded(Engine::new(session));
            let before = State::capture(&engine);
            let destination = root.join(if relative { "relative-output.wav" } else { "absolute-output.wav" });
            let bytes = wire(48_000);
            let plan = bounce_delivery::prepare_destination(engine.session(), &destination).unwrap();
            let receipt = bounce_delivery::publish(bounce_delivery::stage(plan, &bytes).unwrap()).unwrap();
            assert!(receipt.complete);
            assert_eq!(receipt.bytes, bytes.len() as u64);
            assert_eq!(receipt.sha256, sha(&bytes));
            assert_eq!(std::fs::read(destination).unwrap(), bytes);
            before.unchanged_and_redo_works(&mut engine);
        }
        return;
    }
    let dir = Directory::new();
    let root = dir.0.canonicalize().unwrap();
    // Only the isolated child changes CWD. Capture its libtest output so nested fixture
    // harness lines cannot masquerade as this package's Cargo test evidence.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "deleted_cwd_keeps_absolute_video_anchors_independent_of_process_directory", "--nocapture"])
        .env(CHILD_ROOT, root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"), "child must run its fixture");
}

#[test]
fn oversized_video_base_and_combined_name_refuse_privately_without_staging() {
    let dir = Directory::new();
    for (folder, locator) in [(dir.path(&"b".repeat(4097)), "movie.mov".to_owned()), (dir.0.clone(), "m".repeat(4096))] {
        let mut engine = seeded(Engine::new(video_session(Some(folder), locator)));
        let before = State::capture(&engine);
        let destination = dir.path("output.wav");
        let error = failure(bounce_delivery::prepare_destination(engine.session(), &destination));
        assert!(error.contains("cannot resolve current video locator safely"), "{error}");
        assert!(!error.contains("movie.mov"));
        assert!(!error.contains("bbb"));
        assert!(!error.contains("mmm"));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[test]
fn unrelated_video_name_allows_literal_delivery_and_receipt_without_history_changes() {
    let dir = Directory::new();
    for locator in ["unrecovered.mov", "", "existing.mov"] {
        std::fs::write(dir.path("existing.mov"), b"metadata-only movie fixture").unwrap();
        let mut engine = seeded(Engine::new(video_session(Some(dir.0.clone()), locator.into())));
        let before = State::capture(&engine);
        let destination = dir.path(&format!("output-{}.wav", locator.len()));
        let bytes = wire(48_000);
        let plan = bounce_delivery::prepare_destination(engine.session(), &destination).unwrap();
        let receipt = bounce_delivery::publish(bounce_delivery::stage(plan, &bytes).unwrap()).unwrap();
        assert!(receipt.complete);
        assert_eq!(receipt.bytes, bytes.len() as u64);
        assert_eq!(receipt.sha256, sha(&bytes));
        assert_eq!(std::fs::read(destination).unwrap(), bytes);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[cfg(unix)]
#[test]
fn video_aliases_reserve_missing_names_and_uncertain_sources_fail_privately() {
    use std::os::unix::fs::symlink;
    let dir = Directory::new();
    let real = dir.path("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path("alias");
    symlink(&real, &alias).unwrap();
    video_refusal(video_session(None, alias.join("movie.wav").to_str().unwrap().into()), &real.join("MOVIE.wav"), "current video locator");
    let dangling = dir.path("private-dangling.mov");
    symlink(dir.path("absent-target"), &dangling).unwrap();
    let looped = dir.path("private-loop.mov");
    symlink(&looped, &looped).unwrap();
    for locator in [
        dangling.to_str().unwrap().to_owned(),
        looped.to_str().unwrap().to_owned(),
        "private-missing/../movie.mov".into(),
        "private-nul\0.mov".into(),
    ] {
        let engine = seeded(Engine::new(video_session(Some(dir.0.clone()), locator.clone())));
        let before = State::capture(&engine);
        let destination = dir.path("unrelated.wav");
        let entries = std::fs::read_dir(&dir.0).unwrap().count();
        let error = failure(bounce_delivery::prepare_destination(engine.session(), &destination));
        assert!(error.contains("cannot resolve current video locator safely"), "{error}");
        assert!(!error.contains(&locator));
        assert!(!error.contains("private-"));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), entries);
        before.unchanged(&engine);
    }
}

#[cfg(windows)]
#[test]
fn unsupported_windows_video_names_refuse_generically_before_staging() {
    let dir = Directory::new();
    for locator in [r"C:private.mov", r"\private.mov", r"\\?\C:\private.mov", "private.mov:stream", "NUL.mov", "private.mov."] {
        video_refusal(video_session(Some(dir.0.clone()), locator.into()), &dir.path("output.wav"), "cannot resolve current video locator safely");
    }
}

#[cfg(unix)]
#[test]
fn native_video_folder_bytes_preserve_unrelated_delivery_and_destination_policy() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    let dir = Directory::new();
    let native = dir.0.join(OsString::from_vec(b"native-\xff".to_vec()));
    std::fs::create_dir(&native).unwrap();
    let alias = dir.path("utf8-alias");
    symlink(&native, &alias).unwrap();
    // The destination entry remains UTF-8; its canonical parent has native bytes. Existing
    // destination policy refuses that canonical path, so use a separate UTF-8 parent alias
    // in the session folder and compare a native video base against an unrelated UTF-8 output.
    let destination = dir.path("output.wav");
    let session = video_session(Some(native), "Movie.wav".into());
    let plan = bounce_delivery::prepare_destination(&session, &destination).unwrap();
    assert_eq!(plan.destination(), destination.as_path());
    assert!(bounce_delivery::prepare_destination(&session, &alias.join("movie.wav")).is_err());
}

#[test]
fn unrelated_existing_file_and_directory_are_preserved_before_staging() {
    let dir = Directory::new();
    let file = dir.path("foreign.wav");
    let directory = dir.path("folder.wav");
    std::fs::write(&file, b"foreign destination bytes").unwrap();
    std::fs::create_dir(&directory).unwrap();
    let mut engine = engine();
    let before = State::capture(&engine);
    for path in [&file, &directory] {
        let error = failure(bounce_delivery::prepare_destination(engine.session(), path));
        assert!(error.contains("exists"), "{error}");
        before.unchanged(&engine);
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"foreign destination bytes");
    assert!(directory.is_dir());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn late_collision_preserves_foreign_bytes_and_reports_unowned_paths() {
    let dir = Directory::new();
    let path = dir.path("late.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    let stage = bounce_delivery::stage(plan, &wire(48_000)).unwrap();
    let stage_path = stage.staging_path().to_owned();
    std::fs::write(&path, b"late external winner").unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("exists") || error.contains("collision"), "{error}");
    assert!(error.contains(&path.to_string_lossy().to_string()), "{error}");
    assert!(error.contains("retained paths"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"late external winner");
    if cfg!(unix) {
        assert!(!stage_path.exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    } else {
        assert_eq!(std::fs::read(&stage_path).unwrap(), wire(48_000));
        assert!(error.contains(&stage_path.to_string_lossy().to_string()));
    }
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn changed_staging_content_refuses_delivery_before_creating_final() {
    let dir = Directory::new();
    let path = dir.path("changed.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    let bytes = wire(48_000);
    let stage = bounce_delivery::stage(plan, &bytes).unwrap();
    let stage_path = stage.staging_path().to_owned();
    let mut changed = bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    std::fs::write(&stage_path, &changed).unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("content changed") || error.contains("differ"), "{error}");
    assert!(!path.exists());
    if cfg!(unix) {
        assert!(!stage_path.exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    } else {
        assert_eq!(std::fs::read(&stage_path).unwrap(), changed);
        assert!(error.contains(&stage_path.to_string_lossy().to_string()));
    }
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn explicit_discard_cleans_only_proven_files_and_drop_performs_no_filesystem_io() {
    let dir = Directory::new();
    let mut engine = engine();
    let before = State::capture(&engine);
    let path = dir.path("discard.wav");
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    let stage = bounce_delivery::stage(plan, &wire(48_000)).unwrap();
    let stage_path = stage.staging_path().to_owned();
    let retained = bounce_delivery::discard(stage).unwrap();
    assert_stage_cleanup(&stage_path, &retained);
    assert!(!path.exists());
    let path = dir.path("drop.wav");
    let stage = bounce_delivery::stage(bounce_delivery::prepare_destination(engine.session(), &path).unwrap(), &wire(48_000)).unwrap();
    let stage_path = stage.staging_path().to_owned();
    drop(stage);
    assert_eq!(std::fs::read(&stage_path).unwrap(), wire(48_000), "Drop cannot delete files");
    assert!(!path.exists());
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn malformed_destination_and_empty_stage_refuse_without_document_or_artifact_changes() {
    let dir = Directory::new();
    let mut engine = engine();
    let before = State::capture(&engine);
    for path in [PathBuf::new(), dir.path("missing/out.wav"), PathBuf::from("bad\0name.wav"), dir.path(&"x".repeat(4097))] {
        assert!(!failure(bounce_delivery::prepare_destination(engine.session(), &path)).is_empty());
        before.unchanged(&engine);
    }
    let path = dir.path("empty.wav");
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    let error = failure(bounce_delivery::stage(plan, &[]));
    assert!(error.contains("empty"), "{error}");
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn actual_mix_command_delivers_exact_selected_pcm_and_refuses_second_delivery_with_redo_intact() {
    for bus_output in [false, true] {
        let dir = Directory::new();
        let mut engine = engine();
        if bus_output {
            let bus = engine.session_mut().add_bus("Safety stem", ChannelFormat::Stereo);
            engine.session_mut().tracks[0].mixer.output = Route::Bus(bus);
            engine = seeded(engine);
        }
        let before = State::capture(&engine);
        let path = dir.path("mix.wav");
        let params = json!({"path":path,"start":13,"end":82,"format":"wav","bit_depth":"32f","dither":false,"normalize":false,"source":if bus_output { "Safety stem" } else { "main" }});
        let receipt = engine.execute("file.bounce_mix", &params).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_float_wave(&bytes, 48_000, 13, 82);
        assert!(!chunks(&bytes).iter().any(|(name, _)| name == b"bext"), "command must not invent original BWF metadata");
        assert_eq!(receipt["bytes"], bytes.len());
        assert_eq!(receipt["sample_rate"], 48_000);
        assert_eq!(receipt["frames"], 69);
        assert_eq!(receipt["seconds"].as_f64().unwrap(), 69.0 / 48_000.0);
        before.unchanged(&engine);
        let error = failure(engine.execute("file.bounce_mix", &params));
        assert!(error.contains("exists"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[cfg(unix)]
#[test]
fn existing_hardlink_symlink_and_dangling_symlink_never_overwrite_or_follow_targets() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let dir = Directory::new();
    let source = dir.path("original.wav");
    let hard = dir.path("hard.wav");
    let link = dir.path("symbolic.wav");
    let dangling = dir.path("dangling.wav");
    let missing = dir.path("missing-original.wav");
    std::fs::write(&source, wire(48_000)).unwrap();
    std::fs::hard_link(&source, &hard).unwrap();
    symlink(&source, &link).unwrap();
    symlink(&missing, &dangling).unwrap();
    let mut engine = engine();
    let before = State::capture(&engine);
    let identity = std::fs::metadata(&source).unwrap();
    for path in [&source, &hard, &link, &dangling] {
        let error = failure(bounce_delivery::prepare_destination(engine.session(), path));
        assert!(error.contains("exists"), "{error}");
        before.unchanged(&engine);
    }
    assert_eq!(std::fs::read(&source).unwrap(), wire(48_000));
    assert_eq!(std::fs::read(&hard).unwrap(), wire(48_000));
    assert_eq!(std::fs::read(&link).unwrap(), wire(48_000));
    assert_eq!(std::fs::read_link(&dangling).unwrap(), missing);
    assert!(!missing.exists());
    let after = std::fs::metadata(&source).unwrap();
    assert_eq!((after.dev(), after.ino()), (identity.dev(), identity.ino()));
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 4);
    before.unchanged_and_redo_works(&mut engine);
}

#[cfg(unix)]
#[test]
fn symlink_parent_aliases_cannot_replace_missing_original_or_persisted_pcm_locators() {
    use std::os::unix::fs::symlink;
    for policy in [ImportPolicy::Copy, ImportPolicy::Reference, ImportPolicy::Convert] {
        let dir = Directory::new();
        let session = protected_session(&dir, policy);
        let folder = session.folder.clone().unwrap();
        let alias = dir.path("alias");
        symlink(&folder, &alias).unwrap();
        let mut engine = seeded(Engine::new(session));
        let before = State::capture(&engine);
        for relative in ["Original Audio/original.wav", "Audio Files/recorded.wav"] {
            let path = alias.join(relative);
            let error = failure(bounce_delivery::prepare_destination(engine.session(), &path));
            assert!(error.contains("recorded media locator"), "{error}");
            assert!(!path.exists());
            before.unchanged(&engine);
        }
        assert_eq!(std::fs::read_link(&alias).unwrap(), folder);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[cfg(unix)]
#[test]
fn late_symlink_collision_preserves_link_and_external_target_bytes() {
    use std::os::unix::fs::symlink;
    let dir = Directory::new();
    let path = dir.path("late-link.wav");
    let external = dir.path("external.wav");
    std::fs::write(&external, b"foreign target remains").unwrap();
    let mut engine = engine();
    let before = State::capture(&engine);
    let stage = bounce_delivery::stage(bounce_delivery::prepare_destination(engine.session(), &path).unwrap(), &wire(48_000)).unwrap();
    let stage_path = stage.staging_path().to_owned();
    symlink(&external, &path).unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("exists") || error.contains("collision"), "{error}");
    assert!(error.contains(&path.to_string_lossy().to_string()));
    assert_eq!(std::fs::read_link(&path).unwrap(), external);
    assert_eq!(std::fs::read(&external).unwrap(), b"foreign target remains");
    assert!(!stage_path.exists());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    before.unchanged_and_redo_works(&mut engine);
}

#[cfg(unix)]
#[test]
fn replaced_staging_inode_is_preserved_and_reported_with_no_final_delivery() {
    let dir = Directory::new();
    let path = dir.path("replace.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let stage = bounce_delivery::stage(bounce_delivery::prepare_destination(engine.session(), &path).unwrap(), &wire(48_000)).unwrap();
    let stage_path = stage.staging_path().to_owned();
    let replacement = dir.path("replacement");
    std::fs::write(&replacement, b"foreign staging replacement").unwrap();
    std::fs::rename(&replacement, &stage_path).unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("identity changed"), "{error}");
    assert!(error.contains(&stage_path.to_string_lossy().to_string()), "{error}");
    assert!(!path.exists());
    assert_eq!(std::fs::read(&stage_path).unwrap(), b"foreign staging replacement");
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    before.unchanged_and_redo_works(&mut engine);
}

#[cfg(unix)]
#[test]
fn changed_parent_between_prepare_and_stage_refuses_before_creating_any_artifact() {
    let dir = Directory::new();
    let parent = dir.path("delivery");
    let moved = dir.path("moved-delivery");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("out.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let plan = bounce_delivery::prepare_destination(engine.session(), &path).unwrap();
    std::fs::rename(&parent, &moved).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("foreign"), b"new directory occupant").unwrap();
    let error = failure(bounce_delivery::stage(plan, &wire(48_000)));
    assert!(error.contains("directory identity changed"), "{error}");
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
    assert_eq!(std::fs::read(parent.join("foreign")).unwrap(), b"new directory occupant");
    before.unchanged_and_redo_works(&mut engine);
}

#[cfg(unix)]
#[test]
fn changed_parent_after_stage_preserves_moved_artifact_and_reports_recorded_cleanup_path() {
    let dir = Directory::new();
    let parent = dir.path("delivery");
    let moved = dir.path("moved-delivery");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("out.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let stage = bounce_delivery::stage(bounce_delivery::prepare_destination(engine.session(), &path).unwrap(), &wire(48_000)).unwrap();
    let recorded_stage = stage.staging_path().to_owned();
    let moved_stage = moved.join(recorded_stage.file_name().unwrap());
    std::fs::rename(&parent, &moved).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("foreign"), b"replacement directory occupant").unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("directory identity changed"), "{error}");
    assert!(error.contains(&recorded_stage.to_string_lossy().to_string()), "unproved recorded locator must be reported: {error}");
    assert!(!path.exists());
    assert!(!recorded_stage.exists());
    assert_eq!(std::fs::read(&moved_stage).unwrap(), wire(48_000));
    assert_eq!(std::fs::read(parent.join("foreign")).unwrap(), b"replacement directory occupant");
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    before.unchanged_and_redo_works(&mut engine);
}

#[cfg(unix)]
#[test]
fn changed_parent_to_symlink_cannot_publish_into_a_foreign_directory() {
    use std::os::unix::fs::symlink;
    let dir = Directory::new();
    let parent = dir.path("delivery");
    let moved = dir.path("moved-delivery");
    let foreign = dir.path("foreign-directory");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("out.wav"), b"foreign destination through alias").unwrap();
    let path = parent.join("out.wav");
    let mut engine = engine();
    let before = State::capture(&engine);
    let stage = bounce_delivery::stage(bounce_delivery::prepare_destination(engine.session(), &path).unwrap(), &wire(48_000)).unwrap();
    let recorded_stage = stage.staging_path().to_owned();
    let moved_stage = moved.join(recorded_stage.file_name().unwrap());
    std::fs::rename(&parent, &moved).unwrap();
    symlink(&foreign, &parent).unwrap();
    let error = failure(bounce_delivery::publish(stage));
    assert!(error.contains("directory changed"), "{error}");
    assert!(error.contains(&recorded_stage.to_string_lossy().to_string()), "{error}");
    assert_eq!(std::fs::read_link(&parent).unwrap(), foreign);
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign destination through alias");
    assert_eq!(std::fs::read(&moved_stage).unwrap(), wire(48_000));
    assert_eq!(std::fs::read_dir(&foreign).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    before.unchanged_and_redo_works(&mut engine);
}

#[test]
fn direct_bwf_render_and_delivery_preserve_literal_metadata_and_selected_pcm_bytes() {
    for bus_output in [false, true] {
        let dir = Directory::new();
        let mut engine = engine();
        let bus = if bus_output {
            let bus = engine.session_mut().add_bus("BWF safety stem", ChannelFormat::Stereo);
            engine.session_mut().tracks[0].mixer.output = Route::Bus(bus);
            engine = seeded(engine);
            Some(bus)
        } else {
            None
        };
        let before = State::capture(&engine);
        let path = dir.path("explicit-bwf.wav");
        let request = io::BounceRequest {
            range: soundcraft_time::Range::new(13, 82),
            tail_samples: 0,
            preroll_start: 0,
            sample_rate: soundcraft_time::SampleRate::HZ_48000,
            bus,
            fold_stereo: false,
            normalize: false,
            encode: soundcraft_audio_io::EncodeOptions {
                format: FileFormat::Wav,
                bit_depth: soundcraft_audio_io::BitDepth::Float32,
                dither: false,
                bwf: Some(bwf()),
            },
        };
        let plan = io::plan_bounce(engine.session(), &request).unwrap();
        assert_eq!(plan.frames, 69);
        assert_eq!(plan.session_frames, 69);
        assert_eq!(plan.channels, 2);
        assert_eq!(plan.processing_frames, 82);
        assert_eq!(plan.sample_rate.hz(), 48_000);
        before.unchanged(&engine);
        assert!(!path.exists());
        let (output, receipt) = io::deliver_bounce(&engine, &path, &request).unwrap();
        assert_eq!(output.frames, 69);
        assert_eq!(output.channels, 2);
        assert_eq!(output.sample_rate.hz(), 48_000);
        assert_eq!(output.input_range, soundcraft_time::Range::new(13, 82));
        assert_eq!(output.tail_samples, 0);
        assert_eq!(output.preroll_start, 0);
        assert!(receipt.complete);
        assert_eq!(receipt.bytes, output.bytes.len() as u64);
        assert_eq!(receipt.sha256, sha(&output.bytes));
        assert_eq!(Path::new(&receipt.output_path), path.canonicalize().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), output.bytes);
        assert_float_wave(&output.bytes, 48_000, 13, 82);

        // Compare the complete 602-byte bext against independently declared wire metadata,
        // including literal fixed fields, >32-bit timestamp, version and zero reserved bytes.
        let expected_wire = wire(48_000);
        assert_bwf(&expected_wire);
        let expected_chunks = chunks(&expected_wire);
        let actual_chunks = chunks(&output.bytes);
        let bext = payload(&actual_chunks, b"bext");
        assert_eq!(bext, payload(&expected_chunks, b"bext"));
        assert_eq!(u64::from_le_bytes(bext[338..346].try_into().unwrap()), TIMESTAMP);
        assert_eq!(payload(&actual_chunks, b"data").len(), 69 * 2 * 4);
        before.unchanged(&engine);
        let error = failure(io::deliver_bounce(&engine, &path, &request));
        assert!(error.contains("exists"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), output.bytes);
        before.unchanged_and_redo_works(&mut engine);
    }
}

#[test]
fn typed_int32_flac_wrappers_preserve_clipped_literal_pcm_and_seeded_document_state() {
    let mut engine = engine();
    let before = State::capture(&engine);
    let range = soundcraft_time::Range::new(13, 82);
    let options =
        soundcraft_audio_io::EncodeOptions { format: FileFormat::Flac, bit_depth: soundcraft_audio_io::BitDepth::Int32, dither: false, bwf: None };
    let legacy = io::bounce_bytes(&engine, range, &options, false).unwrap();
    before.unchanged(&engine);
    let explicit = io::bounce_source_at_rate(&engine, range, &options, false, false, None, soundcraft_time::SampleRate::HZ_48000).unwrap();
    before.unchanged(&engine);
    assert_eq!(legacy.0, explicit.bytes);
    assert_eq!(explicit.frames, 69);
    assert_eq!(explicit.channels, 2);
    assert_eq!(explicit.sample_rate.hz(), 48_000);
    assert_eq!(explicit.input_range, range);
    assert_eq!(explicit.tail_samples, 0);
    assert_eq!(explicit.preroll_start, 13);
    assert!(!explicit.bytes.is_empty());

    // Independently read FLAC's mandatory first STREAMINFO block: its packed
    // rate/channel/depth/frame fields cannot be supplied by the production decoder.
    let bytes = &explicit.bytes;
    assert!(bytes.len() >= 42);
    assert_eq!(&bytes[..4], b"fLaC");
    assert_eq!(bytes[4] & 0x7f, 0);
    assert_eq!(u32::from_be_bytes([0, bytes[5], bytes[6], bytes[7]]), 34);
    let stream = u64::from_be_bytes(bytes[18..26].try_into().unwrap());
    assert_eq!((stream >> 44) & 0xfffff, 48_000);
    assert_eq!(((stream >> 41) & 7) + 1, 2);
    assert_eq!(((stream >> 36) & 31) + 1, 32);
    assert_eq!(stream & ((1_u64 << 36) - 1), 69);

    let (info, decoded) = soundcraft_audio_io::decode(bytes, Some("flac")).unwrap();
    assert_eq!(info.format, FileFormat::Flac);
    assert_eq!(info.sample_format, SampleFormat::Int32);
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.frames, 69);
    assert_eq!(info.channels, 2);
    assert_eq!(decoded.sample_rate, 48_000);
    assert_eq!(decoded.num_channels(), 2);
    assert_eq!(decoded.frames(), 69);
    const LEFT: [f32; 4] = [0.25, -0.125, 1.0, -0.5];
    const RIGHT: [f32; 4] = [0.5, 0.125, -1.0, -0.25];
    for (channel, samples) in decoded.channels.iter().enumerate() {
        assert_eq!(samples.len(), 69);
        for (frame, actual) in samples.iter().copied().enumerate() {
            let expected = [LEFT, RIGHT][channel][(13 + frame) % 4];
            assert!(actual.is_finite());
            assert_eq!(actual.to_bits(), expected.to_bits(), "channel {channel}, frame {frame}");
        }
    }
    assert!(decoded.channels.iter().flatten().any(|sample| sample.abs() > 0.02));
    assert_eq!(decoded.channels[0][1], 1.0);
    assert_eq!(decoded.channels[1][1], -1.0);
    assert_ne!(decoded.channels[0][1].to_bits(), 1.25_f32.to_bits());
    assert_ne!(decoded.channels[1][1].to_bits(), (-2.5_f32).to_bits());
    before.unchanged_and_redo_works(&mut engine);
}
