//! Prepared snapshot, state, safety and width references; baseline seven-family tests stay frozen.
#[path = "support/automation_reference.rs"]
mod reference;
use reference::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::{MixEngine, OfflineRenderer, SessionPreparer};
use soundcraft_model::{
    AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Route, SendSlot, Session, SourceAudio, SourceId, TrackId, TrackKind,
};
use std::sync::Arc;

fn fixture() -> (Session, TrackId, u64) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original prepared source"));
    let source = SourceId(900);
    let channels = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) as f32).collect()).collect();
    session.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", source, 0, 0, LEN as i64));
    session.track_mut(track).unwrap().mixer.pan = vec![-1.0, 1.0];
    (session, track, clip.0)
}
fn lane(session: &mut Session, track: TrackId, param: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(param);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    session.track_mut(track).unwrap().automation.push(lane);
}
fn prepared(session: Session, block: usize) -> OfflineRenderer {
    let mut renderer = OfflineRenderer::prepare(Arc::new(session), block).unwrap();
    renderer.mix_mut().pan_law = PanLaw::Zero;
    renderer
}
fn render(renderer: &mut OfflineRenderer, start: usize, frames: usize, sizes: &[usize]) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); 2];
    let mut done = 0;
    let mut index = 0;
    while done < frames {
        let n = sizes[index % sizes.len()].min(frames - done);
        let mut block = vec![vec![0.0; n]; 2];
        renderer.render((start + done) as i64, n, &mut block).unwrap();
        assert_eq!(block.len(), 2);
        assert!(block.iter().all(|ch| ch.len() == n));
        for (dst, src) in out.iter_mut().zip(block) {
            dst.extend(src);
        }
        done += n;
        index += 1;
    }
    out
}
fn combined_session() -> (Session, TrackId) {
    let (mut session, track, _) = fixture();
    let mut insert = Insert::new("gain");
    insert.params.insert("gain".into(), 0.0);
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
    let bus = session.add_bus("Original prepared bus", ChannelFormat::Stereo);
    let aux = session.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Original prepared return"));
    session.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
    session.track_mut(aux).unwrap().mixer.pan = vec![-1.0, 1.0];
    let mut send = SendSlot::new(Route::Bus(bus));
    send.level_db = 0.0;
    session.track_mut(track).unwrap().mixer.sends[0] = Some(send);
    for (param, points) in [
        (AutoParam::Volume, VOLUME),
        (AutoParam::Pan(0), PAN0),
        (AutoParam::Pan(1), PAN1),
        (AutoParam::Mute, MUTE),
        (AutoParam::SendLevel(0), SEND_LEVEL),
        (AutoParam::SendPan(0), SEND_PAN),
        (AutoParam::SendMute(0), SEND_MUTE),
        (AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN),
    ] {
        lane(&mut session, track, param, points);
    }
    (session, track)
}
fn combined_reference(start: usize, frames: usize, volume_offset: f64, history: &mut GainMemory) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; frames]; 2];
    for (n, at) in (start..start + frames).enumerate() {
        let plugin = history.next(linear(PLUGIN_GAIN, at as i64));
        let volume = db_gain(linear(VOLUME, at as i64) + volume_offset, -144.0);
        let safety = if step(MUTE, at as i64) { 0.0 } else { 1.0 };
        let x = [signal(0, at) * plugin * volume * safety, signal(1, at) * plugin * volume * safety];
        let p0 = pan(linear(PAN0, at as i64), None);
        let p1 = pan(linear(PAN1, at as i64), None);
        let balance = pan(linear(SEND_PAN, at as i64), None);
        let send = if step(SEND_MUTE, at as i64) { 0.0 } else { db_gain(linear(SEND_LEVEL, at as i64), -144.0) };
        for (ch, values) in out.iter_mut().enumerate() {
            values[n] = x[0] * p0[ch] + x[1] * p1[ch] + x[ch] * balance[ch] * send;
        }
    }
    out
}

#[test]
fn paired_snapshots_install_new_curves_without_resetting_retained_gain_memory() {
    let (session, track) = combined_session();
    let mut renderer = prepared(session.clone(), 512);
    let mut history = GainMemory::new();
    history.reset();
    check(&render(&mut renderer, 0, LEN, &[1, 7, 37, 64]), &combined_reference(0, LEN, 0.0, &mut history), 2, LEN, 3e-5);
    let mut changed = session.clone();
    let volume = changed.track_mut(track).unwrap().lane_mut(&AutoParam::Volume);
    for point in &mut volume.points {
        point.value += 6.0;
    }
    changed.track_mut(track).unwrap().comments = "Ordinary revision preserves original processor".into();
    renderer.reprepare(Arc::new(changed)).unwrap();
    let expected = combined_reference(43, 193, 6.0, &mut history);
    let mut fresh = GainMemory::new();
    let wrong_reset = combined_reference(43, 193, 6.0, &mut fresh);
    different(&expected, &wrong_reset, 0.01);
    check(&render(&mut renderer, 43, 193, &[7, 37, 1, 64]), &expected, 2, 193, 3e-5);
}

#[test]
fn prepared_volume_canonicalizes_duplicate_extreme_times_without_mutating_source() {
    let (mut session, track, _) = fixture();
    let points = [(i64::MIN, -6.0), (i64::MIN, -3.0), (0, -12.0), (0, -6.0), (73, -3.0), (73, 3.0), (i64::MAX, 6.0), (i64::MAX, 0.0)];
    lane(&mut session, track, AutoParam::Volume, &points);
    let source = Arc::new(session);
    let original = source.to_json().unwrap();
    let mut renderer = OfflineRenderer::prepare(Arc::clone(&source), 512).unwrap();
    // Explicit independently selected last values; interpolation uses unique endpoints.
    let canonical = [(i64::MIN, -3.0), (0, -6.0), (73, 3.0), (i64::MAX, 0.0)];
    let expected =
        (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * db_gain(linear(&canonical, at as i64), -144.0)).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut renderer, 0, LEN, &[1, 7, 37, 100]), &expected, 2, LEN, 2e-6);
    assert_eq!(source.to_json().unwrap(), original);
}

#[test]
fn nonfinite_preparation_failure_leaves_last_valid_snapshot_usable() {
    let (mut session, track, _) = fixture();
    lane(&mut session, track, AutoParam::Volume, VOLUME);
    let mut renderer = prepared(session.clone(), 512);
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut invalid = session.clone();
        invalid.track_mut(track).unwrap().lane_mut(&AutoParam::Volume).points[1].value = value;
        assert!(renderer.reprepare(Arc::new(invalid)).is_err());
        let expected =
            (0..2).map(|ch| (43..43 + 97).map(|at| signal(ch, at) * db_gain(linear(VOLUME, at as i64), -144.0)).collect()).collect::<Vec<Vec<f64>>>();
        check(&render(&mut renderer, 43, 97, &[7, 37, 1]), &expected, 2, 97, 2e-6);
    }
}

#[test]
fn pre_post_sends_share_track_vca_safety_but_only_post_reads_fader_offsets() {
    for pre in [false, true] {
        for return_kind in [TrackKind::Aux, TrackKind::Folder] {
            let (mut session, track, _) = fixture();
            let vca = session.add_track(TrackKind::Vca, ChannelFormat::Stereo, Some("Original safety VCA"));
            session.track_mut(track).unwrap().mixer.vca = Some(vca.0);
            let bus = session.add_bus("Original composed bus", ChannelFormat::Stereo);
            let aux = session.add_track(return_kind, ChannelFormat::Stereo, Some("Original composed return"));
            session.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
            session.track_mut(aux).unwrap().mixer.pan = vec![-1.0, 1.0];
            session.track_mut(track).unwrap().mixer.output = Route::None;
            let mut send = SendSlot::new(Route::Bus(bus));
            send.level_db = 0.0;
            send.pre_fader = pre;
            session.track_mut(track).unwrap().mixer.sends[9] = Some(send);
            let vca_volume = [(0, -3.0), (94, 3.0), (211, -6.0), (367, 0.0)];
            let vca_mute = [(0, 0.0), (97, 1.0), (113, 0.0)];
            let trim = [(0, -3.0), (94, 0.0), (211, 3.0), (367, 0.0)];
            lane(&mut session, vca, AutoParam::Volume, &vca_volume);
            lane(&mut session, vca, AutoParam::Mute, &vca_mute);
            lane(&mut session, track, AutoParam::Volume, VOLUME);
            lane(&mut session, track, AutoParam::Mute, MUTE);
            lane(&mut session, track, AutoParam::Plugin { slot: u8::MAX, param: "trim".into() }, &trim);
            lane(&mut session, track, AutoParam::SendLevel(9), SEND_LEVEL);
            lane(&mut session, track, AutoParam::SendPan(9), SEND_PAN);
            lane(&mut session, track, AutoParam::SendMute(9), SEND_MUTE);
            let expected = (0..2)
                .map(|ch| {
                    (0..LEN)
                        .map(|at| {
                            let time = at as i64;
                            if step(MUTE, time) || step(&vca_mute, time) || step(SEND_MUTE, time) {
                                return 0.0;
                            }
                            let fader = if pre {
                                1.0
                            } else {
                                db_gain((linear(VOLUME, time) + linear(&vca_volume, time) + linear(&trim, time)).min(12.0), -144.0)
                            };
                            signal(ch, at) * fader * db_gain(linear(SEND_LEVEL, time), -144.0) * pan(linear(SEND_PAN, time), None)[ch]
                        })
                        .collect()
                })
                .collect::<Vec<Vec<f64>>>();
            let mut renderer = prepared(session.clone(), 512);
            check(&render(&mut renderer, 0, LEN, &[1, 7, 37, 64]), &expected, 2, LEN, 2e-6);
            session.track_mut(track).unwrap().mixer.mute = true;
            renderer.reprepare(Arc::new(session.clone())).unwrap();
            check(&render(&mut renderer, 0, LEN, &[37]), &vec![vec![0.0; LEN]; 2], 2, LEN, 2e-6);
            session.track_mut(track).unwrap().mixer.mute = false;
            session.track_mut(vca).unwrap().mixer.mute = true;
            renderer.reprepare(Arc::new(session)).unwrap();
            check(&render(&mut renderer, 0, LEN, &[37]), &vec![vec![0.0; LEN]; 2], 2, LEN, 2e-6);
        }
    }
}

fn limiter(input: &[Vec<f64>], threshold: f64, reduction: &mut f64) -> Vec<Vec<f64>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0; frames]; input.len()];
    for at in 0..frames {
        let peak = input.iter().map(|ch| ch[at].abs()).fold(0.0, f64::max);
        let target = (20.0 * peak.max(1e-12).log10() - threshold).max(0.0) * (1.0 - 1.0 / 1000.0);
        let millis = if target > *reduction { 0.01 } else { 5.0 };
        let coefficient = (-1.0f64 / (millis * 0.001 * 48_000.0)).exp();
        *reduction = target + (*reduction - target) * coefficient;
        let attenuation = 10f64.powf(-*reduction / 20.0);
        for (dst, src) in out.iter_mut().zip(input) {
            dst[at] = src[at] * attenuation;
        }
    }
    out
}
#[test]
fn unchanged_clip_effect_revision_retains_limiter_memory_changed_configuration_replaces_it() {
    let (mut session, track, clip) = fixture();
    for (name, value) in [
        ("threshold", -12.0),
        ("attack", 0.01),
        ("release", 5.0),
        ("knee", 0.0),
        ("makeup", 0.0),
        ("limiter", 1.0),
        ("lookahead", 0.0),
        ("sc_hpf_on", 0.0),
        ("mix", 100.0),
    ] {
        session.edit.values.insert(format!("clip_fx.{clip}.comp.{name}"), value);
    }
    let mut renderer = prepared(session.clone(), 64);
    let mut reduction = 0.0;
    let first = (0..2).map(|ch| (0..64).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut renderer, 0, 64, &[64]), &limiter(&first, -12.0, &mut reduction), 2, 64, 3e-6);
    session.track_mut(track).unwrap().comments = "Original effect configuration unchanged".into();
    renderer.reprepare(Arc::new(session.clone())).unwrap();
    let next = (0..2).map(|ch| (64..128).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    let expected = limiter(&next, -12.0, &mut reduction);
    let wrong_fresh = limiter(&next, -12.0, &mut 0.0);
    different(&expected, &wrong_fresh, 1e-4);
    check(&render(&mut renderer, 64, 64, &[64]), &expected, 2, 64, 3e-6);
    session.edit.values.insert(format!("clip_fx.{clip}.comp.threshold"), -24.0);
    renderer.reprepare(Arc::new(session)).unwrap();
    let last = (0..2).map(|ch| (128..192).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut renderer, 128, 64, &[64]), &limiter(&last, -24.0, &mut 0.0), 2, 64, 3e-6);
}

#[test]
fn changed_prepared_block_installs_new_processor_and_equal_effective_blocks_retain_it() {
    let (mut session, track, _) = fixture();
    let mut insert = Insert::new("gain");
    insert.params.insert("gain".into(), -12.0);
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
    let source = Arc::new(session);
    let mut prepare = SessionPreparer::default();
    let mut mixer = MixEngine::new(48_000.0, 64);
    drop(mixer.install_prepared(prepare.prepare(Arc::clone(&source), 42, 64).unwrap()));
    let mut warm = GainMemory::new();
    let gains = (0..64).map(|_| warm.next(-12.0)).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (0..64).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
    let mut actual = vec![vec![0.0; 64]; 2];
    mixer.render_prepared(0, 64, &mut actual).unwrap();
    check(&actual, &expected, 2, 64, 2e-5);
    drop(mixer.install_prepared(prepare.prepare(Arc::clone(&source), 42, 128).unwrap()));
    let mut fresh = GainMemory::new();
    let gains = (64..192).map(|_| fresh.next(-12.0)).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (64..192).enumerate().map(|(i, at)| signal(ch, at) * gains[i]).collect()).collect::<Vec<Vec<f64>>>();
    let mut actual = vec![vec![0.0; 128]; 2];
    mixer.render_prepared(64, 128, &mut actual).unwrap();
    check(&actual, &expected, 2, 128, 2e-5);
    drop(mixer.install_prepared(prepare.prepare(Arc::clone(&source), 42, 0).unwrap())); // Effective 16 starts a new processor.
    let mut retained = GainMemory::new();
    let mut actual = vec![vec![0.0; 16]; 2];
    mixer.render_prepared(192, 16, &mut actual).unwrap();
    let gains = (0..16).map(|_| retained.next(-12.0)).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (192..208).enumerate().map(|(i, at)| signal(ch, at) * gains[i]).collect()).collect::<Vec<Vec<f64>>>();
    check(&actual, &expected, 2, 16, 2e-5);
    drop(mixer.install_prepared(prepare.prepare(source, 42, 7).unwrap())); // Also effective 16: retain the warmed processor.
    mixer.render_prepared(208, 16, &mut actual).unwrap();
    let gains = (0..16).map(|_| retained.next(-12.0)).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (208..224).enumerate().map(|(i, at)| signal(ch, at) * gains[i]).collect()).collect::<Vec<Vec<f64>>>();
    check(&actual, &expected, 2, 16, 2e-5);
}
