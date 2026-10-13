//! Paired immutable document/automation snapshots and control-thread workspaces.

use super::*;
use crate::automation::{Curve, TrackAutomation};
use soundcraft_dsp::{ParamInfo, ParamTarget, ProcessDelivery};

const MAX_WORKSPACE_BYTES: usize = 128 << 20;

// A conservative admission budget for prepared adapter storage. Processor-owned DSP
// storage, allocator bookkeeping, and the caller's immutable document are outside this policy.
#[derive(Default)]
struct WorkspaceBudget(usize);

impl WorkspaceBudget {
    fn charge(&mut self, count: usize, size: usize) -> Result<(), PreparationError> {
        self.0 = count.checked_mul(size).and_then(|bytes| self.0.checked_add(bytes)).ok_or(PreparationError("workspace size overflow"))?;
        if self.0 > MAX_WORKSPACE_BYTES {
            return Err(PreparationError("mix workspace exceeds capacity"));
        }
        Ok(())
    }
}

fn track_plan_budget(t: &Track, budget: &mut WorkspaceBudget, points: bool) -> Result<(), PreparationError> {
    budget.charge(1, size_of::<TrackAutomation>() + 128)?;
    budget.charge(soundcraft_model::SEND_SLOTS, size_of::<crate::automation::SendAutomation>())?;
    for lane in &t.automation {
        // Covers a curve/Arc and a conservatively charged map node, even for ignored lanes.
        budget.charge(1, size_of::<Curve>() + 256)?;
        if let AutoParam::Plugin { param, .. } = &lane.param {
            budget.charge(param.len(), 1)?;
        }
        if points {
            // Sorting/canonicalization and Vec -> Arc may temporarily own two arrays.
            budget.charge(lane.points.len(), size_of::<soundcraft_model::AutomationPoint>() * 2)?;
        }
    }
    Ok(())
}

fn plan_budget(source: &Session) -> Result<WorkspaceBudget, PreparationError> {
    let mut budget = WorkspaceBudget::default();
    budget.charge(1, size_of::<PreparedSession>() + 64)?;
    let mut points = 0usize;
    for track in &source.tracks {
        track_plan_budget(track, &mut budget, true)?;
        for lane in &track.automation {
            points = points.checked_add(lane.points.len()).ok_or(PreparationError("automation point count overflow"))?;
            if points > crate::automation::MAX_PLAN_POINTS {
                return Err(PreparationError("automation plan exceeds the point limit"));
            }
        }
    }
    Ok(budget)
}

fn slot_budget(ins: &soundcraft_model::Insert, budget: &mut WorkspaceBudget, ch: usize, block: usize) -> Result<(), PreparationError> {
    let parameters = soundcraft_dsp::plugin_info(&ins.plugin).map_or(soundcraft_dsp::MAX_AUTOMATION_PARAMS, |info| info.params.len());
    if parameters > soundcraft_dsp::MAX_AUTOMATION_PARAMS {
        return Err(PreparationError("plugin parameter catalog exceeds capacity"));
    }
    budget.charge(1, size_of::<PluginSlot>() + size_of::<RetainedSlot>() + size_of::<SlotSeed>() + 256)?;
    budget.charge(ins.plugin.len(), 2)?;
    // Capturing Vec<ParamSeed> into Arc<[ParamSeed]> can temporarily own both arrays.
    budget.charge(parameters, 2 * size_of::<ParamSeed>() + size_of::<Binding>() + size_of::<ParamTarget<'static>>() + 2 * size_of::<f32>())?;
    budget.charge(ch, size_of::<Vec<f32>>())?;
    budget.charge(ch.checked_mul(block).ok_or(PreparationError("plugin workspace overflow"))?, size_of::<f32>())?;
    if is_third_party(&ins.plugin) {
        // Queue/COM event adapter estimates include both pending and FFI layers, initial
        // metadata/value caches, and bounded editor/output queues. This is not vendor RAM.
        budget.charge(parameters, 512)?;
        budget.charge(soundcraft_dsp::MAX_AUTOMATION_NOTES, 256)?;
        budget.charge(1, 64 << 10)?;
    }
    Ok(())
}

fn workspace_budget(source: &Session, block: usize) -> Result<WorkspaceBudget, PreparationError> {
    let mut budget = plan_budget(source)?;
    let main_ch = main_channels(source);
    budget.charge(1, size_of::<MixEngine>() + 128)?;
    budget.charge(main_ch.checked_mul(block).ok_or(PreparationError("workspace size overflow"))?, size_of::<f32>())?;
    budget.charge(main_ch, size_of::<Vec<f32>>() + size_of::<f32>())?;
    for track in &source.tracks {
        let ch = strip_channels(track, main_ch);
        // Hash-map capacity and the separately reserved parallel-work vector both hold
        // strip-sized storage, even while the live values move between them.
        budget.charge(1, 3 * size_of::<Strip>() + size_of::<StripMeter>() + 512)?;
        track_plan_budget(track, &mut budget, false)?;
        if let Some(vca) = track.mixer.vca.and_then(|id| source.tracks.iter().find(|vca| vca.id.0 == id && vca.kind == TrackKind::Vca)) {
            track_plan_budget(vca, &mut budget, false)?;
        }
        budget.charge(ch.checked_mul(block).ok_or(PreparationError("workspace size overflow"))?, 28)?;
        budget.charge(ch, 8 * size_of::<Vec<f32>>() + size_of::<f32>())?;
        budget.charge(128, size_of::<u8>())?;
        budget.charge(track.mixer.inserts.len(), size_of::<Option<PluginSlot>>())?;
        for ins in track.mixer.inserts.iter().flatten() {
            slot_budget(ins, &mut budget, ch, block)?;
        }
        if let Some(ins) = &track.instrument {
            slot_budget(ins, &mut budget, ch.max(2), block)?;
        }
        for clip in track.clips() {
            // Preflight exact matching stored keys without constructing parameter strings.
            let mut effect = false;
            for key in source.edit.values.keys() {
                if let Some((id, suffix)) = key.strip_prefix("clip_fx.").and_then(|key| key.split_once('.'))
                    && id.parse::<u64>().ok() == Some(clip.id.0)
                {
                    if !effect {
                        budget.charge(1, size_of::<ClipFx>() + 128)?;
                        effect = true;
                    }
                    // Collection plus retained configuration, with capacity-growth margin.
                    budget.charge(4, size_of::<(String, f32)>())?;
                    budget.charge(suffix.len(), 2)?;
                }
            }
        }
    }
    for bus in &source.busses {
        let ch = bus.format.channels().clamp(1, MAX_CHANNELS);
        budget.charge(1, size_of::<BusBuf>() + 128)?;
        budget.charge(ch, size_of::<Vec<f32>>())?;
        budget.charge(ch.checked_mul(block).ok_or(PreparationError("bus workspace overflow"))?, size_of::<f32>())?;
    }
    Ok(budget)
}

/// Every latency-producing clip chain and dry peer is aligned to the track's maximum.
/// Ring storage is admitted before allocation; effect-owned DSP storage is separate.
fn prepare_clip_alignment(track: &Track, strip: &mut Strip, enabled: bool, rate: f32, budget: &mut WorkspaceBudget) -> Result<(), PreparationError> {
    let mut maximum = 0usize;
    if enabled && track.kind == TrackKind::Audio {
        for fx in strip.clip_fx.values() {
            maximum = maximum.max(fx.latency()?);
        }
    }
    strip.clip_latency = maximum;
    if maximum > 0 {
        for clip in track.clips().iter().filter(|clip| clip.is_audio()) {
            if !strip.clip_fx.contains_key(&clip.id.0) {
                budget.charge(1, size_of::<ClipFx>() + 128)?;
                strip.clip_fx.insert(clip.id.0, ClipFx::new(rate, strip.buf.len()));
            }
        }
    }
    for fx in strip.clip_fx.values_mut() {
        let delay = if enabled { maximum.checked_sub(fx.latency()?).ok_or(PreparationError("ClipFX arrival mismatch"))? } else { 0 };
        budget.charge(delay.checked_mul(fx.ch).ok_or(PreparationError("ClipFX workspace overflow"))?, size_of::<f32>())?;
        budget.charge(fx.ch, size_of::<Vec<f32>>())?;
        if fx.align.bufs.len() != fx.ch || fx.align.bufs.iter().any(|channel| channel.len() < delay) {
            fx.align.reserve(delay, fx.ch)?;
        }
        if !fx.align.set(delay, fx.ch) {
            return Err(PreparationError("ClipFX alignment exceeds prepared capacity"));
        }
    }
    Ok(())
}

/// Preparation/rendering failures contain no plugin-owned strings or callback allocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparationError(pub &'static str);

impl std::fmt::Display for PreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PreparationError {}

/// A document and its compiled curves are always installed together.
pub struct PreparedSession {
    source: Arc<Session>,
    tracks: HashMap<TrackId, TrackAutomation>,
    lifetime: u64,
}

impl PreparedSession {
    pub fn source(&self) -> &Arc<Session> {
        &self.source
    }
    pub fn prepare(source: Arc<Session>, lifetime: u64) -> Result<Arc<Self>, PreparationError> {
        let _budget = plan_budget(&source)?;
        let mut aggregate = 0;
        let mut tracks = HashMap::with_capacity(source.tracks.len());
        for t in &source.tracks {
            if tracks.insert(t.id, TrackAutomation::compile(t, &mut aggregate).map_err(PreparationError)?).is_some() {
                return Err(PreparationError("duplicate track identity"));
            }
        }
        Ok(Arc::new(Self { source, tracks, lifetime }))
    }
}

#[derive(Clone)]
struct ParamSeed {
    info: ParamInfo,
    value: f32,
}

/// Captured once after restore/prepare and before any document or automated targets.
#[derive(Clone)]
pub(super) struct SlotSeed {
    serial: u64,
    id: String,
    ch: usize,
    params: Arc<[ParamSeed]>,
    latency: usize,
}

impl SlotSeed {
    fn latency_capacity(&self, sample_rate: f32) -> usize {
        let compressor = if self.id == "compressor" {
            self.params
                .iter()
                .find(|p| p.info.id == "lookahead")
                .map_or(0, |p| ((f64::from(p.info.max) * f64::from(sample_rate) / 1000.0).ceil() as usize).min(1 << 20))
        } else {
            0
        };
        self.latency.max(compressor)
    }
    fn capture(plugin: &dyn Plugin, id: &str, ch: usize, serial: u64) -> Result<Arc<Self>, PreparationError> {
        let info = plugin.info();
        if info.params.len() > soundcraft_dsp::MAX_AUTOMATION_PARAMS {
            return Err(PreparationError("plugin parameter catalog exceeds capacity"));
        }
        let mut params = Vec::with_capacity(info.params.len());
        for info in info.params {
            if info.id.is_empty()
                || !info.min.is_finite()
                || !info.max.is_finite()
                || info.max < info.min
                || !info.default.is_finite()
                || params.iter().any(|p: &ParamSeed| p.info.id == info.id)
            {
                return Err(PreparationError("invalid plugin parameter catalog"));
            }
            let value = info.clamp(plugin.param(info.id).filter(|v| v.is_finite()).unwrap_or(info.default));
            params.push(ParamSeed { info: info.clone(), value });
        }
        let latency = plugin.latency();
        if latency > (1 << 20) {
            return Err(PreparationError("plugin latency exceeds prepared limit"));
        }
        Ok(Arc::new(Self { serial, id: id.into(), ch, params: params.into(), latency }))
    }
}

struct Binding {
    id: &'static str,
    info: ParamInfo,
    base: f32,
    curve: Option<Curve>,
}

pub(super) struct PluginAutomation {
    bindings: Vec<Binding>,
    targets: Vec<ParamTarget<'static>>,
    values: Vec<f32>,
    applied: Vec<f32>,
    scratch: Vec<Vec<f32>>,
    reads: bool,
    /// Static built-in lookahead is known before the first aux block is processed.
    static_lookahead_ms: Option<f32>,
}

impl PluginAutomation {
    fn prepare(seed: &SlotSeed, ins: &soundcraft_model::Insert, t: &TrackAutomation, index: usize, block: usize) -> Result<Self, PreparationError> {
        let mut bindings = Vec::with_capacity(seed.params.len());
        for param in seed.params.iter() {
            let base = ins.params.get(param.info.id).copied().unwrap_or(param.value);
            if !base.is_finite() {
                return Err(PreparationError("nonfinite stored plugin parameter"));
            }
            let curve = u8::try_from(index).ok().and_then(|i| t.plugins.get(&(i, param.info.id.to_string()))).cloned();
            bindings.push(Binding { id: param.info.id, info: param.info.clone(), base: param.info.clamp(base), curve });
        }
        let n = bindings.len();
        let reads = t.mode.reads();
        let static_lookahead_ms = if seed.id == "compressor" {
            bindings.iter().find(|b| b.id == "lookahead" && (!reads || b.curve.is_none())).map(|b| b.base)
        } else {
            None
        };
        Ok(Self {
            bindings,
            targets: Vec::with_capacity(n),
            values: vec![0.0; n],
            applied: vec![f32::NAN; n],
            scratch: vec![vec![0.0; block]; seed.ch],
            reads,
            static_lookahead_ms,
        })
    }

    pub(super) fn static_latency(&self, sample_rate: f32) -> Option<usize> {
        // Match the built-in compressor's sane_sr/ms_to_samples/round operations exactly.
        // A readable lookahead curve still uses live processor latency; dynamic PDC is separate.
        let sr = if sample_rate.is_finite() && sample_rate >= 1000.0 { sample_rate.min(1_536_000.0) } else { 48_000.0 };
        self.static_lookahead_ms.map(|ms| (ms * 0.001 * sr).round() as usize)
    }

    /// Render a continuous curve as exact sample targets; constant targets keep full blocks.
    pub(super) fn process(&mut self, plugin: &mut dyn Plugin, io: &mut [Vec<f32>], pos: Samples, frames: usize) -> ProcessDelivery {
        let varying = self.reads && self.bindings.iter().any(|b| b.curve.is_some());
        let span = if varying { 1 } else { frames.max(1) };
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(span);
            let at = crate::automation::position(pos, done);
            self.targets.clear();
            for (i, b) in self.bindings.iter().enumerate() {
                let raw = if self.reads { b.curve.as_ref().map_or(b.base, |c| c.value(at, b.base)) } else { b.base };
                let value = b.info.clamp(raw);
                if let Some(v) = self.values.get_mut(i) {
                    *v = value;
                }
                if self.applied.get(i).is_none_or(|v| v.to_bits() != value.to_bits()) {
                    self.targets.push(ParamTarget { id: b.id, value });
                }
            }
            for (dst, src) in self.scratch.iter_mut().zip(io.iter()) {
                if let (Some(dst), Some(src)) = (dst.get_mut(..n), src.get(done..done + n)) {
                    dst.copy_from_slice(src);
                }
            }
            let receipt = plugin.process_with_targets(&mut self.scratch, n, &self.targets);
            if receipt == ProcessDelivery::Delivered {
                self.applied.copy_from_slice(&self.values);
            } else {
                // The plugin may already have changed internal state; never acknowledge that batch.
                self.applied.fill(f32::NAN);
                for dst in &mut self.scratch {
                    if let Some(dst) = dst.get_mut(..n) {
                        dst.fill(0.0);
                    }
                }
                for (dst, src) in io.iter_mut().zip(self.scratch.iter()) {
                    if let (Some(dst), Some(src)) = (dst.get_mut(done..done + n), src.get(..n)) {
                        dst.copy_from_slice(src);
                    }
                }
                return receipt;
            }
            for (dst, src) in io.iter_mut().zip(self.scratch.iter()) {
                if let (Some(dst), Some(src)) = (dst.get_mut(done..done + n), src.get(..n)) {
                    dst.copy_from_slice(src);
                }
            }
            done += n;
        }
        ProcessDelivery::Delivered
    }

    pub(super) fn invalidate(&mut self) {
        self.applied.fill(f32::NAN);
    }
    fn retain_delivered(&mut self, old: &Self) {
        for ((binding, applied), (previous, value)) in self.bindings.iter().zip(&mut self.applied).zip(old.bindings.iter().zip(&old.applied)) {
            if binding.id == previous.id {
                *applied = *value;
            }
        }
    }
}

impl PluginSlot {
    pub(super) fn prepared(
        mut plugin: Box<dyn Plugin>,
        ins: &soundcraft_model::Insert,
        auto: &TrackAutomation,
        index: usize,
        ch: usize,
        block: usize,
    ) -> Result<Self, PreparationError> {
        let seed = SlotSeed::capture(plugin.as_ref(), &ins.plugin, ch, 0)?;
        plugin
            .prepare_automation(seed.params.len(), soundcraft_dsp::MAX_AUTOMATION_NOTES)
            .map_err(|_| PreparationError("plugin automation capacity preparation failed"))?;
        let automation = PluginAutomation::prepare(&seed, ins, auto, index, block)?;
        Ok(Self { id: ins.plugin.clone(), ch, plugin, seed, automation })
    }
}

pub(super) struct RetainedSlot {
    track: TrackId,
    index: usize,
    seed: Arc<SlotSeed>,
    automation: PluginAutomation,
}

/// Ready-to-install workspace. Moving matching processors into it does not create instances.
pub struct PreparedMixUpdate {
    source: Arc<Session>,
    engine: MixEngine,
    retained: Vec<RetainedSlot>,
}

impl PreparedMixUpdate {
    pub fn source(&self) -> &Arc<Session> {
        &self.source
    }

    /// Only these slots keep their original prepared processor and editor identity.
    pub fn retains_instance(&self, track: TrackId, slot: usize) -> bool {
        self.retained.iter().any(|retained| retained.track == track && retained.index == slot)
    }

    /// Obtain new instances' editor handles on the control thread before installation.
    pub fn editors(&mut self) -> Vec<(TrackId, usize, Box<dyn soundcraft_dsp::PluginEditor>)> {
        let mut out = Vec::new();
        for (id, strip) in &mut self.engine.strips {
            for (index, slot) in strip.plugins.iter_mut().enumerate() {
                if let Some(editor) = slot.as_mut().and_then(|s| s.plugin.editor()) {
                    out.push((*id, index, editor));
                }
            }
            if let Some(editor) = strip.instrument.as_mut().and_then(|s| s.plugin.editor()) {
                out.push((*id, INSTRUMENT_SLOT, editor));
            }
        }
        out
    }
}

/// Control/offline catalog of instance identities and their immutable initial baselines.
#[derive(Default)]
pub struct SessionPreparer {
    lifetime: Option<u64>,
    rate: f32,
    block: usize,
    serial: u64,
    catalog: HashMap<(TrackId, usize), Arc<SlotSeed>>,
}

/// Missing processors remain errors for typed callers; legacy offline wrappers bypass them.
#[derive(Clone, Copy)]
pub(super) enum MissingPluginPolicy {
    Error,
    Bypass,
}

impl SessionPreparer {
    pub fn invalidate(&mut self) {
        self.catalog.clear();
        self.lifetime = None;
    }

    pub fn prepare(&mut self, source: Arc<Session>, lifetime: u64, block: usize) -> Result<PreparedMixUpdate, PreparationError> {
        self.prepare_with_policy(source, lifetime, block, MissingPluginPolicy::Error)
    }

    fn prepare_with_policy(
        &mut self,
        source: Arc<Session>,
        lifetime: u64,
        block: usize,
        missing: MissingPluginPolicy,
    ) -> Result<PreparedMixUpdate, PreparationError> {
        let block = block.clamp(16, 16_384);
        let mut budget = workspace_budget(&source, block)?;
        let prepared = PreparedSession::prepare(source, lifetime)?;
        let s = prepared.source.as_ref();
        let sr = s.sample_rate.as_f64() as f32;
        let mut catalog =
            if self.lifetime == Some(lifetime) && self.rate == sr && self.block == block { self.catalog.clone() } else { HashMap::new() };
        let mut engine = MixEngine::new(sr, block);
        engine.external = true;
        engine.prepared = Some(Arc::clone(&prepared));
        engine.main_fmt = s.main_format();
        let ch_main = main_channels(s);
        engine.main = vec![vec![0.0; engine.max_block]; ch_main];
        engine.main_meter.peaks = vec![0.0; ch_main];
        engine.order = processing_order(s);
        engine.work = Vec::with_capacity(s.tracks.len());
        let mut retained = Vec::new();
        let mut slots = Vec::new();
        for t in &s.tracks {
            let ch = strip_channels(t, ch_main);
            let mut strip = Strip::new();
            strip.automation = prepared.tracks.get(&t.id).cloned().ok_or(PreparationError("missing prepared track"))?;
            strip.vca = t
                .mixer
                .vca
                .and_then(|id| s.tracks.iter().find(|vca| vca.id.0 == id && vca.kind == TrackKind::Vca))
                .and_then(|vca| prepared.tracks.get(&vca.id).cloned().map(|auto| (vca.mixer.volume_db, auto)));
            strip.buf = vec![vec![0.0; engine.max_block]; ch];
            strip.pre = vec![vec![0.0; engine.max_block]; ch];
            strip.clipbuf = vec![vec![0.0; engine.max_block]; ch];
            strip.scratch = vec![0.0; engine.max_block];
            strip.held = Vec::with_capacity(128);
            strip.plugins.resize_with(t.mixer.inserts.len(), || None);
            for (index, ins) in t.mixer.inserts.iter().enumerate() {
                if let Some(ins) = ins {
                    slots.push((t.id, index, ins, ch));
                }
            }
            if let Some(ins) = &t.instrument {
                slots.push((t.id, INSTRUMENT_SLOT, ins, ch.max(2)));
            }
            for clip in t.clips() {
                if let Some(params) = clip_fx_params(s, clip.id.0) {
                    let mut fx = ClipFx::new(sr, ch);
                    fx.configure(&params);
                    strip.clip_fx.insert(clip.id.0, fx);
                }
            }
            prepare_clip_alignment(t, &mut strip, s.edit.delay_compensation, sr, &mut budget)?;
            strip.latency = strip.clip_latency;
            engine.meters.insert(t.id, StripMeter { peaks: vec![0.0; ch], ..StripMeter::default() });
            engine.strips.insert(t.id, strip);
        }
        let mut next_catalog = HashMap::with_capacity(slots.len());
        for (track, index, ins, ch) in slots {
            let key = (track, index);
            let auto = prepared.tracks.get(&track).ok_or(PreparationError("missing prepared track"))?;
            let seed = catalog.remove(&key).filter(|seed| seed.id == ins.plugin && seed.ch == ch);
            if let Some(seed) = seed {
                let automation = PluginAutomation::prepare(&seed, ins, auto, index, engine.max_block)?;
                if ins.active
                    && !ins.bypass
                    && (index != INSTRUMENT_SLOT || s.track(track).is_some_and(|track| matches!(track.kind, TrackKind::Instrument | TrackKind::Midi)))
                {
                    let latency = automation.static_latency(sr).unwrap_or(seed.latency);
                    let strip = engine.strips.get_mut(&track).ok_or(PreparationError("missing prepared strip"))?;
                    strip.latency = strip
                        .latency
                        .checked_add(latency)
                        .filter(|latency| *latency <= (1 << 20))
                        .ok_or(PreparationError("prepared strip latency exceeds limit"))?;
                }
                retained.push(RetainedSlot { track, index, seed: Arc::clone(&seed), automation });
                next_catalog.insert(key, seed);
            } else {
                let state = ins.state_bytes();
                let Some(mut plugin) = create_instance(&ins.plugin, state.as_deref(), sr, engine.max_block, ch) else {
                    match missing {
                        MissingPluginPolicy::Error => return Err(PreparationError("plugin instance unavailable")),
                        MissingPluginPolicy::Bypass => continue,
                    }
                };
                self.serial = self.serial.checked_add(1).ok_or(PreparationError("instance identity overflow"))?;
                let seed = SlotSeed::capture(plugin.as_ref(), &ins.plugin, ch, self.serial)?;
                plugin
                    .prepare_automation(seed.params.len(), soundcraft_dsp::MAX_AUTOMATION_NOTES)
                    .map_err(|_| PreparationError("plugin automation capacity preparation failed"))?;
                let automation = PluginAutomation::prepare(&seed, ins, auto, index, engine.max_block)?;
                if ins.active
                    && !ins.bypass
                    && (index != INSTRUMENT_SLOT || s.track(track).is_some_and(|track| matches!(track.kind, TrackKind::Instrument | TrackKind::Midi)))
                {
                    let latency = automation.static_latency(sr).unwrap_or(seed.latency);
                    let strip = engine.strips.get_mut(&track).ok_or(PreparationError("missing prepared strip"))?;
                    strip.latency = strip
                        .latency
                        .checked_add(latency)
                        .filter(|latency| *latency <= (1 << 20))
                        .ok_or(PreparationError("prepared strip latency exceeds limit"))?;
                }
                let slot = PluginSlot { id: ins.plugin.clone(), ch, plugin, seed: Arc::clone(&seed), automation };
                if let Some(st) = engine.strips.get_mut(&track) {
                    if index == INSTRUMENT_SLOT {
                        st.instrument_release_pending = !ins.active || ins.bypass;
                        st.instrument = Some(slot);
                    } else if let Some(dst) = st.plugins.get_mut(index) {
                        *dst = Some(slot);
                    }
                }
                next_catalog.insert(key, seed);
            }
        }
        // Allocate bounded PDC capacity on the control thread. A later dynamic increase is a render error.
        let latency = next_catalog
            .values()
            .try_fold(0usize, |sum, seed| sum.checked_add(seed.latency_capacity(sr)))
            .filter(|latency| *latency <= (1 << 20))
            .ok_or(PreparationError("latency overflow or capacity exceeded"))?;
        let latency = engine
            .strips
            .values()
            .try_fold(latency, |total, strip| total.checked_add(strip.clip_latency))
            .filter(|latency| *latency <= (1 << 20))
            .ok_or(PreparationError("ClipFX aggregate latency exceeds prepared limit"))?;
        budget.charge(pdc::workspace_bytes(s, if s.edit.delay_compensation { latency } else { 0 }, block)?, 1)?;
        let plan = pdc::Plan::compile(s, &engine.strips, sr, latency)?;
        if s.edit.delay_compensation {
            engine.order = plan.processing_order();
        }
        plan.install_edges(s, &mut engine.strips, block)?;
        engine.latency = plan.output_latency(None)?;
        engine.pdc = Some(plan);
        for (id, st) in &mut engine.strips {
            if let Some(t) = s.track(*id) {
                st.rtp = midi_properties(s, t);
                sync_sample(st, t.instrument.as_ref(), s);
            }
        }
        for bus in &s.busses {
            let ch = bus.format.channels().clamp(1, MAX_CHANNELS);
            engine.busses.insert(bus.id, BusBuf { fmt: bus.format, buf: vec![vec![0.0; engine.max_block]; ch] });
        }
        self.catalog = next_catalog;
        self.lifetime = Some(lifetime);
        self.rate = sr;
        self.block = block;
        Ok(PreparedMixUpdate { source: Arc::clone(&prepared.source), engine, retained })
    }
}

impl MixEngine {
    /// Whether the installed immutable pair belongs to this exact document and lifetime.
    /// A value-equal clone or an unsuccessful installation does not satisfy the binding.
    pub fn is_prepared_for(&self, source: &Arc<Session>, lifetime: u64) -> bool {
        self.installation_error.is_none()
            && self.prepared.as_ref().is_some_and(|prepared| prepared.lifetime == lifetime && Arc::ptr_eq(&prepared.source, source))
    }

    /// Trusted immutable initial values for active hosted parameters omitted by the model.
    /// These are creation/restore baselines, not a processor's transient last Read target.
    /// The exceptional export request allocates bounded returned IDs/records without vendor calls.
    pub fn try_capture_static_baselines(&self) -> Result<Vec<(TrackId, usize, String, f32)>, PreparationError> {
        if let Some(error) = self.installation_error {
            return Err(error);
        }
        let source = self.prepared.as_ref().ok_or(PreparationError("no prepared mix snapshot"))?.source();
        let mut slots = 0usize;
        let mut records = 0usize;
        let mut bytes = 0usize;
        let mut out = Vec::new();
        for track in source.tracks.iter().filter(|track| !track.inactive) {
            let strip = self.strips.get(&track.id).ok_or(PreparationError("export baseline strip is unavailable"))?;
            for (index, insert) in track
                .mixer
                .inserts
                .iter()
                .enumerate()
                .filter_map(|(index, insert)| insert.as_ref().map(|insert| (index, insert)))
                .chain(track.instrument.as_ref().map(|insert| (INSTRUMENT_SLOT, insert)))
            {
                if !insert.active || !is_third_party(&insert.plugin) {
                    continue;
                }
                slots = slots
                    .checked_add(1)
                    .filter(|slots| *slots <= MAX_EXPORT_STATE_SLOTS)
                    .ok_or(PreparationError("export baseline slot limit exceeded"))?;
                let slot = if index == INSTRUMENT_SLOT { strip.instrument.as_ref() } else { strip.plugins.get(index).and_then(Option::as_ref) }
                    .filter(|slot| slot.id == insert.plugin)
                    .ok_or(PreparationError("required export baseline instance is unavailable"))?;
                for parameter in slot.seed.params.iter().filter(|parameter| !insert.params.contains_key(parameter.info.id)) {
                    let id = parameter.info.id;
                    if id.len() > 4096 || !parameter.value.is_finite() {
                        return Err(PreparationError("invalid export static baseline"));
                    }
                    records = records
                        .checked_add(1)
                        .filter(|records| *records <= (1 << 20))
                        .ok_or(PreparationError("export baseline record limit exceeded"))?;
                    bytes = bytes
                        .checked_add(size_of::<(TrackId, usize, String, f32)>())
                        .and_then(|bytes| bytes.checked_add(id.len()))
                        .filter(|bytes| *bytes <= (64 << 20))
                        .ok_or(PreparationError("export baseline byte limit exceeded"))?;
                    out.try_reserve(1).map_err(|_| PreparationError("export baseline allocation failed"))?;
                    let mut owned = String::new();
                    owned.try_reserve_exact(id.len()).map_err(|_| PreparationError("export baseline ID allocation failed"))?;
                    owned.push_str(id);
                    out.push((track.id, index, owned, parameter.value));
                }
            }
        }
        Ok(out)
    }

    /// Configure an export's absolute source cutoff without resetting ringing processors.
    /// The default None leaves ordinary playback and specialized Track Bounce unchanged.
    pub fn set_source_cutoff(&mut self, cutoff: Option<Samples>) -> Result<(), PreparationError> {
        if let Some(error) = self.installation_error {
            return Err(error);
        }
        if self.prepared.is_none() {
            return Err(PreparationError("no prepared mix snapshot"));
        }
        self.source_cutoff = cutoff;
        self.source_cutoff_started = false;
        for strip in self.strips.values_mut() {
            strip.skip_source = false;
            strip.source_window = cutoff.is_some();
            for fx in strip.clip_fx.values_mut() {
                fx.window_active = false;
                fx.window_block = None;
            }
        }
        Ok(())
    }

    /// Prepared graph arrival at a named bus, or main plus serial Master latency.
    pub fn prepared_output_latency(&self, bus: Option<BusId>) -> Result<usize, PreparationError> {
        if let Some(error) = self.installation_error {
            return Err(error);
        }
        self.pdc.as_ref().ok_or(PreparationError("no prepared compensation plan"))?.output_latency(bus)
    }

    pub(super) fn prepare_compatibility(&mut self, source: &Session) -> Result<(), PreparationError> {
        let mut budget = workspace_budget(source, self.max_block)?;
        self.ensure_synced(source);
        let prepared = PreparedSession::prepare(Arc::new(source.clone()), 0)?;
        let mut total_latency = 0usize;
        for t in &source.tracks {
            let Some(st) = self.strips.get_mut(&t.id) else { continue };
            st.automation = prepared.tracks.get(&t.id).cloned().ok_or(PreparationError("missing prepared track"))?;
            st.vca = t
                .mixer
                .vca
                .and_then(|id| source.tracks.iter().find(|vca| vca.id.0 == id && vca.kind == TrackKind::Vca))
                .and_then(|vca| prepared.tracks.get(&vca.id).cloned().map(|auto| (vca.mixer.volume_db, auto)));
            for (index, ins) in t.mixer.inserts.iter().enumerate() {
                if let (Some(ins), Some(Some(slot))) = (ins, st.plugins.get_mut(index)) {
                    let mut automation = PluginAutomation::prepare(&slot.seed, ins, &st.automation, index, self.max_block)?;
                    automation.retain_delivered(&slot.automation);
                    slot.automation = automation;
                    total_latency = total_latency.saturating_add(slot.seed.latency_capacity(self.sample_rate).max(slot.plugin.latency()));
                }
            }
            if let (Some(ins), Some(slot)) = (&t.instrument, &mut st.instrument) {
                let mut automation = PluginAutomation::prepare(&slot.seed, ins, &st.automation, INSTRUMENT_SLOT, self.max_block)?;
                automation.retain_delivered(&slot.automation);
                slot.automation = automation;
                total_latency = total_latency.saturating_add(slot.seed.latency_capacity(self.sample_rate).max(slot.plugin.latency()));
            }
            st.rtp = midi_properties(source, t);
            st.scratch.resize(self.max_block, 0.0);
            st.pre = vec![vec![0.0; self.max_block]; st.buf.len()];
            st.clipbuf = vec![vec![0.0; self.max_block]; st.buf.len()];
            for clip in t.clips() {
                if let Some(params) = clip_fx_params(source, clip.id.0) {
                    let fx = st.clip_fx.entry(clip.id.0).or_insert_with(|| ClipFx::new(self.sample_rate, st.buf.len()));
                    fx.configure(&params);
                } else if let Some(fx) = st.clip_fx.get_mut(&clip.id.0) {
                    fx.configure(&[]);
                }
            }
            st.clip_fx.retain(|id, _| t.clips().iter().any(|clip| clip.id.0 == *id));
            prepare_clip_alignment(t, st, source.edit.delay_compensation, self.sample_rate, &mut budget)?;
            total_latency = total_latency.checked_add(st.clip_latency).ok_or(PreparationError("ClipFX latency overflow"))?;
            st.latency = pdc::own_latency(t, st, self.sample_rate)?;
            self.meters.entry(t.id).or_default().peaks.resize(st.buf.len(), 0.0);
        }
        if total_latency > (1 << 20) {
            return Err(PreparationError("latency exceeds prepared limit"));
        }
        budget.charge(pdc::workspace_bytes(source, if source.edit.delay_compensation { total_latency } else { 0 }, self.max_block)?, 1)?;
        let plan = pdc::Plan::compile(source, &self.strips, self.sample_rate, total_latency)?;
        if source.edit.delay_compensation {
            self.order = plan.processing_order();
        }
        plan.install_edges(source, &mut self.strips, self.max_block)?;
        self.latency = plan.output_latency(None)?;
        self.pdc = Some(plan);
        self.main_meter.peaks.resize(self.main.len(), 0.0);
        self.work.reserve(source.tracks.len());
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Install the already prepared pair/workspace. Return the old workspace for off-thread retirement.
    pub fn install_prepared(&mut self, mut update: PreparedMixUpdate) -> Self {
        let same_document = self.sample_rate == update.engine.sample_rate
            && self.max_block == update.engine.max_block
            && self.prepared.as_ref().zip(update.engine.prepared.as_ref()).is_some_and(|(a, b)| a.lifetime == b.lifetime);
        for retained in &mut update.retained {
            let previous_instrument_state =
                self.strips.get(&retained.track).map(|strip| (strip.instrument_enabled, strip.instrument_release_pending));
            let old = self.strips.get_mut(&retained.track).and_then(|s| {
                if retained.index == INSTRUMENT_SLOT { s.instrument.take() } else { s.plugins.get_mut(retained.index).and_then(Option::take) }
            });
            if let Some(mut old) = old {
                if !same_document || old.seed.serial != retained.seed.serial || old.id != retained.seed.id || old.ch != retained.seed.ch {
                    update.engine.installation_error = Some(PreparationError("prepared live instance identity mismatch"));
                    if let Some(st) = self.strips.get_mut(&retained.track) {
                        if retained.index == INSTRUMENT_SLOT {
                            st.instrument = Some(old);
                        } else if let Some(dst) = st.plugins.get_mut(retained.index) {
                            *dst = Some(old);
                        }
                    }
                    continue;
                }
                retained.automation.retain_delivered(&old.automation);
                std::mem::swap(&mut old.automation, &mut retained.automation);
                if let Some(st) = update.engine.strips.get_mut(&retained.track) {
                    if retained.index == INSTRUMENT_SLOT {
                        if let Some((enabled, release_pending)) = previous_instrument_state {
                            st.instrument_enabled = enabled;
                            st.instrument_release_pending = release_pending;
                        }
                        st.instrument = Some(old);
                    } else if let Some(dst) = st.plugins.get_mut(retained.index) {
                        *dst = Some(old);
                    }
                }
            } else {
                update.engine.installation_error = Some(PreparationError("prepared live instance identity mismatch"));
            }
        }
        if same_document {
            for (id, st) in &mut update.engine.strips {
                if let Some(old) = self.strips.get_mut(id) {
                    // Unchanged Clip Effects keep their filter/dynamics tail across document edits.
                    // The unused replacement moves into the old workspace for off-thread retirement.
                    for (clip, fx) in &mut st.clip_fx {
                        if let Some(previous) = old.clip_fx.get_mut(clip)
                            && fx.sr == previous.sr
                            && fx.ch == previous.ch
                            && fx.configured == previous.configured
                        {
                            if fx.align.len == previous.align.len {
                                std::mem::swap(fx, previous);
                            } else {
                                // Keep the unchanged processors but use the new prepared alignment.
                                std::mem::swap(&mut fx.eq, &mut previous.eq);
                                std::mem::swap(&mut fx.comp, &mut previous.comp);
                            }
                        }
                    }
                    std::mem::swap(&mut st.held, &mut old.held);
                    let same_sample = st.sample.as_ref().zip(old.sample.as_ref()).is_some_and(|(a, b)| Arc::ptr_eq(a, b))
                        || (st.sample.is_none() && old.sample.is_none());
                    if !same_sample && let Some(instrument) = &mut st.instrument {
                        instrument.plugin.set_sample(st.sample_source.clone());
                    }
                    if let (Some(new), Some(previous)) = (&mut st.output_edge, &mut old.output_edge) {
                        new.retain_from(previous);
                    }
                    for (new, previous) in st.send_edges.iter_mut().zip(&mut old.send_edges) {
                        if let (Some(new), Some(previous)) = (new, previous) {
                            new.retain_from(previous);
                        }
                    }
                }
            }
            update.engine.last_end = self.last_end;
        }
        std::mem::swap(&mut update.engine.input, &mut self.input);
        update.engine.pan_law = self.pan_law;
        update.engine.metronome = self.metronome;
        let mut old = std::mem::replace(self, update.engine);
        old.retired_bindings = update.retained;
        old
    }

    pub fn render_prepared(&mut self, pos: Samples, frames: usize, out: &mut [Vec<f32>]) -> Result<(), PreparationError> {
        self.render_prepared_output_inner(pos, frames, None, out, false)
    }

    /// Render one output through the installed pair, splitting a source cutoff exactly.
    /// All buffers were prepared by the control owner; this method allocates no span scratch.
    pub fn render_prepared_output(&mut self, pos: Samples, frames: usize, bus: Option<BusId>, out: &mut [Vec<f32>]) -> Result<(), PreparationError> {
        self.render_prepared_output_inner(pos, frames, bus, out, true)
    }

    fn render_prepared_output_inner(
        &mut self,
        pos: Samples,
        frames: usize,
        bus: Option<BusId>,
        out: &mut [Vec<f32>],
        strict_finite: bool,
    ) -> Result<(), PreparationError> {
        if frames > self.max_block || out.iter().any(|channel| channel.len() < frames) {
            return Err(PreparationError("render span exceeds prepared buffer shape"));
        }
        let prepared = self.prepared.clone().ok_or(PreparationError("no prepared mix snapshot"))?;
        if bus.is_some_and(|id| prepared.source.bus(id).is_none()) {
            return Err(PreparationError("unknown render bus"));
        }
        let span = i64::try_from(frames).map_err(|_| PreparationError("render span exceeds timeline range"))?;
        pos.checked_add(span).ok_or(PreparationError("render position overflow"))?;
        let mut done = 0usize;
        while done < frames {
            let at = pos.checked_add(done as i64).ok_or(PreparationError("render position overflow"))?;
            let mut count = frames - done;
            if let Some(cutoff) = self.source_cutoff {
                if at < cutoff {
                    count = count.min(usize::try_from(cutoff.saturating_sub(at)).unwrap_or(usize::MAX));
                    if self.source_cutoff_started {
                        self.source_cutoff_started = false;
                        for strip in self.strips.values_mut() {
                            strip.skip_source = false;
                            for fx in strip.clip_fx.values_mut() {
                                fx.reset_window();
                            }
                        }
                    }
                } else if !self.source_cutoff_started {
                    self.source_cutoff_started = true;
                    for strip in self.strips.values_mut() {
                        strip.skip_source = true;
                        if let Some(instrument) = &mut strip.instrument {
                            instrument.plugin.all_notes_off();
                        }
                        strip.held.clear();
                    }
                }
            }
            self.render_core(&prepared.source, at, count, &mut []);
            if let Some(error) = self.render_error {
                for channel in out.iter_mut() {
                    channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
                }
                return Err(error);
            }
            let source = match bus {
                Some(id) => self.busses.get(&id).map(|bus| bus.buf.as_slice()).ok_or(PreparationError("render bus is unavailable"))?,
                None => self.main.as_slice(),
            };
            if strict_finite && source.iter().any(|channel| channel.iter().take(count).any(|sample| !sample.is_finite())) {
                for channel in out.iter_mut() {
                    channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
                }
                return Err(PreparationError("selected render output contains nonfinite samples"));
            }
            for (channel, output) in out.iter_mut().enumerate() {
                for offset in 0..count {
                    if let Some(dst) = output.get_mut(done + offset) {
                        let sample = source.get(channel).and_then(|channel| channel.get(offset)).copied().unwrap_or(0.0);
                        *dst = if sample.is_finite() { sample } else { 0.0 };
                    }
                }
            }
            done += count;
        }
        Ok(())
    }
}

/// Stateful explicit offline renderer; edits preserve matching processors and their creation seeds.
pub struct OfflineRenderer {
    preparer: SessionPreparer,
    mix: MixEngine,
}

impl OfflineRenderer {
    pub fn prepare(source: Arc<Session>, block: usize) -> Result<Self, PreparationError> {
        Self::prepare_with_policy(source, block, MissingPluginPolicy::Error)
    }
    pub(super) fn prepare_with_policy(source: Arc<Session>, block: usize, missing: MissingPluginPolicy) -> Result<Self, PreparationError> {
        let mut preparer = SessionPreparer::default();
        let update = preparer.prepare_with_policy(Arc::clone(&source), 0, block, missing)?;
        let mut mix = MixEngine::new(source.sample_rate.as_f64() as f32, block);
        drop(mix.install_prepared(update));
        Ok(Self { preparer, mix })
    }
    pub fn reprepare(&mut self, source: Arc<Session>) -> Result<(), PreparationError> {
        let update = self.preparer.prepare(source, 0, self.mix.max_block)?;
        drop(self.mix.install_prepared(update));
        Ok(())
    }
    pub fn render(&mut self, pos: Samples, frames: usize, out: &mut [Vec<f32>]) -> Result<(), PreparationError> {
        self.mix.render_prepared(pos, frames, out)
    }
    pub fn render_output(&mut self, pos: Samples, frames: usize, bus: Option<BusId>, out: &mut [Vec<f32>]) -> Result<(), PreparationError> {
        self.mix.render_prepared_output(pos, frames, bus, out)
    }
    pub fn mix(&self) -> &MixEngine {
        &self.mix
    }
    pub fn mix_mut(&mut self) -> &mut MixEngine {
        &mut self.mix
    }
}
