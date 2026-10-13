//! Original controlled processors exercise generic insert readers through the public adoption API.
//! This compatibility path is not a prepared-callback or vendor qualification.
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::{Category, ParamInfo, Plugin, PluginInfo, Taper, Unit};
use soundcraft_mix::{INSTRUMENT_SLOT, MixEngine, PreparedInstance};
use soundcraft_model::{AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackId, TrackKind};
use std::sync::Arc;

const FRAMES: usize = 389;
const GAIN: &[(i64, f64)] = &[(0, 0.5), (37, 1.5), (94, 0.25), (211, 2.0), (367, 0.75)];
const CHOICE: &[(i64, f64)] = &[(0, 0.0), (10, 2.0), (94, 1.0), (211, 0.0), (367, 2.0)];
const TOGGLE: &[(i64, f64)] = &[(0, 0.0), (14, 1.0), (127, 0.0), (211, 1.0), (367, 0.0)];
const PAN1: &[(i64, f64)] = &[(0, 1.0), (37, -1.0), (94, 0.25), (211, -0.5), (367, 1.0)];
static PARAMS: [ParamInfo; 3] = [
    ParamInfo { id: "Gain", name: "Original scalar", min: 0.0, max: 2.0, default: 0.75, unit: Unit::None, taper: Taper::Linear, choices: &[] },
    ParamInfo {
        id: "Mode",
        name: "Original choice",
        min: 0.0,
        max: 2.0,
        default: 0.0,
        unit: Unit::Choice,
        taper: Taper::Linear,
        choices: &["Original A", "Original B", "Original C"],
    },
    ParamInfo { id: "Invert", name: "Original switch", min: 0.0, max: 1.0, default: 0.0, unit: Unit::Toggle, taper: Taper::Linear, choices: &[] },
];
static INSERT_INFO: PluginInfo = PluginInfo {
    id: "original.mock",
    name: "Original controlled insert",
    short_name: "Mock",
    category: Category::Other,
    params: &PARAMS,
    is_instrument: false,
    audiosuite: false,
};
static INSTRUMENT_INFO: PluginInfo = PluginInfo {
    id: "original.instrument",
    name: "Original controlled instrument",
    short_name: "MockInst",
    category: Category::Instrument,
    params: &PARAMS,
    is_instrument: true,
    audiosuite: false,
};
struct Original {
    values: [f32; 3],
    instrument: bool,
}
impl Plugin for Original {
    fn info(&self) -> &'static PluginInfo {
        if self.instrument { &INSTRUMENT_INFO } else { &INSERT_INFO }
    }
    fn prepare(&mut self, _sr: f32, _max: usize, _channels: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(index) = PARAMS.iter().position(|p| p.id == id) else { return false };
        let value = value.clamp(PARAMS[index].min, PARAMS[index].max);
        self.values[index] = if index == 0 { value } else { value.round() };
        true
    }
    fn param(&self, id: &str) -> Option<f32> {
        PARAMS.iter().position(|p| p.id == id).map(|i| self.values[i])
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let gain = self.values[0] * [1.0f32, 0.5, 2.0][self.values[1] as usize] * if self.values[2] >= 0.5 { -1.0 } else { 1.0 };
        for (ch, channel) in io.iter_mut().enumerate() {
            for sample in channel.iter_mut().take(frames) {
                if self.instrument {
                    *sample = if ch == 0 { 0.125 } else { -0.25 };
                }
                *sample *= gain;
            }
        }
    }
}
fn linear(points: &[(i64, f64)], at: i64) -> f64 {
    if at <= points[0].0 {
        return points[0].1;
    }
    for pair in points.windows(2) {
        if at < pair[1].0 {
            let ratio = (at - pair[0].0) as f64 / (pair[1].0 - pair[0].0) as f64;
            return pair[0].1 + (pair[1].1 - pair[0].1) * ratio;
        }
    }
    points.last().unwrap().1
}
fn held(points: &[(i64, f64)], at: i64) -> f64 {
    points.iter().rev().find(|p| p.0 <= at).map_or(points[0].1, |p| p.1)
}
fn signal(ch: usize, at: usize) -> f64 {
    if (at / 17).is_multiple_of(2) { [0.25, -0.125][ch] } else { [-0.125, 0.375][ch] }
}
fn fixture(kind: TrackKind, format: ChannelFormat) -> (Session, TrackId) {
    let mut s = Session::default();
    let t = s.add_track(kind, format, Some("Original generic reader"));
    if kind == TrackKind::Audio {
        let source = SourceId(957);
        let channels = (0..2).map(|ch| (0..FRAMES).map(|at| signal(ch, at) as f32).collect()).collect();
        s.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
        let clip = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", source, 0, 0, FRAMES as i64));
    }
    s.track_mut(t).unwrap().mixer.pan = vec![-1.0, 1.0];
    (s, t)
}
fn lane(s: &mut Session, t: TrackId, param: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(param);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    s.track_mut(t).unwrap().automation.push(lane);
}
fn engine(t: TrackId, slots: &[(usize, bool)]) -> MixEngine {
    let mut mix = MixEngine::new(48_000.0, 1024);
    mix.set_external_instances(true);
    mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
    mix.adopt(
        slots
            .iter()
            .map(|&(slot, instrument)| PreparedInstance {
                track: t,
                slot,
                id: if instrument { "clap:original.instrument".into() } else { "clap:original.mock".into() },
                ch: 2,
                plugin: Box::new(Original { values: [0.75, 0.0, 0.0], instrument }),
            })
            .collect(),
    );
    mix
}
fn render(mix: &mut MixEngine, s: &Session, start: usize, frames: usize, sizes: &[usize]) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); 2];
    let mut done = 0;
    let mut index = 0;
    while done < frames {
        let n = sizes[index % sizes.len()].min(frames - done);
        let mut block = vec![vec![0.0; n]; 2];
        mix.render(s, (start + done) as i64, n, &mut block);
        for (dst, src) in out.iter_mut().zip(block) {
            assert_eq!(src.len(), n);
            dst.extend(src);
        }
        done += n;
        index += 1;
    }
    out
}
fn check(actual: &[Vec<f32>], expected: &[Vec<f64>], frames: usize) {
    assert_eq!(actual.len(), 2);
    assert_eq!(expected.len(), 2);
    let mut audible = false;
    for (a, e) in actual.iter().zip(expected) {
        assert_eq!(a.len(), frames);
        assert_eq!(e.len(), frames);
        for (&a, &e) in a.iter().zip(e) {
            assert!(a.is_finite() && e.is_finite());
            assert!((f64::from(a) - e).abs() < 1e-6, "{a} versus {e}");
            audible |= e.abs() > 0.02;
        }
    }
    assert!(audible);
}

#[test]
fn generic_choice_toggle_stream_is_plain_linear_then_setter_quantized() {
    let (mut s, t) = fixture(TrackKind::Audio, ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[9] = Some(Insert::new("clap:original.mock"));
    for (id, points) in [("Gain", GAIN), ("Mode", CHOICE), ("Invert", TOGGLE)] {
        lane(&mut s, t, AutoParam::Plugin { slot: 9, param: id.into() }, points);
    }
    let expected = (0..2)
        .map(|ch| {
            (0..FRAMES)
                .map(|at| {
                    signal(ch, at)
                        * linear(GAIN, at as i64)
                        * [1.0, 0.5, 2.0][linear(CHOICE, at as i64).round() as usize]
                        * if linear(TOGGLE, at as i64).round() >= 1.0 { -1.0 } else { 1.0 }
                })
                .collect()
        })
        .collect::<Vec<Vec<f64>>>();
    let wrong = (0..2)
        .map(|ch| {
            (0..FRAMES)
                .map(|at| {
                    signal(ch, at)
                        * linear(GAIN, at as i64)
                        * [1.0, 0.5, 2.0][held(CHOICE, at as i64).round() as usize]
                        * if held(TOGGLE, at as i64) >= 0.5 { -1.0 } else { 1.0 }
                })
                .collect()
        })
        .collect::<Vec<Vec<f64>>>();
    assert!(expected.iter().flatten().zip(wrong.iter().flatten()).any(|(a, b)| (a - b).abs() > 0.05));
    for schedule in [&[1usize][..], &[7][..], &[37][..], &[64][..], &[100][..], &[512][..], &[1024][..], &[1, 7, 37, 64, 100][..]] {
        check(&render(&mut engine(t, &[(9, false)]), &s, 0, FRAMES, schedule), &expected, FRAMES);
    }
}

#[test]
fn exact_case_seed_and_imported_future_targets_remain_inert_and_round_trip() {
    let (mut s, t) = fixture(TrackKind::Audio, ChannelFormat::Stereo);
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:original.mock"));
    for param in [
        AutoParam::Plugin { slot: 0, param: "gain".into() },
        AutoParam::Plugin { slot: 10, param: "Gain".into() },
        AutoParam::SendLevel(10),
        AutoParam::SendMute(254),
        AutoParam::Pan(2),
    ] {
        lane(&mut s, t, param, &[(0, 1.0), (211, 0.0)]);
    }
    let text = s.to_json().unwrap();
    let mut imported = Session::from_json(&text).unwrap();
    assert_eq!(imported.track(t).unwrap().automation, s.track(t).unwrap().automation);
    // Model JSON excludes decoded audio. Reattach the exact original immutable synthetic pool;
    // native media persistence/reopen is exercised independently by the Engine tests.
    imported.pool = s.pool.clone();
    assert!(Arc::ptr_eq(imported.pool.get(SourceId(957)).unwrap(), s.pool.get(SourceId(957)).unwrap()));
    assert_eq!(imported.pool.get(SourceId(957)).unwrap().buffer, s.pool.get(SourceId(957)).unwrap().buffer);
    let expected = (0..2).map(|ch| (43..307).map(|at| signal(ch, at) * 0.75).collect()).collect::<Vec<Vec<f64>>>();
    check(&render(&mut engine(t, &[(0, false)]), &imported, 43, 264, &[1, 7, 37, 64, 100]), &expected, 264);
}

#[test]
fn nominal_mono_instrument_uses_actual_stereo_pan_and_separate_insert_identity() {
    let (mut s, t) = fixture(TrackKind::Instrument, ChannelFormat::Mono);
    let track = s.track_mut(t).unwrap();
    track.instrument = Some(Insert::new("clap:original.instrument"));
    track.mixer.inserts[0] = Some(Insert::new("clap:original.mock"));
    lane(&mut s, t, AutoParam::Plugin { slot: 0, param: "Gain".into() }, GAIN);
    lane(&mut s, t, AutoParam::Pan(1), PAN1);
    let expected = (0..2)
        .map(|ch| {
            (0..FRAMES)
                .map(|at| {
                    let pan = linear(PAN1, at as i64).clamp(-1.0, 1.0);
                    let right_gain = if ch == 0 { (1.0 - pan).min(1.0) } else { (1.0 + pan).min(1.0) };
                    (if ch == 0 { 0.125 * 0.75 } else { 0.0 } + (-0.25 * 0.75) * right_gain) * linear(GAIN, at as i64)
                })
                .collect()
        })
        .collect::<Vec<Vec<f64>>>();
    check(&render(&mut engine(t, &[(INSTRUMENT_SLOT, true), (0, false)]), &s, 0, FRAMES, &[1, 7, 37, 64, 100]), &expected, FRAMES);
}
