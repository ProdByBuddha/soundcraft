//! Bounded immutable capture/placement plans. No media is rendered or rewritten by capture.
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};
use soundcraft_model::{
    ChannelFormat, Clip, ClipContent, ClipDefinition, ClipDefinitionId, ClipId, ClipMetadata, ClipTemplate, DefinitionTimebase, ElasticOriginal,
    Session, SourceId, TrackId, TrackKind,
};
use soundcraft_time::{Range, Samples};
use std::collections::BTreeSet;

const MAX_CLIPS: usize = 100_000;
const MAX_POINTS: usize = 1_000_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_METADATA: usize = 65_536;

fn bad(message: impl Into<String>) -> EngineError {
    cmd::bad("reusable clip", message)
}
fn position(value: Samples) -> Result<Samples> {
    if (0..=cmd::MAX_POSITION).contains(&value) { Ok(value) } else { Err(bad("position is outside the supported timeline")) }
}
fn add(a: Samples, b: Samples) -> Result<Samples> {
    position(a.checked_add(b).ok_or_else(|| bad("position overflow"))?)
}
fn subtract(a: Samples, b: Samples) -> Result<Samples> {
    a.checked_sub(b).ok_or_else(|| bad("coordinate overflow"))
}

#[derive(Default)]
struct Budget {
    clips: usize,
    points: usize,
    bytes: usize,
    metadata: usize,
}
impl Budget {
    fn bytes(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(|| bad("metadata size overflow"))?;
        if self.bytes > MAX_BYTES { Err(bad("reusable clip planning exceeds 128 MiB")) } else { Ok(()) }
    }
    fn sequence(&mut self, sequence: &soundcraft_midi::Sequence) -> Result<()> {
        let points = sequence.notes.len().checked_add(sequence.ctrls.len()).ok_or_else(|| bad("event count overflow"))?;
        self.points = self.points.checked_add(points).ok_or_else(|| bad("event count overflow"))?;
        if self.points > MAX_POINTS {
            return Err(bad("reusable clip planning exceeds 1000000 points"));
        }
        self.bytes(points.checked_mul(64).ok_or_else(|| bad("event size overflow"))?)
    }
    fn clip(&mut self, clip: &Clip) -> Result<()> {
        self.clips = self.clips.checked_add(1).ok_or_else(|| bad("clip count overflow"))?;
        self.points =
            self.points.checked_add(clip.gain_env.len()).and_then(|count| count.checked_add(2)).ok_or_else(|| bad("point count overflow"))?;
        if self.clips > MAX_CLIPS || self.points > MAX_POINTS {
            return Err(bad("reusable clip planning inventory limit exceeded"));
        }
        self.bytes(
            clip.name
                .len()
                .checked_add(512)
                .and_then(|bytes| bytes.checked_add(clip.gain_env.len().checked_mul(16)?))
                .ok_or_else(|| bad("clip size overflow"))?,
        )?;
        if let ClipContent::Midi { sequence } = &clip.content {
            self.sequence(sequence)?;
        }
        Ok(())
    }
    fn template(&mut self, template: &ClipTemplate, name: &str) -> Result<()> {
        self.clips = self.clips.checked_add(1).ok_or_else(|| bad("clip count overflow"))?;
        self.points =
            self.points.checked_add(template.gain_env.len()).and_then(|count| count.checked_add(2)).ok_or_else(|| bad("point count overflow"))?;
        if self.clips > MAX_CLIPS || self.points > MAX_POINTS {
            return Err(bad("reusable clip planning inventory limit exceeded"));
        }
        self.bytes(
            name.len()
                .checked_add(512)
                .and_then(|bytes| bytes.checked_add(template.gain_env.len().checked_mul(16)?))
                .ok_or_else(|| bad("clip size overflow"))?,
        )?;
        if let ClipContent::Midi { sequence } = &template.content {
            self.sequence(sequence)?;
        }
        Ok(())
    }
    fn metadata(&mut self, metadata: &ClipMetadata) -> Result<()> {
        self.metadata = self
            .metadata
            .checked_add(metadata.values.len())
            .and_then(|count| count.checked_add(metadata.flags.len()))
            .ok_or_else(|| bad("metadata count overflow"))?;
        if self.metadata > MAX_METADATA {
            return Err(bad("reusable metadata limit exceeded"));
        }
        for key in metadata.values.keys().chain(metadata.flags.iter()) {
            self.bytes(key.len().saturating_add(64))?;
        }
        Ok(())
    }
}

struct Ids {
    next: u64,
}
impl Ids {
    fn take(&mut self) -> Result<u64> {
        let id = self.next;
        self.next = id.checked_add(1).ok_or_else(|| bad("session identity space exhausted"))?;
        Ok(id)
    }
}

fn inventory(session: &Session) -> Result<(Ids, Budget)> {
    validate_tempo(session)?;
    if session.tracks.len() > 4096 || session.clip_definitions.len() > MAX_CLIPS {
        return Err(bad("document inventory limit exceeded"));
    }
    let mut ids = BTreeSet::new();
    let mut max_id = 0;
    let mut budget = Budget::default();
    for id in session
        .tracks
        .iter()
        .map(|track| track.id.0)
        .chain(session.all_clips().map(|clip| clip.id.0))
        .chain(session.sources.iter().map(|source| source.id.0))
        .chain(session.videos.iter().map(|source| source.id.0))
        .chain(session.busses.iter().map(|bus| bus.id.0))
        .chain(session.groups.iter().map(|group| group.id.0))
        .chain(session.markers.iter().map(|marker| marker.id.0))
        .chain(session.clip_definitions.iter().map(|definition| definition.id.0))
    {
        if ids.len() >= MAX_CLIPS || id == 0 || !ids.insert(id) {
            return Err(bad("ambiguous or excessive session identities"));
        }
        max_id = max_id.max(id);
    }
    for clip in session.all_clips() {
        budget.clip(clip)?;
    }
    for group in session.all_clips().filter_map(|clip| clip.group) {
        max_id = max_id.max(group);
    }
    for original in session.midi_originals.values() {
        budget.sequence(original)?;
    }
    for definition in &session.clip_definitions {
        definition.validate().map_err(bad)?;
        budget.template(&definition.template, &definition.name)?;
        budget.metadata(&definition.metadata)?;
        if let Some(original) = &definition.midi_original {
            budget.sequence(original)?;
        }
    }
    if session.edit.values.len().saturating_add(session.edit.flags.len()) > MAX_METADATA {
        return Err(bad("document edit metadata limit exceeded"));
    }
    budget.metadata = budget
        .metadata
        .checked_add(session.edit.values.len())
        .and_then(|count| count.checked_add(session.edit.flags.len()))
        .ok_or_else(|| bad("metadata count overflow"))?;
    if budget.metadata > MAX_METADATA {
        return Err(bad("document and reusable metadata limit exceeded"));
    }
    for key in session.edit.values.keys().chain(session.edit.flags.iter()) {
        budget.bytes(key.len().saturating_add(64))?;
    }
    for key in session.edit.values.keys().chain(session.edit.flags.iter()) {
        if let Some(owner) = metadata_owner(key)? {
            max_id = max_id.max(owner);
        }
    }
    for owner in session.midi_originals.keys() {
        if *owner == 0 {
            return Err(bad("invalid saved performance owner"));
        }
        max_id = max_id.max(*owner);
    }
    let next = session.next_id.max(max_id.checked_add(1).ok_or_else(|| bad("session identity space exhausted"))?).max(1);
    if next == u64::MAX {
        return Err(bad("session identity space exhausted"));
    }
    Ok((Ids { next }, budget))
}

fn validate_tempo(session: &Session) -> Result<()> {
    let tempos = session.tempo.tempos();
    let meters = session.tempo.meters();
    if !(8_000..=768_000).contains(&session.sample_rate.hz())
        || tempos.len().saturating_add(meters.len()) > MAX_METADATA
        || tempos.first().is_none_or(|event| event.tick != 0)
        || meters.first().is_none_or(|event| event.tick != 0)
        || tempos.iter().any(|event| !(0..=cmd::MAX_POSITION).contains(&event.tick) || !event.bpm.is_finite() || !(5.0..=1000.0).contains(&event.bpm))
        || meters.iter().any(|event| {
            !(0..=cmd::MAX_POSITION).contains(&event.tick)
                || !(1..=64).contains(&event.numerator)
                || ![1, 2, 4, 8, 16, 32, 64].contains(&event.denominator)
        })
        || tempos.windows(2).any(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a.tick >= b.tick))
        || meters.windows(2).any(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a.tick >= b.tick))
    {
        return Err(bad("invalid or excessive tempo map"));
    }
    Ok(())
}

fn supplied_position(engine: &Engine, command: &str, params: &Value, key: &str) -> Result<Option<Samples>> {
    if params.get(key).and_then(Value::as_str).is_some_and(|text| text.len() > 256) {
        return Err(bad("position text exceeds 256 bytes"));
    }
    cmd::position_param_raw(engine, command, params, key)?.map(position).transpose()
}

fn validate_clip(session: &Session, clip: &Clip, require_loaded: bool) -> Result<()> {
    position(clip.start)?;
    add(clip.start, clip.length)?;
    if clip.length <= 0
        || !clip.gain_db.is_finite()
        || !clip.stretch.is_finite()
        || clip.stretch <= 0.0
        || clip.gain_env.len() > MAX_POINTS
        || clip.gain_env.iter().any(|&(at, value)| !(0..=cmd::MAX_POSITION).contains(&at) || !value.is_finite())
        || clip.gain_env.windows(2).any(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a.0 >= b.0))
        || !(0..=cmd::MAX_POSITION).contains(&clip.fade_in.len)
        || !(0..=cmd::MAX_POSITION).contains(&clip.fade_out.len)
    {
        return Err(bad("clip has invalid timing, gain or stretch"));
    }
    if let Some(window) = clip.slice {
        if !window.audio_phase_samples.is_finite() || !(0.0..1.0).contains(&window.audio_phase_samples) {
            return Err(bad("invalid audio source phase"));
        }
        for coordinate in [window.audio_origin_samples, window.fade_in_elapsed_samples, window.fade_out_remaining_samples] {
            if !(-cmd::MAX_POSITION..=cmd::MAX_POSITION).contains(&coordinate)
                || coordinate.checked_add(clip.length).is_none_or(|end| !(-cmd::MAX_POSITION..=cmd::MAX_POSITION).contains(&end))
            {
                return Err(bad("clip slice coordinate overflow"));
            }
        }
    }
    match &clip.content {
        ClipContent::Audio { source, offset } => {
            let media = session.source(*source).ok_or_else(|| bad("audio source metadata is unavailable"))?;
            if *offset < 0
                || media.channels == 0
                || media.sample_rate != session.sample_rate.hz()
                || media.frames == 0
                || media.frames > cmd::MAX_POSITION as u64
            {
                return Err(bad("audio source geometry is invalid"));
            }
            let first = clip.source_position(0);
            let last = clip.source_position(clip.length - 1);
            if !first.is_finite()
                || !last.is_finite()
                || first < 0.0
                || last < first
                || last >= media.frames as f64
                || last > cmd::MAX_POSITION as f64
            {
                return Err(bad("clip window exceeds its audio source"));
            }
            if let Some(audio) = session.pool.get(*source) {
                if audio.buffer.sample_rate != media.sample_rate
                    || audio.buffer.channels.len() != usize::from(media.channels)
                    || audio.buffer.channels.iter().any(|channel| channel.len() as u64 != media.frames)
                {
                    return Err(bad("decoded audio shape differs from source metadata"));
                }
            } else if require_loaded {
                return Err(bad("decoded audio is unavailable; restore its media before placement"));
            }
        }
        ClipContent::Video { source, offset } => {
            let video = session.video(*source).ok_or_else(|| bad("movie source metadata is unavailable"))?;
            if *offset < 0 || !video.duration.is_finite() || video.duration < 0.0 {
                return Err(bad("invalid movie metadata"));
            }
            let extent = video.duration * session.sample_rate.hz() as f64;
            if extent > 0.0 && clip.source_position(clip.length - 1) >= extent {
                return Err(bad("clip window exceeds its movie source"));
            }
        }
        ClipContent::Midi { sequence } => {
            validate_sequence(sequence)?;
        }
    }
    Ok(())
}

fn validate_sequence(sequence: &soundcraft_midi::Sequence) -> Result<()> {
    use soundcraft_midi::CtrlKind;
    if sequence.notes.len().saturating_add(sequence.ctrls.len()) > MAX_POINTS
        || sequence.notes.iter().any(|note| {
            note.start < 0
                || note.length <= 0
                || note.start.checked_add(note.length).is_none_or(|end| end > cmd::MAX_POSITION)
                || note.pitch > 127
                || note.channel > 15
                || !(1..=127).contains(&note.velocity)
                || note.release_velocity > 127
        })
        || sequence.ctrls.iter().any(|event| {
            event.tick < 0
                || event.tick > cmd::MAX_POSITION
                || event.channel > 15
                || match event.kind {
                    CtrlKind::Cc(number) => number > 127 || !(0..=127).contains(&event.value),
                    CtrlKind::PolyPressure(pitch) => pitch > 127 || !(0..=127).contains(&event.value),
                    CtrlKind::PitchBend => !(-8192..=8191).contains(&event.value),
                    _ => !(0..=127).contains(&event.value),
                }
        })
    {
        return Err(bad("invalid MIDI sequence"));
    }
    Ok(())
}

fn midi_cuts(session: &Session, clip: &Clip, range: Range) -> Result<(i64, i64)> {
    let base = session.tempo.samples_to_ticks(clip.start, session.sample_rate);
    let origin = session.tempo.tick_to_samples(base, session.sample_rate);
    let lo_sample = add(origin, subtract(range.start, clip.start)?)?;
    let hi_sample = add(origin, subtract(range.end, clip.start)?)?;
    let lo = subtract(session.tempo.samples_to_ticks(lo_sample, session.sample_rate), base)?;
    let hi = subtract(session.tempo.samples_to_ticks(hi_sample, session.sample_rate), base)?;
    if lo < 0 || hi <= lo || hi > cmd::MAX_POSITION {
        return Err(bad("MIDI selection has no supported positive tick span"));
    }
    Ok((lo, hi))
}

fn crop_sequence(sequence: &soundcraft_midi::Sequence, lo: i64, hi: i64) -> Result<soundcraft_midi::Sequence> {
    validate_sequence(sequence)?;
    let mut output = soundcraft_midi::Sequence::default();
    for note in &sequence.notes {
        let end = note.start.checked_add(note.length).ok_or_else(|| bad("MIDI note end overflow"))?;
        if note.start < hi && end > lo {
            let mut cropped = *note;
            let start = note.start.max(lo);
            cropped.start = subtract(start, lo)?;
            cropped.length = subtract(end.min(hi), start)?;
            output.notes.push(cropped);
        }
    }
    for event in &sequence.ctrls {
        if event.tick >= lo && event.tick < hi {
            let mut cropped = *event;
            cropped.tick = subtract(event.tick, lo)?;
            output.ctrls.push(cropped);
        }
    }
    output.sort();
    Ok(output)
}

fn slice_clip(session: &Session, clip: &Clip, range: Range) -> Result<(Clip, Option<(i64, i64)>)> {
    validate_clip(session, clip, false)?;
    if range.start < clip.start || range.end > clip.end() || range.is_empty() {
        return Err(bad("slice must be inside the clip"));
    }
    let delta = subtract(range.start, clip.start)?;
    let length = subtract(range.end, range.start)?;
    let mut output = clip.clone();
    let mut window = clip.slice.unwrap_or_default();
    if matches!(clip.content, ClipContent::Audio { .. }) {
        window.audio_origin_samples = window.audio_origin_samples.checked_add(delta).ok_or_else(|| bad("source origin overflow"))?;
    } else if let ClipContent::Video { offset, .. } = &mut output.content {
        *offset = offset.checked_add(delta).ok_or_else(|| bad("movie offset overflow"))?;
    }
    window.fade_in_elapsed_samples = window.fade_in_elapsed_samples.checked_add(delta).ok_or_else(|| bad("fade origin overflow"))?;
    window.fade_out_remaining_samples = window
        .fade_out_remaining_samples
        .checked_add(subtract(clip.length, delta.checked_add(length).ok_or_else(|| bad("slice end overflow"))?)?)
        .ok_or_else(|| bad("fade tail overflow"))?;
    output.slice = Some(window);
    output.gain_env = clip.sliced_gain_env(delta, length);
    output.start = range.start;
    output.length = length;
    output.sync_point = clip.sync_point.saturating_sub(delta).clamp(0, length);
    let cuts = if let ClipContent::Midi { sequence } = &mut output.content {
        let (lo, hi) = midi_cuts(session, clip, range)?;
        *sequence = crop_sequence(sequence, lo, hi)?;
        Some((lo, hi))
    } else {
        None
    };
    validate_clip(session, &output, false)?;
    Ok((output, cuts))
}

fn normalized_key(key: &str, id: ClipId) -> Option<String> {
    for family in ["clip_fx", "elastic"] {
        if let Some(suffix) = key.strip_prefix(&format!("{family}.{}.", id.0)) {
            return Some(format!("{family}.{{id}}.{suffix}"));
        }
    }
    for family in ["clip.bpm", "clip.len0", "clip.pitch", "clip_fx.bypass"] {
        if key == format!("{family}.{}", id.0) {
            return Some(format!("{family}.{{id}}"));
        }
    }
    None
}

fn metadata_owner(key: &str) -> Result<Option<u64>> {
    let owner = ["clip.bpm.", "clip.len0.", "clip.pitch.", "clip_fx.bypass."].iter().find_map(|prefix| key.strip_prefix(prefix)).or_else(|| {
        ["clip_fx.", "elastic."].iter().find_map(|prefix| key.strip_prefix(prefix).and_then(|rest| rest.split_once('.').map(|(owner, _)| owner)))
    });
    if owner.is_some() && key.len() > 256 {
        return Err(bad("clip metadata key exceeds 256 bytes"));
    }
    owner.map(|owner| owner.parse::<u64>().ok().filter(|owner| *owner != 0).ok_or_else(|| bad("invalid clip metadata owner"))).transpose()
}

fn metadata(session: &Session, clip: &Clip, selected: Range) -> Result<ClipMetadata> {
    let mut output = ClipMetadata::default();
    let elastic = |suffix: &str| session.edit.values.get(&format!("elastic.{}.{suffix}", clip.id.0)).copied();
    match (elastic("src"), elastic("off"), elastic("len")) {
        (None, None, None) => {}
        (Some(source), Some(offset), Some(length))
            if source.is_finite()
                && source.fract() == 0.0
                && source >= 1.0
                && source < (1u64 << 53) as f64
                && offset.is_finite()
                && offset.fract() == 0.0
                && (0.0..=cmd::MAX_POSITION as f64).contains(&offset)
                && length.is_finite()
                && length.fract() == 0.0
                && (1.0..=cmd::MAX_POSITION as f64).contains(&length) =>
        {
            let source = SourceId(source as u64);
            if session.source(source).is_none() {
                return Err(bad("unwarped source metadata is unavailable"));
            }
            let ratio = elastic("ratio").unwrap_or(1.0);
            let phase = elastic("phase").unwrap_or(0.0);
            let anchor = elastic("anchor").unwrap_or(0.0);
            if !ratio.is_finite()
                || ratio <= 0.0
                || !phase.is_finite()
                || !(0.0..1.0).contains(&phase)
                || !anchor.is_finite()
                || !(0.0..=cmd::MAX_POSITION as f64).contains(&anchor)
            {
                return Err(bad("invalid elastic source mapping"));
            }
            let delta = subtract(selected.start, clip.start)?;
            let rendered_origin = clip.source_position(delta);
            let cursor = offset + phase + (rendered_origin - anchor) / ratio;
            let selected_length = (selected.len() as f64 / (ratio * clip.stretch)).round();
            if !cursor.is_finite()
                || !(0.0..=cmd::MAX_POSITION as f64).contains(&cursor)
                || !selected_length.is_finite()
                || !(1.0..=cmd::MAX_POSITION as f64).contains(&selected_length)
            {
                return Err(bad("selected elastic source window is unsupported"));
            }
            let floor = cursor.floor();
            if session.source(source).is_none_or(|media| cursor + selected_length - 1.0 >= media.frames as f64) {
                return Err(bad("selected elastic window exceeds the original source"));
            }
            output.elastic_original = Some(ElasticOriginal {
                source,
                offset: floor as i64,
                length: selected_length as i64,
                phase: cursor - floor,
                rendered_origin_samples: rendered_origin,
            });
        }
        _ => return Err(bad("invalid elastic original metadata")),
    }
    for family in ["clip_fx", "elastic"] {
        let prefix = format!("{family}.{}.", clip.id.0);
        for (key, value) in session.edit.values.range(prefix.clone()..) {
            if !key.starts_with(&prefix) {
                break;
            }
            if let Some(key) = normalized_key(key, clip.id) {
                if ["elastic.{id}.src", "elastic.{id}.off", "elastic.{id}.len", "elastic.{id}.phase", "elastic.{id}.anchor"].contains(&key.as_str()) {
                    continue;
                }
                output.values.insert(key, *value);
            }
        }
        for key in session.edit.flags.range(prefix.clone()..) {
            if !key.starts_with(&prefix) {
                break;
            }
            if let Some(key) = normalized_key(key, clip.id) {
                output.flags.insert(key);
            }
        }
    }
    for family in ["clip.bpm", "clip.len0", "clip.pitch", "clip_fx.bypass"] {
        let key = format!("{family}.{}", clip.id.0);
        if let Some(value) = session.edit.values.get(&key) {
            output.values.insert(format!("{family}.{{id}}"), *value);
        }
        if session.edit.flags.contains(&key) {
            output.flags.insert(format!("{family}.{{id}}"));
        }
    }
    Ok(output)
}

fn instance_metadata(session: &mut Session, id: ClipId, metadata: &ClipMetadata) {
    for (key, value) in &metadata.values {
        session.edit.values.insert(key.replace("{id}", &id.0.to_string()), *value);
    }
    for key in &metadata.flags {
        session.edit.flags.insert(key.replace("{id}", &id.0.to_string()));
    }
    if let Some(original) = &metadata.elastic_original {
        for (suffix, value) in [
            ("src", original.source.0 as f64),
            ("off", original.offset as f64),
            ("len", original.length as f64),
            ("phase", original.phase),
            ("anchor", original.rendered_origin_samples),
        ] {
            session.edit.values.insert(format!("elastic.{}.{suffix}", id.0), value);
        }
    }
}

pub(crate) fn capture(engine: &mut Engine, params: &Value) -> Result<Value> {
    let session = engine.session();
    let (mut ids, mut budget) = inventory(session)?;
    let name = match params.get("name") {
        Some(Value::String(name)) if !name.is_empty() && name.len() <= 4096 && !name.contains('\0') => name.clone(),
        Some(_) => return Err(bad("name must be a nonempty string of at most 4096 bytes")),
        None => "Captured".into(),
    };
    let start = supplied_position(engine, "clip.capture", params, "start")?;
    let end = supplied_position(engine, "clip.capture", params, "end")?;
    let range = match (start, end) {
        (Some(start), Some(end)) if end > start => Some(Range { start, end }),
        (None, None) if !session.edit.selection.is_empty() => {
            position(session.edit.selection.start)?;
            position(session.edit.selection.end)?;
            Some(session.edit.selection)
        }
        (None, None) => None,
        _ => return Err(bad("pass a positive start/end range")),
    };
    if params.get("clips").is_some() && params.get("clip").is_some() {
        return Err(bad("pass clip or clips, not both"));
    }
    if (params.get("clips").is_some() || params.get("clip").is_some()) && (params.get("tracks").is_some() || params.get("track").is_some()) {
        return Err(bad("pass clip targets or track targets, not both"));
    }
    let selected: Vec<ClipId> = if let Some(value) = params.get("clips") {
        let array = value.as_array().ok_or_else(|| bad("clips must be an array of ids"))?;
        if array.is_empty() || array.len() > MAX_CLIPS {
            return Err(bad("pass a nonempty bounded clip selection"));
        }
        array.iter().map(|value| value.as_u64().filter(|id| *id != 0).map(ClipId).ok_or_else(|| bad("invalid clip id"))).collect::<Result<_>>()?
    } else if let Some(value) = params.get("clip") {
        vec![ClipId(value.as_u64().filter(|id| *id != 0).ok_or_else(|| bad("invalid clip id"))?)]
    } else if params.get("tracks").is_none() && params.get("track").is_none() {
        session.edit.selected_clips.clone()
    } else {
        Vec::new()
    };
    let mut seen = BTreeSet::new();
    for id in &selected {
        if !seen.insert(*id) || session.find_clip(*id).is_none() {
            return Err(bad("duplicate or unknown selected clip"));
        }
    }
    if selected.is_empty() && params.get("tracks").is_none() && params.get("track").is_none() {
        for track in &session.edit.selected_tracks {
            if session.track(*track).is_none() {
                return Err(bad("unknown selected track"));
            }
        }
    }
    let tracks = if selected.is_empty() { cmd::tracks_required(engine, "clip.capture", params)? } else { Vec::new() };
    let targets: Vec<_> = session
        .tracks
        .iter()
        .flat_map(|track| track.clips().iter().map(move |clip| (track, clip)))
        .filter(|(track, clip)| if selected.is_empty() { tracks.contains(&track.id) } else { seen.contains(&clip.id) })
        .collect();
    let mut definitions = Vec::new();
    let mut response = Vec::new();
    for (track, clip) in targets {
        let selected_range = match range {
            Some(range) => match clip.range().intersect(&range) {
                Some(range) => range,
                None => continue,
            },
            None => clip.range(),
        };
        budget.clip(clip)?;
        let (part, cuts) = slice_clip(session, clip, selected_range)?;
        let metadata = metadata(session, clip, selected_range)?;
        budget.metadata(&metadata)?;
        let original = match (session.midi_originals.get(&clip.id.0), cuts) {
            (Some(original), Some((lo, hi))) => {
                budget.sequence(original)?;
                Some(crop_sequence(original, lo, hi)?)
            }
            (None, _) => None,
            _ => return Err(bad("saved MIDI performance belongs to a non-MIDI clip")),
        };
        let id = ClipDefinitionId(ids.take()?);
        let timebase = cuts.map_or(DefinitionTimebase::Samples, |(lo, hi)| DefinitionTimebase::MidiTicks { span: hi - lo });
        let definition = ClipDefinition { id, name: name.clone(), template: ClipTemplate::from(&part), timebase, midi_original: original, metadata };
        definition.validate().map_err(bad)?;
        let kind = match part.content {
            ClipContent::Audio { .. } => "audio",
            ClipContent::Midi { .. } => "midi",
            ClipContent::Video { .. } => "video",
        };
        response.push(
            json!({"id": id, "kind": kind, "track": track.id, "start": selected_range.start, "end": selected_range.end, "length": part.length}),
        );
        definitions.push(definition);
    }
    if definitions.is_empty() {
        return Err(bad("selection contains no clips"));
    }
    if session.clip_definitions.len().checked_add(definitions.len()).is_none_or(|count| count > MAX_CLIPS) {
        return Err(bad("too many reusable definitions"));
    }
    let count = definitions.len();
    let session = engine.session_mut();
    session.clip_definitions.extend(definitions);
    session.next_id = ids.next;
    Ok(json!({"definitions": response, "count": count}))
}

pub(crate) fn place(engine: &mut Engine, params: &Value) -> Result<Value> {
    let session = engine.session();
    let (mut ids, mut budget) = inventory(session)?;
    let id = ClipDefinitionId(params.get("definition").and_then(Value::as_u64).ok_or_else(|| bad("definition id required"))?);
    let definition = session.clip_definition(id).ok_or_else(|| bad("unknown reusable definition"))?;
    let at = supplied_position(engine, "clip.place_definition", params, "at")?.unwrap_or(session.edit.selection.start);
    position(at)?;
    let track = cmd::track_param(engine, "clip.place_definition", params, "track")?;
    let kind = match definition.template.content {
        ClipContent::Audio { .. } => TrackKind::Audio,
        ClipContent::Midi { .. } => TrackKind::Midi,
        ClipContent::Video { .. } => TrackKind::Video,
    };
    let format = match definition.template.content {
        ClipContent::Audio { source, .. } => {
            ChannelFormat::for_channels(usize::from(session.source(source).ok_or_else(|| bad("audio source metadata unavailable"))?.channels))
        }
        _ => ChannelFormat::Mono,
    };
    if let Some(track) = track {
        let target = session.track(track).ok_or_else(|| bad("unknown destination track"))?;
        if target.kind != kind && !(kind == TrackKind::Midi && target.kind == TrackKind::Instrument) {
            return Err(bad("definition and destination track kinds disagree"));
        }
        if target.playlist().is_none() {
            return Err(bad("destination track has no active playlist"));
        }
    }
    if track.is_none() && session.tracks.len() >= 4096 {
        return Err(bad("new destination exceeds the track limit"));
    }
    let new_track_id = if track.is_none() { Some(TrackId(ids.take()?)) } else { None };
    let clip_id = ClipId(ids.take()?);
    budget.template(&definition.template, &definition.name)?;
    budget.metadata(&definition.metadata)?;
    if let Some(original) = &definition.midi_original {
        budget.sequence(original)?;
    }
    let mut clip = definition.template.instantiate(clip_id, &definition.name, at);
    clip.definition = Some(id);
    if let DefinitionTimebase::MidiTicks { span } = definition.timebase {
        let tick = session.tempo.samples_to_ticks(at, session.sample_rate);
        let end_tick = tick.checked_add(span).ok_or_else(|| bad("MIDI destination tick overflow"))?;
        let end = session.tempo.tick_to_samples(end_tick, session.sample_rate);
        clip.length = subtract(end, session.tempo.tick_to_samples(tick, session.sample_rate))?;
        if clip.length <= 0 {
            return Err(bad("MIDI destination has no positive sample span"));
        }
        clip.sync_point = clip.sync_point.clamp(0, clip.length);
    }
    validate_clip(session, &clip, true)?;
    let mut survivors = Vec::new();
    let mut remapped = Vec::new();
    let mut removed = BTreeSet::new();
    if let Some(track) = track {
        for existing in session.track(track).ok_or_else(|| bad("unknown destination track"))?.clips() {
            if !existing.range().overlaps(&clip.range()) {
                survivors.push(existing.clone());
                continue;
            }
            if existing.edit_locked || existing.time_locked {
                return Err(bad("destination contains a locked clip"));
            }
            removed.insert(existing.id);
            let mut ranges = Vec::new();
            if existing.start < clip.start {
                ranges.push(Range { start: existing.start, end: clip.start });
            }
            if existing.end() > clip.end() {
                ranges.push(Range { start: clip.end(), end: existing.end() });
            }
            for (index, range) in ranges.into_iter().enumerate() {
                budget.clip(existing)?;
                let (mut survivor, cuts) = slice_clip(session, existing, range)?;
                survivor.id = if index == 0 && range.start == existing.start { existing.id } else { ClipId(ids.take()?) };
                let metadata = metadata(session, existing, range)?;
                budget.metadata(&metadata)?;
                let original = match (session.midi_originals.get(&existing.id.0), cuts) {
                    (Some(original), Some((lo, hi))) => {
                        budget.sequence(original)?;
                        Some(crop_sequence(original, lo, hi)?)
                    }
                    _ => None,
                };
                remapped.push((survivor.id, metadata, original));
                survivors.push(survivor);
            }
        }
    }
    let definition_metadata = definition.metadata.clone();
    let definition_original = definition.midi_original.clone();
    let name = definition.name.clone();
    let length = clip.length;
    let destination = track.or(new_track_id).ok_or_else(|| bad("destination plan is missing"))?;
    let session = engine.session_mut();
    if let Some(id) = new_track_id {
        // add_track's allocation is confined to this fully admitted commit; reserve the exact id.
        session.next_id = id.0;
        session.add_track(kind, format, Some(&name));
    }
    let track = destination;
    survivors.push(clip);
    if let Some(playlist) = session.track_mut(track).and_then(|track| track.playlist_mut()) {
        playlist.clips = survivors;
        playlist.sort();
    }
    session.edit.values.retain(|key, _| !metadata_owner(key).ok().flatten().is_some_and(|id| removed.contains(&ClipId(id))));
    session.edit.flags.retain(|key| !metadata_owner(key).ok().flatten().is_some_and(|id| removed.contains(&ClipId(id))));
    session.midi_originals.retain(|id, _| !removed.contains(&ClipId(*id)));
    for (id, metadata, original) in remapped {
        instance_metadata(session, id, &metadata);
        if let Some(original) = original {
            session.midi_originals.insert(id.0, original);
        }
    }
    instance_metadata(session, clip_id, &definition_metadata);
    if let Some(original) = definition_original {
        session.midi_originals.insert(clip_id.0, original);
    }
    session.edit.selected_clips.retain(|id| !removed.contains(id));
    session.next_id = ids.next;
    Ok(json!({"definition": id, "clip": clip_id, "track": track, "at": at, "length": length}))
}

pub(crate) fn clear(engine: &mut Engine, params: &Value) -> Result<Value> {
    let values = params.get("definitions").and_then(Value::as_array).ok_or_else(|| bad("definitions must be an array of ids"))?;
    if values.is_empty() || values.len() > MAX_CLIPS {
        return Err(bad("pass a nonempty bounded definition selection"));
    }
    let mut ids = BTreeSet::new();
    for value in values {
        let id = ClipDefinitionId(value.as_u64().ok_or_else(|| bad("invalid definition id"))?);
        if !ids.insert(id) || engine.session().clip_definition(id).is_none() {
            return Err(bad("duplicate or unknown definition id"));
        }
    }
    let session = engine.session_mut();
    session.clip_definitions.retain(|definition| !ids.contains(&definition.id));
    for playlist in session.tracks.iter_mut().flat_map(|track| &mut track.playlists) {
        for clip in &mut playlist.clips {
            if clip.definition.is_some_and(|id| ids.contains(&id)) {
                clip.definition = None;
            }
        }
    }
    Ok(json!({"definitions": ids.len()}))
}
