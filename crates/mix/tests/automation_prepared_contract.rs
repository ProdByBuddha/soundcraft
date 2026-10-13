//! Independent prepared-path references beyond the frozen baseline tests.
#[path = "support/automation_reference.rs"]
mod reference;
use reference::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::{MixEngine, OfflineRenderer, SessionPreparer};
use soundcraft_model::{
    AutoParam, AutomationLane, AutomationMode, AutomationPoint, ChannelFormat, Clip, Insert, Route, SendSlot, Session, SourceAudio, SourceId,
    TrackId, TrackKind,
};
use std::sync::Arc;
use test_allocation_counter::{CountingSystem, measure};

#[global_allocator]
static ALLOCATOR: CountingSystem = CountingSystem;

fn source(format: ChannelFormat) -> (Session, TrackId) {
    let mut session = Session::default();
    session.outputs[0].format = format;
    let track = session.add_track(TrackKind::Audio, format, Some("Original contract source"));
    let id = SourceId(701);
    let channels = (0..format.channels()).map(|ch| (0..LEN).map(|at| sample(ch, at) as f32).collect()).collect();
    session.pool.insert(id, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = session.new_clip_id();
    let t = session.track_mut(track).unwrap();
    t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", id, 0, 0, LEN as i64));
    t.mixer.pan = if format == ChannelFormat::Mono { vec![0.0] } else { vec![-1.0, 1.0] };
    (session, track)
}
fn sample(ch: usize, at: usize) -> f64 {
    if ch < 2 { signal(ch, at) } else { (ch + 1) as f64 / 32.0 * if (at / 17).is_multiple_of(2) { 1.0 } else { -0.5 } }
}
fn lane(s: &mut Session, t: TrackId, p: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(p);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    s.track_mut(t).unwrap().automation.push(lane);
}
fn make(s: Session) -> OfflineRenderer {
    let mut r = OfflineRenderer::prepare(Arc::new(s), 512).unwrap();
    r.mix_mut().pan_law = PanLaw::Zero;
    r
}
fn render(r: &mut OfflineRenderer, start: usize, frames: usize, channels: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); channels];
    let mut at = 0;
    for size in [1, 7, 37, 64, 100].into_iter().cycle() {
        if at == frames {
            break;
        }
        let n = size.min(frames - at);
        let mut block = vec![vec![0.0; n]; channels];
        r.render((start + at) as i64, n, &mut block).unwrap();
        assert_eq!(block.len(), channels);
        for (dst, src) in out.iter_mut().zip(block) {
            assert_eq!(src.len(), n);
            dst.extend(src);
        }
        at += n;
    }
    out
}
fn ret(s: &mut Session, format: ChannelFormat, name: &str) -> soundcraft_model::BusId {
    let bus = s.add_bus(name, format);
    let aux = s.add_track(TrackKind::Aux, format, Some(name));
    let a = s.track_mut(aux).unwrap();
    a.mixer.input = Route::Bus(bus);
    a.mixer.pan = if format == ChannelFormat::Mono { vec![0.0] } else { vec![-1.0, 1.0] };
    bus
}
fn signed_trim(current: &mut f64, db: f64, inverted: bool) -> f64 {
    let target = db_gain(db.clamp(-144.0, 12.0), -144.0) * if inverted { -1.0 } else { 1.0 };
    let coefficient = (-1.0f64 / (0.010 * 48_000.0)).exp();
    if *current != target {
        *current = target + (*current - target) * coefficient;
        if (*current - target).abs() <= 1e-6 * (1.0 + target.abs()) {
            *current = target;
        }
    }
    *current
}

#[test]
fn simultaneous_trim_targets_interpolate_toggle_then_quantize_before_smoothing() {
    let (mut s, t) = source(ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[9] = Some(Insert::new("trim"));
    let gain = [(0, -6.0), (10, 6.0), (53, -12.0), (111, 3.0), (211, -3.0), (367, 0.0)];
    let invert = [(0, 0.0), (10, 1.0), (53, 0.0), (111, 1.0), (211, 0.0), (367, 1.0)];
    lane(&mut s, t, AutoParam::Plugin { slot: 9, param: "gain".into() }, &gain);
    lane(&mut s, t, AutoParam::Plugin { slot: 9, param: "invert".into() }, &invert);
    let mut current = 1.0;
    let mut wrong_current = 1.0;
    let mut gains = Vec::new();
    let mut wrong = Vec::new();
    for at in 0..LEN {
        gains.push(signed_trim(&mut current, linear(&gain, at as i64), linear(&invert, at as i64).round() >= 1.0));
        wrong.push(signed_trim(&mut wrong_current, linear(&gain, at as i64), step(&invert, at as i64)));
    }
    // A held control must differ: step() is deliberately supplied a separately held curve here.
    let held = |at: usize| invert.iter().rev().find(|p| p.0 <= at as i64).is_some_and(|p| p.1 >= 0.5);
    wrong_current = 1.0;
    for (at, value) in wrong.iter_mut().enumerate() {
        *value = signed_trim(&mut wrong_current, linear(&gain, at as i64), held(at));
    }
    let expected = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * gains[at]).collect()).collect::<Vec<Vec<f64>>>();
    let wrong = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) * wrong[at]).collect()).collect::<Vec<Vec<f64>>>();
    different(&expected, &wrong, 0.01);
    check(&render(&mut make(s), 0, LEN, 2), &expected, 2, LEN, 2e-5);
}

#[test]
fn creation_seed_survives_clear_off_bypass_resume_and_equal_length_key_replacement() {
    let (mut s, t) = source(ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("trim"));
    lane(&mut s, t, AutoParam::Plugin { slot: 0, param: "gain".into() }, &[(0, -12.0)]);
    let mut r = make(s.clone());
    let mut current = 1.0;
    let expected = |start: usize, frames: usize, db: f64, invert: bool, current: &mut f64| {
        let gains = (0..frames).map(|_| signed_trim(current, db, invert)).collect::<Vec<_>>();
        (0..2).map(|ch| (start..start + frames).enumerate().map(|(n, at)| signal(ch, at) * gains[n]).collect()).collect::<Vec<Vec<f64>>>()
    };
    check(&render(&mut r, 0, 97, 2), &expected(0, 97, -12.0, false, &mut current), 2, 97, 2e-5);
    s.track_mut(t).unwrap().mixer.automation_mode = AutomationMode::Off;
    r.reprepare(Arc::new(s.clone())).unwrap();
    check(&render(&mut r, 97, 53, 2), &expected(97, 53, 0.0, false, &mut current), 2, 53, 2e-5);
    s.track_mut(t).unwrap().mixer.automation_mode = AutomationMode::Read;
    s.track_mut(t).unwrap().automation.clear();
    r.reprepare(Arc::new(s.clone())).unwrap();
    check(&render(&mut r, 43, 31, 2), &expected(43, 31, 0.0, false, &mut current), 2, 31, 2e-5);
    lane(&mut s, t, AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN);
    s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().bypass = true;
    r.reprepare(Arc::new(s.clone())).unwrap();
    let bypass = (0..2).map(|ch| (173..211).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut r, 173, 38, 2), &bypass, 2, 38, 2e-6);
    s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().bypass = false;
    r.reprepare(Arc::new(s.clone())).unwrap();
    let gains = (211..264).map(|at| signed_trim(&mut current, linear(PLUGIN_GAIN, at as i64), false)).collect::<Vec<_>>();
    let expected_resume = (0..2).map(|ch| (211..264).enumerate().map(|(n, at)| signal(ch, at) * gains[n]).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut r, 211, 53, 2), &expected_resume, 2, 53, 2e-5);
    s.track_mut(t).unwrap().automation.clear();
    let insert = s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap();
    insert.params.insert("gain".into(), -6.0);
    assert_eq!(insert.params.len(), 1);
    r.reprepare(Arc::new(s.clone())).unwrap();
    check(&render(&mut r, 43, 31, 2), &expected(43, 31, -6.0, false, &mut current), 2, 31, 2e-5);
    let insert = s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap();
    insert.params.clear();
    insert.params.insert("invert".into(), 1.0);
    assert_eq!(insert.params.len(), 1);
    r.reprepare(Arc::new(s)).unwrap();
    check(&render(&mut r, 43, 31, 2), &expected(43, 31, 0.0, true, &mut current), 2, 31, 2e-5);
}

#[test]
fn scalar_send_level_mute_preserve_identity_wide_ambisonic_and_unrelated_routes() {
    for format in [
        ChannelFormat::Stereo,
        ChannelFormat::Surround51,
        ChannelFormat::Surround71,
        ChannelFormat::Atmos714,
        ChannelFormat::Atmos916,
        ChannelFormat::Ambisonics(1),
        ChannelFormat::Ambisonics(2),
        ChannelFormat::Ambisonics(3),
    ] {
        for pre in [false, true] {
            let (mut s, t) = source(format);
            let bus = ret(&mut s, format, "Original automated return");
            let independent = ret(&mut s, format, "Original unaffected return");
            let a = s.track_mut(t).unwrap();
            a.mixer.output = Route::None;
            a.mixer.volume_db = -3.0;
            let mut send = SendSlot::new(Route::Bus(bus));
            send.level_db = 0.0;
            send.pre_fader = pre;
            a.mixer.sends[0] = Some(send);
            let mut other = SendSlot::new(Route::Bus(independent));
            other.level_db = -12.0;
            other.pre_fader = true;
            a.mixer.sends[9] = Some(other);
            lane(&mut s, t, AutoParam::SendLevel(0), SEND_LEVEL);
            lane(&mut s, t, AutoParam::SendMute(0), SEND_MUTE);
            let channels = format.channels();
            let expected = (0..channels)
                .map(|ch| {
                    (43..307)
                        .map(|at| {
                            let automated = if step(SEND_MUTE, at as i64) {
                                0.0
                            } else {
                                db_gain(linear(SEND_LEVEL, at as i64), -144.0) * if pre { 1.0 } else { db_gain(-3.0, -144.0) }
                            };
                            sample(ch, at) * (automated + db_gain(-12.0, -144.0))
                        })
                        .collect()
                })
                .collect::<Vec<Vec<f64>>>();
            check(&render(&mut make(s), 43, 264, channels), &expected, channels, 264, 2e-6);
        }
    }
}

#[test]
fn stereo_balanced_send_to_mono_bus_has_independent_half_sum_reference() {
    let (mut s, t) = source(ChannelFormat::Stereo);
    let bus = ret(&mut s, ChannelFormat::Mono, "Original mono fold return");
    let a = s.track_mut(t).unwrap();
    a.mixer.output = Route::None;
    let mut send = SendSlot::new(Route::Bus(bus));
    send.level_db = 0.0;
    a.mixer.sends[0] = Some(send);
    lane(&mut s, t, AutoParam::SendLevel(0), SEND_LEVEL);
    lane(&mut s, t, AutoParam::SendPan(0), SEND_PAN);
    lane(&mut s, t, AutoParam::SendMute(0), SEND_MUTE);
    let values = (43..307)
        .map(|at| {
            if step(SEND_MUTE, at as i64) {
                return 0.0;
            }
            let p = pan(linear(SEND_PAN, at as i64), None);
            0.5 * (signal(0, at) * p[0] + signal(1, at) * p[1]) * db_gain(linear(SEND_LEVEL, at as i64), -144.0)
        })
        .collect::<Vec<_>>();
    check(&render(&mut make(s), 43, 264, 2), &[values.clone(), values], 2, 264, 2e-6);
}

#[test]
fn all_mode_and_disabled_lane_compatibility_has_independent_combined_pcm() {
    for mode in AutomationMode::ALL {
        let (mut s, t) = source(ChannelFormat::Stereo);
        let bus = ret(&mut s, ChannelFormat::Stereo, "Original mode return");
        let a = s.track_mut(t).unwrap();
        a.mixer.automation_mode = mode;
        a.mixer.volume_db = -3.0;
        a.mixer.inserts[0] = Some(Insert::new("gain"));
        let mut send = SendSlot::new(Route::Bus(bus));
        send.level_db = -6.0;
        a.mixer.sends[0] = Some(send);
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
            lane(&mut s, t, param, points);
        }
        for l in &mut s.track_mut(t).unwrap().automation {
            l.enabled = false;
        }
        let mut gain = GainMemory::new();
        gain.reset(); // Fresh independent history: both current and target are one.
        let mut expected = vec![vec![0.0; LEN]; 2];
        let reads = mode != AutomationMode::Off;
        for at in 0..LEN {
            let time = at as i64;
            let plugin = gain.next(if reads { linear(PLUGIN_GAIN, time) } else { 0.0 });
            let volume = db_gain(if reads && mode != AutomationMode::Write { linear(VOLUME, time) } else { -3.0 }, -144.0);
            let gate = if reads && step(MUTE, time) { 0.0 } else { 1.0 };
            let p0 = pan(if reads { linear(PAN0, time) } else { -1.0 }, None);
            let p1 = pan(if reads { linear(PAN1, time) } else { 1.0 }, None);
            let balance = pan(if reads { linear(SEND_PAN, time) } else { 0.0 }, None);
            let send = if reads && step(SEND_MUTE, time) { 0.0 } else { db_gain(if reads { linear(SEND_LEVEL, time) } else { -6.0 }, -144.0) };
            for (ch, out) in expected.iter_mut().enumerate() {
                out[at] = plugin * volume * gate * (signal(0, at) * p0[ch] + signal(1, at) * p1[ch] + signal(ch, at) * balance[ch] * send);
            }
        }
        check(&render(&mut make(s), 0, LEN, 2), &expected, 2, LEN, 3e-5);
    }
}

#[test]
fn prepared_install_and_steady_render_have_zero_observed_thread_allocations() {
    let (mut s, t) = source(ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    lane(&mut s, t, AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN);
    lane(&mut s, t, AutoParam::Volume, VOLUME);
    let mut prepare = SessionPreparer::default();
    let update = prepare.prepare(Arc::new(s.clone()), 42, 512).unwrap();
    let mut mixer = MixEngine::new(48_000.0, 512);
    let (retired, allocations) = measure(|| mixer.install_prepared(update));
    assert_eq!(allocations, 0);
    drop(retired);
    let mut block = vec![vec![0.0; 37]; 2];
    let mut history = GainMemory::new();
    for start in [0usize, 37, 74, 43, 211] {
        let (result, allocations) = measure(|| mixer.render_prepared(start as i64, 37, &mut block));
        result.unwrap();
        assert_eq!(allocations, 0);
        let gains = (start..start + 37)
            .map(|at| history.next(linear(PLUGIN_GAIN, at as i64)) * db_gain(linear(VOLUME, at as i64), -144.0))
            .collect::<Vec<_>>();
        let expected = (0..2).map(|ch| (start..start + 37).enumerate().map(|(n, at)| signal(ch, at) * gains[n]).collect()).collect::<Vec<Vec<f64>>>();
        check(&block, &expected, 2, 37, 3e-5);
    }
    s.track_mut(t).unwrap().comments = "Original retained workspace".into();
    let update = prepare.prepare(Arc::new(s), 42, 512).unwrap();
    assert!(update.retains_instance(t, 0));
    let (retired, allocations) = measure(|| mixer.install_prepared(update));
    assert_eq!(allocations, 0);
    drop(retired);
    let (_, positive) = measure(|| std::hint::black_box(Box::new(std::hint::black_box(48u64))));
    assert!(positive > 0, "counter observes a deliberate allocation");
}

#[test]
fn aggregate_rejection_preserves_prior_preparer_instance_identity() {
    let (mut s, t) = source(ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    let mut prepare = SessionPreparer::default();
    let first = prepare.prepare(Arc::new(s.clone()), 42, 512).unwrap();
    let mut mix = MixEngine::new(48_000.0, 512);
    drop(mix.install_prepared(first));
    let mut excessive = s.clone();
    for _ in 0..512 {
        let added = excessive.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original budget control"));
        for slot in &mut excessive.track_mut(added).unwrap().mixer.inserts {
            *slot = Some(Insert::new("clap:original.capacity.unavailable"));
        }
    }
    let failure = prepare.prepare(Arc::new(excessive), 42, 512);
    let error = match failure {
        Ok(_) => panic!("aggregate admission must refuse before unavailable hosted creation"),
        Err(error) => error,
    };
    assert!(error.0.contains("capacity"), "admission failure precedes unavailable instance lookup: {error}");
    let update = prepare.prepare(Arc::new(s), 42, 512).unwrap();
    assert!(update.retains_instance(t, 0));
    drop(mix.install_prepared(update));
    let mut output = vec![vec![0.0; 31]; 2];
    mix.render_prepared(43, 31, &mut output).unwrap();
    let expected = (0..2).map(|ch| (43..74).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&output, &expected, 2, 31, 2e-6);
}

#[test]
fn missing_plugins_remain_strict_errors_and_legacy_dry_passthrough() {
    use soundcraft_mix::{PreparationError, render_bus, render_range, try_render_bus, try_render_range};
    use soundcraft_time::Range;

    // A valid hosted ID must never discover ambient modules in this controlled case.
    assert_eq!(std::env::var("SOUNDCRAFT_CLAP_SEARCH_PATHS").as_deref(), Ok(""));
    let unavailable = |error: PreparationError| assert_eq!(error.0, "plugin instance unavailable");
    let frames = 64;
    let source_id = SourceId(702);
    let amplitude = 0.25f32;
    // The default panner specifies nominal -3 dB at center, rather than -3.0103 dB.
    let expected = (f64::from(amplitude) * 10f64.powf(-3.0 / 20.0)) as f32;
    assert!((expected - amplitude * std::f32::consts::FRAC_1_SQRT_2).abs() > 1e-6);
    for missing in ["original-unknown-builtin-strict-compat", "clap:org.soundcraft.original.missing-strict-compat"] {
        let mut original = Session::default();
        let bus = original.add_bus("Original dry reference", ChannelFormat::Stereo);
        let track = original.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Original missing-plugin source"));
        let audio = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![vec![amplitude; frames]] }));
        original.pool.insert(source_id, Arc::clone(&audio));
        let clip = original.new_clip_id();
        let t = original.track_mut(track).unwrap();
        t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original DC", source_id, 0, 0, frames as i64));
        t.mixer.pan = vec![0.0];
        t.comments = "Original source/document correlation".into();
        let mut insert = Insert::new(missing);
        insert.params.insert("OriginalExactKey".into(), 0.75);
        insert.set_state_bytes(b"Original missing-plugin state");
        t.mixer.inserts[0] = Some(insert);
        for routed_to_bus in [false, true] {
            let mut session = original.clone();
            session.track_mut(track).unwrap().mixer.output = if routed_to_bus { Route::Bus(bus) } else { Route::Main };
            let document = session.to_json().unwrap();
            let missing_insert = session.track(track).unwrap().mixer.inserts[0].clone();
            let shared = Arc::new(session.clone());
            let range = Range::new(0, frames as i64);
            let mut preparer = SessionPreparer::default();
            unavailable(preparer.prepare(Arc::clone(&shared), 73, 64).err().expect("typed preparation must reject an unavailable instance"));
            unavailable(
                OfflineRenderer::prepare(Arc::clone(&shared), 64).err().expect("typed offline preparation must reject an unavailable instance"),
            );
            unavailable(try_render_range(&session, range, 64).expect_err("fallible main rendering must reject an unavailable instance"));
            unavailable(try_render_bus(&session, bus, range, 64).expect_err("fallible bus rendering must reject an unavailable instance"));
            let dry = if routed_to_bus { render_bus(&session, bus, range, 64) } else { render_range(&session, range, 64) };
            assert_eq!(dry.len(), 2, "legacy output shape for {missing}");
            for channel in &dry {
                assert_eq!(channel.len(), frames);
                for (frame, sample) in channel.iter().enumerate() {
                    assert!(
                        sample.is_finite() && (*sample - expected).abs() < 1e-6,
                        "audible dry reference for {missing}, bus={routed_to_bus}, frame={frame}: {sample} != {expected}"
                    );
                }
            }
            assert!(dry.iter().flatten().any(|sample| sample.abs() > 0.1), "the dry output must be audible");
            assert_eq!(session.to_json().unwrap(), document);
            assert_eq!(shared.to_json().unwrap(), document);
            assert!(Arc::ptr_eq(session.pool.get(source_id).unwrap(), &audio));
            assert!(Arc::ptr_eq(shared.pool.get(source_id).unwrap(), &audio));
            assert_eq!(audio.buffer.channels, vec![vec![amplitude; frames]]);
            assert_eq!(session.track(track).unwrap().mixer.inserts[0], missing_insert);
            assert_eq!(session.track(track).unwrap().mixer.inserts[0].as_ref().unwrap().plugin, missing);
        }
    }
}
