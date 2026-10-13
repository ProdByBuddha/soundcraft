//! Independent retained-window PCM and prepared-path allocation/performance evidence.
//! Timing measurements are diagnostics, not physical-device or deadline guarantees.

use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::pan::PanLaw;
use soundcraft_mix::{MixEngine, SessionPreparer};
use soundcraft_model::{ChannelFormat, Clip, ClipSliceContext, Fade, FadeShape, Session, SourceAudio, SourceId, TrackKind};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;
use test_allocation_counter::{CountingSystem, measure};

#[global_allocator]
static ALLOCATOR: CountingSystem = CountingSystem;

const PARENT_START: i64 = 211;
const PARENT_LENGTH: i64 = 421;
const RAW_OFFSET: i64 = 31;
const CROP_START: i64 = 37;
const CROP_END: i64 = 304;
const DESTINATION: i64 = 1003;
const LENGTH: i64 = CROP_END - CROP_START;
const STRETCH: f64 = 2.5;
const SOURCE_FRAMES: usize = 1024;
const PCM_TOLERANCE: f64 = 3e-6;

fn literal_input(channel: usize, frame: usize) -> f32 {
    let i = frame as f64;
    match channel {
        0 => (0.05 + 0.001 * i + 0.03 * (0.047 * i + 0.0003 * i * i).sin()) as f32,
        1 => (-0.12 + 0.0007 * i + 0.08 * (0.023 * i + 0.0004 * i * i).cos()) as f32,
        _ => panic!("literal stereo fixture has exactly two channels"),
    }
}

fn literal_envelope(relative: f64) -> f64 {
    const POINTS: [(f64, f64); 5] = [(0.0, -3.0), (19.0, 5.0), (81.0, -7.0), (191.0, 2.0), (421.0, -5.0)];
    if relative <= POINTS[0].0 {
        return POINTS[0].1;
    }
    for pair in POINTS.windows(2) {
        let [(a, before), (b, after)] = [pair[0], pair[1]];
        if relative <= b {
            return before + (after - before) * (relative - a) / (b - a);
        }
    }
    POINTS[4].1
}

fn literal_frame(channel: usize, timeline: i64, retained: bool, curved: bool) -> f64 {
    let (start, length) = if retained { (DESTINATION, LENGTH) } else { (PARENT_START, PARENT_LENGTH) };
    if !(start..start + length).contains(&timeline) {
        return 0.0;
    }
    let parent_relative = (timeline - start + if retained { CROP_START } else { 0 }) as f64;
    let coordinate = RAW_OFFSET as f64 + parent_relative / STRETCH;
    let before = coordinate.floor() as usize;
    let fraction = coordinate - before as f64;
    assert!(before + 1 < SOURCE_FRAMES);
    let a = f64::from(literal_input(channel, before));
    let b = f64::from(literal_input(channel, before + 1));
    let source = a + (b - a) * fraction;
    let (envelope, incoming, outgoing) = if curved {
        let incoming = if parent_relative < 97.0 { 0.5 - 0.5 * (std::f64::consts::PI * parent_relative / 97.0).cos() } else { 1.0 };
        let remaining = PARENT_LENGTH as f64 - parent_relative;
        let outgoing = if remaining <= 173.0 { (remaining / 173.0).powi(3) } else { 1.0 };
        (literal_envelope(parent_relative), incoming, outgoing)
    } else {
        (0.0, 1.0, 1.0)
    };
    source * 10.0_f64.powf((-2.0 + envelope) / 20.0) * incoming * outgoing
}

fn session(retained: bool, curved: bool) -> Arc<Session> {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Independent retained window"));
    let source = SourceId(701);
    let channels = (0..2).map(|channel| (0..SOURCE_FRAMES).map(|frame| literal_input(channel, frame)).collect()).collect();
    session.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let id = session.new_clip_id();
    let mut clip = Clip::audio(
        id,
        "Original literal source",
        source,
        RAW_OFFSET,
        if retained { DESTINATION } else { PARENT_START },
        if retained { LENGTH } else { PARENT_LENGTH },
    );
    clip.gain_db = -2.0;
    clip.stretch = STRETCH;
    if retained {
        clip.slice = Some(ClipSliceContext {
            audio_origin_samples: CROP_START,
            audio_phase_samples: 0.0,
            fade_in_elapsed_samples: CROP_START,
            fade_out_remaining_samples: PARENT_LENGTH - CROP_END,
        });
    }
    if curved {
        clip.gain_env = if retained {
            vec![(0, (5.0 - 12.0 * 18.0 / 62.0) as f32), (44, -7.0), (154, 2.0), (LENGTH, (2.0 - 7.0 * 113.0 / 230.0) as f32)]
        } else {
            vec![(0, -3.0), (19, 5.0), (81, -7.0), (191, 2.0), (PARENT_LENGTH, -5.0)]
        };
        clip.fade_in = Fade { len: 97, shape: FadeShape::SCurve };
        clip.fade_out = Fade { len: 173, shape: FadeShape::Exponential };
    }
    let target = session.track_mut(track).unwrap();
    target.mixer.pan = vec![-1.0, 1.0];
    target.playlist_mut().unwrap().clips.push(clip);
    Arc::new(session)
}

fn counter_positive_control() {
    let (value, allocations) = measure(|| black_box(Box::new(black_box(48_u64))));
    assert!(allocations > 0, "counter must observe a deliberate allocation");
    assert_eq!(*value, 48);
    drop(value);
}

fn prepared(retained: bool, curved: bool, block: usize) -> (MixEngine, Arc<Session>) {
    let source = session(retained, curved);
    let mut preparer = SessionPreparer::default();
    let update = preparer.prepare(Arc::clone(&source), 1, block).unwrap();
    let mut mixer = MixEngine::new(48_000.0, block);
    let (retired, allocations) = measure(|| mixer.install_prepared(update));
    assert_eq!(allocations, 0, "actual prepared install must allocate nothing on the measured thread");
    // Old prepared workspaces and their Arc owners are disposed on this control path.
    drop(retired);
    mixer.pan_law = PanLaw::Zero;
    (mixer, source)
}

fn check_pcm(output: &[Vec<f32>], position: i64, frames: usize, retained: bool, curved: bool) -> (f64, f64) {
    assert_eq!(output.len(), 2);
    let mut energy = 0.0;
    let mut difference = 0.0;
    for (channel, samples) in output.iter().enumerate() {
        assert_eq!(samples.len(), frames);
        for (frame, actual) in samples.iter().copied().enumerate() {
            let expected = literal_frame(channel, position + frame as i64, retained, curved);
            assert!(actual.is_finite());
            assert!(
                (f64::from(actual) - expected).abs() <= PCM_TOLERANCE * (1.0 + expected.abs()),
                "channel {channel}, timeline {}, actual {actual}, independent expected {expected}",
                position + frame as i64
            );
            if expected == 0.0 {
                assert_eq!(actual, 0.0, "outside half-open visible window must be silent");
            }
            energy += f64::from(actual).powi(2);
        }
    }
    for (left, right) in output[0].iter().zip(&output[1]) {
        difference += (f64::from(*left) - f64::from(*right)).abs();
    }
    (energy, difference)
}

#[test]
fn original_and_retained_curves_match_literal_pcm_with_zero_prepared_install_and_chunk_allocations() {
    counter_positive_control();
    for retained in [false, true] {
        let (start, length) = if retained { (DESTINATION, LENGTH) } else { (PARENT_START, PARENT_LENGTH) };
        for chunk in [1, 7, 31, 32, 33, 64, 127, 512] {
            let (mut mixer, source) = prepared(retained, true, 512);
            let original_clip = source.tracks[0].clips().first().unwrap().clone();
            let original_audio = Arc::clone(source.pool.get(SourceId(701)).unwrap());
            let mut output = vec![vec![0.5; 512]; 2];
            let mut position = start - 23;
            let end = start + length + 19;
            let mut energy = 0.0;
            let mut difference = 0.0;
            while position < end {
                let frames = chunk.min((end - position) as usize);
                for channel in &mut output {
                    channel.resize(frames, 0.5);
                    channel.fill(0.5);
                }
                let (result, allocations) = measure(|| mixer.render_prepared(position, frames, &mut output));
                result.unwrap();
                assert_eq!(allocations, 0, "chunk {chunk}, retained {retained}, position {position}");
                let (block_energy, block_difference) = check_pcm(&output, position, frames, retained, true);
                energy += block_energy;
                difference += block_difference;
                position += frames as i64;
            }
            assert!(energy > 0.02, "the whole verified signal must be audible");
            assert!(difference > 0.02, "the independent stereo channels must differ");
            for position in [start + 101, start + 3, start + 201, start - 19, start + length - 11, start + 37] {
                for channel in &mut output {
                    channel.resize(64, 0.5);
                    channel.fill(0.5);
                }
                let (result, allocations) = measure(|| mixer.render_prepared(position, 64, &mut output));
                result.unwrap();
                assert_eq!(allocations, 0, "seek must not allocate");
                check_pcm(&output, position, 64, retained, true);
            }
            assert_eq!(source.tracks[0].clips().first().unwrap(), &original_clip);
            assert!(Arc::ptr_eq(source.pool.get(SourceId(701)).unwrap(), &original_audio));
        }
    }
}

#[test]
fn constant_and_curved_retained_paths_report_actual_bounded_timings_and_zero_thread_allocations() {
    counter_positive_control();
    for curved in [false, true] {
        let (mut mixer, source) = prepared(true, curved, 64);
        let original_clip = source.tracks[0].clips().first().unwrap().clone();
        let mut output = vec![vec![0.5; 64]; 2];
        let positions = [DESTINATION, DESTINATION + 64, DESTINATION + 128, DESTINATION + 192, DESTINATION + 256, DESTINATION - 33];
        let mut energy = 0.0;
        for position in positions {
            mixer.render_prepared(position, 64, &mut output).unwrap();
            energy += check_pcm(&output, position, 64, true, curved).0;
        }
        assert!(energy > 0.02);
        let mut observed_allocations = 0_usize;
        let mut checksum = 0.0_f64;
        let started = Instant::now();
        for index in 0..2048 {
            let position = positions[index % positions.len()];
            let (result, allocations) = measure(|| mixer.render_prepared(position, 64, &mut output));
            result.unwrap();
            observed_allocations += allocations;
            let sample = index % 64;
            checksum += f64::from(black_box(output[0][sample])).powi(2) + 0.5 * f64::from(black_box(output[1][sample])).powi(2);
        }
        let elapsed = started.elapsed();
        assert_eq!(observed_allocations, 0);
        assert!(checksum.is_finite() && checksum > 0.02);
        assert_eq!(source.tracks[0].clips().first().unwrap(), &original_clip);
        println!(
            "SC-ED46 prepared_gain_probe mode={} blocks=2048 frames_per_block=64 channels=2 elapsed_render_counter_checksum_ns={} checksum={checksum:.12} observed_thread_allocations={observed_allocations} arch={} debug_assertions={}",
            if curved { "curved" } else { "constant" },
            elapsed.as_nanos(),
            std::env::consts::ARCH,
            cfg!(debug_assertions)
        );
    }
}

#[test]
fn finite_extrema_keep_exact_integer_and_quarter_phase_sample_bits_in_original_and_retained_windows() {
    const INPUT: [[f32; 8]; 2] = [
        [f32::MAX, -f32::MAX, f32::MAX, -f32::MAX, f32::MAX, -f32::MAX, f32::MAX, -f32::MAX],
        [-f32::MAX, f32::MAX, -f32::MAX, f32::MAX, -f32::MAX, f32::MAX, -f32::MAX, f32::MAX],
    ];
    let audio = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: INPUT.iter().map(|samples| samples.to_vec()).collect() }));
    let cases = [
        ("original integer whole", 211_i64, 8_usize, 0_usize, 0.0_f64, false),
        ("retained integer origin", 1003, 7, 1, 0.0, true),
        ("retained quarter phase", 1003, 7, 0, 0.25, true),
    ];
    for (name, start, frames, origin, phase, retained) in cases {
        let mut session = Session::default();
        let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
        let source = SourceId(701);
        session.pool.insert(source, Arc::clone(&audio));
        let mut clip = Clip::audio(session.new_clip_id(), name, source, 0, start, frames as i64);
        if retained {
            clip.slice = Some(ClipSliceContext {
                audio_origin_samples: origin as i64,
                audio_phase_samples: phase,
                fade_in_elapsed_samples: 0,
                fade_out_remaining_samples: 0,
            });
        }
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(clip);
        let output = soundcraft_mix::render_clips(&session, track, soundcraft_time::Range::new(start, start + frames as i64));
        assert_eq!(output.len(), 2, "{name}");
        for (channel, input) in INPUT.iter().enumerate() {
            assert_eq!(output[channel].len(), frames, "{name}");
            for (frame, actual) in output[channel].iter().copied().enumerate() {
                let coordinate = origin as f64 + phase + frame as f64;
                let index = coordinate.floor() as usize;
                let fraction = coordinate - index as f64;
                let a = f64::from(input[index]);
                let b = f64::from(input.get(index + 1).copied().unwrap_or(0.0));
                // Independently weighted convex interpolation stays finite for opposite
                // Float32 extrema. Integer samples, including the final sample, stay exact.
                let expected = ((1.0 - fraction) * a + fraction * b) as f32;
                assert!(actual.is_finite(), "{name}, channel {channel}, frame {frame}: {actual}");
                assert!(expected.is_finite() && expected.abs() > 1.0, "fixture must remain nonzero, finite, and above unity");
                assert_eq!(actual.to_bits(), expected.to_bits(), "{name}, channel {channel}, frame {frame}");
            }
        }
        assert!(Arc::ptr_eq(session.pool.get(source).unwrap(), &audio));
        for (actual, input) in audio.buffer.channels.iter().zip(INPUT) {
            assert_eq!(actual.len(), 8);
            assert_eq!(actual.iter().map(|sample| sample.to_bits()).collect::<Vec<_>>(), input.map(f32::to_bits));
        }
    }
}
