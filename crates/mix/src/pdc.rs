//! Prepared max-plus routing arrivals and independent output/send alignment.
//! Topology, scratch and delay capacity are owned before publication to the callback.

use super::*;
use std::collections::VecDeque;
use std::mem::size_of;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Destination {
    Main,
    Bus(BusId),
}

pub(super) struct Edge {
    pub target: Destination,
    format: ChannelFormat,
    source: usize,
    destination: usize,
    pub scratch: Vec<Vec<f32>>,
    pub align: DelayLine,
}

impl Edge {
    fn new(
        target: Destination,
        format: ChannelFormat,
        source: usize,
        destination: usize,
        channels: usize,
        block: usize,
        capacity: usize,
    ) -> Result<Self, PreparationError> {
        let mut align = DelayLine::default();
        align.reserve(capacity, channels)?;
        Ok(Self { target, format, source, destination, scratch: window_planar(channels, block)?, align })
    }

    pub fn retain_from(&mut self, old: &mut Self) {
        if self.target == old.target
            && self.format == old.format
            && self.align.bufs.len() == old.align.bufs.len()
            && self.align.bufs.iter().zip(&old.align.bufs).all(|(new, old)| old.len() >= new.len())
        {
            std::mem::swap(&mut self.align, &mut old.align);
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Track { index: usize, leaf: Option<usize> },
    Join,
}

struct Node {
    kind: Kind,
    parents: Vec<usize>,
}

pub(super) struct Plan {
    enabled: bool,
    capacity: usize,
    nodes: Vec<Node>,
    order: Vec<usize>,
    own: Vec<usize>,
    arrivals: Vec<usize>,
    tracks: HashMap<TrackId, usize>,
    buses: HashMap<BusId, usize>,
    main: usize,
    masters: Vec<usize>,
    master_latency: usize,
}

fn aux(track: &Track) -> bool {
    matches!(track.kind, TrackKind::Aux | TrackKind::Folder)
}

fn live(track: &Track) -> bool {
    !track.inactive && !matches!(track.kind, TrackKind::Master | TrackKind::Vca)
}

pub(super) fn own_latency(track: &Track, strip: &Strip, rate: f32) -> Result<usize, PreparationError> {
    let mut latency =
        checked_insert_latency(track, strip, rate)?.checked_add(strip.clip_latency).ok_or(PreparationError("ClipFX strip latency overflow"))?;
    if matches!(track.kind, TrackKind::Instrument | TrackKind::Midi)
        && track.instrument.as_ref().is_some_and(|insert| insert.active && !insert.bypass)
    {
        let instrument = strip.instrument.as_ref().ok_or(PreparationError("latency instrument is unavailable"))?;
        latency = latency
            .checked_add(instrument.plugin.latency())
            .filter(|latency| *latency <= (1 << 20))
            .ok_or(PreparationError("instrument latency exceeds prepared limit"))?;
    }
    Ok(latency)
}

/// Conservative admission includes metadata, each actual destination scratch and full bounded
/// delay capacity per edge. It is not a bound on vendor allocation or all process memory.
pub(super) fn workspace_bytes(source: &Session, capacity: usize, block: usize) -> Result<usize, PreparationError> {
    if capacity > (1 << 20) {
        return Err(PreparationError("PDC capacity exceeds limit"));
    }
    let mut bytes = source
        .tracks
        .len()
        .checked_add(source.busses.len())
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(512))
        .ok_or(PreparationError("PDC metadata overflow"))?;
    for track in source.tracks.iter().filter(|track| live(track)) {
        bytes = bytes
            .checked_add(track.mixer.sends.len().checked_mul(size_of::<Option<Edge>>() + 64).ok_or(PreparationError("PDC edge metadata overflow"))?)
            .ok_or(PreparationError("PDC metadata overflow"))?;
        let channels = |target: &Route| match target {
            Route::Main => Some(main_channels(source)),
            Route::Bus(id) => source.bus(*id).map(|bus| bus.format.channels().clamp(1, MAX_CHANNELS)),
            _ => None,
        };
        let mut charge = |channels: usize| -> Result<(), PreparationError> {
            let values =
                capacity.checked_add(block).and_then(|frames| frames.checked_mul(channels)).ok_or(PreparationError("PDC edge workspace overflow"))?;
            let edge = values
                .checked_mul(size_of::<f32>())
                .and_then(|bytes| bytes.checked_add(channels * size_of::<Vec<f32>>() * 2 + 256))
                .ok_or(PreparationError("PDC edge workspace overflow"))?;
            bytes = bytes.checked_add(edge).ok_or(PreparationError("PDC workspace overflow"))?;
            Ok(())
        };
        if let Some(channels) = channels(&track.mixer.output) {
            charge(channels)?;
        }
        for send in track.mixer.sends.iter().flatten() {
            if let Route::Bus(id) = send.target
                && let Some(bus) = source.bus(id)
            {
                charge(bus.format.channels().clamp(1, MAX_CHANNELS))?;
            }
        }
    }
    Ok(bytes)
}

impl Plan {
    pub fn compile(source: &Session, strips: &HashMap<TrackId, Strip>, rate: f32, capacity: usize) -> Result<Self, PreparationError> {
        let mut nodes = Vec::new();
        let mut tracks = HashMap::new();
        let mut buses = HashMap::new();
        let mut masters = Vec::new();
        let mut leaf = 0usize;
        for (index, track) in source.tracks.iter().enumerate() {
            if track.kind == TrackKind::Master && !track.inactive {
                masters.push(index);
            }
            if !live(track) {
                continue;
            }
            let node = nodes.len();
            let leaf_index = if aux(track) {
                None
            } else {
                let index = leaf;
                leaf += 1;
                Some(index)
            };
            nodes.push(Node { kind: Kind::Track { index, leaf: leaf_index }, parents: Vec::new() });
            tracks.insert(track.id, node);
        }
        for bus in &source.busses {
            if buses.insert(bus.id, nodes.len()).is_some() {
                return Err(PreparationError("duplicate routing bus identity"));
            }
            nodes.push(Node { kind: Kind::Join, parents: Vec::new() });
        }
        let main = nodes.len();
        nodes.push(Node { kind: Kind::Join, parents: Vec::new() });
        for track in source.tracks.iter().filter(|track| live(track)) {
            let from = *tracks.get(&track.id).ok_or(PreparationError("PDC source is unavailable"))?;
            if aux(track)
                && let Route::Bus(input) = track.mixer.input
                && let Some(&parent) = buses.get(&input)
            {
                nodes[from].parents.push(parent);
            }
            let target = match track.mixer.output {
                Route::Main => Some(main),
                Route::Bus(id) => buses.get(&id).copied(),
                _ => None,
            };
            if let Some(target) = target {
                nodes[target].parents.push(from);
            }
            for send in track.mixer.sends.iter().flatten() {
                if let Route::Bus(id) = send.target
                    && let Some(&target) = buses.get(&id)
                {
                    nodes[target].parents.push(from);
                }
            }
        }
        let mut order = Vec::new();
        if source.edit.delay_compensation {
            let mut remaining: Vec<_> = nodes.iter().map(|node| node.parents.len()).collect();
            let mut queue: VecDeque<_> = remaining.iter().enumerate().filter_map(|(index, count)| (*count == 0).then_some(index)).collect();
            while let Some(index) = queue.pop_front() {
                order.push(index);
                for (child, node) in nodes.iter().enumerate() {
                    let links = node.parents.iter().filter(|parent| **parent == index).count();
                    if links == 0 {
                        continue;
                    }
                    remaining[child] = remaining[child].checked_sub(links).ok_or(PreparationError("PDC dependency count mismatch"))?;
                    if remaining[child] == 0 {
                        queue.push_back(child);
                    }
                }
            }
            if order.len() != nodes.len() {
                return Err(PreparationError("cyclic routing has no finite latency-compensation plan"));
            }
        } else {
            order.extend(0..nodes.len());
        }
        let count = nodes.len();
        let mut plan = Self {
            enabled: source.edit.delay_compensation,
            capacity,
            nodes,
            order,
            own: vec![0; count],
            arrivals: vec![0; count],
            tracks,
            buses,
            main,
            masters,
            master_latency: 0,
        };
        plan.project_inner(source, strips, &[], rate, true)?;
        Ok(plan)
    }

    /// Uses only compiled arrays/topology and existing strips. No callback allocation/sort.
    pub fn project(
        &mut self,
        source: &Session,
        strips: &HashMap<TrackId, Strip>,
        work: &[(usize, Strip)],
        rate: f32,
    ) -> Result<(), PreparationError> {
        self.project_inner(source, strips, work, rate, false)
    }

    fn project_inner(
        &mut self,
        source: &Session,
        strips: &HashMap<TrackId, Strip>,
        work: &[(usize, Strip)],
        rate: f32,
        initial: bool,
    ) -> Result<(), PreparationError> {
        self.arrivals.fill(0);
        self.master_latency = 0;
        if !self.enabled {
            return Ok(());
        }
        for (node_index, node) in self.nodes.iter().enumerate() {
            let Kind::Track { index, leaf } = node.kind else { continue };
            let track = source.tracks.get(index).ok_or(PreparationError("PDC document track is unavailable"))?;
            let strip = match leaf.and_then(|leaf| work.get(leaf)) {
                Some((actual, strip)) if *actual == index => strip,
                _ => strips.get(&track.id).ok_or(PreparationError("PDC live strip is unavailable"))?,
            };
            self.own[node_index] = if initial {
                strip.latency
            } else if work.is_empty() || leaf.is_none() {
                own_latency(track, strip, rate)?
            } else {
                strip.latency
            };
        }
        for &index in &self.order {
            let node = self.nodes.get(index).ok_or(PreparationError("PDC node is unavailable"))?;
            let mut arrival = 0usize;
            for &parent in &node.parents {
                arrival = arrival.max(*self.arrivals.get(parent).ok_or(PreparationError("PDC parent is unavailable"))?);
            }
            self.arrivals[index] = arrival
                .checked_add(self.own[index])
                .filter(|arrival| *arrival <= self.capacity)
                .ok_or(PreparationError("reported routing latency exceeds prepared capacity"))?;
        }
        for &index in &self.masters {
            let track = source.tracks.get(index).ok_or(PreparationError("PDC Master is unavailable"))?;
            let strip = strips.get(&track.id).ok_or(PreparationError("PDC Master strip is unavailable"))?;
            self.master_latency = self
                .master_latency
                .checked_add(if initial { strip.latency } else { own_latency(track, strip, rate)? })
                .filter(|latency| *latency <= self.capacity)
                .ok_or(PreparationError("Master latency exceeds prepared capacity"))?;
        }
        self.output_latency(None)?;
        Ok(())
    }

    pub fn processing_order(&self) -> Vec<usize> {
        let mut leaves = Vec::new();
        let mut auxes = Vec::new();
        for &index in &self.order {
            if let Kind::Track { index, leaf } = self.nodes[index].kind {
                if leaf.is_some() {
                    leaves.push(index);
                } else {
                    auxes.push(index);
                }
            }
        }
        leaves.sort_unstable();
        leaves.extend(auxes);
        leaves
    }

    pub fn output_latency(&self, bus: Option<BusId>) -> Result<usize, PreparationError> {
        let node = match bus {
            Some(id) => *self.buses.get(&id).ok_or(PreparationError("unknown PDC bus"))?,
            None => self.main,
        };
        let arrival = *self.arrivals.get(node).ok_or(PreparationError("PDC output is unavailable"))?;
        if bus.is_some() {
            return Ok(arrival);
        }
        arrival
            .checked_add(self.master_latency)
            .filter(|latency| *latency <= self.capacity)
            .ok_or(PreparationError("output latency exceeds prepared capacity"))
    }

    pub fn edge_delay(&self, edge: &Edge) -> Result<usize, PreparationError> {
        self.arrivals
            .get(edge.destination)
            .zip(self.arrivals.get(edge.source))
            .and_then(|(destination, source)| destination.checked_sub(*source))
            .ok_or(PreparationError("PDC edge arrival mismatch"))
    }

    pub fn master_latency(&self) -> usize {
        self.master_latency
    }

    pub fn expected_aux_latency(&self, id: TrackId) -> Result<usize, PreparationError> {
        self.tracks.get(&id).and_then(|node| self.own.get(*node)).copied().ok_or(PreparationError("PDC aux is unavailable"))
    }

    pub fn install_edges(&self, source: &Session, strips: &mut HashMap<TrackId, Strip>, block: usize) -> Result<(), PreparationError> {
        for track in source.tracks.iter().filter(|track| live(track)) {
            let from = *self.tracks.get(&track.id).ok_or(PreparationError("PDC source is unavailable"))?;
            let edge = |target: Destination| -> Result<Option<Edge>, PreparationError> {
                let (node, format, channels) = match target {
                    Destination::Main => (self.main, source.main_format(), main_channels(source)),
                    Destination::Bus(id) => {
                        let Some(&node) = self.buses.get(&id) else { return Ok(None) };
                        let format = source.bus(id).ok_or(PreparationError("PDC bus is unavailable"))?.format;
                        (node, format, format.channels().clamp(1, MAX_CHANNELS))
                    }
                };
                Edge::new(target, format, from, node, channels, block, if self.enabled { self.capacity } else { 0 }).map(Some)
            };
            let strip = strips.get_mut(&track.id).ok_or(PreparationError("PDC strip is unavailable"))?;
            let mut output = match track.mixer.output {
                Route::Main => edge(Destination::Main)?,
                Route::Bus(id) => edge(Destination::Bus(id))?,
                _ => None,
            };
            if let (Some(new), Some(old)) = (&mut output, &mut strip.output_edge) {
                new.retain_from(old);
            }
            strip.output_edge = output;
            let mut sends = Vec::new();
            sends.try_reserve_exact(track.mixer.sends.len()).map_err(|_| PreparationError("PDC send allocation failed"))?;
            for send in &track.mixer.sends {
                sends.push(match send.as_ref().map(|send| &send.target) {
                    Some(Route::Bus(id)) => edge(Destination::Bus(*id))?,
                    _ => None,
                });
            }
            for (new, old) in sends.iter_mut().zip(&mut strip.send_edges) {
                if let (Some(new), Some(old)) = (new, old) {
                    new.retain_from(old);
                }
            }
            strip.send_edges = sends;
        }
        Ok(())
    }
}
