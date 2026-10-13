//! Original mathematical expectations; no production lane, pan, mixer or smoother helpers.
pub const LEN: usize = 389;
pub const VOLUME: &[(i64, f64)] = &[(13, -6.0), (37, 0.0), (93, -12.0), (94, 3.0), (173, -3.0), (211, -9.0), (367, 0.0)];
pub const PAN0: &[(i64, f64)] = &[(13, -1.0), (37, 0.75), (93, -0.25), (94, 1.0), (173, -0.5), (367, -1.0)];
pub const PAN1: &[(i64, f64)] = &[(13, 1.0), (37, -0.75), (93, 0.25), (94, -1.0), (173, 0.5), (367, 1.0)];
pub const MUTE: &[(i64, f64)] = &[(13, 0.0), (37, 1.0), (61, 0.0), (127, 1.0), (193, 0.0)];
pub const SEND_LEVEL: &[(i64, f64)] = &[(13, -144.0), (37, -6.0), (94, 3.0), (173, -12.0), (249, -3.0), (367, -6.0)];
pub const SEND_PAN: &[(i64, f64)] = &[(13, -1.0), (61, 0.25), (94, 1.0), (173, -0.5), (211, 0.0), (367, -1.0)];
pub const SEND_MUTE: &[(i64, f64)] = &[(13, 0.0), (61, 1.0), (94, 0.0), (173, 1.0), (211, 0.0)];
pub const PLUGIN_GAIN: &[(i64, f64)] = &[(13, -12.0), (37, 6.0), (93, -24.0), (94, 9.0), (173, -96.0), (211, -3.0), (367, 0.0)];

pub fn signal(channel: usize, frame: usize) -> f64 {
    let values = if (frame / 17).is_multiple_of(2) { [0.25, -0.125] } else { [-0.125, 0.375] };
    values[channel]
}

pub fn linear(points: &[(i64, f64)], at: i64) -> f64 {
    let first_time = points[0].0;
    if at < first_time {
        return points.iter().take_while(|p| p.0 == first_time).last().unwrap().1;
    }
    let mut left = points[0];
    for &point in points {
        if point.0 <= at {
            left = point;
            continue;
        }
        let fraction = (i128::from(at) - i128::from(left.0)) as f64 / (i128::from(point.0) - i128::from(left.0)) as f64;
        return left.1 + (point.1 - left.1) * fraction;
    }
    left.1
}

pub fn step(points: &[(i64, f64)], at: i64) -> bool {
    let first_time = points[0].0;
    let mut value = points.iter().take_while(|p| p.0 == first_time).last().unwrap().1;
    for &(time, next) in points {
        if time > at {
            break;
        }
        value = next;
    }
    value >= 0.5
}

pub fn db_gain(db: f64, off: f64) -> f64 {
    if db <= off { 0.0 } else { 10f64.powf(db / 20.0) }
}

// None is the center-preserving Zero/balance law. Other depths are mono constant-power laws.
pub fn pan(pan: f64, depth: Option<f64>) -> [f64; 2] {
    let p = pan.clamp(-1.0, 1.0);
    match depth {
        None => [(1.0 - p).min(1.0), (1.0 + p).min(1.0)],
        Some(db) => {
            let angle = (p + 1.0) * std::f64::consts::PI / 4.0;
            let exponent = db / -3.0103;
            [angle.cos().max(0.0).powf(exponent), angle.sin().max(0.0).powf(exponent)]
        }
    }
}

pub struct GainMemory {
    current: f64,
    target: f64,
}
impl GainMemory {
    pub fn new() -> Self {
        Self { current: 1.0, target: 1.0 }
    }
    pub fn reset(&mut self) {
        self.current = self.target;
    }
    pub fn next(&mut self, db: f64) -> f64 {
        self.target = db_gain(db.clamp(-96.0, 24.0), -96.0);
        let coefficient = (-1.0f64 / (0.020 * 48_000.0)).exp();
        if self.current != self.target {
            self.current = self.target + (self.current - self.target) * coefficient;
            if (self.current - self.target).abs() <= 1e-6 * (1.0 + self.target.abs()) {
                self.current = self.target;
            }
        }
        self.current
    }
}

pub fn check(actual: &[Vec<f32>], expected: &[Vec<f64>], channels: usize, frames: usize, tolerance: f64) {
    assert_eq!(actual.len(), channels);
    assert_eq!(expected.len(), channels);
    for (channel, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(actual.len(), frames);
        assert_eq!(expected.len(), frames);
        for (frame, (&a, &e)) in actual.iter().zip(expected).enumerate() {
            assert!(a.is_finite() && e.is_finite());
            assert!((f64::from(a) - e).abs() < tolerance, "channel {channel}, frame {frame}: {a} vs {e}");
        }
    }
    assert!(expected.iter().flatten().any(|x| x.abs() > 0.02) || expected.iter().flatten().all(|x| *x == 0.0));
}

pub fn different(a: &[Vec<f64>], b: &[Vec<f64>], threshold: f64) {
    assert_eq!(a.len(), b.len());
    assert!(a.iter().zip(b).all(|(a, b)| a.len() == b.len()));
    assert!(a.iter().flatten().zip(b.iter().flatten()).any(|(a, b)| (a - b).abs() > threshold));
}
