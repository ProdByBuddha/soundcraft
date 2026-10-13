use soundcraft_dsp::offline::resample_to_frames_cancellable;

#[test]
fn conversion_preserves_independent_tone_frequency_and_channel_scale() {
    let input: Vec<f32> = (0..4410).map(|n| (std::f64::consts::TAU * 1000.0 * n as f64 / 44100.0).sin() as f32 * 0.4).collect();
    let stereo = vec![input.clone(), input.iter().map(|value| -0.5 * value).collect()];
    let output = resample_to_frames_cancellable(&stereo, 44100, 48000, 4800, || false).unwrap();
    assert_eq!(output.len(), 2);
    assert!(output.iter().all(|channel| channel.len() == 4800));
    for (n, (left, right)) in output[0].iter().zip(&output[1]).enumerate().take(4700).skip(100) {
        let expected = (std::f64::consts::TAU * 1000.0 * n as f64 / 48000.0).sin() as f32 * 0.4;
        assert!((*left - expected).abs() < 0.0002, "sample {n}");
        assert!((*right + expected * 0.5).abs() < 0.0001, "right sample {n}");
    }
}

#[test]
fn cancellation_is_observed_during_kernel_and_output_work() {
    let input = vec![vec![0.25; 4410]];
    let mut calls = 0;
    let result = resample_to_frames_cancellable(&input, 44100, 48000, 4800, || {
        calls += 1;
        calls == 3
    });
    assert_eq!(result.unwrap_err(), "sample-rate conversion cancelled");
    assert_eq!(calls, 3);
    // At this ratio the kernel uses 66 checks. Cancellation after check 70 occurs during
    // output production, while a complete channel needs another 19 output checks.
    let mut calls = 0;
    let result = resample_to_frames_cancellable(&input, 44100, 48000, 4800, || {
        calls += 1;
        calls == 72
    });
    assert_eq!(result.unwrap_err(), "sample-rate conversion cancelled");
    assert_eq!(calls, 72);
}

#[test]
fn equal_rate_copies_exact_bits_and_can_cancel_mid_copy() {
    let values = [0.0f32, -0.0, 1.5, -2.25, f32::from_bits(1)];
    let input = vec![values.repeat(300)];
    let output = resample_to_frames_cancellable(&input, 48000, 48000, 1500, || false).unwrap();
    assert_eq!(output[0].iter().map(|v| v.to_bits()).collect::<Vec<_>>(), input[0].iter().map(|v| v.to_bits()).collect::<Vec<_>>());
    let mut calls = 0;
    let result = resample_to_frames_cancellable(&input, 48000, 48000, 1500, || {
        calls += 1;
        calls == 5
    });
    assert_eq!(result.unwrap_err(), "sample-rate conversion cancelled");
    assert_eq!(calls, 5);
}

#[test]
fn invalid_shape_rate_and_requested_length_are_refused() {
    let input = vec![vec![0.0; 64]];
    for (from, to, frames) in [(0, 48000, 64), (48000, 900000, 64), (48000, 48000, 63), (48000, 44100, 0), (48000, 44100, usize::MAX)] {
        assert!(resample_to_frames_cancellable(&input, from, to, frames, || false).is_err());
    }
    assert!(resample_to_frames_cancellable(&[vec![0.0; 64], vec![0.0; 63]], 48000, 44100, 59, || false).is_err());
}
