use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, Session, TrackKind};
use std::sync::Arc;

#[test]
fn truncated_ascii_and_unicode_names_cannot_collide_with_another_track() {
    for character in ['A', '界'] {
        let existing: String = std::iter::repeat_n(character, 64).collect();
        let mut session = Session::default();
        let occupied = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&existing));
        let target = session.add_track(TrackKind::Aux, ChannelFormat::Mono, Some("Target"));
        let mut engine = Engine::new(session);
        let before = engine.session_arc();
        let revision = engine.revision;

        let result = engine.execute("track.rename", &json!({"track": target.0, "name": format!("  {existing} extra  ")}));
        assert!(result.is_err());
        assert!(Arc::ptr_eq(&before, &engine.session_arc()));
        assert_eq!(engine.revision, revision);
        assert_eq!(engine.session().track(occupied).unwrap().name, existing);
        assert_eq!(engine.session().track(target).unwrap().name, "Target");
        assert!(!engine.is_dirty());
        assert!(!engine.can_undo());
    }
}

#[test]
fn rename_returns_the_stored_unicode_name_and_undo_restores_the_document() {
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Surround51, Some("Original"));
    session.track_mut(track).unwrap().comments = "Retain this comment".into();
    let mut engine = Engine::new(session);
    let before = engine.session().to_json().unwrap();
    let stored = "é".repeat(64);

    let result = engine.execute("track.rename", &json!({"track": track.0, "name": format!(" {stored}extra ")})).unwrap();
    assert_eq!(result["name"], stored);
    let renamed = engine.session().track(track).unwrap();
    assert_eq!(renamed.name, stored);
    assert_eq!(renamed.name.chars().count(), 64);
    assert_eq!(renamed.id, track);
    assert_eq!(renamed.format, ChannelFormat::Surround51);
    assert_eq!(renamed.comments, "Retain this comment");
    assert_eq!(engine.undo_history(), ["Rename Track"]);
    assert!(engine.undo());
    assert_eq!(engine.session().to_json().unwrap(), before);
    assert!(engine.redo());
    assert_eq!(engine.session().track(track).unwrap().name, stored);
}

#[test]
fn unchanged_name_after_truncation_does_not_make_an_undo_step() {
    let name = "A".repeat(64);
    let mut session = Session::default();
    let track = session.add_track(TrackKind::Audio, ChannelFormat::Mono, Some(&name));
    let mut engine = Engine::new(session);
    let before = engine.session_arc();
    let revision = engine.revision;

    let result = engine.execute("track.rename", &json!({"track": track.0, "name": format!("{name} suffix")})).unwrap();
    assert_eq!(result["name"], name);
    assert!(Arc::ptr_eq(&before, &engine.session_arc()));
    assert_eq!(engine.revision, revision);
    assert!(!engine.is_dirty());
    assert!(!engine.can_undo());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn renamed_track_keeps_its_comments_after_conflict_and_native_reopen() {
    struct OwnedDirectory(std::path::PathBuf);
    impl Drop for OwnedDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let mut session = Session::default();
    let lead = session.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Lead"));
    let room = session.add_track(TrackKind::Aux, ChannelFormat::Mono, Some("Room"));
    assert_ne!(lead, room);
    let mut engine = Engine::new(session);
    let lead_comments = "A: close microphone — 麦克风\nKeep with Lead";
    let room_comments = "B: room return 🎙\nKeep with Room";
    for (track, comments) in [(lead, lead_comments), (room, room_comments)] {
        engine.execute("track.comments", &json!({"track": track.0, "comments": comments})).unwrap();
        assert_eq!(engine.session().track(track).unwrap().comments, comments);
    }
    let unchanged_room = engine.session().track(room).unwrap().clone();

    let renamed = engine.execute("track.rename", &json!({"track": lead.0, "name": "  Lead Vocal  "})).unwrap();
    assert_eq!(renamed["name"], "Lead Vocal");
    let renamed_lead = engine.session().track(lead).unwrap();
    assert_eq!(renamed_lead.id, lead);
    assert_eq!(renamed_lead.name, "Lead Vocal");
    assert_eq!(renamed_lead.kind, TrackKind::Audio);
    assert_eq!(renamed_lead.format, ChannelFormat::Stereo);
    assert_eq!(renamed_lead.comments, lead_comments);
    assert_eq!(engine.session().track(room), Some(&unchanged_room));

    // Keep a real pending redo so rejection must preserve its contents, not just an empty stack.
    engine.execute("track.rename", &json!({"track": room.0, "name": "Room Scratch"})).unwrap();
    assert!(engine.undo());
    assert_eq!(engine.session().track(room), Some(&unchanged_room));
    assert!(engine.can_redo());
    assert_eq!(engine.redo_label(), Some("Rename Track"));
    engine.clipboard.length = 32;
    let before = engine.session_arc();
    let document = engine.session().to_json().unwrap();
    let revision = engine.revision;
    let generation = engine.session_generation();
    let dirty = engine.is_dirty();
    let history = engine.undo_history();
    let redo = engine.redo_label().map(str::to_string);
    let journal = engine.journal.clone();
    let clipboard = engine.clipboard.clone();
    let path = engine.path.clone();
    assert!(dirty);
    assert!(!history.is_empty());
    assert!(!journal.is_empty());

    let rejected = engine.execute("track.rename", &json!({"track": lead.0, "name": "Room"})).unwrap_err();
    assert!(matches!(rejected, soundcraft_engine::EngineError::BadParams(ref command, _) if command == "track.rename"));
    assert!(Arc::ptr_eq(&before, &engine.session_arc()));
    assert_eq!(engine.session().to_json().unwrap(), document);
    assert_eq!(engine.revision, revision);
    assert_eq!(engine.session_generation(), generation);
    assert_eq!(engine.is_dirty(), dirty);
    assert_eq!(engine.undo_history(), history);
    assert!(engine.can_redo());
    assert_eq!(engine.redo_label().map(str::to_string), redo);
    assert_eq!(engine.journal, journal);
    assert_eq!(engine.clipboard, clipboard);
    assert_eq!(engine.path, path);
    assert!(engine.redo());
    assert_eq!(engine.session().track(room).unwrap().name, "Room Scratch");
    assert_eq!(engine.session().track(room).unwrap().comments, room_comments);
    assert_eq!(engine.session().track(lead).unwrap().name, "Lead Vocal");
    assert_eq!(engine.session().track(lead).unwrap().comments, lead_comments);
    assert!(engine.undo());
    assert_eq!(engine.session().to_json().unwrap(), document);

    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!("soundcraft-track-comments-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let owned = OwnedDirectory(directory);
    let file = owned.0.join("Comments.scraft");
    let file_path = file.to_str().unwrap();
    let expected_tracks = engine.session().tracks.clone();
    let saved = engine.execute("session.save", &json!({"path": file_path})).unwrap();
    assert_eq!(saved["path"], file_path);
    assert_eq!(saved["audio_files_written"], 0);
    assert!(file.is_file());
    assert!(!engine.is_dirty());
    assert_eq!(engine.session().tracks, expected_tracks);

    let mut reopened = Engine::default();
    let opened = reopened.execute("session.open", &json!({"path": file_path})).unwrap();
    assert_eq!(opened["tracks"], 2);
    assert_eq!(opened["missing"], json!([]));
    assert_eq!(reopened.session().tracks.len(), 2);
    for expected in &expected_tracks {
        assert_eq!(reopened.session().track(expected.id), Some(expected));
    }
    assert_eq!(reopened.session().track_by_name("Lead Vocal").unwrap().id, lead);
    assert_eq!(reopened.session().track(lead).unwrap().comments, lead_comments);
    assert_eq!(reopened.session().track_by_name("Room").unwrap().id, room);
    assert_eq!(reopened.session().track(room).unwrap().comments, room_comments);
    assert!(reopened.session().track_by_name("Lead").is_none());
    assert_eq!(reopened.path.as_deref(), Some(file_path));
    assert!(!reopened.is_dirty());
    assert!(!reopened.can_undo());
    assert!(!reopened.can_redo());
}
