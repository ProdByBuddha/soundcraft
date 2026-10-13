//! SC-ED46 persistence and ownership acceptance using independently authored PCM16 RIFF bytes.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use soundcraft_engine::{Engine, io};
use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
use soundcraft_model::{ChannelFormat, Clip, ClipContent, ClipId, Playlist, Session, SourceId, TrackId, TrackKind, VideoSource};
use soundcraft_time::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("soundcraft-reusable-clip-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn text(&self, name: &str) -> String {
        self.path(name).to_string_lossy().into_owned()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn sample(ch: usize, i: usize) -> f32 {
    let integer = if ch == 0 { 1024 + (i * 13 % 12000) as i16 } else { -3072 - (i * 7 % 10000) as i16 };
    f32::from(integer) / 32768.0
}
fn chunk(dst: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    dst.extend_from_slice(id);
    dst.extend_from_slice(&(body.len() as u32).to_le_bytes());
    dst.extend_from_slice(body);
    if !body.len().is_multiple_of(2) {
        dst.push(0);
    }
}
fn wave() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&48000u32.to_le_bytes());
    fmt.extend_from_slice(&192000u32.to_le_bytes());
    fmt.extend_from_slice(&4u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    let mut pcm = Vec::new();
    for i in 0..1024 {
        for ch in 0..2 {
            pcm.extend_from_slice(&((sample(ch, i) * 32768.0) as i16).to_le_bytes());
        }
    }
    let mut body = b"WAVE".to_vec();
    chunk(&mut body, b"fmt ", &fmt);
    chunk(&mut body, b"JvOr", b"SC-ED46\0opaque\xff\x81 retention fixture!");
    chunk(&mut body, b"data", &pcm);
    chunk(&mut body, b"tail", b"independent trailing bytes");
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes
}
fn audio(dir: &Directory) -> (Engine, TrackId, ClipId, SourceId, Vec<u8>) {
    let bytes = wave();
    let input = dir.path("input.wav");
    std::fs::write(&input, &bytes).unwrap();
    let mut e = Engine::default();
    e.configure_original_store(dir.path("original-store"), 1 << 20).unwrap();
    let result = e.execute("file.import_audio", &json!({"path":input,"policy":"copy"})).unwrap();
    let row = &result["imported"][0];
    let t = TrackId(row["track"].as_u64().unwrap());
    let id = ClipId(row["clip"].as_u64().unwrap());
    let source = SourceId(row["source"].as_u64().unwrap());
    let s = e.session_mut();
    let c = s.find_clip_mut(id).unwrap();
    c.start = 211;
    c.length = 421;
    c.content = ClipContent::Audio { source, offset: 31 };
    c.stretch = 2.5;
    c.gain_db = -3.0;
    let mut alternate = c.clone();
    alternate.id = s.new_clip_id();
    alternate.start = 800;
    let mut pl = Playlist::new("Alternate instance");
    pl.clips.push(alternate);
    s.track_mut(t).unwrap().playlists.push(pl);
    s.edit.selected_tracks = vec![t];
    s.edit.selected_clips = vec![id];
    s.edit.selection = Range::new(248, 515);
    (e, t, id, source, bytes)
}
fn capture(e: &mut Engine, id: ClipId, start: i64, end: i64) -> u64 {
    let result = e.execute("clip.capture", &json!({"clip":id,"start":start,"end":end,"name":"Retained after deletion"})).unwrap();
    assert_eq!(result["count"], 1);
    result["definitions"][0]["id"].as_u64().unwrap()
}
fn list(e: &mut Engine) -> Value {
    e.execute("clip.definitions", &json!({})).unwrap()["definitions"].clone()
}
fn remove_all(e: &mut Engine) {
    let ids: Vec<_> = e.session().all_clips().map(|c| c.id).collect();
    e.execute("clip.clear", &json!({"clips":ids})).unwrap();
    assert_eq!(e.session().all_clips().count(), 0, "alternate playlists count as timeline instances");
}
fn place(e: &mut Engine, definition: u64, t: TrackId, at: i64) -> ClipId {
    let result = e.execute("clip.place_definition", &json!({"definition":definition,"track":t,"at":at})).unwrap();
    ClipId(result["clip"].as_u64().unwrap())
}
fn resolve(s: &Session, path: &str) -> PathBuf {
    if Path::new(path).is_absolute() { PathBuf::from(path) } else { s.folder.as_ref().unwrap().join(path) }
}
fn original(e: &Engine, source: SourceId, bytes: &[u8]) {
    let provenance = e.session().source(source).unwrap().original.as_ref().unwrap();
    assert_eq!(provenance.bytes, bytes.len() as u64);
    assert_eq!(provenance.sha256, format!("{:x}", Sha256::digest(bytes)));
    assert_eq!(provenance.info.sample_rate, 48000);
    assert_eq!(provenance.info.channels, 2);
    assert_eq!(std::fs::read(resolve(e.session(), &provenance.path)).unwrap(), bytes);
    assert!(e.session().pool.original_asset(source).is_some());
}
fn pcm(e: &Engine, t: TrackId, at: i64) {
    let actual = soundcraft_mix::render_clips(e.session(), t, Range::new(at, at + 267));
    assert_eq!(actual.len(), 2);
    let mut energy = [0.0f64; 2];
    for (ch, channel) in actual.iter().enumerate() {
        assert_eq!(channel.len(), 267);
        for (j, &a) in channel.iter().enumerate() {
            let x = 31.0 + (37 + j) as f64 / 2.5;
            let q = x.floor() as usize;
            let f = (x - q as f64) as f32;
            let s = sample(ch, q) + (sample(ch, q + 1) - sample(ch, q)) * f;
            let expected = f64::from(s) * 10f64.powf(-3.0 / 20.0);
            assert!(a.is_finite());
            assert!((f64::from(a) - expected).abs() < 2e-6, "ch{ch} sample{j}: {a} vs {expected}");
            energy[ch] += expected * expected;
        }
    }
    assert!(energy[0] > 0.01 && energy[1] > 0.01);
}

#[test]
fn native_save_open_and_moved_strict_copy_reuse_definition_only_source_and_original_bytes() {
    let dir = Directory::new();
    let (mut e, t, old, source, bytes) = audio(&dir);
    let definition = capture(&mut e, old, 248, 515);
    let retained = list(&mut e);
    remove_all(&mut e);
    std::fs::remove_file(dir.path("input.wav")).unwrap();
    io::save_session(&mut e, &dir.text("normal/Retained.scraft")).unwrap();
    assert_eq!(list(&mut e), retained);
    let mut opened = Engine::default();
    assert!(io::open_session(&mut opened, &dir.text("normal/Retained.scraft")).unwrap().is_empty());
    assert_eq!(opened.session().all_clips().count(), 0);
    assert_eq!(list(&mut opened), retained);
    original(&opened, source, &bytes);
    let snapshot = e.session_arc();
    let state = (e.revision, e.is_dirty(), e.undo_history(), e.redo_label().map(str::to_owned), e.path.clone(), e.journal.clone());
    let result = e.execute("session.save_copy", &json!({"path":dir.text("copy/Portable.scraft")})).unwrap();
    assert_eq!(result["audio_complete"], true);
    assert_eq!(result["audio_sources_packaged"], 1);
    assert!(Arc::ptr_eq(&snapshot, &e.session_arc()));
    assert_eq!((e.revision, e.is_dirty(), e.undo_history(), e.redo_label().map(str::to_owned), e.path.clone(), e.journal.clone()), state);
    drop(snapshot);
    drop(opened);
    drop(e);
    std::fs::rename(dir.path("copy"), dir.path("moved")).unwrap();
    std::fs::remove_dir_all(dir.path("normal")).unwrap();
    std::fs::remove_dir_all(dir.path("original-store")).unwrap();
    let mut moved = Engine::default();
    assert!(io::open_session(&mut moved, &dir.text("moved/Portable.scraft")).unwrap().is_empty());
    assert_eq!(moved.session().all_clips().count(), 0);
    assert_eq!(list(&mut moved), retained);
    original(&moved, source, &bytes);
    let first = place(&mut moved, definition, t, 1003);
    let second = place(&mut moved, definition, t, 4097);
    assert_ne!(first, old);
    assert_ne!(first, second);
    assert_ne!(first.0, definition);
    pcm(&moved, t, 1003);
    pcm(&moved, t, 4097);
    let second_before = moved.session().find_clip(second).unwrap().1.clone();
    moved.session_mut().find_clip_mut(first).unwrap().gain_db = -18.0;
    assert_eq!(moved.session().find_clip(second).unwrap().1, &second_before);
    assert_eq!(list(&mut moved), retained);
}

#[test]
fn recovery_copy_preserves_definition_only_media_and_leaves_the_open_document_untouched() {
    let dir = Directory::new();
    let (mut e, t, old, source, bytes) = audio(&dir);
    let definition = capture(&mut e, old, 248, 515);
    remove_all(&mut e);
    let retained = list(&mut e);
    let snapshot = e.session_arc();
    let state = (e.revision, e.is_dirty(), e.undo_history(), e.redo_label().map(str::to_owned), e.path.clone(), e.journal.clone());
    io::save_copy(&e, &dir.text("recovery/Recovered.scraft")).unwrap();
    assert!(Arc::ptr_eq(&snapshot, &e.session_arc()));
    assert_eq!((e.revision, e.is_dirty(), e.undo_history(), e.redo_label().map(str::to_owned), e.path.clone(), e.journal.clone()), state);
    let mut recovered = Engine::default();
    assert!(io::open_session(&mut recovered, &dir.text("recovery/Recovered.scraft")).unwrap().is_empty());
    assert_eq!(recovered.session().all_clips().count(), 0);
    assert_eq!(list(&mut recovered), retained);
    original(&recovered, source, &bytes);
    let new = place(&mut recovered, definition, t, 1003);
    assert_ne!(new, old);
    pcm(&recovered, t, 1003);
}

#[test]
fn retained_definitions_guard_source_clear_and_original_lease_collection() {
    let dir = Directory::new();
    let (mut e, _, old, source, bytes) = audio(&dir);
    let definition = capture(&mut e, old, 248, 515);
    remove_all(&mut e);
    let retained = list(&mut e);
    let held_session = e.session_arc();
    let held_audio = Arc::clone(held_session.pool.get(source).unwrap());
    let asset = held_audio.original_asset.as_ref().unwrap().path.clone();
    for with_clips in [false, true] {
        let before = e.session_arc();
        let state = (e.revision, e.undo_history(), e.redo_label().map(str::to_owned));
        assert!(e.execute("clip.clear", &json!({"sources":[source],"with_clips":with_clips})).is_err());
        assert!(Arc::ptr_eq(&before, &e.session_arc()));
        assert_eq!((e.revision, e.undo_history(), e.redo_label().map(str::to_owned)), state);
        assert_eq!(list(&mut e), retained);
        assert_eq!(e.collect_original_assets().unwrap(), 0);
        assert_eq!(std::fs::read(&asset).unwrap(), bytes);
    }
    e.execute("clip.clear_definitions", &json!({"definitions":[definition]})).unwrap();
    assert!(list(&mut e).as_array().unwrap().is_empty());
    assert!(e.undo());
    assert_eq!(list(&mut e), retained);
    assert!(e.redo());
    e.execute("clip.clear", &json!({"sources":[source]})).unwrap();
    assert!(e.session().source(source).is_none());
    assert_eq!(e.collect_original_assets().unwrap(), 0, "Undo and held/prepared owners retain encoded original");
    assert!(e.undo());
    assert!(e.session().source(source).is_some());
    assert!(e.redo());
    e.replace_session(Session::default());
    assert_eq!(e.collect_original_assets().unwrap(), 0);
    drop(held_session);
    assert_eq!(e.collect_original_assets().unwrap(), 0, "decoded audio Arc is an independent prepared-style owner");
    drop(held_audio);
    assert!(asset.exists(), "lease Drop performs no filesystem work");
    assert_eq!(e.collect_original_assets().unwrap(), 1);
    assert!(!asset.exists());
}

#[test]
fn missing_derivative_keeps_definition_and_original_but_strict_copy_rejects_atomically() {
    let dir = Directory::new();
    let (mut e, _, old, source, bytes) = audio(&dir);
    let definition = capture(&mut e, old, 248, 515);
    remove_all(&mut e);
    io::save_session(&mut e, &dir.text("normal/Missing.scraft")).unwrap();
    let retained = list(&mut e);
    let working = resolve(e.session(), &e.session().source(source).unwrap().path);
    std::fs::remove_file(working).unwrap();
    let mut missing = Engine::default();
    assert_eq!(io::open_session(&mut missing, &dir.text("normal/Missing.scraft")).unwrap().len(), 1);
    assert_eq!(list(&mut missing), retained);
    assert_eq!(missing.session().all_clips().count(), 0);
    assert!(!missing.session().pool.contains(source));
    original(&missing, source, &bytes);
    let snapshot = missing.session_arc();
    assert!(missing.execute("session.save_copy", &json!({"path":dir.text("rejected/Copy.scraft")})).is_err());
    assert!(!dir.path("rejected").exists());
    assert!(Arc::ptr_eq(&snapshot, &missing.session_arc()));
    io::save_copy(&missing, &dir.text("recovery/Missing.scraft")).unwrap();
    let mut recovered = Engine::default();
    assert_eq!(io::open_session(&mut recovered, &dir.text("recovery/Missing.scraft")).unwrap().len(), 1);
    assert_eq!(list(&mut recovered), retained);
    assert!(recovered.session().clip_definitions.iter().any(|d| d.id.0 == definition));
    original(&recovered, source, &bytes);
}

#[test]
fn midi_and_video_definitions_survive_native_open_with_no_timeline_instances() {
    let dir = Directory::new();
    let mut s = Session::default();
    let midi_t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Retained MIDI"));
    let midi = s.new_clip_id();
    let note = Note { pitch: 62, velocity: 99, release_velocity: 73, channel: 3, start: 20, length: 600 };
    let current = Sequence { notes: vec![note], ctrls: vec![CtrlEvent { tick: 180, channel: 3, kind: CtrlKind::Program, value: 11 }] };
    let mut original_note = note;
    original_note.pitch = 65;
    original_note.start = 80;
    original_note.length = 200;
    s.midi_originals.insert(midi.0, Sequence { notes: vec![original_note], ctrls: vec![] });
    s.track_mut(midi_t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(midi, "MIDI parent", 24_000, 20_000, current));
    let video_t = s.add_track(TrackKind::Video, ChannelFormat::Mono, Some("Retained video"));
    let video_source = SourceId(s.alloc());
    s.videos.push(VideoSource {
        id: video_source,
        name: "Referenced picture".into(),
        path: dir.text("external.mov"),
        width: 320,
        height: 240,
        frame_rate: 30.0,
        duration: 1.0,
        codec: "mjpeg".into(),
    });
    let video = s.new_clip_id();
    s.track_mut(video_t).unwrap().playlist_mut().unwrap().clips.push(Clip::video(video, "Picture parent", video_source, 31, 211, 421));
    s.edit.selected_clips = vec![midi];
    s.edit.selection = Range::new(26_500, 34_000);
    let mut e = Engine::new(s);
    let midi_def = capture(&mut e, midi, 26_500, 34_000);
    let video_def = capture(&mut e, video, 248, 515);
    remove_all(&mut e);
    let retained = list(&mut e);
    io::save_session(&mut e, &dir.text("mixed/Retained.scraft")).unwrap();
    drop(e);
    let mut opened = Engine::default();
    io::open_session(&mut opened, &dir.text("mixed/Retained.scraft")).unwrap();
    assert_eq!(opened.session().all_clips().count(), 0);
    assert_eq!(list(&mut opened), retained);
    assert!(!opened.session().midi_originals.contains_key(&midi.0), "old clip performance is no longer a timeline owner");
    let new_midi = place(&mut opened, midi_def, midi_t, 48_000);
    assert_ne!(new_midi, midi);
    let cropped = Sequence {
        notes: vec![Note { start: 0, length: 300, ..note }],
        ctrls: vec![CtrlEvent { tick: 80, channel: 3, kind: CtrlKind::Program, value: 11 }],
    };
    assert_eq!(opened.session().find_clip(new_midi).unwrap().1.content, ClipContent::Midi { sequence: cropped });
    let cropped_original = Sequence { notes: vec![Note { start: 0, length: 180, ..original_note }], ctrls: vec![] };
    assert_eq!(opened.session().midi_originals.get(&new_midi.0), Some(&cropped_original));
    opened.execute("event.restore_performance", &json!({"clips":[new_midi]})).unwrap();
    assert_eq!(opened.session().find_clip(new_midi).unwrap().1.content, ClipContent::Midi { sequence: cropped_original });
    let new_video = place(&mut opened, video_def, video_t, 1003);
    assert_ne!(new_video, video);
    let picture = opened.session().find_clip(new_video).unwrap().1;
    assert_eq!(picture.video_source(), Some(video_source));
    assert_eq!(picture.source_position(83), 151.0);
    assert_eq!(opened.session().video(video_source).unwrap().path, dir.text("external.mov"));
    assert_eq!(list(&mut opened), retained);
}

#[test]
fn legacy_json_defaults_and_definition_ids_remain_safe_through_save_open() {
    let mut value = serde_json::to_value(Session::default()).unwrap();
    value["format_version"] = json!(1);
    value.as_object_mut().unwrap().remove("clip_definitions");
    let legacy = Session::from_json(&value.to_string()).unwrap();
    assert!(legacy.clip_definitions.is_empty());
    let dir = Directory::new();
    let (mut e, t, old, _, _) = audio(&dir);
    let definition = capture(&mut e, old, 248, 515);
    remove_all(&mut e);
    io::save_session(&mut e, &dir.text("ids/Retained.scraft")).unwrap();
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(dir.path("ids/Retained.scraft")).unwrap()).unwrap();
    value["next_id"] = json!(0);
    let repaired = Session::from_json(&value.to_string()).unwrap();
    assert!(repaired.next_id > definition);
    let retained = e.session().clip_definitions[0].clone();
    let mut damaged = value.clone();
    damaged["clip_definitions"][0]["template"]["length"] = json!(-1);
    assert!(Session::from_json(&damaged.to_string()).is_err(), "hostile definition must be rejected before it can become usable media");
    assert_eq!(e.session().clip_definitions[0], retained);
    let mut opened = Engine::default();
    io::open_session(&mut opened, &dir.text("ids/Retained.scraft")).unwrap();
    let new = place(&mut opened, definition, t, 1003);
    assert!(new.0 > definition);
}

#[test]
fn native_reopen_keeps_fades_longer_than_the_retained_window_and_exact_boundary_envelope() {
    let dir = Directory::new();
    let (mut e, t, old, _, _) = audio(&dir);
    let parent = e.session_mut().find_clip_mut(old).unwrap();
    parent.fade_in = soundcraft_model::Fade { len: 397, shape: soundcraft_model::FadeShape::SCurve };
    parent.fade_out = soundcraft_model::Fade { len: 373, shape: soundcraft_model::FadeShape::Exponential };
    parent.gain_env = vec![(0, 0.0), (421, -12.0)];
    let definition = capture(&mut e, old, 248, 315);
    remove_all(&mut e);
    let retained = list(&mut e);
    io::save_session(&mut e, &dir.text("fade/Retained.scraft")).unwrap();
    let mut opened = Engine::default();
    assert!(io::open_session(&mut opened, &dir.text("fade/Retained.scraft")).unwrap().is_empty());
    assert_eq!(list(&mut opened), retained);
    let id = place(&mut opened, definition, t, 1003);
    let clip = opened.session().find_clip(id).unwrap().1;
    assert_eq!(clip.length, 67);
    assert_eq!(clip.fade_in.len, 397);
    assert_eq!(clip.fade_out.len, 373);
    assert_eq!(clip.slice.unwrap().fade_out_remaining_samples, 317);
    let out = soundcraft_mix::render_clips(opened.session(), t, Range::new(1003, 1070));
    let mut energy = 0.0;
    for (ch, channel) in out.iter().enumerate() {
        assert_eq!(channel.len(), 67);
        for (j, &actual) in channel.iter().enumerate() {
            let rel = (37 + j) as f64;
            let x = 31.0 + rel / 2.5;
            let q = x.floor() as usize;
            let f = (x - q as f64) as f32;
            let pcm = sample(ch, q) + (sample(ch, q + 1) - sample(ch, q)) * f;
            let incoming = 0.5 - 0.5 * (std::f64::consts::PI * rel / 397.0).cos();
            let remaining = 421.0 - rel;
            let outgoing = if remaining <= 373.0 { (remaining / 373.0).powi(3) } else { 1.0 };
            let expected = f64::from(pcm) * 10f64.powf((-3.0 - 12.0 * rel / 421.0) / 20.0) * incoming * outgoing;
            assert!((f64::from(actual) - expected).abs() < 2e-6, "native fade ch{ch} frame{j}: {actual} vs {expected}");
            energy += expected * expected;
        }
    }
    assert!(energy > 1e-8, "native cropped fade fixture must be audible and nonzero");
}

#[test]
fn oversized_sparse_native_session_rejects_before_media_work_and_preserves_pending_redo() {
    // Hash only the small owned fixture files. The hostile file remains a sparse metadata
    // fixture and is never read into a test allocation.
    fn inventory(folder: &Path, root: &Path, sparse: &Path) -> Vec<(PathBuf, bool, u64, Option<String>)> {
        let mut result = Vec::new();
        for entry in std::fs::read_dir(folder).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = entry.metadata().unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if metadata.is_dir() {
                result.push((relative, true, 0, None));
                result.extend(inventory(&path, root, sparse));
            } else {
                let hash = if path == sparse {
                    None
                } else {
                    assert!(metadata.len() < 1 << 20, "only bounded owned fixture files are hashed");
                    Some(format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())))
                };
                result.push((relative, false, metadata.len(), hash));
            }
        }
        result.sort();
        result
    }
    let dir = Directory::new();
    let (mut e, t, old, source, bytes) = audio(&dir);
    capture(&mut e, old, 248, 515);
    remove_all(&mut e);
    io::save_session(&mut e, &dir.text("normal/Current.scraft")).unwrap();
    e.execute("track.rename", &json!({"track":t,"name":"Pending oversized-file rename"})).unwrap();
    assert!(e.undo());
    assert!(e.can_redo());
    original(&e, source, &bytes);
    let sparse = dir.path("oversized.scraft");
    let file = std::fs::File::create(&sparse).unwrap();
    let bytes_over_limit = u64::try_from(Session::MAX_JSON_BYTES).unwrap() + 1;
    file.set_len(bytes_over_limit).unwrap();
    drop(file);
    assert_eq!(std::fs::metadata(&sparse).unwrap().len(), bytes_over_limit);
    let files = inventory(&dir.0, &dir.0, &sparse);
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
        e.session_generation(),
    );
    let error = io::open_session(&mut e, &dir.text("oversized.scraft")).unwrap_err();
    assert!(error.to_string().contains("session JSON exceeds 128 MiB"), "{error}");
    assert!(Arc::ptr_eq(&before, &e.session_arc()));
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
            e.clipboard.clone(),
            e.session_generation()
        ),
        state
    );
    assert_eq!(inventory(&dir.0, &dir.0, &sparse), files, "native rejection must not create, rewrite or delete original-store/session files");
    original(&e, source, &bytes);
    assert!(e.redo());
    assert_eq!(e.session().track(t).unwrap().name, "Pending oversized-file rename");
}
