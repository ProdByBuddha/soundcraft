//! Actual seven-family commands, atomicity, Undo/Redo and native persistence.
#[path = "../../mix/tests/support/automation_reference.rs"]
mod reference;
use reference::*;
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::{Engine, io};
use soundcraft_mix::MixEngine;
use soundcraft_model::{AutoParam, ChannelFormat, Clip, Route, Session, TrackId, TrackKind};
use std::sync::Arc;

fn fixture() -> (Engine, TrackId) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original seven families"));
    let channels = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) as f32).collect()).collect();
    let source =
        io::add_source(&mut session, "Original binary-fraction source", AudioBuffer { sample_rate: 48_000, channels }, None, FileFormat::Wav);
    let clip = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", source, 0, 0, LEN as i64));
    session.track_mut(track).unwrap().mixer.pan = vec![-1.0, 1.0];
    session.edit.selected_tracks = vec![track];
    session.edit.selected_clips = vec![clip];
    let mut engine = Engine::new(session);
    engine.execute("mix.insert", &json!({"track":track.0,"slot":"A","plugin":"gain","params":{"gain":0}})).unwrap();
    engine.execute("mix.send", &json!({"track":track.0,"slot":"A","bus":"Original return","level_db":0})).unwrap();
    let created = engine.execute("track.new", &json!({"kind":"aux","format":"stereo","name":"Original audible return"})).unwrap();
    let aux = TrackId(created["tracks"][0].as_u64().unwrap());
    let bus = engine.session().bus_by_name("Original return").unwrap().id;
    let return_track = engine.session_mut().track_mut(aux).unwrap();
    return_track.mixer.input = Route::Bus(bus);
    return_track.mixer.pan = vec![-1.0, 1.0];
    engine.execute("track.comments", &json!({"track":track.0,"comments":"Original seven curves\n識別 by stable ID"})).unwrap();
    engine.mark_clean();
    (engine, track)
}
fn curves() -> [(&'static str, AutoParam, &'static [(i64, f64)]); 8] {
    [
        ("volume", AutoParam::Volume, VOLUME),
        ("pan", AutoParam::Pan(0), PAN0),
        ("pan2", AutoParam::Pan(1), PAN1),
        ("mute", AutoParam::Mute, MUTE),
        ("send_a_level", AutoParam::SendLevel(0), SEND_LEVEL),
        ("send_a_pan", AutoParam::SendPan(0), SEND_PAN),
        ("send_a_mute", AutoParam::SendMute(0), SEND_MUTE),
        ("plugin:a:gain", AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN),
    ]
}
fn set_all(engine: &mut Engine, track: TrackId) {
    for (id, _, points) in curves() {
        for &(at, value) in points {
            engine.execute("automation.set_point", &json!({"track":track.0,"param":id,"at":at,"value":value})).unwrap();
        }
    }
}
fn render(engine: &Engine) -> Vec<Vec<f32>> {
    let mut mixer = MixEngine::new(48_000.0, 512);
    let mut result = vec![Vec::new(); 2];
    let mut at = 0;
    while at < LEN {
        let count = 37.min(LEN - at);
        let mut block = vec![vec![0.0; count]; 2];
        mixer.render(engine.session(), at as i64, count, &mut block);
        assert_eq!(block.len(), 2);
        assert!(block.iter().all(|ch| ch.len() == count));
        for (out, block) in result.iter_mut().zip(block) {
            out.extend(block);
        }
        at += count;
    }
    assert!(result.iter().flatten().any(|x| x.abs() > 0.02));
    result
}
fn expected() -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; LEN]; 2];
    let mut gain = GainMemory::new();
    gain.reset(); // A fresh prepared default Gain starts at its initial target, one.
    for at in 0..LEN {
        let plugin = gain.next(linear(PLUGIN_GAIN, at as i64));
        let volume = db_gain(linear(VOLUME, at as i64), -144.0);
        let safety = if step(MUTE, at as i64) { 0.0 } else { 1.0 };
        let x = [signal(0, at) * plugin * volume * safety, signal(1, at) * plugin * volume * safety];
        let p0 = pan(linear(PAN0, at as i64), None);
        let p1 = pan(linear(PAN1, at as i64), None);
        let balance = pan(linear(SEND_PAN, at as i64), None);
        let send = if step(SEND_MUTE, at as i64) { 0.0 } else { db_gain(linear(SEND_LEVEL, at as i64), -144.0) };
        for (channel, values) in out.iter_mut().enumerate() {
            // Direct stereo panners plus an independently balanced post-fader send/Aux return.
            values[at] = x[0] * p0[channel] + x[1] * p1[channel] + x[channel] * balance[channel] * send;
        }
    }
    out
}

#[test]
fn all_seven_point_commands_undo_redo_and_render_against_independent_pcm() {
    let (mut engine, track) = fixture();
    let before = engine.session().to_json().unwrap();
    let baseline_history = engine.undo_history().len();
    let point_count = curves().iter().map(|(_, _, points)| points.len()).sum::<usize>();
    set_all(&mut engine, track);
    assert_eq!(engine.undo_history().len(), baseline_history + point_count);
    let after = engine.session().to_json().unwrap();
    let snapshot = engine.session_arc();
    let state = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.clipboard.clone(), engine.path.clone());
    for _ in 0..point_count {
        assert!(engine.undo());
    }
    assert_eq!(engine.session().to_json().unwrap(), before);
    assert!(!engine.is_dirty());
    for _ in 0..point_count {
        assert!(engine.redo());
    }
    assert_eq!(engine.session().to_json().unwrap(), after);
    let reference = expected();
    let static_wrong = (0..2).map(|ch| (0..LEN).map(|at| 2.0 * signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    different(&reference, &static_wrong, 0.05);
    let render_snapshot = engine.session_arc();
    let render_state =
        (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.clipboard.clone(), engine.path.clone());
    check(&render(&engine), &reference, 2, LEN, 3e-5);
    assert!(Arc::ptr_eq(&render_snapshot, &engine.session_arc()));
    assert_eq!(
        render_state,
        (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.clipboard.clone(), engine.path.clone())
    );
    assert_eq!(engine.session().edit.selected_clips, snapshot.edit.selected_clips);
    assert_eq!(engine.session().edit.selected_tracks, snapshot.edit.selected_tracks);
    assert_eq!(engine.journal, state.3);
}

#[test]
fn all_seven_write_range_commands_round_trip_exact_lanes_and_history() {
    let (mut engine, track) = fixture();
    set_all(&mut engine, track);
    for (id, param, points) in curves() {
        let before = engine.session().to_json().unwrap();
        let history = engine.undo_history().len();
        let value = points[points.len() - 1].1;
        engine.execute("automation.write_range", &json!({"track":track.0,"param":id,"start":113,"end":149,"value":value})).unwrap();
        let after = engine.session().to_json().unwrap();
        let lane = engine.session().track(track).unwrap().lane(&param).unwrap();
        assert!(lane.points.iter().any(|p| p.at == 113 && f64::from(p.value) == value));
        assert!(lane.points.iter().any(|p| p.at == 149 && f64::from(p.value) == value));
        assert_eq!(engine.undo_history().len(), history + 1);
        assert!(engine.undo());
        assert_eq!(engine.session().to_json().unwrap(), before);
        assert!(engine.redo());
        assert_eq!(engine.session().to_json().unwrap(), after);
        assert!(engine.undo());
    }
}

#[test]
fn mixed_invalid_plugin_target_rejects_without_consuming_seeded_redo_or_mutating_state() {
    let (mut engine, track) = fixture();
    let other = engine.execute("track.new", &json!({"kind":"audio","format":"stereo","name":"Original empty insert"})).unwrap();
    let other = TrackId(other["tracks"][0].as_u64().unwrap());
    engine.mark_clean();
    engine.execute("track.comments", &json!({"track":track.0,"comments":"Seeded usable Redo"})).unwrap();
    let redo_document = engine.session().to_json().unwrap();
    assert!(engine.undo());
    let snapshot = engine.session_arc();
    let document = engine.session().to_json().unwrap();
    let state = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.clipboard.clone(), engine.path.clone());
    let result = engine.execute("automation.set_point", &json!({"tracks":[track.0,other.0],"param":"plugin:a:gain","at":37,"value":6}));
    assert!(result.is_err(), "one target has no populated insert A: {result:?}");
    assert!(Arc::ptr_eq(&snapshot, &engine.session_arc()));
    assert_eq!(document, engine.session().to_json().unwrap());
    assert_eq!(
        state,
        (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.clipboard.clone(), engine.path.clone())
    );
    assert!(engine.redo());
    assert_eq!(engine.session().to_json().unwrap(), redo_document);
}

#[test]
fn native_save_open_preserves_all_seven_curves_ids_source_and_fresh_reference_pcm() {
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("soundcraft-seven-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let owned = Temp(dir);
    let (mut engine, track) = fixture();
    set_all(&mut engine, track);
    engine.execute("track.rename", &json!({"track":track.0,"name":"Original 七 families"})).unwrap();
    let path = owned.0.join("Original/Seven.scraft");
    io::save_session(&mut engine, &path.to_string_lossy()).unwrap();
    let saved = engine.session_arc();
    let mut reopened = Engine::default();
    assert!(io::open_session(&mut reopened, &path.to_string_lossy()).unwrap().is_empty());
    assert_eq!(reopened.session().track(track), saved.track(track));
    assert_eq!(reopened.session().sources, saved.sources);
    assert_eq!(reopened.session().outputs, saved.outputs);
    assert_eq!(reopened.session().busses, saved.busses);
    assert!(!reopened.is_dirty());
    assert!(!reopened.can_undo());
    let original_source = saved.sources[0].id;
    let loaded = reopened.session().pool.get(original_source).unwrap();
    let original = saved.pool.get(original_source).unwrap();
    assert_eq!(loaded.buffer, original.buffer);
    for (_, param, points) in curves() {
        let lane = reopened.session().track(track).unwrap().lane(&param).unwrap();
        assert_eq!(lane.points.len(), points.len());
        for (actual, &(at, value)) in lane.points.iter().zip(points) {
            assert_eq!(actual.at, at);
            assert_eq!(f64::from(actual.value), value);
        }
    }
    check(&render(&reopened), &expected(), 2, LEN, 3e-5);
    assert!(Arc::ptr_eq(&saved, &engine.session_arc()));
}
