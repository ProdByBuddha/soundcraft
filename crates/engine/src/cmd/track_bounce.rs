//! Selected-track file delivery. Plans and exports do not mutate or journal the document.

use super::*;
use serde_json::json;
use soundcraft_audio_io::{BitDepth, EncodeOptions, FileFormat};
use soundcraft_mix::track_bounce::{self as render, Options};
use soundcraft_model::{ClipContent, TrackKind};
use std::path::{Path, PathBuf};

const ID: &str = "track.bounce";
const PARAMS: &str = "{tracks?, path? (single track) | dir? (native batch), format?: wav|aiff|flac, bit_depth?: 16|24|32f, dither?: true, render_volume?: true, render_pan?: true, start?, end?, tail_seconds?: 0, expected_revision?}";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: ID,
            label: "Bounce...",
            menu: &["Track"],
            shortcut: Some("Cmd+Alt+Shift+B"),
            params: PARAMS,
            enabled: has_selection,
            run: bounce,
            journal: false,
            undoable: false,
            view: false,
        },
        CommandSpec {
            id: "track.bounce_preview",
            label: "Preview Track Bounce",
            menu: &[],
            shortcut: None,
            params: PARAMS,
            enabled: has_selection,
            run: preview,
            journal: false,
            undoable: false,
            view: false,
        },
    ]
}

struct FilePlan {
    render: render::Plan,
    path: PathBuf,
    name: String,
}

struct Plan {
    files: Vec<FilePlan>,
    issues: Vec<Value>,
    opts: EncodeOptions,
    revision: u64,
}

fn boolean(p: &Value, key: &str, default: bool) -> Result<bool> {
    p.get(key).map_or(Ok(default), |v| v.as_bool().ok_or_else(|| bad(ID, format!("`{key}` must be boolean"))))
}

fn text<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>> {
    p.get(key)
        .map(|v| {
            v.as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= 4096)
                .ok_or_else(|| bad(ID, format!("`{key}` must be nonempty text (at most 4096 bytes)")))
        })
        .transpose()
}

fn position(e: &Engine, p: &Value, key: &str) -> Result<Option<i64>> {
    let n = position_param_raw(e, ID, p, key)?;
    if n.is_some_and(|n| !(0..=MAX_POSITION).contains(&n)) {
        return Err(bad(ID, "range positions must be nonnegative and within the position limit"));
    }
    Ok(n)
}

fn source_end(e: &Engine, id: TrackId) -> Result<i64> {
    let s = e.session();
    let t = s.track(id).ok_or_else(|| bad(ID, "unknown source"))?;
    render::validate_metadata(s, t).map_err(|r| bad(ID, r))?;
    let mut end = 0;
    for c in t.clips().iter().filter(|c| !c.muted) {
        let ce = c.start.checked_add(c.length).filter(|_| c.start >= 0 && c.length > 0).ok_or_else(|| bad(ID, "invalid source clip range"))?;
        if t.kind == TrackKind::Instrument {
            if let ClipContent::Midi { sequence } = &c.content {
                let base = s.tempo.samples_to_ticks(c.start, s.sample_rate);
                let duration = s.edit.value(&format!("rtp.{}.duration", id.0), 100.0).clamp(1.0, 1000.0) / 100.0;
                let delay = s.edit.value(&format!("rtp.{}.delay", id.0), 0.0).clamp(-1e7, 1e7) as i64;
                for n in &sequence.notes {
                    let on = base.saturating_add(n.start.saturating_add(delay).max(0));
                    let off = on.saturating_add(((n.length as f64 * duration) as i64).max(1));
                    if s.tempo.tick_to_samples(on, s.sample_rate) < ce {
                        end = end.max(s.tempo.tick_to_samples(off, s.sample_rate).min(ce));
                    }
                }
            }
        } else {
            end = end.max(ce);
        }
    }
    Ok(end)
}

fn extension(f: FileFormat) -> &'static str {
    match f {
        FileFormat::Aiff => "aif",
        FileFormat::Flac => "flac",
        _ => "wav",
    }
}

fn stem(name: &str, id: TrackId) -> String {
    let clean = crate::io::sanitize_name(name);
    let mut out = String::new();
    for c in clean.chars() {
        if out.len() + c.len_utf8() > 96 {
            break;
        }
        out.push(c);
    }
    format!("{out}-{}", id.0)
}

fn plan(e: &Engine, p: &Value) -> Result<Plan> {
    if cfg!(all(not(unix), not(target_arch = "wasm32"))) {
        return Err(bad(
            ID,
            "native Track Bounce publication currently requires Unix file identity and a hard-link-capable filesystem; Windows delivery is unsupported",
        ));
    }
    let object = p.as_object().ok_or_else(|| bad(ID, "parameters must be an object"))?;
    let keys = [
        "tracks",
        "track",
        "path",
        "dir",
        "format",
        "bit_depth",
        "dither",
        "render_volume",
        "render_pan",
        "start",
        "end",
        "tail_seconds",
        "expected_revision",
    ];
    if let Some(key) = object.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(bad(ID, format!("unsupported option `{key}`")));
    }
    if let Some(v) = p.get("expected_revision")
        && v.as_u64() != Some(e.revision)
    {
        return Err(bad(ID, "Track Bounce preview is stale; reopen the dialog or preview the current revision"));
    }
    if p.get("tracks").is_some_and(|v| !v.is_array() || v.as_array().is_some_and(|v| v.len() > 128)) {
        return Err(bad(ID, "`tracks` must be an array of at most 128 sources"));
    }
    if p.get("tracks").is_none() && p.get("track").is_none() && e.session().edit.selected_tracks.len() > 128 {
        return Err(bad(ID, "at most 128 selected sources can be delivered in one batch"));
    }
    if p.get("tracks").is_some() && p.get("track").is_some() {
        return Err(bad(ID, "give track or tracks, exclusively"));
    }
    let mut tracks = tracks_required(e, ID, p)?;
    let mut seen = std::collections::HashSet::new();
    tracks.retain(|t| seen.insert(*t));
    if tracks.len() > 128 {
        return Err(bad(ID, "at most 128 sources can be delivered in one batch"));
    }
    let path = text(p, "path")?;
    let dir = text(p, "dir")?;
    if (path.is_some() == dir.is_some()) || (path.is_some() && tracks.len() != 1) {
        return Err(bad(ID, "give `path` for one source or `dir` for a batch, exclusively"));
    }
    if cfg!(target_arch = "wasm32") && (dir.is_some() || tracks.len() != 1) {
        return Err(bad(ID, "web Track Bounce supports one file destination; native batch directories are unsupported"));
    }
    let named_format = text(p, "format")?;
    let ext = path.and_then(|p| Path::new(p).extension()).and_then(|x| x.to_str());
    let format = soundcraft_audio_io::encode_format_for(named_format.or(ext).unwrap_or("wav"))
        .ok_or_else(|| bad(ID, "supported file formats: wav, aiff, flac"))?;
    if path.is_some() && ext.and_then(soundcraft_audio_io::encode_format_for) != Some(format) {
        return Err(bad(ID, "file extension must match the requested format"));
    }
    let bit_depth = match p.get("bit_depth") {
        None => BitDepth::Int24,
        Some(v) if v == 16 || v == "16" => BitDepth::Int16,
        Some(v) if v == 24 || v == "24" => BitDepth::Int24,
        Some(v) if v == "32f" => BitDepth::Float32,
        _ => return Err(bad(ID, "bit_depth must be 16, 24 or 32f")),
    };
    if format == FileFormat::Flac && bit_depth == BitDepth::Float32 {
        return Err(bad(ID, "FLAC cannot store 32-bit float"));
    }
    let dither = boolean(p, "dither", true)? && bit_depth != BitDepth::Float32;
    let options = Options { volume: boolean(p, "render_volume", true)?, pan: boolean(p, "render_pan", true)? };
    let start = position(e, p, "start")?;
    let end = position(e, p, "end")?;
    if start.is_some() != end.is_some() {
        return Err(bad(ID, "start and end must be supplied together"));
    }
    let edit = &e.session().edit;
    let timeline = if edit.link_timeline_edit { edit.selection } else { edit.timeline_selection };
    let selected = if let (Some(start), Some(end)) = (start, end) {
        Some(Range { start, end })
    } else if !timeline.is_empty() {
        Some(timeline)
    } else {
        None
    };
    if selected.is_some_and(|r| r.start < 0 || r.end <= r.start) {
        return Err(bad(ID, "range must have nonnegative start < end"));
    }
    let seconds = p.get("tail_seconds").map_or(Ok(0.0), |v| {
        v.as_f64()
            .filter(|v| v.is_finite() && (0.0..=14_400.0).contains(v))
            .ok_or_else(|| bad(ID, "tail_seconds must be finite and between 0 and 14400"))
    })?;
    let tail = e.session().sample_rate.samples(seconds);
    let mut files = Vec::new();
    let mut issues = Vec::new();
    let mut aggregate = 0usize;
    for id in tracks {
        let t = e.session().track(id).ok_or_else(|| bad(ID, "unknown track"))?;
        if t.name.len() > 4096 {
            return Err(bad(ID, "track name exceeds export metadata limit"));
        }
        let file =
            path.map_or_else(|| Path::new(dir.unwrap_or_default()).join(format!("{}.{}", stem(&t.name, id), extension(format))), PathBuf::from);
        let result = (|| {
            let range = match selected {
                Some(r) => r,
                None => Range { start: 0, end: source_end(e, id)? },
            };
            let end = range.end.checked_add(tail).filter(|v| *v <= MAX_POSITION).ok_or_else(|| bad(ID, "tail exceeds the position limit"))?;
            let render = render::plan(e.session(), id, Range { start: range.start, end }, range.end, options).map_err(|r| bad(ID, r))?;
            if format == FileFormat::Flac && (render.format.channels() > 8 || e.session().sample_rate.hz() > 655_350) {
                return Err(bad(ID, "FLAC supports at most eight channels and 655350 Hz"));
            }
            aggregate = aggregate
                .checked_add(render.memory_bytes)
                .filter(|n| *n <= render::MAX_BOUNCE_BYTES)
                .ok_or_else(|| bad(ID, "batch PCM, codec, workspace and Instrument preroll estimates exceed the 512 MiB aggregate policy budget"))?;
            check_destination(&file)?;
            Ok(render)
        })();
        match result {
            Ok(render) => files.push(FilePlan { render, path: file, name: t.name.clone() }),
            Err(err) => issues.push(json!({"track": id, "name": t.name, "path": file, "reason": err.to_string()})),
        }
    }
    Ok(Plan { files, issues, opts: EncodeOptions { format, bit_depth, dither, bwf: None }, revision: e.revision })
}

fn manifest(e: &Engine, p: &Plan) -> Value {
    json!({"ready": p.issues.is_empty(), "revision": p.revision, "sample_rate": e.session().sample_rate.hz(), "format": p.opts.format,
        "bit_depth": p.opts.bit_depth, "dither": p.opts.dither, "issues": p.issues,
        "files": p.files.iter().map(|f| json!({"track": f.render.track, "name": f.name, "path": f.path, "layout": f.render.format,
            "channels": f.render.format.channels(), "capture_end": f.render.capture_end, "start": f.render.range.start, "end": f.render.range.end, "frames": f.render.frames,
            "processing_frames": f.render.processing_frames, "preroll_frames": f.render.processing_frames - f.render.frames,
            "memory_bytes": f.render.memory_bytes})).collect::<Vec<_>>()})
}

fn preview(e: &mut Engine, p: &Value) -> Result<Value> {
    let p = plan(e, p)?;
    Ok(manifest(e, &p))
}

#[cfg(any(unix, target_arch = "wasm32"))]
fn encoded(e: &Engine, f: &FilePlan, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let channels = render::render(e.session(), &f.render).map_err(|r| bad(ID, r))?;
    if channels.len() != f.render.format.channels() || channels.iter().any(|c| c.len() != f.render.frames) {
        return Err(bad(ID, "renderer did not return the complete planned shape"));
    }
    let buffer = soundcraft_audio_io::AudioBuffer { sample_rate: e.session().sample_rate.hz(), channels };
    soundcraft_audio_io::encode_with_channel_mask(&buffer, opts, f.render.format.channel_mask()).map_err(|err| EngineError::Io(err.to_string()))
}

fn bounce(e: &mut Engine, p: &Value) -> Result<Value> {
    let p = plan(e, p)?;
    if !p.issues.is_empty() {
        return Err(bad(ID, json!(p.issues).to_string()));
    }
    let mut result = manifest(e, &p);
    #[cfg(target_arch = "wasm32")]
    let sizes = {
        let f = p.files.first().ok_or_else(|| bad(ID, "no source files"))?;
        let bytes = encoded(e, f, &p.opts)?;
        crate::io::write_file(&f.path.to_string_lossy(), &bytes)?;
        vec![bytes.len()]
    };
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    let sizes = native::deliver(e, &p)?;
    #[cfg(all(not(unix), not(target_arch = "wasm32")))]
    let sizes = {
        fn unsupported() -> Result<Vec<usize>> {
            Err(bad(ID, "native Track Bounce publication is unsupported on this platform"))
        }
        unsupported()?
    };
    if let Some(files) = result.get_mut("files").and_then(Value::as_array_mut) {
        for (f, size) in files.iter_mut().zip(sizes) {
            f["bytes"] = json!(size);
        }
    }
    result["delivered"] = json!(true);
    Ok(result)
}

fn check_destination(path: &Path) -> Result<()> {
    if path.file_name().and_then(|n| n.to_str()).is_none_or(|n| n.is_empty() || n == "." || n == "..") {
        return Err(bad(ID, "destination needs a file name"));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(bad(ID, format!("destination already exists: {}", path.display()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(EngineError::Io(err.to_string())),
        }
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if !std::fs::metadata(parent).is_ok_and(|m| m.is_dir() && !m.permissions().readonly()) {
            return Err(bad(ID, "destination directory must exist and be writable"));
        }
    }
    Ok(())
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod native {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Owned {
        path: PathBuf,
        metadata: std::fs::Metadata,
    }

    impl Owned {
        fn verify(&self) -> std::io::Result<()> {
            use std::os::unix::fs::MetadataExt;
            let current = std::fs::symlink_metadata(&self.path)?;
            if current.dev() == self.metadata.dev() && current.ino() == self.metadata.ino() {
                Ok(())
            } else {
                Err(std::io::Error::other("file identity changed; preserved"))
            }
        }
        fn remove(&self) -> std::io::Result<()> {
            let current = match std::fs::symlink_metadata(&self.path) {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if current.dev() == self.metadata.dev() && current.ino() == self.metadata.ino() {
                    return std::fs::remove_file(&self.path);
                }
            }
            #[cfg(not(unix))]
            let _ = (&current, &self.metadata);
            // No portable identity proof: preserve replaced/unverifiable paths, report them.
            Err(std::io::Error::other("output identity changed or cannot be verified; preserved"))
        }
    }

    fn stage(path: &Path, bytes: &[u8]) -> Result<Owned> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        for _ in 0..64 {
            let tmp = parent.join(format!(".soundcraft-bounce-{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(f) => f,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(EngineError::Io(format!("{}: {err}", tmp.display()))),
            };
            let metadata = file
                .metadata()
                .map_err(|err| EngineError::Io(format!("cannot establish staging identity; retained path {}: {err}", tmp.display())))?;
            let owned = Owned { path: tmp, metadata };
            if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                let cleanup = owned.remove().err().map_or(String::new(), |err| format!("; staging preserved at {}: {err}", owned.path.display()));
                return Err(EngineError::Io(format!("write failed: {err}{cleanup}")));
            }
            return Ok(owned);
        }
        Err(EngineError::Io("cannot reserve a unique staging file".into()))
    }

    pub(super) fn deliver(e: &Engine, p: &Plan) -> Result<Vec<usize>> {
        let mut staging = Vec::new();
        let mut published = Vec::new();
        let mut sizes = Vec::new();
        let result: Result<()> = (|| {
            for f in &p.files {
                let bytes = encoded(e, f, &p.opts)?;
                sizes.push(bytes.len());
                staging.push(stage(&f.path, &bytes)?);
            }
            publish(&staging, &p.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), &mut published)?;
            Ok(())
        })();
        finish(&staging, &published, result)?;
        Ok(sizes)
    }

    fn publish(staging: &[Owned], paths: &[PathBuf], published: &mut Vec<Owned>) -> Result<()> {
        for (path, tmp) in paths.iter().zip(staging) {
            tmp.verify().map_err(|err| EngineError::Io(format!("staging identity unavailable; preserved {}: {err}", tmp.path.display())))?;
            // Same directory/filesystem; exclusive hard links never replace a destination.
            std::fs::hard_link(&tmp.path, path)
                .map_err(|err| EngineError::Io(format!("cannot publish {} without overwrite: {err}", path.display())))?;
            published.push(Owned { path: path.clone(), metadata: tmp.metadata.clone() });
            // The source path can change between verification and link. Verify the final too;
            // an unverified final remains tracked for retained-path reporting, never blind unlink.
            published
                .last()
                .ok_or_else(|| EngineError::Io("missing publication ownership".into()))?
                .verify()
                .map_err(|err| EngineError::Io(format!("published identity unavailable; preserved {}: {err}", path.display())))?;
        }
        Ok(())
    }

    fn finish(staging: &[Owned], published: &[Owned], result: Result<()>) -> Result<()> {
        let mut residual = Vec::new();
        if result.is_err() {
            for f in published {
                if f.remove().is_err() {
                    residual.push(f.path.display().to_string());
                }
            }
        }
        for f in staging {
            if f.remove().is_err() {
                residual.push(f.path.display().to_string());
            }
        }
        if let Err(err) = result {
            return Err(EngineError::Io(format!("{err}; retained paths requiring cleanup: {}", residual.join(", "))));
        }
        if !residual.is_empty() {
            return Err(EngineError::Io(format!(
                "files delivered: {}; staging requiring cleanup: {}",
                published.iter().map(|f| f.path.display().to_string()).collect::<Vec<_>>().join(", "),
                residual.join(", ")
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn dir() -> PathBuf {
            let p = std::env::temp_dir().join(format!("sc-bounce-publish-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&p).unwrap();
            p
        }
        #[test]
        fn late_collision_rolls_back_only_created_paths() {
            let dir = dir();
            let a = dir.join("a.wav");
            let b = dir.join("b.wav");
            let staged = vec![stage(&a, b"first").unwrap(), stage(&b, b"second").unwrap()];
            std::fs::write(&b, b"external").unwrap();
            let mut published = Vec::new();
            let result = publish(&staged, &[a.clone(), b.clone()], &mut published);
            assert!(finish(&staged, &published, result).is_err());
            assert!(!a.exists());
            assert_eq!(std::fs::read(&b).unwrap(), b"external");
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            std::fs::remove_dir_all(dir).unwrap();
        }
        #[test]
        fn replaced_staging_and_final_are_preserved_and_reported() {
            let dir = dir();
            let path = dir.join("output.wav");
            let staged = vec![stage(&path, b"ours").unwrap()];
            let mut published = Vec::new();
            publish(&staged, std::slice::from_ref(&path), &mut published).unwrap();
            // Rename replacements over live owned inodes, so inode reuse cannot hide the change.
            let replacement = dir.join("replacement");
            std::fs::write(&replacement, b"external-final").unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            std::fs::write(&replacement, b"external-stage").unwrap();
            std::fs::rename(&replacement, &staged[0].path).unwrap();
            assert!(published[0].verify().is_err(), "a replacement must fail post-link identity verification");
            let err = finish(&staged, &published, Err(EngineError::Io("synthetic late failure".into()))).unwrap_err().to_string();
            assert!(err.contains(&path.display().to_string()));
            assert!(err.contains(&staged[0].path.display().to_string()));
            assert_eq!(std::fs::read(&path).unwrap(), b"external-final");
            assert_eq!(std::fs::read(&staged[0].path).unwrap(), b"external-stage");
            let mut outputs = Vec::new();
            assert!(publish(&staged, &[dir.join("other.wav")], &mut outputs).is_err());
            assert!(!dir.join("other.wav").exists());
            std::fs::remove_dir_all(dir).unwrap();
        }
        #[test]
        fn publication_link_failure_cleans_owned_staging() {
            let dir = dir();
            let path = dir.join("output.wav");
            let staged = vec![stage(&path, b"ours").unwrap()];
            let mut published = Vec::new();
            let result = publish(&staged, &[dir.join("missing/output.wav")], &mut published);
            assert!(finish(&staged, &published, result).is_err());
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
            std::fs::remove_dir(dir).unwrap();
        }
    }
}
