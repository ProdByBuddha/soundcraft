//! Clips: windows onto audio sources or MIDI sequences, placed on a playlist.

use crate::{ClipDefinitionId, ClipId, SourceId};
use soundcraft_midi::Sequence;
use soundcraft_time::{Range, Samples};

/// Fade curve shapes (Pro Tools offers standard, equal-power, S-curve and custom presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum FadeShape {
    Linear,
    #[default]
    EqualPower,
    SCurve,
    Exponential,
    Logarithmic,
}

impl FadeShape {
    pub const ALL: [FadeShape; 5] = [FadeShape::Linear, FadeShape::EqualPower, FadeShape::SCurve, FadeShape::Exponential, FadeShape::Logarithmic];

    /// Fade-in gain at `t` ∈ [0,1] (fade-outs use `gain(1 - t)`).
    pub fn gain(self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match self {
            FadeShape::Linear => t,
            FadeShape::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
            FadeShape::SCurve => 0.5 - 0.5 * (t * std::f32::consts::PI).cos(),
            FadeShape::Exponential => t * t * t,
            FadeShape::Logarithmic => 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FadeShape::Linear => "Linear",
            FadeShape::EqualPower => "Equal Power",
            FadeShape::SCurve => "S-Curve",
            FadeShape::Exponential => "Exponential",
            FadeShape::Logarithmic => "Logarithmic",
        }
    }

    pub fn from_id(s: &str) -> Option<FadeShape> {
        let s = s.to_ascii_lowercase().replace(['-', '_', ' '], "");
        FadeShape::ALL.into_iter().find(|f| f.label().to_ascii_lowercase().replace(['-', ' '], "") == s)
    }
}

/// A fade at a clip boundary, `len` samples long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Fade {
    pub len: Samples,
    pub shape: FadeShape,
}

/// What a clip plays.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ClipContent {
    /// Audio from `source`, starting `offset` samples into the file.
    Audio { source: SourceId, offset: Samples },
    /// MIDI notes, with ticks relative to the clip start.
    Midi { sequence: Sequence },
    /// Picture from the movie `source` ([`crate::VideoSource`], `Session::videos`), starting
    /// `offset` samples into the movie. Video clips carry no audio.
    Video { source: SourceId, offset: Samples },
}

/// A visible window whose source and fade coordinates retain their original anchors.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ClipSliceContext {
    pub audio_origin_samples: Samples,
    pub fade_in_elapsed_samples: Samples,
    pub fade_out_remaining_samples: Samples,
    #[serde(default, skip_serializing_if = "zero")]
    pub audio_phase_samples: f64,
}

fn zero(value: &f64) -> bool {
    *value == 0.0
}

/// A clip on a playlist.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub name: String,
    /// Timeline start (samples).
    pub start: Samples,
    /// Length (samples, > 0).
    pub length: Samples,
    pub content: ClipContent,
    /// Static clip gain in dB.
    #[serde(default)]
    pub gain_db: f32,
    /// Clip gain envelope breakpoints: (offset from clip start, dB). Empty = flat `gain_db`.
    #[serde(default)]
    pub gain_env: Vec<(Samples, f32)>,
    #[serde(default)]
    pub fade_in: Fade,
    #[serde(default)]
    pub fade_out: Fade,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub edit_locked: bool,
    #[serde(default)]
    pub time_locked: bool,
    #[serde(default)]
    pub rating: u8,
    /// Sync point offset from clip start.
    #[serde(default)]
    pub sync_point: Samples,
    /// Optional colour override (RGB).
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Elastic / Vari-speed ratio (1.0 = none) — applied by rendered processing.
    #[serde(default = "one")]
    pub stretch: f64,
    /// Reversed playback (AudioSuite Reverse renders into a new source; this is for previews).
    #[serde(default)]
    pub group: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<ClipSliceContext>,
    /// Optional provenance; changing an instance never changes its definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<ClipDefinitionId>,
}

fn one() -> f64 {
    1.0
}

impl Clip {
    pub fn audio(id: ClipId, name: impl Into<String>, source: SourceId, offset: Samples, start: Samples, length: Samples) -> Clip {
        Clip {
            id,
            name: name.into(),
            start,
            length: length.max(1),
            content: ClipContent::Audio { source, offset },
            gain_db: 0.0,
            gain_env: Vec::new(),
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            muted: false,
            edit_locked: false,
            time_locked: false,
            rating: 0,
            sync_point: 0,
            color: None,
            stretch: 1.0,
            group: None,
            slice: None,
            definition: None,
        }
    }

    pub fn midi(id: ClipId, name: impl Into<String>, start: Samples, length: Samples, sequence: Sequence) -> Clip {
        let mut c = Clip::audio(id, name, SourceId(0), 0, start, length);
        c.content = ClipContent::Midi { sequence };
        c
    }

    /// A clip showing the movie `source` from `offset` samples into it.
    pub fn video(id: ClipId, name: impl Into<String>, source: SourceId, offset: Samples, start: Samples, length: Samples) -> Clip {
        let mut c = Clip::audio(id, name, SourceId(0), 0, start, length);
        c.content = ClipContent::Video { source, offset };
        c
    }

    pub fn end(&self) -> Samples {
        self.start.saturating_add(self.length)
    }

    pub fn range(&self) -> Range {
        Range { start: self.start, end: self.end() }
    }

    pub fn is_audio(&self) -> bool {
        matches!(self.content, ClipContent::Audio { .. })
    }

    pub fn is_video(&self) -> bool {
        matches!(self.content, ClipContent::Video { .. })
    }

    /// The audio source this clip plays (None for MIDI and video clips).
    pub fn source(&self) -> Option<SourceId> {
        match self.content {
            ClipContent::Audio { source, .. } => Some(source),
            ClipContent::Midi { .. } | ClipContent::Video { .. } => None,
        }
    }

    /// The movie this clip shows (video clips only).
    pub fn video_source(&self) -> Option<SourceId> {
        match self.content {
            ClipContent::Video { source, .. } => Some(source),
            ClipContent::Audio { .. } | ClipContent::Midi { .. } => None,
        }
    }

    /// Offset into the audio file or movie (0 for MIDI).
    pub fn source_offset(&self) -> Samples {
        match self.content {
            ClipContent::Audio { offset, .. } | ClipContent::Video { offset, .. } => offset,
            ClipContent::Midi { .. } => 0,
        }
    }

    /// Fractional media coordinate. Audio preserves a sliced stretch phase; picture is linear.
    pub fn source_position(&self, rel: Samples) -> f64 {
        match self.content {
            ClipContent::Audio { offset, .. } => {
                let origin = self.slice.map_or(0, |window| window.audio_origin_samples);
                let stretch = if self.stretch.is_finite() && self.stretch > 0.0 { self.stretch } else { 1.0 };
                offset as f64 + self.slice.map_or(0.0, |window| window.audio_phase_samples) + origin.saturating_add(rel) as f64 / stretch
            }
            ClipContent::Video { offset, .. } => offset.saturating_add(rel) as f64,
            ClipContent::Midi { .. } => 0.0,
        }
    }

    pub fn envelope_db_at(&self, rel: Samples) -> f32 {
        env_value(&self.gain_env, rel)
    }

    pub fn gain_db_at(&self, rel: Samples) -> f32 {
        self.gain_db + self.envelope_db_at(rel)
    }

    pub fn fade_in_gain_at(&self, rel: Samples) -> f32 {
        let elapsed = self.slice.map_or(0, |window| window.fade_in_elapsed_samples).saturating_add(rel);
        if self.fade_in.len > 0 && elapsed < self.fade_in.len { self.fade_in.shape.gain(elapsed as f32 / self.fade_in.len as f32) } else { 1.0 }
    }

    pub fn fade_out_gain_at(&self, rel: Samples) -> f32 {
        let from_end = self.length.saturating_sub(rel).saturating_add(self.slice.map_or(0, |window| window.fade_out_remaining_samples));
        if self.fade_out.len > 0 && from_end <= self.fade_out.len {
            self.fade_out.shape.gain(from_end as f32 / self.fade_out.len as f32)
        } else {
            1.0
        }
    }

    /// Gain (linear) at `rel` samples from the clip start: clip gain × envelope × fades.
    pub fn gain_at(&self, rel: Samples) -> f32 {
        db_to_gain(self.gain_db_at(rel)) * self.fade_in_gain_at(rel) * self.fade_out_gain_at(rel)
    }

    /// Move the start edge to `new_start` keeping material anchored (trim start).
    pub fn trim_start_to(&mut self, new_start: Samples) {
        if let Some(mut window) = self.slice {
            let new_start = new_start.min(self.end().saturating_sub(1));
            let delta = new_start.saturating_sub(self.start);
            let length = self.length.saturating_sub(delta).max(1);
            self.gain_env = self.sliced_gain_env(delta, length);
            if self.is_audio() {
                window.audio_origin_samples = window.audio_origin_samples.saturating_add(delta);
            } else if let ClipContent::Video { offset, .. } = &mut self.content {
                *offset = offset.saturating_add(delta);
            }
            window.fade_in_elapsed_samples = window.fade_in_elapsed_samples.saturating_add(delta);
            self.slice = Some(window);
            self.length = length;
            self.start = new_start;
            self.sync_point = self.sync_point.saturating_sub(delta).clamp(0, length);
            return;
        }
        let new_start = new_start.min(self.end() - 1);
        let delta = new_start - self.start;
        if let ClipContent::Audio { offset, .. } | ClipContent::Video { offset, .. } = &mut self.content {
            *offset = offset.saturating_add(delta);
        }
        // MIDI note ticks are re-anchored by the engine, which knows the tempo map.
        self.length -= delta;
        self.start = new_start;
        self.clamp_fades();
        self.gain_env = self.gain_env.iter().filter_map(|&(t, v)| (t - delta >= 0).then_some((t - delta, v))).collect();
    }

    /// Move the end edge to `new_end`.
    pub fn trim_end_to(&mut self, new_end: Samples) {
        if let Some(mut window) = self.slice {
            let new_end = new_end.max(self.start.saturating_add(1));
            let length = new_end.saturating_sub(self.start).max(1);
            self.gain_env = self.sliced_gain_env(0, length);
            window.fade_out_remaining_samples = window.fade_out_remaining_samples.saturating_add(self.length.saturating_sub(length));
            self.slice = Some(window);
            self.length = length;
            self.sync_point = self.sync_point.clamp(0, length);
            return;
        }
        let new_end = new_end.max(self.start + 1);
        self.length = new_end - self.start;
        self.clamp_fades();
    }

    /// Keep both fades within the clip.
    pub fn clamp_fades(&mut self) {
        let maximum = if self.slice.is_some() { 1 << 40 } else { self.length };
        self.fade_in.len = self.fade_in.len.clamp(0, maximum);
        self.fade_out.len = self.fade_out.len.clamp(0, maximum);
    }

    /// Preserve interpolated values at both cuts; input envelopes must be finite and sorted.
    pub fn sliced_gain_env(&self, origin: Samples, length: Samples) -> Vec<(Samples, f32)> {
        if self.gain_env.is_empty() {
            return Vec::new();
        }
        let end = origin.saturating_add(length);
        let mut points = Vec::with_capacity(self.gain_env.len().saturating_add(2));
        points.push((0, self.envelope_db_at(origin)));
        points.extend(self.gain_env.iter().filter_map(|&(at, value)| (at > origin && at < end).then_some((at.saturating_sub(origin), value))));
        points.push((length, self.envelope_db_at(end)));
        points
    }
}

fn env_value(env: &[(Samples, f32)], at: Samples) -> f32 {
    let index = env.partition_point(|&(tick, _)| tick < at);
    let Some(&(tick, value)) = env.get(index) else { return env.last().map_or(0.0, |&(_, value)| value) };
    match index.checked_sub(1).and_then(|previous| env.get(previous)).copied() {
        Some((previous, before)) if tick > previous => {
            before + (value - before) * (at.saturating_sub(previous) as f32 / tick.saturating_sub(previous) as f32)
        }
        _ => value,
    }
}

pub(crate) fn db_to_gain(db: f32) -> f32 {
    if db.is_nan() || db <= -144.0 { 0.0 } else { 10f32.powf(db.min(48.0) / 20.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_shapes_hit_endpoints() {
        for s in FadeShape::ALL {
            assert!(s.gain(0.0).abs() < 1e-6, "{s:?}");
            assert!((s.gain(1.0) - 1.0).abs() < 1e-6, "{s:?}");
            assert_eq!(s.gain(f32::NAN), 0.0);
            assert_eq!(FadeShape::from_id(s.label()), Some(s));
        }
    }

    #[test]
    fn gain_with_fades() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 0, 100, 1000);
        c.fade_in = Fade { len: 100, shape: FadeShape::Linear };
        c.fade_out = Fade { len: 100, shape: FadeShape::Linear };
        assert!(c.gain_at(0).abs() < 1e-6);
        assert!((c.gain_at(50) - 0.5).abs() < 1e-6);
        assert!((c.gain_at(500) - 1.0).abs() < 1e-6);
        assert!((c.gain_at(950) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn gain_envelope_interpolates() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 0, 0, 1000);
        c.gain_env = vec![(0, 0.0), (1000, -20.0)];
        assert!((c.gain_at(500) - db_to_gain(-10.0)).abs() < 1e-4);
    }

    #[test]
    fn trims_keep_material_anchored() {
        let mut c = Clip::audio(ClipId(1), "a", SourceId(1), 1000, 100, 1000);
        c.trim_start_to(300);
        assert_eq!((c.start, c.length, c.source_offset()), (300, 800, 1200));
        c.trim_end_to(500);
        assert_eq!(c.length, 200);
        c.trim_end_to(-50);
        assert_eq!(c.length, 1);
        // Regression: trimming the start must not leave a fade-out longer than the clip.
        let mut c = Clip::audio(ClipId(2), "b", SourceId(1), 0, 0, 1000);
        c.fade_out = Fade { len: 800, shape: FadeShape::Linear };
        c.trim_start_to(900);
        assert!(c.fade_out.len <= c.length);
    }
}
