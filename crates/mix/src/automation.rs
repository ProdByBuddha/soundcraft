//! Immutable, bounded automation prepared on a control/offline thread.

use soundcraft_model::{AutoParam, AutomationMode, AutomationPoint, Track};
use soundcraft_time::Samples;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const MAX_PLAN_POINTS: usize = 1 << 22;

#[derive(Clone)]
pub(crate) struct Curve {
    points: Arc<[AutomationPoint]>,
    held: bool,
}

impl Curve {
    pub(crate) fn value(&self, at: Samples, base: f32) -> f32 {
        let Some(first) = self.points.first() else { return base };
        if at < first.at {
            return first.value;
        }
        let i = self.points.partition_point(|p| p.at <= at);
        let Some(a) = i.checked_sub(1).and_then(|i| self.points.get(i)) else { return base };
        let Some(b) = self.points.get(i) else { return a.value };
        if self.held || b.at <= a.at {
            return a.value;
        }
        let ratio = (i128::from(at) - i128::from(a.at)) as f64 / (i128::from(b.at) - i128::from(a.at)) as f64;
        (f64::from(a.value) + (f64::from(b.value) - f64::from(a.value)) * ratio) as f32
    }
}

#[derive(Clone, Default)]
pub(crate) struct SendAutomation {
    pub(crate) level: Option<Curve>,
    pub(crate) pan: Option<Curve>,
    pub(crate) mute: Option<Curve>,
}

#[derive(Clone)]
pub(crate) struct TrackAutomation {
    pub(crate) mode: AutomationMode,
    volume: Option<Curve>,
    pan: [Option<Curve>; 2],
    mute: Option<Curve>,
    pub(crate) sends: Vec<SendAutomation>,
    trim: Option<Curve>,
    pub(crate) plugins: BTreeMap<(u8, String), Curve>,
}

impl Default for TrackAutomation {
    fn default() -> Self {
        Self { mode: AutomationMode::Read, volume: None, pan: [None, None], mute: None, sends: Vec::new(), trim: None, plugins: BTreeMap::new() }
    }
}

impl TrackAutomation {
    pub(crate) fn compile(t: &Track, aggregate: &mut usize) -> Result<Self, &'static str> {
        let mut out = Self { mode: t.mixer.automation_mode, sends: vec![SendAutomation::default(); soundcraft_model::SEND_SLOTS], ..Self::default() };
        for lane in &t.automation {
            *aggregate = aggregate.checked_add(lane.points.len()).ok_or("automation point count overflow")?;
            if *aggregate > MAX_PLAN_POINTS {
                return Err("automation plan exceeds the point limit");
            }
            if lane.points.is_empty() {
                continue;
            }
            if lane.points.iter().any(|p| !p.value.is_finite()) {
                return Err("automation contains a nonfinite value");
            }
            let mut points = lane.points.clone();
            let (lo, hi, _) = lane.param.range();
            for point in &mut points {
                point.value = point.value.clamp(lo, hi);
            }
            points.sort_by_key(|p| p.at);
            // A later equal-time point wins, including at the first timestamp.
            let mut canonical: Vec<AutomationPoint> = Vec::with_capacity(points.len());
            for point in points {
                if let Some(last) = canonical.last_mut().filter(|p| p.at == point.at) {
                    *last = point;
                } else {
                    canonical.push(point);
                }
            }
            let curve = Curve { points: canonical.into(), held: matches!(lane.param, AutoParam::Mute | AutoParam::SendMute(_)) };
            // Legacy enabled=false lanes remain readable; changing that policy is a separate migration.
            match &lane.param {
                AutoParam::Volume => out.volume.get_or_insert(curve),
                AutoParam::Pan(i) => {
                    if let Some(p) = out.pan.get_mut(usize::from(*i)) {
                        p.get_or_insert(curve);
                    }
                    continue;
                }
                AutoParam::Mute => out.mute.get_or_insert(curve),
                AutoParam::SendLevel(i) | AutoParam::SendPan(i) | AutoParam::SendMute(i) => {
                    if let Some(send) = out.sends.get_mut(usize::from(*i)) {
                        match lane.param {
                            AutoParam::SendLevel(_) => {
                                send.level.get_or_insert(curve);
                            }
                            AutoParam::SendPan(_) => {
                                send.pan.get_or_insert(curve);
                            }
                            _ => {
                                send.mute.get_or_insert(curve);
                            }
                        }
                    }
                    continue;
                }
                AutoParam::Plugin { slot, param } if *slot == u8::MAX && param == "trim" => out.trim.get_or_insert(curve),
                AutoParam::Plugin { slot, param } => {
                    out.plugins.entry((*slot, param.clone())).or_insert(curve);
                    continue;
                }
            };
        }
        Ok(out)
    }

    pub(crate) fn volume(&self, base: f32, at: Samples) -> f32 {
        if self.mode.reads() && self.mode != AutomationMode::Write { self.volume.as_ref().map_or(base, |c| c.value(at, base)) } else { base }
    }

    pub(crate) fn pan(&self, index: usize, base: f32, at: Samples) -> f32 {
        if self.mode.reads() { self.pan.get(index).and_then(Option::as_ref).map_or(base, |c| c.value(at, base)).clamp(-1.0, 1.0) } else { base }
    }

    pub(crate) fn mute(&self, at: Samples) -> bool {
        self.mode.reads() && self.mute.as_ref().is_some_and(|c| c.value(at, 0.0) >= 0.5)
    }

    pub(crate) fn trim(&self, at: Samples) -> f32 {
        self.trim.as_ref().map_or(0.0, |c| c.value(at, 0.0))
    }

    pub(crate) fn send_value(&self, curve: Option<&Curve>, base: f32, at: Samples) -> f32 {
        if self.mode.reads() { curve.map_or(base, |c| c.value(at, base)) } else { base }
    }
}

pub(crate) fn position(start: Samples, frame: usize) -> Samples {
    start.saturating_add(i64::try_from(frame).unwrap_or(i64::MAX))
}
