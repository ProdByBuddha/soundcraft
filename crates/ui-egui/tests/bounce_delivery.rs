//! Real dialog/dispatcher acceptance. Expected Float32 PCM comes from literal samples,
//! not a second renderer. The callback/hosted-state comparisons live in Playback tests.
use egui::{Event, Key, Modifiers, PointerButton, vec2};
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::{Engine, io};
use soundcraft_model::{ChannelFormat, Clip, Session, TrackKind};
use soundcraft_time::Range;
use soundcraft_ui_egui::{Services, SoundApp, dialogs::Dialog, menus};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("soundcraft-bounce-ui-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn output(&self) -> PathBuf {
        self.0.join("Mix.wav")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample(channel: usize, frame: usize) -> f32 {
    [[0.25, -0.125, 0.5, -0.25], [-0.5, 0.125, 0.25, -0.125]][channel][frame % 4]
}
fn engine() -> Engine {
    let mut session = Session::default();
    session.edit.selection = Range::new(13, 82);
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Literal PCM"));
    session.track_mut(track).unwrap().mixer.pan = vec![-1.0, 1.0];
    let source = io::add_source(
        &mut session,
        "Literal PCM",
        AudioBuffer { sample_rate: 48_000, channels: (0..2).map(|channel| (0..97).map(|frame| sample(channel, frame)).collect()).collect() },
        None,
        FileFormat::Wav,
    );
    let id = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "Literal PCM", source, 0, 0, 97));
    let mut engine = Engine::new(session);
    engine.execute("track.new", &json!({"name":"Preserved Redo"})).unwrap();
    assert!(engine.undo());
    assert!(engine.can_redo());
    engine
}

struct Document {
    arc: Arc<Session>,
    revision: u64,
    dirty: bool,
    path: Option<String>,
    history: Vec<String>,
    redo: Option<String>,
    journal: Vec<(String, Value)>,
    position: i64,
    generation: u64,
}
impl Document {
    fn capture(e: &Engine) -> Self {
        Self {
            arc: e.session_arc(),
            revision: e.revision,
            dirty: e.is_dirty(),
            path: e.path.clone(),
            history: e.undo_history(),
            redo: e.redo_label().map(str::to_owned),
            journal: e.journal.clone(),
            position: e.transport.position,
            generation: e.session_generation(),
        }
    }
    fn assert_unchanged(&self, e: &Engine) {
        assert!(Arc::ptr_eq(&self.arc, &e.session_arc()));
        assert_eq!(
            (
                e.revision,
                e.is_dirty(),
                &e.path,
                e.undo_history(),
                e.redo_label().map(str::to_owned),
                &e.journal,
                e.transport.position,
                e.session_generation()
            ),
            (self.revision, self.dirty, &self.path, self.history.clone(), self.redo.clone(), &self.journal, self.position, self.generation)
        );
    }
}

fn harness(path: &Path, render: bool) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(engine(), None, Services::default());
    menus::invoke_menu(&mut app, "file.bounce_mix", "File > Bounce Mix...");
    match app.dialogs.open.as_mut().unwrap() {
        Dialog::Bounce { path: destination, bit_depth, .. } => {
            *destination = path.to_string_lossy().into_owned();
            *bit_depth = "32f".into();
        }
        _ => panic!("Bounce dialog required"),
    }
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
fn shot(h: &mut Harness<'_, SoundApp>, name: &str) {
    if let Some(dir) = std::env::var_os("SOUNDCRAFT_BOUNCE_DELIVERY_SHOT_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render().unwrap().save(PathBuf::from(dir).join(name)).unwrap();
    }
}

#[test]
fn actual_dialog_controls_deliver_exact_selected_pcm_and_silent_source_cutoff_tail() {
    let dir = Directory::new();
    let mut h = harness(&dir.output(), std::env::var_os("SOUNDCRAFT_BOUNCE_DELIVERY_SHOT_DIR").is_some());
    assert!(matches!(h.state().dialogs.open, Some(Dialog::Bounce { include_preroll: true, tail_seconds: 0.0, sample_rate: 48_000, .. })));
    h.get_by_label("Render history before selection").click();
    h.run_steps(4);
    assert!(matches!(h.state().dialogs.open, Some(Dialog::Bounce { include_preroll: false, .. })));
    h.get_by_label("Effect tail").click();
    h.run_steps(2);
    h.get_by_label("Effect tail").type_text("0.001");
    h.run_steps(4);
    assert!(matches!(h.state().dialogs.open, Some(Dialog::Bounce { tail_seconds, .. }) if (tail_seconds-0.001).abs() < 1e-12));
    let document = Document::capture(&h.state().engine);
    shot(&mut h, "bounce-ready.png");
    h.get_by_label("Bounce").click();
    h.run_steps(4);
    assert!(h.state().dialogs.open.is_none(), "{}", h.state().ui.status);
    document.assert_unchanged(&h.state().engine);
    let (_, audio) = soundcraft_audio_io::decode(&std::fs::read(dir.output()).unwrap(), Some("wav")).unwrap();
    assert_eq!((audio.sample_rate, audio.channels.len(), audio.frames()), (48_000, 2, 117));
    for channel in 0..2 {
        for frame in 0..69 {
            assert_eq!(audio.channels[channel][frame].to_bits(), sample(channel, 13 + frame).to_bits());
        }
        assert!(audio.channels[channel][69..].iter().all(|v| *v == 0.0), "tail must not admit later source audio");
    }
    assert!(h.state().ui.status.contains("117 frames"));
    assert!(h.state_mut().engine.redo());
    assert!(h.state().engine.session().track_by_name("Preserved Redo").is_some());
}

#[test]
fn collision_keeps_controls_and_existing_bytes_then_retry_delivers_to_a_new_file() {
    let dir = Directory::new();
    let output = dir.output();
    let existing = b"This is an existing user's file, not an export placeholder.";
    std::fs::write(&output, existing).unwrap();
    let mut h = harness(&output, std::env::var_os("SOUNDCRAFT_BOUNCE_DELIVERY_SHOT_DIR").is_some());
    let document = Document::capture(&h.state().engine);
    h.get_by_label("Bounce").click();
    h.run_steps(4);
    assert_eq!(std::fs::read(&output).unwrap(), existing);
    document.assert_unchanged(&h.state().engine);
    assert!(
        matches!(h.state().dialogs.open, Some(Dialog::Bounce { ref error, bit_depth: ref depth, include_preroll: true, .. }) if error.contains("already exists") && depth == "32f")
    );
    shot(&mut h, "bounce-collision.png");
    let retry = dir.0.join("Retry.wav");
    if let Some(Dialog::Bounce { path, .. }) = &mut h.state_mut().dialogs.open {
        *path = retry.to_string_lossy().into_owned();
    }
    h.get_by_label("Bounce").click();
    h.run_steps(4);
    assert!(h.state().dialogs.open.is_none(), "{}", h.state().ui.status);
    document.assert_unchanged(&h.state().engine);
    let (_, audio) = soundcraft_audio_io::decode(&std::fs::read(&retry).unwrap(), Some("wav")).unwrap();
    assert_eq!((audio.sample_rate, audio.channels.len(), audio.frames()), (48_000, 2, 69));
    assert_eq!(std::fs::read(&output).unwrap(), existing);
}

#[test]
fn escape_and_clicked_cancel_suppress_same_frame_enter_without_creating_a_file() {
    for clicked in [false, true] {
        let dir = Directory::new();
        let mut h = harness(&dir.output(), false);
        let document = Document::capture(&h.state().engine);
        if clicked {
            let pos = h.get_by_label("Cancel").rect().center();
            h.input_mut().events.extend([
                Event::PointerMoved(pos),
                Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
                Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
            ]);
        } else {
            h.input_mut().events.push(Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
        }
        h.input_mut().events.push(Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
        h.run_steps(4);
        assert!(h.state().dialogs.open.is_none());
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
        document.assert_unchanged(&h.state().engine);
        assert!(h.state_mut().engine.redo());
    }
}

#[test]
fn oversize_tail_is_refused_without_writing_and_keeps_the_dialog_for_correction() {
    let dir = Directory::new();
    let mut h = harness(&dir.output(), false);
    if let Some(Dialog::Bounce { tail_seconds, .. }) = &mut h.state_mut().dialogs.open {
        *tail_seconds = 14_400.0;
    }
    let document = Document::capture(&h.state().engine);
    h.get_by_label("Bounce").click();
    h.run_steps(4);
    assert!(matches!(h.state().dialogs.open, Some(Dialog::Bounce { ref error, .. }) if !error.is_empty()));
    assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
    document.assert_unchanged(&h.state().engine);
}

#[test]
fn direct_bounce_reports_stored_state_and_checks_original_revision_before_any_delivery() {
    let dir = Directory::new();
    let mut app = SoundApp::new(engine(), None, Services::default());
    let document = Document::capture(&app.engine);
    let wrong = app.run("file.bounce_mix", json!({"path":dir.output(), "expected_revision":app.engine.revision+1}));
    assert!(wrong.unwrap_err().contains("session changed"));
    assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
    document.assert_unchanged(&app.engine);
    let receipt =
        app.run("file.bounce_mix", json!({"path":dir.output(), "bit_depth":"32f", "dither":false, "expected_revision":app.engine.revision})).unwrap();
    assert_eq!(
        (receipt["frames"].as_u64(), receipt["sample_rate"].as_u64(), receipt["plugin_state"].as_str()),
        (Some(69), Some(48_000), Some("stored_document"))
    );
    assert_eq!(receipt["delivery"], "native_file_synced");
    assert_eq!(receipt["publication"]["complete"], true);
    assert!(app.dialogs.open.is_none());
    document.assert_unchanged(&app.engine);
}

#[test]
fn replacing_a_document_closes_the_previous_bounce_dialog_without_exporting() {
    for direct in [false, true] {
        let dir = Directory::new();
        let mut h = harness(&dir.output(), false);
        if direct {
            h.state_mut().engine.replace_session(Session::default());
        } else {
            h.state_mut().run("session.new", json!({"name":"Replacement"})).unwrap();
        }
        h.run_steps(4);
        assert!(h.state().dialogs.open.is_none());
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
        assert!(!h.state().engine.can_redo());
    }
}
