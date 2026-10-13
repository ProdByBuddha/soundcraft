//! Independent PCM references for the bounded send consumers, with no hosted plugins/devices.
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::{MixEngine, render_bus, render_range};
use soundcraft_model::{
    AutoParam, AutomationLane, AutomationMode, AutomationPoint, BusId, ChannelFormat, Clip, Route, SendSlot, Session, SourceAudio, SourceId, TrackId,
    TrackKind,
};
use soundcraft_time::Range;
use std::sync::Arc;

const LEN: usize = 640;
const PAN: [(i64, f32); 3] = [(37, -1.0), (173, 0.75), (401, 1.0)];
const MUTE: [(i64, f32); 5] = [(37, 0.0), (97, 1.0), (173, 0.0), (257, 1.0), (349, 0.0)];

fn fixture(format: ChannelFormat) -> (Session, TrackId, BusId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, format, None);
    let src = SourceId(900);
    let channels = (0..format.channels()).map(|ch| (0..LEN).map(|n| signal(ch, n)).collect()).collect();
    s.pool.insert(src, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let cid = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "Original synthetic", src, 0, 0, LEN as i64));
    let bus = s.add_bus("Send", ChannelFormat::Stereo);
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
    let tr = s.track_mut(t).unwrap();
    tr.mixer.output = Route::None;
    let mut send = SendSlot::new(Route::Bus(bus));
    send.level_db = 0.0;
    tr.mixer.sends[0] = Some(send);
    (s, t, bus)
}
fn signal(ch: usize, n: usize) -> f32 {
    if n % 17 < 9 { 0.25 + ch as f32 / 100.0 } else { -0.125 - ch as f32 / 200.0 }
}
fn lane(s: &mut Session, t: TrackId, param: AutoParam, points: &[(i64, f32)]) {
    let mut lane = AutomationLane::new(param);
    for &(at, value) in points {
        lane.set_point(at, value);
    }
    s.track_mut(t).unwrap().automation.push(lane);
}
fn automated() -> (Session, TrackId, BusId) {
    let (mut s, t, bus) = fixture(ChannelFormat::Mono);
    lane(&mut s, t, AutoParam::SendPan(0), &PAN);
    lane(&mut s, t, AutoParam::SendMute(0), &MUTE);
    (s, t, bus)
}
// The expectation never calls AutomationLane::value_at, pan::gains or any mixer helper.
fn expected_pan(at: i64) -> f64 {
    if at <= PAN[0].0 {
        return f64::from(PAN[0].1);
    }
    for pair in PAN.windows(2) {
        if at < pair[1].0 {
            let f = (at - pair[0].0) as f64 / (pair[1].0 - pair[0].0) as f64;
            return f64::from(pair[0].1) + f64::from(pair[1].1 - pair[0].1) * f;
        }
    }
    f64::from(PAN[2].1)
}
fn muted(at: i64) -> bool {
    MUTE.iter().rev().find(|p| p.0 <= at).unwrap_or(&MUTE[0]).1 >= 0.5
}
fn pan_factors(p: f64, law: PanLaw) -> [f64; 2] {
    if law == PanLaw::Zero {
        return [(1.0 - p).min(1.0), (1.0 + p).min(1.0)];
    }
    let depth = match law {
        PanLaw::Minus3 => -3.0,
        PanLaw::Minus4_5 => -4.5,
        PanLaw::Minus6 => -6.0,
        PanLaw::Zero => 0.0,
    };
    let angle = (p + 1.0) * std::f64::consts::PI / 4.0;
    [angle.cos().max(0.0).powf(depth / -3.0103), angle.sin().max(0.0).powf(depth / -3.0103)]
}
fn assert_shape(out: &[Vec<f32>], channels: usize, frames: usize) {
    assert_eq!(out.len(), channels);
    for channel in out {
        assert_eq!(channel.len(), frames);
    }
}
fn check(out: &[Vec<f32>], start: i64, frames: usize, law: PanLaw, scale: f64) {
    assert_shape(out, 2, frames);
    for (ch, values) in out.iter().enumerate() {
        for (n, &actual) in values.iter().enumerate() {
            let at = start + n as i64;
            let reference = if muted(at) { 0.0 } else { f64::from(signal(0, at as usize)) * scale * pan_factors(expected_pan(at), law)[ch] };
            assert!((f64::from(actual) - reference).abs() < 2e-7, "channel {ch}, at {at}: {actual} vs {reference}");
        }
    }
}
fn chunks(eng: &mut MixEngine, s: &Session, start: i64, length: usize, sizes: &[usize]) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(), Vec::new()];
    let mut offset = 0;
    let mut k = 0;
    while offset < length {
        let n = sizes[k % sizes.len()].min(length - offset);
        let mut block = vec![vec![0.0; n]; 2];
        eng.render(s, start + offset as i64, n, &mut block);
        for (out, block) in out.iter_mut().zip(block) {
            out.extend(block);
        }
        offset += n;
        k += 1;
    }
    out
}

#[test]
fn pan_and_mute_match_independent_pcm_across_chunks() {
    let (s, _, bus) = automated();
    for block in [1, 37, 64, 100, 512, 1024] {
        let mut e = MixEngine::new(48_000.0, block.max(16));
        let out = chunks(&mut e, &s, 0, LEN, &[block]);
        check(&out, 0, LEN, PanLaw::Minus3, 1.0);
    }
    let mut e = MixEngine::new(48_000.0, 1024);
    check(&chunks(&mut e, &s, 0, LEN, &[1, 37, 64, 3, 100, 19]), 0, LEN, PanLaw::Minus3, 1.0);
    check(&render_bus(&s, bus, Range::new(0, LEN as i64), 37), 0, LEN, PanLaw::Minus3, 1.0);
}
#[test]
fn all_pan_laws_match_numeric_reference_and_static_calibration() {
    let (s, _, _) = automated();
    for law in [PanLaw::Minus3, PanLaw::Minus4_5, PanLaw::Minus6, PanLaw::Zero] {
        let mut e = MixEngine::new(48_000.0, 1024);
        e.pan_law = law;
        check(&chunks(&mut e, &s, 0, LEN, &[100]), 0, LEN, law, 1.0);
        for at in [0, 77, 173, 349, 401] {
            let (mut reference, t, _) = fixture(ChannelFormat::Mono);
            reference.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().pan = expected_pan(at) as f32;
            let mut static_engine = MixEngine::new(48_000.0, 16);
            static_engine.pan_law = law;
            let static_pcm = chunks(&mut static_engine, &reference, at, 1, &[1]);
            let automated_pcm = chunks(&mut e, &s, at, 1, &[1]);
            assert_shape(&automated_pcm, 2, 1);
            assert_shape(&static_pcm, 2, 1);
            assert_eq!(automated_pcm, static_pcm);
        }
    }
}
#[test]
fn seeking_used_engine_has_no_cursor_history() {
    let (s, _, _) = automated();
    let mut e = MixEngine::new(48_000.0, 1024);
    let _ = chunks(&mut e, &s, 0, 520, &[100]);
    for at in [31, 77, 97, 98, 173, 256, 257, 301] {
        let out = chunks(&mut e, &s, at, 31, &[17, 1, 13]);
        check(&out, at, 31, PanLaw::Minus3, 1.0);
        let mut fresh = MixEngine::new(48_000.0, 1024);
        assert_eq!(out, chunks(&mut fresh, &s, at, 31, &[17, 1, 13]));
    }
}
#[test]
fn loop_wrap_and_tiny_final_chunks_use_absolute_knots() {
    let (s, _, _) = automated();
    let mut e = MixEngine::new(48_000.0, 1024);
    // Same loop splitting as playback: do not render past the loop end before wrapping.
    for _ in 0..3 {
        check(&chunks(&mut e, &s, 37, 264, &[100, 63]), 37, 264, PanLaw::Minus3, 1.0);
    }
    for _ in 0..2 {
        check(&chunks(&mut e, &s, 97, 76, &[37]), 97, 76, PanLaw::Minus3, 1.0);
    }
}
#[test]
fn modes_and_lane_enabled_preserve_existing_reader_policy() {
    for mode in AutomationMode::ALL {
        let (mut s, t, _) = automated();
        let tr = s.track_mut(t).unwrap();
        tr.mixer.automation_mode = mode;
        tr.mixer.sends[0].as_mut().unwrap().mute = true;
        for lane in &mut tr.automation {
            lane.enabled = false;
        }
        let out = render_range(&s, Range::new(0, LEN as i64), 37);
        assert_shape(&out, 2, LEN);
        if mode == AutomationMode::Off {
            assert!(out.iter().flatten().all(|v| *v == 0.0));
        } else {
            check(&out, 0, LEN, PanLaw::Minus3, 1.0);
        }
    }
}
#[test]
fn missing_and_empty_lanes_keep_static_pan_and_mute() {
    let (mut s, t, _) = fixture(ChannelFormat::Mono);
    let reference = render_range(&s, Range::new(0, LEN as i64), 100);
    lane(&mut s, t, AutoParam::SendPan(0), &[]);
    lane(&mut s, t, AutoParam::SendMute(0), &[]);
    assert_eq!(render_range(&s, Range::new(0, LEN as i64), 100), reference);
    s.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().mute = true;
    assert!(render_range(&s, Range::new(0, LEN as i64), 100).iter().flatten().all(|v| *v == 0.0));
}
#[test]
fn static_matrix_mute_steps_and_threshold_match_reference() {
    let (mut s, t, bus) = fixture(ChannelFormat::Stereo);
    let reference = render_bus(&s, bus, Range::new(0, LEN as i64), 100);
    lane(&mut s, t, AutoParam::SendMute(0), &[(0, 0.49), (97, 0.5), (173, 0.0)]);
    let out = render_bus(&s, bus, Range::new(0, LEN as i64), 100);
    assert_shape(&out, 2, LEN);
    assert_shape(&reference, 2, LEN);
    assert_eq!(out.len(), reference.len());
    for (actual, reference) in out.iter().zip(reference) {
        assert_eq!(actual.len(), reference.len());
        for (i, (&a, r)) in actual.iter().zip(reference).enumerate() {
            assert_eq!(a, if (97..173).contains(&i) { 0.0 } else { r });
        }
    }
}
#[test]
fn mute_preserves_routing_for_mono_surround_and_sixteen_channels() {
    for fmt in [ChannelFormat::Mono, ChannelFormat::Stereo, ChannelFormat::Surround51, ChannelFormat::Atmos714, ChannelFormat::Atmos916] {
        let (mut s, t, bus) = fixture(fmt);
        s.busses.iter_mut().find(|b| b.id == bus).unwrap().format = fmt;
        let reference = render_bus(&s, bus, Range::new(0, LEN as i64), 37);
        assert!(reference.iter().flatten().any(|v| v.abs() > 0.05));
        lane(&mut s, t, AutoParam::SendMute(0), &MUTE);
        let out = render_bus(&s, bus, Range::new(0, LEN as i64), 100);
        assert_shape(&out, fmt.channels(), LEN);
        assert_shape(&reference, fmt.channels(), LEN);
        assert_eq!(out.len(), reference.len());
        for (actual, reference) in out.iter().zip(reference) {
            assert_eq!(actual.len(), reference.len());
            for (i, (&a, r)) in actual.iter().zip(reference).enumerate() {
                assert_eq!(a, if muted(i as i64) { 0.0 } else { r });
            }
        }
    }
}
#[test]
fn send_pan_matches_supported_width_laws_and_preserves_wide_fallback() {
    for (src_fmt, dst_fmt) in [
        (ChannelFormat::Stereo, ChannelFormat::Stereo),
        (ChannelFormat::Mono, ChannelFormat::Surround51),
        (ChannelFormat::Surround51, ChannelFormat::Stereo),
    ] {
        let (mut s, t, bus) = fixture(src_fmt);
        s.busses.iter_mut().find(|b| b.id == bus).unwrap().format = dst_fmt;
        let reference = render_bus(&s, bus, Range::new(0, LEN as i64), 37);
        assert!(reference.iter().flatten().any(|v| v.abs() > 0.05));
        let mut static_control = s.clone();
        lane(&mut s, t, AutoParam::SendPan(0), &PAN);
        assert_shape(&reference, dst_fmt.channels(), LEN);
        let actual = render_bus(&s, bus, Range::new(0, LEN as i64), 37);
        assert_shape(&actual, dst_fmt.channels(), LEN);
        assert!(actual.iter().flatten().all(|sample| sample.is_finite()));
        if src_fmt == ChannelFormat::Stereo {
            for (ch, channel) in actual.iter().enumerate() {
                for (n, &sample) in channel.iter().enumerate() {
                    let pan = expected_pan(n as i64);
                    let balance = if ch == 0 { (1.0 - pan).min(1.0) } else { (1.0 + pan).min(1.0) };
                    let expected = f64::from(signal(ch, n)) * balance;
                    assert!((f64::from(sample) - expected).abs() < 2e-7, "stereo channel {ch}, at {n}: {sample} vs {expected}");
                }
            }
            assert!(actual.iter().zip(&reference).any(|(a, r)| a.iter().zip(r).any(|(a, r)| (*a - *r).abs() > 0.05)));
            assert_eq!(render_bus(&s, bus, Range::new(0, LEN as i64), 37), actual);
        } else if src_fmt == ChannelFormat::Mono {
            // Static spatial routing calibrates all six channels independently at each sample.
            for n in 0..LEN {
                static_control.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().pan = expected_pan(n as i64) as f32;
                let expected = render_bus(&static_control, bus, Range::new(n as i64, n as i64 + 1), 37);
                assert_shape(&expected, 6, 1);
                assert!(expected.iter().flatten().all(|sample| sample.is_finite()));
                for (ch, (actual_channel, expected_channel)) in actual.iter().zip(&expected).enumerate() {
                    assert_eq!(actual_channel[n], expected_channel[0], "spatial channel {ch}, at {n}");
                }
            }
            assert!(actual.iter().zip(&reference).any(|(a, r)| a.iter().zip(r).any(|(a, r)| (*a - *r).abs() > 0.05)));
            assert_eq!(render_bus(&s, bus, Range::new(0, LEN as i64), 37), actual);
        } else {
            // A wider source retains its existing static matrix; this is not spatial automation acceptance.
            assert_eq!(actual, reference);
            assert_eq!(render_bus(&s, bus, Range::new(0, LEN as i64), 37), reference);
        }
    }
}
#[test]
fn pre_post_fader_send_level_and_direct_output_are_preserved() {
    for pre in [false, true] {
        let (mut s, t, bus) = automated();
        let tr = s.track_mut(t).unwrap();
        tr.mixer.volume_db = -6.0;
        let snd = tr.mixer.sends[0].as_mut().unwrap();
        snd.pre_fader = pre;
        snd.level_db = -3.0;
        let factor = 10.0f64.powf((-3.0 + if pre { 0.0 } else { -6.0 }) / 20.0);
        check(&render_range(&s, Range::new(0, LEN as i64), 37), 0, LEN, PanLaw::Minus3, factor);
        // Keep the direct path isolated from the Aux while comparing source-main PCM.
        let aux = s.tracks.iter().find(|tr| tr.kind == TrackKind::Aux).unwrap().id;
        s.track_mut(aux).unwrap().inactive = true;
        s.track_mut(t).unwrap().mixer.output = Route::Main;
        let direct = render_range(&s, Range::new(0, LEN as i64), 37);
        s.track_mut(t).unwrap().automation.clear();
        s.track_mut(t).unwrap().mixer.sends[0] = None;
        assert_eq!(render_range(&s, Range::new(0, LEN as i64), 37), direct);
        assert_eq!(s.bus(bus).unwrap().format, ChannelFormat::Stereo);
    }
}
#[test]
fn track_solo_and_vca_suppression_cannot_be_unmuted_by_send_lane() {
    for suppression in [0, 1, 2] {
        let (mut s, t, _) = automated();
        s.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().pre_fader = true;
        match suppression {
            0 => s.track_mut(t).unwrap().mixer.mute = true,
            1 => {
                let other = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
                s.track_mut(other).unwrap().mixer.solo = true;
            }
            _ => {
                let vca = s.add_track(TrackKind::Vca, ChannelFormat::Mono, None);
                s.track_mut(vca).unwrap().mixer.mute = true;
                s.track_mut(t).unwrap().mixer.vca = Some(vca.0);
            }
        }
        assert!(render_range(&s, Range::new(0, LEN as i64), 100).iter().flatten().all(|v| *v == 0.0));
    }
}
#[test]
fn slots_a_and_j_and_absent_slots_do_not_drive_each_other() {
    let (mut s, t, bus) = fixture(ChannelFormat::Mono);
    let other = s.add_bus("J", ChannelFormat::Stereo);
    let mut send = SendSlot::new(Route::Bus(other));
    send.level_db = 0.0;
    s.track_mut(t).unwrap().mixer.sends[9] = Some(send);
    let reference = render_bus(&s, bus, Range::new(0, LEN as i64), 37);
    assert!(reference.iter().flatten().any(|v| v.abs() > 0.05));
    lane(&mut s, t, AutoParam::SendPan(9), &PAN);
    lane(&mut s, t, AutoParam::SendMute(9), &MUTE);
    lane(&mut s, t, AutoParam::SendMute(8), &[(0, 1.0)]);
    assert_eq!(render_bus(&s, bus, Range::new(0, LEN as i64), 37), reference);
    check(&render_bus(&s, other, Range::new(0, LEN as i64), 100), 0, LEN, PanLaw::Minus3, 1.0);
}
#[test]
fn raw_lanes_compile_or_reject_without_changing_document() {
    use soundcraft_mix::{OfflineRenderer, SessionPreparer, try_render_range};

    for points in [
        vec![AutomationPoint { at: 3, value: -1.0 }, AutomationPoint { at: 1, value: 1.0 }],
        vec![AutomationPoint { at: 0, value: f32::NAN }],
        vec![AutomationPoint { at: 0, value: f32::INFINITY }],
    ] {
        let (mut s, t, _) = fixture(ChannelFormat::Mono);
        let valid = s.clone();
        let reference = render_range(&s, Range::new(0, LEN as i64), 100);
        assert_shape(&reference, 2, LEN);
        assert!(reference.iter().flatten().all(|sample| sample.is_finite()));
        assert!(reference.iter().flatten().any(|sample| sample.abs() > 0.05));
        for param in [AutoParam::SendPan(0), AutoParam::SendMute(0)] {
            let mut lane = AutomationLane::new(param);
            lane.points = points.clone();
            s.track_mut(t).unwrap().automation.push(lane);
        }
        let before: Vec<_> = s.track(t).unwrap().automation.iter().flat_map(|l| l.points.iter().map(|p| (p.at, p.value.to_bits()))).collect();
        if points.iter().all(|point| point.value.is_finite()) {
            let actual = render_range(&s, Range::new(0, LEN as i64), 100);
            assert_shape(&actual, 2, LEN);
            assert!(actual.iter().flatten().all(|sample| sample.is_finite()));
            for (ch, channel) in actual.iter().enumerate() {
                for (n, &sample) in channel.iter().enumerate() {
                    let expected = if n < 3 || ch == 1 { 0.0 } else { f64::from(signal(0, n)) };
                    assert!((f64::from(sample) - expected).abs() < 2e-7, "sorted channel {ch}, at {n}: {sample} vs {expected}");
                }
            }
            assert!(actual.iter().zip(&reference).any(|(a, r)| a.iter().zip(r).any(|(a, r)| (*a - *r).abs() > 0.05)));
            assert_eq!(render_range(&s, Range::new(0, LEN as i64), 100), actual);
        } else {
            let mut preparer = SessionPreparer::default();
            let error = preparer.prepare(Arc::new(s.clone()), 74, 100).err().expect("nonfinite points must reject typed preparation");
            assert_eq!(error.0, "automation contains a nonfinite value");
            let error = try_render_range(&s, Range::new(0, LEN as i64), 100).expect_err("nonfinite points must reject fallible rendering");
            assert_eq!(error.0, "automation contains a nonfinite value");
            let mut renderer = OfflineRenderer::prepare(Arc::new(valid), 100).unwrap();
            let error = renderer.reprepare(Arc::new(s.clone())).expect_err("nonfinite reprepare must fail while preserving the valid renderer");
            assert_eq!(error.0, "automation contains a nonfinite value");
            let mut actual = vec![Vec::new(), Vec::new()];
            let mut start = 0;
            while start < LEN {
                let frames = renderer.mix().max_block().min(LEN - start);
                let mut block = vec![vec![0.0; frames]; 2];
                renderer.render(start as i64, frames, &mut block).unwrap();
                assert_shape(&block, 2, frames);
                assert!(block.iter().flatten().all(|sample| sample.is_finite()));
                for (out, block) in actual.iter_mut().zip(block) {
                    out.extend(block);
                }
                start += frames;
            }
            assert_shape(&actual, 2, LEN);
            assert_eq!(actual, reference);
            assert!(actual.iter().flatten().any(|sample| sample.abs() > 0.05));
            // Missing-instance compatibility does not swallow a nonfinite-plan rejection.
            assert!(render_range(&s, Range::new(0, LEN as i64), 100).is_empty());
        }
        let after: Vec<_> = s.track(t).unwrap().automation.iter().flat_map(|l| l.points.iter().map(|p| (p.at, p.value.to_bits()))).collect();
        assert_eq!(before, after);
    }
}
