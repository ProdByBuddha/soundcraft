//! SC-ED46 independent retained-clip acceptance. No production crop or gain helper is an oracle.
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_dsp::{Category, Plugin, PluginInfo};
use soundcraft_engine::{Engine, io};
use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
use soundcraft_mix::{INSTRUMENT_SLOT, MixEngine, PreparedInstance};
use soundcraft_model::{ChannelFormat, Clip, ClipContent, ClipId, Fade, FadeShape, Insert, Session, SourceId, TrackId, TrackKind, VideoSource};
use std::sync::{Arc, Mutex};

const ENV: &[(i64, f64)] = &[(0, -3.0), (19, 5.0), (81, -7.0), (191, 2.0), (421, -5.0)];
fn signal(ch: usize, at: usize) -> f32 {
    let i = at as f64;
    if ch == 0 {
        (0.05 + 0.001 * i + 0.03 * (0.047 * i + 0.0003 * i * i).sin()) as f32
    } else {
        (-0.12 + 0.0007 * i + 0.08 * (0.023 * i + 0.0004 * i * i).cos()) as f32
    }
}
fn fixture(stretch: f64, shape: FadeShape) -> (Engine, TrackId, ClipId, SourceId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Independent original"));
    let source = io::add_source(
        &mut s,
        "Original synthetic PCM",
        AudioBuffer { sample_rate: 48_000, channels: (0..2).map(|ch| (0..1024).map(|i| signal(ch, i)).collect()).collect() },
        None,
        FileFormat::Wav,
    );
    let id = s.new_clip_id();
    let mut clip = Clip::audio(id, "Untouched parent", source, 31, 211, 421);
    clip.stretch = stretch;
    clip.gain_db = -2.0;
    clip.gain_env = ENV.iter().map(|&(i, db)| (i, db as f32)).collect();
    clip.fade_in = Fade { len: 97, shape };
    clip.fade_out = Fade { len: 173, shape: FadeShape::Exponential };
    clip.rating = 4;
    clip.color = Some([17, 83, 191]);
    clip.sync_point = 83;
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(clip);
    s.track_mut(t).unwrap().mixer.pan = vec![-1.0, 1.0];
    s.edit.selected_tracks = vec![t];
    s.edit.selected_clips = vec![id];
    s.edit.selection = soundcraft_time::Range::new(248, 515);
    let mut e = Engine::new(s);
    e.mark_clean();
    (e, t, id, source)
}
fn capture(e: &mut Engine, id: ClipId, start: i64, end: i64) -> u64 {
    let result = e.execute("clip.capture", &json!({"clip":id,"start":start,"end":end,"name":"Retained middle"})).unwrap();
    assert_eq!(result["count"], 1);
    assert_eq!(result["definitions"][0]["start"], start);
    assert_eq!(result["definitions"][0]["end"], end);
    result["definitions"][0]["id"].as_u64().unwrap()
}
fn definitions(e: &mut Engine) -> Value {
    e.execute("clip.definitions", &json!({})).unwrap()["definitions"].clone()
}
fn place(e: &mut Engine, definition: u64, t: TrackId, at: i64) -> ClipId {
    let r = e.execute("clip.place_definition", &json!({"definition":definition,"track":t,"at":at})).unwrap();
    assert_eq!(r["definition"], definition);
    assert_eq!(r["track"], t.0);
    assert_eq!(r["at"], at);
    ClipId(r["clip"].as_u64().unwrap())
}
fn env(at: i64) -> f64 {
    for pair in ENV.windows(2) {
        if at <= pair[1].0 {
            let (a, va) = pair[0];
            let (b, vb) = pair[1];
            return va + (vb - va) * (at - a) as f64 / (b - a) as f64;
        }
    }
    ENV.last().unwrap().1
}
fn fade(shape: FadeShape, t: f64) -> f64 {
    match shape {
        FadeShape::Linear => t,
        FadeShape::EqualPower => (t * std::f64::consts::FRAC_PI_2).sin(),
        FadeShape::SCurve => 0.5 - 0.5 * (t * std::f64::consts::PI).cos(),
        FadeShape::Exponential => t * t * t,
        FadeShape::Logarithmic => 1.0 - (1.0 - t).powi(3),
    }
}
fn expected(ch: usize, rel: i64, stretch: f64, shape: FadeShape) -> f64 {
    let x = 31.0 + rel as f64 / stretch;
    let q = x.floor() as usize;
    let fraction = (x - q as f64) as f32;
    // The input is a literal f32 PCM fixture. Independently interpolate those input values.
    let sample = signal(ch, q) + (signal(ch, q + 1) - signal(ch, q)) * fraction;
    let input_fade = if rel < 97 { fade(shape, rel as f64 / 97.0) } else { 1.0 };
    let from_end = 421 - rel;
    let output_fade = if from_end <= 173 { (from_end as f64 / 173.0).powi(3) } else { 1.0 };
    f64::from(sample) * 10f64.powf((-2.0 + env(rel)) / 20.0) * input_fade * output_fade
}
fn check_pcm(s: &Session, at: i64, origin: i64, length: i64, stretch: f64, shape: FadeShape, sizes: &[usize]) {
    let padding = at.min(5);
    let first = at - padding;
    let frames = usize::try_from(length + padding + 7).unwrap();
    let mut mix = MixEngine::new(48_000.0, 1024);
    mix.set_external_instances(true);
    mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
    let mut done = 0;
    let mut block_index = 0;
    let mut energy = [0.0f64; 2];
    let mut difference = 0.0;
    while done < frames {
        let n = sizes[block_index % sizes.len()].min(frames - done);
        let mut out = vec![vec![0.0; n]; 2];
        mix.render(s, first + done as i64, n, &mut out);
        assert_eq!(out.len(), 2);
        for (ch, channel) in out.iter().enumerate() {
            assert_eq!(channel.len(), n);
            for (i, &actual) in channel.iter().enumerate() {
                let j = first + (done + i) as i64 - at;
                let reference = if (0..length).contains(&j) { expected(ch, origin + j, stretch, shape) } else { 0.0 };
                assert!(actual.is_finite());
                let error = (f64::from(actual) - reference).abs();
                assert!(error <= 3e-6 + 3e-6 * reference.abs(), "ch{ch}, local{j}, blocks{sizes:?}: {actual} versus {reference}, error{error}");
                energy[ch] += reference * reference;
                if ch == 0 && (0..length).contains(&j) {
                    difference += (expected(0, origin + j, stretch, shape) - expected(1, origin + j, stretch, shape)).abs();
                }
            }
        }
        done += n;
        block_index += 1;
    }
    assert!(energy[0] > 0.01 && energy[1] > 0.01, "silence must never pass this oracle");
    assert!(difference > 1.0, "the fixture must distinguish channel order");
}

#[test]
fn audio_capture_preserves_original_and_exact_fades_phase_across_chunks_and_destinations() {
    for stretch in [1.0, 2.5, 0.8] {
        for shape in FadeShape::ALL {
            let (mut e, t, original_id, source) = fixture(stretch, shape);
            check_pcm(e.session(), 211, 0, 421, stretch, shape, &[1, 7, 31, 33, 127]);
            check_pcm(e.session(), 211, 0, 421, stretch, shape, &[512]);
            let before = e.session_arc();
            let original = e.session().find_clip(original_id).unwrap().1.clone();
            let definition = capture(&mut e, original_id, 248, 515);
            assert_eq!(e.session().tracks, before.tracks);
            assert_eq!(e.session().sources, before.sources);
            assert_eq!(e.session().midi_originals, before.midi_originals);
            assert!(Arc::ptr_eq(e.session().pool.get(source).unwrap(), before.pool.get(source).unwrap()));
            assert_eq!(e.session().find_clip(original_id).unwrap().1, &original);
            let retained = definitions(&mut e);
            assert_eq!(retained.as_array().unwrap().len(), 1);
            assert_eq!(retained[0]["timebase"], "Samples");
            assert_eq!(retained[0]["template"]["content"], serde_json::to_value(&original.content).unwrap());
            assert_eq!(retained[0]["template"]["length"], 267);
            assert_eq!(
                retained[0]["template"]["slice"],
                json!({"audio_origin_samples":37,"fade_in_elapsed_samples":37,"fade_out_remaining_samples":117})
            );
            assert!(e.undo());
            assert_eq!(e.session().tracks, before.tracks);
            assert!(definitions(&mut e).as_array().unwrap().is_empty());
            assert!(e.redo());
            assert_eq!(definitions(&mut e), retained);
            e.execute("clip.clear", &json!({"clips":[original_id]})).unwrap();
            assert_eq!(e.session().all_clips().count(), 0);
            let mut placed_ids = Vec::new();
            for at in [0, 1003, 4097] {
                let id = place(&mut e, definition, t, at);
                assert_ne!(id, original_id);
                assert_ne!(id.0, definition);
                assert!(!placed_ids.contains(&id));
                placed_ids.push(id);
                let c = e.session().find_clip(id).unwrap().1;
                assert_eq!(c.content, original.content);
                assert_eq!((c.start, c.length, c.stretch, c.gain_db), (at, 267, stretch, -2.0));
                assert_eq!(c.rating, 4);
                assert_eq!(c.color, Some([17, 83, 191]));
                assert_eq!(c.sync_point, 46);
                assert_eq!(c.fade_in, original.fade_in);
                assert_eq!(c.fade_out, original.fade_out);
                let context = c.slice.unwrap();
                assert_eq!((context.audio_origin_samples, context.fade_in_elapsed_samples, context.fade_out_remaining_samples), (37, 37, 117));
                assert_eq!(c.gain_env.len(), 4);
                for ((actual_at, actual_db), (expected_at, expected_db)) in
                    c.gain_env.iter().zip([(0, env(37)), (44, -7.0), (154, 2.0), (267, env(304))])
                {
                    assert_eq!(*actual_at, expected_at);
                    assert!((f64::from(*actual_db) - expected_db).abs() < 1e-6);
                }
                for size in [1, 7, 31, 32, 33, 64, 127, 512] {
                    check_pcm(e.session(), at, 37, 267, stretch, shape, &[size]);
                }
                check_pcm(e.session(), at, 37, 267, stretch, shape, &[1, 7, 31, 33, 127]);
                let placed = c.clone();
                assert!(e.undo());
                assert!(e.session().find_clip(id).is_none());
                assert_eq!(definitions(&mut e), retained);
                assert!(e.redo());
                assert_eq!(e.session().find_clip(id).unwrap().1, &placed);
                e.execute("clip.clear", &json!({"clips":[id]})).unwrap();
            }
            assert_eq!(definitions(&mut e), retained);
        }
    }
}

#[test]
fn nested_capture_and_trim_retain_cumulative_source_and_fade_coordinates() {
    let (mut e, t, old, _) = fixture(2.5, FadeShape::SCurve);
    let first = capture(&mut e, old, 248, 515);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let placed = place(&mut e, first, t, 1003);
    let second = capture(&mut e, placed, 1018, 1185);
    e.execute("clip.clear", &json!({"clips":[placed]})).unwrap();
    let new = place(&mut e, second, t, 4097);
    let clip = e.session().find_clip(new).unwrap().1;
    assert_eq!(clip.source_offset(), 31);
    assert_eq!(clip.slice.unwrap().audio_origin_samples, 52);
    check_pcm(e.session(), 4097, 52, 167, 2.5, FadeShape::SCurve, &[7, 31, 33]);
    // Model trims are independently checked against the unchanged analytic parent law.
    e.session_mut().find_clip_mut(new).unwrap().trim_start_to(4108);
    e.session_mut().find_clip_mut(new).unwrap().trim_end_to(4251);
    check_pcm(e.session(), 4108, 63, 143, 2.5, FadeShape::SCurve, &[1, 7, 33]);
}

fn note(pitch: u8, start: i64, length: i64) -> Note {
    Note { pitch, start, length, channel: pitch % 4, velocity: pitch + 25, release_velocity: pitch - 20 }
}
fn cc(tick: i64, kind: CtrlKind, value: i32) -> CtrlEvent {
    CtrlEvent { tick, channel: 2, kind, value }
}
fn midi_fixture() -> (Engine, TrackId, ClipId, Sequence, Sequence) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("MIDI original"));
    let id = s.new_clip_id();
    let current = Sequence {
        notes: vec![note(50, 0, 100), note(60, 80, 70), note(64, 125, 75), note(67, 350, 125), note(72, 20, 600), note(75, 400, 20)],
        ctrls: vec![
            cc(99, CtrlKind::Cc(7), 1),
            cc(100, CtrlKind::Program, 3),
            cc(180, CtrlKind::Cc(0), 4),
            cc(180, CtrlKind::Cc(32), 5),
            cc(180, CtrlKind::Program, 7),
            cc(180, CtrlKind::PitchBend, -1234),
            cc(399, CtrlKind::ChannelPressure, 91),
            cc(400, CtrlKind::PolyPressure(72), 20),
        ],
    };
    let original =
        Sequence { notes: vec![note(61, 60, 140), note(83, 300, 300)], ctrls: vec![cc(70, CtrlKind::Cc(7), 2), cc(320, CtrlKind::PitchBend, 2345)] };
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(id, "Original MIDI", 24_000, 20_000, current));
    s.midi_originals.insert(id.0, original);
    s.edit.selected_clips = vec![id];
    s.edit.selected_tracks = vec![t];
    s.edit.selection = soundcraft_time::Range::new(26_500, 34_000);
    let current_crop = Sequence {
        notes: vec![note(60, 0, 50), note(72, 0, 300), note(64, 25, 75), note(67, 250, 50)],
        ctrls: vec![
            cc(0, CtrlKind::Program, 3),
            cc(80, CtrlKind::Cc(0), 4),
            cc(80, CtrlKind::Cc(32), 5),
            cc(80, CtrlKind::Program, 7),
            cc(80, CtrlKind::PitchBend, -1234),
            cc(299, CtrlKind::ChannelPressure, 91),
        ],
    };
    let original_crop = Sequence { notes: vec![note(61, 0, 100), note(83, 200, 100)], ctrls: vec![cc(220, CtrlKind::PitchBend, 2345)] };
    (Engine::new(s), t, id, current_crop, original_crop)
}
#[test]
fn midi_intersects_both_note_edges_preserves_controller_order_and_separate_performance() {
    let (mut e, t, old, current, original) = midi_fixture();
    let before = e.session_arc();
    let definition = capture(&mut e, old, 26_500, 34_000);
    assert_eq!(e.session().tracks, before.tracks);
    assert_eq!(e.session().midi_originals, before.midi_originals);
    let retained = definitions(&mut e);
    assert_eq!(retained[0]["timebase"], json!({"MidiTicks":{"span":300}}));
    assert_eq!(retained[0]["template"]["content"], serde_json::to_value(ClipContent::Midi { sequence: current.clone() }).unwrap());
    assert_eq!(retained[0]["midi_original"], serde_json::to_value(&original).unwrap());
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    assert_eq!(e.session().all_clips().count(), 0);
    let first = place(&mut e, definition, t, 48_000);
    let second = place(&mut e, definition, t, 72_000);
    for id in [first, second] {
        let c = e.session().find_clip(id).unwrap().1;
        assert_eq!(c.length, 7500);
        assert_eq!(c.content, ClipContent::Midi { sequence: current.clone() });
        assert_eq!(e.session().midi_originals.get(&id.0), Some(&original));
    }
    e.execute("event.restore_performance", &json!({"clips":[first]})).unwrap();
    assert_eq!(e.session().find_clip(first).unwrap().1.content, ClipContent::Midi { sequence: original.clone() });
    assert_eq!(e.session().find_clip(second).unwrap().1.content, ClipContent::Midi { sequence: current });
    assert_eq!(definitions(&mut e), retained);
    e.execute("clip.clear_definitions", &json!({"definitions":[definition]})).unwrap();
    assert!(definitions(&mut e).as_array().unwrap().is_empty());
    assert!(e.session().find_clip(first).is_some() && e.session().find_clip(second).is_some());
    assert!(e.undo());
    assert_eq!(definitions(&mut e), retained);
    assert!(e.redo());
    assert!(definitions(&mut e).as_array().unwrap().is_empty());
}

#[test]
fn midi_tick_span_relocates_across_independently_integrated_tempo_segments() {
    let (mut e, t, old, _, _) = midi_fixture();
    e.session_mut().tempo.set_tempo(1200, 60.0).unwrap();
    let definition = capture(&mut e, old, 26_500, 39_000);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let early = place(&mut e, definition, t, 0);
    assert_eq!(e.session().find_clip(early).unwrap().1.length, 8000, "320 ticks at25samples/tick");
    let late = place(&mut e, definition, t, 40_000);
    assert_eq!(e.session().find_clip(late).unwrap().1.length, 16_000, "320 ticks at50samples/tick");
}

#[test]
fn video_capture_retains_linear_picture_coordinates_without_audio_source_aliasing() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Video, ChannelFormat::Mono, Some("Picture"));
    let source = SourceId(s.alloc());
    s.videos.push(VideoSource {
        id: source,
        name: "Independent picture".into(),
        path: "/fixture/picture.mov".into(),
        width: 320,
        height: 240,
        frame_rate: 30.0,
        duration: 1.0,
        codec: "mjpeg".into(),
    });
    let old = s.new_clip_id();
    let mut c = Clip::video(old, "Picture parent", source, 31, 211, 421);
    c.stretch = 2.5; // Existing picture semantics are linear, even with this stored value.
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c.clone());
    s.edit.selection = soundcraft_time::Range::new(248, 515);
    s.edit.selected_clips = vec![old];
    let mut e = Engine::new(s);
    let definition = capture(&mut e, old, 248, 515);
    assert_eq!(e.session().find_clip(old).unwrap().1, &c);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let id = place(&mut e, definition, t, 1003);
    let placed = e.session().find_clip(id).unwrap().1;
    assert!(placed.is_video());
    assert_eq!(placed.video_source(), Some(source));
    assert_eq!(placed.source(), None);
    assert_eq!(placed.length, 267);
    for j in [0, 1, 83, 266] {
        assert_eq!(placed.source_position(j), (31 + 37 + j) as f64);
    }
    assert!(e.session().sources.is_empty());
}

fn atomic_error(e: &mut Engine, command: &str, params: Value) {
    let before = e.session_arc();
    let text = e.session().to_json().unwrap();
    let state = (
        e.revision,
        e.is_dirty(),
        e.undo_history(),
        e.undo_label().map(str::to_owned),
        e.redo_label().map(str::to_owned),
        e.path.clone(),
        e.journal.clone(),
        e.clipboard.clone(),
    );
    assert!(e.execute(command, &params).is_err(), "{command}: {params}");
    assert!(Arc::ptr_eq(&before, &e.session_arc()), "error replaced document: {command} {params}");
    assert_eq!(e.session().to_json().unwrap(), text);
    assert_eq!(
        (
            e.revision,
            e.is_dirty(),
            e.undo_history(),
            e.undo_label().map(str::to_owned),
            e.redo_label().map(str::to_owned),
            e.path.clone(),
            e.journal.clone(),
            e.clipboard.clone()
        ),
        state
    );
}
#[test]
fn hostile_capture_place_and_clear_are_atomic_and_preserve_pending_redo() {
    let (mut e, t, old, source) = fixture(2.5, FadeShape::SCurve);
    let definition = capture(&mut e, old, 248, 515);
    e.execute("track.rename", &json!({"track":t,"name":"Pending rename"})).unwrap();
    assert!(e.undo());
    assert!(e.can_redo());
    for p in [
        json!({"clip":999_999,"start":248,"end":515}),
        json!({"clips":[old.0,999_999],"start":248,"end":515}),
        json!({"clips":"bad","start":248,"end":515}),
        json!({"clip":old,"start":248,"end":248}),
        json!({"clip":old,"start":515,"end":248}),
        json!({"clip":old,"start":900,"end":901}),
        json!({"clip":old,"start":i64::MAX-1,"end":i64::MAX}),
        json!({"clip":old,"start":"bad","end":515}),
    ] {
        atomic_error(&mut e, "clip.capture", p);
    }
    for p in [
        json!({"definition":999_999,"track":t,"at":0}),
        json!({"definition":definition,"track":999_999,"at":0}),
        json!({"definition":definition,"track":t,"at":i64::MAX-1}),
        json!({"definition":"bad","track":t,"at":0}),
        json!({"definition":definition,"track":t,"at":"bad"}),
    ] {
        atomic_error(&mut e, "clip.place_definition", p);
    }
    atomic_error(&mut e, "clip.clear_definitions", json!({"definitions":[definition,999_999]}));
    atomic_error(&mut e, "clip.clear", json!({"sources":[source],"with_clips":true}));
    assert!(e.redo());
    assert_eq!(e.session().track(t).unwrap().name, "Pending rename");
    for corruption in 0..5 {
        let (mut e, _, old, source) = fixture(2.5, FadeShape::SCurve);
        match corruption {
            0 => e.session_mut().next_id = u64::MAX,
            1 => e.session_mut().find_clip_mut(old).unwrap().stretch = f64::NAN,
            2 => e.session_mut().find_clip_mut(old).unwrap().gain_db = f32::INFINITY,
            3 => e.session_mut().find_clip_mut(old).unwrap().content = ClipContent::Audio { source: SourceId(999_999), offset: 31 },
            _ => e.session_mut().find_clip_mut(old).unwrap().content = ClipContent::Audio { source, offset: 1000 },
        }
        atomic_error(&mut e, "clip.capture", json!({"clip":old,"start":248,"end":515}));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    On(usize, u8, u8),
    Off(usize, u8),
}
static TRACE_INFO: PluginInfo = PluginInfo {
    id: "sc-ed46.trace",
    name: "Original ED46 trace instrument",
    short_name: "Trace",
    category: Category::Instrument,
    params: &[],
    is_instrument: true,
    audiosuite: false,
};
struct Trace(Arc<Mutex<Vec<Event>>>);
impl Plugin for Trace {
    fn info(&self) -> &'static PluginInfo {
        &TRACE_INFO
    }
    fn prepare(&mut self, _sr: f32, _max: usize, _ch: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _id: &str, _value: f32) -> bool {
        false
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        for ch in io {
            ch.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
    }
    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        self.0.lock().unwrap().push(Event::On(offset, note, velocity));
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        self.0.lock().unwrap().push(Event::Off(offset, note));
    }
}
#[test]
fn cropped_both_edge_note_releases_at_zero_of_the_next_block() {
    let (mut e, t, old, _, _) = midi_fixture();
    if let ClipContent::Midi { sequence } = &mut e.session_mut().find_clip_mut(old).unwrap().content {
        sequence.notes = vec![note(72, 20, 600)];
        sequence.ctrls.clear();
    }
    let definition = capture(&mut e, old, 26_500, 34_000);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let id = place(&mut e, definition, t, 0);
    assert_eq!(e.session().find_clip(id).unwrap().1.length, 7500);
    e.session_mut().track_mut(t).unwrap().instrument = Some(Insert::new("clap:sc-ed46.trace"));
    let events = Arc::new(Mutex::new(Vec::with_capacity(8)));
    let mut mix = MixEngine::new(48_000.0, 3750);
    mix.set_external_instances(true);
    mix.adopt(vec![PreparedInstance {
        track: t,
        slot: INSTRUMENT_SLOT,
        id: "clap:sc-ed46.trace".into(),
        ch: 2,
        plugin: Box::new(Trace(Arc::clone(&events))),
    }]);
    let mut out = vec![vec![0.0; 3750]; 2];
    mix.render(e.session(), 0, 3750, &mut out);
    assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97)]);
    mix.render(e.session(), 3750, 3750, &mut out);
    assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97)]);
    mix.render(e.session(), 7500, 1, &mut out);
    assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97), Event::Off(0, 72)]);
    mix.render(e.session(), 7501, 1, &mut out);
    assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97), Event::Off(0, 72)]);
}

#[test]
fn per_id_metadata_remaps_and_definition_clear_only_detaches_instance_provenance() {
    let (mut e, t, old, _) = fixture(2.5, FadeShape::SCurve);
    let unwarped = io::add_source(
        e.session_mut(),
        "Unwarped definition-only",
        AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 1024]] },
        None,
        FileFormat::Wav,
    );
    let values = [
        (format!("clip_fx.{}.gain", old.0), 3.5),
        (format!("clip.pitch.{}", old.0), -4.0),
        (format!("elastic.{}.ratio", old.0), 1.5),
        (format!("elastic.{}.src", old.0), unwarped.0 as f64),
        (format!("elastic.{}.off", old.0), 9.0),
        (format!("elastic.{}.len", old.0), 112.0),
        (format!("elastic.{}.phase", old.0), 0.2),
        (format!("elastic.{}.anchor", old.0), 31.0),
    ];
    for (key, value) in &values {
        e.session_mut().edit.values.insert(key.clone(), *value);
    }
    e.session_mut().edit.flags.insert(format!("clip_fx.bypass.{}", old.0));
    let parent = e.session_mut().find_clip_mut(old).unwrap();
    parent.edit_locked = true;
    parent.time_locked = true;
    parent.group = Some(777);
    let before = e.session_arc();
    let definition = capture(&mut e, old, 248, 515);
    assert_eq!(e.session().tracks, before.tracks);
    assert_eq!(e.session().edit, before.edit);
    let retained = definitions(&mut e);
    assert_eq!(retained[0]["metadata"]["values"], json!({"clip_fx.{id}.gain":3.5,"clip.pitch.{id}":-4.0,"elastic.{id}.ratio":1.5}));
    let elastic = &retained[0]["metadata"]["elastic_original"];
    assert_eq!(elastic["source"], unwarped.0);
    assert_eq!(elastic["offset"], 19);
    assert_eq!(elastic["length"], 71);
    assert!((elastic["phase"].as_f64().unwrap() - 1.0 / 15.0).abs() < 1e-10);
    assert!((elastic["rendered_origin_samples"].as_f64().unwrap() - 45.8).abs() < 1e-10);
    assert_eq!(retained[0]["metadata"]["flags"], json!(["clip_fx.bypass.{id}"]));
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    atomic_error(&mut e, "clip.clear", json!({"sources":[unwarped],"with_clips":true}));
    let first = place(&mut e, definition, t, 1003);
    let second = place(&mut e, definition, t, 4097);
    for id in [first, second] {
        let c = e.session().find_clip(id).unwrap().1;
        assert!(!c.edit_locked && !c.time_locked && c.group.is_none());
        assert_eq!(c.definition.map(|id| id.0), Some(definition));
        for (key, value) in &values[..3] {
            let new_key = key.replace(&format!(".{}", old.0), &format!(".{}", id.0));
            assert_eq!(e.session().edit.values.get(&new_key), Some(value));
        }
        for (suffix, expected) in [("src", unwarped.0 as f64), ("off", 19.0), ("len", 71.0), ("phase", 1.0 / 15.0), ("anchor", 45.8)] {
            let actual = e.session().edit.values[&format!("elastic.{}.{suffix}", id.0)];
            assert!((actual - expected).abs() < 1e-10);
        }
        assert!(e.session().edit.flags.contains(&format!("clip_fx.bypass.{}", id.0)));
    }
    let instances = e.session().tracks.clone();
    let metadata = e.session().edit.clone();
    e.execute("clip.clear_definitions", &json!({"definitions":[definition]})).unwrap();
    assert!(definitions(&mut e).as_array().unwrap().is_empty());
    for id in [first, second] {
        assert!(e.session().find_clip(id).unwrap().1.definition.is_none());
    }
    let mut detached = instances.clone();
    for clip in detached.iter_mut().flat_map(|t| &mut t.playlists).flat_map(|p| &mut p.clips) {
        clip.definition = None;
    }
    assert_eq!(e.session().tracks, detached);
    assert_eq!(e.session().edit, metadata);
    assert!(e.undo());
    assert_eq!(e.session().tracks, instances);
    assert_eq!(definitions(&mut e), retained);
    assert!(e.redo());
    assert_eq!(e.session().tracks, detached);
    assert!(e.session().source(unwarped).is_some());
}

#[test]
fn malformed_normalized_metadata_and_elastic_source_identity_reject_without_partial_capture() {
    for kind in 0..5 {
        let (mut e, t, old, source) = fixture(2.5, FadeShape::SCurve);
        match kind {
            0 => {
                e.session_mut().edit.values.insert(format!("elastic.{}.unknown", old.0), 1.0);
            }
            1 => {
                e.session_mut().edit.values.insert(format!("clip_fx.{}.", old.0), 1.0);
            }
            2 => {
                e.session_mut().edit.values.insert(format!("clip_fx.{}.gain", old.0), f64::NAN);
            }
            3 => {
                e.session_mut().edit.values.insert(format!("elastic.{}.src", old.0), source.0 as f64);
            }
            _ => {
                for (suffix, value) in [("src", 1.5), ("off", 0.0), ("len", 8.0)] {
                    e.session_mut().edit.values.insert(format!("elastic.{}.{suffix}", old.0), value);
                }
            }
        }
        e.execute("track.rename", &json!({"track":t,"name":"Pending metadata rename"})).unwrap();
        assert!(e.undo());
        atomic_error(&mut e, "clip.capture", json!({"clip":old,"start":248,"end":515}));
        assert!(e.can_redo());
    }
}

#[test]
fn placement_overwrite_preserves_both_trimmed_neighbor_windows_with_one_undo() {
    let (mut e, t, old, _) = fixture(2.5, FadeShape::SCurve);
    let parent = e.session().find_clip(old).unwrap().1.clone();
    let definition = capture(&mut e, old, 248, 515);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let neighbor = e.session_mut().new_clip_id();
    let mut destination = parent;
    destination.id = neighbor;
    destination.start = 0;
    destination.name = "Destination before overwrite".into();
    e.session_mut().track_mut(t).unwrap().playlist_mut().unwrap().clips.push(destination);
    e.session_mut().find_clip_mut(neighbor).unwrap().edit_locked = true;
    e.execute("track.rename", &json!({"track":t,"name":"Pending overwrite rename"})).unwrap();
    assert!(e.undo());
    atomic_error(&mut e, "clip.place_definition", json!({"definition":definition,"track":t,"at":100}));
    assert!(e.can_redo());
    e.session_mut().find_clip_mut(neighbor).unwrap().edit_locked = false;
    let before = e.session_arc();
    let placed = place(&mut e, definition, t, 100);
    let clips = e.session().track(t).unwrap().clips();
    assert_eq!(clips.iter().map(|c| (c.start, c.length)).collect::<Vec<_>>(), [(0, 100), (100, 267), (367, 54)]);
    assert_eq!(clips[0].id, neighbor);
    assert_eq!(clips[1].id, placed);
    assert_ne!(clips[2].id, neighbor);
    assert_ne!(clips[2].id, placed);
    assert_eq!(clips[0].slice.unwrap().audio_origin_samples, 0);
    assert_eq!(clips[2].slice.unwrap().audio_origin_samples, 367);
    let after = e.session_arc();
    let mut mix = MixEngine::new(48_000.0, 1024);
    mix.set_external_instances(true);
    mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
    let mut done = 0;
    while done < 421 {
        let n = 7.min(421 - done);
        let mut out = vec![vec![0.0; n]; 2];
        mix.render(e.session(), done as i64, n, &mut out);
        for (ch, channel) in out.iter().enumerate() {
            for (i, &a) in channel.iter().enumerate() {
                let at = (done + i) as i64;
                let rel = if (100..367).contains(&at) { 37 + at - 100 } else { at };
                let reference = expected(ch, rel, 2.5, FadeShape::SCurve);
                assert!((f64::from(a) - reference).abs() < 4e-6, "trimmed neighbor ch{ch} at{at}: {a} vs {reference}");
            }
        }
        done += n;
    }
    assert!(e.undo());
    assert_eq!(e.session().tracks, before.tracks);
    assert_eq!(e.session().edit, before.edit);
    assert_eq!(e.session().clip_definitions, before.clip_definitions);
    assert!(e.redo());
    assert_eq!(e.session().tracks, after.tracks);
    assert_eq!(e.session().edit, after.edit);
}

#[test]
fn partial_elastic_original_keeps_fractional_cursor_through_capture_trim_restore_and_recapture() {
    let (mut e, t, old, _) = fixture(2.5, FadeShape::SCurve);
    let source = io::add_source(
        e.session_mut(),
        "Independent unwarped stereo",
        AudioBuffer { sample_rate: 48_000, channels: (0..2).map(|ch| (0..1024).map(|i| signal(ch, i) * 0.5).collect()).collect() },
        None,
        FileFormat::Wav,
    );
    let parent = e.session_mut().find_clip_mut(old).unwrap();
    parent.gain_db = 0.0;
    parent.gain_env.clear();
    parent.fade_in.len = 0;
    parent.fade_out.len = 0;
    for (suffix, value) in [("src", source.0 as f64), ("off", 9.0), ("len", 112.0), ("ratio", 1.5), ("phase", 0.2), ("anchor", 31.0)] {
        e.session_mut().edit.values.insert(format!("elastic.{}.{suffix}", old.0), value);
    }
    let first = capture(&mut e, old, 248, 515);
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    atomic_error(&mut e, "clip.clear", json!({"sources":[source],"with_clips":true}));
    let placed = place(&mut e, first, t, 1003);
    let second = capture(&mut e, placed, 1018, 1185);
    e.execute("clip.clear", &json!({"clips":[placed]})).unwrap();
    let new = place(&mut e, second, t, 4097);
    let mapped = e.session().clip_definitions.iter().find(|d| d.id.0 == second).unwrap().metadata.elastic_original.as_ref().unwrap();
    assert_eq!(mapped.source, source);
    assert_eq!((mapped.offset, mapped.length), (23, 45));
    assert!((mapped.phase - 1.0 / 15.0).abs() < 1e-10);
    assert!((mapped.rendered_origin_samples - 51.8).abs() < 1e-10);
    e.session_mut().find_clip_mut(new).unwrap().trim_start_to(4110);
    e.session_mut().find_clip_mut(new).unwrap().trim_end_to(4251);
    let result = e.execute("clip.remove_warp", &json!({"clips":[new]})).unwrap();
    assert_eq!(result["restored"], 1);
    let restored = e.session().find_clip(new).unwrap().1;
    assert_eq!(restored.source(), Some(source));
    assert_eq!((restored.start, restored.length, restored.stretch), (4110, 38, 1.0));
    // Original law: 9.2 + ((31 + 65/2.5) -31)/1.5 = 26 +8/15.
    let expected_cursor = 26.0 + 8.0 / 15.0;
    assert!((restored.source_position(0) - expected_cursor).abs() < 1e-10);
    let out = soundcraft_mix::render_clips(e.session(), t, soundcraft_time::Range::new(4110, 4148));
    for (ch, channel) in out.iter().enumerate() {
        assert_eq!(channel.len(), 38);
        for (j, &actual) in channel.iter().enumerate() {
            let x = expected_cursor + j as f64;
            let q = x.floor() as usize;
            let f = (x - q as f64) as f32;
            let a = signal(ch, q) * 0.5;
            let b = signal(ch, q + 1) * 0.5;
            let reference = a + (b - a) * f;
            assert!(reference.abs() > 1e-4);
            assert!((actual - reference).abs() < 2e-6, "elastic restore ch{ch} frame{j}: {actual} vs {reference}");
        }
    }
    for suffix in ["src", "off", "len", "ratio", "phase", "anchor"] {
        assert!(!e.session().edit.values.contains_key(&format!("elastic.{}.{suffix}", new.0)));
    }
    let third = capture(&mut e, new, 4113, 4136);
    e.execute("clip.clear", &json!({"clips":[new]})).unwrap();
    let again = place(&mut e, third, t, 1003);
    let c = e.session().find_clip(again).unwrap().1;
    assert_eq!(c.length, 23);
    for j in [0, 1, 22] {
        assert!((c.source_position(j) - (expected_cursor + 3.0 + j as f64)).abs() < 1e-10);
    }
    atomic_error(&mut e, "clip.clear", json!({"sources":[source],"with_clips":true}));
}

#[test]
fn midi_source_rounding_phase_uses_clip_relative_cuts_and_preserves_both_edge_events() {
    let (mut e, t, old, _, _) = midi_fixture();
    e.session_mut().find_clip_mut(old).unwrap().start = 1013;
    // Base tick41 is sample1025. Local cuts2513/10013 map from that base to
    // absolute ticks142/442, hence relative [101,401), not [100,400).
    let definition = capture(&mut e, old, 3526, 11026);
    let current = Sequence {
        notes: vec![note(60, 0, 49), note(72, 0, 300), note(64, 24, 75), note(67, 249, 51), note(75, 299, 1)],
        ctrls: vec![
            cc(79, CtrlKind::Cc(0), 4),
            cc(79, CtrlKind::Cc(32), 5),
            cc(79, CtrlKind::Program, 7),
            cc(79, CtrlKind::PitchBend, -1234),
            cc(298, CtrlKind::ChannelPressure, 91),
            cc(299, CtrlKind::PolyPressure(72), 20),
        ],
    };
    let original = Sequence { notes: vec![note(61, 0, 99), note(83, 199, 101)], ctrls: vec![cc(219, CtrlKind::PitchBend, 2345)] };
    let retained = definitions(&mut e);
    assert_eq!(retained[0]["timebase"], json!({"MidiTicks":{"span":300}}));
    assert_eq!(retained[0]["template"]["content"], serde_json::to_value(ClipContent::Midi { sequence: current.clone() }).unwrap());
    assert_eq!(retained[0]["midi_original"], serde_json::to_value(&original).unwrap());
    e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
    let id = place(&mut e, definition, t, 1003);
    let placed = e.session().find_clip(id).unwrap().1;
    assert_eq!((placed.start, placed.length), (1003, 7500));
    assert_eq!(placed.content, ClipContent::Midi { sequence: current });
    assert_eq!(e.session().midi_originals.get(&id.0), Some(&original));
}

#[test]
fn non_tick_midi_destinations_deliver_onsets_and_exact_durations_across_tempo_changes() {
    for (destination, duration, change_tempo) in [(1003, 7500, false), (27488, 12500, true)] {
        let (mut e, t, old, _, _) = midi_fixture();
        e.session_mut().find_clip_mut(old).unwrap().start = 1013;
        if change_tempo {
            e.session_mut().tempo.set_tempo(1200, 60.0).unwrap();
        }
        if let ClipContent::Midi { sequence } = &mut e.session_mut().find_clip_mut(old).unwrap().content {
            sequence.notes = vec![note(72, 20, 600)];
            sequence.ctrls.clear();
        }
        let definition = capture(&mut e, old, 3526, 11026);
        e.execute("clip.clear", &json!({"clips":[old]})).unwrap();
        let id = place(&mut e, definition, t, destination);
        let placed = e.session().find_clip(id).unwrap().1;
        assert_eq!((placed.start, placed.length), (destination, duration));
        // Literal musical integration: at1003, 300ticks*25=7500samples.
        // At27488, rounded base1100 lies100ticks before1200:100*25+200*50=12500.
        e.session_mut().track_mut(t).unwrap().instrument = Some(Insert::new("clap:sc-ed46.trace"));
        let events = Arc::new(Mutex::new(Vec::with_capacity(8)));
        let half = usize::try_from(duration / 2).unwrap();
        let mut mix = MixEngine::new(48_000.0, half);
        mix.set_external_instances(true);
        mix.adopt(vec![PreparedInstance {
            track: t,
            slot: INSTRUMENT_SLOT,
            id: "clap:sc-ed46.trace".into(),
            ch: 2,
            plugin: Box::new(Trace(Arc::clone(&events))),
        }]);
        let mut out = vec![vec![0.0; half]; 2];
        mix.render(e.session(), destination, half, &mut out);
        assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97)], "non-tick destination must not lose its first onset");
        mix.render(e.session(), destination + duration / 2, half, &mut out);
        assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97)]);
        mix.render(e.session(), destination + duration, 1, &mut out);
        assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97), Event::Off(0, 72)]);
        mix.render(e.session(), destination + duration + 1, 1, &mut out);
        assert_eq!(*events.lock().unwrap(), vec![Event::On(0, 72, 97), Event::Off(0, 72)]);
    }
}
