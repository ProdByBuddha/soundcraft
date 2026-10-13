pub mod record;

use soundcraft_dsp::{Plugin, PluginEditor};
#[cfg(test)]
use soundcraft_mix::PreparedInstance;
use soundcraft_mix::{MAX_CHANNELS, MixEngine, PreparationError, PreparedMixUpdate, SessionPreparer, StripMeter, itu_stereo_fold, main_channels};
use soundcraft_model::{Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

#[cfg(test)]
mod automation_tests;
#[cfg(test)]
pub mod bounce_capture;
#[cfg(test)]
mod export_queue_tests;

/// Plugin states read from the live instances: `(track, slot or INSTRUMENT_SLOT, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

/// State for a detached export. Pending editor gestures remain queued for normal UI handling.
#[derive(Debug)]
pub struct ExportSnapshot {
    pub states: PluginStates,
    /// Immutable prepared baselines for keys absent from the stored model. Current Read
    /// automation values must never become the starting state of a fresh export instance.
    pub static_baselines: Vec<EditorEdit>,
    pub editor_edits: Vec<EditorEdit>,
}

type ExportState = (PluginStates, Vec<EditorEdit>);

/// Allocated on the control thread and returned intact there, including after a timed-out request.
struct ExportCommand {
    source: Arc<Session>,
    lifetime: u64,
    result: Option<Result<ExportState, PreparationError>>,
    after_render_epoch: Option<u64>,
    reply: Sender<Box<ExportCommand>>,
}

/// One fixed-size envelope allocated on the control thread before publication. The callback
/// fills the initially empty retired slot and returns this same allocation for control-thread
/// disposal. Its storage is separate from the prepared-workspace admission estimate; queued
/// updates and vendor allocations are not a total-memory guarantee.
struct PreparedCommand {
    update: Option<PreparedMixUpdate>,
    pending: Vec<Vec<f32>>,
    fold: [(f32, f32); MAX_CHANNELS],
    retired: Option<RetiredWorkspace>,
}

struct RetiredWorkspace {
    mix: MixEngine,
    pending: Vec<Vec<f32>>,
    session: Arc<Session>,
}

enum Cmd {
    /// A new snapshot plus the third-party instances it needs (created on the UI thread).
    #[cfg(test)]
    Session(Arc<Session>, Vec<PreparedInstance>),
    Prepared(Box<PreparedCommand>),
    /// Read every plugin state and reply.
    CaptureStates(Sender<PluginStates>),
    CaptureExport(Box<ExportCommand>),
    Input(Arc<record::InputRing>),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
}

impl Cmd {
    /// Called only by the control-thread producer (or headless test setup).
    fn prepared(update: PreparedMixUpdate, pending: Vec<Vec<f32>>, fold: [(f32, f32); MAX_CHANNELS]) -> Self {
        Self::Prepared(Box::new(PreparedCommand { update: Some(update), pending, fold, retired: None }))
    }
}

enum Retirement {
    Plugins(Vec<Box<dyn Plugin>>),
    Workspace(Box<PreparedCommand>),
    Audio(Box<AudioState>),
    Export(Box<ExportCommand>),
}

impl Retirement {
    fn dispose(self) {
        match self {
            Self::Plugins(plugins) => drop(plugins),
            Self::Workspace(mut command) => {
                if let Some(workspace) = command.retired.take() {
                    drop(workspace.mix);
                    drop(workspace.pending);
                    drop(workspace.session);
                }
                // Also disposes a rejected/malformed update and frees the original envelope;
                // this method is reached only by the control-thread retirement receiver.
                drop(command);
            }
            Self::Audio(state) => drop(state),
            Self::Export(command) => drop(command),
        }
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        match self {
            Self::Plugins(plugins) => plugins.len(),
            Self::Workspace(_) => 0,
            Self::Audio(_) => 0,
            Self::Export(_) => 0,
        }
    }
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
}

/// State owned by the audio callback.
struct AudioState {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pending_len: usize,
    read: f64,
    device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Retired plugin instances go back to the UI thread to be dropped there.
    garbage: Sender<Retirement>,
    // Preserve a rejected retirement rather than dropping its heavy error value here.
    // Player keeps the receiver alive through shutdown; only private misuse can reject it.
    rejected_retirement: Option<Retirement>,
    /// A malformed private prepared envelope is returned intact and forces silence until a
    /// later valid prepared snapshot is installed; Play/monitoring cannot bypass the guard.
    invalid_prepared: bool,
    /// One control-allocated export envelope waits for a naturally rendered block during
    /// playback. The request never forces DSP, seeks, or changes the transport.
    pending_export: Option<Box<ExportCommand>>,
    render_epoch: u64,
    render_epoch_error: Option<PreparationError>,
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

impl AudioState {
    fn new(rx: Receiver<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, garbage: Sender<Retirement>) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        AudioState {
            garbage,
            rejected_retirement: None,
            invalid_prepared: false,
            pending_export: None,
            render_epoch: 0,
            render_epoch_error: None,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            device_rate,
            input: None,
            recording: false,
        }
    }

    fn drain(&mut self) {
        if self.rejected_retirement.is_some() {
            self.playing = false;
            return;
        }
        #[cfg(test)]
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Recording(r) => self.recording = r,
                Cmd::CaptureStates(reply) => {
                    #[cfg(test)]
                    self.mix.ensure_synced(&self.session);
                    let _ = reply.send(self.mix.capture_states());
                }
                Cmd::CaptureExport(mut command) => {
                    if self.pending_export.is_some() {
                        command.result = Some(Err(PreparationError("another export state request is pending")));
                        self.reply_export(command);
                    } else {
                        command.after_render_epoch = self.playing.then_some(self.render_epoch);
                        self.pending_export = Some(command);
                    }
                }
                #[cfg(test)]
                Cmd::Session(s, instances) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        self.retire(Retirement::Plugins(old.drain_plugins()));
                    }
                    self.mix.adopt(instances);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    if self.pending.len() != nch {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                        self.pending_len = 0;
                        self.read = 0.0;
                    }
                    self.fold = fold_gains(&s);
                    self.session = s;
                }
                Cmd::Prepared(mut command) => {
                    // The sole producer supplies Some(update) and an empty retired slot. Do
                    // not overwrite populated retirement storage or drop a malformed Box here.
                    let update = if command.retired.is_none() { command.update.take() } else { None };
                    let Some(update) = update else {
                        self.invalid_prepared = true;
                        self.playing = false;
                        self.pending_len = 0;
                        self.read = 0.0;
                        self.retire(Retirement::Workspace(command));
                        break;
                    };
                    let session = Arc::clone(update.source());
                    let old_mix = self.mix.install_prepared(update);
                    let old_pending = std::mem::replace(&mut self.pending, std::mem::take(&mut command.pending));
                    let old_session = std::mem::replace(&mut self.session, session);
                    self.fold = command.fold;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.invalid_prepared = false;
                    command.retired = Some(RetiredWorkspace { mix: old_mix, pending: old_pending, session: old_session });
                    self.retire(Retirement::Workspace(command));
                }
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.mix.reset();
                }
                Cmd::Locate(at) => {
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                }
            }
            if self.rejected_retirement.is_some() {
                self.playing = false;
                break;
            }
        }
        if self.invalid_prepared {
            self.playing = false;
        }
        #[cfg(test)]
        if changed {
            // Place adopted instances now (not only once playback starts), so state captures and
            // editors see them; `sync` creates no third-party plugins here.
            self.mix.ensure_synced(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    fn reply_export(&mut self, command: Box<ExportCommand>) {
        let reply = command.reply.clone();
        if let Err(error) = reply.send(command) {
            self.retire(Retirement::Export(error.0));
        }
    }

    fn finish_export_capture(&mut self) {
        // A broken private garbage lifetime must not overwrite/drop a previous heavy
        // retirement on this callback. Control teardown owns both retained envelopes.
        if self.rejected_retirement.is_some() {
            return;
        }
        let Some(request) = self.pending_export.as_ref() else { return };
        let matches =
            !self.invalid_prepared && Arc::ptr_eq(&request.source, &self.session) && self.mix.is_prepared_for(&request.source, request.lifetime);
        let waiting = request.after_render_epoch.is_some_and(|epoch| epoch == self.render_epoch);
        if matches && waiting && self.playing {
            return;
        }
        let Some(mut command) = self.pending_export.take() else { return };
        command.result = Some(if !matches {
            Err(PreparationError("export state does not match the current prepared document"))
        } else if waiting {
            Err(PreparationError("playback stopped before export state synchronization"))
        } else if command.after_render_epoch.is_some() && self.render_epoch_error.is_some() {
            Err(PreparationError("audio rendering failed before export state synchronization"))
        } else {
            self.mix.try_capture_static_baselines().and_then(|baselines| self.mix.try_capture_states().map(|states| (states, baselines)))
        });
        self.reply_export(command);
    }

    fn send_garbage(&mut self) {
        if self.rejected_retirement.is_some() {
            return;
        }
        let g = self.mix.take_retired();
        if !g.is_empty() {
            self.retire(Retirement::Plugins(g));
        }
    }

    fn retire(&mut self, retired: Retirement) {
        if let Err(error) = self.garbage.send(retired) {
            self.rejected_retirement = Some(error.0);
            self.playing = false;
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize) {
        let mut n = quantum.clamp(1, BLOCK);
        if self.invalid_prepared {
            for channel in &mut self.pending {
                channel.fill(0.0);
            }
            self.pending_len = n;
            return;
        }
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                self.playing = false;
                self.end = None;
                self.shared.playing.store(false, Ordering::Relaxed);
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        self.mix.recording = self.recording;
        self.render_epoch_error = self.mix.render_prepared(self.pos, n, &mut self.pending).err();
        if self.render_epoch_error.is_some() {
            for channel in &mut self.pending {
                channel.iter_mut().take(n).for_each(|sample| *sample = 0.0);
            }
        }
        self.render_epoch = self.render_epoch.wrapping_add(1);
        self.send_garbage();
        self.pending_len = n;
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.main.copy_from(&self.mix.main_meter);
            for (k, v) in &self.mix.meters {
                match m.tracks.get_mut(k) {
                    Some(d) => d.copy_from(v),
                    None => {
                        m.tracks.insert(*k, v.clone());
                    }
                }
            }
        }
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Fill an interleaved device buffer.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        self.drain();
        if self.rejected_retirement.is_some() || self.invalid_prepared {
            out.fill(0.0);
            return;
        }
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring {
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.main = StripMeter::default();
                m.tracks.clear();
            }
            return;
        }
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let callback_frames = out.len() / channels;
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        for frame in out.chunks_mut(channels) {
            while self.read >= self.pending_len as f64 {
                self.read -= self.pending_len as f64;
                if !self.playing && !monitoring {
                    break;
                }
                self.render_next(quantum);
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = (self.read - i0 as f64) as f32;
            let sounding = self.playing || monitoring;
            let nch = self.pending.len();
            let sample = |c: usize| {
                let ch = self.pending.get(c);
                let a = ch.and_then(|v| v.get(i0)).copied().unwrap_or(0.0);
                let b = ch.and_then(|v| v.get(i0 + 1)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            if nch > 2 && channels < nch {
                // Fewer device channels than the main mix: ITU fold-down to stereo.
                let (mut l, mut r) = (0.0f32, 0.0f32);
                if sounding {
                    for (c, (gl, gr)) in self.fold.iter().enumerate().take(nch) {
                        let v = sample(c);
                        l += v * gl;
                        r += v * gr;
                    }
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = match c {
                        0 => l.clamp(-1.0, 1.0),
                        1 => r.clamp(-1.0, 1.0),
                        _ => 0.0,
                    };
                }
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    // Stereo mixes on a mono device play the left channel (as before).
                    *o = if c < nch.max(1) && sounding { sample(c).clamp(-1.0, 1.0) } else { 0.0 };
                }
            }
            self.read += step;
        }
    }
}

/// Exactly one preallocated audio state circulates through this capacity-one slot. The
/// control owner never sends or registers a blocking/select waiter after construction.
struct StateExchange {
    sender: crossbeam_channel::Sender<Box<AudioState>>,
    receiver: crossbeam_channel::Receiver<Box<AudioState>>,
    shutdown: AtomicBool,
    returned_to_retirement: AtomicBool,
    initial_failure: Mutex<Option<Box<AudioState>>>,
}

impl StateExchange {
    fn new(state: AudioState) -> Arc<Self> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        // On the control thread, before any producer is published.
        let failed = sender.try_send(Box::new(state)).err().map(crossbeam_channel::TrySendError::into_inner);
        let shutdown = failed.is_some();
        Arc::new(Self {
            sender,
            receiver,
            shutdown: AtomicBool::new(shutdown),
            returned_to_retirement: AtomicBool::new(false),
            initial_failure: Mutex::new(failed),
        })
    }

    fn reclaim(&self) -> Option<Box<AudioState>> {
        if let Some(state) = self.initial_failure.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(state);
        }
        loop {
            match self.receiver.try_recv() {
                Ok(state) => return Some(state),
                Err(crossbeam_channel::TryRecvError::Disconnected) => return None,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    if self.returned_to_retirement.load(Ordering::Acquire) {
                        return None;
                    }
                    // Only shutdown waits; no crossbeam waiter is registered, so callback
                    // notification cannot acquire a waiter lock because of this consumer.
                    #[cfg(not(target_arch = "wasm32"))]
                    std::thread::yield_now();
                    #[cfg(target_arch = "wasm32")]
                    return None;
                }
            }
        }
    }
}

struct CallbackOwner {
    exchange: Arc<StateExchange>,
    enabled: Arc<AtomicBool>,
    rejected: Option<Retirement>,
}

impl CallbackOwner {
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        if !self.enabled.load(Ordering::Acquire) || self.exchange.shutdown.load(Ordering::Acquire) || self.rejected.is_some() {
            out.fill(0.0);
            return;
        }
        let Ok(mut state) = self.exchange.receiver.try_recv() else {
            out.fill(0.0);
            return;
        };
        state.fill(out, channels);
        state.finish_export_capture();
        if let Err(error) = self.exchange.sender.try_send(state) {
            // Full is excluded by the one-state/sole-returner invariant. Disconnection is
            // excluded while this owner holds the exchange's receiver. Preserve ownership
            // even if that invariant is violated: the live control receiver disposes it.
            let state = error.into_inner();
            let garbage = state.garbage.clone();
            self.exchange.shutdown.store(true, Ordering::Release);
            match garbage.send(Retirement::Audio(state)) {
                Ok(()) => self.exchange.returned_to_retirement.store(true, Ordering::Release),
                Err(error) => self.rejected = Some(error.0),
            }
        }
    }
}

struct EditorBatch {
    track: TrackId,
    slot: usize,
    edits: Vec<(String, f32)>,
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    #[cfg(test)]
    shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    #[cfg(test)]
    rate: f32,
    /// Editor handles of the live instances.
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    pending_editor_edits: Vec<EditorEdit>,
    /// Keep a drained vendor batch intact when export admission fails. Ordinary idle
    /// still delivers its valid gestures; a failed export must not consume them.
    pending_editor_batch: Option<EditorBatch>,
    garbage: Receiver<Retirement>,
}

impl Side {
    fn poll_editor_edits(&mut self) -> Result<(), PreparationError> {
        const MAX_EDITS: usize = 4096;
        if self.pending_editor_batch.is_some() {
            return Err(PreparationError("pending editor batch must be processed before export"));
        }
        for (&(track, slot), editor) in self.editors.iter_mut() {
            let edits = editor.idle();
            let error = if edits.len() > MAX_EDITS.saturating_sub(self.pending_editor_edits.len()) {
                Some(PreparationError("pending editor edits exceed export capture capacity"))
            } else if edits.iter().any(|(id, value)| id.is_empty() || id.len() > 4096 || !value.is_finite()) {
                Some(PreparationError("invalid pending editor parameter"))
            } else if self.pending_editor_edits.try_reserve(edits.len()).is_err() {
                Some(PreparationError("editor capture allocation failed"))
            } else {
                None
            };
            if let Some(error) = error {
                self.pending_editor_batch = Some(EditorBatch { track, slot, edits });
                return Err(error);
            }
            self.pending_editor_edits.extend(edits.into_iter().map(|(id, value)| (track, slot, id, value)));
        }
        Ok(())
    }

    fn take_editor_edits(&mut self) -> Vec<EditorEdit> {
        if let Err(error) = self.poll_editor_edits() {
            log::warn!("editor polling: {error}");
        }
        let mut edits = std::mem::take(&mut self.pending_editor_edits);
        if let Some(batch) = self.pending_editor_batch.take() {
            edits.extend(
                batch
                    .edits
                    .into_iter()
                    .filter(|(id, value)| !id.is_empty() && id.len() <= 4096 && value.is_finite())
                    .map(|(id, value)| (batch.track, batch.slot, id, value)),
            );
        }
        edits
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop).
    fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.try_recv() {
            g.dispose();
        }
    }

    #[cfg(test)]
    fn forget(&mut self, key: (TrackId, usize)) {
        self.pending_editor_edits.retain(|(track, slot, _, _)| (*track, *slot) != key);
        if self.pending_editor_batch.as_ref().is_some_and(|batch| (batch.track, batch.slot) == key) {
            self.pending_editor_batch = None;
        }
        self.shadow.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

/// The transport and audio device.
pub struct Player {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    side: Mutex<Side>,
    preparer: Mutex<SessionPreparer>,
    exchange: Arc<StateExchange>,
    #[cfg(not(target_arch = "wasm32"))]
    clock: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.exchange.shutdown.store(true, Ordering::Release);
        self.shared.playing.store(false, Ordering::Relaxed);
        if let Some(stream) = self._stream.take() {
            use cpal::traits::StreamTrait;
            let _ = stream.pause();
            drop(stream);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(clock) = self.clock.take() {
            let _ = clock.join();
        }
        // Receiver and editors remain alive until every callback lease has returned. On
        // WebAudio close() is asynchronous; the state slot is independent of closure lifetime.
        let state = self.exchange.reclaim();
        let side = self.side.get_mut().unwrap_or_else(PoisonError::into_inner);
        for editor in side.editors.values_mut() {
            editor.close();
        }
        side.editors.clear();
        drop(state); // includes active workspace, rejected retirement, and queued commands
        side.collect_garbage();
    }
}

impl Player {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> Player {
        let p = Player::open(Arc::clone(&session));
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn try_new(session: Arc<Session>, lifetime: u64) -> Result<Self, PreparationError> {
        let player = Self::open(Arc::clone(&session));
        player.try_update_session(session, lifetime)?;
        Ok(player)
    }

    fn open(session: Arc<Session>) -> Player {
        let (gtx, grx) = channel();
        let side = Mutex::new(Side {
            #[cfg(test)]
            shadow: HashMap::new(),
            #[cfg(test)]
            rate: session.sample_rate.as_f64() as f32,
            editors: HashMap::new(),
            pending_editor_edits: Vec::new(),
            pending_editor_batch: None,
            garbage: grx,
        });
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let want = session.sample_rate.hz();
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        match open_device(want, want_ch) {
            Ok((device, config, name)) => {
                let rate = config.sample_rate.0;
                let channels = usize::from(config.channels);
                let exchange = StateExchange::new(AudioState::new(rx, Arc::clone(&shared), session, f64::from(rate), gtx));
                // A failed build may keep its closure alive, especially on WebAudio. It
                // cannot acquire the state unless this particular producer was enabled.
                let enabled = Arc::new(AtomicBool::new(false));
                let mut callback = CallbackOwner { exchange: Arc::clone(&exchange), enabled: Arc::clone(&enabled), rejected: None };
                let err_fn = |e: cpal::StreamError| {
                    mark_audio_thread();
                    log::warn!("audio stream error: {e}");
                };
                use cpal::traits::{DeviceTrait, StreamTrait};
                match device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        mark_audio_thread();
                        callback.fill(data, channels);
                    },
                    err_fn,
                    None,
                ) {
                    Ok(stream) => {
                        enabled.store(true, Ordering::Release);
                        if let Err(e) = stream.play() {
                            log::warn!("audio stream failed to start: {e}");
                        }
                        log::info!("audio device: {name} @ {rate} Hz, {channels} ch");
                        Player {
                            shared,
                            tx,
                            side,
                            preparer: Mutex::new(SessionPreparer::default()),
                            exchange,
                            #[cfg(not(target_arch = "wasm32"))]
                            clock: None,
                            _stream: Some(stream),
                            device_name: name,
                            device_rate: rate,
                            device_channels: config.channels,
                            silent: false,
                        }
                    }
                    Err(e) => {
                        log::warn!("cannot open audio stream: {e}; using a silent clock");
                        // The disabled failed producer shares no active lease. Reuse the
                        // original receivers and retirement lifetime rather than abandoning them.
                        Player::silent_clock_with(shared, tx, exchange, rate, side)
                    }
                }
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                let exchange = StateExchange::new(AudioState::new(rx, Arc::clone(&shared), session, f64::from(want), gtx));
                Player::silent_clock_with(shared, tx, exchange, want, side)
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn silent_clock_with(shared: Arc<Shared>, tx: Sender<Cmd>, exchange: Arc<StateExchange>, rate: u32, side: Mutex<Side>) -> Player {
        let mut callback = CallbackOwner { exchange: Arc::clone(&exchange), enabled: Arc::new(AtomicBool::new(true)), rejected: None };
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            while !callback.exchange.shutdown.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * f64::from(rate)) as usize;
                last = now;
                let n = frames.saturating_mul(2).min(buf.len());
                if let Some(b) = buf.get_mut(..n) {
                    callback.fill(b, 2);
                }
            }
        });
        let clock = match spawn {
            Ok(clock) => Some(clock),
            Err(e) => {
                log::warn!("clock thread failed: {e}");
                exchange.shutdown.store(true, Ordering::Release);
                None
            }
        };
        Player {
            shared,
            tx,
            side,
            preparer: Mutex::new(SessionPreparer::default()),
            exchange,
            clock,
            _stream: None,
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            silent: true,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn silent_clock_with(shared: Arc<Shared>, tx: Sender<Cmd>, exchange: Arc<StateExchange>, rate: u32, side: Mutex<Side>) -> Player {
        // No background clock exists in this fallback; reject publication immediately.
        exchange.shutdown.store(true, Ordering::Release);
        Player {
            shared,
            tx,
            side,
            preparer: Mutex::new(SessionPreparer::default()),
            exchange,
            _stream: None,
            device_name: "No audio device".into(),
            device_rate: rate,
            device_channels: 0,
            silent: true,
        }
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, source: Arc<Session>) {
        let _ = self.try_update_session(source, 0);
    }

    /// Compile curves, allocate workspace, and prepare new instances on the caller thread.
    /// `lifetime` changes on Open/New and keeps saved-state captures from recapturing live seeds.
    pub fn try_update_session(&self, source: Arc<Session>, lifetime: u64) -> Result<(), PreparationError> {
        if self.exchange.shutdown.load(Ordering::Acquire) {
            return Err(PreparationError("audio worker is unavailable"));
        }
        let mut preparer = self.preparer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut update = preparer.prepare(Arc::clone(&source), lifetime, BLOCK)?;
        let mut side = self.side();
        side.pending_editor_edits.retain(|(track, slot, _, _)| update.retains_instance(*track, *slot));
        if side.pending_editor_batch.as_ref().is_some_and(|batch| !update.retains_instance(batch.track, batch.slot)) {
            side.pending_editor_batch = None;
        }
        side.editors.retain(|(track, slot), editor| {
            let retained = update.retains_instance(*track, *slot);
            if !retained {
                editor.close();
            }
            retained
        });
        side.collect_garbage();
        for (track, slot, editor) in update.editors() {
            if let Some(mut old) = side.editors.insert((track, slot), editor) {
                old.close();
            }
        }
        let pending = vec![vec![0.0; BLOCK]; main_channels(&source)];
        let command = Cmd::prepared(update, pending, fold_gains(&source));
        if self.tx.send(command).is_err() {
            preparer.invalidate();
            for editor in side.editors.values_mut() {
                editor.close();
            }
            side.editors.clear();
            side.pending_editor_edits.clear();
            side.pending_editor_batch = None;
            return Err(PreparationError("audio worker is unavailable"));
        }
        Ok(())
    }

    /// Reads the state of every live third-party plugin (for saving the session). Waits briefly
    /// for the audio thread to answer between two blocks; empty when it does not answer.
    pub fn capture_states(&self) -> PluginStates {
        self.side().collect_garbage();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (tx, rx) = channel();
            if self.tx.send(Cmd::CaptureStates(tx)).is_err() {
                return Vec::new();
            }
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or_default()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Vec::new()
        }
    }

    /// Capture the matching prepared document's current hosted state without editing a session.
    /// A missing worker, timeout, stale snapshot or unavailable state is an explicit error.
    pub fn capture_export_state(&self, source: Arc<Session>, lifetime: u64) -> Result<ExportSnapshot, PreparationError> {
        {
            let mut side = self.side();
            side.collect_garbage();
            side.poll_editor_edits()?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (reply, receiver) = channel();
            let command = Box::new(ExportCommand { source, lifetime, result: None, after_render_epoch: None, reply });
            self.tx.send(Cmd::CaptureExport(command)).map_err(|_| PreparationError("audio worker is unavailable for export capture"))?;
            let mut command = receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .map_err(|_| PreparationError("audio worker did not provide an export snapshot"))?;
            let (states, static_baselines) = command.result.take().ok_or(PreparationError("audio worker returned no export state"))??;
            let editor_edits = {
                let mut side = self.side();
                side.collect_garbage();
                // The callback replies after a naturally rendered block during playback;
                // collect the editor events from that block without consuming normal idle.
                side.poll_editor_edits()?;
                side.pending_editor_edits.clone()
            };
            Ok(ExportSnapshot { states, static_baselines, editor_edits })
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = lifetime;
            let editor_edits = self.side().pending_editor_edits.clone();
            if !soundcraft_mix::instance_specs(&source).is_empty() || !editor_edits.is_empty() {
                return Err(PreparationError("synchronous live hosted-state export is unavailable in the browser"));
            }
            Ok(ExportSnapshot { states: Vec::new(), static_baselines: Vec::new(), editor_edits })
        }
    }

    /// Per-UI-frame housekeeping. Export polling never consumes these pending gestures.
    pub fn idle(&self) -> Vec<EditorEdit> {
        let mut side = self.side();
        side.collect_garbage();
        side.take_editor_edits()
    }

    /// True when the plugin in this slot has its own editor.
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.contains_key(&(track, slot))
    }

    /// Opens the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        match self.side().editors.get_mut(&(track, slot)) {
            Some(e) => e.open(),
            None => Err("this plugin has no editor (or it is not loaded)".into()),
        }
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get(&(track, slot)).is_some_and(|e| e.is_open())
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
        self.shared.position.store(at, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v, Ordering::Relaxed);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Copy meters into reusable UI storage. Once track/channel counts are stable this performs
    /// no allocation, unlike cloning a new snapshot every video frame.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        let source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        copy_meter_snapshot(&source, out);
    }
}

fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    #[cfg(test)]
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.forget(key);
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(mut plugin) = made {
                if let Some(e) = plugin.editor() {
                    self.editors.insert(key, e);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }
}

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}

/// Open the default output device: f32 at the session rate if possible, with as many channels as
/// it offers up to `want_ch` (at least stereo when available).
fn open_device(want_rate: u32, want_ch: u16) -> Result<(cpal::Device, cpal::StreamConfig, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| "no default output device".to_string())?;
    #[allow(deprecated)]
    let name = device.name().unwrap_or_else(|_| "Audio device".into());
    // Prefer an f32 config at the session rate; else the device default (we resample).
    let want_ch = want_ch.max(2);
    let mut chosen: Option<cpal::StreamConfig> = None;
    if let Ok(configs) = device.supported_output_configs() {
        for c in configs {
            if c.sample_format() == cpal::SampleFormat::F32
                && c.min_sample_rate().0 <= want_rate
                && c.max_sample_rate().0 >= want_rate
                && c.channels() >= 2
                && c.channels() <= want_ch
                && chosen.as_ref().is_none_or(|x| c.channels() > x.channels)
            {
                chosen = Some(c.with_sample_rate(cpal::SampleRate(want_rate)).config());
            }
        }
    }
    let config = match chosen {
        Some(c) => c,
        None => {
            let d = device.default_output_config().map_err(|e| e.to_string())?;
            if d.sample_format() != cpal::SampleFormat::F32 {
                return Err(format!("unsupported device sample format {:?}", d.sample_format()));
            }
            d.config()
        }
    };
    Ok((device, config, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
        let keep = TrackId(1);
        let stale = TrackId(2);
        let mut source = MeterSnapshot::default();
        source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0 });
        source.main.peaks = vec![0.75, 0.5];
        let mut out = MeterSnapshot::default();
        out.tracks.insert(stale, StripMeter::default());
        copy_meter_snapshot(&source, &mut out);
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[&keep], source.tracks[&keep]);
        assert_eq!(out.main, source.main);
    }

    #[test]
    fn audio_state_plays_and_stops_at_end() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(shared.position.load(Ordering::Relaxed) >= 1000);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn short_device_callbacks_render_short_mix_blocks() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn surround_main_maps_to_device_channels_or_folds_down() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        s.outputs[0].format = ChannelFormat::Surround51;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
        let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (garbage, _retired) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, garbage);
        let update = SessionPreparer::default().prepare(Arc::clone(&s), 0, BLOCK).unwrap();
        tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; main_channels(&s)], fold_gains(&s))).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // An 8-channel device: channels in order, the rest silent.
        let mut out = vec![0.0f32; 8 * 256];
        st.fill(&mut out, 8);
        let f = &out[8 * 100..8 * 101];
        for c in 0..6 {
            assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
        }
        assert!(f[6] == 0.0 && f[7] == 0.0);
        // A stereo device: ITU fold-down.
        let mut out = vec![0.0f32; 2 * 256];
        st.fill(&mut out, 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
        assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
        assert_eq!(shared.meters.lock().unwrap().main.peaks.len(), 6);
    }

    struct Stateful(Box<dyn Plugin>);

    impl Plugin for Stateful {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            self.0.info()
        }
        fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
            self.0.prepare(sr, mb, ch);
        }
        fn reset(&mut self) {
            self.0.reset();
        }
        fn set_param(&mut self, id: &str, v: f32) -> bool {
            self.0.set_param(id, v)
        }
        fn param(&self, id: &str) -> Option<f32> {
            self.0.param(id)
        }
        fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
            self.0.process(io, frames);
        }
        fn save_state(&mut self) -> Option<Vec<u8>> {
            Some(b"blob".to_vec())
        }
    }

    #[test]
    fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin: Box<dyn Plugin> = Box::new(Stateful(soundcraft_dsp::create("gain").unwrap()));
        tx.send(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }])).unwrap();
        let (rtx, rrx) = channel();
        tx.send(Cmd::CaptureStates(rtx)).unwrap();
        // Stopped: the callback only drains commands (adopting the instance) and answers.
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert_eq!(rrx.try_recv().unwrap(), vec![(t, 1, b"blob".to_vec())]);
        assert!(grx.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance {
                track: t,
                slot: 1,
                id: "clap:test.fake".into(),
                ch: 2,
                plugin: Box::new(Stateful(soundcraft_dsp::create("gain").unwrap())),
            }],
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.try_recv().unwrap().len(), 1);
    }

    #[test]
    fn side_creates_each_missing_instance_once() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let (_gtx, grx) = channel();
        let mut side = Side {
            shadow: HashMap::new(),
            rate: 48_000.0,
            editors: HashMap::new(),
            pending_editor_edits: Vec::new(),
            pending_editor_batch: None,
            garbage: grx,
        };
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
        assert!(side.prepare(&s).is_empty(), "the plugin is not installed; built-ins stay with the engine");
        assert_eq!(side.shadow.get(&(t, 0)), Some(&("clap:does.not.exist".to_string(), 1, false)));
        assert!(side.prepare(&s).is_empty());
        assert_eq!(side.shadow.len(), 1, "failures are remembered, not retried every update");
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        side.prepare(&s);
        assert!(side.shadow.is_empty());
    }

    #[test]
    fn looping_wraps() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, channel().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
        let mut out = vec![0.0f32; 20_000];
        st.fill(&mut out, 2);
        let p = shared.position.load(Ordering::Relaxed);
        assert!(p <= 1000, "position {p} should stay inside the loop");
        assert!(shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn armed_tracks_are_heard_and_metered_after_playback_stops() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.record_arm = true;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (tx, rx) = channel();
        let s = Arc::new(s);
        let (garbage, _retired) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, garbage);
        let update = SessionPreparer::default().prepare(Arc::clone(&s), 0, BLOCK).unwrap();
        tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; main_channels(&s)], fold_gains(&s))).unwrap();
        let ring = record::InputRing::new(1, 48_000);
        tx.send(Cmd::Input(Arc::clone(&ring))).unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(!shared.playing.load(Ordering::Relaxed));
        ring.push(&[0.5; 4096]);
        st.fill(&mut out, 2);
        assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
        assert!(shared.meters.lock().unwrap().tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
    }
}
