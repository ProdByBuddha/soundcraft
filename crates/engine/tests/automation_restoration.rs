//! Native command atomicity and restoration/persistence acceptance for prepared automation.
#![cfg(not(target_arch = "wasm32"))]

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::{Engine, EngineError, io};
use soundcraft_model::{AutoParam, ChannelFormat, Clip, ClipId, Insert, Route, SendSlot, Session, SourceId, TrackId, TrackKind, b64};
use soundcraft_time::SampleRate;
use std::path::PathBuf;
use std::sync::Arc;

#[allow(dead_code)]
#[path = "../../mix/tests/support/automation_reference.rs"]
mod reference;

const HOST_GAIN: &str = "clap:org.soundcraft.test.gain";
const HOST_CURVE: &[(i64, f64)] = &[(13, 0.25), (37, 1.75), (93, 0.5), (94, 2.0), (173, 0.0), (211, 0.75), (367, 1.0)];

struct OwnedDirectory(PathBuf);
impl OwnedDirectory {
    fn new(prefix: &str) -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The original owned fixture, built from its unchanged source for ordinary CI. An execution
/// owner may supply an already-built directory; neither path searches installed plugins.
fn owned_fixture() -> Option<OwnedDirectory> {
    let owned;
    let directory = if let Some(path) = std::env::var_os("SOUNDCRAFT_TEST_CLAP_FIXTURE_DIR") {
        owned = None;
        let path = std::fs::canonicalize(path).unwrap();
        assert!(path.is_dir(), "explicit original CLAP fixture directory must exist");
        path
    } else {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/fixture/Cargo.toml");
        assert!(manifest.is_file(), "original owned CLAP fixture source is required");
        let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("engine-restoration-clap-fixture-target");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "--quiet", "--offline", "--locked", "--jobs", "1", "--manifest-path"])
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .unwrap();
        assert!(status.success(), "building the original controlled CLAP fixture failed");
        let install = OwnedDirectory::new("soundcraft-engine-clap-restoration");
        let library = if cfg!(target_os = "macos") {
            "libclap_fixture.dylib"
        } else if cfg!(windows) {
            "clap_fixture.dll"
        } else {
            "libclap_fixture.so"
        };
        let destination = if cfg!(target_os = "macos") {
            let executable = install.0.join("vendor/Fixture.clap/Contents/MacOS");
            std::fs::create_dir_all(&executable).unwrap();
            executable.join("Fixture")
        } else {
            std::fs::create_dir(install.0.join("vendor")).unwrap();
            install.0.join("vendor/Fixture.clap")
        };
        std::fs::copy(target.join("debug").join(library), destination).unwrap();
        let path = install.0.clone();
        owned = Some(install);
        path
    };
    // add_search_dir scans only this explicit directory and seeds the registry before any
    // metadata/instance lookup. The resulting cache must contain only the two owned types.
    assert_eq!(soundcraft_clap_host::add_search_dir(&directory), 2);
    let descriptors = soundcraft_clap_host::scan();
    assert_eq!(descriptors.len(), 2);
    assert!(descriptors.iter().all(|d| d.path.starts_with(&directory) && d.vendor == "SoundCraft tests"));
    assert!(descriptors.iter().any(|d| d.id == HOST_GAIN));
    assert!(descriptors.iter().any(|d| d.id == "clap:org.soundcraft.test.synth"));
    owned
}

fn audio_engine(plugin: &str) -> (Engine, TrackId, SourceId, ClipId, AudioBuffer) {
    let mut session = Session::new("Restored source — 原始", SampleRate::new(48_000).unwrap());
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original source"));
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: (0..2).map(|channel| (0..reference::LEN).map(|frame| reference::signal(channel, frame) as f32).collect()).collect(),
    };
    let source = io::add_source(&mut session, "Immutable original", buffer.clone(), None, FileFormat::Wav);
    let clip = session.new_clip_id();
    let strip = session.track_mut(track).unwrap();
    strip.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original clip — 原始", source, 0, 0, reference::LEN as i64));
    // Imported documents can omit a registered static parameter. Do not materialize its key.
    strip.mixer.inserts[0] = Some(Insert::new(plugin));
    session.edit.selected_tracks = vec![track];
    let mut engine = Engine::new(session);
    engine.execute("track.comments", &json!({"track": track.0, "comments": "Keep source identity 🎙\nImmutable original — 原始"})).unwrap();
    (engine, track, source, clip, buffer)
}

fn points(engine: &mut Engine, track: TrackId, param: &str, curve: &[(i64, f64)]) {
    for &(at, value) in curve {
        engine.execute("automation.set_point", &json!({"track": track.0, "param": param, "at": at, "value": value})).unwrap();
    }
}

fn source_bits(session: &Session, source: SourceId, expected: &AudioBuffer) {
    let actual = &session.pool.get(source).unwrap().buffer;
    assert_eq!(actual.sample_rate, expected.sample_rate);
    assert_eq!(actual.channels.len(), expected.channels.len());
    for (actual, expected) in actual.channels.iter().zip(&expected.channels) {
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    }
}

fn render_read_only(engine: &Engine, renderer: &mut soundcraft_mix::OfflineRenderer, expected: &[Vec<f64>], tolerance: f64) {
    let before = engine.session_arc();
    let serialized = before.to_json().unwrap();
    let revision = engine.revision;
    let generation = engine.session_generation();
    let dirty = engine.is_dirty();
    let history = engine.undo_history();
    let journal = engine.journal.clone();
    let mut actual = vec![vec![0.0; reference::LEN]; 2];
    renderer.render(0, reference::LEN, &mut actual).unwrap();
    reference::check(&actual, expected, 2, reference::LEN, tolerance);
    assert!(Arc::ptr_eq(&before, &engine.session_arc()));
    assert_eq!(engine.session().to_json().unwrap(), serialized);
    assert_eq!(engine.revision, revision);
    assert_eq!(engine.session_generation(), generation);
    assert_eq!(engine.is_dirty(), dirty);
    assert_eq!(engine.undo_history(), history);
    assert_eq!(engine.journal, journal);
}

fn scalar_reference(automated: bool, reads: bool, bypass: bool) -> Vec<Vec<f64>> {
    (0..2)
        .map(|channel| {
            (0..reference::LEN)
                .map(|frame| {
                    let position = frame as i64;
                    let gain = if bypass {
                        1.0
                    } else if automated && reads {
                        reference::linear(HOST_CURVE, position)
                    } else {
                        1.5
                    };
                    let volume = if reads { reference::db_gain(reference::linear(reference::VOLUME, position), -144.0) } else { 1.0 };
                    reference::signal(channel, frame) * gain * volume
                })
                .collect()
        })
        .collect()
}

#[test]
fn restored_omitted_key_returns_to_creation_state_then_capture_save_open_preserves_originals() {
    let _fixture = owned_fixture();
    let mut plugin = soundcraft_clap_host::instantiate(HOST_GAIN).unwrap();
    plugin.prepare(48_000.0, 64, 2);
    plugin.prepare_automation(2, 0).unwrap();
    let mut input = vec![vec![0.25; 64]; 2];
    let delivered = plugin.process_with_targets(&mut input, 64, &[soundcraft_dsp::ParamTarget { id: "7", value: 1.5 }]);
    assert_eq!(delivered, soundcraft_dsp::ProcessDelivery::Delivered);
    let initial_state = plugin.save_state().unwrap();
    assert_eq!(initial_state.len(), 16);
    assert_eq!(f64::from_le_bytes(initial_state[..8].try_into().unwrap()), 1.5);
    assert_eq!(f64::from_le_bytes(initial_state[8..].try_into().unwrap()), 0.0);
    drop(plugin);

    let (mut engine, track, source, clip, buffer) = audio_engine(HOST_GAIN);
    let stored = engine.execute("mix.insert_state", &json!({"track": track.0, "slot": "a", "state": b64::encode(&initial_state)})).unwrap();
    assert_eq!(stored["bytes"], 16);
    points(&mut engine, track, "plugin:a:7", HOST_CURVE);
    points(&mut engine, track, "volume", reference::VOLUME);
    let original = engine.session_arc();
    let original_clip = original.track(track).unwrap().clips()[0].clone();
    let original_lane = original.track(track).unwrap().lane(&AutoParam::Plugin { slot: 0, param: "7".into() }).unwrap().clone();
    assert_eq!(original_clip.id, clip);
    assert!(original.track(track).unwrap().mixer.inserts[0].as_ref().unwrap().params.is_empty());
    let mut renderer = soundcraft_mix::OfflineRenderer::prepare(engine.session_arc(), reference::LEN).unwrap();
    let active = scalar_reference(true, true, false);
    let restored = scalar_reference(false, true, false);
    reference::different(&active, &restored, 0.05);
    render_read_only(&engine, &mut renderer, &active, 1e-6);

    engine.execute("mix.automation_mode", &json!({"track": track.0, "mode": "off"})).unwrap();
    renderer.reprepare(engine.session_arc()).unwrap();
    render_read_only(&engine, &mut renderer, &scalar_reference(false, false, false), 1e-6);
    engine.execute("mix.automation_mode", &json!({"track": track.0, "mode": "read"})).unwrap();
    renderer.reprepare(engine.session_arc()).unwrap();
    render_read_only(&engine, &mut renderer, &active, 1e-6);

    engine.execute("mix.insert_bypass", &json!({"track": track.0, "slot": "a", "value": true})).unwrap();
    renderer.reprepare(engine.session_arc()).unwrap();
    render_read_only(&engine, &mut renderer, &scalar_reference(true, true, true), 1e-6);
    engine.execute("mix.insert_bypass", &json!({"track": track.0, "slot": "a", "value": false})).unwrap();
    renderer.reprepare(engine.session_arc()).unwrap();
    render_read_only(&engine, &mut renderer, &active, 1e-6);

    let cleared = engine.execute("automation.clear", &json!({"track": track.0, "param": "plugin:a:7", "start": 0, "end": 0})).unwrap();
    assert_eq!(cleared["removed"], HOST_CURVE.len());
    renderer.reprepare(engine.session_arc()).unwrap();
    render_read_only(&engine, &mut renderer, &restored, 1e-6);
    assert!(engine.session().track(track).unwrap().mixer.inserts[0].as_ref().unwrap().params.is_empty());
    assert_eq!(engine.session().track(track).unwrap().clips(), std::slice::from_ref(&original_clip));
    assert_eq!(original.track(track).unwrap().lane(&original_lane.param), Some(&original_lane));
    assert!(Arc::ptr_eq(original.pool.get(source).unwrap(), engine.session().pool.get(source).unwrap()));
    source_bits(&original, source, &buffer);
    source_bits(engine.session(), source, &buffer);

    // Save only after the omitted key has been restored. Saving while automation is active
    // remains a separate unresolved baseline/state policy (EX10), not proven by this test.
    let captured = renderer.mix_mut().capture_states();
    assert_eq!(captured.len(), 1);
    let (captured_track, slot, state) = &captured[0];
    assert_eq!((*captured_track, *slot), (track, 0));
    assert_eq!(state, &initial_state);
    let history = engine.undo_history();
    let journal = engine.journal.clone();
    engine.execute("mix.insert_state", &json!({"track": captured_track.0, "slot": slot, "state": b64::encode(state)})).unwrap();
    assert_eq!(engine.undo_history(), history);
    assert_eq!(engine.journal, journal);
    let expected_tracks = engine.session().tracks.clone();
    let directory = OwnedDirectory::new("soundcraft-automation-restoration-save");
    let file = directory.0.join("Restored.scraft");
    let saved = engine.execute("session.save", &json!({"path": file.to_str().unwrap()})).unwrap();
    assert_eq!(saved["audio_files_written"], 1);
    assert!(!engine.is_dirty());
    assert_eq!(engine.session().tracks, expected_tracks);
    source_bits(engine.session(), source, &buffer);

    let mut reopened = Engine::default();
    let opened = reopened.execute("session.open", &json!({"path": file.to_str().unwrap()})).unwrap();
    assert_eq!(opened["missing"], json!([]));
    assert_eq!(reopened.session().tracks, expected_tracks);
    assert_eq!(reopened.session().track_by_name("Original source").unwrap().id, track);
    assert_eq!(reopened.session().track(track).unwrap().clips()[0].id, clip);
    assert_eq!(reopened.session().track(track).unwrap().mixer.inserts[0].as_ref().unwrap().state_bytes(), Some(initial_state));
    source_bits(reopened.session(), source, &buffer);
    let mut fresh = soundcraft_mix::OfflineRenderer::prepare(reopened.session_arc(), reference::LEN).unwrap();
    render_read_only(&reopened, &mut fresh, &restored, 1e-6);
    assert!(!reopened.is_dirty());
    assert!(!reopened.can_undo());
    assert!(!reopened.can_redo());
    source_bits(&original, source, &buffer);
    assert_eq!(original.track(track).unwrap().lane(&original_lane.param), Some(&original_lane));
}

#[test]
fn fresh_open_uses_independent_gain_20ms_reference_without_mutating_saved_source_or_curves() {
    let (mut engine, track, source, clip, buffer) = audio_engine("gain");
    points(&mut engine, track, "plugin:a:gain", reference::PLUGIN_GAIN);
    let original = engine.session_arc();
    let expected_tracks = original.tracks.clone();
    let directory = OwnedDirectory::new("soundcraft-gain-automation-reopen");
    let file = directory.0.join("Gain.scraft");
    let saved = engine.execute("session.save", &json!({"path": file.to_str().unwrap()})).unwrap();
    assert_eq!(saved["audio_files_written"], 1);
    let mut reopened = Engine::default();
    let opened = reopened.execute("session.open", &json!({"path": file.to_str().unwrap()})).unwrap();
    assert_eq!(opened["missing"], json!([]));
    assert_eq!(reopened.session().tracks, expected_tracks);
    assert_eq!(reopened.session().track(track).unwrap().clips()[0].id, clip);
    assert!(reopened.session().track(track).unwrap().mixer.inserts[0].as_ref().unwrap().params.is_empty());
    source_bits(reopened.session(), source, &buffer);
    let mut memory = reference::GainMemory::new();
    let mut expected = vec![vec![0.0; reference::LEN]; 2];
    for frame in 0..reference::LEN {
        let gain = memory.next(reference::linear(reference::PLUGIN_GAIN, frame as i64));
        for (channel, samples) in expected.iter_mut().enumerate() {
            samples[frame] = reference::signal(channel, frame) * gain;
        }
    }
    let static_unity: Vec<Vec<f64>> = (0..2).map(|channel| (0..reference::LEN).map(|frame| reference::signal(channel, frame)).collect()).collect();
    reference::different(&expected, &static_unity, 0.02);
    let mut renderer = soundcraft_mix::OfflineRenderer::prepare(reopened.session_arc(), reference::LEN).unwrap();
    // Same 2e-5 bound as the original isolated real-Gain baseline, without widening it.
    render_read_only(&reopened, &mut renderer, &expected, 2e-5);
    assert_eq!(reopened.session().tracks, original.tracks);
    source_bits(&original, source, &buffer);
    source_bits(reopened.session(), source, &buffer);
    assert!(!reopened.is_dirty());
}

fn mixed_tracks() -> (Engine, TrackId, TrackId) {
    let mut session = Session::new("Command restoration", SampleRate::new(48_000).unwrap());
    let stereo = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Stereo source"));
    let mono = session.add_track(TrackKind::Aux, ChannelFormat::Mono, Some("Mono return"));
    let source = io::add_source(
        &mut session,
        "Immutable synthetic audio",
        AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 128], vec![-0.125; 128]] },
        None,
        FileFormat::Wav,
    );
    let clip = session.new_clip_id();
    let track = session.track_mut(stereo).unwrap();
    track.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original source", source, 0, 0, 128));
    track.mixer.inserts[0] = Some(Insert::new("gain"));
    track.mixer.sends[0] = Some(SendSlot::new(Route::None));
    // A different valid plugin makes exact-ID validation fail on the second selected track.
    session.track_mut(mono).unwrap().mixer.inserts[0] = Some(Insert::new("compressor"));
    session.edit.selected_tracks = vec![stereo, mono];
    let mut engine = Engine::new(session);
    for (track, comments) in [(stereo, "Source — 原始\nKeep its audio and curves"), (mono, "Return 🎙\nKeep this distinct comment")] {
        engine.execute("track.comments", &json!({"track": track.0, "comments": comments})).unwrap();
    }
    (engine, stereo, mono)
}

fn rejects_without_mutation(engine: &mut Engine, command: &str, params: &Value, redo_track: TrackId) {
    let before = engine.session_arc();
    let document = before.to_json().unwrap();
    let revision = engine.revision;
    let generation = engine.session_generation();
    let dirty = engine.is_dirty();
    let history = engine.undo_history();
    let redo = engine.redo_label().map(str::to_owned);
    let journal = engine.journal.clone();
    let clipboard = engine.clipboard.clone();
    let path = engine.path.clone();
    let requests = engine.transport_requests.clone();
    assert!(engine.can_redo());
    assert_eq!(redo.as_deref(), Some("Rename Track"));
    assert!(!history.is_empty());
    assert!(!journal.is_empty());

    let error = engine.execute(command, params).unwrap_err();
    assert!(matches!(error, EngineError::BadParams(ref id, _) if id == command), "{command}: {params}: {error}");
    assert!(Arc::ptr_eq(&before, &engine.session_arc()), "{command}: {params}");
    assert_eq!(engine.session().to_json().unwrap(), document);
    assert_eq!(engine.revision, revision);
    assert_eq!(engine.session_generation(), generation);
    assert_eq!(engine.is_dirty(), dirty);
    assert_eq!(engine.undo_history(), history);
    assert!(engine.can_redo());
    assert_eq!(engine.redo_label().map(str::to_owned), redo);
    assert_eq!(engine.journal, journal);
    assert_eq!(engine.clipboard, clipboard);
    assert_eq!(engine.path, path);
    assert_eq!(engine.transport_requests, requests);

    // Exercise the retained redo entry rather than checking only its label or length.
    assert!(engine.redo());
    assert_eq!(engine.session().track(redo_track).unwrap().name, "Pending rename");
    assert_eq!(engine.session().track(redo_track).unwrap().comments, before.track(redo_track).unwrap().comments);
    assert!(engine.undo());
    assert_eq!(engine.session().to_json().unwrap(), document);
}

#[test]
fn selected_mixed_tracks_reject_invalid_automation_before_any_edit_and_keep_usable_redo() {
    let (mut engine, stereo, mono) = mixed_tracks();
    // Positive controls prove these same command entry points accept a real occupied lane.
    engine.execute("automation.set_point", &json!({"track": stereo.0, "param": "plugin:a:gain", "at": 13, "value": -6.0})).unwrap();
    engine.execute("automation.write_range", &json!({"track": stereo.0, "param": "plugin:a:gain", "start": 37, "end": 93, "value": 3.0})).unwrap();
    engine.execute("track.view", &json!({"track": stereo.0, "view": "plugin:a:gain"})).unwrap();
    engine.execute("track.rename", &json!({"track": mono.0, "name": "Pending rename"})).unwrap();
    assert!(engine.undo());
    engine.clipboard.length = 23;
    assert!(engine.is_dirty());

    for param in [
        "pan2",            // valid for the first stereo track, invalid for the mono return
        "send_a_pan",      // occupied on the first track, absent on the second
        "plugin:a:gain",   // exact ID on Gain, absent on the second track's Compressor
        "plugin:a:Gain",   // exact IDs are case-sensitive
        "plugin:b:gain",   // empty insert
        "plugin:k:gain",   // beyond the ten-insert family cardinality
        "plugin:256:gain", // beyond the serialized index representation
        "send_k_mute",     // beyond the ten-send family cardinality
        "send_256_level",  // beyond the serialized index representation
    ] {
        for (command, params) in [
            ("automation.set_point", json!({"param": param, "at": 17, "value": 0.25})),
            ("automation.write_range", json!({"param": param, "start": 17, "end": 61, "value": 0.25})),
            ("track.view", json!({"view": param})),
        ] {
            rejects_without_mutation(&mut engine, command, &params, mono);
        }
    }
    for command in ["automation.set_point", "automation.write_range"] {
        let params = json!({"track": stereo.0, "param": "plugin:a:gain", "at": 17, "start": 17, "end": 61, "value": f64::MAX});
        rejects_without_mutation(&mut engine, command, &params, mono);
    }
}
