//! Reusable selection definitions, independent of the searchable Clip List UI.
use super::*;
use crate::cmd;
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "clip.capture",
            "Capture...",
            ["Clip"],
            Some("Cmd+R"),
            "{name?, clip?|clips?|tracks?, start?, end?} — saves one reusable definition per selected clip intersection; timeline unchanged",
            has_range,
            crate::clip_capture::capture
        ),
        cmd!(
            "clip.place_definition",
            "Place Reusable Clip",
            [],
            None,
            "{definition: id, track?: id|name, at?: position} — creates a fresh instance; exact position overwrites its destination in every edit mode",
            always,
            crate::clip_capture::place
        ),
        cmd!(query "clip.definitions", "Reusable Clips", [], None, "{}", always,
            |e, _| Ok(json!({"definitions": e.session().clip_definitions}))),
        cmd!(
            "clip.clear_definitions",
            "Clear Reusable Clips",
            [],
            None,
            "{definitions: [id]} — removes definitions; timeline instances and source files remain",
            always,
            crate::clip_capture::clear
        ),
    ]
}
