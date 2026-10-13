//! Actual owned CLAP delivery through the paired Mix preparation and offline renderer.
//! The processor receipt and affine PCM are independent of CLAP's requested and Mix's applied caches.
#![cfg(not(target_arch = "wasm32"))]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::OfflineRenderer;
use soundcraft_model::{AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Session, SourceAudio, SourceId, TrackId, TrackKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const P: usize = 4096;
const ID: &str = "clap:org.soundcraft.test.capacity";

/// Build the unchanged owned fixture at this test's current source root, then seed only it.
/// This binary has one fixture type; add_search_dir initializes the registry without ambient discovery.
fn owned_fixture_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../clap-host/tests/capacity_fixture/Cargo.toml");
        let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mix-automation-hosted-capacity-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--quiet", "--offline", "--locked", "--jobs", "1", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env_remove("RUSTFLAGS")
            .status()
            .expect("build unchanged owned CLAP capacity fixture");
        assert!(status.success(), "owned CLAP capacity fixture build failed");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mix-automation-hosted-install-{}-{nonce}", std::process::id()));
        // Never remove a pre-existing path; creation establishes this process's ownership.
        std::fs::create_dir(&dir).expect("create unique owned fixture directory");
        let lib = if cfg!(target_os = "macos") {
            "libclap_capacity_fixture.dylib"
        } else if cfg!(windows) {
            "clap_capacity_fixture.dll"
        } else {
            "libclap_capacity_fixture.so"
        };
        let built = target.join("debug").join(lib);
        if cfg!(target_os = "macos") {
            let bin = dir.join("Capacity.clap/Contents/MacOS");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::copy(built, bin.join("Capacity")).unwrap();
        } else {
            std::fs::copy(built, dir.join("Capacity.clap")).unwrap();
        }
        assert_eq!(soundcraft_clap_host::add_search_dir(&dir), 1);
        let descriptors = soundcraft_clap_host::scan_paths(std::slice::from_ref(&dir));
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].id, ID);
        dir
    })
}

fn source(channel: usize, at: usize) -> f32 {
    if channel == 0 { 0.25 + at as f32 / 32.0 } else { -0.5 + at as f32 / 64.0 }
}

fn plain(id: usize) -> f32 {
    match id {
        0 => 1.0,
        1 => 1.75, // Choice rounds to two in the host setter.
        2 => 0.75, // Toggle quantizes to one in the host setter.
        4094 | 4095 => 0.0,
        _ => (id % 31) as f32 / 32.0,
    }
}

fn expected_values(gain: f32, bias: f32) -> Vec<f64> {
    (0..P)
        .map(|id| {
            f64::from(match id {
                0 => gain,
                1 => 2.0,
                2 => 1.0,
                4095 => bias,
                _ => plain(id),
            })
        })
        .collect()
}

fn fixture() -> (Session, TrackId) {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Owned hosted affine source"));
    let source_id = SourceId(901);
    let channels = (0..2).map(|ch| (0..8).map(|at| source(ch, at)).collect()).collect();
    session.pool.insert(source_id, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = session.new_clip_id();
    let t = session.track_mut(track).unwrap();
    t.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original binary fractions", source_id, 0, 0, 8));
    t.mixer.pan = vec![-1.0, 1.0];
    t.mixer.inserts[0] = Some(Insert::new(ID));
    assert!(t.mixer.inserts[0].as_ref().unwrap().params.is_empty(), "registered targets must not depend on stored keys");
    for id in 0..P {
        let points = match id {
            0 => vec![(0, 1.0), (1, 1.5), (2, 1.5), (3, 0.5), (4, 0.5)],
            4095 => vec![(0, 0.0), (1, 0.125), (2, 0.125), (3, 0.25), (4, 0.25)],
            4094 => vec![(0, 0.0), (1, 1.0), (2, 0.0), (3, 1.0), (4, 0.0)],
            _ => vec![(0, plain(id))],
        };
        let mut lane = AutomationLane::new(AutoParam::Plugin { slot: 0, param: id.to_string() });
        lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value }).collect();
        t.automation.push(lane);
    }
    (session, track)
}

#[derive(Debug, PartialEq)]
struct Receipt {
    values: Vec<f64>,
    stats: [u64; 4],
}

fn receipt(renderer: &mut OfflineRenderer, track: TrackId) -> Receipt {
    let states = renderer.mix_mut().capture_states();
    assert_eq!(states.len(), 1);
    assert_eq!((states[0].0, states[0].1), (track, 0));
    let bytes = &states[0].2;
    assert_eq!(bytes.len(), P * 8 + 32);
    let values = bytes[..P * 8].chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap())).collect();
    let stats = std::array::from_fn(|i| u64::from_le_bytes(bytes[P * 8 + i * 8..P * 8 + (i + 1) * 8].try_into().unwrap()));
    Receipt { values, stats }
}

fn successful_sample(renderer: &mut OfflineRenderer, at: usize, gain: f32, bias: f32) {
    let mut pcm = vec![vec![0.875; 1]; 2];
    renderer.render(at as i64, 1, &mut pcm).unwrap();
    assert_eq!(pcm.len(), 2);
    assert!(pcm.iter().all(|ch| ch.len() == 1 && ch[0].is_finite()));
    let expected: Vec<_> = (0..2).map(|ch| (f64::from(source(ch, at)) * f64::from(gain) + f64::from(bias)) as f32).collect();
    assert!(expected.iter().any(|v| v.abs() > 0.125), "independent affine reference must be audible");
    assert_eq!(pcm, expected.into_iter().map(|v| vec![v]).collect::<Vec<_>>());
}

fn refused_sample(renderer: &mut OfflineRenderer, at: usize) {
    let mut pcm = vec![vec![0.875; 1]; 2];
    assert!(renderer.render(at as i64, 1, &mut pcm).is_err(), "actual processor refusal must reach the Mix caller");
    assert_eq!(pcm.len(), 2);
    assert!(pcm.iter().all(|ch| ch.len() == 1 && ch[0].is_finite()));
    assert_eq!(pcm, vec![vec![0.0; 1]; 2], "failed output replaces nonzero sentinels with finite silence");
}

#[test]
fn prepared_mix_delivers_all_4096_and_retries_unacknowledged_failed_targets() {
    let _owned = owned_fixture_dir();
    let (session, track) = fixture();
    let mut renderer = OfflineRenderer::prepare(Arc::new(session.clone()), 64).unwrap();

    successful_sample(&mut renderer, 0, 1.0, 0.0);
    let initial = receipt(&mut renderer, track);
    assert_eq!(initial, Receipt { values: expected_values(1.0, 0.0), stats: [1, P as u64, 0, 0] });

    refused_sample(&mut renderer, 1);
    assert_eq!(receipt(&mut renderer, track), initial, "failed batch cannot become actual processor state");
    // Gain/bias equal the failed span. CLAP discards that queue, so only a Mix resend can deliver them.
    successful_sample(&mut renderer, 2, 1.5, 0.125);
    let first_retry = receipt(&mut renderer, track);
    assert_eq!(first_retry, Receipt { values: expected_values(1.5, 0.125), stats: [2, P as u64, 0, 0] });

    refused_sample(&mut renderer, 3);
    assert_eq!(receipt(&mut renderer, track), first_retry);
    // A newer failed target set must also be retried at the current absolute position.
    successful_sample(&mut renderer, 4, 0.5, 0.25);
    assert_eq!(receipt(&mut renderer, track), Receipt { values: expected_values(0.5, 0.25), stats: [3, P as u64, 0, 0] });

    successful_sample(&mut renderer, 5, 0.5, 0.25);
    assert_eq!(receipt(&mut renderer, track), Receipt { values: expected_values(0.5, 0.25), stats: [4, 0, 0, 0] });
    // A document-only reprepare retains the actual processor and its successfully delivered cache.
    let mut edited = session;
    edited.name.push_str(" — retained instance");
    renderer.reprepare(Arc::new(edited)).unwrap();
    successful_sample(&mut renderer, 6, 0.5, 0.25);
    assert_eq!(receipt(&mut renderer, track), Receipt { values: expected_values(0.5, 0.25), stats: [5, 0, 0, 0] });
}
