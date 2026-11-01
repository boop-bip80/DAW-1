//! `ClapPlugin`: a loaded CLAP instance behind the safe `soundcraft_dsp::Plugin` trait.

use crate::ClapError;
use crate::ffi::{EVENT_CAP, Event, GuiLink, HostState, Instance, NotePort, PortBuffers};
use clap_sys::ext::note_ports::{CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI};
use soundcraft_dsp::{Plugin, PluginEditor, PluginInfo, PluginRestartReader, Transport};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest block we activate a plugin for.
const MAX_BLOCK: usize = 1 << 16;
/// Most channels a host strip may ask for.
const MAX_CHANNELS: usize = 64;

/// A different token for every live CLAP instance. Exhaustion fails closed (token 0 is invalid).
static NEXT_RESTART_TOKEN: AtomicU64 = AtomicU64::new(1);

fn mint_restart_token(counter: &AtomicU64) -> u64 {
    // compare_exchange_weak is available at our Rust 1.90 MSRV; try_update is only 1.95+.
    // Minting occurs on the control thread, never while rendering audio.
    let mut next = counter.load(Ordering::Relaxed);
    loop {
        let Some(after) = next.checked_add(1) else { return 0 };
        match counter.compare_exchange_weak(next, after, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => next = actual,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NoteMode {
    None,
    Clap,
    Midi,
}

/// A read-only restart request from one CLAP instance. It owns only host-side atomic state,
/// no plugin pointer; observation remains safe after the native instance is destroyed.
///
/// This stage exposes pending `clap_host.request_restart` notifications. SoundCraft does not
/// yet automatically stop, reactivate or replace the plugin in response.
#[derive(Clone)]
pub struct ClapRestartHandle {
    state: Arc<HostState>,
    token: u64,
}

impl ClapRestartHandle {
    /// Returns the coalesced request without consuming it or invoking native code. Activation
    /// temporarily takes the bit, restores it on failure, and preserves newer callbacks.
    pub fn requested(&self) -> bool {
        self.state.restart.load(Ordering::Acquire)
    }
}

impl PluginRestartReader for ClapRestartHandle {
    fn requested(&self) -> bool {
        ClapRestartHandle::requested(self)
    }

    fn instance_token(&self) -> Option<u64> {
        (self.token != 0).then_some(self.token)
    }
}

/// A hosted CLAP plugin. Parameter ids are the CLAP param ids as decimal strings ("0", "17", …);
/// their human names are in `info().params`.
pub struct ClapPlugin {
    inst: Instance,
    info: &'static PluginInfo,
    restart_token: u64,
    /// CLAP id of each `info.params` entry (same order).
    ids: Vec<u32>,
    values: Vec<f32>,
    /// Pending events, kept sorted by time (stable for equal times). Never grows past `EVENT_CAP`.
    queue: Vec<Event>,
    /// Inert per-instance evidence probe. No MixEngine/monitor callback reads this yet.
    release_probe: Option<ReleaseProbe>,
    next_release_serial: u64,
    inputs: PortBuffers,
    outputs: PortBuffers,
    in_main: Option<usize>,
    out_main: Option<usize>,
    notes: NoteMode,
    /// The note port takes raw MIDI (controllers, pitch bend, program changes).
    midi_in: bool,
    held: [bool; 128],
    channels: usize,
    max_block: usize,
    latency: usize,
    steady: i64,
    transport: Option<Transport>,
}

impl ClapPlugin {
    pub(crate) fn new(inst: Instance, info: &'static PluginInfo) -> ClapPlugin {
        let ids: Vec<u32> = info.params.iter().map(|p| p.id.parse::<u32>().unwrap_or(u32::MAX)).collect();
        let values = info.params.iter().zip(&ids).map(|(p, id)| inst.param_value(*id).map_or(p.default, |v| p.clamp(v as f32))).collect();
        ClapPlugin {
            inst,
            info,
            restart_token: mint_restart_token(&NEXT_RESTART_TOKEN),
            ids,
            values,
            queue: Vec::with_capacity(EVENT_CAP),
            release_probe: None,
            next_release_serial: 0,
            inputs: PortBuffers::new(&[], 0),
            outputs: PortBuffers::new(&[], 0),
            in_main: None,
            out_main: None,
            notes: NoteMode::None,
            midi_in: false,
            held: [false; 128],
            channels: 2,
            max_block: 0,
            latency: 0,
            steady: 0,
            transport: None,
        }
    }

    /// Observe native restart requests independently of optional state or GUI extensions.
    /// Automatic reactivation is not wired into the mixer yet.
    pub fn restart_handle(&self) -> ClapRestartHandle {
        ClapRestartHandle { state: self.inst.restart_state(), token: self.restart_token }
    }

    /// Re-reads every parameter value from the plugin (after a state load).
    fn refresh_values(&mut self) {
        for ((v, p), id) in self.values.iter_mut().zip(self.info.params).zip(&self.ids) {
            if let Some(x) = self.inst.param_value(*id) {
                *v = p.clamp(x as f32);
            }
        }
        // Pending changes from before the load would undo it.
        self.queue.retain(|e| !matches!(e, Event::Param { .. }));
    }

    /// Inserts a normal event without allocating. An overflowing note-on is not recorded as
    /// held; a dropped note-off must remain held until it can be delivered.
    fn push(&mut self, e: Event) -> bool {
        push_sorted(&mut self.queue, e, false)
    }

    /// Give releases priority over new notes, controllers and parameter changes when the
    /// bounded queue is saturated. No allocation occurs on the audio thread.
    fn push_release(&mut self, e: Event) -> bool {
        push_sorted(&mut self.queue, e, true)
    }

    /// Inspect **already queued** NoteOff/CC64-off events and arm an inert evidence probe.
    /// This does not enqueue releases, open hardware, or acknowledge any monitor cleanup.
    /// Only a future host-aware mixer adapter could use the returned token, and it must
    /// request the full required release set before calling this method.
    pub fn arm_release_probe(&mut self, required: &[ReleaseRequirement]) -> Result<ReleaseProbeToken, ReleaseProbeError> {
        let probe = prepare_release_probe(&self.queue, self.restart_token, self.next_release_serial, required, self.release_probe.as_ref())?;
        self.next_release_serial = probe.token.serial;
        let token = probe.token;
        self.release_probe = Some(probe);
        Ok(token)
    }

    /// Read-only observation of a specific native instance and probe serial. A `Delivered`
    /// observation means the tagged events passed a successful native process callback;
    /// it does NOT guarantee that third-party synth voices have become silent.
    pub fn release_probe_evidence(&self, token: ReleaseProbeToken) -> Option<ReleaseEvidence> {
        self.release_probe.as_ref().filter(|probe| probe.token == token).map(|probe| probe.evidence)
    }

    fn block_pending_probe(&mut self, reason: ReleaseBlockReason) {
        if let Some(probe) = &mut self.release_probe {
            probe.block(reason);
        }
    }
}

/// An idempotent release has a separate identity for each channel/key, plus a
/// distinct sustain-pedal-up (CC64=0) identity for each MIDI channel. A zero-velocity
/// MIDI note-on is also a release. CLAP note events target channel zero.
const NOTE_RELEASE_IDS: usize = 16 * 128;
const RELEASE_ID_COUNT: usize = NOTE_RELEASE_IDS + 16;
const RELEASE_WORDS: usize = RELEASE_ID_COUNT.div_ceil(64);

/// One release identity within the native CLAP queue. Note events emitted by the Plugin
/// `note_off` interface use MIDI channel zero; raw MIDI note-offs retain their channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseRequirement {
    NoteOff { channel: u8, pitch: u8 },
    SustainOff { channel: u8 },
}

impl ReleaseRequirement {
    fn index(self) -> Option<usize> {
        match self {
            Self::NoteOff { channel, pitch } if channel < 16 && pitch < 128 => Some(usize::from(channel) * 128 + usize::from(pitch)),
            Self::SustainOff { channel } if channel < 16 => Some(NOTE_RELEASE_IDS + usize::from(channel)),
            _ => None,
        }
    }
}

/// Issued by one particular CLAP instance; an old serial can never witness new cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseProbeToken {
    instance: u64,
    serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseBlockReason {
    NativeError,
    Reset,
    Suspended,
    Reprepared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseEvidence {
    Pending,
    Delivered,
    Blocked(ReleaseBlockReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseProbeError {
    NoInstance,
    Busy,
    InvalidRequirements,
    DuplicateRequirement,
    MissingQueuedRelease,
    QueueSaturated,
    SerialExhausted,
}

/// A stack-allocated bitset: 2048 note-off identities + 16 pedal-up identities.
/// Each probe is bounded independently of the native event queue's fixed capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReleaseBits([u64; RELEASE_WORDS]);

impl ReleaseBits {
    const EMPTY: Self = Self([0; RELEASE_WORDS]);

    fn set(&mut self, index: usize) {
        if let Some(word) = self.0.get_mut(index / 64) {
            *word |= 1u64 << (index % 64);
        }
    }

    fn has(&self, index: usize) -> bool {
        self.0.get(index / 64).is_some_and(|word| word & (1u64 << (index % 64)) != 0)
    }

    fn merge(&mut self, other: Self) {
        for (dst, src) in self.0.iter_mut().zip(other.0) {
            *dst |= src;
        }
    }

    fn covers(&self, required: &Self) -> bool {
        self.0.iter().zip(required.0).all(|(observed, required)| *observed & required == required)
    }
}

struct ReleaseProbe {
    token: ReleaseProbeToken,
    required: ReleaseBits,
    accepted: ReleaseBits,
    provisional: ReleaseBits,
    evidence: ReleaseEvidence,
}

impl ReleaseProbe {
    fn block(&mut self, reason: ReleaseBlockReason) {
        if self.evidence == ReleaseEvidence::Pending {
            self.provisional = ReleaseBits::EMPTY;
            self.evidence = ReleaseEvidence::Blocked(reason);
        }
    }

    /// Called only after a successful native sub-block AND removal from the pending queue.
    fn accept_chunk(&mut self, releases: ReleaseBits) {
        if self.evidence == ReleaseEvidence::Pending {
            self.provisional.merge(releases);
        }
    }

    /// A later native error in the same outer process call must not promote provisional data.
    fn complete_successful_call(&mut self) {
        if self.evidence == ReleaseEvidence::Pending {
            self.accepted.merge(self.provisional);
            self.provisional = ReleaseBits::EMPTY;
            if self.accepted.covers(&self.required) {
                self.evidence = ReleaseEvidence::Delivered;
            }
        }
    }
}

fn prepare_release_probe(
    queue: &[Event],
    instance: u64,
    previous_serial: u64,
    required: &[ReleaseRequirement],
    current: Option<&ReleaseProbe>,
) -> Result<ReleaseProbe, ReleaseProbeError> {
    if instance == 0 {
        return Err(ReleaseProbeError::NoInstance);
    }
    if current.is_some_and(|probe| probe.evidence == ReleaseEvidence::Pending) {
        return Err(ReleaseProbeError::Busy);
    }
    if required.is_empty() {
        return Err(ReleaseProbeError::InvalidRequirements);
    }
    if required.len() > EVENT_CAP {
        return Err(ReleaseProbeError::QueueSaturated);
    }
    let mut bits = ReleaseBits::EMPTY;
    for &release in required {
        let index = release.index().ok_or(ReleaseProbeError::InvalidRequirements)?;
        if bits.has(index) {
            return Err(ReleaseProbeError::DuplicateRequirement);
        }
        if !queue.iter().any(|event| release_index(event) == Some(index)) {
            return Err(if queue.len() == EVENT_CAP { ReleaseProbeError::QueueSaturated } else { ReleaseProbeError::MissingQueuedRelease });
        }
        bits.set(index);
    }
    let serial = previous_serial.checked_add(1).ok_or(ReleaseProbeError::SerialExhausted)?;
    Ok(ReleaseProbe {
        token: ReleaseProbeToken { instance, serial },
        required: bits,
        accepted: ReleaseBits::EMPTY,
        provisional: ReleaseBits::EMPTY,
        evidence: ReleaseEvidence::Pending,
    })
}

/// The event references are inspected only after native Ok. Remove first, then publish the
/// provisional bitset, so merely enqueueing/inspecting does not count as delivery.
fn consume_successful_chunk(queue: &mut Vec<Event>, count: usize, probe: &mut Option<ReleaseProbe>) {
    let mut observed = ReleaseBits::EMPTY;
    if probe.as_ref().is_some_and(|probe| probe.evidence == ReleaseEvidence::Pending) {
        for event in queue.iter().take(count) {
            if let Some(index) = release_index(event) {
                observed.set(index);
            }
        }
    }
    queue.drain(..count.min(queue.len()));
    if let Some(probe) = probe {
        probe.accept_chunk(observed);
    }
}

fn release_index(e: &Event) -> Option<usize> {
    match *e {
        Event::NoteOff { key, .. } => Some(usize::from(key)),
        Event::Midi { data, .. } => {
            let status = data[0] & 0xF0;
            let channel = usize::from(data[0] & 0x0F);
            if status == 0x80 || (status == 0x90 && data[2] == 0) {
                Some(channel * 128 + usize::from(data[1] & 0x7F))
            } else if status == 0xB0 && data[1] == 64 && data[2] == 0 {
                Some(NOTE_RELEASE_IDS + channel)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A stop must cancel scheduled note starts. Otherwise a note-on later in this block could
/// immediately retrigger a voice after `all_notes_off` has already sent its release.
fn is_note_start(e: &Event) -> bool {
    match *e {
        Event::NoteOn { .. } => true,
        Event::Midi { data, .. } => data[0] & 0xF0 == 0x90 && data[2] != 0,
        _ => false,
    }
}

/// Stable time ordering, with releases favored when full. First discard a lower-priority
/// event; when the queue consists entirely of releases, only discard a duplicate release.
/// If every release is unique, report failure so held-note bookkeeping is retained.
fn push_sorted(queue: &mut Vec<Event>, e: Event, prioritize_release: bool) -> bool {
    if queue.len() >= queue.capacity() {
        if !prioritize_release {
            return false;
        }
        let eviction = queue.iter().rposition(|q| release_index(q).is_none()).or_else(|| {
            let mut seen = [false; RELEASE_ID_COUNT];
            queue.iter().position(|q| release_index(q).and_then(|i| seen.get_mut(i)).is_some_and(|seen| std::mem::replace(seen, true)))
        });
        let Some(i) = eviction else { return false };
        queue.remove(i);
    }
    let pos = queue.iter().rposition(|q| q.time() <= e.time()).map_or(0, |i| i + 1);
    queue.insert(pos, e);
    true
}

/// Accept a host parameter change only when it can reach the plugin. A saturated event queue
/// must not make the cached session value diverge from the plugin's actual parameter.
fn queue_param_update(queue: &mut Vec<Event>, values: &mut [f32], index: usize, id: u32, value: f32) -> bool {
    let Some(current) = values.get_mut(index) else { return false };
    // An existing change can always be coalesced, even when the queue is full.
    if let Some(Event::Param { value: pending, .. }) = queue.iter_mut().find(|e| matches!(e, Event::Param { id: queued, .. } if *queued == id)) {
        *pending = f64::from(value);
    } else if !push_sorted(queue, Event::Param { time: 0, id, value: f64::from(value) }, false) {
        return false;
    }
    *current = value;
    true
}

/// A process error may have consumed part of a block; a reset may have no native
/// callback. Retry only idempotent writes/releases at time zero, never note starts
/// or unrelated MIDI that could double-trigger.
fn retain_idempotent_for_retry(queue: &mut Vec<Event>) {
    queue.retain(|e| matches!(e, Event::Param { .. }) || release_index(e).is_some());
    for e in queue {
        e.set_time(0);
    }
}

/// A plugin without a native reset callback still needs queued note/pedal releases.
/// Cancel starts and arbitrary controllers, retain idempotent writes/releases at time 0,
/// and clear local held bookkeeping without allocating on the render thread.
fn retain_safe_events_on_reset(queue: &mut Vec<Event>, held: &mut [bool; 128]) {
    retain_idempotent_for_retry(queue);
    held.fill(false);
}

fn pick_main(ports: &[crate::ffi::PortInfo]) -> Option<usize> {
    ports.iter().position(|p| p.main && p.channels > 0).or_else(|| ports.iter().position(|p| p.channels > 0))
}

fn note_mode(p: Option<NotePort>) -> NoteMode {
    match p {
        Some(p) if p.supported & CLAP_NOTE_DIALECT_CLAP != 0 => NoteMode::Clap,
        Some(p) if p.supported & CLAP_NOTE_DIALECT_MIDI != 0 => NoteMode::Midi,
        _ => NoteMode::None,
    }
}

/// An old restart request is consumed only when reactivation works. Clear it before the native
/// activate call so a *new* request raised during activation remains observable afterwards.
fn activate_preserving_restart(state: &HostState, activate: impl FnOnce() -> Result<(), ClapError>) -> Result<(), ClapError> {
    let previous = state.restart.swap(false, Ordering::AcqRel);
    let result = activate();
    if result.is_err() && previous {
        // Restoring true never overwrites a newer native request.
        state.restart.store(true, Ordering::Release);
    }
    result
}

impl Plugin for ClapPlugin {
    fn info(&self) -> &'static PluginInfo {
        self.info
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize, channels: usize) {
        self.block_pending_probe(ReleaseBlockReason::Reprepared);
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { f64::from(sample_rate) } else { 48_000.0 };
        let mb = max_block.clamp(1, MAX_BLOCK);
        self.channels = channels.clamp(1, MAX_CHANNELS);
        self.inst.deactivate();
        self.inst.service_callback();
        let ports = self.inst.audio_ports(true).and_then(|ins| Ok((ins, self.inst.audio_ports(false)?)));
        let (ins, outs) = match ports {
            Ok(p) => p,
            Err(e) => {
                // Unsupported layout: stay inactive (`process` passes audio through untouched).
                log::warn!("{}: {e}", self.info.id);
                self.inputs = PortBuffers::new(&[], 0);
                self.outputs = PortBuffers::new(&[], 0);
                self.in_main = None;
                self.out_main = None;
                self.max_block = 0;
                self.latency = 0;
                return;
            }
        };
        self.in_main = pick_main(&ins);
        self.out_main = pick_main(&outs);
        self.inputs = PortBuffers::new(&ins.iter().map(|p| p.channels).collect::<Vec<_>>(), mb);
        self.outputs = PortBuffers::new(&outs.iter().map(|p| p.channels).collect::<Vec<_>>(), mb);
        let port = self.inst.note_port();
        self.notes = note_mode(port);
        self.midi_in = port.is_some_and(|p| p.supported & CLAP_NOTE_DIALECT_MIDI != 0);
        self.max_block = mb;
        let restart_state = self.inst.restart_state();
        match activate_preserving_restart(&restart_state, || self.inst.activate(sr, 1, u32::try_from(mb).unwrap_or(u32::MAX))) {
            Ok(()) => {
                self.latency = self.inst.latency() as usize;
                self.inst.state().latency_changed.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.id);
                self.latency = 0;
            }
        }
    }

    fn reset(&mut self) {
        self.block_pending_probe(ReleaseBlockReason::Reset);
        self.inst.reset();
        retain_safe_events_on_reset(&mut self.queue, &mut self.held);
    }

    fn suspend(&mut self) {
        self.block_pending_probe(ReleaseBlockReason::Suspended);
        // Called by the mixer before retirement, on the audio/render thread. Deactivation and
        // destruction remain deferred to the main thread that drops the retired instance.
        self.inst.stop_processing();
    }

    fn restart_instance_token(&self) -> Option<u64> {
        (self.restart_token != 0).then_some(self.restart_token)
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(i) = self.info.params.iter().position(|p| p.id == id) else { return false };
        let (Some(pi), Some(&cid)) = (self.info.params.get(i), self.ids.get(i)) else { return false };
        queue_param_update(&mut self.queue, &mut self.values, i, cid, pi.clamp(value))
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
        let last = u32::try_from(frames - 1).unwrap_or(u32::MAX);
        for e in &mut self.queue {
            if e.time() > last {
                e.set_time(last);
            }
        }
        let mut start = 0usize;
        while start < frames {
            let n = (frames - start).min(self.max_block);
            let end = u32::try_from(start + n).unwrap_or(u32::MAX);
            // Feed the main input port (mono sources fill every port channel).
            if let Some(p) = self.in_main {
                for c in 0..self.inputs.channels(p) {
                    let src = io.get(c.min(ch - 1)).and_then(|s| s.get(start..start + n));
                    if let (Some(src), Some(dst)) = (src, self.inputs.channel_mut(p, c).and_then(|d| d.get_mut(..n))) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let k = self.queue.iter().position(|e| e.time() >= end).unwrap_or(self.queue.len());
            let offset = u32::try_from(start).unwrap_or(u32::MAX);
            let events = self.queue.get(..k).unwrap_or(&[]);
            let r = self.inst.process(n, self.steady, &mut self.inputs, &mut self.outputs, events, offset, self.transport.as_ref());
            self.steady = self.steady.saturating_add(i64::try_from(n).unwrap_or(0));
            match (r, self.out_main) {
                (Ok(_), Some(p)) => {
                    consume_successful_chunk(&mut self.queue, k, &mut self.release_probe);
                    let oc = self.outputs.channels(p);
                    for (c, dst) in io.iter_mut().take(ch).enumerate() {
                        let Some(dst) = dst.get_mut(start..start + n) else { continue };
                        if ch == 1 && oc >= 2 {
                            if let (Some(l), Some(r)) = (self.outputs.channel(p, 0), self.outputs.channel(p, 1)) {
                                for ((d, a), b) in dst.iter_mut().zip(l).zip(r) {
                                    *d = 0.5 * (a + b);
                                }
                            }
                        } else if let Some(src) = self.outputs.channel(p, c.min(oc.saturating_sub(1))).and_then(|s| s.get(..n)) {
                            dst.copy_from_slice(src);
                        }
                    }
                }
                (Ok(_), None) => {
                    consume_successful_chunk(&mut self.queue, k, &mut self.release_probe);
                }
                (Err(e), _) => {
                    self.block_pending_probe(ReleaseBlockReason::NativeError);
                    retain_idempotent_for_retry(&mut self.queue);
                    log::debug!("{}: {e}", self.info.id);
                    return;
                }
            }
            start += n;
        }
        if let Some(probe) = &mut self.release_probe {
            probe.complete_successful_call();
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.transport = Some(*transport);
    }

    fn note_on(&mut self, offset: usize, note: u8, velocity: u8) {
        if note > 127 {
            return;
        }
        if velocity == 0 {
            self.note_off(offset, note);
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let vel = velocity.min(127);
        let queued = match self.notes {
            NoteMode::Clap => self.push(Event::NoteOn { time, key: note, velocity: f64::from(vel) / 127.0 }),
            NoteMode::Midi => self.push(Event::Midi { time, data: [0x90, note, vel] }),
            NoteMode::None => return,
        };
        if queued && let Some(h) = self.held.get_mut(note as usize) {
            *h = true;
        }
    }

    fn note_off(&mut self, offset: usize, note: u8) {
        if note > 127 {
            return;
        }
        let time = u32::try_from(offset).unwrap_or(u32::MAX);
        let queued = match self.notes {
            NoteMode::Clap => self.push_release(Event::NoteOff { time, key: note }),
            NoteMode::Midi => self.push_release(Event::Midi { time, data: [0x80, note, 0] }),
            NoteMode::None => return,
        };
        if queued && let Some(h) = self.held.get_mut(note as usize) {
            *h = false;
        }
    }

    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        if self.midi_in {
            let e = Event::Midi { time: u32::try_from(offset).unwrap_or(u32::MAX), data: msg };
            if release_index(&e).is_some() {
                self.push_release(e);
            } else {
                self.push(e);
            }
        }
    }

    fn all_notes_off(&mut self) {
        self.queue.retain(|e| !is_note_start(e));
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
        let link = self.inst.gui().ok_or_else(|| "this plugin has no editor".to_string())?;
        link.open(self.info.name)
    }

    fn close_editor(&mut self) {
        if let Some(link) = self.inst.gui() {
            link.close();
        }
    }

    fn editor(&mut self) -> Option<Box<dyn PluginEditor>> {
        let link = self.inst.gui()?;
        Some(Box::new(ClapEditor { link, info: self.info, ids: self.ids.clone() }))
    }

    fn state_reader(&mut self) -> Option<Box<dyn soundcraft_dsp::PluginStateReader>> {
        let link = self.inst.state_link()?;
        Some(Box::new(move || link.save_state()))
    }

    fn restart_reader(&mut self) -> Option<Box<dyn PluginRestartReader>> {
        Some(Box::new(self.restart_handle()))
    }
}

/// A CLAP plugin's floating editor, driven from the main thread (see [`GuiLink`]).
struct ClapEditor {
    link: Arc<GuiLink>,
    info: &'static PluginInfo,
    ids: Vec<u32>,
}

impl PluginEditor for ClapEditor {
    fn open(&mut self) -> Result<(), String> {
        // Edits reported before the editor opened are stale.
        let _ = self.link.idle();
        self.link.open(self.info.name)
    }

    fn close(&mut self) {
        self.link.close();
    }

    fn is_open(&self) -> bool {
        self.link.is_open()
    }

    fn idle(&mut self) -> Vec<(String, f32)> {
        let raw = self.link.idle();
        let mut out: Vec<(String, f32)> = Vec::new();
        for (cid, v) in raw {
            let Some(i) = self.ids.iter().position(|&x| x == cid) else { continue };
            let Some(p) = self.info.params.get(i) else { continue };
            let v = p.clamp(v as f32);
            // Keep only the latest value per parameter.
            match out.iter_mut().find(|(id, _)| id == p.id) {
                Some(e) => e.1 = v,
                None => out.push((p.id.to_string(), v)),
            }
        }
        out
    }

    fn state_changed(&mut self) -> bool {
        self.link.host().dirty.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_off(pitch: u8) -> Event {
        Event::NoteOff { time: 0, key: pitch }
    }

    fn pedal_up(channel: u8) -> Event {
        Event::Midi { time: 0, data: [0xB0 | channel, 64, 0] }
    }

    fn probe(queue: &[Event], serial: u64, required: &[ReleaseRequirement]) -> ReleaseProbe {
        prepare_release_probe(queue, 27, serial, required, None).unwrap()
    }

    #[test]
    fn native_release_evidence_requires_queue_drain_and_whole_successful_process_call() {
        let mut queue = Vec::with_capacity(4);
        queue.extend([note_off(60), pedal_up(2), Event::Param { time: 3, id: 7, value: 0.5 }]);
        let req = [ReleaseRequirement::NoteOff { channel: 0, pitch: 60 }, ReleaseRequirement::SustainOff { channel: 2 }];
        let mut active = Some(probe(&queue, 0, &req));
        let token = active.as_ref().unwrap().token;
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Pending);

        // A synthetic successful native sub-block *itself* is the precondition for consuming
        // the first release. The evidence remains provisional until the outer call completes.
        let native = |events: &[Event]| -> Result<(), ()> {
            assert_eq!(events, &[note_off(60)]);
            Ok(())
        };
        assert!(native(&queue[..1]).is_ok());
        consume_successful_chunk(&mut queue, 1, &mut active);
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Pending);
        assert_eq!(queue.len(), 2);

        let native = |events: &[Event]| -> Result<(), ()> {
            assert_eq!(events, &[pedal_up(2)]);
            Ok(())
        };
        assert!(native(&queue[..1]).is_ok());
        consume_successful_chunk(&mut queue, 1, &mut active);
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Pending, "both consumed releases are still provisional");
        active.as_mut().unwrap().complete_successful_call();
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Delivered);
        assert_eq!(active.as_ref().unwrap().token, token);
        assert_eq!(queue.len(), 1, "unrelated parameter event is not a cleanup release");
        assert_eq!(queue.capacity(), 4, "evidence update must not allocate");
    }

    #[test]
    fn later_native_failure_discards_prior_successful_chunk_evidence() {
        let mut queue = vec![note_off(61), pedal_up(3)];
        let mut active =
            Some(probe(&queue, 0, &[ReleaseRequirement::NoteOff { channel: 0, pitch: 61 }, ReleaseRequirement::SustainOff { channel: 3 }]));
        // First native chunk succeeded and was drained.
        consume_successful_chunk(&mut queue, 1, &mut active);
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Pending);
        // A later native chunk fails; never commit the earlier provisional delivery.
        let fake_native = |_events: &[Event]| -> Result<(), ()> { Err(()) };
        assert!(fake_native(&queue).is_err());
        active.as_mut().unwrap().block(ReleaseBlockReason::NativeError);
        retain_idempotent_for_retry(&mut queue);
        active.as_mut().unwrap().complete_successful_call();
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Blocked(ReleaseBlockReason::NativeError));
        assert_eq!(queue, vec![pedal_up(3)], "the retry queue retains only the undelivered idempotent release");
    }

    #[test]
    fn zero_frame_reset_and_suspend_cannot_produce_delivery_evidence() {
        let queue = vec![note_off(60), pedal_up(15)];
        let req = [ReleaseRequirement::NoteOff { channel: 0, pitch: 60 }, ReleaseRequirement::SustainOff { channel: 15 }];
        let mut pending = probe(&queue, 0, &req);
        // There was no native process call: zero frames are not accepted evidence.
        pending.complete_successful_call();
        assert_eq!(pending.evidence, ReleaseEvidence::Pending);
        pending.block(ReleaseBlockReason::Reset);
        pending.complete_successful_call();
        assert_eq!(pending.evidence, ReleaseEvidence::Blocked(ReleaseBlockReason::Reset));

        let mut parked = probe(&queue, 1, &req);
        parked.block(ReleaseBlockReason::Suspended);
        parked.accept_chunk(ReleaseBits::EMPTY);
        parked.complete_successful_call();
        assert_eq!(parked.evidence, ReleaseEvidence::Blocked(ReleaseBlockReason::Suspended));
        assert_ne!(pending.token, parked.token, "probe serial distinguishes rearm on the same native instance");
    }

    #[test]
    fn missing_duplicate_and_saturated_release_requirements_fail_closed() {
        let only_note = [note_off(60)];
        let note = ReleaseRequirement::NoteOff { channel: 0, pitch: 60 };
        let missing = ReleaseRequirement::SustainOff { channel: 0 };
        assert!(matches!(prepare_release_probe(&only_note, 0, 0, &[note], None), Err(ReleaseProbeError::NoInstance)));
        assert!(matches!(prepare_release_probe(&only_note, 27, 0, &[], None), Err(ReleaseProbeError::InvalidRequirements)));
        assert!(matches!(prepare_release_probe(&only_note, 27, 0, &[note, note], None), Err(ReleaseProbeError::DuplicateRequirement)));
        assert!(matches!(prepare_release_probe(&only_note, 27, 0, &[missing], None), Err(ReleaseProbeError::MissingQueuedRelease)));
        assert!(matches!(
            prepare_release_probe(&only_note, 27, 0, &[ReleaseRequirement::SustainOff { channel: 16 }], None),
            Err(ReleaseProbeError::InvalidRequirements)
        ));
        assert!(matches!(prepare_release_probe(&only_note, 27, u64::MAX, &[note], None), Err(ReleaseProbeError::SerialExhausted)));

        let mut saturated = Vec::with_capacity(EVENT_CAP);
        saturated.resize(EVENT_CAP, note_off(60));
        assert!(matches!(prepare_release_probe(&saturated, 27, 0, &[missing], None), Err(ReleaseProbeError::QueueSaturated)));
        assert_eq!(saturated.len(), saturated.capacity());
        let a = probe(&saturated, 0, &[note]);
        assert!(matches!(prepare_release_probe(&saturated, 27, 1, &[note], Some(&a)), Err(ReleaseProbeError::Busy)));
    }

    #[test]
    fn same_channel_key_or_pedal_bits_never_alias_other_release_identities() {
        let queue = vec![note_off(127), Event::Midi { time: 0, data: [0x8F, 127, 0] }, pedal_up(15)];
        let required = [
            ReleaseRequirement::NoteOff { channel: 0, pitch: 127 },
            ReleaseRequirement::NoteOff { channel: 15, pitch: 127 },
            ReleaseRequirement::SustainOff { channel: 15 },
        ];
        let mut active = Some(probe(&queue, 0, &required));
        let mut mut_queue = queue;
        consume_successful_chunk(&mut mut_queue, 3, &mut active);
        active.as_mut().unwrap().complete_successful_call();
        assert_eq!(active.as_ref().unwrap().evidence, ReleaseEvidence::Delivered);
        assert_ne!(required[0].index(), required[1].index());
        assert_ne!(required[1].index(), required[2].index());
    }

    #[test]
    fn restart_tokens_are_unique_and_fail_closed_at_exhaustion() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(mint_restart_token(&counter), u64::MAX - 1);
        assert_eq!(mint_restart_token(&counter), 0, "no token is reused after counter exhaustion");
        assert_eq!(mint_restart_token(&counter), 0);
    }

    #[test]
    fn restart_handle_is_observable_through_an_erased_control_reader() {
        let state = Arc::new(HostState::default());
        let handle = ClapRestartHandle { state: Arc::clone(&state), token: 17 };
        let reader: Box<dyn PluginRestartReader> = Box::new(handle);
        assert!(!reader.requested());
        assert_eq!(reader.instance_token(), Some(17));
        state.restart.store(true, Ordering::Release);
        assert!(reader.requested());
        drop(state);
        assert!(reader.requested(), "the handle contains host atomics but no native instance pointer");
    }

    #[test]
    fn restart_handle_survives_its_sender_and_activation_preserves_new_or_failed_requests() {
        let state = Arc::new(HostState::default());
        let handle = ClapRestartHandle { state: Arc::clone(&state), token: 17 };
        let observer = handle.clone();
        assert!(!handle.requested());
        state.restart.store(true, Ordering::Release);
        assert!(handle.requested());

        // Success acknowledges only requests that predate activation; a callback issued
        // by the fake native activate function remains pending.
        let activated = activate_preserving_restart(&state, || {
            assert!(!handle.requested());
            state.restart.store(true, Ordering::Release);
            Ok(())
        });
        assert!(activated.is_ok());
        assert!(handle.requested());

        // A failed activate cannot make an earlier request disappear.
        let failed = activate_preserving_restart(&state, || Err(ClapError::Process("synthetic activation failed".into())));
        assert!(failed.is_err());
        assert!(observer.requested());

        // A later successful activation with no new request acknowledges the pending one.
        let activated = activate_preserving_restart(&state, || Ok(()));
        assert!(activated.is_ok());
        assert!(!observer.requested());
        drop(state);
        drop(handle);
        assert!(!observer.requested(), "the Arc-backed signal remains readable after the native owner is gone");
    }

    #[test]
    fn saturated_queue_rejects_new_param_without_diverging_and_coalesces_pending_param() {
        let mut queue = Vec::with_capacity(2);
        let mut values = [1.0f32, 0.25];
        assert!(push_sorted(&mut queue, Event::Midi { time: 0, data: [0xB0, 7, 64] }, false));
        assert!(push_sorted(&mut queue, Event::NoteOn { time: 1, key: 60, velocity: 0.5 }, false));
        assert!(!queue_param_update(&mut queue, &mut values, 0, 7, 2.0), "full queue cannot deliver a new parameter event");
        assert_eq!(values, [1.0, 0.25], "failed update must leave cached values unchanged");
        assert_eq!(queue.len(), queue.capacity());

        queue.remove(0);
        assert!(queue_param_update(&mut queue, &mut values, 0, 7, 1.5), "retry works after capacity is freed");
        assert_eq!(values, [1.5, 0.25]);
        assert_eq!(queue.len(), queue.capacity());
        assert!(queue_param_update(&mut queue, &mut values, 0, 7, 0.5), "coalescing a queued parameter needs no extra slot");
        assert!(!queue_param_update(&mut queue, &mut values, 1, 3, 1.0), "an unrelated new parameter still cannot fit");
        assert_eq!(values, [0.5, 0.25]);
        assert_eq!(queue.iter().filter(|e| matches!(e, Event::Param { id: 7, value, .. } if *value == 0.5)).count(), 1);
        assert_eq!(queue.len(), queue.capacity(), "updates never grow the real-time queue");
    }

    #[test]
    fn process_error_retains_only_idempotent_events_for_next_block() {
        let mut queue = Vec::with_capacity(7);
        for e in [
            Event::Param { time: 1, id: 7, value: 0.75 },
            Event::NoteOn { time: 2, key: 60, velocity: 0.8 },
            Event::Midi { time: 3, data: [0xB0, 7, 100] },
            Event::NoteOff { time: 4, key: 60 },
            Event::Midi { time: 5, data: [0x80, 61, 0] },
            Event::Midi { time: 6, data: [0x90, 62, 0] },
            Event::Midi { time: 7, data: [0xB2, 64, 0] },
        ] {
            assert!(push_sorted(&mut queue, e, false));
        }
        retain_idempotent_for_retry(&mut queue);
        assert_eq!(
            queue,
            vec![
                Event::Param { time: 0, id: 7, value: 0.75 },
                Event::NoteOff { time: 0, key: 60 },
                Event::Midi { time: 0, data: [0x80, 61, 0] },
                Event::Midi { time: 0, data: [0x90, 62, 0] },
                Event::Midi { time: 0, data: [0xB2, 64, 0] },
            ],
            "parameter writes and note/pedal releases survive in stable order, never starts or arbitrary CC"
        );
        assert_eq!(queue.capacity(), 7, "recovery never allocates");
        retain_idempotent_for_retry(&mut queue);
        assert_eq!(queue.len(), 5, "another failure cannot duplicate retained events");
    }

    #[test]
    fn sustain_pedal_up_has_distinct_channel_identity_and_release_priority_when_full() {
        let up = Event::Midi { time: 2, data: [0xB2, 64, 0] };
        assert_eq!(release_index(&up), Some(NOTE_RELEASE_IDS + 2));
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0xB0, 64, 0] }), Some(NOTE_RELEASE_IDS));
        assert_ne!(release_index(&up), release_index(&Event::Midi { time: 0, data: [0x82, 64, 0] }));
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0xB2, 64, 127] }), None);
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0xB2, 64, 1] }), None);
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0xB2, 7, 0] }), None);

        let mut queue = Vec::with_capacity(3);
        for e in [
            Event::NoteOn { time: 1, key: 60, velocity: 0.5 },
            Event::Midi { time: 2, data: [0xB2, 64, 127] },
            Event::Param { time: 3, id: 7, value: 0.8 },
        ] {
            assert!(push_sorted(&mut queue, e, false));
        }
        assert!(push_sorted(&mut queue, up, true), "sustain-off must evict a lower-priority event when saturated");
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.capacity(), 3, "priority never allocates in the audio callback");
        assert!(queue.contains(&up));
        assert!(queue.windows(2).all(|w| w[0].time() <= w[1].time()), "priority preserves event time order");

        let mut releases = Vec::with_capacity(2);
        assert!(push_sorted(&mut releases, Event::NoteOff { time: 0, key: 64 }, false));
        assert!(push_sorted(&mut releases, Event::Midi { time: 0, data: [0xB0, 64, 0] }, false));
        assert!(!push_sorted(&mut releases, up, true), "distinct note/pedal releases must not evict each other");
        assert_eq!(releases.len(), 2);
    }

    #[test]
    fn reset_keeps_pending_note_and_pedal_releases_without_replaying_starts() {
        let mut queue = Vec::with_capacity(7);
        for e in [
            Event::Param { time: 1, id: 7, value: 0.25 },
            Event::NoteOn { time: 2, key: 60, velocity: 1.0 },
            Event::Midi { time: 3, data: [0x91, 61, 100] },
            Event::Midi { time: 4, data: [0xB2, 64, 127] },
            Event::NoteOff { time: 5, key: 60 },
            Event::Midi { time: 6, data: [0xB2, 64, 0] },
            Event::Midi { time: 7, data: [0xB1, 7, 30] },
        ] {
            assert!(push_sorted(&mut queue, e, false));
        }
        let mut held = [false; 128];
        held[60] = true;
        held[61] = true;
        retain_safe_events_on_reset(&mut queue, &mut held);
        assert_eq!(
            queue,
            vec![Event::Param { time: 0, id: 7, value: 0.25 }, Event::NoteOff { time: 0, key: 60 }, Event::Midi { time: 0, data: [0xB2, 64, 0] }],
            "reset must preserve idempotent releases for plugins without a native reset callback"
        );
        assert!(held.iter().all(|is_held| !*is_held), "reset cancels local held-note state");
        assert_eq!(queue.capacity(), 7, "reset recovery never allocates");
        retain_safe_events_on_reset(&mut queue, &mut held);
        assert_eq!(queue.len(), 3, "repeated reset must not duplicate releases");
    }

    #[test]
    fn queued_note_off_evicts_an_on_without_allocating_or_reordering() {
        let mut queue = Vec::with_capacity(4);
        for i in 0..4u8 {
            assert!(push_sorted(&mut queue, Event::NoteOn { time: u32::from(i), key: i, velocity: 1.0 }, false));
        }
        assert!(!push_sorted(&mut queue, Event::NoteOn { time: 5, key: 99, velocity: 1.0 }, false));
        assert!(push_sorted(&mut queue, Event::NoteOff { time: 1, key: 42 }, true));
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.capacity(), 4);
        assert!(queue.iter().any(|e| matches!(e, Event::NoteOff { key: 42, .. })));
        assert!(queue.windows(2).all(|w| w[0].time() <= w[1].time()));
    }

    #[test]
    fn full_release_queue_replaces_only_a_duplicate() {
        let mut queue = Vec::with_capacity(4);
        for i in 0..4 {
            assert!(push_sorted(&mut queue, Event::NoteOff { time: i, key: 60 }, false));
        }
        assert!(push_sorted(&mut queue, Event::NoteOff { time: 4, key: 61 }, true));
        assert_eq!(queue.len(), 4);
        assert!(queue.iter().any(|e| matches!(e, Event::NoteOff { key: 60, .. })));
        assert!(queue.iter().any(|e| matches!(e, Event::NoteOff { key: 61, .. })));
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0x91, 60, 0] }), Some(128 + 60));
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0x81, 60, 1] }), Some(128 + 60));
        assert_eq!(release_index(&Event::Midi { time: 0, data: [0x91, 60, 100] }), None);
    }

    #[test]
    fn stop_cancels_scheduled_notes_before_sending_releases() {
        let mut queue = vec![
            Event::NoteOn { time: 10, key: 60, velocity: 0.8 },
            Event::Midi { time: 20, data: [0x91, 61, 100] },
            Event::Midi { time: 25, data: [0x91, 61, 0] },
            Event::NoteOff { time: 30, key: 60 },
        ];
        queue.retain(|e| !is_note_start(e));
        assert_eq!(queue.len(), 2);
        assert!(queue.iter().all(|e| release_index(e).is_some()));
    }

    #[test]
    fn unique_releases_are_never_silently_discarded() {
        let mut queue = Vec::with_capacity(2);
        assert!(push_sorted(&mut queue, Event::NoteOff { time: 0, key: 1 }, false));
        assert!(push_sorted(&mut queue, Event::NoteOff { time: 0, key: 2 }, false));
        assert!(!push_sorted(&mut queue, Event::NoteOff { time: 0, key: 3 }, true));
        assert_eq!(queue.len(), 2);
    }
}
