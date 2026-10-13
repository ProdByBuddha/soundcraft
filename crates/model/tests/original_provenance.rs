use std::path::PathBuf;
use std::sync::Arc;

use soundcraft_audio_io::{AudioBuffer, AudioInfo, BwfInfo, FileFormat, SampleFormat};
use soundcraft_model::{ImportPolicy, OriginalAssetLease, OriginalProvenance, Source, SourceAudio, SourceId, SourcePool};

fn original(rate: u32, raw: u64) -> OriginalProvenance {
    OriginalProvenance {
        version: 1,
        policy: ImportPolicy::Copy,
        path: "Originals/recording.wav".into(),
        sha256: "encoded-original-sha256".into(),
        bytes: 1234,
        info: AudioInfo {
            format: FileFormat::Wav,
            sample_format: SampleFormat::Int24,
            sample_rate: rate,
            channels: 2,
            frames: 99,
            bwf: Some(BwfInfo { time_reference: raw, ..BwfInfo::default() }),
        },
    }
}

#[test]
fn original_rate_counter_and_checked_timeline_conversion_are_distinct() {
    let raw = (1u64 << 40) + 12345;
    for (rate, expected) in [(44_100, 1_196_747_363_397i64), (48_000, 1_099_511_640_121), (96_000, 549_755_820_061)] {
        let source = original(rate, raw);
        let before = source.clone();
        assert_eq!(source.timeline_time_reference(48_000), Ok(Some(expected)));
        assert_eq!(source, before);
        assert_eq!(source.info.bwf.unwrap().time_reference, raw);
    }
    assert!(original(44_100, u64::MAX).timeline_time_reference(48_000).is_err());
    assert!(original(0, 99).timeline_time_reference(48_000).is_err());
    assert!(original(48_000, 99).timeline_time_reference(0).is_err());
    let mut no_bwf = original(48_000, 99);
    no_bwf.info.bwf = None;
    assert_eq!(no_bwf.timeline_time_reference(48_000), Ok(None));
    assert_eq!(original(96_000, 1).timeline_time_reference(48_000), Ok(Some(1)));
}

#[test]
fn legacy_sources_omit_provenance_and_explicit_policies_roundtrip() {
    let legacy = r#"{"id":17,"name":"legacy","path":"Audio Files/legacy.wav","channels":2,"frames":99,"sample_rate":48000,"format":"Wav","time_reference":1099511640121}"#;
    let mut source: Source = serde_json::from_str(legacy).unwrap();
    assert!(source.original.is_none());
    assert_eq!(source.time_reference, (1u64 << 40) + 12345);
    assert!(serde_json::to_value(&source).unwrap().get("original").is_none());
    for (policy, name) in [(ImportPolicy::Copy, "copy"), (ImportPolicy::Reference, "reference"), (ImportPolicy::Convert, "convert")] {
        let mut provenance = original(44_100, (1u64 << 40) + 12345);
        provenance.policy = policy;
        source.original = Some(provenance);
        let value = serde_json::to_value(&source).unwrap();
        assert_eq!(value["original"]["policy"], name);
        assert_eq!(serde_json::from_value::<Source>(value).unwrap(), source);
    }
}

#[test]
fn source_pool_snapshots_keep_managed_original_lease_without_drop_io() {
    let lease = Arc::new(OriginalAssetLease { path: PathBuf::from("managed-original-that-model-does-not-open.wav") });
    let weak = Arc::downgrade(&lease);
    let audio = Arc::new(SourceAudio::with_original_asset(AudioBuffer::new(48_000, 1, 7), lease.clone()));
    assert!(Arc::ptr_eq(audio.original_asset.as_ref().unwrap(), &lease));
    assert!(SourceAudio::new(AudioBuffer::new(48_000, 1, 7)).original_asset.is_none());
    let mut pool = SourcePool::default();
    pool.insert(SourceId(17), audio.clone());
    assert!(Arc::ptr_eq(pool.original_asset(SourceId(17)).unwrap(), &lease));
    let snapshot = pool.clone();
    drop(audio);
    drop(lease);
    pool.remove(SourceId(17));
    assert!(pool.original_asset(SourceId(17)).is_some());
    pool.remove_original(SourceId(17));
    assert!(pool.original_asset(SourceId(17)).is_none());
    assert!(weak.upgrade().is_some());
    assert_eq!(
        snapshot.get(SourceId(17)).unwrap().original_asset.as_ref().unwrap().path,
        PathBuf::from("managed-original-that-model-does-not-open.wav")
    );
    drop(snapshot);
    assert!(weak.upgrade().is_none());
}

#[test]
fn unresolved_derivative_original_survives_pool_clone_and_requires_explicit_release() {
    let id = SourceId(23);
    let lease = Arc::new(OriginalAssetLease { path: PathBuf::from("verified-original-with-missing-derivative.wav") });
    let weak = Arc::downgrade(&lease);
    let mut pool = SourcePool::default();
    pool.retain_original(id, lease.clone());
    assert!(!pool.contains(id));
    assert!(pool.is_empty());
    assert_eq!(pool.ids().count(), 0);
    assert!(Arc::ptr_eq(pool.original_asset(id).unwrap(), &lease));
    let snapshot = pool.clone();
    assert_eq!(pool, snapshot);
    assert_ne!(pool, SourcePool::default());
    let mut different_identity = SourcePool::default();
    different_identity.retain_original(id, Arc::new(OriginalAssetLease { path: lease.path.clone() }));
    assert_ne!(pool, different_identity);
    drop(different_identity);
    drop(lease);
    assert!(pool.remove(id).is_none());
    assert_eq!(pool, snapshot);
    pool.remove_original(id);
    assert_ne!(pool, snapshot);
    assert!(weak.upgrade().is_some());
    drop(snapshot);
    assert!(weak.upgrade().is_none());
}
