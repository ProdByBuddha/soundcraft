use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use soundcraft_audio_io::{
    AudioBuffer, AudioError, BitDepth, EncodeOptions, FileFormat, ReaderOptions, SampleFormat, decode, decode_reader, encode, encode_wav_rf64, probe,
    probe_reader,
};

fn chunk(bytes: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    bytes.extend_from_slice(id);
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(body);
    if body.len() % 2 == 1 {
        bytes.push(0);
    }
}

fn fmt(rate: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&3u16.to_le_bytes()); // IEEE float, independent fixture header.
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&rate.to_le_bytes());
    body.extend_from_slice(&(rate * 8).to_le_bytes());
    body.extend_from_slice(&8u16.to_le_bytes());
    body.extend_from_slice(&32u16.to_le_bytes());
    body
}

fn bwf(rate: u32, frames: usize) -> (Vec<u8>, Vec<Vec<f32>>) {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    chunk(&mut bytes, b"fmt ", &fmt(rate));
    let mut bext = vec![0; 602];
    bext[..18].copy_from_slice(b"Original recording");
    bext[256..266].copy_from_slice(b"SoundCraft");
    let raw = (1u64 << 40) + 12345;
    bext[338..342].copy_from_slice(&(raw as u32).to_le_bytes());
    bext[342..346].copy_from_slice(&((raw >> 32) as u32).to_le_bytes());
    chunk(&mut bytes, b"bext", &bext);
    chunk(&mut bytes, b"JUNK", b"opaque original bytes\0\xff\x80");
    let choices = [1.25f32, -2.5, -0.0, 0.125, -0.75];
    let channels: Vec<Vec<f32>> = (0..2).map(|channel| (0..frames).map(|at| choices[(at + channel * 2) % choices.len()]).collect()).collect();
    let mut data = Vec::new();
    for frame in 0..frames {
        for channel in &channels {
            data.extend_from_slice(&channel[frame].to_le_bytes());
        }
    }
    chunk(&mut bytes, b"data", &data);
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    (bytes, channels)
}

struct ShortReader {
    cursor: Cursor<Arc<[u8]>>,
    max_request: Arc<AtomicUsize>,
    cancel_after: Option<(usize, Arc<AtomicBool>)>,
    reads: usize,
}

impl Read for ShortReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.max_request.fetch_max(buf.len(), Ordering::Relaxed);
        self.reads += 1;
        let count = buf.len().min(17);
        let result = self.cursor.read(&mut buf[..count]);
        if let Some((at, flag)) = &self.cancel_after
            && self.reads == *at
        {
            flag.store(true, Ordering::Release);
        }
        result
    }
}

impl Seek for ShortReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(from)
    }
}

fn short_reader(bytes: Arc<[u8]>, max_request: Arc<AtomicUsize>) -> ShortReader {
    ShortReader { cursor: Cursor::new(bytes), max_request, cancel_after: None, reads: 0 }
}

fn assert_pcm_bits(actual: &AudioBuffer, expected: &[Vec<f32>]) {
    assert_eq!(actual.num_channels(), expected.len());
    for (actual, expected) in actual.channels.iter().zip(expected) {
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual.iter().map(|value| value.to_bits()).collect::<Vec<_>>(), expected.iter().map(|value| value.to_bits()).collect::<Vec<_>>());
    }
}

#[test]
fn mixed_rate_bwf_reader_preserves_raw_metadata_float_bits_and_original_bytes() {
    for rate in [44_100, 48_000, 96_000] {
        let (bytes, expected) = bwf(rate, 20_003);
        let original = bytes.clone();
        let bytes: Arc<[u8]> = bytes.into();
        let max_request = Arc::new(AtomicUsize::new(0));
        let info = probe_reader(short_reader(bytes.clone(), max_request.clone()), Some("wav"), ReaderOptions::default()).unwrap();
        assert_eq!((info.sample_rate, info.channels, info.frames), (rate, 2, 20_003));
        assert_eq!(info.sample_format, SampleFormat::Float32);
        let metadata = info.bwf.as_ref().unwrap();
        assert_eq!(metadata.time_reference, (1u64 << 40) + 12345);
        assert_eq!(metadata.description, "Original recording");
        assert_eq!(metadata.originator, "SoundCraft");
        assert_eq!(info, probe(&bytes, Some("wav")).unwrap());
        let (decoded_info, audio) = decode_reader(short_reader(bytes.clone(), max_request.clone()), None, ReaderOptions::default()).unwrap();
        assert_eq!(decoded_info, info);
        assert_eq!(audio.sample_rate, rate);
        assert_pcm_bits(&audio, &expected);
        assert_eq!(bytes.as_ref(), original.as_slice());
        assert!(max_request.load(Ordering::Relaxed) <= 64 * 1024);
    }
}

#[test]
fn reader_native_aiff_rf64_and_foreign_flac_match_independent_quantized_signal() {
    let channels = vec![vec![-0.75, -0.5, -0.125, 0.0, 0.125, 0.5, 0.75]; 2];
    let buffer = AudioBuffer { sample_rate: 44_100, channels: channels.clone() };
    for format in [FileFormat::Aiff, FileFormat::Flac, FileFormat::Wav] {
        let options = EncodeOptions { format, bit_depth: BitDepth::Int16, dither: false, bwf: None };
        let bytes = if format == FileFormat::Wav { encode_wav_rf64(&buffer, &options).unwrap() } else { encode(&buffer, &options).unwrap() };
        let info = probe_reader(Cursor::new(bytes.clone()), None, ReaderOptions::default()).unwrap();
        assert_eq!((info.format, info.sample_rate, info.channels, info.frames), (format, 44_100, 2, 7));
        assert_eq!(info, probe(&bytes, None).unwrap());
        let (decoded_info, actual) = decode_reader(Cursor::new(bytes.clone()), None, ReaderOptions::default()).unwrap();
        assert_eq!(decoded_info, info);
        assert_pcm_bits(&actual, &channels);
        assert_eq!(actual, decode(&bytes, None).unwrap().1);
    }
    let options = EncodeOptions { format: FileFormat::Aiff, bit_depth: BitDepth::Float32, dither: false, bwf: None };
    let buffer = AudioBuffer { sample_rate: 96_000, channels: vec![vec![-2.5, 1.25, -0.0]] };
    let (_, actual) = decode_reader(Cursor::new(encode(&buffer, &options).unwrap()), None, ReaderOptions::default()).unwrap();
    assert_pcm_bits(&actual, &buffer.channels);
}

#[test]
fn reader_rejects_byte_sample_and_malformed_bounds_before_pcm_allocation() {
    let (bytes, _) = bwf(48_000, 50);
    let options = ReaderOptions { max_bytes: bytes.len() as u64 - 1, ..ReaderOptions::default() };
    assert!(matches!(probe_reader(Cursor::new(bytes.clone()), None, options), Err(AudioError::TooLarge(_))));
    let options = ReaderOptions { max_sample_values: 99, ..ReaderOptions::default() };
    assert!(matches!(decode_reader(Cursor::new(bytes.clone()), None, options.clone()), Err(AudioError::TooLarge(_))));
    assert!(matches!(probe_reader(Cursor::new(bytes), None, options), Err(AudioError::TooLarge(_))));
    for bytes in [b"RIFF\0\0\0\0WAVE".to_vec(), b"FORM\0\0\0\0AIFF".to_vec()] {
        assert!(matches!(decode_reader(Cursor::new(bytes), None, ReaderOptions::default()), Err(AudioError::Malformed(_))));
    }
    let buffer = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 1000]; 2] };
    let bytes = encode(&buffer, &EncodeOptions { format: FileFormat::Flac, bit_depth: BitDepth::Int16, dither: false, bwf: None }).unwrap();
    let options = ReaderOptions { max_sample_values: 1999, ..ReaderOptions::default() };
    assert!(matches!(decode_reader(Cursor::new(bytes), None, options), Err(AudioError::TooLarge(_))));
}

#[test]
fn cancellation_before_probe_during_metadata_pcm_and_foreign_reads_never_returns_partial_success() {
    let (bytes, _) = bwf(48_000, 50_000);
    let cancel = Arc::new(AtomicBool::new(true));
    let options = ReaderOptions { cancel: cancel.clone(), ..ReaderOptions::default() };
    assert!(matches!(probe_reader(Cursor::new(bytes.clone()), None, options), Err(AudioError::Cancelled)));
    for after in [2, 40] {
        cancel.store(false, Ordering::Release);
        let mut reader = short_reader(bytes.clone().into(), Arc::new(AtomicUsize::new(0)));
        reader.cancel_after = Some((after, cancel.clone()));
        let options = ReaderOptions { cancel: cancel.clone(), ..ReaderOptions::default() };
        assert!(matches!(decode_reader(reader, None, options), Err(AudioError::Cancelled)));
    }
    let buffer = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 9000]; 2] };
    let bytes = encode(&buffer, &EncodeOptions { format: FileFormat::Flac, bit_depth: BitDepth::Int16, dither: false, bwf: None }).unwrap();
    cancel.store(false, Ordering::Release);
    let mut reader = short_reader(bytes.into(), Arc::new(AtomicUsize::new(0)));
    reader.cancel_after = Some((4, cancel.clone()));
    let options = ReaderOptions { cancel, ..ReaderOptions::default() };
    assert!(matches!(decode_reader(reader, None, options), Err(AudioError::Cancelled)));
}

/// A sparse recording with a >4GiB opaque chunk: no buffer of that size exists.
struct SparseRecording {
    position: u64,
    prefix: Vec<u8>,
    tail: Vec<u8>,
    tail_at: u64,
    len: u64,
    bytes_read: Arc<AtomicUsize>,
}

impl Read for SparseRecording {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = bytes.len().min(usize::try_from(self.len.saturating_sub(self.position)).unwrap_or(usize::MAX));
        for (index, byte) in bytes[..count].iter_mut().enumerate() {
            let at = self.position + index as u64;
            *byte = if at < self.prefix.len() as u64 {
                self.prefix[at as usize]
            } else if at >= self.tail_at {
                self.tail[(at - self.tail_at) as usize]
            } else {
                0xA5
            };
        }
        self.position += count as u64;
        self.bytes_read.fetch_add(count, Ordering::Relaxed);
        Ok(count)
    }
}

impl Seek for SparseRecording {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let at = match from {
            SeekFrom::Start(at) => i128::from(at),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
        };
        if at < 0 || at > i128::from(self.len) {
            return Err(io::Error::other("invalid sparse seek"));
        }
        self.position = at as u64;
        Ok(self.position)
    }
}

fn sparse_recording(bytes_read: Arc<AtomicUsize>, huge_audio: bool) -> SparseRecording {
    let mut prefix = b"RF64\xff\xff\xff\xffWAVE".to_vec();
    let mut ds64 = vec![0; 28];
    let data_size = if huge_audio { 8u64 * ((1u64 << 30) + 1) } else { 16 };
    ds64[8..16].copy_from_slice(&data_size.to_le_bytes());
    chunk(&mut prefix, b"ds64", &ds64);
    chunk(&mut prefix, b"fmt ", &fmt(48_000));
    if huge_audio {
        prefix.extend_from_slice(b"data\xff\xff\xff\xff");
        let tail_at = prefix.len() as u64 + data_size;
        return SparseRecording { position: 0, len: tail_at, prefix, tail: vec![], tail_at, bytes_read };
    }
    prefix.extend_from_slice(b"JUNK");
    prefix.extend_from_slice(&u32::MAX.to_le_bytes());
    let tail_at = prefix.len() as u64 + u64::from(u32::MAX) + 1;
    let mut tail = b"data\xff\xff\xff\xff".to_vec();
    for value in [1.25f32, -2.5, -0.0, 0.125] {
        tail.extend_from_slice(&value.to_le_bytes());
    }
    SparseRecording { position: 0, len: tail_at + tail.len() as u64, prefix, tail, tail_at, bytes_read }
}

#[test]
fn large_encoded_container_is_seeked_not_buffered_and_oversized_pcm_is_refused() {
    let bytes_read = Arc::new(AtomicUsize::new(0));
    let reader = sparse_recording(bytes_read.clone(), false);
    assert!(reader.len > u64::from(u32::MAX));
    let (info, actual) = decode_reader(reader, None, ReaderOptions::default()).unwrap();
    assert_eq!((info.sample_rate, info.channels, info.frames), (48_000, 2, 2));
    assert_pcm_bits(&actual, &[vec![1.25, -0.0], vec![-2.5, 0.125]]);
    assert!(bytes_read.load(Ordering::Relaxed) < 256);
    bytes_read.store(0, Ordering::Relaxed);
    assert!(matches!(decode_reader(sparse_recording(bytes_read.clone(), true), None, ReaderOptions::default()), Err(AudioError::TooLarge(_))));
    assert!(bytes_read.load(Ordering::Relaxed) < 256);
}
