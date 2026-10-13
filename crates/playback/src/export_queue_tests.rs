//! Failed export admission must not lose valid gestures already drained from an editor.
use super::*;

struct BatchEditor(Vec<(String, f32)>);
impl PluginEditor for BatchEditor {
    fn open(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
    fn is_open(&self) -> bool {
        true
    }
    fn idle(&mut self) -> Vec<(String, f32)> {
        std::mem::take(&mut self.0)
    }
}
fn side(batch: Vec<(String, f32)>) -> Side {
    let (_, garbage) = channel();
    Side {
        shadow: HashMap::new(),
        rate: 48_000.0,
        editors: HashMap::from([((TrackId(1), 0), Box::new(BatchEditor(batch)) as Box<dyn PluginEditor>)]),
        pending_editor_edits: Vec::new(),
        pending_editor_batch: None,
        garbage,
    }
}

#[test]
fn valid_gestures_survive_a_rejected_batch_with_invalid_parameters() {
    let mut side = side(vec![("gain".into(), 0.25), ("bad".into(), f32::NAN), ("gain".into(), 0.5), (String::new(), 1.0)]);
    assert!(side.poll_editor_edits().unwrap_err().to_string().contains("invalid"));
    assert!(side.poll_editor_edits().is_err(), "a retry must not silently drop the retained batch");
    assert_eq!(side.take_editor_edits(), vec![(TrackId(1), 0, "gain".into(), 0.25), (TrackId(1), 0, "gain".into(), 0.5)]);
    assert!(side.take_editor_edits().is_empty(), "ordinary idle consumes each valid gesture exactly once");
}

#[test]
fn capacity_refusal_retains_the_whole_drained_batch_until_normal_idle() {
    let expected: Vec<_> = (0..4097).map(|n| (format!("parameter-{n}"), n as f32 / 4096.0)).collect();
    let mut side = side(expected.clone());
    assert!(side.poll_editor_edits().unwrap_err().to_string().contains("capacity"));
    assert!(side.pending_editor_edits.is_empty());
    assert_eq!(side.pending_editor_batch.as_ref().unwrap().edits, expected);
    let actual = side.take_editor_edits();
    assert_eq!(actual.len(), expected.len());
    for (actual, (id, value)) in actual.iter().zip(expected) {
        assert_eq!(actual, &(TrackId(1), 0, id, value));
    }
    assert!(side.take_editor_edits().is_empty());
}

#[cfg(not(target_arch = "wasm32"))]
mod callback_failures {
    use super::*;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, Clip, TrackKind};
    use std::time::{Duration, Instant};

    const LIFETIME: u64 = 873;

    fn prepared_state() -> (AudioState, Sender<Cmd>, Receiver<Retirement>, Arc<Session>) {
        let mut session = Session::default();
        let track = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Export queue literal"));
        let source = soundcraft_engine::io::add_source(
            &mut session,
            "Export queue literal",
            AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.125; 512], vec![-0.25; 512]] },
            None,
            FileFormat::Wav,
        );
        let clip = session.new_clip_id();
        session.track_mut(track).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "Export queue literal", source, 0, 0, 512));
        let session = Arc::new(session);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
        });
        let (sender, receiver) = channel();
        let (garbage, retired) = channel();
        let mut state = AudioState::new(receiver, shared, Arc::clone(&session), 48_000.0, garbage);
        let update = SessionPreparer::default().prepare(Arc::clone(&session), LIFETIME, BLOCK).unwrap();
        sender.send(Cmd::prepared(update, vec![vec![0.0; BLOCK]; 2], fold_gains(&session))).unwrap();
        state.drain();
        while let Ok(retirement) = retired.try_recv() {
            retirement.dispose();
        }
        state.mix.metronome = false;
        state.mix.pan_law = soundcraft_dsp::pan::PanLaw::Zero;
        assert!(state.mix.is_prepared_for(&session, LIFETIME));
        (state, sender, retired, session)
    }

    fn callback(state: AudioState) -> CallbackOwner {
        CallbackOwner { exchange: StateExchange::new(state), enabled: Arc::new(AtomicBool::new(true)), rejected: None }
    }

    fn no_worker_player() -> (Player, CallbackOwner, Arc<Session>) {
        let (state, sender, garbage, source) = prepared_state();
        let shared = Arc::clone(&state.shared);
        let owner = callback(state);
        let player = Player {
            shared,
            tx: sender,
            side: Mutex::new(Side {
                shadow: HashMap::new(),
                rate: 48_000.0,
                editors: HashMap::new(),
                pending_editor_edits: Vec::new(),
                pending_editor_batch: None,
                garbage,
            }),
            preparer: Mutex::new(SessionPreparer::default()),
            exchange: Arc::clone(&owner.exchange),
            clock: None,
            _stream: None,
            device_name: "Controlled export queue with no physical device".into(),
            device_rate: 48_000,
            device_channels: 0,
            silent: true,
        };
        (player, owner, source)
    }

    fn request(source: Arc<Session>, lifetime: u64) -> (Box<ExportCommand>, Receiver<Box<ExportCommand>>) {
        let (reply, receiver) = channel();
        (Box::new(ExportCommand { source, lifetime, result: None, after_render_epoch: None, reply }), receiver)
    }

    struct Worker {
        stop: Arc<AtomicBool>,
        audible: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<CallbackOwner>>,
    }
    impl Worker {
        fn new(mut callback: CallbackOwner) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let audible = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let sounding = Arc::clone(&audible);
            let thread = std::thread::spawn(move || {
                mark_audio_thread();
                let limit = Instant::now() + Duration::from_secs(5);
                while !stopping.load(Ordering::Acquire) && Instant::now() < limit {
                    let mut output = [0.0f32; 32];
                    callback.fill(&mut output, 2);
                    if output.iter().any(|sample| sample.abs() > 0.01) {
                        sounding.store(true, Ordering::Release);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                callback
            });
            Self { stop, audible, thread: Some(thread) }
        }
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                drop(thread.join().unwrap()); // CallbackOwner returns to the caller for disposal.
            }
        }
    }

    #[test]
    fn public_rpc_rejects_equal_clone_and_wrong_lifetime_with_a_matched_audible_control() {
        let (player, callback, source) = no_worker_player();
        let worker = Worker::new(callback);
        let equal_clone = Arc::new(source.as_ref().clone());
        assert_eq!(equal_clone.as_ref(), source.as_ref());
        assert!(!Arc::ptr_eq(&equal_clone, &source));
        let error = player.capture_export_state(equal_clone, LIFETIME).unwrap_err().to_string();
        assert!(error.contains("current prepared document"), "{error}");
        let error = player.capture_export_state(Arc::clone(&source), LIFETIME + 1).unwrap_err().to_string();
        assert!(error.contains("current prepared document"), "{error}");
        player.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let snapshot = player.capture_export_state(Arc::clone(&source), LIFETIME).unwrap();
        assert!(snapshot.states.is_empty() && snapshot.static_baselines.is_empty() && snapshot.editor_edits.is_empty());
        let audible = Arc::clone(&worker.audible);
        drop(worker);
        assert!(audible.load(Ordering::Acquire), "matched control must actually render the nonzero literal source");
        let state = player.exchange.receiver.try_recv().unwrap();
        assert!(state.render_epoch > 0);
        assert!(Arc::ptr_eq(&state.session, &source));
        player.exchange.sender.try_send(state).unwrap();
    }

    #[test]
    fn playing_capture_waits_for_a_natural_epoch_and_stop_refuses_without_forcing_dsp() {
        let (state, sender, _garbage, source) = prepared_state();
        let mut owner = callback(state);
        let (command, reply) = request(Arc::clone(&source), LIFETIME);
        sender.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        sender.send(Cmd::CaptureExport(command)).unwrap();
        owner.fill(&mut [], 2);
        assert!(reply.try_recv().is_err(), "zero callback frames must not trigger a forced plugin process");
        let state = owner.exchange.receiver.try_recv().unwrap();
        assert!(state.playing && state.pending_export.is_some());
        assert_eq!((state.render_epoch, state.pos), (0, 0));
        owner.exchange.sender.try_send(state).unwrap();
        sender.send(Cmd::Stop).unwrap();
        owner.fill(&mut [], 2);
        let mut command = reply.try_recv().unwrap();
        assert!(command.result.take().unwrap().unwrap_err().to_string().contains("stopped before export"));
        let state = owner.exchange.receiver.try_recv().unwrap();
        assert!(!state.playing && state.pending_export.is_none());
        assert_eq!((state.render_epoch, state.pos), (0, 0));
        owner.exchange.sender.try_send(state).unwrap();
    }

    #[test]
    fn an_actual_prepared_shape_failure_cannot_acknowledge_the_export_epoch() {
        let (mut state, sender, _garbage, source) = prepared_state();
        // Explicit private-buffer corruption invokes the real prepared API's shape error;
        // this is not a vendor/DSP arithmetic surrogate or a successful rendering claim.
        state.pending.iter_mut().for_each(Vec::clear);
        let mut owner = callback(state);
        let (command, reply) = request(source, LIFETIME);
        sender.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        sender.send(Cmd::CaptureExport(command)).unwrap();
        let mut output = [1.0f32; 32];
        owner.fill(&mut output, 2);
        assert!(output.iter().all(|value| *value == 0.0));
        let mut command = reply.try_recv().unwrap();
        assert!(command.result.take().unwrap().is_err(), "a failed real render epoch must not certify synchronized capture");
    }

    #[test]
    fn an_abandoned_exact_reply_box_is_retired_then_reclaimed_on_control() {
        let (state, sender, garbage, source) = prepared_state();
        let weak = Arc::downgrade(&source);
        let mut owner = callback(state);
        let (command, reply) = request(source, LIFETIME);
        let identity = std::ptr::from_ref(command.as_ref()) as usize;
        drop(reply);
        sender.send(Cmd::CaptureExport(command)).unwrap();
        let owner = std::thread::spawn(move || {
            mark_audio_thread();
            owner.fill(&mut [], 2);
            owner
        })
        .join()
        .unwrap();
        let retired = garbage.try_recv().unwrap();
        let Retirement::Export(command) = &retired else { panic!("abandoned capture must return its export envelope") };
        assert_eq!(std::ptr::from_ref(command.as_ref()) as usize, identity);
        assert!(command.result.as_ref().unwrap().is_ok());
        drop(owner.exchange.reclaim().unwrap());
        assert!(weak.upgrade().is_some(), "retired envelope still owns the only session after control disposes active state");
        retired.dispose();
        assert!(weak.upgrade().is_none(), "control disposal reclaims the retired envelope's session");
    }

    struct DropProbe(Arc<Mutex<Vec<std::thread::ThreadId>>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }
    impl Plugin for DropProbe {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
            soundcraft_dsp::plugin_info("gain").unwrap()
        }
        fn prepare(&mut self, _rate: f32, _block: usize, _channels: usize) {}
        fn reset(&mut self) {}
        fn set_param(&mut self, _id: &str, _value: f32) -> bool {
            false
        }
        fn param(&self, _id: &str) -> Option<f32> {
            None
        }
        fn process(&mut self, _io: &mut [Vec<f32>], _frames: usize) {}
    }

    #[test]
    fn a_preexisting_rejected_retirement_and_pending_box_survive_a_broken_receiver() {
        let caller = std::thread::current().id();
        let drops = Arc::new(Mutex::new(Vec::new()));
        let (mut old, _old_sender, _old_garbage, _old_source) = prepared_state();
        let weak_old_shared = Arc::downgrade(&old.shared);
        old.rejected_retirement = Some(Retirement::Plugins(vec![Box::new(DropProbe(Arc::clone(&drops)))]));
        let (mut state, _sender, garbage, source) = prepared_state();
        state.rejected_retirement = Some(Retirement::Audio(Box::new(old)));
        let (command, reply) = request(source, LIFETIME);
        let identity = std::ptr::from_ref(command.as_ref()) as usize;
        state.pending_export = Some(command);
        drop(reply);
        drop(garbage); // Deliberately break the normally retained control receiver.
        let mut owner = callback(state);
        let owner = std::thread::spawn(move || {
            mark_audio_thread();
            let mut output = [1.0f32; 32];
            owner.fill(&mut output, 2);
            assert_eq!(output, [0.0; 32]);
            owner
        })
        .join()
        .unwrap();
        assert!(drops.lock().unwrap().is_empty(), "prior heavy retirement must not be overwritten/dropped on the callback");
        assert!(weak_old_shared.upgrade().is_some());
        let state = owner.exchange.reclaim().unwrap();
        assert!(state.rejected_retirement.is_some());
        assert_eq!(std::ptr::from_ref(state.pending_export.as_ref().unwrap().as_ref()) as usize, identity);
        drop(state); // Actual pending and rejected payloads are finally disposed on control.
        assert_eq!(*drops.lock().unwrap(), vec![caller]);
        assert!(weak_old_shared.upgrade().is_none());
    }

    #[test]
    fn public_no_worker_timeout_preserves_queued_editor_gestures_and_reclaims_the_request() {
        let (player, _owner, source) = no_worker_player();
        let track = source.tracks[0].id;
        player.side().editors.insert((track, 0), Box::new(BatchEditor(vec![("gain".into(), 0.25)])));
        let weak_source = Arc::downgrade(&source);
        let begin = Instant::now();
        let error = player.capture_export_state(Arc::clone(&source), LIFETIME).unwrap_err().to_string();
        assert!(begin.elapsed() >= Duration::from_millis(1900), "exercise the actual two-second receiver timeout");
        assert!(error.contains("did not provide an export snapshot"), "{error}");
        assert_eq!(player.side().pending_editor_edits, vec![(track, 0, "gain".into(), 0.25)]);
        assert_eq!(player.idle(), vec![(track, 0, "gain".into(), 0.25)]);
        assert!(player.idle().is_empty());
        drop(source);
        drop(player); // No worker: real Player teardown owns the still-queued export Box.
        assert!(weak_source.upgrade().is_none());
    }

    #[test]
    fn public_disconnected_worker_refuses_immediately_and_preserves_editor_gestures() {
        let (player, _owner, source) = no_worker_player();
        let track = source.tracks[0].id;
        let mut state = player.exchange.receiver.try_recv().unwrap();
        let (_unused_sender, disconnected_receiver) = channel();
        state.rx = disconnected_receiver; // The actual Player command receiver is now gone.
        player.exchange.sender.try_send(state).unwrap();
        player.side().editors.insert((track, 0), Box::new(BatchEditor(vec![("gain".into(), 0.5)])));
        let error = player.capture_export_state(source, LIFETIME).unwrap_err().to_string();
        assert!(error.contains("unavailable for export capture"), "{error}");
        assert_eq!(player.idle(), vec![(track, 0, "gain".into(), 0.5)]);
        assert!(player.idle().is_empty());
    }
}
