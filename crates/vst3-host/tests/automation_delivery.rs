//! Processor-side receipts and audible affine output, using explicit owned fixture discovery.
use soundcraft_dsp::{AutomationPrepareError, ParamTarget, Plugin, ProcessDelivery};
mod capacity_common;
const P: usize = 4096;
fn create() -> Box<dyn Plugin> {
    let dir = capacity_common::fixture_dir();
    let found = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Capacity Affine");
    let mut p = soundcraft_vst3_host::create(&found[0].id).unwrap();
    p.prepare(48_000.0, 64, 2);
    assert_eq!(p.info().params.len(), P);
    p.prepare_automation(P, 1024).unwrap();
    p
}
fn receipt(p: &mut dyn Plugin) -> (Vec<f64>, [u64; 4]) {
    let blob = p.save_state().unwrap();
    assert_eq!(&blob[..4], b"SCV3");
    let len = u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize;
    let blob = &blob[8..8 + len];
    assert_eq!(blob.len(), P * 8 + 32);
    let values = blob[..P * 8].chunks_exact(8).map(|b| f64::from_le_bytes(b.try_into().unwrap())).collect();
    let stats = std::array::from_fn(|i| u64::from_le_bytes(blob[P * 8 + i * 8..P * 8 + (i + 1) * 8].try_into().unwrap()));
    (values, stats)
}
fn io() -> Vec<Vec<f32>> {
    vec![vec![0.25; 64], vec![-0.5; 64]]
}
#[test]
fn all_registered_params_and_full_note_budget_reach_the_processor() {
    let mut p = create();
    let ids: Vec<String> = (0..P).map(|i| i.to_string()).collect();
    let mut plain: Vec<f32> = (0..P).map(|i| (i % 31) as f32 / 32.0).collect();
    plain[0] = 1.5;
    plain[1] = 2.0;
    plain[2] = 1.0;
    plain[4094] = 0.0;
    plain[4095] = 0.125;
    let targets: Vec<_> = ids.iter().zip(&plain).map(|(id, &value)| ParamTarget { id, value }).collect();
    for i in 0..512 {
        p.note_on(i % 64, 60, 127);
        p.note_off(i % 64, 60);
    }
    let mut audio = io();
    assert_eq!(p.process_with_targets(&mut audio, 64, &targets), ProcessDelivery::Delivered);
    assert!(audio[0].iter().all(|&x| x == 0.5));
    assert!(audio[1].iter().all(|&x| x == -0.625));
    let (values, stats) = receipt(&mut *p);
    assert_eq!(stats, [1, P as u64, 1024, 2 * (0..512u64).map(|i| i % 64).sum::<u64>()]);
    for (i, (&actual, &expected)) in values.iter().zip(&plain).enumerate() {
        let normalized = if i <= 1 { f64::from(expected) / 2.0 } else { f64::from(expected) };
        assert_eq!(actual, normalized, "processor ID{i}");
    }
    // Re-preparing must resend all P, including the final ID and restored static values.
    p.prepare(48_000.0, 64, 2);
    let mut audio = io();
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::Delivered);
    assert_eq!(receipt(&mut *p).1[1], P as u64);
    assert_eq!(audio[0][0], 0.5);
    let state = p.save_state().unwrap();
    let mut restored = create();
    assert!(restored.load_state(&state));
    // Preparation after state load must preserve/resend the full actual restored catalog.
    restored.prepare(48_000.0, 64, 2);
    let mut audio = io();
    assert_eq!(restored.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::Delivered);
    let restored_receipt = receipt(&mut *restored);
    assert_eq!(restored_receipt.0, values);
    assert_eq!(restored_receipt.1, [1, P as u64, 0, 0]);
    assert_eq!(audio[0][0], 0.5);
    assert_eq!(audio[1][0], -0.625);
}
#[test]
fn over_capacity_and_bad_span_fail_before_processor_delivery() {
    let mut p = create();
    let mut audio = io();
    assert_eq!(p.prepare_automation(P + 1, 0), Err(AutomationPrepareError::CapacityExceeded));
    assert_eq!(p.prepare_automation(P, 1025), Err(AutomationPrepareError::CapacityExceeded));
    let too_many = vec![ParamTarget { id: "0", value: 1.0 }; P + 1];
    assert_eq!(p.process_with_targets(&mut audio, 64, &too_many), ProcessDelivery::CapacityExceeded);
    assert_eq!(receipt(&mut *p).1[0], 0);
    p.prepare_automation(P, 2).unwrap();
    p.note_on(0, 60, 127);
    p.note_off(1, 60);
    p.note_on(2, 60, 127);
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::CapacityExceeded);
    assert_eq!(receipt(&mut *p).1[0], 0);
    p.note_on(64, 60, 127);
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::InvalidTarget);
    assert_eq!(receipt(&mut *p).1[0], 0);
    assert_eq!(p.process_with_targets(&mut [vec![1.0; 63]], 64, &[]), ProcessDelivery::ProcessorFailed);
    assert_eq!(receipt(&mut *p).1[0], 0);
    // Failed batches are discarded; retry contains current targets and only valid new notes.
    p.note_on(7, 61, 127);
    assert_eq!(
        p.process_with_targets(&mut audio, 64, &[ParamTarget { id: "0", value: 1.5 }, ParamTarget { id: "4095", value: 0.125 }]),
        ProcessDelivery::Delivered
    );
    assert_eq!(receipt(&mut *p).1, [1, 2, 1, 7]);
    assert_eq!(audio[0][0], 0.5);
}
#[test]
fn processor_error_is_not_cached_success_and_latest_target_retries() {
    let mut p = create();
    let mut audio = io();
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::Delivered);
    let before = receipt(&mut *p);
    let failed = [ParamTarget { id: "0", value: 1.75 }, ParamTarget { id: "4095", value: 0.5 }, ParamTarget { id: "4094", value: 1.0 }];
    assert_eq!(p.process_with_targets(&mut audio, 64, &failed), ProcessDelivery::ProcessorFailed);
    assert_eq!(p.param("0"), Some(1.75), "requested is distinct from delivered");
    assert_eq!(receipt(&mut *p), before, "actual processor state is unchanged by failure");
    audio = io();
    let latest = [ParamTarget { id: "0", value: 0.5 }, ParamTarget { id: "4095", value: 0.125 }, ParamTarget { id: "4094", value: 0.0 }];
    assert_eq!(p.process_with_targets(&mut audio, 64, &latest), ProcessDelivery::Delivered);
    assert_eq!(audio[0][0], 0.25);
    assert_eq!(audio[1][0], -0.125);
    assert_eq!(receipt(&mut *p).1, [2, 3, 0, 0]);
}
#[test]
fn exact_ids_finite_prevalidation_and_plain_choice_toggle_quantization() {
    let mut p = create();
    let mut audio = io();
    let invalid = [ParamTarget { id: "0", value: 1.5 }, ParamTarget { id: "01", value: 1.0 }];
    assert_eq!(p.process_with_targets(&mut audio, 64, &invalid), ProcessDelivery::InvalidTarget);
    assert_eq!(p.param("0"), Some(1.0));
    assert_eq!(receipt(&mut *p).1[0], 0);
    assert_eq!(p.process_with_targets(&mut audio, 64, &[ParamTarget { id: "0", value: f32::NAN }]), ProcessDelivery::InvalidTarget);
    assert_eq!(receipt(&mut *p).1[0], 0);
    for (plain, choice, toggle) in [(0.25, 0.0, 0.0), (0.75, 1.0, 1.0), (1.75, 2.0, 1.0)] {
        let target = [ParamTarget { id: "1", value: plain }, ParamTarget { id: "2", value: plain }];
        assert_eq!(p.process_with_targets(&mut audio, 64, &target), ProcessDelivery::Delivered);
        let values = receipt(&mut *p).0;
        assert_eq!(values[1], choice / 2.0);
        assert_eq!(values[2], toggle);
    }
}

// Instrumentation is an isolated test-only dependency; this host keeps unsafe_code=deny.
#[global_allocator]
static ALLOCATOR: test_allocation_counter::CountingSystem = test_allocation_counter::CountingSystem;
#[test]
fn prepared_controlled_callback_has_no_per_span_allocation() {
    // Independent positive/zero controls prevent a disabled counter from proving the callback.
    let (allocation, positive) = test_allocation_counter::measure(|| std::hint::black_box(vec![std::hint::black_box(7u8); 128]));
    assert!(positive > 0);
    assert_eq!(allocation.len(), 128);
    let (value, zero) = test_allocation_counter::measure(|| std::hint::black_box(3) + std::hint::black_box(4));
    assert_eq!((value, zero), (7, 0));
    let mut p = create();
    let mut audio = io();
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::Delivered);
    let targets = [ParamTarget { id: "0", value: 1.5 }, ParamTarget { id: "4095", value: 0.125 }];
    p.note_on(3, 60, 127);
    p.note_off(12, 60);
    let (status, allocations) = test_allocation_counter::measure(|| p.process_with_targets(&mut audio, 64, &targets));
    assert_eq!(status, ProcessDelivery::Delivered);
    assert_eq!(allocations, 0);
    assert_eq!(audio[0][0], 0.5);
    let ((failed, retry), allocations) = test_allocation_counter::measure(|| {
        let failed = p.process_with_targets(&mut audio, 64, &[ParamTarget { id: "4094", value: 1.0 }]);
        let retry = p.process_with_targets(&mut audio, 64, &[ParamTarget { id: "4094", value: 0.0 }]);
        (failed, retry)
    });
    assert_eq!(failed, ProcessDelivery::ProcessorFailed);
    assert_eq!(retry, ProcessDelivery::Delivered);
    assert_eq!(allocations, 0, "refusal and retry also allocate nothing on the callback thread");
}

#[test]
fn explicit_control_seeds_survive_first_prepare_and_capacity_preparation() {
    let dir = capacity_common::fixture_dir();
    let found = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Capacity Affine");
    let mut p = soundcraft_vst3_host::create(&found[0].id).unwrap();
    assert_eq!(p.info().params.len(), P);
    assert!(p.set_param("0", 1.5));
    assert!(p.set_param("4095", 0.125));
    p.prepare(48_000.0, 64, 2);
    p.prepare_automation(P, 1024).unwrap();
    p.prepare_automation(P, 1024).unwrap();
    let mut audio = io();
    assert_eq!(p.process_with_targets(&mut audio, 64, &[]), ProcessDelivery::Delivered);
    assert_eq!(audio.len(), 2);
    assert!(audio.iter().all(|channel| channel.len() == 64));
    assert!(audio[0].iter().all(|&x| x == 0.5));
    assert!(audio[1].iter().all(|&x| x == -0.625));
    let (values, stats) = receipt(&mut *p);
    assert_eq!(stats, [1, 2, 0, 0]);
    assert_eq!(values[0], 0.75);
    assert_eq!(values[4095], 0.125);
}
