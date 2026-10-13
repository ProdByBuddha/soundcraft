//! `AuPlugin`: a loaded Audio Unit behind the safe `soundcraft_dsp::Plugin` trait.

use crate::ffi::Instance;
use soundcraft_dsp::{AutomationPrepareError, MAX_AUTOMATION_NOTES, MAX_AUTOMATION_PARAMS, ParamTarget, Plugin, PluginInfo, ProcessDelivery};

/// Default prepared MIDI-event budget between two blocks.
const EVENT_CAP: usize = 1024;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;

/// A queued MIDI channel message at a frame offset within the next `process` call.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MidiEvent {
    time: u32,
    status: u8,
    data1: u8,
    data2: u8,
}

/// A hosted Audio Unit. Parameter ids are global-scope `AudioUnitParameterID`s as decimal
/// strings; values are the unit's own values, with names and ranges in `info().params`.
pub struct AuPlugin {
    inst: Instance,
    info: &'static PluginInfo,
    /// Audio Unit id of each `info.params` entry (same order).
    ids: Vec<u32>,
    values: Vec<f32>,
    parameter_capacity: usize,
    note_capacity: usize,
    overflowed: bool,
    /// Pending MIDI, kept sorted by time. Never grows past its off-thread prepared logical bound.
    events: Vec<MidiEvent>,
    held: [bool; 128],
    /// Takes audio input (effects); instruments replace the buffer contents.
    has_input: bool,
    /// Accepts MIDI (instruments and music effects).
    takes_midi: bool,
    channels: usize,
    latency: usize,
    tail: usize,
}

impl AuPlugin {
    pub(crate) fn new(inst: Instance, info: &'static PluginInfo, has_input: bool, takes_midi: bool) -> AuPlugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        let values: Vec<f32> = info.params.iter().zip(&ids).map(|(p, &id)| inst.get_param(id).map_or(p.default, |v| p.clamp(v))).collect();
        AuPlugin {
            inst,
            info,
            ids,
            values,
            parameter_capacity: info.params.len(),
            note_capacity: EVENT_CAP,
            overflowed: false,
            events: Vec::with_capacity(EVENT_CAP),
            held: [false; 128],
            has_input,
            takes_midi,
            channels: 2,
            latency: 0,
            tail: 0,
        }
    }

    /// Re-reads every parameter value from the unit (after a state load).
    fn refresh_values(&mut self) {
        for ((slot, p), &id) in self.values.iter_mut().zip(self.info.params).zip(&self.ids) {
            if let Some(v) = self.inst.get_param(id) {
                *slot = p.clamp(v);
            }
        }
    }

    /// Inserts only into prepared storage; overflow is a failed delivery.
    fn push_event(&mut self, e: MidiEvent) -> bool {
        if !self.takes_midi {
            return false;
        }
        if self.events.len() >= self.note_capacity {
            self.overflowed = true;
            return false;
        }
        let pos = self.events.iter().rposition(|q| q.time <= e.time).map_or(0, |i| i + 1);
        self.events.insert(pos, e);
        true
    }

    fn discard_pending(&mut self) {
        self.events.clear();
        self.overflowed = false;
    }
}

impl AuPlugin {
    fn process_audio(&mut self, io: &mut [Vec<f32>], frames: usize) -> ProcessDelivery {
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return ProcessDelivery::ProcessorFailed;
        }
        if !self.inst.is_ready() {
            return ProcessDelivery::ProcessorFailed;
        }
        let max = self.inst.max_frames();
        let au_ch = self.inst.channels();
        if max == 0 {
            return ProcessDelivery::ProcessorFailed;
        }
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.events {
            e.time = e.time.min(last);
        }
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(max);
            if self.has_input {
                for c in 0..au_ch {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, self.inst.input_mut(c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let end = start + n;
            let k = self.events.iter().position(|e| e.time as usize >= end).unwrap_or(self.events.len());
            for e in self.events.iter().take(k) {
                let offset = u32::try_from((e.time as usize).saturating_sub(start)).unwrap_or(0);
                if !self.inst.midi(e.status, e.data1, e.data2, offset) {
                    return ProcessDelivery::ProcessorFailed;
                }
            }
            self.events.drain(..k);
            match self.inst.render(n) {
                Ok(()) => {
                    for (c, dst) in io.iter_mut().take(ch).enumerate() {
                        let Some(dst) = dst.get_mut(start..end) else { continue };
                        if ch == 1 && au_ch >= 2 {
                            if let (Some(l), Some(r)) = (self.inst.output(0, n), self.inst.output(1, n)) {
                                for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                    let v = 0.5 * (a + b);
                                    *d = if v.is_finite() { v } else { 0.0 };
                                }
                            }
                        } else if let Some(src) = self.inst.output(c.min(au_ch.saturating_sub(1)), n) {
                            for (d, &s) in dst.iter_mut().zip(src) {
                                *d = if s.is_finite() { s } else { 0.0 };
                            }
                        }
                    }
                }
                Err(_) => return ProcessDelivery::ProcessorFailed,
            }
            start = end;
        }
        ProcessDelivery::Delivered
    }
}

impl Plugin for AuPlugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        self.channels = channels.clamp(1, MAX_CHANNELS);
        match self.inst.configure(sr, max_block, self.channels) {
            Ok(_) => {
                self.latency = self.inst.latency();
                self.tail = self.inst.tail();
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency = 0;
                self.tail = 0;
            }
        }
        if self.prepare_automation(self.info.params.len(), EVENT_CAP).is_err() {
            self.overflowed = true;
            return;
        }
        // Reconfiguration deliberately restores static values; reserving capacity alone
        // must not overwrite edits made through the unit's editor.
        for (&id, &value) in self.ids.iter().zip(&self.values) {
            if !self.inst.set_param(id, value) {
                self.overflowed = true;
                return;
            }
        }
    }

    fn reset(&mut self) {
        self.discard_pending();
        self.held = [false; 128];
        self.inst.reset();
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return false };
        let (Some(pi), Some(&aid)) = (self.info.params.get(i), self.ids.get(i)) else { return false };
        let v = pi.clamp(value);
        if !self.inst.set_param(aid, v) {
            return false;
        }
        if let Some(slot) = self.values.get_mut(i) {
            *slot = v;
        }
        true
    }

    fn param(&self, id: &str) -> Option<f32> {
        let i = self.info.params.iter().position(|p| p.id == id)?;
        self.values.get(i).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let result = if self.overflowed { ProcessDelivery::CapacityExceeded } else { self.process_audio(io, frames) };
        if result != ProcessDelivery::Delivered {
            self.discard_pending();
            for channel in io.iter_mut().take(self.channels) {
                channel.iter_mut().take(frames).for_each(|sample| *sample = 0.0);
            }
        }
    }

    fn prepare_automation(&mut self, parameter_capacity: usize, note_capacity: usize) -> Result<(), AutomationPrepareError> {
        if parameter_capacity < self.info.params.len() || parameter_capacity > MAX_AUTOMATION_PARAMS || note_capacity > MAX_AUTOMATION_NOTES {
            return Err(AutomationPrepareError::CapacityExceeded);
        }
        self.discard_pending();
        self.events.try_reserve_exact(note_capacity).map_err(|_| AutomationPrepareError::AllocationFailed)?;
        self.parameter_capacity = parameter_capacity;
        self.note_capacity = note_capacity;
        Ok(())
    }

    fn process_with_targets(&mut self, io: &mut [Vec<f32>], frames: usize, targets: &[ParamTarget<'_>]) -> ProcessDelivery {
        if frames == 0 || io.len() < self.channels || io.iter().take(self.channels).any(|channel| channel.len() < frames) {
            self.discard_pending();
            return ProcessDelivery::ProcessorFailed;
        }
        let failure = if self.overflowed || targets.len() > self.parameter_capacity {
            Some(ProcessDelivery::CapacityExceeded)
        } else if targets.iter().any(|t| !t.value.is_finite() || !self.info.params.iter().any(|p| p.id == t.id))
            || self.events.iter().any(|e| e.time as usize >= frames)
        {
            Some(ProcessDelivery::InvalidTarget)
        } else {
            None
        };
        if let Some(failure) = failure {
            self.discard_pending();
            return failure;
        }
        for target in targets {
            if !self.set_param(target.id, target.value) {
                self.discard_pending();
                return ProcessDelivery::InvalidTarget;
            }
        }
        let result = self.process_audio(io, frames);
        if result != ProcessDelivery::Delivered {
            self.discard_pending();
        }
        result
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn tail_samples(&self) -> usize {
        self.tail
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if note > 127 {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        // Velocity 0 would be a note-off.
        if !self.push_event(MidiEvent { time, status: 0x90, data1: note, data2: velocity.clamp(1, 127) }) {
            return;
        }
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = true;
        }
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        if note > 127 {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        if !self.push_event(MidiEvent { time, status: 0x80, data1: note, data2: 0 }) {
            return;
        }
        if let Some(h) = self.held.get_mut(note as usize) {
            *h = false;
        }
    }

    fn all_notes_off(&mut self) {
        for k in 0..128u8 {
            if self.held.get(k as usize).copied().unwrap_or(false) {
                self.note_off(0, k);
            }
        }
        // CC 123: All Notes Off (catches anything the unit still sustains).
        self.push_event(MidiEvent { time: 0, status: 0xB0, data1: 123, data2: 0 });
    }

    fn save_state(&mut self) -> Option<Vec<u8>> {
        self.inst.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        let ok = self.inst.load_state(data);
        if ok {
            self.refresh_values();
        }
        ok
    }
}
