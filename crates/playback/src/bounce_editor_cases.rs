//! Both real editor cases run from the normal executable's process main thread.

include!("bounce_support.rs");

/// Build and discover the three owned fixtures before the bounded editor runtime phase.
pub fn prepare_owned_fixtures() {
    fixtures();
}

/// Called only by the normal process-main-thread acceptance executable. Uses actual editor handles,
/// actual callback output events, the public capture RPC and the normal idle queue.
pub fn editor_main() {
    fixtures();
    for instrument_slot in [false, true] {
        let (mut s, t) = if instrument_slot {
            instrument(true, 1.5)
        } else {
            let (mut s, t) = session();
            audio(&mut s, t, 0, 1000, 1.0);
            insert(&mut s, t, CLAP_GAIN, &[("7", 1.5)]);
            s.edit.delay_compensation = false;
            (s, t)
        };
        s.track_mut(t).unwrap().comments = "Actual pending editor\n原始 🎙".into();
        let original = Engine::new(s);
        let source = original.session_arc();
        let json = original.session().to_json().unwrap();
        let slot = if instrument_slot { soundcraft_mix::INSTRUMENT_SLOT } else { 0 };
        let mut callback = Harness::new(Arc::clone(&source));
        callback.player.play(0, None, None);
        callback.fill(64, &[64]);
        callback.player.open_editor(t, slot).unwrap();
        // No process/normal idle is inserted between opening the GUI and capture.
        // The fixture's actual gesture arrives only from the next DSP process.
        let snapshot = callback.snapshot(Arc::clone(&source));
        assert!(snapshot.editor_edits.contains(&(t, slot, "7".into(), 0.25)));
        let mut copy = detached(&original, snapshot);
        let expected = if instrument_slot {
            vec![(0..200).map(|at| if at >= 50 { 64.0 / 127.0 * 0.25 } else { 0.0 }).collect::<Vec<_>>(); 2]
        } else {
            (0..2).map(|ch| (0..200).map(|at| signal(ch, at) * 0.25).collect()).collect()
        };
        compare(&exported(&mut copy, Range::new(0, 200), 0, 0, None), &expected, 200, 1e-6);
        let saved_directory = Directory::new("saved-editor-state");
        let saved_path = saved_directory.0.join("CapturedEditor.scraft");
        let expected_track = copy.session().track(t).unwrap().clone();
        copy.execute("session.save", &json!({"path":saved_path})).unwrap();
        let mut reopened = Engine::default();
        let opened = reopened.execute("session.open", &json!({"path":saved_path})).unwrap();
        assert_eq!(opened["missing"], json!([]));
        assert_eq!(reopened.session().track(t), Some(&expected_track));
        compare(&exported(&mut reopened, Range::new(0, 200), 0, 0, None), &expected, 200, 1e-6);
        let edits = callback.player.idle();
        assert!(edits.contains(&(t, slot, "7".into(), 0.25)));
        assert!(callback.player.idle().is_empty());
        let mut normal = Engine::new(original.session().clone());
        for (track, slot, parameter, value) in edits {
            let command = if slot == soundcraft_mix::INSTRUMENT_SLOT { "mix.instrument_param" } else { "mix.insert_param" };
            normal.execute(command, &json!({"track":track.0,"slot":slot,"param":parameter,"value":value})).unwrap();
        }
        let insert = if instrument_slot {
            normal.session().track(t).unwrap().instrument.as_ref().unwrap()
        } else {
            normal.session().track(t).unwrap().mixer.inserts[0].as_ref().unwrap()
        };
        assert_eq!(insert.params["7"], 0.25);
        assert_eq!(original.session().to_json().unwrap(), json);
        assert_eq!(copy.session().track(t).unwrap().comments, "Actual pending editor\n原始 🎙");
        let case = if instrument_slot { "clap_instrument_editor_snapshot" } else { "clap_effect_editor_snapshot" };
        println!("bounce_editor_state::{case}: accepted");
    }
    soundcraft_engine::shutdown_plugin_hosts();
    println!("bounce_editor_state: actual CLAP effect/instrument editor snapshots accepted");
}
