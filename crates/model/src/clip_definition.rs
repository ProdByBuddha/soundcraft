//! Detached reusable media; no playlist or timeline instance owns these definitions.

use crate::{Clip, ClipContent, ClipDefinitionId, ClipId, ClipSliceContext, Fade, SourceId};
use soundcraft_midi::Sequence;
use soundcraft_time::Samples;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DefinitionTimebase {
    Samples,
    MidiTicks { span: i64 },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ElasticOriginal {
    pub source: SourceId,
    pub offset: Samples,
    pub length: Samples,
    #[serde(default)]
    pub phase: f64,
    #[serde(default)]
    pub rendered_origin_samples: f64,
}

/// Keys are normalized templates such as `clip_fx.{id}.gain`, never an old owner id.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClipMetadata {
    #[serde(default)]
    pub values: BTreeMap<String, f64>,
    #[serde(default)]
    pub flags: BTreeSet<String>,
    #[serde(default)]
    pub elastic_original: Option<ElasticOriginal>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClipTemplate {
    pub content: ClipContent,
    pub length: Samples,
    pub gain_db: f32,
    pub gain_env: Vec<(Samples, f32)>,
    pub fade_in: Fade,
    pub fade_out: Fade,
    pub muted: bool,
    pub rating: u8,
    pub sync_point: Samples,
    pub color: Option<[u8; 3]>,
    pub stretch: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<ClipSliceContext>,
}

impl From<&Clip> for ClipTemplate {
    fn from(clip: &Clip) -> Self {
        Self {
            content: clip.content.clone(),
            length: clip.length,
            gain_db: clip.gain_db,
            gain_env: clip.gain_env.clone(),
            fade_in: clip.fade_in,
            fade_out: clip.fade_out,
            muted: clip.muted,
            rating: clip.rating,
            sync_point: clip.sync_point,
            color: clip.color,
            stretch: clip.stretch,
            slice: clip.slice,
        }
    }
}

impl ClipTemplate {
    pub fn instantiate(&self, id: ClipId, name: &str, at: Samples) -> Clip {
        let mut clip = Clip::audio(id, name, SourceId(0), 0, at, self.length);
        clip.content = self.content.clone();
        clip.gain_db = self.gain_db;
        clip.gain_env = self.gain_env.clone();
        clip.fade_in = self.fade_in;
        clip.fade_out = self.fade_out;
        clip.muted = self.muted;
        clip.rating = self.rating;
        clip.sync_point = self.sync_point;
        clip.color = self.color;
        clip.stretch = self.stretch;
        clip.slice = self.slice;
        clip
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClipDefinition {
    pub id: ClipDefinitionId,
    pub name: String,
    pub template: ClipTemplate,
    pub timebase: DefinitionTimebase,
    #[serde(default)]
    pub midi_original: Option<Sequence>,
    #[serde(default)]
    pub metadata: ClipMetadata,
}

impl ClipDefinition {
    /// Includes unwarped media retained for Remove Warp, independently of timeline instances.
    pub fn audio_sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        let source = match self.template.content {
            ClipContent::Audio { source, .. } => Some(source),
            _ => None,
        };
        source.into_iter().chain(self.metadata.elastic_original.as_ref().map(|original| original.source))
    }

    pub fn video_source(&self) -> Option<SourceId> {
        match self.template.content {
            ClipContent::Video { source, .. } => Some(source),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        const LIMIT: i64 = 1 << 40;
        if self.id.0 == 0
            || self.name.is_empty()
            || self.name.len() > 4096
            || self.name.contains('\0')
            || self.template.length <= 0
            || self.template.length > LIMIT
            || !self.template.gain_db.is_finite()
            || !self.template.stretch.is_finite()
            || self.template.stretch <= 0.0
            || self.template.rating > 5
            || self.template.sync_point < 0
            || self.template.sync_point > self.template.length
            || self.template.gain_env.len() > 1_000_000
            || self.template.gain_env.iter().any(|&(at, value)| at < 0 || at > self.template.length || !value.is_finite())
            || self.template.gain_env.windows(2).any(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a.0 >= b.0))
            || !(0..=LIMIT).contains(&self.template.fade_in.len)
            || !(0..=LIMIT).contains(&self.template.fade_out.len)
        {
            return Err("invalid reusable clip geometry or gain");
        }
        if self.template.slice.is_some_and(|window| {
            [window.audio_origin_samples, window.fade_in_elapsed_samples, window.fade_out_remaining_samples]
                .iter()
                .any(|value| !(-LIMIT..=LIMIT).contains(value))
                || !window.audio_phase_samples.is_finite()
                || !(0.0..1.0).contains(&window.audio_phase_samples)
                || window.audio_origin_samples.checked_add(self.template.length).is_none_or(|end| !(-LIMIT..=LIMIT).contains(&end))
                || window.fade_in_elapsed_samples.checked_add(self.template.length).is_none_or(|end| !(-LIMIT..=LIMIT).contains(&end))
                || window.fade_out_remaining_samples.checked_add(self.template.length).is_none_or(|end| !(-LIMIT..=LIMIT).contains(&end))
        }) {
            return Err("invalid reusable clip slice context");
        }
        if !matches!(self.template.content, ClipContent::Audio { .. })
            && self.template.slice.is_some_and(|window| window.audio_origin_samples != 0 || window.audio_phase_samples != 0.0)
        {
            return Err("audio coordinates on non-audio definition");
        }
        if let ClipContent::Audio { offset, .. } = self.template.content {
            let window = self.template.slice.unwrap_or_default();
            let first = offset as f64 + window.audio_phase_samples + window.audio_origin_samples as f64 / self.template.stretch;
            let last = offset as f64
                + window.audio_phase_samples
                + window.audio_origin_samples.checked_add(self.template.length - 1).ok_or("reusable audio coordinate overflow")? as f64
                    / self.template.stretch;
            if !first.is_finite() || !last.is_finite() || first < 0.0 || last < first || last > LIMIT as f64 {
                return Err("invalid reusable audio cursor");
            }
        }
        match (&self.template.content, self.timebase) {
            (ClipContent::Midi { sequence }, DefinitionTimebase::MidiTicks { span }) if span > 0 && span <= LIMIT => {
                validate_sequence(sequence, span)?;
                if let Some(original) = &self.midi_original {
                    validate_sequence(original, span)?;
                }
            }
            (ClipContent::Audio { source, offset } | ClipContent::Video { source, offset }, DefinitionTimebase::Samples)
                if source.0 != 0 && (0..=LIMIT).contains(offset) =>
            {
                if self.midi_original.is_some() {
                    return Err("saved MIDI performance on non-MIDI definition");
                }
            }
            _ => return Err("reusable clip content and timebase disagree"),
        }
        if self.metadata.values.len().saturating_add(self.metadata.flags.len()) > 65_536
            || self.metadata.values.iter().any(|(key, value)| key.len() > 256 || !value.is_finite() || !valid_metadata_key(key))
            || self.metadata.flags.iter().any(|key| key.len() > 256 || !valid_metadata_key(key))
            || self.metadata.elastic_original.as_ref().is_some_and(|original| {
                original.source.0 == 0
                    || original.source.0 >= (1u64 << 53)
                    || original.offset < 0
                    || original.length <= 0
                    || original.offset.checked_add(original.length).is_none_or(|end| end > LIMIT)
                    || !original.phase.is_finite()
                    || !(0.0..1.0).contains(&original.phase)
                    || !original.rendered_origin_samples.is_finite()
                    || !(0.0..=LIMIT as f64).contains(&original.rendered_origin_samples)
            })
        {
            return Err("invalid reusable clip metadata");
        }
        Ok(())
    }
}

pub(crate) fn valid_metadata_key(key: &str) -> bool {
    if key.matches("{id}").count() != 1 || key.contains('\0') {
        return false;
    }
    key.strip_prefix("clip_fx.{id}.").is_some_and(|suffix| !suffix.is_empty() && !suffix.contains('{') && !suffix.contains('}'))
        || key.strip_prefix("elastic.{id}.").is_some_and(|suffix| matches!(suffix, "ratio"))
        || ["clip.bpm.{id}", "clip.len0.{id}", "clip.pitch.{id}", "clip_fx.bypass.{id}"].contains(&key)
}

pub(crate) fn validate_sequence(sequence: &Sequence, span: i64) -> Result<(), &'static str> {
    use soundcraft_midi::CtrlKind;
    if sequence.notes.len().saturating_add(sequence.ctrls.len()) > 1_000_000
        || sequence.notes.iter().any(|note| {
            note.start < 0
                || note.length <= 0
                || note.start.checked_add(note.length).is_none_or(|end| end > span)
                || note.pitch > 127
                || note.channel > 15
                || !(1..=127).contains(&note.velocity)
                || note.release_velocity > 127
        })
        || sequence.ctrls.iter().any(|event| {
            event.tick < 0
                || event.tick >= span
                || event.channel > 15
                || match event.kind {
                    CtrlKind::Cc(number) => number > 127 || !(0..=127).contains(&event.value),
                    CtrlKind::PolyPressure(pitch) => pitch > 127 || !(0..=127).contains(&event.value),
                    CtrlKind::PitchBend => !(-8192..=8191).contains(&event.value),
                    _ => !(0..=127).contains(&event.value),
                }
        })
    {
        Err("invalid reusable MIDI events")
    } else {
        Ok(())
    }
}
