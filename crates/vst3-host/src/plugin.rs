//! `Vst3Plugin`: a loaded VST3 instance behind the safe `soundcraft_dsp::Plugin` trait.

use crate::Vst3Error;
use crate::ffi::{AFTERTOUCH, BusBuffers, EVENT_CAP, EditorLink, HostContext, Instance, NoteEvent, PARAM_QUEUE_CAP, PITCH_BEND};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, Transport, Unit};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use vst3::ComWrapper;

/// Largest block we set a plugin up for.
const MAX_BLOCK: usize = 1 << 16;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;
/// An `unsynced` slot with no value waiting (a NaN pattern, which `set_param` never stores).
const SYNCED: u64 = u64::MAX;

/// Restore parameter values consumed for a failed state save. Only replace the empty sentinel:
/// a newer audio-thread edit must win over the older value captured by the reader.
fn restore_pending_after_failed_save(slots: &[AtomicU64], consumed: &[(usize, u64)]) {
    for &(index, bits) in consumed {
        if let Some(slot) = slots.get(index) {
            let _ = slot.compare_exchange(SYNCED, bits, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
}

/// How a SoundCraft parameter value maps to the VST3 normalized value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mapping {
    /// Toggle/choice index `0..=steps` ↔ `index / steps`.
    Stepped(f64),
    /// Plain value through the controller's `plainParamToNormalized`.
    Plain,
    /// Linear over the info range (the controller gave no usable plain range).
    Linear,
}

/// A hosted VST3 plugin. Parameter ids are VST3 `ParamID`s as decimal strings ("0", "17", …);
/// values are plain (display) values with their names and ranges in `info().params`.
pub struct Vst3Plugin {
    inst: Instance,
    info: &'static PluginInfo,
    /// VST3 id of each `info.params` entry (same order).
    ids: Vec<u32>,
    maps: Vec<Mapping>,
    values: Vec<f32>,
    /// Normalized values waiting for the next block (coalesced per id; bounded).
    pending: Vec<(u32, f64)>,
    /// Host-set normalized values the controller has not seen yet (same order as `ids`; the bits
    /// of an `f64`, or [`SYNCED`]). `set_param` may run on the audio thread, so it never calls the
    /// controller; the state reader, which already does, hands them over first.
    unsynced: Arc<[AtomicU64]>,
    /// Pending notes, kept sorted by time. Never grows past `EVENT_CAP`.
    notes: Vec<NoteEvent>,
    /// The parameter each MIDI controller drives (see `Instance::midi_mapping`).
    midi_map: Vec<Option<u32>>,
    /// Controller points for the next block: (time, param id, normalized value). Bounded.
    controls: Vec<(u32, u32, f64)>,
    /// ParamID of the last accepted MIDI CC64=0 not superseded by a later write.
    /// This proves provenance: another zero-valued controller may alias the same ParamID.
    sustain_release: Option<u32>,
    inputs: BusBuffers,
    outputs: BusBuffers,
    has_input: bool,
    has_output: bool,
    event_input: bool,
    held: [bool; 128],
    channels: usize,
    max_block: usize,
    latency: usize,
    tail: usize,
    position: i64,
    transport: Option<Transport>,
}

impl Vst3Plugin {
    pub(crate) fn new(inst: Instance, info: &'static PluginInfo) -> Vst3Plugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        let maps: Vec<Mapping> = info
            .params
            .iter()
            .zip(&ids)
            .map(|(p, &id)| match p.unit {
                Unit::Toggle | Unit::Choice => Mapping::Stepped(f64::from(p.max.max(1.0))),
                _ => {
                    let lo = inst.to_plain(id, 0.0);
                    let hi = inst.to_plain(id, 1.0);
                    match (lo, hi) {
                        (Some(lo), Some(hi))
                            if (lo as f32 - p.min).abs() <= 1e-6 * p.min.abs().max(1.0)
                                && (hi as f32 - p.max).abs() <= 1e-6 * p.max.abs().max(1.0) =>
                        {
                            Mapping::Plain
                        }
                        _ => Mapping::Linear,
                    }
                }
            })
            .collect();
        let mut p = Vst3Plugin {
            info,
            values: Vec::with_capacity(ids.len()),
            pending: Vec::with_capacity(ids.len().min(PARAM_QUEUE_CAP)),
            unsynced: ids.iter().map(|_| AtomicU64::new(SYNCED)).collect(),
            notes: Vec::with_capacity(EVENT_CAP),
            midi_map: Vec::new(),
            controls: Vec::with_capacity(EVENT_CAP),
            sustain_release: None,
            inputs: BusBuffers::new(&[], 0),
            outputs: BusBuffers::new(&[], 0),
            has_input: false,
            has_output: false,
            event_input: false,
            held: [false; 128],
            channels: 2,
            max_block: 0,
            latency: 0,
            tail: 0,
            position: 0,
            transport: None,
            ids,
            maps,
            inst,
        };
        let values: Vec<f32> = (0..p.ids.len())
            .map(|i| {
                let (Some(pi), Some(&id)) = (p.info.params.get(i), p.ids.get(i)) else { return 0.0 };
                p.inst.normalized_value(id).and_then(|n| p.plain_from_normalized(i, n)).map_or(pi.default, |v| pi.clamp(v))
            })
            .collect();
        p.values = values;
        p
    }

    fn plain_from_normalized(&self, i: usize, n: f64) -> Option<f32> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let v = match self.maps.get(i)? {
            Mapping::Stepped(steps) => (n * steps).round(),
            Mapping::Plain => self.inst.to_plain(id, n)?,
            Mapping::Linear => f64::from(pi.min) + n * f64::from(pi.max - pi.min),
        };
        v.is_finite().then_some(v as f32)
    }

    fn normalized_from_plain(&self, i: usize, v: f32) -> Option<f64> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let span = f64::from(pi.max - pi.min);
        let linear = if span > 0.0 { (f64::from(v) - f64::from(pi.min)) / span } else { 0.0 };
        let n = match self.maps.get(i)? {
            Mapping::Stepped(steps) if *steps > 0.0 => f64::from(v) / steps,
            Mapping::Stepped(_) => 0.0,
            Mapping::Plain => self.inst.to_normalized(id, f64::from(v)).unwrap_or(linear),
            Mapping::Linear => linear,
        };
        n.is_finite().then(|| n.clamp(0.0, 1.0))
    }

    /// The editor's size in pixels without opening it (creates and releases a view). For
    /// diagnostics and tests; real plugins expect this on the main thread.
    pub fn editor_size(&self) -> Result<(u32, u32), String> {
        self.inst.editor().ok_or_else(|| "this plugin has no edit controller".to_string())?.probe()
    }

    /// Re-reads every parameter value from the controller (after a state load) and drops
    /// changes still queued from before it.
    fn refresh_values(&mut self) {
        for i in 0..self.ids.len() {
            let (Some(pi), Some(&id)) = (self.info.params.get(i), self.ids.get(i)) else { continue };
            if let Some(v) = self.inst.normalized_value(id).and_then(|n| self.plain_from_normalized(i, n))
                && let Some(slot) = self.values.get_mut(i)
            {
                *slot = pi.clamp(v);
            }
        }
        self.pending.clear();
        for u in self.unsynced.iter() {
            u.store(SYNCED, Ordering::Relaxed);
        }
    }

    /// Bounded and time-sorted. A note-off can replace a lower-priority event so even
    /// a saturated note queue has room to stop voices.
    fn push_note(&mut self, e: NoteEvent) -> bool {
        if matches!(e, NoteEvent::Off { .. }) { push_release(&mut self.notes, e) } else { push_sorted(&mut self.notes, e) }
    }
}

fn push_sorted(notes: &mut Vec<NoteEvent>, e: NoteEvent) -> bool {
    if notes.len() >= notes.capacity() {
        return false;
    }
    let pos = notes.iter().rposition(|q| q.time() <= e.time()).map_or(0, |i| i + 1);
    notes.insert(pos, e);
    true
}

/// Prioritize note-offs without allocating on the audio thread. Evict a note-on or pressure
/// event first; if the queue contains only releases, a duplicate release can be replaced.
/// Never discard a unique release, and let the caller keep its held-note bookkeeping on failure.
fn push_release(notes: &mut Vec<NoteEvent>, e: NoteEvent) -> bool {
    if notes.len() >= notes.capacity() {
        let eviction = notes.iter().rposition(|q| !matches!(q, NoteEvent::Off { .. })).or_else(|| {
            let mut seen = [false; 128];
            notes.iter().position(|q| {
                if let NoteEvent::Off { key, .. } = *q {
                    seen.get_mut(usize::from(key)).is_some_and(|seen| std::mem::replace(seen, true))
                } else {
                    false
                }
            })
        });
        let Some(i) = eviction else { return false };
        notes.remove(i);
    }
    push_sorted(notes, e)
}

/// A stop must cancel notes scheduled later in the same block, or a release at frame zero
/// could be followed by a new note-on and leave the instrument sounding after transport stops.
fn cancel_pending_starts(notes: &mut Vec<NoteEvent>) {
    notes.retain(|e| !matches!(e, NoteEvent::On { .. }));
}

/// Keeps only the pending releases, one per key and moved to the start of the next block, so at
/// most 128 remain and `all_notes_off` always has room in the queue.
fn keep_releases(notes: &mut Vec<NoteEvent>) {
    let mut seen = [false; 128];
    notes.retain(|e| match *e {
        NoteEvent::Off { key, .. } => seen.get_mut(usize::from(key)).is_some_and(|s| !std::mem::replace(s, true)),
        _ => false,
    });
    for e in notes.iter_mut() {
        e.set_time(0);
    }
}

/// A newer accepted write to the same ParamID supersedes a previous pedal-up.
/// Never infer sustain provenance from the value alone: distinct MIDI CCs may alias.
fn supersede_sustain_release(sustain_release: &mut Option<u32>, id: u32) {
    if *sustain_release == Some(id) {
        *sustain_release = None;
    }
}

/// Queue mapped MIDI without allocating. Pedal-up can replace the last queued
/// controller point when saturated; ordinary controllers cannot evict releases.
/// Only a truly accepted CC64=0 establishes retry provenance.
fn queue_mapped_control(controls: &mut Vec<(u32, u32, f64)>, sustain_release: &mut Option<u32>, control: (u32, u32, f64), sustain_off: bool) -> bool {
    if controls.len() >= controls.capacity() {
        if !sustain_off || controls.is_empty() {
            return false;
        }
        let _ = controls.pop();
    }
    controls.push(control);
    if sustain_off {
        *sustain_release = Some(control.1);
    } else {
        supersede_sustain_release(sustain_release, control.1);
    }
    true
}

/// Finalize only after the entire process call, not after each successful
/// sub-block: later-timestamped controllers still need their native chunk.
/// On error/reset, retry exactly one idempotent sustain-up at frame zero;
/// discard all other controllers that may have been partially consumed.
fn finish_mapped_controls(controls: &mut Vec<(u32, u32, f64)>, sustain_release: &mut Option<u32>, retry: bool) {
    let release = if retry { *sustain_release } else { None };
    controls.clear();
    *sustain_release = None;
    if let Some(id) = release
        && controls.len() < controls.capacity()
    {
        controls.push((0, id, 0.0));
        *sustain_release = Some(id);
    }
}

/// Keep the visible parameter cache and deferred controller-state value in sync with the
/// actual event queue. A saturated queue must not claim a change the processor will never see.
fn queue_param_update(
    pending: &mut Vec<(u32, f64)>,
    values: &mut [f32],
    unsynced: &[AtomicU64],
    index: usize,
    id: u32,
    plain: f32,
    normalized: f64,
) -> bool {
    let (Some(slot), Some(u)) = (values.get_mut(index), unsynced.get(index)) else { return false };
    if let Some(e) = pending.iter_mut().find(|(pid, _)| *pid == id) {
        e.1 = normalized;
    } else if pending.len() < pending.capacity() {
        pending.push((id, normalized));
    } else {
        return false;
    }
    *slot = plain;
    u.store(normalized.to_bits(), Ordering::Relaxed);
    true
}

/// Run one native block, then consume only events the plugin accepted. A failed process may
/// have applied some writes, so retry only idempotent parameter values and note releases;
/// re-sending note-ons or pressure would risk duplicate or stuck voices. All queues stay bounded.
fn process_pending_chunk(
    pending: &mut Vec<(u32, f64)>,
    notes: &mut Vec<NoteEvent>,
    count: usize,
    native_process: impl FnOnce(&[(u32, f64)], &[NoteEvent]) -> Result<(), Vst3Error>,
) -> Result<(), Vst3Error> {
    let result = native_process(pending, notes.get(..count).unwrap_or(&[]));
    if result.is_ok() {
        pending.clear();
        notes.drain(..count.min(notes.len()));
    } else {
        keep_releases(notes);
    }
    result
}

impl Plugin for Vst3Plugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        let mb = max_block.clamp(1, MAX_BLOCK);
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.max_block = 0;
        match self.inst.configure(sr, mb, self.channels == 1) {
            Ok(layout) => {
                self.inputs = BusBuffers::new(&layout.inputs, mb);
                self.outputs = BusBuffers::new(&layout.outputs, mb);
                self.has_input = layout.inputs.first().is_some_and(|&c| c > 0);
                self.has_output = layout.outputs.first().is_some_and(|&c| c > 0);
                self.event_input = layout.event_input;
                self.max_block = mb;
                self.latency = self.inst.latency() as usize;
                self.tail = self.inst.tail() as usize;
                self.midi_map = self.inst.midi_mapping();
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency = 0;
            }
        }
        // Re-send every value so a re-activated plugin matches the session.
        self.pending.clear();
        for i in 0..self.ids.len().min(PARAM_QUEUE_CAP) {
            if let (Some(&id), Some(n)) = (self.ids.get(i), self.values.get(i).and_then(|&v| self.normalized_from_plain(i, v))) {
                self.pending.push((id, n));
            }
        }
    }

    fn reset(&mut self) {
        // Keep pending releases and release whatever is still held, so the processor's voices stop
        // on the next block instead of sounding on while the host forgets them.
        keep_releases(&mut self.notes);
        finish_mapped_controls(&mut self.controls, &mut self.sustain_release, true);
        self.all_notes_off();
        self.held = [false; 128];
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return false };
        let (Some(pi), Some(&vid)) = (self.info.params.get(i), self.ids.get(i)) else { return false };
        let v = pi.clamp(value);
        let Some(n) = self.normalized_from_plain(i, v) else { return false };
        let accepted = queue_param_update(&mut self.pending, &mut self.values, self.unsynced.as_ref(), i, vid, v, n);
        if accepted {
            supersede_sustain_release(&mut self.sustain_release, vid);
        }
        accepted
    }

    fn param(&self, id: &str) -> Option<f32> {
        let i = self.info.params.iter().position(|p| p.id == id)?;
        self.values.get(i).copied()
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        if !self.inst.is_active() || self.max_block == 0 {
            return;
        }
        let ch = self.channels.min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        if frames == 0 || ch == 0 {
            return;
        }
        if self.inst.host().latency_changed.swap(false, Ordering::Relaxed) {
            self.latency = self.inst.latency() as usize;
        }
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.notes {
            if e.time() > last {
                e.set_time(last);
            }
        }
        for c in &mut self.controls {
            c.0 = c.0.min(last);
        }
        let mut start = 0usize;
        let mut failed = false;
        while start < frames {
            let n = (frames - start).min(self.max_block);
            let end = u32::try_from(start + n).unwrap_or(u32::MAX);
            // Feed the main input bus (mono sources fill every bus channel).
            if self.has_input {
                for c in 0..self.inputs.channels(0) {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, self.inputs.channel_mut(0, c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let k = self.notes.iter().position(|e| e.time() >= end).unwrap_or(self.notes.len());
            let offset = u32::try_from(start).unwrap_or(u32::MAX);
            let r = process_pending_chunk(&mut self.pending, &mut self.notes, k, |pending, notes| {
                self.inst.process(
                    n,
                    &mut self.inputs,
                    &mut self.outputs,
                    pending,
                    &self.controls,
                    notes,
                    offset,
                    self.position,
                    self.transport.as_ref(),
                )
            });
            let chunk_failed = r.is_err();
            self.position = self.position.saturating_add(i64::try_from(n).unwrap_or(0));
            match r {
                Ok(()) if self.has_output => {
                    let oc = self.outputs.channels(0);
                    for (c, dst) in io.iter_mut().take(ch).enumerate() {
                        let Some(dst) = dst.get_mut(start..start + n) else { continue };
                        if ch == 1 && oc >= 2 {
                            if let (Some(l), Some(r)) = (self.outputs.channel(0, 0), self.outputs.channel(0, 1)) {
                                for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                    *d = 0.5 * (a + b);
                                }
                            }
                        } else if let Some(src) = self.outputs.channel(0, c.min(oc.saturating_sub(1))).and_then(|s| s.get(..n)) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
                Err(e) => log::debug!("{}: {e}", self.info.id),
                _ => {}
            }
            start += n;
            if chunk_failed {
                // Do not try another sub-block after native failure. The next call retries only
                // coalesced parameter writes, note-offs and accepted sustain-pedal releases at time zero.
                failed = true;
                break;
            }
        }
        finish_mapped_controls(&mut self.controls, &mut self.sustain_release, failed);
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.transport = Some(*transport);
    }

    fn tail_samples(&self) -> usize {
        self.tail
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if note > 127 || !self.event_input {
            return;
        }
        if velocity == 0 {
            self.note_off(offset, note);
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let queued = self.push_note(NoteEvent::On { time, key: note, velocity: f32::from(velocity.min(127)) / 127.0 });
        if queued && let Some(h) = self.held.get_mut(note as usize) {
            *h = true;
        }
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        if note > 127 || !self.event_input {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let queued = self.push_note(NoteEvent::Off { time, key: note });
        if queued && let Some(h) = self.held.get_mut(note as usize) {
            *h = false;
        }
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let [_, d1, d2] = msg.map(|b| b & 0x7F);
        let (ctrl, value) = match msg[0] & 0xF0 {
            0xB0 => (usize::from(d1), f64::from(d2) / 127.0),
            0xD0 => (AFTERTOUCH, f64::from(d1) / 127.0),
            0xE0 => (PITCH_BEND, f64::from(u16::from(d2) << 7 | u16::from(d1)) / 16383.0),
            0xA0 if self.event_input => {
                self.push_note(NoteEvent::Pressure { time, key: d1, pressure: f32::from(d2) / 127.0 });
                return;
            }
            _ => return,
        };
        if let Some(&Some(id)) = self.midi_map.get(ctrl) {
            queue_mapped_control(&mut self.controls, &mut self.sustain_release, (time, id, value), ctrl == 64 && d2 == 0);
        }
    }

    fn all_notes_off(&mut self) {
        cancel_pending_starts(&mut self.notes);
        for k in 0..128u8 {
            if self.held.get(k as usize).copied().unwrap_or(false) {
                self.note_off(0, k);
            }
        }
    }

    fn load_state(&mut self, data: &[u8]) -> bool {
        let ok = self.inst.load_state(data);
        if ok {
            self.refresh_values();
        }
        ok
    }

    fn open_editor(&mut self) -> Result<(), String> {
        self.inst.editor().ok_or_else(|| "this plugin has no editor".to_string())?.open(self.info.name)
    }

    fn close_editor(&mut self) {
        if let Some(e) = self.inst.editor() {
            e.close();
        }
    }

    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        let link = self.inst.editor()?;
        let mut units: HashMap<u32, String> = self.inst.params().into_iter().map(|p| (p.id, p.units)).collect();
        Some(Box::new(Vst3Editor {
            link,
            host: self.inst.host_ref(),
            info: self.info,
            index: self.info.params.iter().enumerate().map(|(i, p)| (p.id, i)).collect(),
            units: self.ids.iter().map(|id| units.remove(id).unwrap_or_default()).collect(),
            ids: self.ids.clone(),
            maps: self.maps.clone(),
        }))
    }

    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let (link, ids, unsynced) = (self.inst.state_link(), self.ids.clone(), Arc::clone(&self.unsynced));
        let plugin_id = self.info.id.to_owned();
        Some(Box::new(move || {
            // A parameter only the controller stores (no processor state) would otherwise be
            // saved with the controller's stale value. Track the values consumed for this attempt:
            // a plugin that refuses to save must not erase the next save's chance to retry.
            let mut consumed = Vec::new();
            for (index, (&id, u)) in ids.iter().zip(unsynced.iter()).enumerate() {
                let bits = u.swap(SYNCED, Ordering::Relaxed);
                if bits != SYNCED {
                    consumed.push((index, bits));
                    link.sync_param(id, f64::from_bits(bits));
                }
            }
            let saved = link.save_state();
            if saved.is_none() {
                restore_pending_after_failed_save(&unsynced, &consumed);
                log::warn!("VST3 plugin {plugin_id}: state save failed; pending parameter changes retained for retry");
            }
            saved
        }))
    }
}

/// A VST3 plugin's editor (`IPlugView` in a host window), driven from the main thread.
struct Vst3Editor {
    link: Arc<EditorLink>,
    host: ComWrapper<HostContext>,
    info: &'static PluginInfo,
    /// Position of each SoundCraft param id in `info.params` (looked up once per row and frame).
    index: HashMap<&'static str, usize>,
    /// The plugin's own unit label of each param (same order as `ids`).
    units: Vec<String>,
    ids: Vec<u32>,
    maps: Vec<Mapping>,
}

impl Vst3Editor {
    fn plain(&self, i: usize, n: f64) -> Option<f32> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let v = match self.maps.get(i)? {
            Mapping::Stepped(steps) => (n * steps).round(),
            Mapping::Plain => self.link.to_plain(id, n)?,
            Mapping::Linear => f64::from(pi.min) + n * f64::from(pi.max - pi.min),
        };
        v.is_finite().then(|| pi.clamp(v as f32))
    }

    fn normalized(&self, i: usize, v: f32) -> Option<f64> {
        let pi = self.info.params.get(i)?;
        let id = *self.ids.get(i)?;
        let span = f64::from(pi.max - pi.min);
        let linear = if span > 0.0 { (f64::from(v) - f64::from(pi.min)) / span } else { 0.0 };
        let n = match self.maps.get(i)? {
            Mapping::Stepped(steps) if *steps > 0.0 => f64::from(v) / steps,
            Mapping::Stepped(_) => 0.0,
            Mapping::Plain => self.link.to_normalized(id, f64::from(v)).unwrap_or(linear),
            Mapping::Linear => linear,
        };
        n.is_finite().then(|| n.clamp(0.0, 1.0))
    }
}

impl PluginEditor for Vst3Editor {
    fn open(&mut self) -> Result<(), String> {
        // Edits reported before the editor opened (e.g. during setup) are stale.
        let _ = self.host.take_edits();
        self.link.open(self.info.name)
    }

    fn close(&mut self) {
        self.link.close();
    }

    fn is_open(&self) -> bool {
        self.link.is_open()
    }

    fn idle(&mut self) -> Vec<(String, f32)> {
        self.link.idle();
        let mut out = Vec::new();
        for (vid, n) in self.host.take_edits() {
            let Some(i) = self.ids.iter().position(|&x| x == vid) else { continue };
            if let (Some(p), Some(v)) = (self.info.params.get(i), self.plain(i, n)) {
                out.push((p.id.to_string(), v));
            }
        }
        out
    }

    fn set_param(&mut self, id: &str, value: f32) {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return };
        if let (Some(&vid), Some(n)) = (self.ids.get(i), self.normalized(i, value)) {
            self.link.set_normalized(vid, n);
        }
    }

    fn value_text(&mut self, id: &str, value: f32) -> Option<String> {
        let &i = self.index.get(id)?;
        let pi = self.info.params.get(i)?;
        // A plugin whose plain mapping is the identity (JUCE, the SDK's default `Parameter`) leaves
        // a 0..1 range with the units of its real one: only its own text says what the value is.
        if !matches!(self.maps.get(i)?, Mapping::Plain) || pi.min != 0.0 || pi.max != 1.0 {
            return None;
        }
        let text = self.link.value_text(*self.ids.get(i)?, self.normalized(i, value)?)?;
        // VST3 keeps the units apart from the text (JUCE's label, the SDK's `RangeParameter`),
        // but some plugins put them in both.
        let units = self.units.get(i).map_or("", |u| u.trim());
        if units.is_empty() || text.trim_end().to_lowercase().ends_with(&units.to_lowercase()) { Some(text) } else { Some(format!("{text} {units}")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_native_chunk_retains_bounded_idempotent_events_for_retry() {
        let mut pending = Vec::with_capacity(2);
        pending.extend([(7, 0.75), (9, 0.25)]);
        let mut notes = Vec::with_capacity(6);
        notes.extend([
            NoteEvent::On { time: 1, key: 60, velocity: 1.0 },
            NoteEvent::Off { time: 2, key: 60 },
            NoteEvent::Pressure { time: 3, key: 61, pressure: 0.5 },
            NoteEvent::Off { time: 4, key: 60 },
            NoteEvent::Off { time: 5, key: 61 },
        ]);
        let (param_capacity, note_capacity) = (pending.capacity(), notes.capacity());
        let mut native_calls = 0;
        let failed = process_pending_chunk(&mut pending, &mut notes, 3, |params, events| {
            native_calls += 1;
            assert_eq!(params, &[(7, 0.75), (9, 0.25)]);
            assert_eq!(events.len(), 3);
            Err(Vst3Error::EventBufferBusy)
        });
        assert_eq!(failed, Err(Vst3Error::EventBufferBusy));
        assert_eq!(native_calls, 1);
        assert_eq!(pending.as_slice(), &[(7, 0.75), (9, 0.25)]);
        assert_eq!(notes.as_slice(), &[NoteEvent::Off { time: 0, key: 60 }, NoteEvent::Off { time: 0, key: 61 }]);
        assert_eq!((pending.capacity(), notes.capacity()), (param_capacity, note_capacity));

        let retried = process_pending_chunk(&mut pending, &mut notes, 2, |params, events| {
            native_calls += 1;
            assert_eq!(params, &[(7, 0.75), (9, 0.25)]);
            assert_eq!(events, &[NoteEvent::Off { time: 0, key: 60 }, NoteEvent::Off { time: 0, key: 61 }]);
            Ok(())
        });
        assert_eq!(retried, Ok(()));
        assert_eq!(native_calls, 2);
        assert!(pending.is_empty());
        assert!(notes.is_empty());
        assert_eq!((pending.capacity(), notes.capacity()), (param_capacity, note_capacity));
    }

    #[test]
    fn mapped_sustain_release_and_note_off_retry_after_native_error_then_clear_on_success() {
        let mut pending = vec![(7, 0.75)];
        let mut notes = Vec::with_capacity(4);
        notes.extend([NoteEvent::On { time: 1, key: 60, velocity: 0.8 }, NoteEvent::Off { time: 2, key: 60 }]);
        let mut controls = Vec::with_capacity(3);
        let mut sustain_release = None;
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (1, 12, 0.5), false));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (2, 64, 0.0), true));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (3, 17, 0.25), false));
        let capacity = controls.capacity();
        let failed = process_pending_chunk(&mut pending, &mut notes, 2, |params, events| {
            assert_eq!(params, &[(7, 0.75)]);
            assert_eq!(events.len(), 2);
            assert_eq!(controls.as_slice(), &[(1, 12, 0.5), (2, 64, 0.0), (3, 17, 0.25)]);
            Err(Vst3Error::EventBufferBusy)
        });
        finish_mapped_controls(&mut controls, &mut sustain_release, failed.is_err());
        assert_eq!(failed, Err(Vst3Error::EventBufferBusy));
        assert_eq!(pending.as_slice(), &[(7, 0.75)]);
        assert_eq!(notes.as_slice(), &[NoteEvent::Off { time: 0, key: 60 }]);
        assert_eq!(controls.as_slice(), &[(0, 64, 0.0)], "only a proven pedal-up may be retried");
        assert_eq!(sustain_release, Some(64));
        assert_eq!(controls.capacity(), capacity, "native failure must not allocate");

        let retry = process_pending_chunk(&mut pending, &mut notes, 1, |params, events| {
            assert_eq!(params, &[(7, 0.75)]);
            assert_eq!(events, &[NoteEvent::Off { time: 0, key: 60 }]);
            assert_eq!(controls.as_slice(), &[(0, 64, 0.0)]);
            Ok(())
        });
        finish_mapped_controls(&mut controls, &mut sustain_release, retry.is_err());
        assert_eq!(retry, Ok(()));
        assert!(pending.is_empty());
        assert!(notes.is_empty());
        assert!(controls.is_empty());
        assert_eq!(sustain_release, None, "successful native processing consumes the release marker");
        assert_eq!(controls.capacity(), capacity);
    }

    #[test]
    fn reset_retains_only_distinct_note_offs_and_the_last_mapped_sustain_up() {
        let mut notes =
            vec![NoteEvent::On { time: 1, key: 60, velocity: 0.8 }, NoteEvent::Off { time: 2, key: 60 }, NoteEvent::Off { time: 3, key: 60 }];
        let mut controls = Vec::with_capacity(4);
        let mut sustain_release = None;
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (1, 3, 0.9), false));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (2, 72, 0.0), true));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (3, 11, 0.0), false));
        keep_releases(&mut notes);
        finish_mapped_controls(&mut controls, &mut sustain_release, true);
        assert_eq!(notes, vec![NoteEvent::Off { time: 0, key: 60 }]);
        assert_eq!(controls.as_slice(), &[(0, 72, 0.0)]);
        assert_eq!(sustain_release, Some(72));
        finish_mapped_controls(&mut controls, &mut sustain_release, true);
        assert_eq!(controls.as_slice(), &[(0, 72, 0.0)], "repeated reset must not duplicate a release");
        assert_eq!(controls.capacity(), 4);
    }

    #[test]
    fn mapped_sustain_provenance_rejects_aliased_zero_cc_and_newer_param_write() {
        let mut controls = Vec::with_capacity(3);
        let mut sustain_release = None;
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (1, 70, 0.0), true));
        assert_eq!(sustain_release, Some(70));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (2, 70, 0.0), false));
        assert_eq!(sustain_release, None, "CC7=0 mapped to the same ParamID is not CC64-off");
        finish_mapped_controls(&mut controls, &mut sustain_release, true);
        assert!(controls.is_empty(), "an aliased zero controller cannot be mistaken for sustain release");

        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (3, 70, 0.0), true));
        let mut pending = Vec::with_capacity(1);
        let mut values = [0.0f32];
        let unsynced = [AtomicU64::new(SYNCED)];
        assert!(queue_param_update(&mut pending, &mut values, &unsynced, 0, 70, 0.5, 0.5));
        supersede_sustain_release(&mut sustain_release, 70);
        finish_mapped_controls(&mut controls, &mut sustain_release, true);
        assert!(controls.is_empty(), "a newer accepted UI parameter write must not replay stale pedal-up");
        assert_eq!(pending.as_slice(), &[(70, 0.5)]);
    }

    #[test]
    fn saturated_mapped_controls_prioritize_sustain_off_without_allocating() {
        let mut controls = Vec::with_capacity(1);
        let mut sustain_release = None;
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (1, 7, 0.8), false));
        assert!(!queue_mapped_control(&mut controls, &mut sustain_release, (2, 8, 0.5), false));
        assert_eq!(controls.as_slice(), &[(1, 7, 0.8)]);
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (3, 64, 0.0), true));
        assert_eq!(controls.as_slice(), &[(3, 64, 0.0)]);
        assert_eq!(sustain_release, Some(64));
        assert!(!queue_mapped_control(&mut controls, &mut sustain_release, (4, 64, 1.0), false));
        assert_eq!(sustain_release, Some(64), "a rejected edit cannot supersede a queued release");
        finish_mapped_controls(&mut controls, &mut sustain_release, true);
        assert_eq!(controls.as_slice(), &[(0, 64, 0.0)]);
        assert_eq!(controls.capacity(), 1);
    }

    #[test]
    fn two_successful_subblocks_keep_later_mapped_controls_until_outer_completion() {
        let mut pending = vec![(7, 0.75)];
        let mut notes = Vec::with_capacity(2);
        let mut controls = Vec::with_capacity(2);
        let mut sustain_release = None;
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (1, 8, 0.5), false));
        assert!(queue_mapped_control(&mut controls, &mut sustain_release, (12, 64, 0.0), true));
        let first = process_pending_chunk(&mut pending, &mut notes, 0, |_params, _events| Ok(()));
        assert_eq!(first, Ok(()));
        assert_eq!(controls.as_slice(), &[(1, 8, 0.5), (12, 64, 0.0)], "first chunk must not drain a future controller");
        let second = process_pending_chunk(&mut pending, &mut notes, 0, |_params, _events| Ok(()));
        assert_eq!(second, Ok(()));
        finish_mapped_controls(&mut controls, &mut sustain_release, false);
        assert!(controls.is_empty());
        assert_eq!(sustain_release, None);
    }

    #[test]
    fn successful_native_chunk_consumes_only_the_dispatched_notes() {
        let mut pending = vec![(7, 0.75)];
        let mut notes = vec![NoteEvent::On { time: 1, key: 60, velocity: 1.0 }, NoteEvent::Off { time: 10, key: 60 }];
        assert_eq!(
            process_pending_chunk(&mut pending, &mut notes, 1, |params, events| {
                assert_eq!(params, &[(7, 0.75)]);
                assert_eq!(events, &[NoteEvent::On { time: 1, key: 60, velocity: 1.0 }]);
                Ok(())
            }),
            Ok(())
        );
        assert!(pending.is_empty());
        assert_eq!(notes, vec![NoteEvent::Off { time: 10, key: 60 }]);
    }

    #[test]
    fn saturated_param_queue_preserves_display_and_unsynced_state_until_retry() {
        let mut pending = Vec::with_capacity(1);
        let mut values = [1.0f32, 200.0];
        let unsynced = [AtomicU64::new(SYNCED), AtomicU64::new(SYNCED)];

        assert!(queue_param_update(&mut pending, &mut values, &unsynced, 0, 7, 1.5, 0.75));
        assert_eq!(pending.as_slice(), &[(7, 0.75)]);
        assert_eq!(unsynced[0].load(Ordering::Relaxed), 0.75f64.to_bits());

        assert!(!queue_param_update(&mut pending, &mut values, &unsynced, 1, 12, 300.0, 0.9));
        assert_eq!(values, [1.5, 200.0], "rejected edit must not alter visible parameters");
        assert_eq!(unsynced[1].load(Ordering::Relaxed), SYNCED, "rejected edit must not alter state-save bookkeeping");
        assert_eq!(pending.as_slice(), &[(7, 0.75)]);

        assert!(queue_param_update(&mut pending, &mut values, &unsynced, 0, 7, 2.0, 1.0));
        assert_eq!(pending.as_slice(), &[(7, 1.0)], "coalesce even at capacity");
        assert_eq!(values[0], 2.0);
        assert_eq!(unsynced[0].load(Ordering::Relaxed), 1.0f64.to_bits());

        pending.clear();
        assert!(queue_param_update(&mut pending, &mut values, &unsynced, 1, 12, 300.0, 0.9), "retry succeeds after drain");
        assert_eq!(values[1], 300.0);
        assert_eq!(unsynced[1].load(Ordering::Relaxed), 0.9f64.to_bits());
        assert_eq!(pending.as_slice(), &[(12, 0.9)]);
        assert_eq!(pending.capacity(), 1, "no real-time allocation");
    }

    #[test]
    fn failed_state_save_restores_pending_values_but_preserves_newer_edits() {
        let old_a = 0.25f64.to_bits();
        let old_b = 0.5f64.to_bits();
        let newer_b = 0.75f64.to_bits();
        let slots = [AtomicU64::new(old_a), AtomicU64::new(old_b)];
        let a = slots[0].swap(SYNCED, Ordering::Relaxed);
        let b = slots[1].swap(SYNCED, Ordering::Relaxed);
        assert_eq!((a, b), (old_a, old_b));

        // Another thread changes the second parameter while state saving is in progress.
        slots[1].store(newer_b, Ordering::Relaxed);
        restore_pending_after_failed_save(&slots, &[(0, a), (1, b)]);
        assert_eq!(slots[0].load(Ordering::Relaxed), old_a, "failed save may retry the original value");
        assert_eq!(slots[1].load(Ordering::Relaxed), newer_b, "never overwrite a newer edit");

        // A successful subsequent save consumes the retried value normally.
        assert_eq!(slots[0].swap(SYNCED, Ordering::Relaxed), old_a);
        assert_eq!(slots[0].load(Ordering::Relaxed), SYNCED);
    }

    /// A saturated host queue must prioritize release events without allocating.
    #[test]
    fn releases_replace_note_ons_and_keep_time_order() {
        let mut notes = Vec::with_capacity(4);
        for i in 0..4u8 {
            assert!(push_sorted(&mut notes, NoteEvent::On { time: u32::from(i), key: i, velocity: 1.0 }));
        }
        assert!(!push_sorted(&mut notes, NoteEvent::On { time: 4, key: 99, velocity: 1.0 }));
        assert!(push_release(&mut notes, NoteEvent::Off { time: 1, key: 42 }));
        assert_eq!(notes.len(), 4);
        assert_eq!(notes.capacity(), 4);
        assert!(notes.iter().any(|n| matches!(n, NoteEvent::Off { key: 42, .. })));
        assert!(notes.windows(2).all(|w| w[0].time() <= w[1].time()));
    }

    #[test]
    fn transport_stop_discards_future_note_ons() {
        let mut notes = vec![
            NoteEvent::On { time: 10, key: 60, velocity: 0.5 },
            NoteEvent::Pressure { time: 15, key: 60, pressure: 0.5 },
            NoteEvent::Off { time: 20, key: 60 },
        ];
        cancel_pending_starts(&mut notes);
        assert_eq!(notes.len(), 2);
        assert!(!notes.iter().any(|e| matches!(e, NoteEvent::On { .. })));
        assert!(notes.iter().any(|e| matches!(e, NoteEvent::Off { key: 60, .. })));
    }

    #[test]
    fn only_duplicate_releases_are_evicted_when_the_queue_is_all_releases() {
        let mut notes = Vec::with_capacity(4);
        for time in 0..4 {
            assert!(push_sorted(&mut notes, NoteEvent::Off { time, key: 60 }));
        }
        assert!(push_release(&mut notes, NoteEvent::Off { time: 4, key: 61 }));
        assert!(notes.iter().any(|n| matches!(n, NoteEvent::Off { key: 60, .. })));
        assert!(notes.iter().any(|n| matches!(n, NoteEvent::Off { key: 61, .. })));

        let mut unique = Vec::with_capacity(2);
        assert!(push_sorted(&mut unique, NoteEvent::Off { time: 0, key: 1 }));
        assert!(push_sorted(&mut unique, NoteEvent::Off { time: 0, key: 2 }));
        assert!(!push_release(&mut unique, NoteEvent::Off { time: 0, key: 3 }));
        assert_eq!(unique.len(), 2);
    }

    /// A queue full of releases still has room to release every held key after a reset.
    #[test]
    fn reset_keeps_room_to_release_every_held_key() {
        let mut notes = Vec::with_capacity(EVENT_CAP);
        for i in 0..EVENT_CAP {
            push_sorted(&mut notes, NoteEvent::Off { time: i as u32, key: (i % 4) as u8 });
        }
        keep_releases(&mut notes);
        for key in 0..128u8 {
            push_sorted(&mut notes, NoteEvent::Off { time: 0, key });
        }
        for key in 0..128u8 {
            assert!(notes.iter().any(|e| matches!(*e, NoteEvent::Off { key: k, .. } if k == key)), "key {key} was never released");
        }
    }
}
