//! Explicit original ownership and playback conversion for native audio imports.

use crate::{SoundApp, theme::Tokens};
use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_time::Samples;

#[derive(Debug, Clone)]
pub struct ImportAudioDialog {
    paths: String,
    policy: String,
    rate_conversion: bool,
    new_tracks: bool,
    use_timestamp: bool,
    at: Samples,
    pub error: String,
}

impl ImportAudioDialog {
    pub(crate) fn new(engine: &Engine, path: Option<String>) -> Self {
        Self {
            paths: path.unwrap_or_default(),
            policy: "copy".into(),
            rate_conversion: false,
            new_tracks: true,
            use_timestamp: false,
            at: engine.session().edit.selection.start.max(0),
            error: String::new(),
        }
    }

    fn params(&self) -> Value {
        let paths: Vec<&str> = self.paths.lines().map(str::trim).filter(|p| !p.is_empty()).collect();
        json!({
            "paths": paths,
            "policy": self.policy,
            "rate_conversion": self.rate_conversion || self.policy == "convert",
            "new_tracks": self.new_tracks,
            "use_timestamp": self.use_timestamp,
            "at": self.at,
        })
    }

    pub(crate) fn show(&mut self, app: &mut SoundApp, ui: &mut egui::Ui, enter: bool) -> Option<Value> {
        ui.set_max_width(520.0);
        ui.label("Audio files — one path per line");
        let paths = ui.add(egui::TextEdit::multiline(&mut self.paths).desired_width(510.0).desired_rows(3));
        if let Some(pick) = &app.services.pick_open
            && ui.button("Add file…").clicked()
            && let Some(path) = pick("file.import_audio", &[])
        {
            if !self.paths.is_empty() {
                self.paths.push('\n');
            }
            self.paths.push_str(&path);
            self.error.clear();
        }
        ui.separator();
        ui.label(egui::RichText::new("Original audio").strong());
        ui.radio_value(&mut self.policy, "copy".into(), "Copy — preserve original bytes in the session");
        ui.radio_value(&mut self.policy, "reference".into(), "Reference — retain the external original file");
        ui.radio_value(&mut self.policy, "convert".into(), "Convert — preserve originals and convert playback audio");
        ui.label(format!("Playback rate: {} Hz", app.engine.session().sample_rate.hz()));
        if self.policy == "convert" {
            ui.label("Mixed-rate files will be converted for playback. Originals keep their rate and BWF metadata.");
        } else {
            ui.checkbox(&mut self.rate_conversion, "Allow session-rate playback conversion for mixed-rate files");
            ui.label("Without this choice, files at a different rate are refused before import.");
        }
        if self.policy == "reference" {
            ui.label("Keep referenced files available and unchanged. Portable copies include verified original bytes.");
        }
        ui.separator();
        ui.checkbox(&mut self.new_tracks, "Create a new audio track for each file");
        if !self.new_tracks {
            ui.label("Use the selected audio track.");
        }
        ui.checkbox(&mut self.use_timestamp, "Place files using their BWF timestamps");
        if !self.use_timestamp {
            ui.label(format!("Insertion: {} session samples", self.at));
        }
        if !self.error.is_empty() {
            ui.colored_label(Tokens::current().rec, self.error.as_str());
        }
        let ready = self.paths.lines().any(|p| !p.trim().is_empty());
        let mut import = false;
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                ui.ctx().memory_mut(|m| m.data.insert_temp(egui::Id::new("dlg_cancel"), true));
            }
            import = ui.add_enabled(ready, egui::Button::new("Import Audio")).clicked() || (ready && enter && !paths.has_focus());
        });
        import.then(|| self.params())
    }
}
