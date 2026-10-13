//! New window-contract admission without large audio allocations, plus tiny literal multichannel
//! Float32 wire exports. No production renderer/encoder constructs the expected PCM or masks.
#![cfg(not(target_arch = "wasm32"))]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use soundcraft_engine::{Engine, io};
use soundcraft_model::{ChannelFormat, Clip, Insert, Route, Session, TrackId, TrackKind};
use soundcraft_time::{Range, SampleRate};
use std::path::PathBuf;
use std::sync::Arc;

const SAMPLE_VALUES: usize = 536_870_912;
const CONVERTED_VALUES: usize = 268_435_456;
const DELIVERY_BYTES: u64 = 2_148_532_224;
const HEADER_ALLOWANCE: u64 = 1_048_576;

fn empty(format: ChannelFormat) -> Session {
    let mut session = Session::new("Window admission, no PCM", SampleRate::HZ_48000);
    session.outputs[0].format = format;
    session
}
fn request(range: Range, tail_samples: i64, preroll_start: i64, rate: u32) -> io::BounceRequest {
    io::BounceRequest {
        range,
        tail_samples,
        preroll_start,
        sample_rate: SampleRate::new(rate).unwrap(),
        bus: None,
        fold_stereo: false,
        normalize: false,
        encode: EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None },
    }
}
fn widths() -> [(ChannelFormat, usize); 3] {
    [(ChannelFormat::Stereo, 2), (ChannelFormat::Surround51, 6), (ChannelFormat::Atmos916, 16)]
}

#[test]
fn source_tail_preroll_working_width_and_delivered_width_admit_exact_boundary_only() {
    assert_eq!(soundcraft_mix::MAX_RENDER_SAMPLES, SAMPLE_VALUES);
    assert_eq!(soundcraft_engine::bounce_delivery::MAX_DELIVERY_BYTES, DELIVERY_BYTES);
    for (format, channels) in widths() {
        for named_bus in [false, true] {
            let mut session = empty(if named_bus { ChannelFormat::Stereo } else { format });
            let bus = named_bus.then(|| session.add_bus("Independent admitted width", format));
            assert!(session.sources.is_empty(), "planning must not require PCM or processors");
            let boundary = SAMPLE_VALUES / channels;
            // Thirty-seven preroll frames and nineteen tail frames count towards WORK,
            // while only source+tail is delivered. No large AudioBuffer is constructed.
            let source_frames = boundary - 37 - 19;
            for depth in [BitDepth::Int16, BitDepth::Int24, BitDepth::Float32] {
                let mut req = request(Range::new(137, 137 + source_frames as i64), 19, 100, 48_000);
                req.bus = bus;
                req.encode.bit_depth = depth;
                let plan = io::plan_bounce(&session, &req).unwrap();
                assert_eq!(plan.processing_frames, boundary);
                assert_eq!(plan.session_frames, boundary - 37);
                assert_eq!(plan.frames, boundary - 37);
                assert_eq!(plan.channels, channels);
                assert_eq!(plan.input_range, req.range);
                assert_eq!(plan.tail_samples, 19);
                assert_eq!(plan.preroll_start, 100);
                assert!(boundary * channels <= SAMPLE_VALUES);
                assert!((boundary + 1) * channels > SAMPLE_VALUES);
                // Three independent ways to add precisely one processing frame, while the
                // delivered buffer still fits, exercise the NEW aggregate window bound.
                let mut plus = req.clone();
                plus.range.end += 1;
                assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("processing bounds"));
                plus = req.clone();
                plus.tail_samples += 1;
                assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("processing bounds"));
                plus = req.clone();
                plus.preroll_start -= 1;
                assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("processing bounds"));
            }
            let mut all_delivery = request(Range::new(0, boundary as i64), 0, 0, 48_000);
            all_delivery.bus = bus;
            let plan = io::plan_bounce(&session, &all_delivery).unwrap();
            assert_eq!(plan.frames, boundary);
            let encoded_upper = plan.frames as u64 * channels as u64 * 4 + HEADER_ALLOWANCE;
            assert!(encoded_upper <= DELIVERY_BYTES);
            all_delivery.range.end += 1;
            assert!((all_delivery.range.len() as u64) * channels as u64 * 4 + HEADER_ALLOWANCE > DELIVERY_BYTES);
            assert!(io::plan_bounce(&session, &all_delivery).is_err());
        }
    }
}

#[test]
fn four_hour_admission_includes_source_tail_and_preroll_as_one_processing_span() {
    let mut session = empty(ChannelFormat::Stereo);
    session.sample_rate = SampleRate::new(8_000).unwrap();
    const FOUR_HOURS: i64 = 115_200_000;
    let req = request(Range::new(17, FOUR_HOURS - 7), 7, 0, 8_000);
    let plan = io::plan_bounce(&session, &req).unwrap();
    assert_eq!(plan.processing_frames, FOUR_HOURS as usize);
    assert_eq!(plan.frames, (FOUR_HOURS - 17) as usize);
    for change in 0..3 {
        let mut plus = req.clone();
        match change {
            0 => plus.range.end += 1,
            1 => plus.tail_samples += 1,
            _ => plus.preroll_start -= 1,
        }
        assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("4 hours"));
    }
    assert!(session.sources.is_empty());
}

#[test]
fn conversion_rounding_output_capacity_aggregate_capacity_and_fold_capacity_are_independent() {
    assert_eq!(soundcraft_dsp::offline::MAX_OFFLINE_SAMPLES, CONVERTED_VALUES);
    for (format, channels) in widths() {
        let session = empty(format);
        // Two-times conversion reaches the output conversion cap before aggregate memory.
        let source_limit = CONVERTED_VALUES / (2 * channels);
        let req = request(Range::new(11, 11 + source_limit as i64 - 2), 2, 0, 96_000);
        let plan = io::plan_bounce(&session, &req).unwrap();
        assert_eq!(plan.session_frames, source_limit);
        assert_eq!(plan.frames, source_limit * 2);
        assert_eq!(plan.channels, channels);
        assert!(plan.frames * channels <= CONVERTED_VALUES);
        assert!((plan.session_frames + plan.frames) * channels <= SAMPLE_VALUES);
        let mut plus = req.clone();
        plus.tail_samples += 1;
        assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("conversion allocation limit"));
        // One-half conversion reaches simultaneous source+delivery storage first. The
        // half-up rounded output below is integer arithmetic, not a production resampler.
        let frame_budget = SAMPLE_VALUES / channels;
        let source_limit = (frame_budget * 2) / 3;
        let req = request(Range::new(0, source_limit as i64 - 19), 19, 0, 24_000);
        let plan = io::plan_bounce(&session, &req).unwrap();
        let rounded = source_limit.div_ceil(2);
        assert_eq!(plan.session_frames, source_limit);
        assert_eq!(plan.frames, rounded);
        assert!((source_limit + rounded) * channels <= SAMPLE_VALUES);
        assert!((source_limit + 1 + (source_limit + 1).div_ceil(2)) * channels > SAMPLE_VALUES);
        let mut plus = req;
        plus.range.end += 1;
        assert!(io::plan_bounce(&session, &plus).unwrap_err().to_string().contains("buffer allocation limit"));
    }
    let session = empty(ChannelFormat::Stereo);
    for (selected, tail, rate, expected) in [(1, 0, 24_000, 1), (3, 2, 24_000, 3), (7, 4, 24_000, 6), (25, 0, 44_100, 23), (3, 2, 96_000, 10)] {
        let plan = io::plan_bounce(&session, &request(Range::new(11, 11 + selected), tail, 0, rate)).unwrap();
        assert_eq!(plan.frames, expected);
        assert_eq!(plan.session_frames, (selected + tail) as usize);
        assert_eq!(plan.processing_frames, (11 + selected + tail) as usize, "preroll is work, not delivery");
    }
    assert!(io::plan_bounce(&session, &request(Range::new(0, 1), 0, 0, 8_000)).unwrap_err().to_string().contains("zero output frames"));
    let session = empty(ChannelFormat::Atmos916);
    let boundary = SAMPLE_VALUES / 18;
    let mut req = request(Range::new(0, boundary as i64 - 7), 7, 0, 48_000);
    req.fold_stereo = true;
    let plan = io::plan_bounce(&session, &req).unwrap();
    assert_eq!(plan.channels, 2);
    assert_eq!(plan.frames, boundary);
    assert!(boundary * (16 + 2) <= SAMPLE_VALUES);
    assert!((boundary + 1) * (16 + 2) > SAMPLE_VALUES);
    req.tail_samples += 1;
    assert!(io::plan_bounce(&session, &req).unwrap_err().to_string().contains("buffer allocation limit"));
}

#[test]
fn encoding_policy_refuses_unsupported_width_rate_depth_and_hostile_public_windows() {
    let mut wide = empty(ChannelFormat::Atmos916);
    let mut req = request(Range::new(0, 512), 0, 0, 48_000);
    req.encode.format = FileFormat::Flac;
    req.encode.bit_depth = BitDepth::Int24;
    assert!(io::plan_bounce(&wide, &req).unwrap_err().to_string().contains("FLAC"));
    req.fold_stereo = true;
    assert_eq!(io::plan_bounce(&wide, &req).unwrap().channels, 2);
    req.encode.bit_depth = BitDepth::Float32;
    assert!(io::plan_bounce(&wide, &req).unwrap_err().to_string().contains("FLAC"));
    let session = empty(ChannelFormat::Surround51);
    req.fold_stereo = false;
    req.encode.bit_depth = BitDepth::Int24;
    req.sample_rate = SampleRate::new(655_350).unwrap();
    assert!(io::plan_bounce(&session, &req).is_ok());
    req.sample_rate = SampleRate::new(655_351).unwrap();
    assert!(io::plan_bounce(&session, &req).unwrap_err().to_string().contains("FLAC"));
    req.encode.format = FileFormat::Mp3;
    assert!(io::plan_bounce(&session, &req).unwrap_err().to_string().contains("encoding format"));
    for (range, tail, preroll) in [
        (Range { start: 42, end: 41 }, 0, 0),
        (Range { start: i64::MIN, end: i64::MAX }, 0, i64::MIN),
        (Range::new(0, 3), i64::MAX, 0),
        (Range::new(0, 3), 0, i64::MIN),
        (Range::point(0), 0, 0),
    ] {
        assert!(io::plan_bounce(&session, &request(range, tail, preroll, 48_000)).is_err());
    }
    wide.outputs[0].format = ChannelFormat::Ambisonics(4);
    assert!(io::plan_bounce(&wide, &request(Range::new(0, 3), 0, 0, 48_000)).unwrap_err().to_string().contains("at most 16"));
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-bounce-window-bounds-{}-{nonce}-{serial}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct State {
    document: Arc<Session>,
    json: String,
    revision: u64,
    dirty: bool,
    undo: Vec<String>,
    redo: Option<String>,
    journal: Vec<(String, Value)>,
    path: Option<String>,
    generation: u64,
    transport: Value,
}
impl State {
    fn capture(engine: &Engine) -> Self {
        Self {
            document: engine.session_arc(),
            json: engine.session().to_json().unwrap(),
            revision: engine.revision,
            dirty: engine.is_dirty(),
            undo: engine.undo_history(),
            redo: engine.redo_label().map(str::to_owned),
            journal: engine.journal.clone(),
            path: engine.path.clone(),
            generation: engine.session_generation(),
            transport: serde_json::to_value(engine.transport).unwrap(),
        }
    }
    fn unchanged(&self, engine: &Engine) {
        assert!(Arc::ptr_eq(&self.document, &engine.session_arc()));
        assert_eq!(engine.session().to_json().unwrap(), self.json);
        assert_eq!(engine.revision, self.revision);
        assert_eq!(engine.is_dirty(), self.dirty);
        assert_eq!(engine.undo_history(), self.undo);
        assert_eq!(engine.redo_label(), self.redo.as_deref());
        assert_eq!(engine.journal, self.journal);
        assert_eq!(engine.path, self.path);
        assert_eq!(engine.session_generation(), self.generation);
        assert_eq!(serde_json::to_value(engine.transport).unwrap(), self.transport);
        for source in &self.document.sources {
            let before = self.document.pool.get(source.id).unwrap();
            let after = engine.session().pool.get(source.id).unwrap();
            assert!(Arc::ptr_eq(before, after));
            assert_eq!(before.buffer, after.buffer);
        }
    }
}
const CHANNELS: [f32; 16] =
    [0.03125, -0.0625, 0.09375, -0.125, 0.15625, -0.1875, 0.21875, -0.25, 0.28125, -0.3125, 0.34375, -0.375, 0.40625, -0.4375, 0.46875, -0.5];
fn sample(channel: usize, frame: usize) -> f32 {
    CHANNELS[channel] * if frame.is_multiple_of(2) { 1.0 } else { -0.5 }
}
fn fixture(main: ChannelFormat, delivery: ChannelFormat, named_bus: bool) -> (Engine, TrackId) {
    let mut session = empty(main);
    let track = session.add_track(TrackKind::Audio, delivery, Some("Literal channel identity — 原始"));
    let source = io::add_source(
        &mut session,
        "Immutable literal channels",
        AudioBuffer { sample_rate: 48_000, channels: (0..delivery.channels()).map(|ch| (0..64).map(|at| sample(ch, at)).collect()).collect() },
        None,
        FileFormat::Wav,
    );
    let id = session.new_clip_id();
    let bus = named_bus.then(|| session.add_bus("Literal stem", delivery));
    let tr = session.track_mut(track).unwrap();
    tr.mixer.pan = vec![-1.0, 1.0];
    tr.comments = "Preserve original channel meaning\n原始".into();
    if let Some(bus) = bus {
        tr.mixer.output = Route::Bus(bus);
    }
    tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "Whole source extends beyond requested cutoff", source, 0, 0, 64));
    session.edit.selection = Range::new(11, 29);
    let mut engine = Engine::new(session);
    engine.execute("track.comments", &json!({"track":track.0,"comments":"Redo after export"})).unwrap();
    assert!(engine.undo());
    (engine, track)
}
fn chunk<'a>(bytes: &'a [u8], name: &[u8; 4]) -> &'a [u8] {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
    let mut at = 12;
    while at < bytes.len() {
        assert!(at + 8 <= bytes.len());
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let end = at + 8 + size;
        assert!(end <= bytes.len());
        if &bytes[at..at + 4] == name {
            return &bytes[at + 8..end];
        }
        at = end + usize::from(!size.is_multiple_of(2));
    }
    panic!("required WAV chunk absent");
}

#[test]
fn tiny_main_and_named_bus_six_sixteen_channel_files_preserve_literal_order_masks_tail_and_redo() {
    for (main, format, named_bus, channels, mask) in [
        (ChannelFormat::Atmos916, ChannelFormat::Atmos916, false, 16, 0x2_d6ff_u32),
        (ChannelFormat::Surround51, ChannelFormat::Surround51, false, 6, 0x3f),
        (ChannelFormat::Stereo, ChannelFormat::Atmos916, true, 16, 0x2_d6ff),
        (ChannelFormat::Atmos916, ChannelFormat::Surround51, true, 6, 0x3f),
    ] {
        let (mut engine, track) = fixture(main, format, named_bus);
        let before = State::capture(&engine);
        let directory = Directory::new();
        let path = directory.0.join("Literal.wav");
        let receipt=engine.execute("file.bounce_mix",&json!({"path":path,"source":if named_bus{"Literal stem"}else{"main"},"format":"wav","bit_depth":"32f","dither":false,"normalize":false,"start":11,"end":29,"tail_samples":5,"preroll_start":0})).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let fmt = chunk(&bytes, b"fmt ");
        let data = chunk(&bytes, b"data");
        assert_eq!(u16::from_le_bytes(fmt[..2].try_into().unwrap()), 0xfffe);
        assert_eq!(fmt.len(), 40);
        assert_eq!(u16::from_le_bytes(fmt[2..4].try_into().unwrap()), channels);
        assert_eq!(u32::from_le_bytes(fmt[4..8].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(fmt[12..14].try_into().unwrap()), channels * 4);
        assert_eq!(u16::from_le_bytes(fmt[14..16].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(fmt[20..24].try_into().unwrap()), mask);
        assert_eq!(&fmt[24..40], &[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]);
        let mut expected = Vec::new();
        for at in 11..34 {
            for ch in 0..usize::from(channels) {
                expected.extend_from_slice(&(if at < 29 { sample(ch, at) } else { 0.0f32 }).to_le_bytes());
            }
        }
        assert_eq!(data, expected);
        assert_eq!(data.len(), 23 * usize::from(channels) * 4);
        let (info, decoded) = soundcraft_audio_io::decode(&bytes, Some("wav")).unwrap();
        assert_eq!(info.frames, 23);
        assert_eq!(info.channels, channels);
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(decoded.channels.len(), usize::from(channels));
        for (ch, audio) in decoded.channels.iter().enumerate() {
            assert_eq!(audio.len(), 23);
            for (offset, &actual) in audio.iter().enumerate() {
                let at = 11 + offset;
                let expected = if at < 29 { sample(ch, at) } else { 0.0 };
                assert!(actual.is_finite());
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
        assert_eq!(receipt["frames"], 23);
        assert_eq!(receipt["channels"], channels);
        assert_eq!(receipt["tail_samples"], 5);
        assert_eq!(receipt["seconds"].as_f64().unwrap(), 23.0 / 48_000.0);
        assert_eq!(receipt["bytes"], bytes.len());
        assert_eq!(receipt["delivery"], "native_file_synced");
        assert_eq!(receipt["plugin_state"], "stored_document");
        assert_eq!(receipt["publication"]["complete"], true);
        assert_eq!(receipt["publication"]["sha256"], format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(PathBuf::from(receipt["output_path"].as_str().unwrap()), path.canonicalize().unwrap());
        before.unchanged(&engine);
        assert!(engine.redo());
        assert_eq!(engine.session().track(track).unwrap().comments, "Redo after export");
    }
}

#[test]
fn malformed_extreme_strict_command_windows_refuse_before_file_and_preserve_source_and_redo() {
    let (mut engine, track) = fixture(ChannelFormat::Atmos916, ChannelFormat::Atmos916, false);
    // If a future admission regression reached preparation, this nonexistent built-in
    // sentinel refuses before a huge render allocation. The asserted field/bound error
    // still requires the intended earlier admission, not an unavailable-processor fallback.
    engine.session_mut().track_mut(track).unwrap().mixer.inserts[0] = Some(Insert::new("SC_MX38_DO_NOT_CREATE_BOUNDARY_SENTINEL"));
    let before = State::capture(&engine);
    let directory = Directory::new();
    let path = directory.0.join("MustNotExist.wav");
    for (mut params, field) in [
        (json!({"start":0,"end":1,"tail_samples":-1}), "tail"),
        (json!({"start":0,"end":1,"tail_samples":i64::MAX}), "tail"),
        (json!({"start":0,"end":1,"tail_samples":0.5}), "tail"),
        (json!({"start":0,"end":1,"tail_samples":"1"}), "tail"),
        (json!({"start":0,"end":1,"preroll_start":1}), "preroll"),
        (json!({"start":0,"end":1,"preroll_start":i64::MIN}), "preroll"),
        (json!({"start":0,"end":1,"preroll_start":u64::MAX}), "preroll"),
        (json!({"start":0,"end":1,"preroll_start":-0.5}), "preroll"),
        (json!({"start":i64::MIN,"end":i64::MAX}), "range"),
        (json!({"start":0,"end":0}), "range"),
        (json!({"start":0,"length":0}), "length"),
        (json!({"start":0,"end":1,"length":1}), "range"),
        (json!({"start":0,"end":10,"sample_rate":0}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":7999}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":768001}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":48000.5}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":u64::MAX}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":"96000"}), "sample"),
        (json!({"start":0,"end":10,"sample_rate":true}), "sample"),
        (json!({"start":0,"end":1,"sample_rate":8000}), "zero output frames"),
        (json!({"start":0,"end":(SAMPLE_VALUES/16+1),"preroll_start":0}), "allocation"),
        (json!({"start":1,"end":(SAMPLE_VALUES/16-17),"tail_samples":17,"preroll_start":-1}), "processing bounds"),
    ] {
        params["path"] = json!(path);
        let error = engine.execute("file.bounce_mix", &params).unwrap_err().to_string();
        assert!(error.contains(field), "expected {field} admission, got {error}");
        assert!(!error.contains("plugin instance"));
        assert!(!path.exists());
        before.unchanged(&engine);
    }
    assert!(engine.redo());
    assert_eq!(engine.session().track(track).unwrap().comments, "Redo after export");
}
