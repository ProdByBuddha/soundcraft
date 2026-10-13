//! The SoundCraft mix engine.
//!
//! [`MixEngine::render`] produces one block of the main output for a session at a timeline
//! position: clips (with clip gain and fades) → trim/phase → inserts → pre-fader sends → fader and
//! mute (with automation) → pan → post-fader sends → busses → aux inputs → main → master faders.
//! The same code drives realtime playback and offline bounces, so what you hear is what you get.
//! Prepared realtime rendering does not allocate additional adapter workspace in steady state.
//! The legacy `render(&Session, ...)` adapter prepares on its caller thread; realtime callers use
//! `SessionPreparer` and `render_prepared` instead.
//!
//! ## Surround
//! The main mix has the format of the session's main output path ([`Session::main_format`]:
//! stereo by default, up to 16 channels such as 7.1.4 or 9.1.6) and every bus has its own format.
//! Channels are in SMPTE / WAV order (L R C LFE Ls Rs …, see
//! [`ChannelFormat::speakers`](soundcraft_model::ChannelFormat::speakers)). Routing a strip into a
//! destination builds a gain matrix per block:
//! - mono/stereo sources into a stereo destination use the stereo pan exactly as before;
//! - into a multichannel destination they use the track's [`SurroundPan`] (pairwise
//!   constant-power panner over the speaker layout, see `soundcraft_dsp::pan::surround_gains`),
//!   or go to L/R with the stereo pan when the track has no surround pan;
//! - sources whose format equals the destination's pass straight through, channel for channel;
//! - other multichannel sources fold to stereo with ITU-R BS.775 coefficients (C and surrounds at
//!   -3 dB, LFE dropped), or map speaker by speaker into other multichannel layouts.
//!
//! ## Plugin instances and threads
//! [`MixEngine::sync`] brings plugin instances in line with the session. Built-in plugins are
//! cheap (allocation only) and are always created there, on whichever thread renders. Hosted
//! third-party plugins (`clap:` / `vst3:` ids, see [`is_third_party`]) are expensive and have
//! main-thread rules, so a realtime host keeps them off the audio thread:
//! - with [`MixEngine::set_external_instances`]`(true)` (realtime playback), `sync` never creates a
//!   third-party plugin. The host's control (UI) thread creates them with [`create_instance`] from
//!   [`instance_specs`] (restoring each insert's stored [`Insert::state`] and calling `prepare`
//!   there), and hands them over with [`MixEngine::adopt`]; `sync` then moves the matching one
//!   into its slot. A slot whose instance has not arrived yet passes audio through.
//! - every instance `sync` replaces or removes (built-in or not) goes to a retired list instead of
//!   being dropped; the host takes it with [`MixEngine::take_retired`] and drops it on the control
//!   thread, so plugin destruction never runs on the audio thread either.
//! - without external instances (the default: offline [`render_range`], bounces, tests) `sync`
//!   creates third-party plugins itself, synchronously, restores their stored state before the
//!   first `process`, and drops retired instances at the end of `sync`.
//!
//! An instance starts from its stored state; the insert's parameter values are then applied on
//! top before the first block (the session's values are kept in step with the plugin, including
//! edits made in its own editor, so both agree; the state carries everything else).
//! [`MixEngine::capture_states`] reads every live instance's state back (for saving the session);
//! it calls into the plugins and allocates, so a realtime host runs it only on request.
//!
//! A built-in instrument's parameter values are applied the same way. A sample-playing
//! instrument (the Sampler) is handed the decoded audio of its insert's
//! [`Insert::sample`] source straight from the session's pool, shared and never copied.
//!
//! [`Insert::state`]: soundcraft_model::Insert::state
//! [`Insert::sample`]: soundcraft_model::Insert::sample
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use soundcraft_dsp::pan::{PanLaw, SpeakerPos, SurroundParams, gains};
use soundcraft_dsp::{Plugin, SampleSource, db_to_gain};
use soundcraft_model::{
    AutoParam, BusId, ChannelFormat, Clip, ClipContent, Route, Session, SourceAudio, Speaker, SurroundPan, Track, TrackId, TrackKind,
};
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;
use std::sync::Arc;

mod automation;
mod pdc;
mod prepared;
pub use prepared::{OfflineRenderer, PreparationError, PreparedMixUpdate, PreparedSession, SessionPreparer};
pub mod track_bounce;

/// Default (stereo) main mix width. The actual width is [`main_channels`].
pub const MAIN_CHANNELS: usize = 2;

/// Widest strip, bus or main mix (9.1.6 / 3rd-order Ambisonics). Wider formats are truncated.
pub const MAX_CHANNELS: usize = 16;

/// Number of channels of a session's main mix.
pub fn main_channels(s: &Session) -> usize {
    s.main_format().channels().clamp(MAIN_CHANNELS, MAX_CHANNELS)
}

/// Slot index used for a track's instrument plugin in [`InstanceSpec`], [`PreparedInstance`] and
/// [`MixEngine::capture_states`] (insert slots use their index).
pub const INSTRUMENT_SLOT: usize = usize::MAX;

/// Strict export capture limits on returned opaque data, separate from vendor allocations.
pub const MAX_EXPORT_STATE_SLOTS: usize = 4096;
pub const MAX_EXPORT_STATE_BYTES: usize = 128 << 20;

/// Hosted third-party plugin ids (CLAP or VST3): created off the audio thread in realtime use.
pub fn is_third_party(id: &str) -> bool {
    id.starts_with(soundcraft_clap_host::ID_PREFIX)
        || id.starts_with(soundcraft_vst3_host::ID_PREFIX)
        || id.starts_with(soundcraft_au_host::ID_PREFIX)
}

/// A third-party plugin instance a session needs: which slot, which plugin, how many channels,
/// and the state to restore.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceSpec {
    pub track: TrackId,
    /// Insert slot index, or [`INSTRUMENT_SLOT`].
    pub slot: usize,
    pub id: String,
    /// Channel count the instance is prepared for.
    pub ch: usize,
    /// Decoded [`Insert::state`](soundcraft_model::Insert::state).
    pub state: Option<Vec<u8>>,
}

/// A plugin instance created and prepared off the audio thread, handed to [`MixEngine::adopt`].
pub struct PreparedInstance {
    pub track: TrackId,
    pub slot: usize,
    pub id: String,
    pub ch: usize,
    pub plugin: Box<dyn Plugin>,
}

/// Every third-party plugin slot (inserts and instruments) of a session, with the channel count
/// [`MixEngine::sync`] prepares it for. Inactive and master tracks are included (they have strips).
pub fn instance_specs(s: &Session) -> Vec<InstanceSpec> {
    let main_ch = main_channels(s);
    let mut out = Vec::new();
    for t in &s.tracks {
        let ch = strip_channels(t, main_ch);
        for (i, ins) in t.mixer.inserts.iter().enumerate() {
            if let Some(ins) = ins
                && is_third_party(&ins.plugin)
            {
                out.push(InstanceSpec { track: t.id, slot: i, id: ins.plugin.clone(), ch, state: ins.state_bytes() });
            }
        }
        if let Some(ins) = &t.instrument
            && is_third_party(&ins.plugin)
        {
            out.push(InstanceSpec { track: t.id, slot: INSTRUMENT_SLOT, id: ins.plugin.clone(), ch: ch.max(2), state: ins.state_bytes() });
        }
    }
    out
}

/// Creates a plugin (built-in or hosted), restores `state` (if any) and prepares it. Expensive
/// for third-party plugins: in realtime use, call it on the control thread.
pub fn create_instance(id: &str, state: Option<&[u8]>, sample_rate: f32, max_block: usize, ch: usize) -> Option<Box<dyn Plugin>> {
    let mut p = create_plugin(id)?;
    if let Some(st) = state
        && !p.load_state(st)
    {
        log::warn!("{id}: the plugin did not accept its stored state");
    }
    p.prepare(sample_rate, max_block, ch);
    Some(p)
}

/// Peak meter values for one strip (linear, per channel, max over the last block).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StripMeter {
    /// First two channels (mono strips repeat channel 0), for stereo meters.
    pub peak: [f32; 2],
    /// Every channel of the strip (or of the main mix).
    pub peaks: Vec<f32>,
    /// Gain reduction of the first dynamics insert, dB.
    pub gain_reduction_db: f32,
}

impl StripMeter {
    /// Copy another meter without reallocating once the channel count is stable.
    pub fn copy_from(&mut self, o: &StripMeter) {
        self.peak = o.peak;
        self.peaks.clone_from(&o.peaks);
        self.gain_reduction_db = o.gain_reduction_db;
    }

    fn measure(&mut self, bufs: &[Vec<f32>], frames: usize, gr: f32) {
        if self.peaks.len() != bufs.len() {
            self.peaks.resize(bufs.len(), 0.0);
        }
        for (p, b) in self.peaks.iter_mut().zip(bufs.iter()) {
            *p = peak(b.get(..frames).unwrap_or(&[]));
        }
        let last = self.peaks.len().saturating_sub(1);
        self.peak = [self.peaks.first().copied().unwrap_or(0.0), self.peaks.get(1.min(last)).copied().unwrap_or(0.0)];
        self.gain_reduction_db = gr;
    }
}

/// A bus: its format and one buffer per channel.
struct BusBuf {
    fmt: ChannelFormat,
    buf: Vec<Vec<f32>>,
}

struct PluginSlot {
    id: String,
    /// Channel count the plugin was prepared for.
    ch: usize,
    plugin: Box<dyn Plugin>,
    seed: Arc<prepared::SlotSeed>,
    automation: prepared::PluginAutomation,
}

struct Strip {
    buf: Vec<Vec<f32>>,
    /// Pre-fader copy for pre-fader sends.
    pre: Vec<Vec<f32>>,
    scratch: Vec<f32>,
    plugins: Vec<Option<PluginSlot>>,
    instrument: Option<PluginSlot>,
    /// Actual live processing state; retained only with this instrument's identity.
    instrument_enabled: bool,
    instrument_release_pending: bool,
    /// The pool audio handed to the instrument with `set_sample` (sample-playing instruments).
    sample: Option<Arc<SourceAudio>>,
    sample_source: Option<Arc<dyn SampleSource>>,
    rtp: [f64; 4],
    /// MIDI notes currently sounding on the instrument (pitch), for note-offs at stops/seek.
    held: Vec<u8>,
    muted: bool,
    automation: automation::TrackAutomation,
    error: Option<PreparationError>,
    vca: Option<(f32, automation::TrackAutomation)>,
    /// Export-only volume bypass. Ordinary/realtime strips always leave this false.
    skip_volume: bool,
    /// Export tail: stop clip input/new note scheduling while processors keep ringing.
    skip_source: bool,
    /// Continue previously fed ClipFX on zero input only in an explicit export window.
    source_window: bool,
    gr: f32,
    /// Per-clip effect chains (Clip Effects) and a scratch buffer to render clips into.
    clip_fx: HashMap<u64, ClipFx>,
    clipbuf: Vec<Vec<f32>>,
    clip_latency: usize,
    /// Total latency of the active inserts (samples).
    latency: usize,
    /// Independently prepared, destination-width compensation for output and each send.
    output_edge: Option<pdc::Edge>,
    send_edges: Vec<Option<pdc::Edge>>,
}

/// A multichannel delay line (samples), resized only when the delay changes.
#[derive(Default)]
struct DelayLine {
    bufs: Vec<Vec<f32>>,
    idx: usize,
    len: usize,
}

impl DelayLine {
    fn reserve(&mut self, capacity: usize, ch: usize) -> Result<(), PreparationError> {
        if capacity > (1 << 20) {
            return Err(PreparationError("PDC capacity exceeds limit"));
        }
        self.bufs = window_planar(ch, capacity)?;
        self.idx = 0;
        self.len = 0;
        Ok(())
    }
    fn clear(&mut self) {
        self.idx = 0;
        for channel in &mut self.bufs {
            channel.fill(0.0);
        }
    }
    fn set(&mut self, len: usize, ch: usize) -> bool {
        if len > (1 << 20) {
            return false;
        }
        if self.bufs.len() != ch || self.bufs.iter().any(|buf| buf.len() < len) {
            return false;
        }
        if len != self.len {
            self.len = len;
            self.idx = 0;
            for buf in &mut self.bufs {
                if let Some(buf) = buf.get_mut(..len) {
                    buf.fill(0.0);
                }
            }
        }
        true
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if self.len == 0 {
            return;
        }
        let start = self.idx;
        for (c, ch) in io.iter_mut().enumerate() {
            let Some(ring) = self.bufs.get_mut(c) else { continue };
            let mut i = start;
            for x in ch.iter_mut().take(frames) {
                if let Some(r) = ring.get_mut(i) {
                    std::mem::swap(r, x);
                }
                i += 1;
                if i >= self.len {
                    i = 0;
                }
            }
        }
        self.idx = (start + frames) % self.len.max(1);
    }
}

/// Clip Effects: an EQ and a compressor per clip, configured from `edit.values`
/// (`clip_fx.<clip>.eq.<param>`, `clip_fx.<clip>.comp.<param>`, `clip_fx.<clip>.gain`).
struct ClipFx {
    eq: Option<Box<dyn Plugin>>,
    comp: Option<Box<dyn Plugin>>,
    gain_db: f32,
    configured: Vec<(String, f32)>,
    window_active: bool,
    window_block: Option<Samples>,
    align: DelayLine,
    flush: usize,
    sr: f32,
    ch: usize,
}

impl Strip {
    fn new() -> Self {
        Strip {
            buf: Vec::new(),
            pre: Vec::new(),
            scratch: Vec::new(),
            plugins: Vec::new(),
            instrument: None,
            instrument_enabled: false,
            instrument_release_pending: false,
            sample: None,
            sample_source: None,
            rtp: [0.0, 0.0, 100.0, 0.0],
            held: Vec::new(),
            muted: false,
            automation: automation::TrackAutomation::default(),
            error: None,
            vca: None,
            skip_volume: false,
            skip_source: false,
            source_window: false,
            gr: 0.0,
            clip_fx: HashMap::new(),
            clipbuf: Vec::new(),
            clip_latency: 0,
            latency: 0,
            output_edge: None,
            send_edges: Vec::new(),
        }
    }
}

/// Renders sessions. Keep one per playback stream; plugin state persists across blocks.
pub struct MixEngine {
    sample_rate: f32,
    max_block: usize,
    strips: HashMap<TrackId, Strip>,
    busses: HashMap<BusId, BusBuf>,
    main: Vec<Vec<f32>>,
    main_fmt: ChannelFormat,
    pub meters: HashMap<TrackId, StripMeter>,
    pub main_meter: StripMeter,
    pub pan_law: PanLaw,
    /// Last rendered position end (to detect seeks).
    last_end: Samples,
    /// Snapshot key of the session the strips/order were synced to.
    synced: (usize, usize, usize),
    order: Vec<usize>,
    /// Current total compensation delay (samples) of the mix.
    pub latency: usize,
    /// Delay already applied to audio written onto busses: the slowest track insert.
    /// A bus render drops this so the bus lines up with the timeline. The extra delay
    /// that lines direct outputs up with aux returns stays in `latency` and is removed
    /// only from the main mix.
    track_latency: usize,
    /// Live input for the next block (planar), set by the audio host for input monitoring.
    pub input: Vec<Vec<f32>>,
    /// Transport stopped: only monitored inputs sound (no clips).
    pub monitor_only: bool,
    /// Recording: record-armed tracks hear their input (auto input).
    pub recording: bool,
    /// Mix the metronome (Options > Click) into the output. Playback turns this on; offline renders
    /// (bounces, stems, exports) leave it off so the click is never printed into a file.
    pub metronome: bool,
    /// Third-party instances come from [`MixEngine::adopt`] instead of being created in `sync`.
    external: bool,
    /// Instances handed over by the host, waiting for `sync` to place them.
    adopt: HashMap<(TrackId, usize), PreparedInstance>,
    /// Instances `sync` took out of service, for the host to drop (see [`MixEngine::take_retired`]).
    retired: Vec<Box<dyn Plugin>>,
    /// Ownership workspace reused for parallel strip processing.
    work: Vec<(usize, Strip)>,
    prepared: Option<Arc<PreparedSession>>,
    render_error: Option<PreparationError>,
    installation_error: Option<PreparationError>,
    retired_bindings: Vec<prepared::RetainedSlot>,
    source_cutoff: Option<Samples>,
    source_cutoff_started: bool,
    pdc: Option<pdc::Plan>,
}

/// What `sync` needs to fill a slot.
struct SlotCtx<'a> {
    sr: f32,
    mb: usize,
    external: bool,
    adopt: &'a mut HashMap<(TrackId, usize), PreparedInstance>,
    retired: &'a mut Vec<Box<dyn Plugin>>,
}

impl SlotCtx<'_> {
    fn retire(&mut self, slot: Option<PluginSlot>) {
        if let Some(s) = slot {
            self.retired.push(s.plugin);
        }
    }

    /// A new instance for `ins` on track `t`, slot `idx`, prepared for `ch` channels.
    fn make(&mut self, t: TrackId, idx: usize, ins: &soundcraft_model::Insert, ch: usize) -> Option<PluginSlot> {
        let third = is_third_party(&ins.plugin);
        let plugin = if third && self.external {
            match self.adopt.remove(&(t, idx)) {
                Some(p) if p.id == ins.plugin && p.ch == ch => Some(p.plugin),
                Some(p) => {
                    self.retired.push(p.plugin);
                    None
                }
                None => None,
            }
        } else {
            let state = if third { ins.state_bytes() } else { None };
            create_instance(&ins.plugin, state.as_deref(), self.sr, self.mb, ch)
        }?;
        let auto = automation::TrackAutomation::default();
        PluginSlot::prepared(plugin, ins, &auto, idx, ch, self.mb).ok()
    }
}

impl MixEngine {
    pub fn new(sample_rate: f32, max_block: usize) -> Self {
        let max_block = max_block.clamp(16, 16_384);
        MixEngine {
            sample_rate: if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48_000.0 },
            max_block,
            strips: HashMap::new(),
            busses: HashMap::new(),
            main: vec![vec![0.0; max_block]; MAIN_CHANNELS],
            main_fmt: ChannelFormat::Stereo,
            meters: HashMap::new(),
            main_meter: StripMeter::default(),
            pan_law: PanLaw::Minus3,
            last_end: 0,
            synced: (0, 0, 0),
            order: Vec::new(),
            latency: 0,
            track_latency: 0,
            input: Vec::new(),
            monitor_only: false,
            recording: false,
            metronome: false,
            external: false,
            adopt: HashMap::new(),
            retired: Vec::new(),
            work: Vec::new(),
            prepared: None,
            render_error: None,
            installation_error: None,
            retired_bindings: Vec::new(),
            source_cutoff: None,
            source_cutoff_started: false,
            pdc: None,
        }
    }

    /// Realtime mode: third-party plugins are created by the host and handed over with
    /// [`MixEngine::adopt`] instead of being created in `sync` (see the crate docs).
    pub fn set_external_instances(&mut self, on: bool) {
        self.external = on;
    }

    /// Takes instances created off the audio thread; the next render's `sync` places them.
    /// An earlier instance waiting for the same slot is retired.
    pub fn adopt(&mut self, instances: Vec<PreparedInstance>) {
        for p in instances {
            if let Some(old) = self.adopt.insert((p.track, p.slot), p) {
                self.retired.push(old.plugin);
            }
        }
        // Force a re-sync so waiting slots pick their instance up.
        self.synced = (usize::MAX, usize::MAX, usize::MAX);
    }

    /// Instances taken out of service since the last call (drop them off the audio thread).
    /// Does not allocate.
    pub fn take_retired(&mut self) -> Vec<Box<dyn Plugin>> {
        std::mem::take(&mut self.retired)
    }

    /// Every instance this engine holds (strips, instruments, clip effects excluded) plus the
    /// retired and waiting ones, leaving the engine empty: hand them to another thread to drop
    /// before replacing the engine.
    pub fn drain_plugins(&mut self) -> Vec<Box<dyn Plugin>> {
        let mut out = std::mem::take(&mut self.retired);
        for st in self.strips.values_mut() {
            out.extend(st.plugins.iter_mut().filter_map(Option::take).map(|s| s.plugin));
            out.extend(st.instrument.take().map(|s| s.plugin));
        }
        out.extend(self.adopt.drain().map(|(_, p)| p.plugin));
        self.synced = (usize::MAX, usize::MAX, usize::MAX);
        out
    }

    /// The state of every live plugin that has one (third-party plugins), as
    /// `(track, slot or INSTRUMENT_SLOT, state)`. Calls into the plugins and allocates.
    pub fn capture_states(&mut self) -> Vec<(TrackId, usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (tid, st) in self.strips.iter_mut() {
            for (i, slot) in st.plugins.iter_mut().enumerate() {
                if let Some(data) = slot.as_mut().and_then(|s| s.plugin.save_state()) {
                    out.push((*tid, i, data));
                }
            }
            if let Some(data) = st.instrument.as_mut().and_then(|s| s.plugin.save_state()) {
                out.push((*tid, INSTRUMENT_SLOT, data));
            }
        }
        out.sort_by_key(|(t, s, _)| (*t, *s));
        out
    }

    /// Capture every required active hosted slot for an export. Disabled tracks/slots and
    /// built-ins do not require opaque state; enabled bypassed slots still retain their state.
    /// Unlike legacy saving, an unavailable state is an error rather than a silent omission.
    /// This exceptional control request calls vendor code and allocates; limits apply after
    /// a vendor returns its blob and cannot bound its internal allocation or execution time.
    pub fn try_capture_states(&mut self) -> Result<Vec<(TrackId, usize, Vec<u8>)>, PreparationError> {
        if let Some(error) = self.installation_error {
            return Err(error);
        }
        let source = Arc::clone(self.prepared.as_ref().ok_or(PreparationError("no prepared mix snapshot"))?.source());
        let mut required = 0usize;
        for track in source.tracks.iter().filter(|track| !track.inactive) {
            let inserts = track.mixer.inserts.iter().flatten().filter(|insert| insert.active && is_third_party(&insert.plugin)).count();
            let instrument = usize::from(track.instrument.as_ref().is_some_and(|insert| insert.active && is_third_party(&insert.plugin)));
            required = required
                .checked_add(inserts)
                .and_then(|count| count.checked_add(instrument))
                .filter(|count| *count <= MAX_EXPORT_STATE_SLOTS)
                .ok_or(PreparationError("export plugin state slot limit exceeded"))?;
        }
        let mut out = Vec::new();
        out.try_reserve_exact(required).map_err(|_| PreparationError("export plugin state allocation failed"))?;
        let mut bytes = 0usize;
        for track in source.tracks.iter().filter(|track| !track.inactive) {
            for (index, insert) in track.mixer.inserts.iter().enumerate() {
                let Some(insert) = insert.as_ref().filter(|insert| insert.active && is_third_party(&insert.plugin)) else { continue };
                let slot = self
                    .strips
                    .get_mut(&track.id)
                    .and_then(|strip| strip.plugins.get_mut(index))
                    .and_then(Option::as_mut)
                    .filter(|slot| slot.id == insert.plugin)
                    .ok_or(PreparationError("required export plugin instance is unavailable"))?;
                let data = slot.plugin.save_state().ok_or(PreparationError("required export plugin state is unavailable"))?;
                admit_export_state(&data, &mut bytes)?;
                out.push((track.id, index, data));
            }
            if let Some(insert) = track.instrument.as_ref().filter(|insert| insert.active && is_third_party(&insert.plugin)) {
                let slot = self
                    .strips
                    .get_mut(&track.id)
                    .and_then(|strip| strip.instrument.as_mut())
                    .filter(|slot| slot.id == insert.plugin)
                    .ok_or(PreparationError("required export instrument instance is unavailable"))?;
                let data = slot.plugin.save_state().ok_or(PreparationError("required export instrument state is unavailable"))?;
                admit_export_state(&data, &mut bytes)?;
                out.push((track.id, INSTRUMENT_SLOT, data));
            }
        }
        out.sort_by_key(|(track, slot, _)| (*track, *slot));
        Ok(out)
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// Format of the main mix as of the last sync.
    pub fn main_format(&self) -> ChannelFormat {
        self.main_fmt
    }

    /// Channels of the main mix as of the last sync.
    pub fn main_channels(&self) -> usize {
        self.main.len()
    }

    /// Reset all plugin state (on stop / seek).
    pub fn reset(&mut self) {
        self.source_cutoff_started = false;
        for s in self.strips.values_mut() {
            if let Some(edge) = &mut s.output_edge {
                edge.align.clear();
            }
            for edge in s.send_edges.iter_mut().flatten() {
                edge.align.clear();
            }
            for fx in s.clip_fx.values_mut() {
                fx.reset_alignment();
            }
            if self.source_cutoff.is_some() {
                s.skip_source = false;
                for fx in s.clip_fx.values_mut() {
                    fx.reset_window();
                }
            }
            for p in s.plugins.iter_mut().flatten() {
                p.plugin.reset();
                p.automation.invalidate();
            }
            if let Some(i) = &mut s.instrument {
                i.plugin.all_notes_off();
                i.plugin.reset();
                i.automation.invalidate();
            }
            s.held.clear();
        }
        for meter in self.meters.values_mut() {
            meter.peak = [0.0; 2];
            meter.peaks.fill(0.0);
            meter.gain_reduction_db = 0.0;
        }
        self.main_meter.peak = [0.0; 2];
        self.main_meter.peaks.fill(0.0);
        self.main_meter.gain_reduction_db = 0.0;
    }

    /// Bring plugin instances in line with the session (creates/destroys as needed). Allocates;
    /// call outside the realtime thread when the document changes, or let `render` do it.
    pub fn sync(&mut self, s: &Session) {
        let mb = self.max_block;
        let main_ch = main_channels(s);
        self.main_fmt = s.main_format();
        if self.main.len() != main_ch {
            self.main = vec![vec![0.0; mb]; main_ch];
        }
        let mut cx = SlotCtx { sr: self.sample_rate, mb, external: self.external, adopt: &mut self.adopt, retired: &mut self.retired };
        let gone: Vec<TrackId> = self.strips.keys().filter(|id| s.track(**id).is_none()).copied().collect();
        for id in gone {
            if let Some(mut st) = self.strips.remove(&id) {
                for p in st.plugins.drain(..) {
                    cx.retire(p);
                }
                cx.retire(st.instrument.take());
            }
        }
        for t in &s.tracks {
            let ch = strip_channels(t, main_ch);
            let strip = self.strips.entry(t.id).or_insert_with(Strip::new);
            if strip.buf.len() != ch || strip.buf.first().map_or(0, Vec::len) != mb {
                strip.buf = vec![vec![0.0; mb]; ch];
            }
            while strip.plugins.len() > t.mixer.inserts.len() {
                let p = strip.plugins.pop().flatten();
                cx.retire(p);
            }
            strip.plugins.resize_with(t.mixer.inserts.len(), || None);
            for (idx, (slot, ins)) in strip.plugins.iter_mut().zip(t.mixer.inserts.iter()).enumerate() {
                match ins {
                    Some(i) if slot.as_ref().is_none_or(|p| p.id != i.plugin || p.ch != ch) => {
                        let old = slot.take();
                        cx.retire(old);
                        *slot = cx.make(t.id, idx, i, ch);
                    }
                    None => {
                        let old = slot.take();
                        cx.retire(old);
                    }
                    _ => {}
                }
            }
            let want = t.instrument.as_ref();
            let ich = ch.max(2);
            if strip.instrument.as_ref().map(|p| (p.id.as_str(), p.ch)) != want.map(|i| (i.plugin.as_str(), ich)) {
                let old = strip.instrument.take();
                cx.retire(old);
                strip.instrument = want.and_then(|i| cx.make(t.id, INSTRUMENT_SLOT, i, ich));
                strip.instrument_enabled = false;
                strip.instrument_release_pending = strip.instrument.is_some() && want.is_some_and(|insert| !insert.active || insert.bypass);
                strip.sample = None;
            }
            sync_sample(strip, want, s);
        }
        // Instances nobody claimed (the session changed again before they arrived).
        let leftovers: Vec<(TrackId, usize)> = cx.adopt.keys().copied().collect();
        for k in leftovers {
            if let Some(p) = cx.adopt.remove(&k) {
                cx.retired.push(p.plugin);
            }
        }
        if !self.external {
            self.retired.clear();
        }
        for b in &s.busses {
            let n = b.format.channels().clamp(1, MAX_CHANNELS);
            let bus = self.busses.entry(b.id).or_insert_with(|| BusBuf { fmt: b.format, buf: Vec::new() });
            bus.fmt = b.format;
            if bus.buf.len() != n {
                bus.buf = vec![vec![0.0; mb]; n];
            }
        }
        self.busses.retain(|id, _| s.bus(*id).is_some());
    }

    /// Re-syncs plugin instances and the routing order when the session's structure changed
    /// since the last sync (or instances were adopted). `render` calls it; a realtime host may
    /// also call it right after swapping sessions so slots are filled before playback starts.
    pub fn ensure_synced(&mut self, s: &Session) {
        let key = (structure_fingerprint(s), s.tracks.len(), s.busses.len());
        if key != self.synced {
            self.sync(s);
            self.order = processing_order(s);
            self.synced = key;
        }
    }

    /// Render `frames` (≤ max_block) samples starting at `pos` into `out` (≥ 2 channels).
    pub fn render(&mut self, s: &Session, pos: Samples, frames: usize, out: &mut [Vec<f32>]) {
        if on_audio_thread() {
            self.render_error = Some(PreparationError("unprepared render on audio thread"));
            for channel in out {
                channel.iter_mut().take(frames).for_each(|v| *v = 0.0);
            }
            return;
        }
        if let Err(error) = self.prepare_compatibility(s) {
            self.render_error = Some(error);
            for channel in out {
                channel.iter_mut().take(frames).for_each(|v| *v = 0.0);
            }
            return;
        }
        self.render_core(s, pos, frames, out);
    }

    fn render_core(&mut self, s: &Session, pos: Samples, frames: usize, out: &mut [Vec<f32>]) {
        self.render_error = self.installation_error;
        let frames = frames.min(self.max_block);
        if pos != self.last_end {
            for strip in self.strips.values_mut() {
                for slot in strip.plugins.iter_mut().flatten() {
                    slot.automation.invalidate();
                }
            }
            // Seek: silence held notes and ringing plugins.
            for st in self.strips.values_mut() {
                if let Some(edge) = &mut st.output_edge {
                    edge.align.clear();
                }
                for edge in st.send_edges.iter_mut().flatten() {
                    edge.align.clear();
                }
                for fx in st.clip_fx.values_mut() {
                    fx.reset_alignment();
                }
                if let Some(i) = &mut st.instrument {
                    i.plugin.all_notes_off();
                }
                st.held.clear();
            }
        }
        self.last_end = pos.saturating_add(frames as i64);
        for b in self.busses.values_mut() {
            for c in b.buf.iter_mut() {
                c.iter_mut().take(frames).for_each(|x| *x = 0.0);
            }
        }
        for c in &mut self.main {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
        let any_solo = s.tracks.iter().any(|t| t.mixer.solo && !t.inactive);
        let order = std::mem::take(&mut self.order);
        let live = |t: &Track| !(t.kind == TrackKind::Master || t.inactive || t.kind == TrackKind::Vca);
        let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
        // Phase 1: independent strips (audio, instrument, MIDI) in parallel.
        let mut work = std::mem::take(&mut self.work);
        work.clear();
        for &i in &order {
            let Some(t) = s.tracks.get(i).filter(|t| live(t) && !is_aux(t)) else { continue };
            if let Some(st) = self.strips.remove(&t.id) {
                work.push((i, st));
            }
        }
        let busses = std::mem::take(&mut self.busses);
        let live_in = LiveInput { input: &self.input, monitor_only: self.monitor_only, recording: self.recording };
        run_strips(&mut work, |(i, st)| {
            if let Some(t) = s.tracks.get(*i) {
                process_strip(s, t, st, &busses, &live_in, pos, frames, any_solo);
            }
        });
        self.busses = busses;
        let pdc_enabled = s.edit.delay_compensation;
        self.track_latency = if pdc_enabled { work.iter().map(|(_, strip)| strip.latency).max().unwrap_or(0) } else { 0 };
        match self.pdc.as_mut() {
            Some(plan) => {
                if let Err(error) = plan.project(s, &self.strips, &work, self.sample_rate) {
                    self.render_error = Some(error);
                }
                match plan.output_latency(None) {
                    Ok(latency) => self.latency = latency,
                    Err(error) => self.render_error = Some(error),
                }
            }
            None => self.render_error = Some(PreparationError("no prepared compensation plan")),
        }
        for (i, st) in work.iter_mut() {
            if let Some(t) = s.tracks.get(*i) {
                self.route_strip(t, st, pos, frames);
            }
        }
        for (i, st) in work.drain(..) {
            if st.error.is_some() {
                self.render_error = st.error;
            }
            if let Some(t) = s.tracks.get(i) {
                self.strips.insert(t.id, st);
            }
        }
        self.work = work;
        let is_aux = |t: &Track| matches!(t.kind, TrackKind::Aux | TrackKind::Folder);
        // Phase 2: auxes in dependency order (they read busses).
        for t in order.iter().filter_map(|&i| s.tracks.get(i)).filter(|t| live(t)).filter(|t| is_aux(t)) {
            let Some(mut st) = self.strips.remove(&t.id) else { continue };

            let live_in = LiveInput { input: &self.input, monitor_only: self.monitor_only, recording: self.recording };
            process_strip(s, t, &mut st, &self.busses, &live_in, pos, frames, any_solo);
            if pdc_enabled {
                match self.pdc.as_ref().ok_or(PreparationError("no prepared compensation plan")).and_then(|plan| plan.expected_aux_latency(t.id)) {
                    Ok(expected) if expected == st.latency => {}
                    Ok(_) => st.error = Some(PreparationError("aux processor latency changed within the render span")),
                    Err(error) => st.error = Some(error),
                }
            }
            self.route_strip(t, &mut st, pos, frames);
            if st.error.is_some() {
                self.render_error = st.error;
            }
            self.strips.insert(t.id, st);
        }
        self.order = order;
        // Master inserts are post-fader; ordinary strip inserts remain pre-fader.
        let mut master_latency = 0usize;
        for t in s.tracks.iter().filter(|t| t.kind == TrackKind::Master && !t.inactive) {
            if let Some(strip) = self.strips.get_mut(&t.id) {
                strip.error = None;
                let auto = &strip.automation;
                for (dst, src) in strip.buf.iter_mut().zip(self.main.iter()) {
                    if let (Some(d), Some(sr)) = (dst.get_mut(..frames), src.get(..frames)) {
                        for (i, (d, x)) in d.iter_mut().zip(sr).enumerate() {
                            *d = x * db_to_gain(auto.volume(t.mixer.volume_db, automation::position(pos, i)));
                        }
                    }
                }
                for (i, ins) in t.mixer.inserts.iter().enumerate() {
                    if let (Some(ins), Some(Some(slot))) = (ins, strip.plugins.get_mut(i))
                        && ins.active
                        && !ins.bypass
                        && slot.automation.process(slot.plugin.as_mut(), &mut strip.buf, pos, frames) != soundcraft_dsp::ProcessDelivery::Delivered
                    {
                        strip.error = Some(PreparationError("master plugin target delivery failed"));
                        for channel in &mut strip.buf {
                            channel.iter_mut().take(frames).for_each(|v| *v = 0.0);
                        }
                    }
                }
                match checked_insert_latency(t, strip, self.sample_rate) {
                    Ok(latency) => {
                        if let Some(total) = master_latency.checked_add(latency).filter(|latency| *latency <= (1 << 20)) {
                            master_latency = total;
                        } else {
                            strip.error = Some(PreparationError("master latency exceeds prepared limit"));
                        }
                    }
                    Err(error) => strip.error = Some(error),
                }
                for (dst, src) in self.main.iter_mut().zip(strip.buf.iter()) {
                    if let (Some(d), Some(sr)) = (dst.get_mut(..frames), src.get(..frames)) {
                        d.copy_from_slice(sr);
                    }
                }
                if strip.error.is_some() {
                    self.render_error = strip.error;
                }
                if let Some(meter) = self.meters.get_mut(&t.id) {
                    meter.measure(&self.main, frames, 0.0);
                }
            }
        }
        if pdc_enabled && self.pdc.as_ref().is_none_or(|plan| plan.master_latency() != master_latency) {
            self.render_error = Some(PreparationError("Master processor latency changed within the render span"));
        }
        // Metronome: summed after the master fader, so it reaches the speakers without passing
        // through any track, send or insert. Only the playback engine sets `metronome`.
        // Not while monitoring only (transport stopped): the position does not advance there, so the
        // start of the same beat would repeat every block as a buzz.
        if self.metronome && !self.monitor_only && s.edit.click && (!s.edit.flag("click.only_during_record") || self.recording) {
            click::render(s, pos, frames, &mut self.main);
        }
        // Output: min(out.len(), main channels) channels; any further output channels are silent.
        for (c, o) in out.iter_mut().enumerate() {
            let src = self.main.get(c);
            for i in 0..frames {
                let v = if self.render_error.is_some() { 0.0 } else { src.and_then(|m| m.get(i)).copied().unwrap_or(0.0) };
                if let Some(d) = o.get_mut(i) {
                    *d = if v.is_finite() { v } else { 0.0 };
                }
            }
        }
        self.main_meter.measure(&self.main, frames, 0.0);
    }

    /// Copy one rendered block of `bus` into `out`. A missing bus stays silent.
    fn copy_bus(&self, bus: BusId, frames: usize, out: &mut [Vec<f32>]) {
        for o in out.iter_mut() {
            for x in o.iter_mut().take(frames) {
                *x = 0.0;
            }
        }
        let Some(src) = self.busses.get(&bus) else { return };
        for (o, c) in out.iter_mut().zip(src.buf.iter()) {
            for i in 0..frames {
                let v = c.get(i).copied().unwrap_or(0.0);
                if let Some(d) = o.get_mut(i) {
                    *d = if v.is_finite() { v } else { 0.0 };
                }
            }
        }
    }

    /// Pan/gain each route before its independently prepared destination-width delay.
    fn route_strip(&mut self, t: &Track, strip: &mut Strip, pos: Samples, frames: usize) {
        if let Some(meter) = self.meters.get_mut(&t.id) {
            meter.measure(&strip.buf, frames, strip.gr);
        }
        let src_fmt = strip_format(t, strip.buf.len());
        let law = self.pan_law;
        for (index, snd) in t.mixer.sends.iter().enumerate() {
            let Some(snd) = snd else { continue };
            let Some(Some(edge)) = strip.send_edges.get_mut(index) else { continue };
            let pdc::Destination::Bus(bus) = edge.target else { continue };
            let Some(dst) = self.busses.get_mut(&bus) else { continue };
            for channel in &mut edge.scratch {
                channel.iter_mut().take(frames).for_each(|value| *value = 0.0);
            }
            let src = if snd.pre_fader { &strip.pre } else { &strip.buf };
            let send = strip.automation.sends.get(index);
            let surround = snd.surround.as_ref().or(if snd.follow_main_pan { t.mixer.surround.as_ref() } else { None });
            for frame in 0..frames {
                let at = automation::position(pos, frame);
                let mute = strip.automation.send_value(send.and_then(|send| send.mute.as_ref()), f32::from(u8::from(snd.mute)), at) >= 0.5;
                if mute {
                    continue;
                }
                let gain = db_to_gain(strip.automation.send_value(send.and_then(|send| send.level.as_ref()), snd.level_db, at));
                let pan = strip.automation.send_value(send.and_then(|send| send.pan.as_ref()), snd.pan, at).clamp(-1.0, 1.0);
                let stereo = if src.len() == 1 { [pan, 0.0] } else { [-1.0, 1.0] };
                let mut matrix = build_matrix(src_fmt, src.len(), dst.fmt, edge.scratch.len(), &PanSpec { stereo, surround }, law);
                if src.len() == 2 && !src_fmt.is_ambisonic() {
                    for (row, balance) in matrix.iter_mut().zip([(1.0 - pan).min(1.0), (1.0 + pan).min(1.0)]) {
                        for value in row {
                            *value *= balance;
                        }
                    }
                }
                apply_matrix_frame(src, frame, &mut edge.scratch, &matrix, gain);
            }
            if let Err(error) = align_route(self.pdc.as_ref(), edge, frames) {
                strip.error = Some(error);
            }
            sum_route(&edge.scratch, &mut dst.buf, frames);
        }
        let Some(edge) = &mut strip.output_edge else { return };
        let (format, destination) = match edge.target {
            pdc::Destination::Main => (self.main_fmt, &mut self.main),
            pdc::Destination::Bus(bus) => {
                let Some(dst) = self.busses.get_mut(&bus) else { return };
                (dst.fmt, &mut dst.buf)
            }
        };
        for channel in &mut edge.scratch {
            channel.iter_mut().take(frames).for_each(|value| *value = 0.0);
        }
        for frame in 0..frames {
            let at = automation::position(pos, frame);
            let stereo = std::array::from_fn(|index| {
                let base = t.mixer.pan.get(index).copied().unwrap_or(if index == 0 { 0.0 } else { 1.0 });
                strip.automation.pan(index, base, at)
            });
            let matrix =
                build_matrix(src_fmt, strip.buf.len(), format, edge.scratch.len(), &PanSpec { stereo, surround: t.mixer.surround.as_ref() }, law);
            apply_matrix_frame(&strip.buf, frame, &mut edge.scratch, &matrix, 1.0);
        }
        if let Err(error) = align_route(self.pdc.as_ref(), edge, frames) {
            strip.error = Some(error);
        }
        sum_route(&edge.scratch, destination, frames);
    }
}

fn align_route(plan: Option<&pdc::Plan>, edge: &mut pdc::Edge, frames: usize) -> Result<(), PreparationError> {
    let delay = plan.ok_or(PreparationError("no prepared compensation plan"))?.edge_delay(edge)?;
    if !edge.align.set(delay, edge.scratch.len()) {
        return Err(PreparationError("route PDC exceeds prepared capacity"));
    }
    edge.align.process(&mut edge.scratch, frames);
    Ok(())
}

fn sum_route(source: &[Vec<f32>], destination: &mut [Vec<f32>], frames: usize) {
    for (destination, source) in destination.iter_mut().zip(source) {
        for (destination, source) in destination.iter_mut().zip(source).take(frames) {
            *destination += *source;
        }
    }
}

fn admit_export_state(data: &[u8], total: &mut usize) -> Result<(), PreparationError> {
    if data.len() > soundcraft_model::b64::MAX_DECODED_BYTES {
        return Err(PreparationError("export plugin state exceeds the session blob limit"));
    }
    *total = total
        .checked_add(data.len())
        .filter(|total| *total <= MAX_EXPORT_STATE_BYTES)
        .ok_or(PreparationError("export plugin state byte limit exceeded"))?;
    Ok(())
}

fn checked_insert_latency(track: &Track, strip: &Strip, sample_rate: f32) -> Result<usize, PreparationError> {
    track.mixer.inserts.iter().enumerate().try_fold(0usize, |latency, (index, insert)| {
        let Some(insert) = insert.as_ref().filter(|insert| insert.active && !insert.bypass) else { return Ok(latency) };
        // Legacy compatibility renderers deliberately bypass a missing processor. Strict
        // preparation rejects it before publication, so no typed window takes this path.
        let Some(slot) = strip.plugins.get(index).and_then(Option::as_ref) else { return Ok(latency) };
        if slot.id != insert.plugin {
            return Err(PreparationError("latency plugin identity does not match the document"));
        }
        let next = slot.automation.static_latency(sample_rate).unwrap_or_else(|| slot.plugin.latency());
        latency.checked_add(next).filter(|latency| *latency <= (1 << 20)).ok_or(PreparationError("insert latency exceeds prepared limit"))
    })
}

/// Source → trim → inserts → (pre-fader copy) → mute/fader for one strip. Independent of other
/// strips except that aux inputs read `busses`, so non-aux strips can run in parallel.
/// Live input routed to monitored tracks.
struct LiveInput<'a> {
    input: &'a [Vec<f32>],
    monitor_only: bool,
    recording: bool,
}

/// Uses the already prepared stereo scratch for nominal mono instruments. Both ordinary
/// processing and a disabled transition deliver the same actual channel shape.
fn process_instrument_span(
    slot: &mut PluginSlot,
    buffer: &mut [Vec<f32>],
    scratch: &mut Vec<f32>,
    pos: Samples,
    frames: usize,
) -> soundcraft_dsp::ProcessDelivery {
    if buffer.len() >= 2 {
        return slot.automation.process(slot.plugin.as_mut(), buffer, pos, frames);
    }
    let Some(mono) = buffer.first_mut() else { return soundcraft_dsp::ProcessDelivery::InvalidTarget };
    let mut stereo = [std::mem::take(mono), std::mem::take(scratch)];
    let receipt = slot.automation.process(slot.plugin.as_mut(), &mut stereo, pos, frames);
    let [left, right] = stereo;
    *mono = left;
    *scratch = right;
    for (left, right) in mono.iter_mut().zip(scratch.iter()).take(frames) {
        *left = 0.5 * (*left + *right);
    }
    receipt
}

fn process_strip(
    s: &Session,
    t: &Track,
    strip: &mut Strip,
    busses: &HashMap<BusId, BusBuf>,
    live: &LiveInput<'_>,
    pos: Samples,
    frames: usize,
    any_solo: bool,
) {
    strip.error = None;
    for c in strip.buf.iter_mut() {
        c.iter_mut().take(frames).for_each(|x| *x = 0.0);
    }
    match t.kind {
        TrackKind::Audio
            if !strip.skip_source
                && !live.input.is_empty()
                && (t.mixer.input_monitor || (t.mixer.record_arm && (live.monitor_only || live.recording))) =>
        {
            // Input monitoring: the track hears its hardware input instead of its clips.
            let first = match &t.mixer.input {
                Route::Hardware(h) => {
                    h.trim_start_matches("In ").split(['-', ' ']).next().and_then(|n| n.parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1))
                }
                _ => 0,
            };
            let n_in = live.input.len().max(1);
            for (k, dst) in strip.buf.iter_mut().enumerate() {
                if let Some(src) = live.input.get((first + k) % n_in) {
                    for (d, x) in dst.iter_mut().zip(src.iter()).take(frames) {
                        *d = *x;
                    }
                }
            }
        }
        TrackKind::Audio if live.monitor_only => {}
        TrackKind::Instrument | TrackKind::Midi if live.monitor_only => {}
        TrackKind::Audio if !strip.skip_source => {
            let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
            for clip in t.clips() {
                if clip.muted || !clip.is_audio() || !clip.range().overlaps(&block) {
                    continue;
                }
                if let Some(fx) = strip.clip_fx.get_mut(&clip.id.0) {
                    for channel in &mut strip.clipbuf {
                        channel.iter_mut().take(frames).for_each(|v| *v = 0.0);
                    }
                    render_clip(s, clip, pos, frames, &mut strip.clipbuf, &mut strip.scratch);
                    fx.process(&mut strip.clipbuf, frames);
                    fx.window_active |= strip.source_window;
                    fx.window_block = Some(pos);
                    let consumed = usize::try_from(block.end.saturating_sub(clip.range().end).max(0)).unwrap_or(usize::MAX);
                    fx.flush = strip.clip_latency.saturating_sub(consumed);
                    for (dst, src) in strip.buf.iter_mut().zip(&strip.clipbuf) {
                        for (dst, src) in dst.iter_mut().zip(src).take(frames) {
                            *dst += *src;
                        }
                    }
                } else {
                    render_clip(s, clip, pos, frames, &mut strip.buf, &mut strip.scratch);
                }
            }
        }
        TrackKind::Aux | TrackKind::Folder => {
            if let Route::Bus(b) = &t.mixer.input
                && let Some(src) = busses.get(b)
            {
                let (ns, nd) = (src.buf.len(), strip.buf.len());
                let dst_fmt = strip_format(t, nd);
                let m = if src.fmt == dst_fmt && ns == nd {
                    identity_matrix(ns)
                } else if ns == 1 && nd == 2 {
                    // A mono bus feeds both sides of a stereo aux.
                    let mut m = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
                    m[0][0] = 1.0;
                    m[0][1] = 1.0;
                    m
                } else {
                    let stereo = if ns == 1 { [0.0, 0.0] } else { [-1.0, 1.0] };
                    build_matrix(src.fmt, ns, dst_fmt, nd, &PanSpec { stereo, surround: None }, PanLaw::Minus3)
                };
                apply_matrix(&src.buf, frames, &mut strip.buf, &m, 1.0);
            }
        }
        TrackKind::Instrument | TrackKind::Midi => {
            let enabled = t.instrument.as_ref().is_some_and(|insert| insert.active && !insert.bypass);
            if strip.instrument_enabled && !enabled {
                strip.instrument_enabled = false;
                strip.instrument_release_pending = true;
            }
            if let Some(inst) = &mut strip.instrument {
                if strip.instrument_release_pending {
                    // Host reset alone is insufficient for VST3: it clears the pending
                    // note queue without a processor reset. Deliver one natural silent
                    // span of offs before reset; never acknowledge a rejected release.
                    // All pitches also cover a newly restored opaque voice not in our
                    // held-key list. These are offs, never invented note-ons/chases.
                    for pitch in 0..128u8 {
                        inst.plugin.note_off(0, pitch);
                    }
                    inst.plugin.all_notes_off();
                    let receipt = process_instrument_span(inst, &mut strip.buf, &mut strip.scratch, pos, frames);
                    for channel in &mut strip.buf {
                        channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
                    }
                    if receipt == soundcraft_dsp::ProcessDelivery::Delivered {
                        inst.plugin.reset();
                        inst.automation.invalidate();
                        strip.held.clear();
                        strip.instrument_release_pending = false;
                    } else {
                        strip.error = Some(PreparationError("instrument note release delivery failed"));
                    }
                    // Even if re-enabled while a failed release was pending, this span
                    // stays silent. Only a subsequent span can schedule future notes.
                } else if enabled {
                    strip.instrument_enabled = true;
                    // #152: push the instrument's stored parameter values into the live plugin
                    // before processing, the same way inserts do below. Without this, the values
                    // loaded from a session (or set via `mix.insert_state`) never reach the plugin
                    // instance and the rendered audio matches the plugin's defaults — e.g. a saved
                    // `level = -12 dB` sounds the same as `level = -6 dB`.
                    if !strip.skip_source
                        && let Err(error) = schedule_notes(s, t, pos, frames, inst, &mut strip.held, strip.rtp)
                    {
                        strip.error = Some(error);
                    }
                    if process_instrument_span(inst, &mut strip.buf, &mut strip.scratch, pos, frames) != soundcraft_dsp::ProcessDelivery::Delivered {
                        strip.error = Some(PreparationError("instrument target delivery failed"));
                    }
                }
            }
        }
        _ => {}
    }
    if t.kind == TrackKind::Audio {
        for fx in strip.clip_fx.values_mut().filter(|fx| (fx.window_active || fx.flush > 0) && fx.window_block != Some(pos)) {
            for channel in &mut strip.clipbuf {
                channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
            }
            fx.process(&mut strip.clipbuf, frames);
            fx.window_block = Some(pos);
            fx.flush = fx.flush.saturating_sub(frames);
            for (dst, src) in strip.buf.iter_mut().zip(&strip.clipbuf) {
                for (dst, src) in dst.iter_mut().zip(src).take(frames) {
                    *dst += *src;
                }
            }
        }
    }
    let trim = db_to_gain(t.mixer.trim_db) * if t.mixer.phase_invert { -1.0 } else { 1.0 };
    if (trim - 1.0).abs() > f32::EPSILON {
        for c in strip.buf.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x *= trim);
        }
    }
    let mut gr = 0.0f32;
    let mut latency =
        if matches!(t.kind, TrackKind::Instrument | TrackKind::Midi) && t.instrument.as_ref().is_some_and(|insert| insert.active && !insert.bypass) {
            strip.instrument.as_ref().map_or(0, |instrument| instrument.plugin.latency())
        } else {
            strip.clip_latency
        };
    for (i, ins) in t.mixer.inserts.iter().enumerate() {
        let (Some(ins), Some(Some(slot))) = (ins, strip.plugins.get_mut(i)) else { continue };
        if !ins.active || ins.bypass {
            continue;
        }
        if slot.automation.process(slot.plugin.as_mut(), &mut strip.buf, pos, frames) != soundcraft_dsp::ProcessDelivery::Delivered {
            strip.error = Some(PreparationError("insert target delivery failed"));
            for channel in &mut strip.buf {
                channel.iter_mut().take(frames).for_each(|v| *v = 0.0);
            }
        }
        match latency.checked_add(slot.plugin.latency()).filter(|latency| *latency <= (1 << 20)) {
            Some(total) => latency = total,
            None => strip.error = Some(PreparationError("strip latency exceeds prepared limit")),
        }
        if gr == 0.0 {
            gr = slot.plugin.gain_reduction_db();
        }
    }
    strip.gr = gr;
    strip.latency = latency;
    let soloed_out = any_solo && !t.mixer.solo && !t.mixer.solo_safe && !receives_solo(s, t);
    strip.muted = t.mixer.mute || soloed_out;
    if t.mixer.sends.iter().flatten().any(|x| x.pre_fader) {
        for (dst, src) in strip.pre.iter_mut().zip(&strip.buf) {
            if let (Some(dst), Some(src)) = (dst.get_mut(..frames), src.get(..frames)) {
                dst.copy_from_slice(src);
            }
        }
    }
    let vca = t.mixer.vca.and_then(|id| s.tracks.iter().find(|vca| vca.id.0 == id && vca.kind == TrackKind::Vca));
    // VCA metadata is prepared before rendering; no point scans occur on the audio thread.
    for i in 0..frames {
        let at = automation::position(pos, i);
        let vca_mute = strip.vca.as_ref().is_some_and(|(_, auto)| auto.mute(at)) || vca.is_some_and(|vca| vca.mixer.mute);
        let mute = strip.muted || strip.automation.mute(at) || vca_mute || strip.error.is_some();
        let mut volume = if strip.skip_volume { 0.0 } else { strip.automation.volume(t.mixer.volume_db, at) + strip.automation.trim(at) };
        if !strip.skip_volume
            && let Some((base, auto)) = &strip.vca
        {
            volume += auto.volume(*base, at);
        }
        let gain = if mute { 0.0 } else { db_to_gain(volume.min(12.0)) };
        for channel in &mut strip.buf {
            if let Some(sample) = channel.get_mut(i) {
                *sample *= gain;
            }
        }
        // Track mute also gates pre-fader sends; fader/VCA gain do not.
        if mute {
            for channel in &mut strip.pre {
                if let Some(sample) = channel.get_mut(i) {
                    *sample = 0.0;
                }
            }
        }
    }
}

/// Clip-effect parameters of a clip, or None when it has none or they are bypassed.
fn clip_fx_params(s: &Session, clip: u64) -> Option<Vec<(String, f32)>> {
    if s.edit.values.is_empty() {
        return None;
    }
    let pre = format!("clip_fx.{clip}.");
    let v: Vec<(String, f32)> = s
        .edit
        .values
        .range(pre.clone()..)
        .take_while(|(k, _)| k.starts_with(&pre))
        .map(|(k, v)| (k.get(pre.len()..).unwrap_or("").to_string(), *v as f32))
        .collect();
    if v.is_empty() || s.edit.flag(&format!("clip_fx.bypass.{clip}")) { None } else { Some(v) }
}

impl ClipFx {
    fn new(sr: f32, ch: usize) -> ClipFx {
        ClipFx {
            eq: None,
            comp: None,
            gain_db: 0.0,
            configured: Vec::new(),
            window_active: false,
            window_block: None,
            align: DelayLine::default(),
            flush: 0,
            sr,
            ch,
        }
    }
    fn make(&self, id: &str) -> Option<Box<dyn Plugin>> {
        soundcraft_dsp::create(id).map(|mut p| {
            p.prepare(self.sr, 16_384, self.ch.max(1));
            p
        })
    }
    /// Modules exist only when one of their parameters is set.
    fn configure(&mut self, params: &[(String, f32)]) {
        self.configured.clear();
        self.configured.extend(params.iter().cloned());
        self.gain_db = 0.0;
        let has_eq = params.iter().any(|(k, _)| k.starts_with("eq."));
        let has_comp = params.iter().any(|(k, _)| k.starts_with("comp."));
        if has_eq && self.eq.is_none() {
            self.eq = self.make("eq_7band");
        } else if !has_eq {
            self.eq = None;
        }
        if has_comp && self.comp.is_none() {
            self.comp = self.make("compressor");
        } else if !has_comp {
            self.comp = None;
        }
        for (k, v) in params {
            if let Some(p) = k.strip_prefix("eq.") {
                if let Some(eq) = &mut self.eq
                    && eq.param(p) != Some(*v)
                {
                    eq.set_param(p, *v);
                }
            } else if let Some(p) = k.strip_prefix("comp.") {
                if let Some(c) = &mut self.comp
                    && c.param(p) != Some(*v)
                {
                    c.set_param(p, *v);
                }
            } else if k == "gain" {
                self.gain_db = *v;
            }
        }
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if let Some(eq) = &mut self.eq {
            eq.process(io, frames);
        }
        if let Some(c) = &mut self.comp {
            c.process(io, frames);
        }
        self.align.process(io, frames);
        if self.gain_db.abs() > 1e-6 {
            let g = db_to_gain(self.gain_db);
            for ch in io.iter_mut() {
                ch.iter_mut().take(frames).for_each(|x| *x *= g);
            }
        }
    }
    fn latency(&self) -> Result<usize, PreparationError> {
        self.eq
            .as_ref()
            .map_or(0, |plugin| plugin.latency())
            .checked_add(self.comp.as_ref().map_or(0, |plugin| plugin.latency()))
            .filter(|latency| *latency <= (1 << 20))
            .ok_or(PreparationError("ClipFX latency exceeds prepared limit"))
    }
    fn reset_alignment(&mut self) {
        self.align.clear();
        self.flush = 0;
        self.window_active = false;
        self.window_block = None;
        if let Some(comp) = &mut self.comp
            && comp.latency() > 0
        {
            comp.reset();
        }
    }
    fn reset_window(&mut self) {
        self.align.clear();
        self.flush = 0;
        self.window_active = false;
        self.window_block = None;
        if let Some(eq) = &mut self.eq {
            eq.reset();
        }
        if let Some(comp) = &mut self.comp {
            comp.reset();
        }
    }
}

/// Hash of everything `sync`/`processing_order` depend on (tracks, kinds, routing, plugins, sends).
fn structure_fingerprint(s: &Session) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for t in &s.tracks {
        t.id.hash(&mut h);
        t.kind.hash(&mut h);
        t.format.hash(&mut h);
        t.inactive.hash(&mut h);
        t.mixer.inserts.len().hash(&mut h);
        t.mixer.input.hash(&mut h);
        t.mixer.output.hash(&mut h);
        for i in &t.mixer.inserts {
            i.as_ref().map(|x| x.plugin.as_str()).hash(&mut h);
        }
        for snd in t.mixer.sends.iter().flatten() {
            snd.target.hash(&mut h);
        }
        t.instrument.as_ref().map(|x| x.plugin.as_str()).hash(&mut h);
        let sample = t.instrument.as_ref().and_then(|x| x.sample);
        sample.hash(&mut h);
        // Which decoded audio it is, so new audio under the same id (another session) re-syncs.
        sample.and_then(|id| s.pool.get(id)).map(|a| Arc::as_ptr(a) as usize).hash(&mut h);
    }
    for b in &s.busses {
        b.id.hash(&mut h);
        b.format.hash(&mut h);
    }
    s.main_format().hash(&mut h);
    h.finish() as usize
}

std::thread_local! {
    static AUDIO_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the calling thread as the realtime audio thread (`soundcraft_playback`'s cpal callbacks
/// do, every call: one thread-local store). Strips [`MixEngine::render`] hands to worker threads
/// count as the audio thread while they render for it. Code that might run there asks
/// [`on_audio_thread`] before doing anything that blocks; the desktop app's logger does, so a
/// `log::` record from the audio thread never takes a lock or touches a file.
pub fn mark_audio_thread() {
    AUDIO_THREAD.set(true);
}

/// True on the thread [`mark_audio_thread`] marked, and on a worker while it renders strips for
/// it. Lock-free and allocation-free.
pub fn on_audio_thread() -> bool {
    AUDIO_THREAD.get()
}

/// Run `f` on this thread as (or as not) the audio thread, then restore the thread's own mark
/// (also when `f` unwinds): a pool worker renders for realtime playback and offline bounces alike.
#[cfg(not(target_arch = "wasm32"))]
fn as_audio_thread(audio: bool, f: impl FnOnce()) {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            AUDIO_THREAD.set(self.0);
        }
    }
    let _restore = Restore(AUDIO_THREAD.replace(audio));
    f();
}

/// Process strips, in parallel on native targets when there are enough of them.
#[cfg(not(target_arch = "wasm32"))]
fn run_strips<T: Send>(work: &mut [T], f: impl Fn(&mut T) + Sync + Send) {
    use rayon::prelude::*;
    if work.len() >= 4 {
        // The workers render on behalf of this thread: on the audio thread, they are it too.
        let audio = on_audio_thread();
        work.par_iter_mut().for_each(|w| as_audio_thread(audio, || f(w)));
    } else {
        work.iter_mut().for_each(f);
    }
}

#[cfg(target_arch = "wasm32")]
fn run_strips<T: Send>(work: &mut [T], f: impl Fn(&mut T) + Sync + Send) {
    work.iter_mut().for_each(f);
}

fn strip_channels(t: &Track, main_ch: usize) -> usize {
    match t.kind {
        TrackKind::Master => main_ch,
        TrackKind::Instrument => t.channels().clamp(2, MAX_CHANNELS),
        _ => t.channels().clamp(1, MAX_CHANNELS),
    }
}

/// The format a strip's buffer actually carries (a mono instrument renders in stereo).
fn strip_format(t: &Track, n: usize) -> ChannelFormat {
    if t.format.channels().min(MAX_CHANNELS) == n { t.format } else { ChannelFormat::for_channels(n) }
}

/// Audio/instrument tracks first, then auxes ordered so bus producers come before consumers.
fn processing_order(s: &Session) -> Vec<usize> {
    let mut order: Vec<usize> =
        (0..s.tracks.len()).filter(|&i| s.tracks.get(i).is_some_and(|t| !matches!(t.kind, TrackKind::Aux | TrackKind::Folder))).collect();
    let mut auxes: Vec<usize> =
        (0..s.tracks.len()).filter(|&i| s.tracks.get(i).is_some_and(|t| matches!(t.kind, TrackKind::Aux | TrackKind::Folder))).collect();
    // Depth = how many aux hops feed this aux's input bus (bounded; cycles cap out).
    let depth = |i: usize| -> usize {
        let mut d = 0;
        let mut cur = vec![i];
        for _ in 0..8 {
            let inputs: Vec<&Route> = cur.iter().filter_map(|&k| s.tracks.get(k)).map(|t| &t.mixer.input).collect();
            let feeders: Vec<usize> = auxes_feeding(s, &inputs);
            if feeders.is_empty() {
                break;
            }
            d += 1;
            cur = feeders;
        }
        d
    };
    auxes.sort_by_key(|&i| depth(i));
    order.extend(auxes);
    order
}

fn auxes_feeding(s: &Session, inputs: &[&Route]) -> Vec<usize> {
    s.tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t.kind, TrackKind::Aux | TrackKind::Folder))
        .filter(|(_, t)| {
            inputs.iter().any(|r| matches!(r, Route::Bus(_)) && (&&t.mixer.output == r || t.mixer.sends.iter().flatten().any(|sn| &&sn.target == r)))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Implicit solo: an aux is not muted when a soloed track feeds its input bus.
fn receives_solo(s: &Session, t: &Track) -> bool {
    let Route::Bus(b) = &t.mixer.input else { return false };
    s.tracks
        .iter()
        .any(|o| o.mixer.solo && (o.mixer.output == Route::Bus(*b) || o.mixer.sends.iter().flatten().any(|sn| sn.target == Route::Bus(*b))))
}

/// Metronome: a short synthesized blip on every beat of the tempo map, accented on beat 1 of
/// each bar. Settings come from `setup.click_countoff` (`click.*` values).
mod click {
    use super::*;

    /// Blip length in seconds.
    const LEN_S: f64 = 0.03;

    pub fn render(s: &Session, pos: Samples, frames: usize, main: &mut [Vec<f32>]) {
        let sr = s.sample_rate;
        let rate = sr.as_f64();
        let len = (LEN_S * rate) as i64;
        let end = pos.saturating_add(frames as i64);
        let gain = f64::from(db_to_gain(s.edit.value("click.volume_db", -6.0).clamp(-60.0, 12.0) as f32));
        let accent = (note_hz(s.edit.value("click.accent_note", 84.0)), s.edit.value("click.accent_velocity", 127.0));
        let normal = (note_hz(s.edit.value("click.normal_note", 72.0)), s.edit.value("click.normal_velocity", 100.0));
        // Start one blip length early so a click that began in an earlier block still rings into this one.
        let t0 = s.tempo.samples_to_ticks(pos.saturating_sub(len), sr).max(0).saturating_sub(1);
        let mut tick = t0.saturating_sub(s.tempo.bar_beat_at_tick(t0).tick);
        // Blocks hold a few beats at most; the guard only protects against a malformed tempo map.
        for _ in 0..4096 {
            let at = s.tempo.tick_to_samples(tick, sr);
            if at >= end {
                break;
            }
            if at.saturating_add(len) > pos {
                let (hz, vel) = if s.tempo.bar_beat_at_tick(tick).beat == 1 { accent } else { normal };
                let amp = gain * (vel.clamp(1.0, 127.0) / 127.0);
                blip(main, at - pos, frames, hz, amp as f32, rate);
            }
            tick = tick.saturating_add(s.tempo.meter_at_tick(tick).ticks_per_beat());
        }
    }

    /// Adds one decaying sine blip whose first sample sits `rel` frames into the block (negative
    /// when it began before the block) to every channel.
    fn blip(main: &mut [Vec<f32>], rel: i64, frames: usize, hz: f64, amp: f32, rate: f64) {
        let len = (LEN_S * rate) as usize;
        let w = std::f64::consts::TAU * hz / rate;
        let first = usize::try_from(-rel).unwrap_or(0);
        for i in first..len {
            let Ok(at) = usize::try_from(rel + i as i64) else { continue };
            if at >= frames {
                break;
            }
            let env = (-6.0 * i as f64 / len as f64).exp() as f32;
            let v = ((w * i as f64).sin() as f32) * env * amp;
            for ch in main.iter_mut() {
                if let Some(d) = ch.get_mut(at) {
                    *d += v;
                }
            }
        }
    }

    /// Click pitch for a MIDI note setting (clamped to the MIDI range).
    fn note_hz(note: f64) -> f64 {
        soundcraft_dsp::midi_to_hz(note.clamp(0.0, 127.0))
    }
}

fn render_clip(s: &Session, clip: &Clip, pos: Samples, frames: usize, buf: &mut [Vec<f32>], scratch: &mut [f32]) {
    let ClipContent::Audio { source, .. } = clip.content else { return };
    let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
    let Some(isect) = clip.range().intersect(&block) else { return };
    let Some(audio) = s.pool.get(source) else { return };
    let nsrc = audio.buffer.channels.len();
    if nsrc == 0 {
        return;
    }
    let i0 = usize::try_from(isect.start - pos).unwrap_or(0);
    let i1 = usize::try_from(isect.end - pos).unwrap_or(0).min(frames);
    // A captured window must play the same curve at any destination and block size. The
    // previous block-relative 32-sample approximation changed its phase after a middle cut.
    // Constant clips keep the cheap path; curves follow their actual per-sample model law.
    let constant_gain = (clip.gain_env.is_empty() && clip.fade_in.len <= 0 && clip.fade_out.len <= 0).then(|| clip.gain_at(0));
    // Prepared strips already own a max-block scratch buffer. Evaluate curves once per
    // frame, shared across channels, without allocating in the audio callback.
    let mut gain_buffer = if constant_gain.is_none() { scratch.get_mut(..frames) } else { None };
    if let Some(gains) = gain_buffer.as_mut() {
        for (k, gain) in gains.iter_mut().enumerate().skip(i0).take(i1.saturating_sub(i0)) {
            *gain = clip.gain_at(pos.saturating_add(k as i64).saturating_sub(clip.start));
        }
    }
    for (ch, dst) in buf.iter_mut().enumerate() {
        let src = audio.buffer.channels.get(ch % nsrc).map(Vec::as_slice).unwrap_or(&[]);
        for k in i0..i1 {
            let rel = pos.saturating_add(k as i64).saturating_sub(clip.start);
            let f = clip.source_position(rel);
            let x0 = f.floor();
            let fr = f - x0;
            let a = if x0 >= 0.0 { src.get(x0 as usize).copied().unwrap_or(0.0) } else { 0.0 };
            let v = if fr == 0.0 {
                a
            } else {
                let b = if x0 + 1.0 >= 0.0 { src.get((x0 + 1.0) as usize).copied().unwrap_or(0.0) } else { 0.0 };
                // Finite opposite Float32 extrema can overflow b-a in f32. Interpolate
                // in f64 so both integer reads and fractional convex combinations stay finite.
                (f64::from(a) * (1.0 - fr) + f64::from(b) * fr) as f32
            };
            let gain = constant_gain.or_else(|| gain_buffer.as_ref().and_then(|gains| gains.get(k)).copied()).unwrap_or_else(|| clip.gain_at(rel));
            if let Some(d) = dst.get_mut(k) {
                *d += v * gain;
            }
        }
    }
}

fn midi_properties(s: &Session, t: &Track) -> [f64; 4] {
    [
        s.edit.value(&format!("rtp.{}.velocity", t.id.0), 0.0),
        s.edit.value(&format!("rtp.{}.transpose", t.id.0), 0.0),
        s.edit.value(&format!("rtp.{}.duration", t.id.0), 100.0),
        s.edit.value(&format!("rtp.{}.delay", t.id.0), 0.0),
    ]
}

/// Send note on/offs for MIDI clips overlapping the block.
fn schedule_notes(
    s: &Session,
    t: &Track,
    pos: Samples,
    frames: usize,
    inst: &mut PluginSlot,
    held: &mut Vec<u8>,
    properties: [f64; 4],
) -> Result<(), PreparationError> {
    let sr = s.sample_rate;
    let end = pos.saturating_add(frames as i64);
    for clip in t.clips() {
        if clip.muted {
            continue;
        }
        let ClipContent::Midi { sequence } = &clip.content else { continue };
        // A note whose cropped end equals a block boundary is released at frame zero of
        // the next block. Half-open clip overlap alone used to skip that final note-off.
        if clip.start >= end || clip.end() < pos {
            continue;
        }
        let base = s.tempo.samples_to_ticks(clip.start, sr);
        let base_sample = s.tempo.tick_to_samples(base, sr);
        let (dv, dp, dur, delay) = (
            properties[0].clamp(-127.0, 127.0) as i64,
            properties[1].clamp(-127.0, 127.0) as i64,
            properties[2].clamp(1.0, 1000.0) / 100.0,
            properties[3].clamp(-1e7, 1e7) as i64,
        );
        for n0 in &sequence.notes {
            let mut n = *n0;
            n.velocity = (i64::from(n.velocity) + dv).clamp(1, 127) as u8;
            n.pitch = (i64::from(n.pitch) + dp).clamp(0, 127) as u8;
            n.length = ((n.length as f64 * dur) as i64).max(1);
            n.start = n.start.saturating_add(delay).max(0);
            // Ticks are relative to the exact clip position. Preserve its fractional-tick
            // sample phase so a zero-offset note is not scheduled before a placed window.
            let on = clip.start.saturating_add(s.tempo.tick_to_samples(base.saturating_add(n.start), sr).saturating_sub(base_sample));
            let off = clip
                .start
                .saturating_add(s.tempo.tick_to_samples(base.saturating_add(n.start).saturating_add(n.length), sr).saturating_sub(base_sample))
                .min(clip.end());
            if on >= clip.end() {
                continue;
            }
            if on >= pos && on < end {
                inst.plugin.note_on(usize::try_from(on - pos).unwrap_or(0), n.pitch, n.velocity);
                if !held.contains(&n.pitch) && held.len() < 128 {
                    held.push(n.pitch);
                }
            }
            if off >= pos && off < end {
                inst.plugin.note_off(usize::try_from(off - pos).unwrap_or(0), n.pitch);
                held.retain(|p| *p != n.pitch);
            }
        }
    }
    Ok(())
}

/// Session audio shared with a sample-playing instrument.
struct PoolSample(Arc<SourceAudio>);

impl SampleSource for PoolSample {
    fn sample_rate(&self) -> f32 {
        self.0.buffer.sample_rate as f32
    }
    fn num_channels(&self) -> usize {
        self.0.buffer.num_channels()
    }
    fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.0.channel(ch)
    }
}

/// Hands a strip's instrument the audio of its insert's sample source when it changed (a new
/// sample, a cleared one, or a new instance). Missing media plays nothing.
fn sync_sample(strip: &mut Strip, want: Option<&soundcraft_model::Insert>, s: &Session) {
    let audio = want.and_then(|i| i.sample).and_then(|id| s.pool.get(id));
    let same = match (strip.sample.as_ref(), audio) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    strip.sample_source = audio.map(|audio| Arc::new(PoolSample(Arc::clone(audio))) as Arc<dyn SampleSource>);
    if let Some(inst) = strip.instrument.as_mut() {
        inst.plugin.set_sample(strip.sample_source.clone());
    }
    strip.sample = audio.cloned();
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| if v.is_finite() { m.max(v.abs()) } else { m })
}

/// Per-block routing gains: `m[src][dst]`.
type Matrix = [[f32; MAX_CHANNELS]; MAX_CHANNELS];

/// How a source is panned: the stereo pan pair (mono uses `[0]`) and the optional surround pan.
struct PanSpec<'a> {
    stereo: [f32; 2],
    surround: Option<&'a SurroundPan>,
}

fn identity_matrix(n: usize) -> Matrix {
    let mut m = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
    for (i, row) in m.iter_mut().enumerate().take(n) {
        if let Some(g) = row.get_mut(i) {
            *g = 1.0;
        }
    }
    m
}

/// ITU-R BS.775 stereo fold-down gains (to L, to R) for one speaker: C and surrounds at -3 dB,
/// the centre surround at -6 dB per side, LFE dropped.
pub fn itu_stereo_fold(sp: Speaker) -> (f32, f32) {
    use Speaker::*;
    let h = std::f32::consts::FRAC_1_SQRT_2;
    match sp {
        L | Lc | Lw => (1.0, 0.0),
        R | Rc | Rw => (0.0, 1.0),
        C => (h, h),
        Lfe => (0.0, 0.0),
        Cs => (0.5, 0.5),
        Ls | Lss | Lrs | Ltf | Ltm | Ltr => (h, 0.0),
        Rs | Rss | Rrs | Rtf | Rtm | Rtr => (0.0, h),
    }
}

/// Speaker directions of a format (for the DSP panner). Returns the count written.
fn layout_of(fmt: ChannelFormat, out: &mut [SpeakerPos; MAX_CHANNELS]) -> usize {
    let sp = fmt.speakers();
    for (o, s) in out.iter_mut().zip(sp.iter()) {
        *o = SpeakerPos { az: s.azimuth(), el: s.elevation(), lfe: s.is_lfe() };
    }
    sp.len().min(MAX_CHANNELS)
}

fn surround_params(sp: &SurroundPan, dx: f32) -> SurroundParams {
    SurroundParams { x: (sp.x + dx).clamp(-1.0, 1.0), y: sp.y, z: sp.z, divergence: sp.divergence, center: sp.center / 100.0 }
}

/// Routing gains from a source (`ns` channels of `src_fmt`) into a destination (`nd` channels of
/// `dst_fmt`). See the crate docs for the rules.
fn build_matrix(src_fmt: ChannelFormat, ns: usize, dst_fmt: ChannelFormat, nd: usize, pan: &PanSpec<'_>, law: PanLaw) -> Matrix {
    let mut m = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    let (ns, nd) = (ns.min(MAX_CHANNELS), nd.min(MAX_CHANNELS));
    if ns == 0 || nd == 0 {
        return m;
    }
    let set = |m: &mut Matrix, s: usize, d: usize, g: f32| {
        if let Some(x) = m.get_mut(s).and_then(|r| r.get_mut(d)) {
            *x += g;
        }
    };
    if nd == 1 {
        // Mono destination: the stereo routing summed at -6 dB.
        let st = build_matrix(src_fmt, ns, ChannelFormat::Stereo, 2, pan, law);
        for (row, srow) in m.iter_mut().zip(st.iter()).take(ns) {
            row[0] = 0.5 * (srow[0] + srow[1]);
        }
        return m;
    }
    let src_sp = src_fmt.speakers();
    let src_amb = src_fmt.is_ambisonic() && ns >= 4;
    if nd == 2 && !dst_fmt.is_ambisonic() {
        match ns {
            1 => {
                let (gl, gr) = gains(pan.stereo[0], law);
                m[0][0] = gl;
                m[0][1] = gr;
            }
            2 if !src_amb => {
                let (a_l, a_r) = gains(pan.stereo[0], PanLaw::Zero);
                let (b_l, b_r) = gains(pan.stereo[1], PanLaw::Zero);
                m[0] = [0.0; MAX_CHANNELS];
                m[0][0] = a_l;
                m[0][1] = a_r;
                m[1][0] = b_l;
                m[1][1] = b_r;
            }
            _ if src_amb => {
                for (d, az) in [(0usize, -30.0f32), (1, 30.0)] {
                    let g = soundcraft_dsp::pan::ambisonic_decode(az, 0.0, 2);
                    for (c, gc) in g.iter().enumerate() {
                        set(&mut m, c, d, *gc);
                    }
                }
            }
            _ if src_sp.len() == ns => {
                for (s, sp) in src_sp.iter().enumerate() {
                    let (l, r) = itu_stereo_fold(*sp);
                    set(&mut m, s, 0, l);
                    set(&mut m, s, 1, r);
                }
            }
            _ => {
                // Unknown layout: first two channels to L/R, the rest to the centre.
                let h = std::f32::consts::FRAC_1_SQRT_2;
                m[0][0] = 1.0;
                m[1][1] = 1.0;
                for row in m.iter_mut().take(ns).skip(2) {
                    row[0] = h;
                    row[1] = h;
                }
            }
        }
        return m;
    }
    if ns > 2 && src_fmt == dst_fmt && ns == nd {
        return identity_matrix(ns);
    }
    // Multichannel destination.
    let mut layout = [SpeakerPos::default(); MAX_CHANNELS];
    let nl = layout_of(dst_fmt, &mut layout);
    let layout = layout.get(..nl).unwrap_or(&[]);
    let dst_sp = dst_fmt.speakers();
    let dst_amb = dst_fmt.is_ambisonic();
    let find = |sp: Speaker| dst_sp.iter().position(|x| *x == sp).filter(|i| *i < nd);
    let mut g = [0.0f32; MAX_CHANNELS];
    if dst_amb {
        if src_amb || (src_fmt.is_ambisonic() && ns > 2) {
            return identity_matrix(ns.min(nd));
        }
        let mut enc = |m: &mut Matrix, s: usize, az: f32, el: f32, spread: f32| {
            soundcraft_dsp::pan::ambisonic_encode(az, el, spread, &mut g);
            for (d, gd) in g.iter().enumerate().take(nd) {
                set(m, s, d, *gd);
            }
        };
        match (ns, pan.surround) {
            (1 | 2, Some(sp)) => {
                for s in 0..ns {
                    let dx = if ns == 2 { if s == 0 { -1.0 } else { 1.0 } } else { 0.0 };
                    let p = surround_params(sp, dx);
                    enc(&mut m, s, soundcraft_dsp::pan::puck_azimuth(&[], p.x, p.y), p.z * 90.0, p.divergence);
                }
            }
            (1, None) => enc(&mut m, 0, pan.stereo[0].clamp(-1.0, 1.0) * 30.0, 0.0, 0.0),
            (2, None) => {
                enc(&mut m, 0, -30.0, 0.0, 0.0);
                enc(&mut m, 1, 30.0, 0.0, 0.0);
            }
            _ => {
                for (s, sp) in src_sp.iter().enumerate().take(ns) {
                    if !sp.is_lfe() {
                        enc(&mut m, s, sp.azimuth(), sp.elevation(), 0.0);
                    }
                }
            }
        }
        return m;
    }
    if dst_sp.len() != nd {
        // No layout to pan on: channel for channel.
        return identity_matrix(ns.min(nd));
    }
    let (li, ri) = (find(Speaker::L), find(Speaker::R));
    match (ns, pan.surround) {
        (1 | 2, Some(sp)) if !src_amb => {
            for s in 0..ns {
                let dx = if ns == 2 { if s == 0 { -1.0 } else { 1.0 } } else { 0.0 };
                soundcraft_dsp::pan::surround_gains(layout, &surround_params(sp, dx), &mut g);
                for (d, gd) in g.iter().enumerate().take(nd) {
                    set(&mut m, s, d, *gd);
                }
                if let Some(lfe) = find(Speaker::Lfe)
                    && sp.lfe_db > -143.9
                {
                    set(&mut m, s, lfe, db_to_gain(sp.lfe_db.min(12.0)) / ns as f32);
                }
            }
        }
        (1, None) => {
            let (gl, gr) = gains(pan.stereo[0], law);
            if let (Some(l), Some(r)) = (li, ri) {
                set(&mut m, 0, l, gl);
                set(&mut m, 0, r, gr);
            }
        }
        (2, None) if !src_amb => {
            let (a_l, a_r) = gains(pan.stereo[0], PanLaw::Zero);
            let (b_l, b_r) = gains(pan.stereo[1], PanLaw::Zero);
            if let (Some(l), Some(r)) = (li, ri) {
                set(&mut m, 0, l, a_l);
                set(&mut m, 0, r, a_r);
                set(&mut m, 1, l, b_l);
                set(&mut m, 1, r, b_r);
            }
        }
        _ if src_amb => {
            let n_spk = dst_sp.iter().filter(|s| !s.is_lfe()).count();
            for (d, sp) in dst_sp.iter().enumerate().take(nd) {
                if sp.is_lfe() {
                    continue;
                }
                let dec = soundcraft_dsp::pan::ambisonic_decode(sp.azimuth(), sp.elevation(), n_spk);
                for (c, gc) in dec.iter().enumerate() {
                    set(&mut m, c, d, *gc);
                }
            }
        }
        _ if src_sp.len() == ns => {
            for (s, sp) in src_sp.iter().enumerate() {
                if let Some(d) = find(*sp) {
                    set(&mut m, s, d, 1.0);
                } else if sp.is_lfe() {
                    // No LFE in the destination: dropped.
                } else {
                    soundcraft_dsp::pan::direction_gains(layout, sp.azimuth(), sp.elevation(), &mut g);
                    for (d, gd) in g.iter().enumerate().take(nd) {
                        set(&mut m, s, d, *gd);
                    }
                }
            }
        }
        _ => return identity_matrix(ns.min(nd)),
    }
    m
}

/// `dst[d] += Σ_s src[s] × gain × m[s][d]` for every destination channel (no allocation).
fn apply_matrix(src: &[Vec<f32>], frames: usize, dst: &mut [Vec<f32>], m: &Matrix, gain: f32) {
    let ns = src.len().min(MAX_CHANNELS);
    for (d, out) in dst.iter_mut().enumerate().take(MAX_CHANNELS) {
        let mut idx = [(0usize, 0.0f32); MAX_CHANNELS];
        let mut k = 0;
        for (s, row) in m.iter().enumerate().take(ns) {
            let g = row.get(d).copied().unwrap_or(0.0);
            if g != 0.0
                && let Some(slot) = idx.get_mut(k)
            {
                *slot = (s, g);
                k += 1;
            }
        }
        let taps = idx.get(..k).unwrap_or(&[]);
        if taps.is_empty() {
            continue;
        }
        for (i, o) in out.iter_mut().enumerate().take(frames) {
            let mut acc = 0.0f32;
            for &(s, g) in taps {
                acc += src.get(s).and_then(|c| c.get(i)).copied().unwrap_or(0.0) * gain * g;
            }
            *o += acc;
        }
    }
}

fn apply_matrix_frame(src: &[Vec<f32>], frame: usize, dst: &mut [Vec<f32>], matrix: &Matrix, gain: f32) {
    for (channel, dst) in dst.iter_mut().enumerate().take(MAX_CHANNELS) {
        let Some(sample) = dst.get_mut(frame) else { continue };
        let mut value = 0.0;
        for (index, src) in src.iter().enumerate().take(MAX_CHANNELS) {
            value += src.get(frame).copied().unwrap_or(0.0) * matrix.get(index).and_then(|row| row.get(channel)).copied().unwrap_or(0.0) * gain;
        }
        *sample += value;
    }
}

/// Ceiling on one offline render (samples summed over channels): 2^29 f32 = 2 GiB, about 93
/// minutes of stereo at 48 kHz. Longer requests are truncated rather than exhausting memory.
pub const MAX_RENDER_SAMPLES: usize = 1 << 29;

/// Timeline input selection, explicit zero-input tail, and the first history sample to process.
/// Output corresponds exactly to range followed by tail; preroll never adds delivered frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderWindow {
    pub range: Range,
    pub tail_samples: Samples,
    pub preroll_start: Samples,
}

impl RenderWindow {
    pub fn output_frames(&self) -> Result<usize, PreparationError> {
        let selected = self
            .range
            .end
            .checked_sub(self.range.start)
            .filter(|frames| *frames > 0)
            .ok_or(PreparationError("render window needs a positive non-overflowing selection"))?;
        if self.tail_samples < 0 {
            return Err(PreparationError("render window tail must be nonnegative"));
        }
        self.range.end.checked_add(self.tail_samples).ok_or(PreparationError("render window end overflow"))?;
        selected
            .checked_add(self.tail_samples)
            .and_then(|frames| usize::try_from(frames).ok())
            .ok_or(PreparationError("render window length exceeds platform limits"))
    }
}

fn window_planar(channels: usize, frames: usize) -> Result<Vec<Vec<f32>>, PreparationError> {
    let mut out = Vec::new();
    out.try_reserve_exact(channels).map_err(|_| PreparationError("render channel allocation failed"))?;
    for _ in 0..channels {
        let mut channel = Vec::new();
        channel.try_reserve_exact(frames).map_err(|_| PreparationError("render sample allocation failed"))?;
        channel.resize(frames, 0.0);
        out.push(channel);
    }
    Ok(out)
}

/// Fallible output rendering with continuous preroll, an exact input cutoff and ringing tail.
/// A single restored/prepared instance set is warmed and rendered; no probing render consumes
/// its state. Admission precedes DSP processing, including discarded preroll/latency work.
pub fn try_render_window(source: Arc<Session>, bus: Option<BusId>, window: RenderWindow, block: usize) -> Result<Vec<Vec<f32>>, PreparationError> {
    let frames = window.output_frames()?;
    let preroll = window
        .range
        .start
        .checked_sub(window.preroll_start)
        .filter(|frames| *frames >= 0)
        .and_then(|frames| usize::try_from(frames).ok())
        .ok_or(PreparationError("preroll must begin at or before the selection"))?;
    soundcraft_time::SampleRate::new(source.sample_rate.hz()).map_err(|_| PreparationError("invalid render sample rate"))?;
    let channels = match bus {
        Some(id) => source.bus(id).ok_or(PreparationError("unknown render bus"))?.format.channels(),
        None => {
            if source.main_format().channels() > MAX_CHANNELS {
                return Err(PreparationError("render output supports at most 16 channels"));
            }
            main_channels(&source)
        }
    };
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return Err(PreparationError("render output supports 1..16 channels"));
    }
    let maximum_work =
        usize::try_from(source.sample_rate.samples(4.0 * 3600.0)).map_err(|_| PreparationError("render duration exceeds platform limits"))?;
    let base_work = preroll
        .checked_add(frames)
        .filter(|frames| *frames <= maximum_work)
        .ok_or(PreparationError("render window including preroll is limited to 4 hours"))?;
    if frames.checked_mul(channels).is_none_or(|samples| samples > MAX_RENDER_SAMPLES) {
        return Err(PreparationError("render window exceeds the offline allocation limit"));
    }
    let mut renderer = OfflineRenderer::prepare(Arc::clone(&source), block)?;
    let latency = renderer.mix().prepared_output_latency(bus)?;
    let total = base_work
        .checked_add(latency)
        .filter(|frames| *frames <= maximum_work)
        .ok_or(PreparationError("render work including compensation exceeds duration limit"))?;
    let discard = preroll.checked_add(latency).ok_or(PreparationError("render prefix overflow"))?;
    let processing_end = window
        .preroll_start
        .checked_add(i64::try_from(total).map_err(|_| PreparationError("render work exceeds timeline range"))?)
        .ok_or(PreparationError("render processing end overflow"))?;
    let expected_end = window
        .range
        .end
        .checked_add(window.tail_samples)
        .and_then(|end| end.checked_add(latency as i64))
        .ok_or(PreparationError("render compensation end overflow"))?;
    if processing_end != expected_end {
        return Err(PreparationError("render window arithmetic is inconsistent"));
    }
    let mut out = window_planar(channels, frames)?;
    let quantum = renderer.mix().max_block();
    let mut scratch = window_planar(channels, quantum)?;
    renderer.mix_mut().set_source_cutoff(Some(window.range.end))?;
    // Match a fresh Play: ClipFX static configuration has already reached its setters,
    // while insert/instrument targets are first delivered by the prepared renderer.
    renderer.mix_mut().reset();
    let mut done = 0usize;
    while done < total {
        let count = (total - done).min(quantum);
        let at = window.preroll_start.checked_add(done as i64).ok_or(PreparationError("render position overflow"))?;
        renderer.render_output(at, count, bus, &mut scratch)?;
        let actual_latency = renderer.mix().prepared_output_latency(bus)?;
        if actual_latency != latency {
            return Err(PreparationError("processor latency changed during the render window"));
        }
        let first = discard.saturating_sub(done).min(count);
        for (dst, src) in out.iter_mut().zip(&scratch) {
            for offset in first..count {
                let output_at =
                    done.checked_add(offset).and_then(|at| at.checked_sub(discard)).ok_or(PreparationError("render output offset overflow"))?;
                let value = src.get(offset).copied().ok_or(PreparationError("render scratch shape changed"))?;
                *dst.get_mut(output_at).ok_or(PreparationError("render output shape changed"))? = value;
            }
        }
        done += count;
    }
    Ok(out)
}

/// Offline render of the main mix over `range` (planar, [`main_channels`] channels in SMPTE/WAV
/// order). The mix's delay-compensation latency is removed, so the result lines up with the
/// timeline.
pub fn render_range(s: &Session, range: Range, block: usize) -> Vec<Vec<f32>> {
    try_render_output(Arc::new(s.clone()), None, range, block, prepared::MissingPluginPolicy::Bypass).unwrap_or_default()
}

/// Fallible offline preparation and rendering; production delivery callers propagate failures.
pub fn try_render_range(s: &Session, range: Range, block: usize) -> Result<Vec<Vec<f32>>, PreparationError> {
    try_render_output(Arc::new(s.clone()), None, range, block, prepared::MissingPluginPolicy::Error)
}

/// Compatibility offline bus render. An unknown bus is silent.
pub fn render_bus(s: &Session, bus: BusId, range: Range, block: usize) -> Vec<Vec<f32>> {
    if s.bus(bus).is_none() {
        return Vec::new();
    }
    try_render_output(Arc::new(s.clone()), Some(bus), range, block, prepared::MissingPluginPolicy::Bypass).unwrap_or_default()
}

pub fn try_render_bus(s: &Session, bus: BusId, range: Range, block: usize) -> Result<Vec<Vec<f32>>, PreparationError> {
    if s.bus(bus).is_none() {
        return Err(PreparationError("unknown render bus"));
    }
    try_render_output(Arc::new(s.clone()), Some(bus), range, block, prepared::MissingPluginPolicy::Error)
}

fn try_render_output(
    source: Arc<Session>,
    bus: Option<BusId>,
    range: Range,
    block: usize,
    missing: prepared::MissingPluginPolicy,
) -> Result<Vec<Vec<f32>>, PreparationError> {
    let channels = bus.and_then(|id| source.bus(id).map(|b| b.format.channels().min(MAX_CHANNELS))).unwrap_or_else(|| main_channels(&source));
    let len = usize::try_from(range.len().max(0)).unwrap_or(0).min(MAX_RENDER_SAMPLES / channels.max(1));
    let latency = {
        let mut probe = OfflineRenderer::prepare_with_policy(Arc::clone(&source), block, missing)?;
        let count = probe.mix().max_block().min(len.max(1));
        probe.render(range.start, count, &mut [])?;
        probe.mix().prepared_output_latency(bus)?
    };
    let total = len
        .checked_add(latency)
        .filter(|n| *n <= MAX_RENDER_SAMPLES / channels.max(1))
        .ok_or(PreparationError("latency exceeds offline allocation limit"))?;
    let mut out = vec![vec![0.0; total]; channels];
    let mut renderer = OfflineRenderer::prepare_with_policy(source, block, missing)?;
    let b = renderer.mix().max_block();
    let mut tmp = vec![vec![0.0; b]; channels];
    let mut done = 0;
    while done < total {
        let n = (total - done).min(b);
        let position = automation::position(range.start, done);
        if let Some(bus) = bus {
            renderer.render(position, n, &mut [])?;
            renderer.mix().copy_bus(bus, n, &mut tmp);
        } else {
            renderer.render(position, n, &mut tmp)?;
        }
        for (dst, src) in out.iter_mut().zip(&tmp) {
            if let (Some(dst), Some(src)) = (dst.get_mut(done..done + n), src.get(..n)) {
                dst.copy_from_slice(src);
            }
        }
        done += n;
    }
    if latency > 0 {
        for channel in &mut out {
            channel.drain(..latency.min(channel.len()));
        }
    }
    Ok(out)
}

/// ITU-R BS.775 stereo fold-down of planar audio in `fmt` (C and surrounds at -3 dB, LFE dropped;
/// Ambisonics play their W channel). Mono/stereo input is returned unchanged.
pub fn fold_down_stereo(fmt: ChannelFormat, ch: &[Vec<f32>]) -> Vec<Vec<f32>> {
    if ch.len() <= 2 {
        return ch.to_vec();
    }
    let len = ch.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = vec![vec![0.0f32; len]; 2];
    let sp = fmt.speakers();
    let h = std::f32::consts::FRAC_1_SQRT_2;
    for (c, src) in ch.iter().enumerate() {
        let (gl, gr) = match sp.get(c) {
            Some(x) if sp.len() == ch.len() => itu_stereo_fold(*x),
            _ if c == 0 => (h, h),
            _ => (0.0, 0.0),
        };
        for (g, dst) in [(gl, 0usize), (gr, 1)] {
            if g == 0.0 {
                continue;
            }
            if let Some(d) = out.get_mut(dst) {
                for (o, x) in d.iter_mut().zip(src.iter()) {
                    *o += x * g;
                }
            }
        }
    }
    out
}

/// Render only one track's clips (no mixer processing) — Consolidate.
pub fn render_clips(s: &Session, track: TrackId, range: Range) -> Vec<Vec<f32>> {
    let Some(t) = s.track(track) else { return Vec::new() };
    let len = usize::try_from(range.len().max(0)).unwrap_or(0).min(MAX_RENDER_SAMPLES / t.channels().max(1));
    let mut out = vec![vec![0.0f32; len]; t.channels().max(1)];
    let mut scratch = Vec::new();
    for c in t.clips() {
        if c.muted {
            continue;
        }
        match clip_fx_params(s, c.id.0) {
            None => render_clip(s, c, range.start, len, &mut out, &mut scratch),
            Some(params) => {
                let mut tmp = vec![vec![0.0f32; len]; out.len()];
                render_clip(s, c, range.start, len, &mut tmp, &mut scratch);
                let mut fx = ClipFx::new(s.sample_rate.as_f64() as f32, tmp.len());
                fx.configure(&params);
                let mut done = 0;
                while done < len {
                    let n = (len - done).min(4096);
                    let mut block: Vec<Vec<f32>> = tmp.iter().map(|ch| ch.get(done..done + n).map(<[f32]>::to_vec).unwrap_or_default()).collect();
                    fx.process(&mut block, n);
                    for (o, b) in out.iter_mut().zip(block.iter()) {
                        if let Some(dst) = o.get_mut(done..done + n) {
                            for (x, y) in dst.iter_mut().zip(b.iter()) {
                                *x += *y;
                            }
                        }
                    }
                    done += n;
                }
            }
        }
    }
    out
}

/// Render one track through its inserts (pre-fader) — Commit / Freeze.
pub fn render_track_pre_fader(s: &Session, track: TrackId, range: Range) -> Vec<Vec<f32>> {
    try_render_track_pre_fader(s, track, range).unwrap_or_default()
}

pub fn try_render_track_pre_fader(s: &Session, track: TrackId, range: Range) -> Result<Vec<Vec<f32>>, PreparationError> {
    let mut solo = s.clone();
    // Multichannel tracks render into a main mix of their own format (straight through).
    let fmt = s.track(track).map_or(ChannelFormat::Stereo, |t| if t.channels() > 2 { t.format } else { ChannelFormat::Stereo });
    solo.outputs = vec![soundcraft_model::OutputPath { name: "Out".into(), first_channel: 0, format: fmt }];
    for t in &mut solo.tracks {
        if t.id == track {
            t.mixer.volume_db = 0.0;
            t.mixer.mute = false;
            t.mixer.solo = false;
            t.mixer.output = Route::Main;
            t.mixer.pan = match t.channels() {
                1 => vec![0.0],
                _ => vec![-1.0, 1.0],
            };
            t.automation.retain(|l| matches!(l.param, AutoParam::Plugin { .. }));
            t.mixer.sends.iter_mut().for_each(|x| *x = None);
            t.mixer.surround = None;
        } else {
            t.inactive = true;
        }
    }
    let mut out = try_render_range(&solo, range, 1024)?;
    // Mono tracks: fold back to one channel (undo the centre pan law).
    if s.track(track).is_some_and(|t| t.channels() == 1)
        && let Some(l) = out.first().cloned()
    {
        let g = std::f32::consts::SQRT_2;
        out = vec![l.iter().map(|x| x * g).collect()];
    }
    Ok(out)
}

/// A built-in plugin, else a hosted CLAP plugin (`clap:<id>`).
fn create_plugin(id: &str) -> Option<Box<dyn Plugin>> {
    soundcraft_dsp::create(id)
        .or_else(|| soundcraft_clap_host::create(id))
        .or_else(|| soundcraft_vst3_host::create(id))
        .or_else(|| soundcraft_au_host::create(id))
}

#[cfg(test)]
mod audio_thread_tests {
    use super::*;

    #[test]
    fn only_a_marked_thread_is_the_audio_thread() {
        assert!(!on_audio_thread());
        let marked = std::thread::spawn(|| {
            let before = on_audio_thread();
            mark_audio_thread();
            (before, on_audio_thread())
        })
        .join()
        .unwrap();
        assert_eq!(marked, (false, true));
        assert!(!on_audio_thread(), "marking another thread leaves this one alone");
    }

    /// What `on_audio_thread` says inside each strip job, for strips run from a thread that is
    /// (or is not) the audio thread.
    fn seen_by_strips(audio: bool) -> Vec<bool> {
        std::thread::spawn(move || {
            if audio {
                mark_audio_thread();
            }
            let mut work = vec![None; 64];
            run_strips(&mut work, |w| {
                // Long enough that the pool's workers take part.
                std::thread::sleep(std::time::Duration::from_millis(1));
                *w = Some(on_audio_thread());
            });
            work.into_iter().map(|w| w.unwrap()).collect()
        })
        .join()
        .unwrap()
    }

    #[test]
    fn strips_rendered_for_the_audio_thread_count_as_the_audio_thread_and_only_then() {
        assert!(seen_by_strips(true).iter().all(|&rt| rt), "a worker rendering for the audio thread is on it");
        assert!(seen_by_strips(false).iter().all(|&rt| !rt), "the workers don't stay marked afterwards");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{Insert, SourceAudio, SourceId};
    use std::sync::Arc;

    fn synth_session(level_db: f32) -> Session {
        use soundcraft_midi::{Note, Sequence};
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let mut seq = Sequence::default();
        seq.notes.push(Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 });
        let cid = s.new_clip_id();
        let tr = s.track_mut(t).unwrap();
        tr.playlist_mut().unwrap().clips.push(Clip::midi(cid, "n", 0, 24_000, seq));
        let mut ins = Insert::new("subtractive_synth");
        ins.params.insert("level".into(), level_db);
        tr.instrument = Some(ins);
        s
    }

    #[test]
    fn stored_instrument_params_reach_the_plugin() {
        // #152: a saved instrument level of -12 dB must render quieter than -6 dB.
        // RMS over the held part of the note, past any parameter smoothing.
        let rms = |db: f32| {
            let out = render_range(&synth_session(db), Range::new(0, 12_000), 512);
            let tail = out[0].get(6_000..).unwrap_or_default();
            (tail.iter().map(|x| x * x).sum::<f32>() / tail.len().max(1) as f32).sqrt()
        };
        let (loud, quiet) = (rms(-6.0), rms(-12.0));
        assert!(loud > 0.0, "the synth should sound");
        assert!((quiet / loud - 0.5).abs() < 0.05, "-12 dB vs -6 dB should halve the level: {quiet} / {loud}");
    }

    #[test]
    fn insert_slot_a_automation_does_not_drive_the_instrument() {
        let mut s = synth_session(-6.0);
        let reference = render_range(&s, Range::new(0, 12_000), 512);
        let tr = &mut s.tracks[0];
        let mut lane = soundcraft_model::AutomationLane::new(AutoParam::Plugin { slot: 0, param: "level".into() });
        lane.set_point(0, -48.0);
        tr.automation.push(lane);
        assert_eq!(render_range(&s, Range::new(0, 12_000), 512), reference);
    }

    fn session_with_dc(level: f32, frames: usize) -> (Session, TrackId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; frames]] };
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(buf)));
        let id = s.new_clip_id();
        if let Some(pl) = s.track_mut(t).and_then(|t| t.playlist_mut()) {
            pl.clips.push(Clip::audio(id, "dc", SourceId(900), 0, 0, frames as i64));
        }
        (s, t)
    }

    #[test]
    fn mono_center_pan_is_minus_3db() {
        let (s, _) = session_with_dc(0.5, 4800);
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0][1000] - expect).abs() < 1e-3, "{}", out[0][1000]);
        assert!((out[1][1000] - expect).abs() < 1e-3);
    }

    #[test]
    fn fader_mute_and_solo() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.volume_db = -6.0;
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!((out[0][2000] - expect).abs() < 1e-3);
        s.track_mut(t).unwrap().mixer.mute = true;
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000].abs() < 1e-6);
        // Soloing another track mutes this one.
        s.track_mut(t).unwrap().mixer.mute = false;
        let other = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(other).unwrap().mixer.solo = true;
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000].abs() < 1e-6);
    }

    #[test]
    fn hard_pans() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.pan = vec![-1.0];
        let out = render_range(&s, Range::new(0, 4800), 256);
        assert!(out[0][100] > 0.49 && out[1][100].abs() < 1e-4);
    }

    #[test]
    fn fades_and_clip_gain_apply() {
        let (mut s, t) = session_with_dc(1.0, 48_000);
        {
            let c = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0];
            c.fade_in = soundcraft_model::Fade { len: 1000, shape: soundcraft_model::FadeShape::Linear };
            c.gain_db = -6.0;
        }
        let out = render_range(&s, Range::new(0, 2000), 512);
        let g = std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!(out[0][0].abs() < 1e-3);
        assert!((out[0][500] - 0.5 * g).abs() < 0.02, "{}", out[0][500]);
        assert!((out[0][1500] - g).abs() < 1e-3);
    }

    #[test]
    fn sends_feed_aux_via_bus() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let bus = s.add_bus("Verb", ChannelFormat::Stereo);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Verb"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        {
            let tr = s.track_mut(t).unwrap();
            tr.mixer.output = Route::None;
            let mut snd = soundcraft_model::SendSlot::new(Route::Bus(bus));
            snd.level_db = 0.0;
            tr.mixer.sends[0] = Some(snd);
        }
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][1000] > 0.1, "aux should carry the send: {}", out[0][1000]);
    }

    #[test]
    fn render_bus_is_the_bus_not_the_main_mix() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let stem = s.add_bus("Stem", ChannelFormat::Stereo);
        let empty = s.add_bus("Empty", ChannelFormat::Stereo);
        s.track_mut(t).unwrap().mixer.output = Route::Bus(stem);
        let main = render_range(&s, Range::new(0, 4800), 512);
        let bus = render_bus(&s, stem, Range::new(0, 4800), 512);
        let silent = render_bus(&s, empty, Range::new(0, 4800), 512);
        assert!(main[0][1000].abs() < 1e-4, "main should not carry a track routed to the bus: {}", main[0][1000]);
        assert!(bus[0][1000].abs() > 0.1, "bus should carry the track: {}", bus[0][1000]);
        assert!(silent.iter().all(|c| c.iter().all(|x| x.abs() < 1e-6)));
    }

    #[test]
    fn volume_automation_ramps() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        {
            let tr = s.track_mut(t).unwrap();
            let l = tr.lane_mut(&AutoParam::Volume);
            l.set_point(0, 0.0);
            l.set_point(24_000, -144.0);
        }
        let out = render_range(&s, Range::new(0, 48_000), 512);
        assert!(out[0][100] > 0.3);
        assert!(out[0][30_000].abs() < 1e-4);
    }

    #[test]
    fn inserts_process_and_survive_unknown_plugins() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("does-not-exist"));
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][100] > 0.3);
        let id = soundcraft_dsp::plugins().iter().find(|p| p.id.contains("gain")).map(|p| p.id).unwrap_or("gain");
        let mut ins = Insert::new(id);
        ins.params.insert("gain".into(), -6.0);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let _ = render_range(&s, Range::new(0, 4800), 512);
    }

    /// A built-in gain behind a third-party id, with a state blob and a log of parameter sets.
    struct Stateful {
        inner: Box<dyn Plugin>,
        blob: Vec<u8>,
        sets: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Plugin for Stateful {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.inner.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.inner.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.sets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.inner.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.inner.process(io, frames);
        }
        fn save_state(&mut self) -> Option<Vec<u8>> {
            Some(self.blob.clone())
        }
    }

    fn render_block(eng: &mut MixEngine, s: &Session, pos: i64) -> f32 {
        let mut out = vec![vec![0.0f32; 512]; 2];
        eng.render(s, pos, 512, &mut out);
        out[0][500]
    }

    #[test]
    fn external_instances_are_adopted_retired_and_captured() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let mut ins = Insert::new("clap:test.fake");
        ins.params.insert("gain".into(), -6.0);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        let specs = instance_specs(&s);
        assert_eq!(specs, vec![InstanceSpec { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, state: None }]);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        // Nothing to adopt yet: the slot passes audio through (and sync never tried to load a
        // CLAP plugin on this thread).
        let dry = render_block(&mut eng, &s, 0);
        assert!(dry > 0.3, "{dry}");
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut gain = soundcraft_dsp::create("gain").unwrap();
        gain.prepare(48_000.0, 512, 1);
        let fake = Stateful { inner: gain, blob: b"live state".to_vec(), sets: Arc::clone(&sets) };
        // An instance for the wrong channel count is retired, not used.
        let wrong = Stateful { inner: soundcraft_dsp::create("gain").unwrap(), blob: Vec::new(), sets: Arc::clone(&sets) };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 2, plugin: Box::new(wrong) }]);
        assert!(render_block(&mut eng, &s, 512) > 0.3);
        assert_eq!(eng.take_retired().len(), 1);
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "clap:test.fake".into(), ch: 1, plugin: Box::new(fake) }]);
        let mut wet = 0.0;
        for b in 2..20 {
            wet = render_block(&mut eng, &s, b * 512);
        }
        assert!((wet / dry - 0.501).abs() < 0.01, "{wet} vs {dry}");
        assert!(sets.load(std::sync::atomic::Ordering::Relaxed) > 0, "no stored state: session params are pushed");
        assert_eq!(eng.capture_states(), vec![(t, 0, b"live state".to_vec())]);
        assert!(eng.take_retired().is_empty());
        // Removing the insert retires the instance instead of dropping it here.
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        render_block(&mut eng, &s, 20 * 512);
        assert_eq!(eng.take_retired().len(), 1);
        assert!(eng.capture_states().is_empty());
        // Instances nobody claims are retired too; drain_plugins empties the engine.
        eng.adopt(vec![PreparedInstance { track: t, slot: 3, id: "clap:x".into(), ch: 1, plugin: soundcraft_dsp::create("gain").unwrap() }]);
        render_block(&mut eng, &s, 21 * 512);
        assert_eq!(eng.take_retired().len(), 1);
        assert!(eng.drain_plugins().is_empty());
    }

    #[test]
    fn stored_state_comes_first_then_session_params() {
        let (mut s, t) = session_with_dc(0.5, 48_000);
        let mut ins = Insert::new("vst3:00000000000000000000000000000001");
        ins.params.insert("gain".into(), -6.0);
        ins.set_state_bytes(b"stored");
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(ins);
        assert_eq!(instance_specs(&s)[0].state.as_deref(), Some(&b"stored"[..]));
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.set_external_instances(true);
        let sets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fake = Stateful { inner: soundcraft_dsp::create("gain").unwrap(), blob: Vec::new(), sets: Arc::clone(&sets) };
        eng.adopt(vec![PreparedInstance { track: t, slot: 0, id: "vst3:00000000000000000000000000000001".into(), ch: 1, plugin: Box::new(fake) }]);
        render_block(&mut eng, &s, 0);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 1, "session params apply once");
        render_block(&mut eng, &s, 512);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 1);
        s.track_mut(t).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("gain".into(), -12.0);
        render_block(&mut eng, &s, 1024);
        assert_eq!(sets.load(std::sync::atomic::Ordering::Relaxed), 2, "later session edits apply");
        // Offline (not external): unknown third-party plugins are simply absent.
        let out = render_range(&s, Range::new(0, 1024), 512);
        assert!(out[0][100] > 0.3);
    }

    #[test]
    fn clip_effects_gain_and_trim_automation_apply() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let cid = s.track(t).unwrap().clips()[0].id.0;
        s.edit.values.insert(format!("clip_fx.{cid}.gain"), -6.0);
        let out = render_range(&s, Range::new(0, 4800), 512);
        let expect = 0.5 * std::f32::consts::FRAC_1_SQRT_2 * db_to_gain(-6.0);
        assert!((out[0][2000] - expect).abs() < 0.01, "{}", out[0][2000]);
        s.edit.set_flag(&format!("clip_fx.bypass.{cid}"), true);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[0][2000] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01);
        s.track_mut(t).unwrap().lane_mut(&AutoParam::Plugin { slot: u8::MAX, param: "trim".into() }).set_point(0, -12.0);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!(out[0][2000] < 0.2, "{}", out[0][2000]);
    }

    #[test]
    fn delay_compensation_aligns_latent_tracks() {
        let mut s = Session::default();
        let mut imp = vec![0.0f32; 48_000];
        imp[1000] = 1.0;
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![imp] })));
        let mut ids = Vec::new();
        for (pan, fx) in [(-1.0f32, true), (1.0, false)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let cid = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(cid, "i", SourceId(900), 0, 0, 48_000));
            tr.mixer.pan = vec![pan];
            if fx {
                tr.mixer.inserts[0] = Some(Insert::new("maximizer"));
            }
            ids.push(t);
        }
        let argmax = |c: &[f32]| c.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i).unwrap();
        let lat = soundcraft_dsp::create("maximizer").unwrap().latency();
        assert!(lat > 0, "test needs a latent plugin");
        let out = render_range(&s, Range::new(0, 8000), 512);
        assert_eq!(argmax(&out[0]), argmax(&out[1]), "PDC should align L and R");
        s.edit.delay_compensation = false;
        let out = render_range(&s, Range::new(0, 8000), 512);
        assert_ne!(argmax(&out[0]), argmax(&out[1]));
    }

    fn impulse_at(ch: &[f32]) -> Vec<usize> {
        ch.iter().enumerate().filter(|(_, v)| v.abs() > 1e-4).map(|(i, _)| i).collect()
    }

    /// An aux insert's lookahead is part of the mix latency on the first block, not the block after.
    #[test]
    fn delay_compensation_aligns_an_aux_insert() {
        let mut s = Session::default();
        let n = 8192;
        let mut left = vec![0.0f32; n];
        for at in [0usize, 2048, 8000] {
            left[at] = 0.25;
        }
        s.pool.insert(SourceId(901), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![left.clone(), left] })));
        let bus = s.add_bus("Test Bus", ChannelFormat::Stereo);
        let source = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Source"));
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Return"));
        let cid = s.new_clip_id();
        s.track_mut(source).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(cid, "impulses", SourceId(901), 0, 0, n as i64));
        s.track_mut(source).unwrap().mixer.output = Route::Bus(bus);
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        let render = |s: &Session, start: i64, end: i64| render_range(s, Range::new(start, end), 1024);
        let control = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&control[0]), vec![0, 2048, 8000]);
        assert_eq!(impulse_at(&control[1]), impulse_at(&control[0]));

        let mut comp = Insert::new("compressor");
        comp.params.insert("lookahead".into(), 5.0);
        comp.params.insert("ratio".into(), 1.0);
        s.track_mut(source).unwrap().mixer.inserts[0] = Some(comp.clone());
        let on_source = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&on_source[0]), impulse_at(&control[0]));

        s.track_mut(source).unwrap().mixer.inserts[0] = None;
        s.track_mut(aux).unwrap().mixer.inserts[0] = Some(comp);
        let on_aux = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&on_aux[0]), vec![0, 2048, 8000], "aux lookahead must stay inside the bounce");
        assert_eq!(impulse_at(&on_aux[1]), impulse_at(&on_aux[0]));
        assert!((on_aux[0][8000] - 0.25).abs() < 1e-3, "{}", on_aux[0][8000]);
        let partial = render(&s, 2048, 3072);
        assert_eq!(impulse_at(&partial[0]), vec![0]);

        s.track_mut(aux).unwrap().mixer.inserts[0].as_mut().unwrap().params.insert("lookahead".into(), 0.0);
        let zero = render(&s, 0, n as i64);
        assert_eq!(impulse_at(&zero[0]), impulse_at(&control[0]));
    }

    #[test]
    fn input_monitoring_replaces_clips() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.input = vec![vec![0.25; 512]];
        let mut out = vec![vec![0.0; 512]; 2];
        // Not monitored: the clip plays.
        eng.render(&s, 0, 512, &mut out);
        assert!((out[0][100] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        // Monitored: the input replaces it, and it still sounds when stopped.
        s.track_mut(t).unwrap().mixer.input_monitor = true;
        eng.render(&s, 512, 512, &mut out);
        assert!((out[0][100] - 0.25 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "{}", out[0][100]);
        eng.monitor_only = true;
        eng.render(&s, 1024, 512, &mut out);
        assert!(out[0][100] > 0.1);
        s.track_mut(t).unwrap().mixer.input_monitor = false;
        eng.render(&s, 1024, 512, &mut out);
        assert!(out[0][100].abs() < 1e-6, "stopped, unmonitored: silence");
    }

    #[test]
    fn empty_session_is_silent() {
        let s = Session::default();
        let out = render_range(&s, Range::new(0, 1000), 128);
        assert!(out.iter().all(|c| c.iter().all(|x| *x == 0.0)));
    }

    #[test]
    fn strip_workspace_keeps_its_allocation_between_blocks() {
        let mut s = Session::default();
        for _ in 0..6 {
            s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        }
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut out = vec![vec![0.0; 512]; 2];
        eng.render(&s, 0, 512, &mut out);
        let capacity = eng.work.capacity();
        assert!(capacity >= 6);
        eng.render(&s, 512, 512, &mut out);
        assert_eq!(eng.work.capacity(), capacity);
    }

    #[test]
    fn stopped_monitoring_does_not_click() {
        let mut s = Session::default();
        s.edit.click = true;
        let mut eng = MixEngine::new(48_000.0, 512);
        eng.metronome = true;
        eng.monitor_only = true;
        let mut out = vec![vec![0.0f32; 512]; 2];
        for _ in 0..4 {
            eng.render(&s, 0, 512, &mut out);
            assert!(out.iter().all(|c| c.iter().all(|x| *x == 0.0)));
        }
    }

    /// Renders `len` frames from zero the way playback does (metronome on), in blocks of `block`.
    fn render_playback(s: &Session, len: usize, block: usize, recording: bool) -> Vec<Vec<f32>> {
        let mut eng = MixEngine::new(s.sample_rate.as_f64() as f32, block);
        eng.metronome = true;
        eng.recording = recording;
        let mut out = vec![vec![0.0f32; len]; 2];
        let mut tmp = vec![vec![0.0f32; block]; 2];
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(block);
            eng.render(s, done as i64, n, &mut tmp);
            for (o, t) in out.iter_mut().zip(tmp.iter()) {
                if let (Some(d), Some(src)) = (o.get_mut(done..done + n), t.get(..n)) {
                    d.copy_from_slice(src);
                }
            }
            done += n;
        }
        out
    }

    #[test]
    fn click_sounds_on_every_beat_only_in_playback() {
        let mut s = Session::default();
        let beat = s.sample_rate.as_f64() as usize * 60 / 120; // 120 BPM quarter notes
        let len = beat * 8;
        s.edit.click = true;
        // Offline renders (bounces, stems) never print the metronome.
        let offline = render_range(&s, Range::new(0, len as i64), 512);
        assert!(offline.iter().all(|c| c.iter().all(|x| *x == 0.0)));

        let out = render_playback(&s, len, 512, false);
        for b in 0..8usize {
            let at = b * beat;
            let on: f32 = out[0][at..at + 200].iter().map(|x| x.abs()).fold(0.0, f32::max);
            let off: f32 = out[0][at + beat / 2..at + beat / 2 + 200].iter().map(|x| x.abs()).fold(0.0, f32::max);
            assert!(on > 0.1, "beat {b} should click, peak {on}");
            assert_eq!(off, 0.0, "between beats must stay silent");
            assert_eq!(out[0][at], out[1][at], "both channels get the click");
        }
        // Blocks that start mid-beat render the same clicks as a single pass.
        let split = render_playback(&s, len, 97, false);
        assert_eq!(out, split);
    }

    #[test]
    fn click_only_during_record_follows_recording() {
        let mut s = Session::default();
        s.edit.click = true;
        s.edit.set_flag("click.only_during_record", true);
        let idle = render_playback(&s, 48_000, 512, false);
        assert!(idle.iter().all(|c| c.iter().all(|x| *x == 0.0)));
        let rec = render_playback(&s, 48_000, 512, true);
        assert!(rec.iter().any(|c| c.iter().any(|x| x.abs() > 0.1)));
    }

    /// A session with one track of `fmt` playing a distinct DC level per channel (0.1, 0.2, …).
    fn multichannel_session(fmt: ChannelFormat, main: ChannelFormat, frames: usize) -> (Session, TrackId) {
        let mut s = Session::default();
        s.outputs[0].format = main;
        let t = s.add_track(TrackKind::Audio, fmt, None);
        let chans: Vec<Vec<f32>> = (0..fmt.channels()).map(|c| (0..frames).map(|i| 0.1 * (c + 1) as f32 + (i % 7) as f32 * 1e-3).collect()).collect();
        s.pool.insert(SourceId(901), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "mc", SourceId(901), 0, 0, frames as i64));
        (s, t)
    }

    #[test]
    fn surround_track_passes_through_bit_exact() {
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround51, 4800);
        let src = s.pool.get(SourceId(901)).unwrap().buffer.channels.clone();
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 6);
        for (o, x) in out.iter().zip(src.iter()) {
            assert_eq!(o, x, "5.1 → 5.1 must be bit-exact");
        }
    }

    #[test]
    fn surround_to_stereo_folds_down_itu() {
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Stereo, 4800);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let x = |c: usize| 0.1 * (c + 1) as f32 + (1000 % 7) as f32 * 1e-3;
        // L R C LFE Ls Rs.
        let l = x(0) + h * x(2) + h * x(4);
        let r = x(1) + h * x(2) + h * x(5);
        assert!((out[0][1000] - l).abs() < 1e-5, "{} vs {l}", out[0][1000]);
        assert!((out[1][1000] - r).abs() < 1e-5, "{} vs {r}", out[1][1000]);
    }

    #[test]
    fn mono_track_on_surround_main_pans() {
        let (mut s, t) = session_with_dc(0.5, 4800);
        s.outputs[0].format = ChannelFormat::Surround51;
        // No surround pan: stereo behaviour on L/R.
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert_eq!(out.len(), 6);
        let h = 0.5 * gains(0.0, PanLaw::Minus3).0;
        assert!((out[0][100] - h).abs() < 1e-6 && (out[1][100] - h).abs() < 1e-6);
        assert!(out[2..].iter().all(|c| c[100].abs() < 1e-7));
        // Front centre: the centre speaker only.
        s.track_mut(t).unwrap().mixer.surround = Some(SurroundPan::default());
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[2][100] - 0.5).abs() < 1e-4, "{}", out[2][100]);
        assert!(out.iter().enumerate().all(|(c, x)| c == 2 || x[100].abs() < 1e-4));
        // Rear right with LFE: Rs plus the LFE send.
        s.track_mut(t).unwrap().mixer.surround = Some(SurroundPan { x: 1.0, y: -1.0, lfe_db: -6.0, ..SurroundPan::default() });
        let mut eng = MixEngine::new(48_000.0, 512);
        let mut buf = vec![vec![0.0; 512]; 6];
        eng.render(&s, 0, 512, &mut buf);
        assert!((buf[5][100] - 0.5).abs() < 1e-4 && (buf[3][100] - 0.5 * db_to_gain(-6.0)).abs() < 1e-4);
        assert_eq!(eng.main_meter.peaks.len(), 6);
        assert_eq!(eng.meters.get(&t).map(|m| m.peaks.len()), Some(1));
        // A stereo device buffer gets the first two channels.
        let mut two = vec![vec![1.0; 512]; 2];
        eng.render(&s, 512, 512, &mut two);
        assert!(two[0][10].abs() < 1e-6);
    }

    #[test]
    fn surround_busses_and_master_fader() {
        let (mut s, t) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround51, 4800);
        let bus = s.add_bus("Stem", ChannelFormat::Surround51);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Surround51, Some("Stem"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        s.track_mut(t).unwrap().mixer.output = Route::Bus(bus);
        let m = s.add_track(TrackKind::Master, ChannelFormat::Surround51, None);
        s.track_mut(m).unwrap().mixer.volume_db = -6.0;
        let out = render_range(&s, Range::new(0, 4800), 512);
        let g = db_to_gain(-6.0);
        for (c, ch) in out.iter().enumerate() {
            let x = 0.1 * (c + 1) as f32 + (1000 % 7) as f32 * 1e-3;
            assert!((ch[1000] - x * g).abs() < 1e-5, "channel {c}: {} vs {}", ch[1000], x * g);
        }
        // A mono bus sums a stereo source like the old stereo bus did.
        let (mut s, t) = session_with_dc(0.5, 4800);
        let bus = s.add_bus("Mono", ChannelFormat::Mono);
        let aux = s.add_track(TrackKind::Aux, ChannelFormat::Mono, Some("M"));
        s.track_mut(aux).unwrap().mixer.input = Route::Bus(bus);
        s.track_mut(aux).unwrap().mixer.pan = vec![-1.0];
        s.track_mut(t).unwrap().mixer.output = Route::Bus(bus);
        let out = render_range(&s, Range::new(0, 4800), 512);
        assert!((out[0][1000] - 0.5 * gains(0.0, PanLaw::Minus3).0).abs() < 1e-6, "{}", out[0][1000]);
    }

    #[test]
    fn surround_upmix_and_downmix_between_layouts() {
        // 5.1 → 7.1: shared speakers map 1:1; 5.x surrounds pan between the 7.1 sides and rears.
        let (s, _) = multichannel_session(ChannelFormat::Surround51, ChannelFormat::Surround71, 2000);
        let out = render_range(&s, Range::new(0, 2000), 512);
        assert_eq!(out.len(), 8);
        let x = |c: usize| 0.1 * (c + 1) as f32 + (100 % 7) as f32 * 1e-3;
        for (c, ch) in out.iter().enumerate().take(4) {
            assert!((ch[100] - x(c)).abs() < 1e-6);
        }
        let e_ls = out[4][100].powi(2) + out[6][100].powi(2);
        assert!((e_ls - x(4).powi(2)).abs() < 1e-5, "Ls energy is preserved");
    }
}
