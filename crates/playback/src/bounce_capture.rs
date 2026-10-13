//! Controlled invocation of the actual Prepared/Play/CallbackOwner pipeline, with independent
//! signal laws and decoded files. No physical audio device is opened or claimed as accepted.
#![cfg(not(target_arch = "wasm32"))]

use soundcraft_model::{AutoParam, AutomationLane, AutomationPoint, Route, SendSlot};

include!("bounce_support.rs");

const TOLERANCE: f64 = 2e-5;
const SCHEDULE: &[usize] = &[1, 7, 37, 64, 511, 512, 513, 1024];

fn audible(actual: &[Vec<f32>]) {
    assert!(actual.iter().flatten().any(|value| value.abs() > 0.001));
}
fn contrast(expected: &[Vec<f64>], wrong: &[Vec<f64>]) {
    assert!(expected.iter().zip(wrong).any(|(a, b)| a.iter().zip(b).any(|(a, b)| (a - b).abs() > TOLERANCE * 10.0)));
}

impl Harness {
    fn start(&mut self, range: Range, tail: i64, preroll: i64) -> usize {
        let mut state = self.owner.exchange.receiver.try_recv().unwrap();
        state.mix.set_source_cutoff(Some(range.end)).unwrap();
        let latency = state.mix.prepared_output_latency(None).unwrap();
        assert!(self.owner.exchange.sender.try_send(state).is_ok());
        self.player.play(preroll, Some(range.end + tail + latency as i64), None);
        latency
    }
    fn window(&mut self, range: Range, tail: i64, preroll: i64, expected_latency: usize, schedule: &[usize]) -> Vec<Vec<f32>> {
        let latency = self.start(range, tail, preroll);
        assert_eq!(latency, expected_latency);
        let discard = usize::try_from(range.start - preroll).unwrap() + latency;
        let frames = usize::try_from(range.len() + tail).unwrap();
        let mut result = self.fill(discard + frames, schedule);
        for channel in &mut result {
            channel.drain(..discard);
            assert_eq!(channel.len(), frames);
        }
        result
    }
}

#[test]
fn cold_and_continuous_selection_gain_matches_callback_file_and_independent_twenty_ms_law() {
    let (mut s, t) = session();
    audio(&mut s, t, 0, 2048, 1.0);
    insert(&mut s, t, "gain", &[("gain", 12.0)]);
    let range = Range::new(137, 1100);
    let frames = range.len() as usize;
    let mut references = Vec::new();
    for preroll in [137, 0] {
        let mut current = 1.0f64;
        let target = 10f64.powf(12.0 / 20.0);
        let coefficient = (-1.0f64 / (0.020 * 48_000.0)).exp();
        let mut expected = vec![Vec::new(); 2];
        for at in preroll..range.end {
            current = target + (current - target) * coefficient;
            if (current - target).abs() <= 1e-6 * (1.0 + target.abs()) {
                current = target;
            }
            if at >= range.start {
                for (ch, channel) in expected.iter_mut().enumerate() {
                    channel.push(signal(ch, at) * current);
                }
            }
        }
        let source = Arc::new(s.clone());
        let mut callback = Harness::new(source);
        let actual = callback.window(range, 0, preroll, 0, SCHEDULE);
        audible(&actual);
        compare(&actual, &expected, frames, TOLERANCE);
        let decoded = exported(&mut Engine::new(s.clone()), range, 0, preroll, None);
        compare(&decoded, &expected, frames, TOLERANCE);
        compare(&decoded, &actual.iter().map(|ch| ch.iter().map(|value| f64::from(*value)).collect()).collect::<Vec<_>>(), frames, 1e-6);
        references.push(expected);
    }
    contrast(&references[0], &references[1]);
}

#[test]
fn time_shift_tail_stops_opposing_later_clip_and_matches_actual_callbacks_at_every_chunk_size() {
    let (mut s, t) = session();
    s.edit.delay_compensation = false;
    audio(&mut s, t, 0, 300, 1.0);
    audio(&mut s, t, 300, 100, -8.0);
    insert(&mut s, t, "time_shift", &[("delay_samples", 7.0)]);
    let range = Range::new(100, 300);
    let tail = 12;
    let frames = (range.len() + tail) as usize;
    let mut references = Vec::new();
    for preroll in [100, 0] {
        let mut expected = vec![Vec::new(); 2];
        let mut wrong = vec![Vec::new(); 2];
        for at in range.start..range.end + tail {
            let age = at - preroll;
            let fraction = (age as f64 / 256.0).min(1.0);
            for ch in 0..2 {
                let input = |n| if n >= preroll && n < range.end { signal(ch, n) } else { 0.0 };
                expected[ch].push((1.0 - fraction) * input(at) + fraction * input(at - 7));
                let leaked = |n| {
                    if n < preroll {
                        0.0
                    } else if n < range.end {
                        signal(ch, n)
                    } else {
                        signal(ch, n) * -8.0
                    }
                };
                wrong[ch].push((1.0 - fraction) * leaked(at) + fraction * leaked(at - 7));
            }
        }
        contrast(&expected, &wrong);
        assert!(expected[0][(range.len() as usize)..].iter().any(|value| value.abs() > 0.001));
        for &size in SCHEDULE {
            let actual = Harness::new(Arc::new(s.clone())).window(range, tail, preroll, 0, &[size]);
            compare(&actual, &expected, frames, TOLERANCE);
            audible(&actual);
        }
        compare(&exported(&mut Engine::new(s.clone()), range, tail, preroll, None), &expected, frames, TOLERANCE);
        references.push(expected);
    }
    contrast(&references[0], &references[1]);
}

fn compressor(s: &mut Session, t: TrackId, milliseconds: f32) {
    insert(s, t, "compressor", &[("lookahead", milliseconds), ("ratio", 1.0), ("threshold", 0.0)]);
}
#[test]
fn serial_aux_master_and_each_output_send_align_and_named_bus_has_upstream_arrival() {
    let (mut s, t) = session();
    s.edit.delay_compensation = true;
    audio(&mut s, t, 0, 2048, 1.0);
    compressor(&mut s, t, 4.0);
    let a = s.add_bus("Arrival A", ChannelFormat::Stereo);
    let b = s.add_bus("Arrival B", ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.sends[0] = Some(SendSlot::new(Route::Bus(a)));
    let aux_a = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Serial A"));
    let aux_b = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Serial B"));
    for (track, input, output, latency) in [(aux_a, a, Route::Bus(b), 1.0), (aux_b, b, Route::Main, 2.0)] {
        let aux = s.track_mut(track).unwrap();
        aux.mixer.input = Route::Bus(input);
        aux.mixer.output = output;
        aux.mixer.pan = vec![-1.0, 1.0];
        compressor(&mut s, track, latency);
    }
    let master = s.add_track(TrackKind::Master, ChannelFormat::Stereo, Some("Serial Master"));
    compressor(&mut s, master, 3.0);
    let dry = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Independent direct"));
    s.track_mut(dry).unwrap().mixer.pan = vec![-1.0, 1.0];
    audio(&mut s, dry, 0, 2048, 0.5);
    // SendSlot's default attenuation is deliberately replaced by a known unity route.
    s.track_mut(t).unwrap().mixer.sends[0].as_mut().unwrap().level_db = 0.0;
    let range = Range::new(137, 1600);
    let tail = 31;
    let frames = (range.len() + tail) as usize;
    let expected: Vec<Vec<f64>> =
        (0..2).map(|ch| (range.start..range.end + tail).map(|at| if at < range.end { signal(ch, at) * 2.5 } else { 0.0 }).collect()).collect();
    let actual = Harness::new(Arc::new(s.clone())).window(range, tail, 0, 480, SCHEDULE);
    compare(&actual, &expected, frames, TOLERANCE);
    audible(&actual);
    compare(&exported(&mut Engine::new(s.clone()), range, tail, 0, None), &expected, frames, TOLERANCE);
    let bus: Vec<Vec<f64>> =
        (0..2).map(|ch| (range.start..range.end + tail).map(|at| if at < range.end { signal(ch, at) } else { 0.0 }).collect()).collect();
    assert_eq!(soundcraft_mix::OfflineRenderer::prepare(Arc::new(s.clone()), BLOCK).unwrap().mix().prepared_output_latency(Some(b)).unwrap(), 240);
    compare(&exported(&mut Engine::new(s.clone()), range, tail, 0, Some("Arrival B")), &bus, frames, TOLERANCE);
    s.track_mut(aux_a).unwrap().mixer.input = Route::Bus(b);
    assert!(SessionPreparer::default().prepare(Arc::new(s.clone()), LIFETIME, BLOCK).is_err(), "enabled cyclic PDC must refuse before rendering");
    s.edit.delay_compensation = false;
    assert!(SessionPreparer::default().prepare(Arc::new(s), LIFETIME, BLOCK).is_ok(), "disabled compatibility routing remains preparable");
}

#[test]
fn overlapping_dry_and_delayed_clip_effects_align_with_independent_pcm_and_finite_flush() {
    let (mut s, t) = session();
    s.edit.delay_compensation = true;
    let delayed = audio(&mut s, t, 0, 300, 1.0);
    audio(&mut s, t, 0, 300, 0.5);
    audio(&mut s, t, 300, 100, -8.0);
    for (key, value) in [("lookahead", 2.0), ("ratio", 1.0), ("threshold", 0.0)] {
        s.edit.values.insert(format!("clip_fx.{}.comp.{key}", delayed.0), value);
    }
    let range = Range::new(0, 300);
    let tail = 23;
    let frames = 323;
    let expected: Vec<Vec<f64>> = (0..2).map(|ch| (0..frames).map(|at| if at < 300 { signal(ch, at as i64) * 1.5 } else { 0.0 }).collect()).collect();
    let wrong: Vec<Vec<f64>> = (0..2)
        .map(|ch| {
            (0..frames)
                .map(|at| if at < 300 { signal(ch, at as i64) } else { 0.0 } + if at + 96 < 300 { signal(ch, (at + 96) as i64) * 0.5 } else { 0.0 })
                .collect()
        })
        .collect();
    contrast(&expected, &wrong);
    for &size in SCHEDULE {
        let actual = Harness::new(Arc::new(s.clone())).window(range, tail, 0, 96, &[size]);
        compare(&actual, &expected, frames, TOLERANCE);
    }
    compare(&exported(&mut Engine::new(s), range, tail, 0, None), &expected, frames, TOLERANCE);
}

#[test]
fn zero_input_clip_eq_tail_matches_independent_first_order_recurrence() {
    let (mut s, t) = session();
    let clip = audio(&mut s, t, 0, 300, 1.0);
    for (key, value) in [("lpf_on", 1.0), ("lpf_freq", 1000.0), ("lpf_slope", 0.0)] {
        s.edit.values.insert(format!("clip_fx.{}.eq.{key}", clip.0), value);
    }
    let range = Range::new(100, 300);
    let tail = 64;
    let frames = 264;
    // Independent bilinear-transform first-order LPF. Expected arithmetic uses f64;
    // coefficient rounding is part of the declared single-precision DSP contract.
    let k = (std::f64::consts::PI * 1000.0 / 48_000.0).tan();
    let b = f64::from((k / (1.0 + k)) as f32);
    let a = f64::from(((k - 1.0) / (1.0 + k)) as f32);
    let mut expected = vec![Vec::new(); 2];
    for (ch, output) in expected.iter_mut().enumerate() {
        let mut previous_input = 0.0;
        let mut previous_output = 0.0;
        for at in 0..range.end + tail {
            let input = if at < range.end { signal(ch, at) } else { 0.0 };
            let value = b * input + b * previous_input - a * previous_output;
            previous_input = input;
            previous_output = value;
            if at >= range.start {
                output.push(value);
            }
        }
    }
    assert!(expected[0][200..].iter().any(|value| value.abs() > 0.001));
    let actual = Harness::new(Arc::new(s.clone())).window(range, tail, 0, 0, SCHEDULE);
    compare(&actual, &expected, frames, TOLERANCE);
    compare(&exported(&mut Engine::new(s), range, tail, 0, None), &expected, frames, TOLERANCE);
}

#[test]
fn real_delayed_instrument_latency_note_cutoff_ring_tail_and_file_match_absolute_note_reference() {
    let range = Range::new(0, 200);
    let tail = 15;
    let frames = 215;
    for compensated in [false, true] {
        let (s, _) = instrument(compensated, 0.75);
        let delay = if compensated { 0 } else { 7 };
        let expected =
            vec![(0..frames).map(|at| if (50 + delay..200 + delay).contains(&at) { 64.0 / 127.0 * 0.75 } else { 0.0 }).collect::<Vec<_>>(); 2];
        for &size in SCHEDULE {
            let actual = Harness::new(Arc::new(s.clone())).window(range, tail, 0, if compensated { 7 } else { 0 }, &[size]);
            compare(&actual, &expected, frames, 1e-6);
            audible(&actual);
        }
        compare(&exported(&mut Engine::new(s), range, tail, 0, None), &expected, frames, 1e-6);
    }
}

#[test]
fn hosted_opaque_read_state_does_not_replace_missing_static_seed_after_off_or_clear() {
    fixtures();
    let vst = if cfg!(windows) { "vst3:001D0A5C111122223333444455556666" } else { "vst3:5C0A1D00111122223333444455556666" };
    for plugin in [CLAP_GAIN, vst] {
        let mut prepared = soundcraft_mix::create_instance(plugin, None, 48_000.0, BLOCK, 2).unwrap();
        assert!(prepared.set_param("7", 1.25));
        prepared.process(&mut vec![vec![0.0; 64]; 2], 64);
        let opaque = prepared.save_state().unwrap();
        drop(prepared);
        let (mut s, t) = session();
        s.edit.delay_compensation = false;
        audio(&mut s, t, 0, 1000, 1.0);
        let mut insert = Insert::new(plugin);
        insert.state = Some(b64::encode(&opaque));
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(insert);
        let mut lane = AutomationLane::new(AutoParam::Plugin { slot: 0, param: "7".into() });
        lane.points = vec![AutomationPoint { at: 0, value: 1.75 }];
        s.track_mut(t).unwrap().automation.push(lane);
        let original = Engine::new(s);
        let source = original.session_arc();
        let original_json = original.session().to_json().unwrap();
        let mut callback = Harness::new(Arc::clone(&source));
        // The owned VST3 controller reports its known setup edit even while closed.
        // Isolate this no-editor regression without discarding a real capture gesture.
        let setup_edits = callback.player.idle();
        if plugin == vst {
            assert_eq!(setup_edits, vec![(t, 0, "7".into(), 1.8)]);
        } else {
            assert!(setup_edits.is_empty());
        }
        callback.player.play(0, None, None);
        let actual = callback.fill(64, &[64]);
        compare(&actual, &(0..2).map(|ch| (0..64).map(|at| signal(ch, at) * 1.75).collect()).collect::<Vec<_>>(), 64, 1e-6);
        callback.player.stop();
        callback.fill(1, &[1]);
        for clear in [false, true] {
            let snapshot = callback.snapshot(Arc::clone(&source));
            assert!(snapshot.editor_edits.is_empty(), "the no-editor control must not contain a newer user edit");
            assert!(snapshot.static_baselines.contains(&(t, 0, "7".into(), 1.25)));
            assert_eq!(snapshot.states.len(), 1);
            let mut copy = detached(&original, snapshot);
            if clear {
                copy.session_mut().track_mut(t).unwrap().automation.clear();
            } else {
                copy.session_mut().track_mut(t).unwrap().mixer.automation_mode = soundcraft_model::AutomationMode::Off;
            }
            let expected = (0..2).map(|ch| (0..200).map(|at| signal(ch, at) * 1.25).collect()).collect::<Vec<_>>();
            let transient = (0..2).map(|ch| (0..200).map(|at| signal(ch, at) * 1.75).collect()).collect::<Vec<_>>();
            contrast(&expected, &transient);
            let actual = Harness::new(copy.session_arc()).window(Range::new(0, 200), 0, 0, 0, SCHEDULE);
            compare(&actual, &expected, 200, 1e-6);
            audible(&actual);
            compare(&exported(&mut copy, Range::new(0, 200), 0, 0, None), &expected, 200, 1e-6);
            assert_eq!(original.session().to_json().unwrap(), original_json);
            assert!(!original.session().track(t).unwrap().mixer.inserts[0].as_ref().unwrap().params.contains_key("7"));
        }
    }
}

#[test]
fn actual_host_nonfinite_pcm_refuses_delivery_without_file_or_document_changes() {
    for mode in [1.0, 2.0] {
        let (mut s, t) = instrument(false, 0.75);
        s.track_mut(t).unwrap().instrument.as_mut().unwrap().params.insert("3".into(), mode);
        let mut engine = Engine::new(s);
        engine.execute("track.comments", &json!({"track":t.0,"comments":"Seeded redo must survive invalid PCM"})).unwrap();
        assert!(engine.undo());
        let before = engine.session_arc();
        let document = before.to_json().unwrap();
        let before_history = (engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone());
        let directory = Directory::new("nonfinite-refusal");
        let path = directory.0.join("MustNotExist.wav");
        let error = engine
            .execute("file.bounce_mix", &json!({"path":path,"start":0,"end":200,"tail_samples":15,"bit_depth":"32f","dither":false}))
            .unwrap_err();
        assert!(error.to_string().contains("nonfinite"), "{error}");
        assert!(!path.exists());
        assert!(Arc::ptr_eq(&before, &engine.session_arc()));
        assert_eq!(engine.session().to_json().unwrap(), document);
        assert_eq!((engine.revision, engine.is_dirty(), engine.undo_history(), engine.journal.clone(), engine.path.clone()), before_history);
        assert!(engine.redo());
        assert_eq!(engine.session().track(t).unwrap().comments, "Seeded redo must survive invalid PCM");
    }
}

#[test]
fn same_width_destination_layout_update_discards_old_mapped_compensation_samples() {
    let (mut s, fast) = session();
    s.edit.delay_compensation = true;
    audio(&mut s, fast, 0, 17, 1.0);
    let bus = s.add_bus("Four-channel layout transition", ChannelFormat::Quad);
    s.track_mut(fast).unwrap().mixer.output = Route::Bus(bus);
    let slow = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Silent latency peer"));
    s.track_mut(slow).unwrap().mixer.output = Route::Bus(bus);
    compressor(&mut s, slow, 1.0);
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Quad, Some("Layout return"));
    s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
    let source = Arc::new(s);
    let mut callback = Harness::new(Arc::clone(&source));
    callback.player.play(0, None, None);
    let before = callback.fill(17, &[17]);
    compare(&before, &vec![vec![0.0; 17]; 2], 17, 1e-12);
    let mut changed = (*source).clone();
    changed.busses.iter_mut().find(|candidate| candidate.id == bus).unwrap().format = ChannelFormat::Lcrs;
    changed.track_mut(aux).unwrap().format = ChannelFormat::Lcrs;
    callback.player.try_update_session(Arc::new(changed), LIFETIME).unwrap();
    // Source input ended before the update; the silent peer has no DSP history. Any
    // audible output now can only be stale Quad-mapped ring data reused as LCRS.
    let after = callback.fill(64, &[1, 7, 37, 19]);
    compare(&after, &vec![vec![0.0; 64]; 2], 64, 1e-12);
}

#[test]
fn owned_clap_and_vst3_instrument_flags_silence_and_resume_only_future_notes_in_mono_and_stereo() {
    let vst_synth = if cfg!(windows) { "vst3:021D0A5C111122223333444455556666" } else { "vst3:5C0A1D02111122223333444455556666" };
    for (plugin, delay, gain) in [(DELAY_SYNTH, 7, 0.75), (vst_synth, 0, 1.0)] {
        for mono in [false, true] {
            for by_bypass in [false, true] {
                let (mut s, track) = instrument(false, gain as f32);
                let t = s.track_mut(track).unwrap();
                let ins = t.instrument.as_mut().unwrap();
                ins.plugin = plugin.into();
                if plugin == vst_synth {
                    ins.params.clear();
                }
                if mono {
                    t.format = ChannelFormat::Mono;
                    t.mixer.pan = vec![0.0];
                }
                let source = Arc::new(s);
                let original_json = source.to_json().unwrap();
                let mut callback = Harness::new(Arc::clone(&source));
                // Nominal mono instruments retain their actual stereo output. With
                // one stored pan, the zero-law inputs are centered and hard right,
                // giving independent channel gains [1, 2]; the device clips [-1, 1].
                let mut state = callback.owner.exchange.receiver.try_recv().unwrap();
                state.mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
                assert!(callback.owner.exchange.sender.try_send(state).is_ok());
                callback.player.play(0, None, None);
                let first = callback.fill(100, SCHEDULE);
                let expected = (0..2)
                    .map(|channel| {
                        (0..100)
                            .map(|at| {
                                let amplitude: f64 = if at >= 50 + delay { 64.0 / 127.0 * gain } else { 0.0 };
                                (amplitude * if mono && channel == 1 { 2.0 } else { 1.0 }).clamp(-1.0, 1.0)
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                compare(&first, &expected, 100, 1e-6);
                audible(&first);
                let mut disabled = (*source).clone();
                let ins = disabled.track_mut(track).unwrap().instrument.as_mut().unwrap();
                if by_bypass {
                    ins.bypass = true;
                } else {
                    ins.active = false;
                }
                let disabled = Arc::new(disabled);
                callback.player.try_update_session(Arc::clone(&disabled), LIFETIME).unwrap();
                // Exercise a real retained voice followed by many separate disabled spans.
                // The original VST3 fixture's processor keeps that voice across host reset.
                let silent = callback.fill(100, &[1]);
                compare(&silent, &vec![vec![0.0; 100]; 2], 100, 1e-12);
                let mut state = callback.owner.exchange.receiver.try_recv().unwrap();
                assert_eq!(state.pos, 200);
                let states = state.mix.try_capture_states().unwrap();
                let baselines = state.mix.try_capture_static_baselines().unwrap();
                if !by_bypass {
                    assert!(states.is_empty());
                    assert!(baselines.is_empty());
                } else if plugin == DELAY_SYNTH {
                    // Active bypass preserves opaque state and omitted static baselines,
                    // even though this instrument's audio processing is disabled.
                    let opaque = [0.75f64.to_le_bytes(), 0.0f64.to_le_bytes()].concat();
                    assert_eq!(states, vec![(track, soundcraft_mix::INSTRUMENT_SLOT, opaque)]);
                    assert_eq!(baselines, vec![(track, soundcraft_mix::INSTRUMENT_SLOT, "3".into(), 0.0)]);
                } else {
                    // The original VST3 Synth returns successful empty component and
                    // controller streams; the host's literal container still retains both.
                    let opaque = [b"SCV3".as_slice(), &[0u8; 8]].concat();
                    assert_eq!(states, vec![(track, soundcraft_mix::INSTRUMENT_SLOT, opaque)]);
                    assert!(baselines.is_empty());
                }
                assert!(callback.owner.exchange.sender.try_send(state).is_ok());
                compare(&exported(&mut Engine::new((*disabled).clone()), Range::new(0, 200), 15, 0, None), &vec![vec![0.0; 215]; 2], 215, 1e-12);
                callback.player.try_update_session(Arc::clone(&source), LIFETIME).unwrap();
                let resumed = callback.fill(100, SCHEDULE);
                // No chase of the note that started at50; the independently timed second
                // note starts at225, then the CLAP fixture adds its real7-sample delay.
                let expected = (0..2)
                    .map(|channel| {
                        (0..100)
                            .map(|at| {
                                let amplitude: f64 = if at >= 25 + delay { 120.0 / 127.0 * gain } else { 0.0 };
                                (amplitude * if mono && channel == 1 { 2.0 } else { 1.0 }).clamp(-1.0, 1.0)
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                compare(&resumed, &expected, 100, 1e-6);
                audible(&resumed);
                assert_eq!(source.to_json().unwrap(), original_json);
            }
        }
    }
}

#[test]
fn newly_prepared_disabled_opaque_instrument_quiesces_before_future_notes() {
    for by_bypass in [false, true] {
        let (mut s, track) = instrument(false, 0.75);
        let ins = s.track_mut(track).unwrap().instrument.as_mut().unwrap();
        ins.params.clear();
        let state = [1.25f64.to_le_bytes(), 0.0f64.to_le_bytes()].concat();
        ins.state = Some(b64::encode(&state));
        if by_bypass {
            ins.bypass = true;
        } else {
            ins.active = false;
        }
        let source = Arc::new(s);
        let original_json = source.to_json().unwrap();
        let mut callback = Harness::new(Arc::clone(&source));
        callback.player.play(0, None, None);
        compare(&callback.fill(1, &[1]), &vec![vec![0.0; 1]; 2], 1, 1e-12);
        let mut resumed = (*source).clone();
        let ins = resumed.track_mut(track).unwrap().instrument.as_mut().unwrap();
        ins.active = true;
        ins.bypass = false;
        callback.player.try_update_session(Arc::new(resumed), LIFETIME).unwrap();
        let actual = callback.fill(100, SCHEDULE);
        // Absolute note50 + actual7delay, relative to resumed position1. The state16
        // restores Gain1.25 independently; no model parameter is fabricated.
        let expected = vec![(0..100).map(|at| if at >= 56 { 64.0 / 127.0 * 1.25 } else { 0.0 }).collect::<Vec<_>>(); 2];
        compare(&actual, &expected, 100, 1e-6);
        audible(&actual);
        assert_eq!(source.to_json().unwrap(), original_json);
    }
}
