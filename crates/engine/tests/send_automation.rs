//! Existing commands/history and saved session PCM for bounded send automation.
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::{Engine, io};
use soundcraft_model::{AutoParam, ChannelFormat, Clip, Route, Session, TrackKind};
use soundcraft_time::Range;
use std::sync::Arc;

fn fixture() -> Engine {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Original"));
    let buffer = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 512]] };
    let src = io::add_source(&mut s, "Original synthetic", buffer, None, FileFormat::Wav);
    let cid = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "Original", src, 0, 0, 512));
    s.track_mut(t).unwrap().mixer.output = Route::None;
    let mut e = Engine::new(s);
    e.execute("mix.send", &json!({"track":"Original","bus":"Send","slot":"A","level_db":0})).unwrap();
    e.execute("track.new", &json!({"name":"Return","kind":"aux","format":"stereo"})).unwrap();
    let bus = e.session().busses.iter().find(|b| b.name == "Send").unwrap().id;
    let aux = e.session().tracks.iter().find(|t| t.kind == TrackKind::Aux).unwrap().id;
    e.session_mut().track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
    e.mark_clean();
    e
}
fn render(e: &Engine) -> Vec<Vec<f32>> {
    let out = soundcraft_mix::render_range(e.session(), Range::new(0, 512), 37);
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|c| c.len() == 512));
    out
}
fn point(e: &mut Engine, param: &str, at: i64, value: f32) {
    e.execute("automation.set_point", &json!({"track":"Original","param":param,"at":at,"value":value})).unwrap();
}
#[test]
fn commands_undo_redo_reuse_renderer_and_preserve_render_state() {
    let mut e = fixture();
    let baseline = render(&e);
    point(&mut e, "send_a_pan", 0, -1.0);
    point(&mut e, "send_a_pan", 173, 1.0);
    point(&mut e, "send_a_mute", 0, 0.0);
    let before = render(&e);
    point(&mut e, "send_a_mute", 97, 1.0);
    let after = render(&e);
    assert_ne!(before, after);
    assert_ne!(baseline, before);
    let arc = e.session_arc();
    let serialized = e.session().to_json().unwrap();
    let revision = e.revision;
    let history = e.undo_history();
    let dirty = e.is_dirty();
    let mut mix = soundcraft_mix::MixEngine::new(48_000.0, 512);
    let mut out = vec![vec![0.0; 512]; 2];
    mix.render(e.session(), 0, 512, &mut out);
    assert_eq!(out, after);
    mix.render(e.session(), 97, 13, &mut out);
    assert!(Arc::ptr_eq(&arc, &e.session_arc()));
    assert_eq!(serialized, e.session().to_json().unwrap());
    assert_eq!(revision, e.revision);
    assert_eq!(history, e.undo_history());
    assert_eq!(dirty, e.is_dirty());
    assert!(e.undo());
    assert_eq!(render(&e), before);
    mix.render(e.session(), 0, 512, &mut out);
    assert_eq!(out, before);
    assert!(e.redo());
    assert_eq!(render(&e), after);
    mix.render(e.session(), 0, 512, &mut out);
    assert_eq!(out, after);
    e.execute("mix.automation_mode", &json!({"track":"Original","mode":"off"})).unwrap();
    assert_eq!(render(&e), baseline);
}
#[test]
fn lane_commands_survive_folder_and_bundle_save_reopen_with_exact_pcm() {
    let mut e = fixture();
    e.execute("mix.send_level", &json!({"track":"Original","slot":"A","pan":0.5,"mute":true})).unwrap();
    point(&mut e, "send_a_pan", 37, -1.0);
    point(&mut e, "send_a_pan", 401, 1.0);
    point(&mut e, "send_a_mute", 0, 0.0);
    e.execute("automation.write_range", &json!({"track":"Original","param":"send_a_mute","start":97,"end":173,"value":1.0})).unwrap();
    let expected = render(&e);
    assert!(expected.iter().flatten().any(|v| v.abs() > 0.05));
    let dir = std::env::temp_dir().join(format!("soundcraft-send-automation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("Original/Send.scraft").to_string_lossy().into_owned();
    io::save_session(&mut e, &path).unwrap();
    let mut reopened = Engine::default();
    assert!(io::open_session(&mut reopened, &path).unwrap().is_empty());
    assert_eq!(render(&reopened), expected);
    let tr = reopened.session().tracks.iter().find(|t| t.name == "Original").unwrap();
    assert_eq!(tr.lane(&AutoParam::SendPan(0)).unwrap().points.len(), 2);
    assert!(!reopened.is_dirty());
    assert!(!reopened.can_undo());
    let (bundle, _) = io::session_bundle(e.session()).unwrap();
    let mut bundled = Engine::default();
    io::open_session_bytes(&mut bundled, "Send.scraftz", &bundle).unwrap();
    assert_eq!(render(&bundled), expected);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn absent_enabled_field_keeps_legacy_reader_behavior() {
    let mut e = fixture();
    point(&mut e, "send_a_pan", 0, -1.0);
    point(&mut e, "send_a_mute", 0, 0.0);
    let expected = render(&e);
    assert!(expected.iter().flatten().any(|v| v.abs() > 0.05));
    let mut json = serde_json::to_value(e.session()).unwrap();
    for track in json["tracks"].as_array_mut().unwrap() {
        for lane in track["automation"].as_array_mut().unwrap() {
            lane.as_object_mut().unwrap().remove("enabled");
        }
    }
    let mut old: Session = serde_json::from_value(json).unwrap();
    old.pool = e.session().pool.clone();
    let tr = old.tracks.iter().find(|t| t.name == "Original").unwrap();
    assert!(tr.automation.iter().all(|l| !l.enabled));
    assert_eq!(soundcraft_mix::render_range(&old, Range::new(0, 512), 100), expected);
}

#[test]
fn repeated_send_curves_preserve_outside_routing_and_strict_copy_pcm() {
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let temp = Temp(std::env::temp_dir().join(format!("soundcraft-repeat-send-copy-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&temp.0);
    std::fs::create_dir_all(&temp.0).unwrap();
    let mut e = fixture();
    let track = e.session().tracks.iter().find(|t| t.name == "Original").unwrap().id;
    let source = e.session().sources[0].id;
    let original = e.session().track(track).unwrap().clips()[0].id;
    {
        let s = e.session_mut();
        s.track_mut(track).unwrap().playlist_mut().unwrap().clips[0].start = 64;
        let before = s.new_clip_id();
        let after = s.new_clip_id();
        let clips = &mut s.track_mut(track).unwrap().playlist_mut().unwrap().clips;
        clips.extend([Clip::audio(before, "Before", source, 16, 8, 16), Clip::audio(after, "After", source, 32, 1200, 16)]);
        clips.sort_by_key(|c| c.start);
        s.edit.selected_tracks = vec![track];
        s.edit.selected_clips = vec![original];
        s.edit.selection = Range::new(64, 576);
        s.edit.automation_follows_edit = true;
    }
    e.execute("mix.send_level", &json!({"track":"Original","slot":"A","pan":0.25,"mute":true})).unwrap();
    for (at, value) in [(0, -1.0), (64, -1.0), (205, 1.0), (575, 0.5), (1088, 0.5), (1200, -1.0), (1280, -1.0)] {
        point(&mut e, "send_a_pan", at, value);
    }
    for (at, value) in [(0, 0.0), (149, 1.0), (213, 0.0), (401, 1.0), (470, 0.0), (1088, 0.0), (1210, 1.0), (1230, 0.0)] {
        point(&mut e, "send_a_mute", at, value);
    }
    let before = e.session().clone();
    let outside_before = soundcraft_mix::render_range(&before, Range::new(0, 1280), 37);
    let original_pcm = soundcraft_mix::render_range(&before, Range::new(64, 576), 37);
    assert_eq!((outside_before.len(), outside_before[0].len(), outside_before[1].len()), (2, 1280, 1280));
    assert_eq!((original_pcm.len(), original_pcm[0].len(), original_pcm[1].len()), (2, 512, 512));
    assert!(original_pcm.iter().flatten().any(|v| v.abs() > 0.1));
    assert!(original_pcm.iter().all(|ch| ch[85..149].iter().all(|v| *v == 0.0)));
    let history = e.undo_history();
    e.execute("edit.repeat", &json!({"count":1})).unwrap();
    assert_eq!(e.undo_history().len(), history.len() + 1);
    assert_eq!(e.session().track(track).unwrap().mixer, before.track(track).unwrap().mixer);
    assert_eq!(e.session().busses, before.busses);
    assert_eq!(e.session().tracks.iter().find(|t| t.kind == TrackKind::Aux), before.tracks.iter().find(|t| t.kind == TrackKind::Aux));
    assert_eq!(e.session().edit.selected_clips, vec![original]);
    assert_eq!(soundcraft_mix::render_range(e.session(), Range::new(64, 576), 100), original_pcm);
    assert_eq!(soundcraft_mix::render_range(e.session(), Range::new(576, 1088), 37), original_pcm);
    let expected = soundcraft_mix::render_range(e.session(), Range::new(0, 1280), 37);
    assert_eq!((expected.len(), expected[0].len(), expected[1].len()), (2, 1280, 1280));
    for (now, old) in expected.iter().zip(&outside_before) {
        assert_eq!(now[..64], old[..64]);
        assert_eq!(now[1088..], old[1088..]);
    }
    let (snapshot, revision, dirty, history, journal, clipboard, path) =
        (e.session_arc(), e.revision, e.is_dirty(), e.undo_history(), e.journal.clone(), e.clipboard.clone(), e.path.clone());
    for bundled in [false, true] {
        let mut opened = Engine::default();
        if bundled {
            let (bytes, report) = soundcraft_engine::session_copy::bundle(&e, &temp.0.join("Copy.scraftz").to_string_lossy()).unwrap();
            assert!(io::open_session_bytes(&mut opened, &report.output_path, &bytes).unwrap().is_empty());
        } else {
            let copy = temp.0.join("native/Copy.scraft");
            e.execute("session.save_copy", &json!({"path":copy,"expected_revision":revision})).unwrap();
            assert!(io::open_session(&mut opened, &copy.to_string_lossy()).unwrap().is_empty());
        }
        assert_eq!(opened.session().track(track).unwrap().automation, snapshot.track(track).unwrap().automation);
        assert_eq!(opened.session().track(track).unwrap().mixer, snapshot.track(track).unwrap().mixer);
        assert_eq!(soundcraft_mix::render_range(opened.session(), Range::new(0, 1280), 64), expected);
        assert_eq!(soundcraft_mix::render_range(opened.session(), Range::new(576, 1088), 100), original_pcm);
    }
    assert!(Arc::ptr_eq(&snapshot, &e.session_arc()));
    assert_eq!(
        (e.revision, e.is_dirty(), e.undo_history(), e.journal.clone(), e.clipboard.clone(), e.path.clone()),
        (revision, dirty, history, journal, clipboard, path)
    );
}
