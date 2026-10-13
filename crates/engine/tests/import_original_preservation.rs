//! Independent encoded-original fixtures and ED-31 preservation acceptance.
//! Fixture construction deliberately does not use the production audio encoder/resampler.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use soundcraft_audio_io::AudioBuffer;
use soundcraft_engine::{
    Engine, io,
    original_audio::{ImportOptions, apply_audio_import, prepare_audio_import},
    session_copy,
};
use soundcraft_model::{ImportPolicy, Session, Source, SourceId};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const TARGET_RATE: u32 = 48_000;
const TIME_REFERENCE: u64 = (1u64 << 40) + 12_345;
const OPAQUE: &[u8] = b"SC-ED31\0opaque\xff\x81\x00!?";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("soundcraft-import-original-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
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

struct Wave {
    rate: u32,
    timestamp: u64,
    bytes: Vec<u8>,
    pcm: Vec<Vec<f32>>,
}
fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}
fn field(body: &mut [u8], start: usize, text: &[u8]) {
    body[start..start + text.len()].copy_from_slice(text);
}
fn wave(rate: u32, timestamp: u64) -> Wave {
    let frames = usize::try_from(rate / 10 + 7).unwrap();
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM, two channels, sixteen bits.
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * 4).to_le_bytes());
    fmt.extend_from_slice(&4u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    let mut bext = vec![0u8; 602];
    field(&mut bext, 0, b"SC-ED31 original fixture");
    field(&mut bext, 256, b"SoundCraft independent");
    field(&mut bext, 288, format!("SC-ED31-{rate}").as_bytes());
    field(&mut bext, 320, b"2026-10-12");
    field(&mut bext, 330, b"01:02:03");
    bext[338..346].copy_from_slice(&timestamp.to_le_bytes());
    bext[346..348].copy_from_slice(&1u16.to_le_bytes());
    let mut data = Vec::new();
    let mut pcm = vec![Vec::new(), Vec::new()];
    for frame in 0..frames {
        let right: i16 = if (frame / usize::try_from(rate / 100).unwrap()).is_multiple_of(2) { -12_288 } else { 4_096 };
        for (channel, value) in [8_192i16, right].into_iter().enumerate() {
            data.extend_from_slice(&value.to_le_bytes());
            pcm[channel].push(f32::from(value) / 32_768.0);
        }
    }
    let mut body = b"WAVE".to_vec();
    chunk(&mut body, b"fmt ", &fmt);
    chunk(&mut body, b"bext", &bext);
    chunk(&mut body, b"JvOr", OPAQUE);
    chunk(&mut body, b"data", &data);
    chunk(&mut body, b"tail", b"opaque trailing bytes");
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&body);
    if timestamp == TIME_REFERENCE {
        let expected = match rate {
            44_100 => "e6781a389031b79d60fe641da1dba12e4c945c83a1b2ace47d843a60193c0fc4",
            48_000 => "8dc3ed7299b6ba6b9f0615eb9b6b8d30523edafd7bed18987d3991275f3aac81",
            96_000 => "a4884b0111a30c01b3310b6941ebe43de6e06a15659d829abb8cdb8fac56f0d4",
            _ => panic!("undeclared fixture rate"),
        };
        assert_eq!(encoded_hash(&bytes), expected, "fixed independent RIFF/BWF fixture digest");
    }
    Wave { rate, timestamp, bytes, pcm }
}
fn encoded_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn write_wave(dir: &Directory, name: &str, wave: &Wave) -> PathBuf {
    let path = dir.path(name);
    std::fs::write(&path, &wave.bytes).unwrap();
    path
}
fn resolve(session: &Session, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { path.to_path_buf() } else { session.folder.as_ref().unwrap().join(path) }
}
fn imported_source<'a>(engine: &'a Engine, row: &Value) -> &'a Source {
    engine.session().source(SourceId(row["source"].as_u64().unwrap())).unwrap()
}
fn assert_original(session: &Session, source: &Source, fixture: &Wave, policy: ImportPolicy) {
    let original = source.original.as_ref().expect("imported original provenance must be explicit");
    assert_eq!(original.version, 1);
    assert_eq!(original.policy, policy);
    assert_eq!(original.bytes, fixture.bytes.len() as u64);
    assert_eq!(original.sha256, encoded_hash(&fixture.bytes));
    assert_eq!(original.info.sample_rate, fixture.rate);
    assert_eq!(original.info.format, soundcraft_audio_io::FileFormat::Wav);
    assert_eq!(original.info.sample_format, soundcraft_audio_io::SampleFormat::Int16);
    assert_eq!(original.info.channels, 2);
    assert_eq!(original.info.frames, fixture.pcm[0].len() as u64);
    let bwf = original.info.bwf.as_ref().unwrap();
    assert_eq!(bwf.time_reference, fixture.timestamp);
    assert_eq!(bwf.description, "SC-ED31 original fixture");
    assert_eq!(bwf.originator, "SoundCraft independent");
    assert_eq!(bwf.originator_reference, format!("SC-ED31-{}", fixture.rate));
    assert_eq!(bwf.origination_date, "2026-10-12");
    assert_eq!(bwf.origination_time, "01:02:03");
    let bytes = std::fs::read(resolve(session, &original.path)).unwrap();
    assert_eq!(bytes, fixture.bytes, "opaque chunks and original integer encoding survive byte-for-byte");
}
fn assert_pcm(buffer: &AudioBuffer, fixture: &Wave) {
    let frames = (fixture.pcm[0].len() as u64 * u64::from(TARGET_RATE) + u64::from(fixture.rate / 2)) / u64::from(fixture.rate);
    assert_eq!(buffer.sample_rate, TARGET_RATE);
    assert_eq!(buffer.channels.len(), 2);
    assert!(buffer.channels.iter().all(|channel| channel.len() as u64 == frames));
    assert!(buffer.channels.iter().flatten().all(|sample| sample.is_finite()));
    assert!(buffer.peak() > 0.3);
    if fixture.rate == TARGET_RATE {
        assert_eq!(buffer.channels, fixture.pcm, "same-rate PCM16 decoding is exact and unconverted");
    } else {
        // Independent piecewise-constant input law. Check each plateau well away from
        // file edges/step transitions; this is channel/SRC delivery proof, not an
        // arbitrary-codec or incumbent-quality claim. No production SRC is an oracle.
        for frame in 128..usize::try_from(frames).unwrap() - 128 {
            let within = frame % 480;
            if (128..352).contains(&within) {
                let expected_right = if (frame / 480).is_multiple_of(2) { -0.375 } else { 0.125 };
                assert!((buffer.channels[0][frame] - 0.25).abs() < 2e-5, "left plateau frame {frame}");
                assert!((buffer.channels[1][frame] - expected_right).abs() < 2e-5, "right plateau frame {frame}");
            }
        }
    }
}
fn assert_receipt(engine: &Engine, row: &Value, fixture: &Wave, policy: &str, at: i64) {
    let source = imported_source(engine, row);
    assert_eq!(row["policy"], policy);
    assert_eq!(row["original"]["sha256"], encoded_hash(&fixture.bytes));
    assert_eq!(row["original"]["bytes"], fixture.bytes.len() as u64);
    assert_eq!(row["original"]["sample_rate"], fixture.rate);
    assert_eq!(row["original"]["frames"], fixture.pcm[0].len() as u64);
    assert_eq!(row["original"]["channels"], 2);
    assert_eq!(row["original"]["time_reference"], fixture.timestamp);
    assert_eq!(row["derivative"]["sample_rate"], TARGET_RATE);
    assert_eq!(row["derivative"]["frames"], source.frames);
    assert_eq!(row["derivative"]["channels"], 2);
    assert_eq!(row["derivative"]["converted"], fixture.rate != TARGET_RATE);
    assert_eq!(row["derivative"]["audio_identity"], source.audio_identity.as_deref().unwrap());
    assert_eq!(row["at"], at);
    let track = engine.session().track(soundcraft_model::TrackId(row["track"].as_u64().unwrap())).unwrap();
    let clip = track.playlist().unwrap().clips.iter().find(|clip| clip.id.0 == row["clip"].as_u64().unwrap()).unwrap();
    assert_eq!(clip.start, at);
    assert_eq!(clip.length as u64, source.frames);
    assert_pcm(&engine.session().pool.get(source.id).unwrap().buffer, fixture);
    let rendered = soundcraft_mix::render_clips(engine.session(), track.id, soundcraft_time::Range::new(at, at + clip.length));
    assert_pcm(&AudioBuffer { sample_rate: TARGET_RATE, channels: rendered }, fixture);
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
    }
}
fn fresh_engine(dir: &Directory) -> Engine {
    let mut engine = Engine::default();
    engine.configure_original_store(dir.path(&format!("store-{}", NEXT.fetch_add(1, Ordering::Relaxed))), 8 << 20).unwrap();
    engine
}
fn seeded_redo_unconfigured() -> Engine {
    let mut engine = Engine::default();
    engine.execute("track.new", &json!({"name":"Seeded Redo"})).unwrap();
    assert!(engine.undo());
    assert_eq!(engine.redo_label(), Some("New..."));
    engine
}
fn seeded_redo(dir: &Directory) -> Engine {
    let mut engine = seeded_redo_unconfigured();
    engine.configure_original_store(dir.path(&format!("store-{}", NEXT.fetch_add(1, Ordering::Relaxed))), 8 << 20).unwrap();
    engine
}

#[test]
fn every_policy_preserves_mixed_rate_bwf_bytes_and_distinguishes_playback_receipts() {
    for (policy, model_policy) in [("copy", ImportPolicy::Copy), ("reference", ImportPolicy::Reference), ("convert", ImportPolicy::Convert)] {
        let dir = Directory::new();
        let fixtures: Vec<_> = [44_100, 48_000, 96_000].into_iter().map(|rate| wave(rate, TIME_REFERENCE)).collect();
        let paths: Vec<_> = fixtures.iter().map(|fixture| write_wave(&dir, &format!("{}.wav", fixture.rate), fixture)).collect();
        let mut engine = fresh_engine(&dir);
        let before = State::capture(&engine);
        let receipt = engine.execute("file.import_audio", &json!({"paths":paths,"policy":policy,"rate_conversion":true,"at":31})).unwrap();
        assert_eq!(receipt["policy"], policy);
        assert_eq!(receipt["rate_conversion"], true);
        let rows = receipt["imported"].as_array().unwrap();
        assert_eq!(rows.len(), fixtures.len());
        assert_eq!(engine.session().sources.len(), 3);
        assert_eq!(engine.undo_history().len(), before.undo.len() + 1, "one accepted batch is one Undo entry");
        assert_eq!(engine.journal.len(), before.journal.len() + 1);
        for ((row, fixture), path) in rows.iter().zip(&fixtures).zip(&paths) {
            assert_receipt(&engine, row, fixture, policy, 31);
            assert_original(engine.session(), imported_source(&engine, row), fixture, model_policy);
            assert_eq!(std::fs::read(path).unwrap(), fixture.bytes, "import does not alter input bytes");
        }
        let accepted = engine.session_arc();
        assert!(engine.undo());
        assert!(Arc::ptr_eq(&before.session, &engine.session_arc()));
        assert!(engine.redo());
        assert!(Arc::ptr_eq(&accepted, &engine.session_arc()));
        for (row, fixture) in rows.iter().zip(&fixtures) {
            assert_original(engine.session(), imported_source(&engine, row), fixture, model_policy);
        }
    }
}

#[test]
fn default_copy_same_rate_is_exact_and_mismatch_requires_explicit_conversion() {
    let dir = Directory::new();
    let same = wave(48_000, TIME_REFERENCE);
    let same_path = write_wave(&dir, "same.wav", &same);
    let mut engine = fresh_engine(&dir);
    let receipt = engine.execute("file.import_audio", &json!({"path":same_path})).unwrap();
    assert_eq!(receipt["policy"], "copy");
    assert_eq!(receipt["rate_conversion"], false);
    assert_receipt(&engine, &receipt["imported"][0], &same, "copy", 0);
    for policy in ["copy", "reference"] {
        let different = wave(44_100, TIME_REFERENCE);
        let path = write_wave(&dir, &format!("different-{policy}.wav"), &different);
        let mut engine = seeded_redo(&dir);
        let before = State::capture(&engine);
        assert!(engine.execute("file.import_audio", &json!({"path":path,"policy":policy})).is_err());
        before.unchanged(&engine);
        assert!(engine.redo());
        assert!(engine.session().track_by_name("Seeded Redo").is_some());
    }
}

#[test]
fn mixed_invalid_io_and_hostile_placement_fail_before_any_document_mutation() {
    let dir = Directory::new();
    let valid = write_wave(&dir, "valid.wav", &wave(48_000, TIME_REFERENCE));
    let invalid = dir.path("invalid.wav");
    std::fs::write(&invalid, b"not a valid audio container").unwrap();
    let folder = dir.path("directory.wav");
    std::fs::create_dir(&folder).unwrap();
    let hostile_timestamp = write_wave(&dir, "timestamp.wav", &wave(44_100, u64::MAX));
    let cases = [
        json!({"paths":[valid,invalid],"policy":"copy"}),
        json!({"paths":[valid,dir.path("missing.wav")],"policy":"convert"}),
        json!({"paths":[valid,folder],"policy":"reference"}),
        json!({"path":valid,"policy":"unknown"}),
        json!({"path":valid,"policy":"copy","at":i64::MAX}),
        json!({"path":hostile_timestamp,"policy":"convert","use_timestamp":true}),
    ];
    for params in cases {
        let mut engine = seeded_redo(&dir);
        let before = State::capture(&engine);
        assert!(engine.execute("file.import_audio", &params).is_err(), "{params}");
        before.unchanged(&engine);
        assert!(engine.redo());
        assert!(engine.session().track_by_name("Seeded Redo").is_some());
    }
}

#[test]
fn owned_originals_survive_external_change_and_native_reopen() {
    for (policy, model_policy) in [("copy", ImportPolicy::Copy), ("convert", ImportPolicy::Convert)] {
        let dir = Directory::new();
        let fixture = wave(44_100, TIME_REFERENCE);
        let external = write_wave(&dir, "external.wav", &fixture);
        let mut engine = fresh_engine(&dir);
        let receipt = engine.execute("file.import_audio", &json!({"path":external,"policy":policy,"rate_conversion":true})).unwrap();
        let id = imported_source(&engine, &receipt["imported"][0]).id;
        let playback = engine.session().pool.get(id).unwrap().buffer.clone();
        std::fs::write(&external, b"external original was replaced after import").unwrap();
        assert_original(engine.session(), engine.session().source(id).unwrap(), &fixture, model_policy);
        let target = dir.path("native/Song.scraft");
        engine.execute("session.save_as", &json!({"path":target})).unwrap();
        std::fs::remove_file(&external).unwrap();
        let mut reopened = fresh_engine(&dir);
        assert!(io::open_session(&mut reopened, target.to_str().unwrap()).unwrap().is_empty());
        let source = reopened.session().source(id).unwrap();
        assert_original(reopened.session(), source, &fixture, model_policy);
        assert_pcm(&reopened.session().pool.get(id).unwrap().buffer, &fixture);
        assert_eq!(
            reopened.session().pool.get(id).unwrap().buffer,
            playback,
            "native derivative preserves every decoded PCM bit, including SRC edges"
        );
        let original_path = resolve(reopened.session(), &source.original.as_ref().unwrap().path);
        let derivative_path = resolve(reopened.session(), &source.path);
        assert_ne!(original_path, derivative_path);
        assert!(source.original.as_ref().unwrap().path.starts_with("Original Audio/"));
        assert!(source.path.starts_with("Audio Files/"));
        assert_ne!(std::fs::read(derivative_path).unwrap(), fixture.bytes);
    }
}

#[test]
fn portable_folder_and_bundle_materialize_every_original_separately_from_derivatives() {
    for (policy, model_policy) in [("copy", ImportPolicy::Copy), ("reference", ImportPolicy::Reference), ("convert", ImportPolicy::Convert)] {
        let dir = Directory::new();
        let fixture = wave(96_000, TIME_REFERENCE);
        let external = write_wave(&dir, "external.wav", &fixture);
        let mut engine = fresh_engine(&dir);
        let receipt = engine.execute("file.import_audio", &json!({"path":external,"policy":policy,"rate_conversion":true})).unwrap();
        let id = imported_source(&engine, &receipt["imported"][0]).id;
        let playback = engine.session().pool.get(id).unwrap().buffer.clone();
        let before = State::capture(&engine);
        let target = dir.path("portable/Copy.scraft");
        assert_eq!(engine.execute("session.save_copy", &json!({"path":target})).unwrap()["audio_complete"], true);
        let (bundle, result) = session_copy::bundle(&engine, "Copy.scraftz").unwrap();
        assert!(result.audio_complete);
        before.unchanged(&engine);
        std::fs::remove_file(&external).unwrap();
        std::fs::rename(dir.path("portable"), dir.path("moved")).unwrap();
        let mut folder = fresh_engine(&dir);
        assert!(io::open_session(&mut folder, dir.path("moved/Copy.scraft").to_str().unwrap()).unwrap().is_empty());
        let source = folder.session().source(id).unwrap();
        assert_original(folder.session(), source, &fixture, model_policy);
        assert_pcm(&folder.session().pool.get(id).unwrap().buffer, &fixture);
        assert_eq!(folder.session().pool.get(id).unwrap().buffer, playback);
        assert_ne!(source.original.as_ref().unwrap().path, source.path);
        let mut bundled = fresh_engine(&dir);
        assert!(io::open_session_bytes(&mut bundled, &result.output_path, &bundle).unwrap().is_empty());
        let source = bundled.session().source(id).unwrap();
        let original = source.original.as_ref().unwrap();
        assert_eq!(original.sha256, encoded_hash(&fixture.bytes));
        assert_eq!(original.info.sample_rate, fixture.rate);
        assert_eq!(original.info.bwf.as_ref().unwrap().time_reference, fixture.timestamp);
        assert_ne!(original.path, source.path);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bundle)).unwrap();
        use std::io::Read as _;
        let mut encoded_original = Vec::new();
        let original_entries: Vec<_> =
            archive.file_names().map(|name| name.unwrap().into_owned()).filter(|name| name.starts_with("Original Audio/")).collect();
        assert_eq!(original_entries.len(), 1);
        archive.by_name(&original_entries[0]).unwrap().read_to_end(&mut encoded_original).unwrap();
        assert_eq!(encoded_original, fixture.bytes);
        assert_original(bundled.session(), source, &fixture, model_policy);
        assert_pcm(&bundled.session().pool.get(id).unwrap().buffer, &fixture);
        assert_eq!(bundled.session().pool.get(id).unwrap().buffer, playback);
    }
}

#[test]
fn reference_tampering_blocks_portable_output_atomically_but_save_preserves_recovery_metadata() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let external = write_wave(&dir, "reference.wav", &fixture);
    let mut engine = fresh_engine(&dir);
    engine.execute("file.import_audio", &json!({"path":external,"policy":"reference"})).unwrap();
    engine.execute("track.new", &json!({"name":"Seeded Redo"})).unwrap();
    assert!(engine.undo());
    let before = State::capture(&engine);
    let mut changed = fixture.bytes.clone();
    changed[100] ^= 1; // Metadata-only tampering: playback PCM can still be identical.
    std::fs::write(&external, changed).unwrap();
    assert_eq!(engine.execute("session.save_copy_preview", &json!({"path":dir.path("copy/Copy.scraft")})).unwrap()["ready"], false);
    let path = dir.path("copy/Copy.scraft");
    assert!(engine.execute("session.save_copy", &json!({"path":path})).is_err());
    before.unchanged(&engine);
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists(), "original validation precedes portable destination media writes");
    assert!(session_copy::bundle(&engine, "Copy.scraftz").is_err());
    before.unchanged(&engine);
    assert!(engine.redo());
    assert!(engine.session().track_by_name("Seeded Redo").is_some());
    let original = engine.session().sources[0].original.clone();
    let native = dir.path("native/Song.scraft");
    engine.execute("session.save_as", &json!({"path":native})).unwrap();
    assert_eq!(engine.session().sources[0].original, original, "ordinary Save retains unresolved original provenance");
    assert!(engine.messages.iter().any(|message| message.contains("original")), "ordinary Save reports a distinct original-media warning");
    let mut reopened = fresh_engine(&dir);
    let issues = io::open_session(&mut reopened, native.to_str().unwrap()).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("original"));
    assert_eq!(reopened.session().sources[0].original, original);
    assert_pcm(&reopened.session().pool.get(reopened.session().sources[0].id).unwrap().buffer, &fixture);
}

#[test]
fn legacy_source_save_open_never_fabricates_encoded_original_provenance() {
    let dir = Directory::new();
    let mut engine = fresh_engine(&dir);
    let id = io::add_source(
        engine.session_mut(),
        "Legacy",
        AudioBuffer { sample_rate: TARGET_RATE, channels: vec![vec![0.25; 37]] },
        None,
        soundcraft_audio_io::FileFormat::Wav,
    );
    assert!(engine.session().source(id).unwrap().original.is_none());
    let target = dir.path("legacy/Song.scraft");
    engine.execute("session.save_as", &json!({"path":target})).unwrap();
    let mut reopened = fresh_engine(&dir);
    assert!(io::open_session(&mut reopened, target.to_str().unwrap()).unwrap().is_empty());
    assert!(reopened.session().source(id).unwrap().original.is_none());
    assert_eq!(reopened.session().pool.get(id).unwrap().buffer.channels, vec![vec![0.25; 37]]);
    let (bundle, result) = session_copy::bundle(&reopened, "Legacy.scraftz").unwrap();
    let mut bundled = fresh_engine(&dir);
    assert!(io::open_session_bytes(&mut bundled, &result.output_path, &bundle).unwrap().is_empty());
    assert!(bundled.session().source(id).unwrap().original.is_none());
}

#[test]
fn original_timestamp_uses_original_rate_checked_rounding_and_session_start() {
    for rate in [44_100, 48_000, 96_000] {
        let dir = Directory::new();
        let fixture = wave(rate, TIME_REFERENCE);
        let path = write_wave(&dir, "timestamp.wav", &fixture);
        let mut engine = fresh_engine(&dir);
        engine.session_mut().timecode_start = 37;
        let expected = i64::try_from((u128::from(TIME_REFERENCE) * u128::from(TARGET_RATE) + u128::from(rate / 2)) / u128::from(rate)).unwrap() - 37;
        let receipt = engine.execute("file.import_audio", &json!({"path":path,"policy":"convert","use_timestamp":true})).unwrap();
        assert_receipt(&engine, &receipt["imported"][0], &fixture, "convert", expected);
        let source = imported_source(&engine, &receipt["imported"][0]);
        assert_eq!(source.time_reference, TIME_REFERENCE, "raw original timestamp remains lossless");
        assert_eq!(source.original.as_ref().unwrap().timeline_time_reference(TARGET_RATE).unwrap(), Some(expected + 37));
        assert_eq!(std::fs::read(path).unwrap(), fixture.bytes);
    }
    let dir = Directory::new();
    let path = write_wave(&dir, "negative.wav", &wave(48_000, 1));
    let mut engine = seeded_redo(&dir);
    engine.session_mut().timecode_start = 2;
    let before = State::capture(&engine);
    assert!(engine.execute("file.import_audio", &json!({"path":path,"policy":"copy","use_timestamp":true})).is_err());
    before.unchanged(&engine);
    assert!(engine.redo());
}

#[test]
fn cancellation_capacity_and_failed_store_admission_keep_seeded_redo_and_document() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    for options in [
        ImportOptions { cancel: Arc::new(AtomicBool::new(true)), ..ImportOptions::default() },
        ImportOptions { max_memory_bytes: 1, ..ImportOptions::default() },
        ImportOptions { max_original_bytes: 1, ..ImportOptions::default() },
    ] {
        let mut engine = seeded_redo(&dir);
        let before = State::capture(&engine);
        assert!(prepare_audio_import(&mut engine, std::slice::from_ref(&path), options).is_err());
        before.unchanged(&engine);
        engine.collect_original_assets().unwrap();
        before.unchanged(&engine);
        assert!(engine.redo());
        assert!(engine.session().track_by_name("Seeded Redo").is_some());
    }
    let mut engine = seeded_redo_unconfigured();
    engine.configure_original_store(dir.path("limited-store"), fixture.bytes.len() as u64 - 1).unwrap();
    let before = State::capture(&engine);
    assert!(prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions::default()).is_err());
    before.unchanged(&engine);
    engine.collect_original_assets().unwrap();
    before.unchanged(&engine);
    assert!(engine.redo());
    let blocked_root = dir.path("not-a-directory");
    std::fs::write(&blocked_root, b"owned test obstruction").unwrap();
    let mut engine = seeded_redo_unconfigured();
    let before = State::capture(&engine);
    assert!(engine.configure_original_store(blocked_root, 1 << 20).is_err());
    before.unchanged(&engine);
    assert!(engine.redo());
}

#[test]
fn cancelled_and_stale_prepared_plans_do_not_apply_or_clear_history() {
    let dir = Directory::new();
    let path = write_wave(&dir, "valid.wav", &wave(48_000, TIME_REFERENCE));
    let mut engine = seeded_redo_unconfigured();
    engine.configure_original_store(dir.path("cancel-store"), 1 << 20).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let plan =
        prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions { cancel: Arc::clone(&cancel), ..ImportOptions::default() })
            .unwrap();
    let before = State::capture(&engine);
    cancel.store(true, Ordering::Relaxed);
    assert!(apply_audio_import(&mut engine, plan).is_err());
    before.unchanged(&engine);
    engine.collect_original_assets().unwrap();
    before.unchanged(&engine);
    assert!(engine.redo());
    let mut engine = seeded_redo(&dir);
    let plan = prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions::default()).unwrap();
    engine.session_mut().comments = "document changed after prepare".into();
    let before = State::capture(&engine);
    assert!(apply_audio_import(&mut engine, plan).is_err());
    before.unchanged(&engine);
    engine.collect_original_assets().unwrap();
    before.unchanged(&engine);
    assert!(engine.redo());
}

#[test]
fn leases_survive_undo_redo_session_and_audio_arcs_then_explicit_collection() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    let mut engine = Engine::default();
    engine.configure_original_store(dir.path("store"), 1 << 20).unwrap();
    let receipt = engine.execute("file.import_audio", &json!({"path":path,"policy":"copy"})).unwrap();
    let id = imported_source(&engine, &receipt["imported"][0]).id;
    let held_session = engine.session_arc();
    let held_audio = Arc::clone(held_session.pool.get(id).unwrap());
    let asset_path = held_audio.original_asset.as_ref().unwrap().path.clone();
    assert_eq!(std::fs::read(&asset_path).unwrap(), fixture.bytes);
    assert!(engine.undo());
    assert_eq!(engine.collect_original_assets().unwrap(), 0);
    assert!(engine.redo());
    assert_eq!(engine.collect_original_assets().unwrap(), 0);
    assert!(engine.undo());
    engine.execute("track.new", &json!({"name":"Clear old Redo"})).unwrap();
    assert!(!engine.can_redo());
    assert_eq!(engine.collect_original_assets().unwrap(), 0, "held session/audio still own original lease");
    drop(held_session);
    assert_eq!(engine.collect_original_assets().unwrap(), 0, "an old prepared-style decoded audio Arc pins the lease");
    drop(held_audio);
    assert!(asset_path.exists(), "dropping leases performs no filesystem I/O");
    assert_eq!(engine.collect_original_assets().unwrap(), 1);
    assert!(!asset_path.exists());
}

#[test]
fn published_originals_are_not_expired_temporary_collection_targets() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    let mut engine = Engine::default();
    engine.configure_original_store(dir.path("store"), 1 << 20).unwrap();
    let receipt = engine.execute("file.import_audio", &json!({"path":path,"policy":"copy"})).unwrap();
    let id = imported_source(&engine, &receipt["imported"][0]).id;
    engine.execute("session.save_as", &json!({"path":dir.path("published/Song.scraft")})).unwrap();
    let published = resolve(engine.session(), &engine.session().source(id).unwrap().original.as_ref().unwrap().path);
    assert_eq!(std::fs::read(&published).unwrap(), fixture.bytes);
    engine.replace_session(Session::default());
    engine.collect_original_assets().unwrap();
    assert_eq!(std::fs::read(&published).unwrap(), fixture.bytes);
}

#[test]
fn completed_snapshots_detach_owned_policies_but_reference_rechecks_before_apply() {
    for (policy, name) in [(ImportPolicy::Copy, "copy"), (ImportPolicy::Reference, "reference"), (ImportPolicy::Convert, "convert")] {
        let dir = Directory::new();
        let fixture = wave(48_000, TIME_REFERENCE);
        let path = write_wave(&dir, "original.wav", &fixture);
        let mut engine = seeded_redo(&dir);
        let before = State::capture(&engine);
        let plan = prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions { policy, ..ImportOptions::default() }).unwrap();
        before.unchanged(&engine);
        assert_eq!(plan.receipt()["policy"], name);
        assert_eq!(plan.receipt()["imported"][0]["original"]["sha256"], encoded_hash(&fixture.bytes));
        let mut changed = fixture.bytes.clone();
        changed[100] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        if policy == ImportPolicy::Reference {
            assert!(apply_audio_import(&mut engine, plan).is_err());
            before.unchanged(&engine);
            assert!(engine.redo());
            assert!(engine.session().track_by_name("Seeded Redo").is_some());
        } else {
            let receipt = apply_audio_import(&mut engine, plan).unwrap();
            assert_receipt(&engine, &receipt["imported"][0], &fixture, name, 0);
            assert_original(engine.session(), imported_source(&engine, &receipt["imported"][0]), &fixture, policy);
        }
        assert_eq!(std::fs::read(&path).unwrap(), changed, "import/apply never rewrites the caller's original");
    }
}

#[test]
fn dropped_plan_defers_filesystem_cleanup_and_stage_io_failure_is_atomic() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    let mut engine = seeded_redo(&dir);
    let before = State::capture(&engine);
    let plan = prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions::default()).unwrap();
    let snapshot = PathBuf::from(plan.receipt()["imported"][0]["original"]["path"].as_str().unwrap());
    before.unchanged(&engine);
    assert_eq!(std::fs::read(&snapshot).unwrap(), fixture.bytes);
    assert_eq!(engine.collect_original_assets().unwrap(), 0);
    drop(plan);
    assert!(snapshot.exists(), "unpublished plan Drop performs no filesystem I/O");
    assert_eq!(engine.collect_original_assets().unwrap(), 1);
    assert!(!snapshot.exists());
    before.unchanged(&engine);
    assert!(engine.redo());
    let mut engine = seeded_redo_unconfigured();
    let blocked = dir.path("stage-store");
    engine.configure_original_store(blocked.clone(), 1 << 20).unwrap();
    std::fs::remove_dir(&blocked).unwrap();
    std::fs::write(&blocked, b"owned deterministic ENOTDIR staging obstruction").unwrap();
    let before = State::capture(&engine);
    assert!(prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions::default()).is_err());
    before.unchanged(&engine);
    assert!(engine.redo());
    assert!(engine.session().track_by_name("Seeded Redo").is_some());
}

#[test]
fn aggregate_original_budget_and_id_capacity_refusal_are_atomic() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    let mut engine = seeded_redo(&dir);
    let before = State::capture(&engine);
    assert!(
        prepare_audio_import(
            &mut engine,
            &[path.clone(), path.clone()],
            ImportOptions { max_original_bytes: fixture.bytes.len() as u64 * 2 - 1, ..ImportOptions::default() }
        )
        .is_err()
    );
    before.unchanged(&engine);
    assert!(engine.redo());
    let mut engine = seeded_redo(&dir);
    engine.session_mut().next_id = u64::MAX;
    let before = State::capture(&engine);
    assert!(prepare_audio_import(&mut engine, std::slice::from_ref(&path), ImportOptions::default()).is_err());
    before.unchanged(&engine);
    assert!(engine.redo());
}

#[test]
fn native_and_bundle_open_qualify_original_hash_failure_while_recovering_valid_derivatives() {
    let dir = Directory::new();
    let fixture = wave(48_000, TIME_REFERENCE);
    let path = write_wave(&dir, "valid.wav", &fixture);
    let mut engine = fresh_engine(&dir);
    let receipt = engine.execute("file.import_audio", &json!({"path":path,"policy":"copy"})).unwrap();
    let id = imported_source(&engine, &receipt["imported"][0]).id;
    let target = dir.path("native/Song.scraft");
    engine.execute("session.save_as", &json!({"path":target})).unwrap();
    let (bundle, result) = session_copy::bundle(&engine, "Copy.scraftz").unwrap();
    let original_path = resolve(engine.session(), &engine.session().source(id).unwrap().original.as_ref().unwrap().path);
    let mut changed = fixture.bytes.clone();
    changed[100] ^= 1;
    std::fs::remove_file(&original_path).unwrap();
    std::fs::write(&original_path, &changed).unwrap();
    let mut reopened = seeded_redo(&dir);
    let issues = io::open_session(&mut reopened, target.to_str().unwrap()).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("original"));
    assert_eq!(reopened.session().source(id).unwrap().original, engine.session().source(id).unwrap().original);
    assert_pcm(&reopened.session().pool.get(id).unwrap().buffer, &fixture);
    assert!(reopened.session().pool.get(id).unwrap().original_asset.is_none(), "an invalid original receives no verified ownership lease");
    let before = State::capture(&reopened);
    io::save_copy(&reopened, dir.path("recovery/Recovery.scraft").to_str().unwrap()).unwrap();
    before.unchanged(&reopened);
    let recovery = Session::from_json(&std::fs::read_to_string(dir.path("recovery/Recovery.scraft")).unwrap()).unwrap();
    let recovered_original = recovery.source(id).unwrap().original.as_ref().unwrap();
    assert!(Path::new(&recovered_original.path).is_absolute(), "moving unresolved relative original paths preserves their old locator");
    assert_eq!(recovered_original.sha256, encoded_hash(&fixture.bytes));
    assert_eq!(recovered_original.info.sample_rate, fixture.rate);
    let additional = write_wave(&dir, "additional.wav", &fixture);
    let previous_count = reopened.session().sources.len();
    let additional_receipt = reopened.execute("file.import_audio", &json!({"path":additional,"policy":"copy"})).unwrap();
    assert_eq!(reopened.session().sources.len(), previous_count + 1, "historical unresolved originals do not block a separate valid import");
    assert_original(reopened.session(), imported_source(&reopened, &additional_receipt["imported"][0]), &fixture, ImportPolicy::Copy);
    assert_eq!(reopened.session().source(id).unwrap().original, before.session.source(id).unwrap().original);
    use std::io::{Read as _, Write as _};
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bundle)).unwrap();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut changed_entries = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name.starts_with("Original Audio/") {
            assert_eq!(bytes, fixture.bytes);
            bytes = changed.clone();
            changed_entries += 1;
        }
        writer.start_file(name, options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    assert_eq!(changed_entries, 1);
    let tampered_bundle = writer.finish().unwrap().into_inner();
    let mut reopened = seeded_redo(&dir);
    let issues = io::open_session_bytes(&mut reopened, &result.output_path, &tampered_bundle).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issues[0].contains("original"));
    assert_eq!(reopened.session().source(id).unwrap().original.as_ref().unwrap().sha256, encoded_hash(&fixture.bytes));
    assert_pcm(&reopened.session().pool.get(id).unwrap().buffer, &fixture);
    assert!(reopened.session().pool.get(id).unwrap().original_asset.is_none());
    let mut future = engine.session().clone();
    future.sources.iter_mut().find(|source| source.id == id).unwrap().original.as_mut().unwrap().version = 2;
    let future_path = dir.path("native/Future.scraft");
    std::fs::write(&future_path, future.to_json().unwrap()).unwrap();
    let mut rejected = seeded_redo(&dir);
    let before = State::capture(&rejected);
    assert!(io::open_session(&mut rejected, future_path.to_str().unwrap()).is_err());
    before.unchanged(&rejected);
    assert!(rejected.redo());
}

#[test]
fn valid_bundle_original_lease_survives_missing_or_bad_derivative_and_pcm_removal() {
    for missing in [false, true] {
        let dir = Directory::new();
        let fixture = wave(48_000, TIME_REFERENCE);
        let path = write_wave(&dir, "valid.wav", &fixture);
        let mut engine = fresh_engine(&dir);
        let receipt = engine.execute("file.import_audio", &json!({"path":path,"policy":"copy"})).unwrap();
        let id = imported_source(&engine, &receipt["imported"][0]).id;
        let (bundle, result) = session_copy::bundle(&engine, "Copy.scraftz").unwrap();
        use std::io::{Read as _, Write as _};
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bundle)).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut derivatives_changed = 0;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name.starts_with("Audio Files/") {
                derivatives_changed += 1;
                if missing {
                    continue;
                }
                bytes = b"owned malformed derivative".to_vec();
            }
            writer.start_file(name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        assert_eq!(derivatives_changed, 1);
        let damaged = writer.finish().unwrap().into_inner();
        let mut reopened = fresh_engine(&dir);
        let issues = io::open_session_bytes(&mut reopened, &result.output_path, &damaged).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].contains("original media"), "the issue concerns only the derivative");
        assert!(!reopened.session().pool.contains(id));
        let original_asset = reopened.session().pool.original_asset(id).expect("valid originals retain a lease without decoded PCM");
        let asset_path = original_asset.path.clone();
        assert_eq!(std::fs::read(&asset_path).unwrap(), fixture.bytes);
        assert_original(reopened.session(), reopened.session().source(id).unwrap(), &fixture, ImportPolicy::Copy);
        assert_eq!(reopened.collect_original_assets().unwrap(), 0);
        assert!(!session_copy::preview(&reopened, None).ready);
        assert!(reopened.execute("session.save_copy", &json!({"path":dir.path("refused/Copy.scraft")})).is_err());
        assert!(!dir.path("refused").exists());
        // The decoded-media removal API deliberately does not release encoded-original ownership.
        assert!(reopened.session_mut().pool.remove(id).is_none());
        assert!(reopened.session().pool.original_asset(id).is_some());
        assert_eq!(reopened.collect_original_assets().unwrap(), 0);
        let held = reopened.session_arc();
        reopened.replace_session(Session::default());
        assert_eq!(reopened.collect_original_assets().unwrap(), 0, "held Session Arc pins original-only leases");
        drop(held);
        assert!(asset_path.exists(), "lease Drop has no filesystem side effects");
        assert_eq!(reopened.collect_original_assets().unwrap(), 1);
        assert!(!asset_path.exists());
    }
}

#[cfg(unix)]
#[test]
fn supplied_fifo_is_refused_for_every_policy_without_blocking_or_document_mutation() {
    use std::os::unix::fs::FileTypeExt as _;
    let dir = Directory::new();
    let fifo = dir.path("named-source.wav");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    assert!(std::fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
    let store = dir.path("fifo-store");
    let (tx, rx) = std::sync::mpsc::channel();
    // Engine and State stay on the worker; no Engine Send assumption or physical device.
    let worker = std::thread::spawn(move || {
        let mut engine = seeded_redo_unconfigured();
        engine.configure_original_store(store, 1 << 20).unwrap();
        for policy in ["copy", "reference", "convert"] {
            let before = State::capture(&engine);
            let error =
                engine.execute("file.import_audio", &json!({"path":fifo,"policy":policy})).expect_err("FIFO must be refused as nonregular input");
            assert!(error.to_string().contains("regular"), "{policy}: {error}");
            before.unchanged(&engine);
        }
        assert!(engine.redo());
        assert!(engine.session().track_by_name("Seeded Redo").is_some());
        tx.send(()).unwrap();
    });
    // A blocking-open regression fails this test in bounded time. This is a generous
    // refusal/liveness bound, not an audio deadline or general I/O performance claim.
    // On timeout/panic the parent still owns cleanup; do not join a blocked worker.
    rx.recv_timeout(std::time::Duration::from_secs(3)).expect("regular-file refusal must finish without waiting for a FIFO writer");
    worker.join().unwrap();
}
