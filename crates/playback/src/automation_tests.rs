//! Actual headless transport and ownership tests; no audio device is opened.
use super::*;
#[path = "../../mix/tests/support/automation_reference.rs"]
mod reference;
use reference::*;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_dsp::{Category, PluginInfo};
use soundcraft_model::{AutoParam, AutomationLane, AutomationPoint, ChannelFormat, Clip, Insert, Route, SendSlot, SourceAudio, SourceId, TrackKind};
use std::sync::atomic::AtomicUsize;
use test_allocation_counter::{CountingSystem, measure};

#[global_allocator]
static ALLOCATOR: CountingSystem = CountingSystem;

fn fixture() -> (Session, TrackId) {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Original headless source"));
    let source = SourceId(813);
    let channels = (0..2).map(|ch| (0..LEN).map(|at| signal(ch, at) as f32).collect()).collect();
    s.pool.insert(source, Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels })));
    let clip = s.new_clip_id();
    let track = s.track_mut(t).unwrap();
    track.playlist_mut().unwrap().clips.push(Clip::audio(clip, "Original", source, 0, 0, LEN as i64));
    track.mixer.pan = vec![-1.0, 1.0];
    (s, t)
}
fn lane(s: &mut Session, t: TrackId, p: AutoParam, points: &[(i64, f64)]) {
    let mut lane = AutomationLane::new(p);
    lane.points = points.iter().map(|&(at, value)| AutomationPoint { at, value: value as f32 }).collect();
    s.track_mut(t).unwrap().automation.push(lane);
}
fn automated() -> Session {
    let (mut s, t) = fixture();
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
    let bus = s.add_bus("Original headless bus", ChannelFormat::Stereo);
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, Some("Original headless return"));
    let a = s.track_mut(aux).unwrap();
    a.mixer.input = Route::Bus(bus);
    a.mixer.pan = vec![-1.0, 1.0];
    let mut send = SendSlot::new(Route::Bus(bus));
    send.level_db = 0.0;
    s.track_mut(t).unwrap().mixer.sends[0] = Some(send);
    for (param, points) in [
        (AutoParam::Volume, VOLUME),
        (AutoParam::Pan(0), PAN0),
        (AutoParam::Pan(1), PAN1),
        (AutoParam::Mute, MUTE),
        (AutoParam::SendLevel(0), SEND_LEVEL),
        (AutoParam::SendPan(0), SEND_PAN),
        (AutoParam::SendMute(0), SEND_MUTE),
        (AutoParam::Plugin { slot: 0, param: "gain".into() }, PLUGIN_GAIN),
    ] {
        lane(&mut s, t, param, points);
    }
    s
}
fn shared() -> Arc<Shared> {
    Arc::new(Shared {
        position: AtomicI64::new(0),
        playing: AtomicBool::new(false),
        speed: AtomicU32::new(1000),
        meters: Mutex::new(MeterSnapshot::default()),
    })
}
fn state(s: Arc<Session>) -> (AudioState, Sender<Cmd>, Receiver<Retirement>) {
    let (tx, rx) = channel();
    let (gtx, grx) = channel();
    (AudioState::new(rx, shared(), s, 48_000.0, gtx), tx, grx)
}
fn install(st: &mut AudioState, tx: &Sender<Cmd>, s: Arc<Session>) {
    let update = SessionPreparer::default().prepare(Arc::clone(&s), 42, BLOCK).unwrap();
    tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; main_channels(&s)], fold_gains(&s))).unwrap();
    st.drain();
    st.mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
    st.mix.metronome = false;
}
fn expected(start: usize, frames: usize, memory: &mut GainMemory) -> Vec<Vec<f64>> {
    let mut out = vec![vec![0.0; frames]; 2];
    for (n, at) in (start..start + frames).enumerate() {
        let time = at as i64;
        let gain = memory.next(linear(PLUGIN_GAIN, time)) * db_gain(linear(VOLUME, time), -144.0) * if step(MUTE, time) { 0.0 } else { 1.0 };
        let p0 = pan(linear(PAN0, time), None);
        let p1 = pan(linear(PAN1, time), None);
        let balance = pan(linear(SEND_PAN, time), None);
        let send = if step(SEND_MUTE, time) { 0.0 } else { db_gain(linear(SEND_LEVEL, time), -144.0) };
        for (ch, values) in out.iter_mut().enumerate() {
            values[n] = (gain * (signal(0, at) * p0[ch] + signal(1, at) * p1[ch] + signal(ch, at) * balance[ch] * send)).clamp(-1.0, 1.0);
        }
    }
    out
}
fn fill(st: &mut AudioState, frames: usize) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); 2];
    let mut done = 0;
    for size in [1, 7, 37, 64, 100].into_iter().cycle() {
        if done == frames {
            break;
        }
        let n = size.min(frames - done);
        let mut device = vec![0.0; n * 2];
        st.fill(&mut device, 2);
        assert_eq!(device.len(), n * 2);
        for frame in device.chunks_exact(2) {
            out[0].push(frame[0]);
            out[1].push(frame[1]);
        }
        done += n;
    }
    out
}

fn compare_transport(actual: &[Vec<f32>], reference: &[Vec<f64>], frames: usize) {
    assert_eq!(actual.len(), 2);
    assert_eq!(reference.len(), 2);
    for (ch, (actual, expected)) in actual.iter().zip(reference).enumerate() {
        assert_eq!(actual.len(), frames);
        assert_eq!(expected.len(), frames);
        for (at, (&a, &e)) in actual.iter().zip(expected).enumerate() {
            assert!(a.is_finite() && e.is_finite());
            assert!((f64::from(a) - e).abs() < 3e-5, "channel {ch}, frame {at}: {a} versus {e}");
        }
    }
}

#[test]
fn actual_transport_locate_nonzero_loop_and_play_stop_carry_declared_gain_history() {
    let s = Arc::new(automated());
    let (mut st, tx, _retired) = state(Arc::clone(&s));
    install(&mut st, &tx, s);
    let mut memory = GainMemory::new();
    memory.reset();
    tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    let initial = expected(0, 31, &mut memory);
    let actual = fill(&mut st, 31);
    assert!(initial.iter().flatten().any(|x| x.abs() > 0.02));
    assert!(actual.iter().flatten().any(|x| x.abs() > 0.02));
    compare_transport(&actual, &initial, 31);
    tx.send(Cmd::Locate(211)).unwrap();
    compare_transport(&fill(&mut st, 76), &expected(211, 76, &mut memory), 76);
    tx.send(Cmd::Locate(43)).unwrap();
    let reference = expected(43, 97, &mut memory);
    let wrong = expected(43, 97, &mut GainMemory::new());
    different(&reference, &wrong, 0.01);
    compare_transport(&fill(&mut st, 97), &reference, 97);
    tx.send(Cmd::Play { from: 43, end: None, looped: Some(Range::new(43, 211)) }).unwrap();
    memory.reset();
    let mut reference_loop_audible = false;
    let mut actual_loop_audible = false;
    for _ in 0..3 {
        let reference = expected(43, 168, &mut memory);
        let actual = fill(&mut st, 168);
        reference_loop_audible |= reference.iter().flatten().any(|x| x.abs() > 0.02);
        actual_loop_audible |= actual.iter().flatten().any(|x| x.abs() > 0.02);
        compare_transport(&actual, &reference, 168);
        assert_eq!(st.pos, 211);
        assert_eq!(st.shared.position.load(Ordering::Relaxed), 211);
    }
    assert!(reference_loop_audible && actual_loop_audible);
    tx.send(Cmd::Stop).unwrap();
    memory.reset();
    check(&fill(&mut st, 16), &vec![vec![0.0; 16]; 2], 2, 16, 0.0f64 + 1e-12);
    assert!(!st.playing);
    assert!(!st.shared.playing.load(Ordering::Relaxed));
    tx.send(Cmd::Play { from: 94, end: None, looped: None }).unwrap();
    memory.reset();
    compare_transport(&fill(&mut st, 31), &expected(94, 31, &mut memory), 31);
}

#[test]
fn prepared_rate_transition_discards_old_pending_pcm_before_next_fill() {
    let s = Arc::new(automated());
    let (mut st, tx, _retired) = state(Arc::clone(&s));
    install(&mut st, &tx, s);
    st.pending.iter_mut().for_each(|ch| ch.fill(0.75));
    st.pending_len = 31;
    st.read = 3.0;
    let empty = Session { sample_rate: soundcraft_time::SampleRate::new(44_100).unwrap(), ..Session::default() };
    let source = Arc::new(empty);
    let update = SessionPreparer::default().prepare(Arc::clone(&source), 42, BLOCK).unwrap();
    tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&source))).unwrap();
    st.drain();
    assert_eq!(st.pending_len, 0);
    assert_eq!(st.read, 0.0);
    assert_eq!(st.session.sample_rate.hz(), 44_100);
    st.mix.metronome = false;
    tx.send(Cmd::Play { from: 43, end: None, looped: None }).unwrap();
    check(&fill(&mut st, 31), &vec![vec![0.0; 31]; 2], 2, 31, 1e-12);
}

#[test]
fn state_exchange_callback_steady_fill_and_unavailable_lease_have_zero_allocations() {
    let (s, _) = fixture();
    let s = Arc::new(s);
    let (mut st, tx, _retired) = state(Arc::clone(&s));
    install(&mut st, &tx, s);
    tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    let exchange = StateExchange::new(st);
    let enabled = Arc::new(AtomicBool::new(true));
    let mut owner = CallbackOwner { exchange: Arc::clone(&exchange), enabled: Arc::clone(&enabled), rejected: None };
    let mut out = vec![0.0; 62];
    owner.fill(&mut out, 2); // populates first meter entries outside steady measurement
    let (_, allocations) = measure(|| owner.fill(&mut out, 2));
    assert_eq!(allocations, 0);
    let actual = (0..2).map(|ch| out.chunks_exact(2).map(|f| f[ch]).collect()).collect::<Vec<Vec<f32>>>();
    let reference = (0..2).map(|ch| (31..62).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&actual, &reference, 2, 31, 2e-6);
    let lease = exchange.receiver.try_recv().unwrap();
    let (_, allocations) = measure(|| owner.fill(&mut out, 2));
    assert_eq!(allocations, 0);
    assert!(out.iter().all(|x| *x == 0.0));
    exchange.sender.try_send(lease).unwrap();
    enabled.store(false, Ordering::Release);
    out.fill(0.5); // An unavailable lease already cleared it; disabled ownership must clear fresh audible input too.
    let (_, allocations) = measure(|| owner.fill(&mut out, 2));
    assert_eq!(allocations, 0);
    assert!(out.iter().all(|x| *x == 0.0));
    let (_, positive) = measure(|| std::hint::black_box(Box::new(std::hint::black_box(48u64))));
    assert!(positive > 0);
    exchange.shutdown.store(true, Ordering::Release);
    drop(exchange.reclaim());
}

static PROBE_INFO: PluginInfo = PluginInfo {
    id: "original.drop.probe",
    name: "Original drop probe",
    short_name: "Probe",
    category: Category::Other,
    params: &[],
    is_instrument: false,
    audiosuite: false,
};
struct Probe {
    drops: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push(std::thread::current().id());
    }
}
impl Plugin for Probe {
    fn info(&self) -> &'static PluginInfo {
        &PROBE_INFO
    }
    fn prepare(&mut self, _sr: f32, _block: usize, _channels: usize) {}
    fn reset(&mut self) {}
    fn set_param(&mut self, _id: &str, _value: f32) -> bool {
        false
    }
    fn param(&self, _id: &str) -> Option<f32> {
        None
    }
    fn process(&mut self, _io: &mut [Vec<f32>], _frames: usize) {}
}
fn probe_state() -> (AudioState, Sender<Cmd>, Receiver<Retirement>, Arc<Mutex<Vec<std::thread::ThreadId>>>) {
    let (mut s, t) = fixture();
    s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:original.drop.probe"));
    let s = Arc::new(s);
    let (mut st, tx, retired) = state(Arc::clone(&s));
    let drops = Arc::new(Mutex::new(Vec::new()));
    st.mix.adopt(vec![PreparedInstance {
        track: t,
        slot: 0,
        id: "clap:original.drop.probe".into(),
        ch: 2,
        plugin: Box::new(Probe { drops: Arc::clone(&drops) }),
    }]);
    st.mix.ensure_synced(&s);
    st.mix.metronome = false;
    (st, tx, retired, drops)
}

#[test]
fn retired_workspace_processor_drops_only_when_control_collects_it() {
    let caller = std::thread::current().id();
    let (st, tx, retired, drops) = probe_state();
    let clean = Arc::new(fixture().0);
    let update = SessionPreparer::default().prepare(Arc::clone(&clean), 42, BLOCK).unwrap();
    tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&clean))).unwrap();
    let exchange = StateExchange::new(st);
    let shared_exchange = Arc::clone(&exchange);
    std::thread::spawn(move || {
        let mut callback = CallbackOwner { exchange: shared_exchange, enabled: Arc::new(AtomicBool::new(true)), rejected: None };
        callback.fill(&mut [0.0; 2], 2);
    })
    .join()
    .unwrap();
    assert!(drops.lock().unwrap().is_empty());
    let mut side = Side {
        shadow: HashMap::new(),
        rate: 48_000.0,
        editors: HashMap::new(),
        pending_editor_edits: Vec::new(),
        pending_editor_batch: None,
        garbage: retired,
    };
    side.collect_garbage();
    assert_eq!(*drops.lock().unwrap(), vec![caller]);
    exchange.shutdown.store(true, Ordering::Release);
    drop(exchange.reclaim());
}

#[test]
fn rejected_retirement_preserves_processor_until_control_disposes_state() {
    let caller = std::thread::current().id();
    let (mut st, tx, retired, drops) = probe_state();
    drop(retired);
    st.playing = true;
    st.shared.playing.store(true, Ordering::Relaxed);
    let clean = Arc::new(fixture().0);
    let update = SessionPreparer::default().prepare(Arc::clone(&clean), 42, BLOCK).unwrap();
    tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&clean))).unwrap();
    std::thread::spawn(move || {
        let mut output = [0.5; 2];
        st.fill(&mut output, 2);
        assert_eq!(output, [0.0; 2]);
        assert!(st.rejected_retirement.is_some());
        assert!(!st.playing);
        st
    })
    .join()
    .map(|st| {
        assert!(drops.lock().unwrap().is_empty());
        drop(st);
    })
    .unwrap();
    assert_eq!(*drops.lock().unwrap(), vec![caller]);
}

struct EditorProbe {
    closed: Arc<AtomicUsize>,
    drops: Arc<Mutex<Vec<std::thread::ThreadId>>>,
}
impl Drop for EditorProbe {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push(std::thread::current().id());
    }
}
impl PluginEditor for EditorProbe {
    fn open(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {
        self.closed.fetch_add(1, Ordering::Relaxed);
    }
    fn is_open(&self) -> bool {
        true
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        Vec::new()
    }
}
fn headless(st: AudioState, tx: Sender<Cmd>, garbage: Receiver<Retirement>) -> Player {
    let shared = Arc::clone(&st.shared);
    let exchange = StateExchange::new(st);
    Player {
        shared,
        tx,
        side: Mutex::new(Side {
            shadow: HashMap::new(),
            rate: 48_000.0,
            editors: HashMap::new(),
            pending_editor_edits: Vec::new(),
            pending_editor_batch: None,
            garbage,
        }),
        preparer: Mutex::new(SessionPreparer::default()),
        exchange,
        clock: None,
        _stream: None,
        device_name: "Original controlled headless".into(),
        device_rate: 48_000,
        device_channels: 0,
        silent: true,
    }
}

#[test]
fn actual_publication_retains_editor_only_for_same_prepared_instance() {
    let caller = std::thread::current().id();
    for replacement in ["removed", "changed-plugin", "changed-rate", "new-document"] {
        let (mut s, t) = fixture();
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("gain"));
        let (st, tx, retired) = state(Arc::new(s.clone()));
        let player = headless(st, tx, retired);
        player.try_update_session(Arc::new(s.clone()), 42).unwrap();
        let mut lease = player.exchange.receiver.try_recv().unwrap();
        lease.drain();
        player.exchange.sender.try_send(lease).unwrap();
        let closed = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(Mutex::new(Vec::new()));
        player.side().editors.insert((t, 0), Box::new(EditorProbe { closed: Arc::clone(&closed), drops: Arc::clone(&drops) }));
        s.track_mut(t).unwrap().comments = "Original revision retains prepared identity".into();
        player.try_update_session(Arc::new(s.clone()), 42).unwrap();
        assert_eq!(closed.load(Ordering::Relaxed), 0);
        assert!(drops.lock().unwrap().is_empty());
        assert!(player.has_editor(t, 0));
        let lifetime = match replacement {
            "removed" => {
                s.track_mut(t).unwrap().mixer.inserts[0] = None;
                42
            }
            "changed-plugin" => {
                s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("trim"));
                42
            }
            "changed-rate" => {
                s.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
                42
            }
            _ => 43,
        };
        player.try_update_session(Arc::new(s), lifetime).unwrap();
        assert_eq!(closed.load(Ordering::Relaxed), 1);
        assert_eq!(*drops.lock().unwrap(), vec![caller]);
        assert!(!player.has_editor(t, 0));
        drop(player);
    }
}

#[test]
fn headless_shutdown_joins_owned_clock_closes_editor_and_disposes_active_and_queued_state_on_control() {
    let caller = std::thread::current().id();
    let (st, tx, retired, drops) = probe_state();
    let mut player = headless(st, tx, retired);
    let closed = Arc::new(AtomicUsize::new(0));
    let editor_drops = Arc::new(Mutex::new(Vec::new()));
    player.side().editors.insert((TrackId(1), 0), Box::new(EditorProbe { closed: Arc::clone(&closed), drops: Arc::clone(&editor_drops) }));
    let clean = Arc::new(fixture().0);
    let weak = Arc::downgrade(&clean);
    let update = SessionPreparer::default().prepare(Arc::clone(&clean), 42, BLOCK).unwrap();
    player.tx.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&clean))).unwrap();
    drop(clean);
    let exchange = Arc::clone(&player.exchange);
    let joined = Arc::new(AtomicBool::new(false));
    let ended = Arc::clone(&joined);
    let (ready_tx, ready_rx) = channel();
    player.clock = Some(std::thread::spawn(move || {
        let mut callback = CallbackOwner { exchange: Arc::clone(&exchange), enabled: Arc::new(AtomicBool::new(false)), rejected: None };
        callback.fill(&mut [0.0; 2], 2);
        ready_tx.send(()).unwrap();
        while !exchange.shutdown.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        ended.store(true, Ordering::Release);
    }));
    ready_rx.recv().unwrap();
    assert!(weak.upgrade().is_some());
    assert!(drops.lock().unwrap().is_empty());
    drop(player);
    assert!(joined.load(Ordering::Acquire));
    assert_eq!(closed.load(Ordering::Relaxed), 1);
    assert_eq!(*editor_drops.lock().unwrap(), vec![caller]);
    assert_eq!(*drops.lock().unwrap(), vec![caller]);
    assert!(weak.upgrade().is_none());
}

#[test]
fn actual_native_silent_clock_constructor_joins_and_disposes_on_control() {
    let caller = std::thread::current().id();
    let (st, tx, garbage, drops) = probe_state();
    let shared = Arc::clone(&st.shared);
    let exchange = StateExchange::new(st);
    let closed = Arc::new(AtomicUsize::new(0));
    let editor_drops = Arc::new(Mutex::new(Vec::new()));
    let mut editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>> = HashMap::new();
    editors.insert((TrackId(1), 0), Box::new(EditorProbe { closed: Arc::clone(&closed), drops: Arc::clone(&editor_drops) }));
    let side =
        Mutex::new(Side { shadow: HashMap::new(), rate: 48_000.0, editors, pending_editor_edits: Vec::new(), pending_editor_batch: None, garbage });
    let weak_shared = Arc::downgrade(&shared);
    let player = Player::silent_clock_with(Arc::clone(&shared), tx, Arc::clone(&exchange), 48_000, side);
    assert!(player._stream.is_none());
    assert!(player.silent);
    let clock = player.clock.as_ref().expect("actual native silent constructor creates its owned clock");
    assert_ne!(clock.thread().id(), caller);
    player.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    // This only bounds fixture liveness; it measures no callback deadline or device performance.
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while shared.position.load(Ordering::Relaxed) == 0 {
        assert!(std::time::Instant::now() < ready_deadline, "owned silent clock did not process its Play command");
        std::thread::yield_now();
    }
    assert!(drops.lock().unwrap().is_empty());
    drop(shared);
    drop(player); // actual production shutdown/join/reclaim/editor/retirement order
    assert_eq!(closed.load(Ordering::Relaxed), 1);
    assert_eq!(*editor_drops.lock().unwrap(), vec![caller]);
    assert_eq!(*drops.lock().unwrap(), vec![caller]);
    assert!(weak_shared.upgrade().is_none());
    assert!(exchange.receiver.try_recv().is_err(), "heavy state was reclaimed despite an external empty exchange handle");
}

#[test]
fn prepared_envelope_returns_same_control_allocation_and_disposes_old_processor_on_control() {
    let caller = std::thread::current().id();
    let (st, tx, retired, drops) = probe_state();
    let source = Arc::new(fixture().0);
    let update = SessionPreparer::default().prepare(Arc::clone(&source), 42, BLOCK).unwrap();
    let command = Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&source));
    let address = match &command {
        Cmd::Prepared(envelope) => {
            assert!(envelope.update.is_some() && envelope.retired.is_none());
            std::ptr::from_ref(envelope.as_ref()) as usize
        }
        _ => panic!("control constructor must return the prepared envelope"),
    };
    tx.send(command).unwrap();
    tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
    let exchange = StateExchange::new(st);
    let callback_exchange = Arc::clone(&exchange);
    let actual = std::thread::spawn(move || {
        let mut callback = CallbackOwner { exchange: callback_exchange, enabled: Arc::new(AtomicBool::new(true)), rejected: None };
        let mut device = vec![0.875; 31 * 2];
        callback.fill(&mut device, 2);
        (0..2).map(|ch| device.chunks_exact(2).map(|frame| frame[ch]).collect()).collect::<Vec<Vec<f32>>>()
    })
    .join()
    .unwrap();
    let expected = (0..2).map(|ch| (0..31).map(|at| signal(ch, at)).collect()).collect::<Vec<Vec<f64>>>();
    check(&actual, &expected, 2, 31, 2e-6);
    assert!(actual.iter().flatten().any(|x| x.abs() > 0.02));
    assert!(drops.lock().unwrap().is_empty(), "callback retained the old processor for control disposal");
    let returned = retired.try_recv().unwrap();
    match &returned {
        Retirement::Workspace(envelope) => {
            assert_eq!(std::ptr::from_ref(envelope.as_ref()) as usize, address);
            assert!(envelope.update.is_none() && envelope.retired.is_some());
        }
        _ => panic!("prepared install must return the original envelope with its retired workspace"),
    }
    returned.dispose();
    assert_eq!(*drops.lock().unwrap(), vec![caller]);
    exchange.shutdown.store(true, Ordering::Release);
    drop(exchange.reclaim());
    // Pointer/ownership evidence does not count allocator deallocation or establish a deadline.
}
