// Shared acceptance setup and independent signal/reference bodies.
use super::*;
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::{Engine, io};
use soundcraft_model::{ChannelFormat, Clip, Insert, TrackKind, b64};
use soundcraft_time::{SampleRate, TempoMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const DELAY_SYNTH: &str = "clap:org.soundcraft.test.bounce-delay-synth";
const CLAP_GAIN: &str = "clap:org.soundcraft.test.gain";
const LIFETIME: u64 = 713;
struct Directory(PathBuf);
impl Directory {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-bounce-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every manifest/descriptor is owned. Missing artifacts fail; no installed search or skip.
fn fixtures() -> &'static Path {
    static DIRECTORY: OnceLock<Directory> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            let directory = Directory::new("controlled-fixtures");
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
            for (manifest, stem, kind, executable, count) in [
                ("clap-host/tests/fixture/Cargo.toml", "clap_fixture", "clap", "Fixture", 2),
                ("clap-host/tests/bounce_fixture/Cargo.toml", "clap_bounce_fixture", "clap", "BounceDelay", 1),
                ("vst3-host/tests/fixture/Cargo.toml", "vst3_fixture", "vst3", "Fixture", 2),
            ] {
                let target = directory.0.join(format!("target-{stem}"));
                let status = std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                    .args(["build", "--quiet", "--offline", "--locked", "--jobs", "1", "--manifest-path"])
                    .arg(root.join(manifest))
                    .arg("--target-dir")
                    .arg(&target)
                    .env_remove("RUSTFLAGS")
                    .status()
                    .unwrap();
                assert!(status.success(), "owned fixture {manifest} must build");
                let library = if cfg!(target_os = "macos") {
                    format!("lib{stem}.dylib")
                } else if cfg!(windows) {
                    format!("{stem}.dll")
                } else {
                    format!("lib{stem}.so")
                };
                let owned = directory.0.join(stem);
                let destination = if kind == "clap" && !cfg!(target_os = "macos") {
                    owned.join(format!("vendor/{executable}.clap"))
                } else {
                    let platform = if cfg!(target_os = "macos") {
                        "MacOS"
                    } else if cfg!(windows) {
                        "x86_64-win"
                    } else if cfg!(target_arch = "aarch64") {
                        "aarch64-linux"
                    } else {
                        "x86_64-linux"
                    };
                    let name = if kind == "clap" || cfg!(target_os = "macos") {
                        executable.to_owned()
                    } else if cfg!(windows) {
                        "Fixture.vst3".into()
                    } else {
                        "Fixture.so".into()
                    };
                    owned.join(format!("vendor/{executable}.{kind}/Contents/{platform}/{name}"))
                };
                std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
                std::fs::copy(target.join("debug").join(library), destination).unwrap();
                let found = if kind == "clap" { soundcraft_clap_host::add_search_dir(&owned) } else { soundcraft_vst3_host::add_search_dir(&owned) };
                assert_eq!(found, count, "exact owned descriptor inventory");
            }
            directory
        })
        .0
        .as_path()
}

fn signal(channel: usize, at: i64) -> f64 {
    if at < 0 {
        return 0.0;
    }
    let left = if (at as usize / 17).is_multiple_of(2) { 0.0625 } else { -0.03125 };
    if channel == 0 { left } else { -left * 0.5 }
}
fn audio(session: &mut Session, track: TrackId, start: i64, length: usize, multiplier: f64) -> soundcraft_model::ClipId {
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: (0..2).map(|ch| (0..length).map(|at| (signal(ch, start + at as i64) * multiplier) as f32).collect()).collect(),
    };
    let source = io::add_source(session, "Original independent source", buffer, None, FileFormat::Wav);
    let id = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "Original", source, 0, start, length as i64));
    id
}
fn session() -> (Session, TrackId) {
    let mut session = Session::new("Controlled export — 原始", SampleRate::new(48_000).unwrap());
    session.edit.click = false;
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Immutable source"));
    session.track_mut(track).unwrap().mixer.pan = vec![-1.0, 1.0];
    (session, track)
}
fn insert(session: &mut Session, track: TrackId, plugin: &str, values: &[(&str, f32)]) {
    let mut insert = Insert::new(plugin);
    insert.params = values.iter().map(|(id, value)| ((*id).to_owned(), *value)).collect();
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
}
fn compare(actual: &[Vec<f32>], expected: &[Vec<f64>], frames: usize, tolerance: f64) {
    assert_eq!(actual.len(), 2);
    assert_eq!(expected.len(), 2);
    for (channel, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(actual.len(), frames);
        assert_eq!(expected.len(), frames);
        for (frame, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
            assert!(actual.is_finite() && expected.is_finite());
            assert!((f64::from(actual) - expected).abs() <= tolerance, "channel {channel} frame {frame}: {actual} != {expected}");
        }
    }
}
struct Harness {
    player: Player,
    owner: CallbackOwner,
}
impl Harness {
    fn new(source: Arc<Session>) -> Self {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (garbage_tx, garbage) = channel();
        let state = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&source), 48_000.0, garbage_tx);
        let exchange = StateExchange::new(state);
        let mut preparer = SessionPreparer::default();
        let mut update = preparer.prepare(Arc::clone(&source), LIFETIME, BLOCK).unwrap();
        let editors = update.editors().into_iter().map(|(track, slot, editor)| ((track, slot), editor)).collect();
        tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&source))).unwrap();
        let mut state = exchange.receiver.try_recv().unwrap();
        state.drain();
        assert!(exchange.sender.try_send(state).is_ok());
        let player = Player {
            shared,
            tx,
            side: Mutex::new(Side {
                #[cfg(test)]
                shadow: HashMap::new(),
                #[cfg(test)]
                rate: 48_000.0,
                editors,
                pending_editor_edits: Vec::new(),
                pending_editor_batch: None,
                garbage,
            }),
            preparer: Mutex::new(preparer),
            exchange: Arc::clone(&exchange),
            clock: None,
            _stream: None,
            device_name: "Controlled CallbackOwner, no physical device".into(),
            device_rate: 48_000,
            device_channels: 2,
            silent: false,
        };
        Self { player, owner: CallbackOwner { exchange, enabled: Arc::new(AtomicBool::new(true)), rejected: None } }
    }
    fn fill(&mut self, count: usize, schedule: &[usize]) -> Vec<Vec<f32>> {
        let mut output = vec![Vec::new(); 2];
        let mut done = 0;
        for &frames in schedule.iter().cycle() {
            if done == count {
                break;
            }
            let n = frames.min(count - done);
            let mut interleaved = vec![0.0; n * 2];
            self.owner.fill(&mut interleaved, 2);
            for sample in interleaved.chunks_exact(2) {
                output[0].push(sample[0]);
                output[1].push(sample[1]);
            }
            done += n;
        }
        output
    }
    fn snapshot(&mut self, source: Arc<Session>) -> ExportSnapshot {
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let owner = &mut self.owner;
            let done = &done;
            let worker = scope.spawn(move || {
                mark_audio_thread();
                while !done.load(Ordering::Acquire) {
                    owner.fill(&mut [0.0; 2], 2);
                    std::thread::yield_now();
                }
            });
            let snapshot = self.player.capture_export_state(source, LIFETIME);
            done.store(true, Ordering::Release);
            worker.join().unwrap();
            snapshot.unwrap()
        })
    }
}

fn exported(engine: &mut Engine, range: Range, tail: i64, preroll: i64, bus: Option<&str>) -> Vec<Vec<f32>> {
    let directory = Directory::new("delivery");
    let path = directory.0.join("Requested.wav");
    let before = engine.session_arc();
    let json = before.to_json().unwrap();
    let history = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone());
    let result = engine
        .execute(
            "file.bounce_mix",
            &json!({"path":path,"format":"wav","bit_depth":"32f","dither":false,"normalize":false,
        "start":range.start,"end":range.end,"tail_samples":tail,"preroll_start":preroll,"source":bus.unwrap_or("main")}),
        )
        .unwrap();
    let (info, decoded) = soundcraft_audio_io::decode(&std::fs::read(&path).unwrap(), Some("wav")).unwrap();
    let frames = usize::try_from(range.len() + tail).unwrap();
    assert_eq!(info.frames, frames as u64);
    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(result["frames"], frames);
    assert_eq!(result["channels"], 2);
    assert_eq!(result["sample_rate"], 48_000);
    assert_eq!(result["seconds"].as_f64().unwrap(), frames as f64 / 48_000.0);
    assert!(Arc::ptr_eq(&before, &engine.session_arc()));
    assert_eq!(engine.session().to_json().unwrap(), json);
    assert_eq!((engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone()), history);
    decoded.channels
}

fn instrument(compensation: bool, gain: f32) -> (Session, TrackId) {
    fixtures();
    let mut s = Session::new("Owned instrument original", SampleRate::new(48_000).unwrap());
    s.tempo = TempoMap::new(120.0, 4, 4).unwrap();
    s.edit.delay_compensation = compensation;
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Actual delayed hosted instrument"));
    let id = s.new_clip_id();
    let sequence = soundcraft_midi::Sequence {
        notes: vec![
            soundcraft_midi::Note { pitch: 60, velocity: 64, release_velocity: 64, channel: 0, start: 2, length: 100 },
            soundcraft_midi::Note { pitch: 72, velocity: 120, release_velocity: 64, channel: 0, start: 9, length: 100 },
        ],
        ctrls: Vec::new(),
    };
    let track = s.track_mut(t).unwrap();
    track.mixer.pan = vec![-1.0, 1.0];
    let mut insert = Insert::new(DELAY_SYNTH);
    insert.params.insert("7".into(), gain);
    track.instrument = Some(insert);
    track.playlist_mut().unwrap().clips.push(Clip::midi(id, "Original notes with later opposing event", 0, 4000, sequence));
    (s, t)
}

fn detached(original: &Engine, snapshot: ExportSnapshot) -> Engine {
    let mut copy = Engine::new(original.session().clone());
    for (track, slot, bytes) in snapshot.states {
        copy.execute("mix.insert_state", &json!({"track":track.0,"slot":if slot == soundcraft_mix::INSTRUMENT_SLOT { json!("instrument") } else { json!(slot) },"state":b64::encode(&bytes)})).unwrap();
    }
    for (track, slot, parameter, value) in snapshot.static_baselines {
        let track = copy.session_mut().track_mut(track).unwrap();
        let insert =
            if slot == soundcraft_mix::INSTRUMENT_SLOT { track.instrument.as_mut().unwrap() } else { track.mixer.inserts[slot].as_mut().unwrap() };
        insert.params.entry(parameter).or_insert(value);
    }
    for (track, slot, parameter, value) in snapshot.editor_edits {
        if slot == soundcraft_mix::INSTRUMENT_SLOT {
            copy.execute("mix.instrument_param", &json!({"track":track.0,"param":parameter,"value":value})).unwrap();
        } else {
            copy.execute("mix.insert_param", &json!({"track":track.0,"slot":slot,"param":parameter,"value":value})).unwrap();
        }
    }
    copy
}
