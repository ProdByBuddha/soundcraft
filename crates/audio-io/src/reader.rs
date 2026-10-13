//! Owned, seekable decode inputs. Only small header prefixes and PCM chunks are copied.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::io::MediaSource;

use crate::pcm::{MAX_DECODE_SAMPLES, PcmKind, decode_sample};
use crate::{AudioBuffer, AudioError, AudioInfo, BwfInfo, FileFormat, Result, aiff, detect_format, symph, wav};

const READ_CHUNK: usize = 64 * 1024;
const MAX_CHUNKS: usize = 100_000;

/// Admission and cooperative cancellation for owned-reader operations.
/// The PCM bound counts sample values across all channels, not frames.
#[derive(Debug, Clone)]
pub struct ReaderOptions {
    pub max_bytes: u64,
    pub max_sample_values: u64,
    pub cancel: Arc<AtomicBool>,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        Self { max_bytes: u64::MAX, max_sample_values: MAX_DECODE_SAMPLES, cancel: Arc::new(AtomicBool::new(false)) }
    }
}

impl ReaderOptions {
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) { Err(AudioError::Cancelled) } else { Ok(()) }
    }

    pub(crate) fn admit_samples(&self, frames: u64, channels: usize) -> Result<()> {
        let total = frames.checked_mul(channels as u64).ok_or_else(|| AudioError::TooLarge("sample count overflow".into()))?;
        if total > self.max_sample_values.min(MAX_DECODE_SAMPLES) {
            return Err(AudioError::TooLarge(format!("{total} sample values exceeds reader decode limit")));
        }
        Ok(())
    }
}

struct BoundedReader<R> {
    reader: R,
    len: u64,
    position: u64,
    options: ReaderOptions,
}

impl<R: Read + Seek> BoundedReader<R> {
    fn new(mut reader: R, options: ReaderOptions) -> Result<Self> {
        options.check_cancelled()?;
        let len = reader.seek(SeekFrom::End(0)).map_err(io_error)?;
        if len > options.max_bytes {
            return Err(AudioError::TooLarge(format!("{len} encoded bytes exceeds reader limit")));
        }
        let position = reader.seek(SeekFrom::Start(0)).map_err(io_error)?;
        if position != 0 {
            return Err(AudioError::Malformed("reader did not seek to requested position".into()));
        }
        options.check_cancelled()?;
        Ok(Self { reader, len, position, options })
    }
}

fn cancellation_io() -> io::Error {
    // Interrupted is deliberately avoided: read_exact retries it indefinitely.
    io::Error::other("audio operation cancelled")
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.options.cancel.load(Ordering::Acquire) {
            return Err(cancellation_io());
        }
        let remaining = self.len.saturating_sub(self.position);
        let count = buf.len().min(READ_CHUNK).min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = self.reader.read(&mut buf[..count])?;
        if read > count {
            return Err(io::Error::other("reader exceeded requested byte count"));
        }
        self.position = self.position.checked_add(read as u64).ok_or_else(|| io::Error::other("reader position overflow"))?;
        if self.options.cancel.load(Ordering::Acquire) {
            return Err(cancellation_io());
        }
        Ok(read)
    }
}

impl<R: Seek> Seek for BoundedReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if self.options.cancel.load(Ordering::Acquire) {
            return Err(cancellation_io());
        }
        let requested = match from {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
        };
        if !(0..=i128::from(self.len)).contains(&requested) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek outside admitted input"));
        }
        let requested = u64::try_from(requested).map_err(|_| io::Error::other("reader seek overflow"))?;
        let actual = self.reader.seek(SeekFrom::Start(requested))?;
        if actual != requested {
            return Err(io::Error::other("reader did not seek to requested position"));
        }
        self.position = actual;
        if self.options.cancel.load(Ordering::Acquire) {
            return Err(cancellation_io());
        }
        Ok(actual)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for BoundedReader<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn io_error(error: io::Error) -> AudioError {
    AudioError::Malformed(error.to_string())
}

fn finish<T>(options: &ReaderOptions, result: Result<T>) -> Result<T> {
    options.check_cancelled()?;
    result
}

struct NativeLayout {
    info: AudioInfo,
    kind: PcmKind,
    data_start: u64,
}

enum Layout {
    Native(NativeLayout),
    Foreign { detected: FileFormat, bwf: Option<BwfInfo> },
}

fn prefix<R: Read + Seek>(reader: &mut BoundedReader<R>, at: u64, length: usize) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(at)).map_err(io_error)?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    Ok(bytes)
}

fn append_chunk(header: &mut Vec<u8>, id: &[u8; 4], body: &[u8], big_endian: bool) -> Result<()> {
    let size = u32::try_from(body.len()).map_err(|_| AudioError::TooLarge("header prefix too large".into()))?;
    header.extend_from_slice(id);
    header.extend_from_slice(&if big_endian { size.to_be_bytes() } else { size.to_le_bytes() });
    header.extend_from_slice(body);
    if !body.len().is_multiple_of(2) {
        header.push(0);
    }
    Ok(())
}

/// Reuse the existing native header validators with a small metadata-only container.
/// Sample offsets and lengths always refer to the admitted original reader.
fn layout<R: Read + Seek>(reader: &mut BoundedReader<R>, ext_hint: Option<&str>) -> Result<Layout> {
    let initial_len = usize::try_from(reader.len.min(12)).unwrap_or(12);
    let initial = prefix(reader, 0, initial_len)?;
    let detected = detect_format(&initial, ext_hint);
    if !wav::is_wav(&initial) && !aiff::is_aiff(&initial) {
        return Ok(Layout::Foreign { detected, bwf: None });
    }
    let is_wav = wav::is_wav(&initial);
    let rf64 = is_wav && initial.get(..4) != Some(b"RIFF");
    let mut fmt = None;
    let mut bext = None;
    let mut comm = None;
    let mut ds64_data = None;
    let mut data = None;
    let mut position = 12u64;
    let mut chunks = 0usize;
    while reader.len.saturating_sub(position) >= 8 {
        reader.options.check_cancelled()?;
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(AudioError::TooLarge("too many audio container chunks".into()));
        }
        let bytes = prefix(reader, position, 8)?;
        let id = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let size_bytes = [bytes[4], bytes[5], bytes[6], bytes[7]];
        let size32 = if is_wav { u32::from_le_bytes(size_bytes) } else { u32::from_be_bytes(size_bytes) };
        let body = position.checked_add(8).ok_or_else(|| AudioError::Malformed("chunk offset overflow".into()))?;
        let remaining = reader.len.saturating_sub(body);
        let declared = if &id == b"data" && rf64 && size32 == u32::MAX { ds64_data.unwrap_or(remaining) } else { u64::from(size32) };
        let length = declared.min(remaining);
        if is_wav {
            if &id == b"data" {
                let length = if size32 == 0 && !rf64 { remaining } else { length };
                if data.is_none() {
                    data = Some((body, length));
                }
            } else if declared <= remaining {
                match &id {
                    b"fmt " => fmt = Some(prefix(reader, body, usize::try_from(length.min(40)).unwrap_or(40))?),
                    b"bext" => bext = Some(prefix(reader, body, usize::try_from(length.min(346)).unwrap_or(346))?),
                    b"ds64" => {
                        let ds = prefix(reader, body, usize::try_from(length.min(28)).unwrap_or(28))?;
                        ds64_data = ds.get(8..16).and_then(|bytes| bytes.try_into().ok()).map(u64::from_le_bytes);
                    }
                    _ => {}
                }
            }
        } else {
            match &id {
                b"COMM" => comm = Some(prefix(reader, body, usize::try_from(length.min(22)).unwrap_or(22))?),
                b"SSND" if data.is_none() => {
                    let offset = if length >= 4 {
                        let bytes = prefix(reader, body, 4)?;
                        u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    } else {
                        0
                    };
                    let start = body
                        .checked_add(8)
                        .and_then(|value| value.checked_add(offset))
                        .ok_or_else(|| AudioError::Malformed("SSND offset overflow".into()))?;
                    data = Some((start.min(reader.len), length.saturating_sub(8).saturating_sub(offset)));
                }
                _ => {}
            }
        }
        if declared > remaining {
            break;
        }
        position = body
            .checked_add(declared)
            .and_then(|value| value.checked_add(declared & 1))
            .ok_or_else(|| AudioError::Malformed("chunk offset overflow".into()))?;
    }
    let mut header = initial;
    if is_wav {
        if let Some(fmt) = fmt {
            append_chunk(&mut header, b"fmt ", &fmt, false)?;
        }
        if let Some(bext) = bext {
            append_chunk(&mut header, b"bext", &bext, false)?;
        }
        if data.is_some() {
            append_chunk(&mut header, b"data", &[], false)?;
        }
        match wav::parse(&header)? {
            wav::WavParse::Foreign { bwf } => Ok(Layout::Foreign { detected: FileFormat::Wav, bwf }),
            wav::WavParse::Native(native) => {
                let (data_start, data_len) = data.ok_or_else(|| AudioError::Malformed("WAV has no data chunk".into()))?;
                let block = native.kind.bytes() as u64 * u64::from(native.info.channels);
                let mut info = native.info;
                info.frames = data_len / block;
                reader.options.admit_samples(info.frames, usize::from(info.channels))?;
                Ok(Layout::Native(NativeLayout { info, kind: native.kind, data_start }))
            }
        }
    } else {
        let declared_frames =
            comm.as_ref().and_then(|bytes| bytes.get(2..6)).and_then(|bytes| bytes.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
        if let Some(comm) = comm {
            append_chunk(&mut header, b"COMM", &comm, true)?;
        }
        append_chunk(&mut header, b"SSND", &[0; 8], true)?;
        match aiff::parse(&header)? {
            aiff::AiffParse::Foreign => Ok(Layout::Foreign { detected: FileFormat::Aiff, bwf: None }),
            aiff::AiffParse::Native(native) => {
                let (data_start, data_len) = data.unwrap_or((reader.len, 0));
                let block = native.kind.bytes() as u64 * u64::from(native.info.channels);
                let mut info = native.info;
                info.frames = u64::from(declared_frames).min(data_len / block);
                reader.options.admit_samples(info.frames, usize::from(info.channels))?;
                Ok(Layout::Native(NativeLayout { info, kind: native.kind, data_start }))
            }
        }
    }
}

fn decode_native<R: Read + Seek>(reader: &mut BoundedReader<R>, layout: &NativeLayout) -> Result<AudioBuffer> {
    reader.options.check_cancelled()?;
    let channels = usize::from(layout.info.channels);
    reader.options.admit_samples(layout.info.frames, channels)?;
    let frames = usize::try_from(layout.info.frames).map_err(|_| AudioError::TooLarge("frame count exceeds address space".into()))?;
    let block = layout.kind.bytes().checked_mul(channels).ok_or_else(|| AudioError::TooLarge("PCM block size overflow".into()))?;
    let chunk_frames = (READ_CHUNK / block).max(1);
    let mut planar = Vec::new();
    planar.try_reserve_exact(channels).map_err(|_| AudioError::TooLarge("PCM channel allocation failed".into()))?;
    for _ in 0..channels {
        let mut channel = Vec::new();
        channel.try_reserve_exact(frames).map_err(|_| AudioError::TooLarge("PCM sample allocation failed".into()))?;
        planar.push(channel);
    }
    let chunk_len = chunk_frames.checked_mul(block).ok_or_else(|| AudioError::TooLarge("PCM chunk size overflow".into()))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(chunk_len).map_err(|_| AudioError::TooLarge("PCM chunk allocation failed".into()))?;
    bytes.resize(chunk_len, 0);
    reader.seek(SeekFrom::Start(layout.data_start)).map_err(io_error)?;
    let mut remaining = frames;
    while remaining > 0 {
        reader.options.check_cancelled()?;
        let count = remaining.min(chunk_frames);
        let bytes = &mut bytes[..count * block];
        reader.read_exact(bytes).map_err(io_error)?;
        for frame in bytes.chunks_exact(block) {
            for (channel, sample) in planar.iter_mut().zip(frame.chunks_exact(layout.kind.bytes())) {
                channel.push(decode_sample(sample, layout.kind));
            }
        }
        remaining -= count;
    }
    reader.options.check_cancelled()?;
    Ok(AudioBuffer { sample_rate: layout.info.sample_rate, channels: planar })
}

/// Probe an owned seekable input without retaining its encoded bytes in memory.
pub fn probe_reader<R: Read + Seek + Send + Sync + 'static>(reader: R, ext_hint: Option<&str>, options: ReaderOptions) -> Result<AudioInfo> {
    let result = (|| {
        let mut reader = BoundedReader::new(reader, options.clone())?;
        match layout(&mut reader, ext_hint)? {
            Layout::Native(layout) => Ok(layout.info),
            Layout::Foreign { detected, bwf } => {
                reader.seek(SeekFrom::Start(0)).map_err(io_error)?;
                let mut info = symph::probe_reader(Box::new(reader), ext_hint, detected, &options)?;
                if bwf.is_some() {
                    info.bwf = bwf;
                }
                Ok(info)
            }
        }
    })();
    finish(&options, result)
}

/// Decode into working PCM with checked sample admission and cooperative cancellation.
/// Codec-internal allocations are outside this adapter's allocation bound.
pub fn decode_reader<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    ext_hint: Option<&str>,
    options: ReaderOptions,
) -> Result<(AudioInfo, AudioBuffer)> {
    let result = (|| {
        let mut reader = BoundedReader::new(reader, options.clone())?;
        match layout(&mut reader, ext_hint)? {
            Layout::Native(layout) => {
                let buffer = decode_native(&mut reader, &layout)?;
                Ok((layout.info, buffer))
            }
            Layout::Foreign { detected, bwf } => {
                reader.seek(SeekFrom::Start(0)).map_err(io_error)?;
                let (mut info, buffer) = symph::decode_reader(Box::new(reader), ext_hint, detected, &options)?;
                if bwf.is_some() {
                    info.bwf = bwf;
                }
                Ok((info, buffer))
            }
        }
    })();
    finish(&options, result)
}
