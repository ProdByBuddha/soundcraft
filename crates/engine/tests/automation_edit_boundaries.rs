//! Actual editing and Track Bounce commands against independently calculated automation PCM.
#![cfg(not(target_arch = "wasm32"))]

use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_dsp::pan::PanLaw;
use soundcraft_engine::{Engine, io};
use soundcraft_mix::OfflineRenderer;
use soundcraft_model::{AutoParam, ChannelFormat, Clip, ClipContent, EditMode, Session, SourceId, TrackId, TrackKind};
use soundcraft_time::Range;
use std::sync::Arc;

#[allow(dead_code)]
#[path = "../../mix/tests/support/automation_reference.rs"]
mod reference;
use reference::*;

const INVERT: &[(i64, f64)] =
    &[(0, 0.0), (16, 0.25), (32, 0.5), (48, 0.75), (64, 1.0), (80, 0.75), (96, 0.5), (112, 0.25), (127, 0.0), (128, 0.0), (256, 0.0), (388, 0.0)];
// Current write_range preserves value_at(end) in the point at end+1. These dyadic
// expectations are declared independently; no production value_at produces the oracle.
const WRITTEN: &[(i64, f64)] = &[(0, 0.0), (16, 0.25), (17, 0.75), (29, 0.75), (30, 29.0 / 64.0), (64, 1.0), (112, 0.25), (127, 0.0), (388, 0.0)];

fn bits(s: &Session, source: SourceId) -> Vec<Vec<u32>> {
    s.pool.get(source).unwrap().buffer.channels.iter().map(|ch| ch.iter().map(|x| x.to_bits()).collect()).collect()
}

fn points(engine: &mut Engine, track: TrackId, param: &str, curve: &[(i64, f64)]) {
    for &(at, value) in curve {
        engine.execute("automation.set_point", &json!({"track":track.0,"param":param,"at":at,"value":value})).unwrap();
    }
}

fn prepared_render(snapshot: Arc<Session>) -> Vec<Vec<f32>> {
    let mut renderer = OfflineRenderer::prepare(snapshot, 512).unwrap();
    renderer.mix_mut().pan_law = PanLaw::Zero;
    let mut result = vec![Vec::new(); 2];
    let mut at = 0;
    for count in [1, 7, 37, 64, 100].into_iter().cycle() {
        if at == LEN {
            break;
        }
        let n = count.min(LEN - at);
        let mut block = vec![vec![0.0; n]; 2];
        renderer.render(at as i64, n, &mut block).unwrap();
        assert_eq!(block.len(), 2);
        for (channel, samples) in result.iter_mut().zip(block) {
            assert_eq!(samples.len(), n);
            channel.extend(samples);
        }
        at += n;
    }
    result
}

fn trim_expectation(curve: &[(i64, f64)], repeated: bool, held_wrong: bool) -> Vec<Vec<f64>> {
    let mut current = 1.0;
    let coefficient = (-1.0f64 / (0.010 * 48_000.0)).exp();
    let mut result = vec![vec![0.0; LEN]; 2];
    for at in 0..LEN {
        let local = if repeated && (128..256).contains(&at) { at - 128 } else { at } as i64;
        let inverted = if held_wrong { step(curve, local) } else { linear(curve, local).round() >= 1.0 };
        let target = if inverted { -1.0 } else { 1.0 };
        if current != target {
            current = target + (current - target) * coefficient;
            if (current - target).abs() <= 1e-6 * (1.0 + target.abs()) {
                current = target;
            }
        }
        // Before Repeat, the destination is a real silent gap. Trim still advances its
        // internal smoothing there; repeating fills the gap with the original raw PCM.
        let audible = repeated || !(128..256).contains(&at);
        for (ch, channel) in result.iter_mut().enumerate() {
            channel[at] = if audible { [0.25, -0.125][ch] * current } else { 0.0 };
        }
    }
    result
}

fn assert_render_immutable(engine: &Engine, source: SourceId, expected: &[Vec<f64>]) {
    let before = engine.session_arc();
    let json = before.to_json().unwrap();
    let state = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone());
    let raw = bits(&before, source);
    check(&prepared_render(before.clone()), expected, 2, LEN, 2e-5);
    assert!(Arc::ptr_eq(&before, &engine.session_arc()));
    assert_eq!(engine.session().to_json().unwrap(), json);
    assert_eq!((engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone()), state);
    assert_eq!(bits(engine.session(), source), raw);
}

fn roundtrip_history(engine: &mut Engine, before: &Arc<Session>, before_history: usize) {
    let after = engine.session().to_json().unwrap();
    assert_eq!(engine.undo_history().len(), before_history + 1);
    assert!(engine.undo());
    assert_eq!(engine.undo_history().len(), before_history);
    assert_eq!(engine.session().to_json().unwrap(), before.to_json().unwrap());
    assert!(engine.redo());
    assert_eq!(engine.undo_history().len(), before_history + 1);
    assert_eq!(engine.session().to_json().unwrap(), after);
}

#[test]
fn actual_thin_write_and_repeat_preserve_plain_toggle_curves_source_and_undo_redo_pcm() {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original edited Toggle"));
    let source = io::add_source(
        &mut session,
        "Original DC, no proprietary media",
        AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; LEN], vec![-0.125; LEN]] },
        None,
        FileFormat::Wav,
    );
    let left = session.new_clip_id();
    let outside = session.new_clip_id();
    let t = session.track_mut(track).unwrap();
    t.mixer.pan = vec![-1.0, 1.0];
    t.playlist_mut().unwrap().clips =
        vec![Clip::audio(left, "Original selected", source, 0, 0, 128), Clip::audio(outside, "Original outside", source, 0, 256, (LEN - 256) as i64)];
    session.edit.selected_tracks = vec![track];
    session.edit.selected_clips = vec![left];
    session.edit.selection = Range::new(0, 128);
    session.edit.edit_mode = EditMode::Slip;
    session.edit.automation_follows_edit = true;
    let mut engine = Engine::new(session);
    engine.execute("track.comments", &json!({"track":track.0,"comments":"Original edited Toggle\n識別"})).unwrap();
    engine.execute("mix.insert", &json!({"track":track.0,"slot":"A","plugin":"trim","params":{"gain":0,"invert":0}})).unwrap();
    points(&mut engine, track, "plugin:a:invert", INVERT);
    let original = engine.session_arc();
    let original_json = original.to_json().unwrap();
    let original_bits = bits(&original, source);
    let original_audio = original.pool.get(source).unwrap().clone();
    let original_outside = original.track(track).unwrap().clips().iter().find(|clip| clip.id == outside).unwrap().clone();
    let plain_expected = trim_expectation(INVERT, false, false);
    different(&plain_expected, &trim_expectation(INVERT, false, true), 0.01);
    assert_render_immutable(&engine, source, &plain_expected);

    let thin_before = engine.session_arc();
    let thin_history = engine.undo_history().len();
    let thinned = engine.execute("automation.thin", &json!({"tracks":[track.0],"tolerance":0})).unwrap();
    assert_eq!(thinned["removed"], 7);
    let invert_param = AutoParam::Plugin { slot: 0, param: "invert".into() };
    let thinned_points = engine.session().track(track).unwrap().lane(&invert_param).unwrap().points.clone();
    assert_eq!(
        thinned_points.iter().map(|p| (p.at, f64::from(p.value))).collect::<Vec<_>>(),
        [(0, 0.0), (64, 1.0), (112, 0.25), (127, 0.0), (388, 0.0)]
    );
    roundtrip_history(&mut engine, &thin_before, thin_history);
    assert_render_immutable(&engine, source, &plain_expected);

    let write_before = engine.session_arc();
    let write_history = engine.undo_history().len();
    engine.execute("automation.write_range", &json!({"track":track.0,"param":"plugin:a:invert","start":17,"end":29,"value":0.75})).unwrap();
    let written_points = engine.session().track(track).unwrap().lane(&invert_param).unwrap().points.clone();
    assert_eq!(written_points.iter().map(|p| (p.at, f64::from(p.value))).collect::<Vec<_>>(), WRITTEN);
    roundtrip_history(&mut engine, &write_before, write_history);
    let written_expected = trim_expectation(WRITTEN, false, false);
    different(&written_expected, &plain_expected, 0.005);
    assert_render_immutable(&engine, source, &written_expected);

    let repeat_before = engine.session_arc();
    let repeat_json = repeat_before.to_json().unwrap();
    let repeat_history = engine.undo_history().len();
    engine.execute("edit.repeat", &json!({"count":1})).unwrap();
    assert_eq!(engine.session().edit.selection, Range::new(0, 128));
    let t = engine.session().track(track).unwrap();
    assert_eq!(t.clips().len(), 3);
    let copied = t.clips().iter().find(|clip| clip.start == 128).unwrap();
    assert_ne!(copied.id, left);
    assert_ne!(copied.id, outside);
    assert_eq!((copied.length, &copied.content), (128, &ClipContent::Audio { source, offset: 0 }));
    assert_eq!(t.clips().iter().find(|clip| clip.id == outside), Some(&original_outside));
    assert_eq!(t.comments, original.track(track).unwrap().comments);
    let repeated_expected = trim_expectation(WRITTEN, true, false);
    different(&repeated_expected, &trim_expectation(WRITTEN, true, true), 0.01);
    assert_render_immutable(&engine, source, &repeated_expected);
    let repeated_json = engine.session().to_json().unwrap();
    assert_eq!(engine.undo_history().len(), repeat_history + 1);
    assert!(engine.undo());
    assert_eq!(engine.session().to_json().unwrap(), repeat_json);
    assert_render_immutable(&engine, source, &written_expected);
    assert!(engine.redo());
    assert_eq!(engine.session().to_json().unwrap(), repeated_json);
    assert_render_immutable(&engine, source, &repeated_expected);
    assert!(Arc::ptr_eq(engine.session().pool.get(source).unwrap(), &original_audio));
    assert_eq!(bits(engine.session(), source), original_bits);
    assert_eq!(original.to_json().unwrap(), original_json);
    assert_eq!(repeat_before.to_json().unwrap(), repeat_json);
    assert_eq!(bits(&original, source), original_bits);
}

#[cfg(unix)]
#[test]
fn actual_track_bounce_volume_bypass_omits_fader_vca_and_legacy_trim_but_retains_mute_safety() {
    struct OwnedDirectory(std::path::PathBuf);
    impl Drop for OwnedDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let directory = OwnedDirectory(std::env::temp_dir().join(format!("soundcraft-automation-bounce-{}-{nonce}", std::process::id())));
    std::fs::create_dir(&directory.0).unwrap();
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original isolated automated strip"));
    let vca = session.add_track(TrackKind::Vca, ChannelFormat::Mono, Some("Original bounce VCA"));
    let channels = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) as f32).collect()).collect();
    let source =
        io::add_source(&mut session, "Original binary-fraction source", AudioBuffer { sample_rate: 48_000, channels }, None, FileFormat::Wav);
    let clip = session.new_clip_id();
    let t = session.track_mut(track).unwrap();
    t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original bounce", source, 0, 0, LEN as i64));
    t.mixer.trim_db = -3.0;
    t.mixer.vca = Some(vca.0);
    session.edit.selected_tracks = vec![track];
    let mut engine = Engine::new(session);
    let vca_volume = [(0, -3.0), (94, 3.0), (211, -6.0), (367, 0.0)];
    let trim = [(0, -3.0), (94, 0.0), (211, 3.0), (367, 0.0)];
    let vca_mute = [(0, 0.0), (97, 1.0), (113, 0.0), (249, 1.0), (267, 0.0)];
    points(&mut engine, track, "volume", VOLUME);
    points(&mut engine, track, "mute", MUTE);
    points(&mut engine, track, "plugin:255:trim", &trim);
    points(&mut engine, vca, "volume", &vca_volume);
    points(&mut engine, vca, "mute", &vca_mute);
    let original = engine.session_arc();
    let original_json = original.to_json().unwrap();
    let original_bits = bits(&original, source);
    let mut references = Vec::new();
    for volume in [false, true] {
        let expected = (0..2)
            .map(|ch| {
                (0..LEN)
                    .map(|at| {
                        if step(MUTE, at as i64) || step(&vca_mute, at as i64) {
                            0.0
                        } else {
                            let fader = if volume {
                                db_gain((linear(VOLUME, at as i64) + linear(&vca_volume, at as i64) + linear(&trim, at as i64)).min(12.0), -144.0)
                            } else {
                                1.0
                            };
                            signal(ch, at) * db_gain(-3.0, -144.0) * fader
                        }
                    })
                    .collect()
            })
            .collect::<Vec<Vec<f64>>>();
        let path = directory.0.join(format!("volume-{volume}.wav"));
        let before = engine.session_arc();
        let state = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone());
        engine
            .execute(
                "track.bounce",
                &json!({"tracks":[track.0],"path":path,"format":"wav","bit_depth":"32f","dither":false,"render_volume":volume,"render_pan":false,"start":0,"end":LEN}),
            )
            .unwrap();
        let (_, decoded) = soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), None).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        check(&decoded.channels, &expected, 2, LEN, 2e-6);
        assert!(Arc::ptr_eq(&before, &engine.session_arc()));
        assert_eq!((engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone()), state);
        assert_eq!(engine.session().to_json().unwrap(), original_json);
        assert_eq!(bits(engine.session(), source), original_bits);
        references.push(expected);
    }
    different(&references[0], &references[1], 0.05);
    engine.execute("mix.mute", &json!({"track":vca.0,"value":true})).unwrap();
    for volume in [false, true] {
        let path = directory.0.join(format!("static-vca-mute-{volume}.wav"));
        let before = engine.session_arc();
        let state = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone());
        engine
            .execute(
                "track.bounce",
                &json!({"tracks":[track.0],"path":path,"bit_depth":"32f","dither":false,"render_volume":volume,"render_pan":false,"start":0,"end":LEN}),
            )
            .unwrap();
        let (_, decoded) = soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), None).unwrap();
        check(&decoded.channels, &vec![vec![0.0; LEN]; 2], 2, LEN, 2e-6);
        assert!(Arc::ptr_eq(&before, &engine.session_arc()));
        assert_eq!((engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone()), state);
        assert_eq!(bits(engine.session(), source), original_bits);
    }
    assert_eq!(original.to_json().unwrap(), original_json);
    assert_eq!(bits(&original, source), original_bits);
}
