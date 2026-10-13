//! Independent prepared-path references for imported boundaries, Master targets and safety.
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::OfflineRenderer;
use soundcraft_model::{AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackId, TrackKind};
use std::sync::Arc;

#[allow(dead_code)]
#[path = "support/automation_reference.rs"]
mod reference;
use reference::*;

fn source() -> (Session, TrackId, SourceId) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original edge source"));
    let id = SourceId(901);
    let channels = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) as f32).collect()).collect();
    session.pool.insert(id, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = session.new_clip_id();
    let t = session.track_mut(track).unwrap();
    t.comments = "Original edges\n識別 by ID".into();
    t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", id, 0, 0, LEN as i64));
    t.mixer.pan = vec![-1.0, 1.0];
    (session, track, id)
}

fn lane(s: &mut Session, t: TrackId, param: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(param);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    s.track_mut(t).unwrap().automation.push(lane);
}

fn render(snapshot: Arc<Session>, schedule: &[usize]) -> Vec<Vec<f32>> {
    let mut renderer = OfflineRenderer::prepare(snapshot, 1_024).unwrap();
    renderer.mix_mut().pan_law = PanLaw::Zero;
    let mut output = vec![Vec::new(); 2];
    let mut at = 0;
    for count in schedule.iter().copied().cycle() {
        if at == LEN {
            break;
        }
        let frames = count.min(LEN - at);
        let mut block = vec![vec![0.0; frames]; 2];
        renderer.render(at as i64, frames, &mut block).unwrap();
        assert_eq!(block.len(), 2);
        for (channel, samples) in output.iter_mut().zip(block) {
            assert_eq!(samples.len(), frames);
            channel.extend(samples);
        }
        at += frames;
    }
    output
}

fn source_bits(s: &Session, source: SourceId) -> Vec<Vec<u32>> {
    s.pool.get(source).unwrap().buffer.channels.iter().map(|channel| channel.iter().map(|x| x.to_bits()).collect()).collect()
}

#[test]
fn imported_unsorted_duplicate_mute_and_future_lanes_render_reopened_immutable_source() {
    let (mut session, track, id) = source();
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    // Stable sorting must retain the later original point at each equal timestamp. The
    // independently declared canonical curve is MUTE, including the first-time duplicates.
    let raw = [(61, 1.0), (13, 0.0), (37, 0.0), (61, 0.0), (37, 1.0), (127, 1.0), (193, 0.0), (13, 1.0), (13, 0.0)];
    lane(&mut session, track, AutoParam::Mute, &raw);
    for param in [
        AutoParam::Pan(2),
        AutoParam::SendLevel(10),
        AutoParam::SendMute(254),
        AutoParam::Plugin { slot: 10, param: "gain".into() },
        AutoParam::Plugin { slot: 0, param: "Gain".into() },
        AutoParam::Plugin { slot: 255, param: "Gain".into() },
    ] {
        lane(&mut session, track, param, &[(0, 1.0), (94, 0.0), (367, 1.0)]);
    }
    let original = Arc::new(session);
    let original_json = original.to_json().unwrap();
    let bits = source_bits(&original, id);
    let raw_points = original.track(track).unwrap().lane(&AutoParam::Mute).unwrap().points.clone();
    let original_audio = original.pool.get(id).unwrap().clone();
    let expected = (0..2).map(|ch| (0..LEN).map(|at| if step(MUTE, at as i64) { 0.0 } else { signal(ch, at) }).collect()).collect::<Vec<Vec<f64>>>();
    let wrong = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    different(&expected, &wrong, 0.1);
    for schedule in [&[1][..], &[7, 37, 64, 100][..], &[1_024][..]] {
        let actual = render(original.clone(), schedule);
        check(&actual, &expected, 2, LEN, 2e-6);
        for at in [0, 12, 13, 36, 37, 60, 61, 126, 127, 192, 193, LEN - 1] {
            for ch in 0..2 {
                assert!((f64::from(actual[ch][at]) - expected[ch][at]).abs() < 2e-6, "exact held endpoint {at}");
            }
        }
    }
    // JSON loading sorts raw points but preserves equal-time order and unknown lanes. Render
    // the actual reopened document, with the same immutable PCM Arc, rather than the old one.
    let mut reopened = Session::from_json(&original_json).unwrap();
    reopened.pool = original.pool.clone();
    let mut sorted = raw_points.clone();
    sorted.sort_by_key(|point| point.at);
    assert_eq!(reopened.track(track).unwrap().lane(&AutoParam::Mute).unwrap().points, sorted);
    assert_eq!(reopened.track(track).unwrap().automation.len(), 7);
    for future in original.track(track).unwrap().automation.iter().skip(1) {
        assert_eq!(reopened.track(track).unwrap().lane(&future.param), Some(future));
    }
    assert_eq!(reopened.track(track).unwrap().comments, original.track(track).unwrap().comments);
    assert_eq!(reopened.track(track).unwrap().clips(), original.track(track).unwrap().clips());
    assert!(Arc::ptr_eq(reopened.pool.get(id).unwrap(), &original_audio));
    let reopened = Arc::new(reopened);
    let reopened_json = reopened.to_json().unwrap();
    check(&render(reopened.clone(), &[37, 1, 64]), &expected, 2, LEN, 2e-6);
    assert_eq!(reopened.to_json().unwrap(), reopened_json);
    assert_eq!(source_bits(&reopened, id), bits);
    assert_eq!(original.to_json().unwrap(), original_json);
    assert_eq!(original.track(track).unwrap().lane(&AutoParam::Mute).unwrap().points, raw_points);
    assert_eq!(source_bits(&original, id), bits);
}

#[test]
fn dynamic_master_gain_insert_and_volume_use_independent_sample_targets_in_every_chunk() {
    let (mut session, _, _) = source();
    let master = session.add_track(TrackKind::Master, ChannelFormat::Mono, Some("Original Master J"));
    session.track_mut(master).unwrap().mixer.inserts[9] = Some(Insert::new("gain"));
    lane(&mut session, master, AutoParam::Volume, VOLUME);
    lane(&mut session, master, AutoParam::Plugin { slot: 9, param: "gain".into() }, PLUGIN_GAIN);
    let original = Arc::new(session);
    let original_json = original.to_json().unwrap();
    let mut memory = GainMemory::new();
    let gains = (0..LEN).map(|at| db_gain(linear(VOLUME, at as i64), -144.0) * memory.next(linear(PLUGIN_GAIN, at as i64))).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
    let static_wrong = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at)).collect::<Vec<_>>()).collect::<Vec<_>>();
    let instant_wrong = (0..2)
        .map(|ch| {
            (0..LEN).map(|at| signal(ch, at) * db_gain(linear(VOLUME, at as i64), -144.0) * db_gain(linear(PLUGIN_GAIN, at as i64), -96.0)).collect()
        })
        .collect::<Vec<Vec<f64>>>();
    different(&expected, &static_wrong, 0.05);
    different(&expected, &instant_wrong, 0.05);
    for schedule in [&[1][..], &[7][..], &[37][..], &[64][..], &[100][..], &[512][..], &[1_024][..], &[1, 7, 37, 64, 100][..]] {
        check(&render(original.clone(), schedule), &expected, 2, LEN, 2e-5);
    }
    assert_eq!(original.to_json().unwrap(), original_json);
    // Main remains stereo. Nominal Mono Master does not establish true mono Main support.
}

#[test]
fn prepared_solo_solo_safe_and_inactive_vca_preserve_static_and_dynamic_safety() {
    let (mut base, track, _) = source();
    let other = base.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Silent solo control"));
    let vca = base.add_track(TrackKind::Vca, ChannelFormat::Mono, Some("Original safety VCA"));
    base.track_mut(track).unwrap().mixer.vca = Some(vca.0);
    lane(&mut base, track, AutoParam::Volume, VOLUME);
    lane(&mut base, track, AutoParam::Mute, MUTE);
    let vca_volume = [(0, -3.0), (94, 3.0), (211, -6.0), (367, 0.0)];
    let vca_mute = [(0, 0.0), (97, 1.0), (113, 0.0), (249, 1.0), (267, 0.0)];
    lane(&mut base, vca, AutoParam::Volume, &vca_volume);
    lane(&mut base, vca, AutoParam::Mute, &vca_mute);
    let audible = (0..2)
        .map(|ch| {
            (0..LEN)
                .map(|at| {
                    if step(MUTE, at as i64) || step(&vca_mute, at as i64) {
                        0.0
                    } else {
                        signal(ch, at) * db_gain((linear(VOLUME, at as i64) + linear(&vca_volume, at as i64)).min(12.0), -144.0)
                    }
                })
                .collect()
        })
        .collect::<Vec<Vec<f64>>>();
    let ungated = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    different(&audible, &ungated, 0.1);
    let silence = vec![vec![0.0; LEN]; 2];
    // (track mute, other solo, source solo-safe, VCA inactive, VCA static mute).
    // An inactive assigned VCA still contributes its metadata under existing compatibility.
    for (track_mute, solo, solo_safe, inactive, vca_static_mute) in [
        (false, false, false, false, false),
        (true, false, false, false, false),
        (false, true, false, false, false),
        (false, true, true, false, false),
        (false, false, false, true, false),
        (false, true, true, true, false),
        (false, false, false, true, true),
    ] {
        let mut s = base.clone();
        s.track_mut(track).unwrap().mixer.mute = track_mute;
        s.track_mut(track).unwrap().mixer.solo_safe = solo_safe;
        s.track_mut(other).unwrap().mixer.solo = solo;
        s.track_mut(vca).unwrap().inactive = inactive;
        s.track_mut(vca).unwrap().mixer.mute = vca_static_mute;
        let snapshot = Arc::new(s);
        let before = snapshot.to_json().unwrap();
        let expected = if track_mute || (solo && !solo_safe) || vca_static_mute { &silence } else { &audible };
        check(&render(snapshot.clone(), &[1, 37, 7, 100]), expected, 2, LEN, 2e-6);
        assert_eq!(snapshot.to_json().unwrap(), before);
    }
}
