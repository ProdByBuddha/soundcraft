use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, BitDepth, BwfInfo, EncodeOptions, FileFormat};
use soundcraft_engine::Engine;
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("soundcraft-import-ui-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn audio(&self) -> PathBuf {
        let path = self.0.join("original-44100.wav");
        let bytes = soundcraft_audio_io::encode(
            &AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.25; 441], vec![-0.125; 441]] },
            &EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None },
        )
        .unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn bwf(&self) -> PathBuf {
        let path = self.0.join("timestamped.wav");
        let bytes = soundcraft_audio_io::encode(
            &AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.25; 441]] },
            &EncodeOptions {
                format: FileFormat::Wav,
                bit_depth: BitDepth::Float32,
                dither: false,
                bwf: Some(BwfInfo { time_reference: 22_050, description: "Synthetic UI acceptance".into(), ..BwfInfo::default() }),
            },
        )
        .unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn harness(path: &std::path::Path, render: bool) -> Harness<'static, SoundApp> {
    harness_with_engine(path, render, Engine::default())
}

fn harness_with_engine(path: &std::path::Path, render: bool, engine: Engine) -> Harness<'static, SoundApp> {
    let selected = path.to_string_lossy().into_owned();
    let services = Services {
        pick_open: Some(Box::new(move |purpose, _| {
            assert_eq!(purpose, "file.import_audio");
            Some(selected.clone())
        })),
        ..Services::default()
    };
    let mut app = SoundApp::new(engine, None, services);
    menus::invoke_menu(&mut app, "file.import_audio", "File > Import > Audio...");
    let mut builder = Harness::builder().with_size(vec2(1600.0, 1100.0));
    if render {
        builder = builder.wgpu();
    }
    let mut h = builder.build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

#[test]
fn native_import_controls_apply_each_policy_and_explicit_conversion() {
    let fixture = Fixture::new();
    let path = fixture.audio();
    let original_bytes = std::fs::read(&path).unwrap();
    let shots = std::env::var_os("SOUNDCRAFT_IMPORT_POLICY_SHOT_DIR").map(PathBuf::from);
    if let Some(dir) = &shots {
        std::fs::create_dir_all(dir).unwrap();
    }
    for (policy, label) in [
        ("copy", "Copy — preserve original bytes in the session"),
        ("reference", "Reference — retain the external original file"),
        ("convert", "Convert — preserve originals and convert playback audio"),
    ] {
        let mut h = harness(&path, shots.is_some());
        assert_eq!(h.state().dialogs.open_name(), Some("import_audio"));
        h.get_by_label("Add file…").click();
        h.run_steps(4);
        h.get_by_label(label).click();
        h.run_steps(4);
        if policy != "convert" {
            h.get_by_label("Allow session-rate playback conversion for mixed-rate files").click();
            h.run_steps(4);
        }
        if let Some(dir) = &shots {
            h.render().unwrap().save(dir.join(format!("{policy}.png"))).unwrap();
        }
        h.get_by_role_and_label(egui::accesskit::Role::Button, "Import Audio").click();
        h.run_steps(4);
        assert!(h.state().dialogs.open.is_none(), "{}", h.state().ui.status);
        let engine = &h.state().engine;
        let source = engine.session().sources.first().unwrap();
        let original = source.original.as_ref().unwrap();
        assert_eq!(serde_json::to_value(original.policy).unwrap(), json!(policy));
        assert_eq!(original.info.sample_rate, 44_100);
        assert_eq!(source.sample_rate, 48_000);
        assert_eq!(source.frames, 480);
        assert_eq!(engine.journal.last().unwrap().0, "file.import_audio");
        assert_eq!(engine.journal.last().unwrap().1["policy"], policy);
        assert_eq!(engine.journal.last().unwrap().1["rate_conversion"], true);
        assert!(h.state().ui.status.contains("original bytes and playback audio recorded separately"));
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    }
}

#[test]
fn rate_mismatch_keeps_policy_dialog_and_cancel_keeps_session_unchanged() {
    let fixture = Fixture::new();
    let path = fixture.audio();
    let mut h = harness(&path, false);
    let before = h.state().engine.session_arc();
    let revision = h.state().engine.revision;
    h.get_by_label("Add file…").click();
    h.run_steps(4);
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Import Audio").click();
    h.run_steps(4);
    assert_eq!(h.state().dialogs.open_name(), Some("import_audio"));
    assert!(std::sync::Arc::ptr_eq(&before, &h.state().engine.session_arc()));
    assert_eq!(revision, h.state().engine.revision);
    assert!(h.state().engine.journal.is_empty());
    h.get_by_label("Cancel").click();
    h.run_steps(4);
    assert!(h.state().dialogs.open.is_none());
    assert!(std::sync::Arc::ptr_eq(&before, &h.state().engine.session_arc()));
    assert_eq!(revision, h.state().engine.revision);
}

#[test]
fn timestamp_and_selected_track_controls_place_bwf_relative_to_session_start() {
    let fixture = Fixture::new();
    let path = fixture.bwf();
    let mut engine = Engine::default();
    let session = engine.session_mut();
    let track = session.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Mono, Some("Existing"));
    session.edit.selected_tracks = vec![track];
    session.timecode_start = 12_000;
    let mut h = harness_with_engine(&path, false, engine);
    h.get_by_label("Add file…").click();
    h.run_steps(4);
    h.get_by_label("Convert — preserve originals and convert playback audio").click();
    h.run_steps(4);
    h.get_by_label("Create a new audio track for each file").click();
    h.run_steps(4);
    h.get_by_label("Place files using their BWF timestamps").click();
    h.run_steps(4);
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Import Audio").click();
    h.run_steps(4);
    assert!(h.state().dialogs.open.is_none(), "{}", h.state().ui.status);
    let engine = &h.state().engine;
    assert_eq!(engine.session().tracks.len(), 1);
    let clip = engine.session().track(track).unwrap().clips().first().unwrap();
    assert_eq!(clip.start, 12_000); // 22,050 / 44,100 * 48,000 minus the session start.
    assert_eq!(clip.length, 480);
    assert_eq!(engine.journal.last().unwrap().1["new_tracks"], false);
    assert_eq!(engine.journal.last().unwrap().1["use_timestamp"], true);
}
