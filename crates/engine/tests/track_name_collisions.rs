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
