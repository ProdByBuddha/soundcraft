//! Seven-family independent red/green PCM acceptance against public mixer APIs.
#[path = "support/automation_reference.rs"]
mod reference;
use reference::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::MixEngine;
use soundcraft_model::{
    AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Route, SendSlot, Session, SourceAudio, SourceId, TrackId, TrackKind,
};
use std::sync::Arc;

fn fixture(format: ChannelFormat, frames: usize) -> (Session, TrackId) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, format, Some("Original automation source"));
    let source = SourceId(900);
    let channels = (0..format.channels()).map(|ch| (0..frames).map(|at| signal(ch, at) as f32).collect()).collect();
    session.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = session.new_clip_id();
    session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original binary fractions", source, 0, 0, frames as i64));
    session.track_mut(track).unwrap().mixer.pan = if format == ChannelFormat::Mono { vec![0.0] } else { vec![-1.0, 1.0] };
    (session, track)
}
fn lane(session: &mut Session, track: TrackId, param: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(param);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    session.track_mut(track).unwrap().automation.push(lane);
}
fn render(engine: &mut MixEngine, session: &Session, start: i64, frames: usize, sizes: &[usize]) -> Vec<Vec<f32>> {
    let mut result = vec![Vec::new(); 2];
    let mut offset = 0;
    let mut index = 0;
    while offset < frames {
        let count = sizes[index % sizes.len()].min(frames - offset);
        let mut block = vec![vec![0.0; count]; 2];
        engine.render(session, start + offset as i64, count, &mut block);
        assert_eq!(block.len(), 2);
        assert!(block.iter().all(|ch| ch.len() == count));
        for (channel, block) in result.iter_mut().zip(block) {
            channel.extend(block);
        }
        offset += count;
        index += 1;
    }
    result
}
fn partitions() -> Vec<Vec<usize>> {
    vec![vec![1], vec![7], vec![37], vec![64], vec![100], vec![512], vec![1024], vec![1, 37, 7, 64, 3, 100, 19]]
}
fn source(start: usize, frames: usize) -> Vec<Vec<f64>> {
    (0..2).map(|ch| (start..start + frames).map(|at| signal(ch, at)).collect()).collect()
}
fn scalar_reference(start: usize, frames: usize, value: impl Fn(i64) -> f64) -> Vec<Vec<f64>> {
    (0..2).map(|ch| (start..start + frames).map(|at| signal(ch, at) * value(at as i64)).collect()).collect()
}
fn verify_partitions(session: &Session, expected: &[Vec<f64>], tolerance: f64) {
    verify_law_partitions(session, expected, tolerance, PanLaw::Zero);
}
fn verify_law_partitions(session: &Session, expected: &[Vec<f64>], tolerance: f64, law: PanLaw) {
    for sizes in partitions() {
        let mut engine = MixEngine::new(48_000.0, 1024);
        engine.pan_law = law;
        let actual = render(&mut engine, session, 0, LEN, &sizes);
        check(&actual, expected, 2, LEN, tolerance);
    }
}

#[test]
fn volume_uses_absolute_linear_db_at_every_sample() {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    lane(&mut session, track, AutoParam::Volume, VOLUME);
    let expected = scalar_reference(0, LEN, |at| db_gain(linear(VOLUME, at), -144.0));
    different(&expected, &source(0, LEN), 0.05);
    verify_partitions(&session, &expected, 2e-6);
}
fn panner_case(index: u8) {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    let points = if index == 0 { PAN0 } else { PAN1 };
    lane(&mut session, track, AutoParam::Pan(index), points);
    let mut expected = vec![vec![0.0; LEN]; 2];
    for at in 0..LEN {
        let p0 = pan(if index == 0 { linear(points, at as i64) } else { -1.0 }, None);
        let p1 = pan(if index == 1 { linear(points, at as i64) } else { 1.0 }, None);
        for (out, channel) in expected.iter_mut().enumerate() {
            channel[at] = signal(0, at) * p0[out] + signal(1, at) * p1[out];
        }
    }
    different(&expected, &source(0, LEN), 0.05);
    verify_partitions(&session, &expected, 2e-6);
}
#[test]
fn pan0_changes_its_stereo_source_channel_per_sample() {
    panner_case(0);
}
#[test]
fn pan1_changes_its_stereo_source_channel_per_sample() {
    panner_case(1);
}
#[test]
fn mono_pan_retains_every_declared_static_law() {
    for (law, depth) in [(PanLaw::Zero, None), (PanLaw::Minus3, Some(-3.0)), (PanLaw::Minus4_5, Some(-4.5)), (PanLaw::Minus6, Some(-6.0))] {
        let (mut session, track) = fixture(ChannelFormat::Mono, LEN);
        lane(&mut session, track, AutoParam::Pan(0), PAN0);
        let expected =
            (0..2).map(|ch| (0..LEN).map(|at| signal(0, at) * pan(linear(PAN0, at as i64), depth)[ch]).collect()).collect::<Vec<Vec<f64>>>();
        verify_law_partitions(&session, &expected, 2e-6, law);
    }
}
#[test]
fn mute_steps_on_the_exact_knot_inside_a_callback() {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    lane(&mut session, track, AutoParam::Mute, MUTE);
    let expected = scalar_reference(0, LEN, |at| if step(MUTE, at) { 0.0 } else { 1.0 });
    different(&expected, &source(0, LEN), 0.05);
    verify_partitions(&session, &expected, 2e-6);
}

fn send_fixture(slot: usize, mono: bool) -> (Session, TrackId) {
    let (mut session, track) = fixture(if mono { ChannelFormat::Mono } else { ChannelFormat::Stereo }, LEN);
    let bus = session.add_bus("Original send bus", ChannelFormat::Stereo);
    let aux = session.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Original return"));
    let return_track = session.track_mut(aux).unwrap();
    return_track.mixer.input = Route::Bus(bus);
    return_track.mixer.pan = vec![-1.0, 1.0];
    let source_track = session.track_mut(track).unwrap();
    source_track.mixer.output = Route::None;
    let mut send = SendSlot::new(Route::Bus(bus));
    send.level_db = 0.0;
    source_track.mixer.sends[slot] = Some(send);
    (session, track)
}
fn send_level_case(slot: usize) {
    let (mut session, track) = send_fixture(slot, false);
    lane(&mut session, track, AutoParam::SendLevel(slot as u8), SEND_LEVEL);
    let expected = scalar_reference(0, LEN, |at| db_gain(linear(SEND_LEVEL, at), -144.0));
    different(&expected, &vec![vec![0.0; LEN]; 2], 0.05);
    verify_partitions(&session, &expected, 2e-6);
}
fn send_pan_case(slot: usize) {
    let (mut session, track) = send_fixture(slot, false);
    lane(&mut session, track, AutoParam::SendPan(slot as u8), SEND_PAN);
    let expected =
        (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * pan(linear(SEND_PAN, at as i64), None)[ch]).collect()).collect::<Vec<Vec<f64>>>();
    different(&expected, &source(0, LEN), 0.05);
    verify_partitions(&session, &expected, 2e-6);
}
fn send_mute_case(slot: usize) {
    let (mut session, track) = send_fixture(slot, false);
    session.track_mut(track).unwrap().mixer.sends[slot].as_mut().unwrap().mute = true;
    lane(&mut session, track, AutoParam::SendMute(slot as u8), SEND_MUTE);
    let expected = scalar_reference(0, LEN, |at| if step(SEND_MUTE, at) { 0.0 } else { 1.0 });
    different(&expected, &source(0, LEN), 0.05);
    verify_partitions(&session, &expected, 2e-6);
}
fn plugin_case(slot: usize) {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    let mut insert = Insert::new("gain");
    insert.params.insert("gain".into(), 0.0);
    session.track_mut(track).unwrap().mixer.inserts[slot] = Some(insert);
    lane(&mut session, track, AutoParam::Plugin { slot: slot as u8, param: "gain".into() }, PLUGIN_GAIN);
    let mut history = GainMemory::new();
    let gains = (0..LEN).map(|at| history.next(linear(PLUGIN_GAIN, at as i64))).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
    let instant = scalar_reference(0, LEN, |at| db_gain(linear(PLUGIN_GAIN, at), -96.0));
    different(&expected, &instant, 0.05);
    different(&expected, &source(0, LEN), 0.02);
    verify_partitions(&session, &expected, 2e-5);
}
macro_rules! slot_cases {
    ($slot:expr, $level:ident, $pan:ident, $mute:ident, $plugin:ident) => {
        #[test]
        fn $level() {
            send_level_case($slot);
        }
        #[test]
        fn $pan() {
            send_pan_case($slot);
        }
        #[test]
        fn $mute() {
            send_mute_case($slot);
        }
        #[test]
        fn $plugin() {
            plugin_case($slot);
        }
    };
}
slot_cases!(0, send_a_level, send_a_pan, send_a_mute, plugin_a_gain);
slot_cases!(1, send_b_level, send_b_pan, send_b_mute, plugin_b_gain);
slot_cases!(2, send_c_level, send_c_pan, send_c_mute, plugin_c_gain);
slot_cases!(3, send_d_level, send_d_pan, send_d_mute, plugin_d_gain);
slot_cases!(4, send_e_level, send_e_pan, send_e_mute, plugin_e_gain);
slot_cases!(5, send_f_level, send_f_pan, send_f_mute, plugin_f_gain);
slot_cases!(6, send_g_level, send_g_pan, send_g_mute, plugin_g_gain);
slot_cases!(7, send_h_level, send_h_pan, send_h_mute, plugin_h_gain);
slot_cases!(8, send_i_level, send_i_pan, send_i_mute, plugin_i_gain);
slot_cases!(9, send_j_level, send_j_pan, send_j_mute, plugin_j_gain);

#[test]
fn mono_send_pan_is_a_positive_control_for_the_existing_consumer() {
    let (mut session, track) = send_fixture(0, true);
    lane(&mut session, track, AutoParam::SendPan(0), SEND_PAN);
    let expected =
        (0..2).map(|ch| (0..LEN).map(|at| signal(0, at) * pan(linear(SEND_PAN, at as i64), None)[ch]).collect()).collect::<Vec<Vec<f64>>>();
    verify_partitions(&session, &expected, 2e-6);
}
#[test]
fn stereo_static_send_balance_has_center_identity_and_endpoint_attenuation() {
    for value in [-1.0, -0.375, 0.0, 0.625, 1.0] {
        let (mut session, track) = send_fixture(0, false);
        session.track_mut(track).unwrap().mixer.sends[0].as_mut().unwrap().pan = value as f32;
        let expected = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * pan(value, None)[ch]).collect()).collect::<Vec<Vec<f64>>>();
        verify_partitions(&session, &expected, 2e-6);
    }
}
#[test]
fn gain_static_reference_includes_initial_one_twenty_ms_and_snap() {
    const LONG: usize = 16_000;
    for target in [-12.0, -96.0] {
        let (mut session, track) = fixture(ChannelFormat::Stereo, LONG);
        let mut insert = Insert::new("gain");
        insert.params.insert("gain".into(), target as f32);
        session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
        let mut history = GainMemory::new();
        let gains = (0..LONG).map(|_| history.next(target)).collect::<Vec<_>>();
        assert_eq!(gains[LONG - 1], db_gain(target, -96.0));
        let expected = (0..2).map(|ch| (0..LONG).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
        for sizes in [vec![1], vec![37], vec![1024]] {
            let mut engine = MixEngine::new(48_000.0, 1024);
            check(&render(&mut engine, &session, 0, LONG, &sizes), &expected, 2, LONG, 2e-5);
        }
    }
}
#[test]
fn gain_state_is_retained_over_seek_loop_and_reset_snaps_last_target() {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    let mut insert = Insert::new("gain");
    insert.params.insert("gain".into(), 0.0);
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(insert);
    lane(&mut session, track, AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN);
    let mut engine = MixEngine::new(48_000.0, 1024);
    let mut history = GainMemory::new();
    for (start, frames, reset) in [(43, 47, false), (211, 29, false), (61, 53, false), (43, 47, false), (43, 47, true)] {
        if reset {
            engine.reset();
            history.reset();
        }
        let gains = (start..start + frames).map(|at| history.next(linear(PLUGIN_GAIN, at as i64))).collect::<Vec<_>>();
        let expected =
            (0..2).map(|ch| (start..start + frames).enumerate().map(|(i, at)| signal(ch, at) * gains[i]).collect()).collect::<Vec<Vec<f64>>>();
        check(&render(&mut engine, &session, start as i64, frames, &[7, 37, 1]), &expected, 2, frames, 2e-5);
    }
}
#[test]
fn plugin_lane_is_not_hidden_by_an_absent_stored_parameter_key() {
    let (mut session, track) = fixture(ChannelFormat::Stereo, LEN);
    session.track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    lane(&mut session, track, AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN);
    let mut history = GainMemory::new();
    let gains = (0..LEN).map(|at| history.next(linear(PLUGIN_GAIN, at as i64))).collect::<Vec<_>>();
    let expected = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
    different(&expected, &source(0, LEN), 0.02);
    verify_partitions(&session, &expected, 2e-5);
}
