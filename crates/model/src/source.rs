//! Audio sources (files in the session's Audio Files folder) and their decoded data, and the
//! movies video clips reference.

use crate::SourceId;
use soundcraft_audio_io::{AudioBuffer, AudioInfo, FileFormat, peaks::Peaks};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// How an import keeps its encoded source separate from working PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportPolicy {
    Copy,
    Reference,
    Convert,
}

/// Identity and metadata of the encoded original, before session-rate conversion.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OriginalProvenance {
    pub version: u32,
    pub policy: ImportPolicy,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub info: AudioInfo,
}

impl OriginalProvenance {
    /// Convert the original BWF sample counter to timeline samples, rounded to nearest.
    /// The stored counter and original sample rate remain unchanged.
    pub fn timeline_time_reference(&self, target_rate: u32) -> Result<Option<i64>, &'static str> {
        let Some(bwf) = &self.info.bwf else { return Ok(None) };
        if self.info.sample_rate == 0 || target_rate == 0 {
            return Err("BWF time reference requires nonzero sample rates");
        }
        let denominator = u128::from(self.info.sample_rate);
        let numerator = u128::from(bwf.time_reference)
            .checked_mul(u128::from(target_rate))
            .and_then(|value| value.checked_add(denominator / 2))
            .ok_or("BWF time reference conversion overflow")?;
        let samples = i64::try_from(numerator / denominator).map_err(|_| "BWF time reference exceeds timeline range")?;
        Ok(Some(samples))
    }
}

/// A runtime ownership token for a managed encoded original.
/// Reclamation belongs to the control-side asset store; dropping a token performs no I/O.
#[derive(Debug)]
pub struct OriginalAssetLease {
    pub path: PathBuf,
}

/// Metadata for an audio file used by the session (serialised).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub name: String,
    /// Path relative to the session folder (or absolute for referenced media).
    pub path: String,
    pub channels: u16,
    pub frames: u64,
    pub sample_rate: u32,
    pub format: FileFormat,
    /// Original BWF time stamp (samples), if any.
    #[serde(default)]
    pub time_reference: u64,
    /// True when the file has not been written to disk yet (recorded/rendered this session).
    #[serde(default)]
    pub unsaved: bool,
    /// Versioned identity of decoded PCM at the source's sample rate. Old sessions may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_identity: Option<String>,
    /// Encoded-original identity and metadata. Legacy and generated sources may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<OriginalProvenance>,
}

/// A movie used by Video track clips (serialised). The pictures themselves are never stored in
/// the session: the UI opens the movie from `path` and decodes frames on demand.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoSource {
    pub id: SourceId,
    pub name: String,
    /// Absolute path of the movie (referenced, never copied into the session folder).
    pub path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Frames per second (0 = unknown).
    #[serde(default)]
    pub frame_rate: f64,
    /// Picture length in seconds.
    #[serde(default)]
    pub duration: f64,
    /// Codec id (`h264`, `prores`, `mjpeg`, `hevc`, …).
    #[serde(default)]
    pub codec: String,
}

impl VideoSource {
    /// Display aspect ratio (width / height), 16:9 when unknown.
    pub fn aspect(&self) -> f64 {
        if self.width == 0 || self.height == 0 { 16.0 / 9.0 } else { f64::from(self.width) / f64::from(self.height) }
    }
}

/// Decoded audio plus waveform overviews (not serialised).
#[derive(Debug)]
pub struct SourceAudio {
    pub buffer: AudioBuffer,
    pub peaks: Vec<Peaks>,
    pub original_asset: Option<Arc<OriginalAssetLease>>,
}

impl SourceAudio {
    pub fn new(buffer: AudioBuffer) -> Self {
        let peaks = buffer.channels.iter().map(|c| Peaks::build(c)).collect();
        SourceAudio { buffer, peaks, original_asset: None }
    }
    pub fn with_original_asset(buffer: AudioBuffer, original_asset: Arc<OriginalAssetLease>) -> Self {
        let mut audio = Self::new(buffer);
        audio.original_asset = Some(original_asset);
        audio
    }
    pub fn frames(&self) -> usize {
        self.buffer.frames()
    }
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.buffer.channels.get(ch).map(Vec::as_slice)
    }
}

/// Shared decoded audio and independent encoded-original ownership, keyed by source id.
#[derive(Debug, Clone, Default)]
pub struct SourcePool {
    map: BTreeMap<SourceId, Arc<SourceAudio>>,
    originals: BTreeMap<SourceId, Arc<OriginalAssetLease>>,
}

impl SourcePool {
    pub fn insert(&mut self, id: SourceId, audio: Arc<SourceAudio>) {
        if let Some(original) = &audio.original_asset {
            self.originals.insert(id, original.clone());
        }
        self.map.insert(id, audio);
    }
    pub fn get(&self, id: SourceId) -> Option<&Arc<SourceAudio>> {
        self.map.get(&id)
    }
    /// Remove decoded PCM only; an original can remain valid when its derivative is unavailable.
    pub fn remove(&mut self, id: SourceId) -> Option<Arc<SourceAudio>> {
        self.map.remove(&id)
    }
    /// Retain a verified managed original, including when no decoded derivative is available.
    pub fn retain_original(&mut self, id: SourceId, original: Arc<OriginalAssetLease>) {
        self.originals.insert(id, original);
    }
    pub fn original_asset(&self, id: SourceId) -> Option<&Arc<OriginalAssetLease>> {
        self.originals.get(&id)
    }
    /// Release this pool's original ownership when the source itself is explicitly removed.
    /// Held SourceAudio/session snapshots can still retain the lease.
    pub fn remove_original(&mut self, id: SourceId) -> Option<Arc<OriginalAssetLease>> {
        self.originals.remove(&id)
    }
    pub fn contains(&self, id: SourceId) -> bool {
        self.map.contains_key(&id)
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn ids(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.map.keys().copied()
    }
}

impl PartialEq for SourcePool {
    fn eq(&self, other: &Self) -> bool {
        self.map.len() == other.map.len()
            && self.map.iter().zip(other.map.iter()).all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
            && self.originals.len() == other.originals.len()
            && self.originals.iter().zip(other.originals.iter()).all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }
}
