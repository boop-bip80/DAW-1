//! The FFI boundary: the only module in SoundCraft allowed to use `unsafe`.
//!
//! It loads CLAP binaries (`libloading`), calls the plugin entry/factory/plugin vtables
//! (`clap-sys` raw bindings) and implements the host callbacks. Everything it exposes to the rest
//! of the crate is a safe API returning `Result`.
//!
//! Soundness rests on the CLAP contract (the plugin honours the vtable signatures and the
//! documented pointer lifetimes). A plugin that violates it (e.g. dereferences garbage) can still
//! crash the process: hosting in-process can't prevent that. What this module guarantees is that
//! *SoundCraft* never hands the plugin dangling or mis-sized pointers, never calls a missing
//! (null) function pointer, and never reads plugin strings beyond their bounds.

use crate::{ClapError, scan};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
    CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_RECORDING, clap_event_header, clap_event_midi, clap_event_note, clap_event_param_value,
    clap_event_transport, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui, clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency, clap_plugin_latency};
use clap_sys::ext::log::{CLAP_EXT_LOG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_WARNING, clap_host_log, clap_log_severity};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_note_port_info, clap_plugin_note_ports};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags, clap_param_info, clap_param_rescan_flags, clap_plugin_params,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state, clap_plugin_state};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{CLAP_PROCESS_ERROR, clap_process};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};
use soundcraft_dsp::Transport;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Most plugins one factory may report (hostile-input cap).
const MAX_PLUGINS_PER_FACTORY: u32 = 1024;
/// Most parameters read from one plugin.
pub(crate) const MAX_PARAMS: u32 = 4096;
/// Most audio ports per direction we provide buffers for.
pub(crate) const MAX_PORTS: u32 = 16;
/// Most channels per audio port.
pub(crate) const MAX_PORT_CHANNELS: u32 = 64;
/// Most feature strings read from a descriptor.
const MAX_FEATURES: usize = 64;
/// Capacity of the per-block event list (preallocated: `process` never allocates).
pub(crate) const EVENT_CAP: usize = 1024;
/// Largest plugin state we accept from `clap_plugin_state.save` (hostile-input cap).
pub(crate) const MAX_STATE_BYTES: usize = 64 << 20;
/// Most editor parameter edits buffered between two UI frames.
const EDIT_CAP: usize = 256;

// ---- plugin strings ------------------------------------------------------------------------

/// Reads a NUL-terminated plugin string (`None` for null).
///
/// SAFETY contract for callers: `p` is null or points to a NUL-terminated string that stays valid
/// for the duration of the call (CLAP guarantees this for descriptor fields).
unsafe fn read_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated per this function's contract.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// Reads a fixed-size `char[N]` name buffer, stopping at the first NUL or the end of the array.
/// `c_char` is `i8` on x86 and `u8` on ARM/RISC-V Linux; the byte reinterpretation is the same for both.
fn read_name(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| u8::from_ne_bytes(c.to_ne_bytes())).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---- loaded binaries -----------------------------------------------------------------------

/// A plugin descriptor as reported by the factory.
#[derive(Debug, Clone)]
pub(crate) struct RawDescriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub description: String,
    pub features: Vec<String>,
}

/// A loaded, initialised CLAP binary. Loaded binaries are cached for the life of the process
/// (see [`Bundle::load`]): plugin code must never be unmapped while an instance may still run.
pub(crate) struct Bundle {
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    // Dropped last: the library must outlive every pointer above.
    _lib: libloading::Library,
}

// SAFETY: CLAP requires the entry and the plugin factory to be thread-safe; the raw pointers are
// read-only vtables inside the loaded library, which `_lib` keeps mapped.
unsafe impl Send for Bundle {}
// SAFETY: as above; `Bundle` has no interior mutability on the Rust side.
unsafe impl Sync for Bundle {}

fn loaded() -> &'static Mutex<HashMap<PathBuf, Arc<Bundle>>> {
    static L: OnceLock<Mutex<HashMap<PathBuf, Arc<Bundle>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Bundle {
    /// Loads (once per path, then cached for the life of the process) and initialises a `.clap`.
    pub fn load(path: &Path) -> Result<Arc<Bundle>, ClapError> {
        let mut map = loaded().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(b) = map.get(path) {
            return Ok(b.clone());
        }
        let b = Arc::new(Bundle::open(path)?);
        map.insert(path.to_path_buf(), b.clone());
        Ok(b)
    }

    fn open(path: &Path) -> Result<Bundle, ClapError> {
        let err = |m: String| ClapError::Load(path.display().to_string(), m);
        let exe = scan::bundle_executable(path).ok_or_else(|| err("no loadable binary".into()))?;
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in CLAP plugin folders or named explicitly.
        let lib = unsafe { libloading::Library::new(&exe) }.map_err(|e| err(e.to_string()))?;
        // SAFETY: `clap_entry` is the CLAP-mandated exported data symbol of type
        // `const clap_plugin_entry`; the symbol's address is therefore a `*const clap_plugin_entry`.
        let entry: *const clap_plugin_entry = match unsafe { lib.get::<*const clap_plugin_entry>(b"clap_entry\0") } {
            Ok(sym) => *sym,
            Err(e) => return Err(err(format!("not a CLAP plugin: {e}"))),
        };
        if entry.is_null() {
            return Err(err("null clap_entry".into()));
        }
        // SAFETY: non-null pointer to the entry struct inside the library, which `lib` keeps mapped.
        let e = unsafe { *entry };
        if !clap_version_is_compatible(e.clap_version) {
            return Err(err(format!("unsupported CLAP version {}.{}", e.clap_version.major, e.clap_version.minor)));
        }
        let (Some(init), Some(get_factory)) = (e.init, e.get_factory) else { return Err(err("incomplete clap_entry".into())) };
        let cpath = CString::new(path.to_string_lossy().into_owned()).map_err(|_| err("path contains NUL".into()))?;
        // SAFETY: `init` comes from a valid entry; the path is a valid NUL-terminated string that
        // outlives the call.
        if !unsafe { init(cpath.as_ptr()) } {
            return Err(err("clap_entry.init failed".into()));
        }
        // SAFETY: the entry is initialised; the id is a static NUL-terminated string.
        let factory = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        if factory.is_null() {
            if let Some(deinit) = e.deinit {
                // SAFETY: balanced with the successful `init` above.
                unsafe { deinit() };
            }
            return Err(err("no plugin factory".into()));
        }
        Ok(Bundle { entry, factory, _lib: lib })
    }

    fn factory(&self) -> clap_plugin_factory {
        // SAFETY: `factory` was non-null at load time and points into the still-mapped library.
        unsafe { *self.factory }
    }

    /// The plugins this binary provides.
    pub fn descriptors(&self) -> Vec<RawDescriptor> {
        let f = self.factory();
        let (Some(count), Some(get)) = (f.get_plugin_count, f.get_plugin_descriptor) else { return Vec::new() };
        // SAFETY: vtable call with the factory pointer it belongs to.
        let n = unsafe { count(self.factory) }.min(MAX_PLUGINS_PER_FACTORY);
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: index is below the reported count.
            let d = unsafe { get(self.factory, i) };
            if d.is_null() {
                continue;
            }
            // SAFETY: non-null descriptor owned by the plugin library, valid while it is loaded.
            let d = unsafe { *d };
            // SAFETY: descriptor string fields are null or NUL-terminated (CLAP contract).
            let Some(id) = (unsafe { read_cstr(d.id) }).filter(|s| !s.is_empty()) else { continue };
            // SAFETY: as above.
            let name = unsafe { read_cstr(d.name) }.unwrap_or_else(|| id.clone());
            let mut features = Vec::new();
            if !d.features.is_null() {
                for k in 0..MAX_FEATURES {
                    // SAFETY: `features` is a NULL-terminated array; we stop at the terminator
                    // (or the cap) and never read past it.
                    let p = unsafe { *d.features.add(k) };
                    if p.is_null() {
                        break;
                    }
                    // SAFETY: a non-null entry of the feature array is a NUL-terminated string.
                    if let Some(s) = unsafe { read_cstr(p) } {
                        features.push(s);
                    }
                }
            }
            out.push(RawDescriptor {
                id,
                name,
                // SAFETY: descriptor string fields are null or NUL-terminated.
                vendor: unsafe { read_cstr(d.vendor) }.unwrap_or_default(),
                // SAFETY: as above.
                version: unsafe { read_cstr(d.version) }.unwrap_or_default(),
                // SAFETY: as above.
                description: unsafe { read_cstr(d.description) }.unwrap_or_default(),
                features,
            });
        }
        out
    }

    /// Creates and initialises one plugin instance.
    pub fn instantiate(self: &Arc<Self>, plugin_id: &str) -> Result<Instance, ClapError> {
        let err = |m: &str| ClapError::Instantiate(plugin_id.to_string(), m.to_string());
        let create = self.factory().create_plugin.ok_or_else(|| err("factory cannot create plugins"))?;
        let cid = CString::new(plugin_id).map_err(|_| err("id contains NUL"))?;
        let state = Arc::new(HostState::default());
        let host = Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: Arc::as_ptr(&state).cast_mut().cast::<c_void>(),
            name: c"SoundCraft".as_ptr(),
            vendor: c"SoundCraft contributors".as_ptr(),
            url: c"https://github.com/storytold/soundcraft".as_ptr(),
            version: c"0.1.0".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_request_restart),
            request_process: Some(host_request_process),
            request_callback: Some(host_request_callback),
        });
        // SAFETY: the factory is valid; `host` is boxed (stable address) and owned by the returned
        // `Instance`, which destroys the plugin before dropping the box; `cid` outlives the call.
        let plugin = unsafe { create(self.factory, &*host, cid.as_ptr()) };
        if plugin.is_null() {
            return Err(err("factory returned no instance"));
        }
        let mut inst = Instance {
            plugin,
            params: std::ptr::null(),
            ports: std::ptr::null(),
            latency: std::ptr::null(),
            notes: std::ptr::null(),
            state_ext: std::ptr::null(),
            gui: Arc::new(GuiLink::default()),
            raw: Vec::with_capacity(EVENT_CAP),
            out_params: Vec::with_capacity(EVENT_CAP),
            active: false,
            processing: false,
            max_frames: 0,
            created: true,
            _host: host,
            state,
            _bundle: self.clone(),
        };
        // SAFETY: `plugin` is a fresh non-null instance; we copy its vtable struct.
        let vt = unsafe { *plugin };
        let ok = match vt.init {
            // SAFETY: `init` is called exactly once, right after creation, as CLAP requires.
            Some(init) => unsafe { init(plugin) },
            None => false,
        };
        if !ok {
            // `Drop` destroys the instance (without deactivating: it never activated).
            return Err(err("plugin init failed"));
        }
        inst.params = inst.extension::<clap_plugin_params>(CLAP_EXT_PARAMS);
        inst.ports = inst.extension::<clap_plugin_audio_ports>(CLAP_EXT_AUDIO_PORTS);
        inst.latency = inst.extension::<clap_plugin_latency>(CLAP_EXT_LATENCY);
        inst.notes = inst.extension::<clap_plugin_note_ports>(CLAP_EXT_NOTE_PORTS);
        inst.state_ext = inst.extension::<clap_plugin_state>(CLAP_EXT_STATE);
        let gui = inst.extension::<clap_plugin_gui>(CLAP_EXT_GUI);
        inst.gui = Arc::new(GuiLink {
            inner: Mutex::new(GuiInner {
                plugin,
                gui,
                state: inst.state_ext,
                alive: true,
                created: false,
                visible: false,
                window: None,
                sizer: None,
            }),
            host: Arc::clone(&inst.state),
        });
        Ok(inst)
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        // SAFETY: `entry` was valid at load time and `_lib` (dropped after this) keeps it mapped.
        let e = unsafe { *self.entry };
        if let Some(deinit) = e.deinit {
            // SAFETY: balanced with the successful `init` in `open`; no instance holds an `Arc`
            // to this bundle any more (they keep it alive), so no plugin code still runs.
            unsafe { deinit() };
        }
    }
}

// ---- host side -----------------------------------------------------------------------------

/// Flags the plugin raises through the host callbacks (any thread).
pub(crate) struct HostState {
    pub restart: AtomicBool,
    pub process: AtomicBool,
    pub callback: AtomicBool,
    pub latency_changed: AtomicBool,
    pub params_rescan: AtomicBool,
    /// `clap_host_state.mark_dirty`.
    pub dirty: AtomicBool,
    /// `clap_host_gui` requests, serviced by the editor's `idle` on the main thread.
    pub gui_closed: AtomicBool,
    pub gui_destroyed: AtomicBool,
    pub gui_show: AtomicBool,
    pub gui_hide: AtomicBool,
    /// A size an embedded editor asked for (`width << 32 | height`; 0 = none pending).
    pub gui_resize: AtomicU64,
    /// Parameter changes the plugin reported (output events: typically edits in its GUI),
    /// waiting for the UI. Bounded by `EDIT_CAP`; the audio thread only `try_lock`s it.
    pub edits: Mutex<Vec<(u32, f64)>>,
}

impl Default for HostState {
    fn default() -> Self {
        HostState {
            restart: AtomicBool::new(false),
            process: AtomicBool::new(false),
            callback: AtomicBool::new(false),
            latency_changed: AtomicBool::new(false),
            params_rescan: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            gui_closed: AtomicBool::new(false),
            gui_destroyed: AtomicBool::new(false),
            gui_show: AtomicBool::new(false),
            gui_hide: AtomicBool::new(false),
            gui_resize: AtomicU64::new(0),
            edits: Mutex::new(Vec::with_capacity(EDIT_CAP)),
        }
    }
}

/// SAFETY contract: `host` is null or one of our `clap_host` structs, whose `host_data` points
/// to the `HostState` owned by the same `Instance`.
unsafe fn state<'a>(host: *const clap_host) -> Option<&'a HostState> {
    if host.is_null() {
        return None;
    }
    // SAFETY: non-null host created by `Bundle::instantiate` (contract above).
    let data = unsafe { (*host).host_data } as *const HostState;
    // SAFETY: `host_data` points to the boxed `HostState`, alive as long as the instance.
    unsafe { data.as_ref() }
}

unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.restart.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_request_process(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.process.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.callback.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_log(_host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    // SAFETY: CLAP passes a NUL-terminated message (or null, handled).
    let m = unsafe { read_cstr(msg) }.unwrap_or_default();
    match severity {
        CLAP_LOG_ERROR | CLAP_LOG_FATAL => log::error!("clap plugin: {m}"),
        CLAP_LOG_WARNING => log::warn!("clap plugin: {m}"),
        _ => log::debug!("clap plugin: {m}"),
    }
}

unsafe extern "C" fn host_params_rescan(host: *const clap_host, _flags: clap_param_rescan_flags) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.params_rescan.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_params_clear(_host: *const clap_host, _id: clap_id, _flags: clap_param_clear_flags) {}

unsafe extern "C" fn host_params_request_flush(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.process.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_latency_changed(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.latency_changed.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_state_mark_dirty(host: *const clap_host) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        s.dirty.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_gui_resize_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn host_gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    // Floating windows belong to the plugin, which resizes them itself; an embedded editor's host
    // window is resized by the editor's `idle` on the main thread.
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) }
        && width > 0
        && height > 0
    {
        s.gui_resize.store((u64::from(width) << 32) | u64::from(height), Ordering::Relaxed);
    }
    true
}

unsafe extern "C" fn host_gui_request_show(host: *const clap_host) -> bool {
    // SAFETY: the plugin passes back the host pointer we gave it.
    match unsafe { state(host) } {
        Some(s) => {
            s.gui_show.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_gui_request_hide(host: *const clap_host) -> bool {
    // SAFETY: the plugin passes back the host pointer we gave it.
    match unsafe { state(host) } {
        Some(s) => {
            s.gui_hide.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

unsafe extern "C" fn host_gui_closed(host: *const clap_host, was_destroyed: bool) {
    // SAFETY: the plugin passes back the host pointer we gave it.
    if let Some(s) = unsafe { state(host) } {
        if was_destroyed {
            s.gui_destroyed.store(true, Ordering::Relaxed);
        }
        s.gui_closed.store(true, Ordering::Relaxed);
    }
}

static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(host_state_mark_dirty) };
static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};
static HOST_LOG: clap_host_log = clap_host_log { log: Some(host_log) };
static HOST_PARAMS: clap_host_params =
    clap_host_params { rescan: Some(host_params_rescan), clear: Some(host_params_clear), request_flush: Some(host_params_request_flush) };
static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(host_latency_changed) };

unsafe extern "C" fn host_get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    // SAFETY: extension ids are NUL-terminated strings (CLAP contract); non-null checked above.
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_LOG {
        (&HOST_LOG as *const clap_host_log).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&HOST_PARAMS as *const clap_host_params).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&HOST_LATENCY as *const clap_host_latency).cast()
    } else if id == CLAP_EXT_STATE {
        (&HOST_STATE as *const clap_host_state).cast()
    } else if id == CLAP_EXT_GUI {
        (&HOST_GUI as *const clap_host_gui).cast()
    } else {
        // Everything else (thread-check, timers, fd support, …) is unsupported: CLAP allows null.
        std::ptr::null()
    }
}

// ---- events --------------------------------------------------------------------------------

/// A host → plugin event, safe to build anywhere; converted to CLAP structs inside `process`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Event {
    Param {
        time: u32,
        id: u32,
        value: f64,
    },
    NoteOn {
        time: u32,
        key: u8,
        velocity: f64,
    },
    NoteOff {
        time: u32,
        key: u8,
    },
    /// A raw MIDI 1.0 message (for plugins whose note port speaks only MIDI).
    Midi {
        time: u32,
        data: [u8; 3],
    },
}

impl Event {
    pub fn time(&self) -> u32 {
        match *self {
            Event::Param { time, .. } | Event::NoteOn { time, .. } | Event::NoteOff { time, .. } | Event::Midi { time, .. } => time,
        }
    }
    pub fn set_time(&mut self, t: u32) {
        match self {
            Event::Param { time, .. } | Event::NoteOn { time, .. } | Event::NoteOff { time, .. } | Event::Midi { time, .. } => *time = t,
        }
    }
}

#[derive(Clone, Copy)]
enum RawEvent {
    Note(clap_event_note),
    Param(clap_event_param_value),
    Midi(clap_event_midi),
}

impl RawEvent {
    fn header(&self) -> *const clap_event_header {
        // The header is the first field of every `repr(C)` CLAP event struct, so a pointer to it is
        // a pointer to the whole event, as CLAP expects.
        match self {
            RawEvent::Note(e) => &e.header,
            RawEvent::Param(e) => &e.header,
            RawEvent::Midi(e) => &e.header,
        }
    }

    fn from_event(e: &Event, offset: u32) -> RawEvent {
        let header = |size: usize, type_: u16, time: u32| clap_event_header {
            size: u32::try_from(size).unwrap_or(u32::MAX),
            time: time.saturating_sub(offset),
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: 0,
        };
        match *e {
            Event::Param { time, id, value } => RawEvent::Param(clap_event_param_value {
                header: header(size_of::<clap_event_param_value>(), CLAP_EVENT_PARAM_VALUE, time),
                param_id: id,
                cookie: std::ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value,
            }),
            Event::NoteOn { time, key, velocity } => RawEvent::Note(clap_event_note {
                header: header(size_of::<clap_event_note>(), CLAP_EVENT_NOTE_ON, time),
                note_id: -1,
                port_index: 0,
                channel: 0,
                key: i16::from(key),
                velocity,
            }),
            Event::NoteOff { time, key } => RawEvent::Note(clap_event_note {
                header: header(size_of::<clap_event_note>(), CLAP_EVENT_NOTE_OFF, time),
                note_id: -1,
                port_index: 0,
                channel: 0,
                key: i16::from(key),
                velocity: 0.0,
            }),
            Event::Midi { time, data } => {
                RawEvent::Midi(clap_event_midi { header: header(size_of::<clap_event_midi>(), CLAP_EVENT_MIDI, time), port_index: 0, data })
            }
        }
    }
}

fn transport_event(t: &Transport) -> clap_event_transport {
    let fixed = |x: f64, factor: i64| if x.is_finite() { (x * factor as f64).round() as i64 } else { 0 };
    let mut flags =
        CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if t.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if t.recording {
        flags |= CLAP_TRANSPORT_IS_RECORDING;
    }
    clap_event_transport {
        header: clap_event_header {
            size: u32::try_from(size_of::<clap_event_transport>()).unwrap_or(u32::MAX),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags,
        song_pos_beats: fixed(t.quarters, CLAP_BEATTIME_FACTOR),
        song_pos_seconds: fixed(t.seconds, CLAP_SECTIME_FACTOR),
        tempo: t.tempo,
        tempo_inc: 0.0,
        loop_start_beats: 0,
        loop_end_beats: 0,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: fixed(t.bar_start, CLAP_BEATTIME_FACTOR),
        bar_number: i32::try_from(t.bar).unwrap_or(0),
        tsig_num: u16::try_from(t.numerator).unwrap_or(4),
        tsig_denom: u16::try_from(t.denominator).unwrap_or(4),
    }
}

unsafe extern "C" fn in_events_size(list: *const clap_input_events) -> u32 {
    if list.is_null() {
        return 0;
    }
    // SAFETY: `list` is the `clap_input_events` we built in `process`; its ctx is the event Vec.
    let v = unsafe { ((*list).ctx as *const Vec<RawEvent>).as_ref() };
    v.map_or(0, |v| u32::try_from(v.len()).unwrap_or(u32::MAX))
}

unsafe extern "C" fn in_events_get(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    if list.is_null() {
        return std::ptr::null();
    }
    // SAFETY: as in `in_events_size`; the Vec is not mutated while the plugin processes.
    let v = unsafe { ((*list).ctx as *const Vec<RawEvent>).as_ref() };
    v.and_then(|v| v.get(index as usize)).map_or(std::ptr::null(), RawEvent::header)
}

unsafe extern "C" fn out_events_try_push(list: *const clap_output_events, event: *const clap_event_header) -> bool {
    // Plugin → host events are accepted. Parameter values (typically edits in the plugin's GUI)
    // are kept, while there is room, so the UI can write them into the session; the rest is
    // ignored.
    if list.is_null() || event.is_null() {
        return true;
    }
    // SAFETY: CLAP passes a valid event header for the duration of the call.
    let h = unsafe { &*event };
    if h.space_id != CLAP_CORE_EVENT_SPACE_ID || h.type_ != CLAP_EVENT_PARAM_VALUE || (h.size as usize) < size_of::<clap_event_param_value>() {
        return true;
    }
    // SAFETY: the header says this is a complete `clap_event_param_value` (type and size checked).
    let e = unsafe { &*event.cast::<clap_event_param_value>() };
    // SAFETY: `ctx` is null or the instance's preallocated `out_params` Vec, which `process`
    // borrows exclusively for the duration of the plugin call.
    if let Some(v) = unsafe { ((*list).ctx as *mut Vec<(u32, f64)>).as_mut() }
        && v.len() < v.capacity()
        && e.value.is_finite()
    {
        v.push((e.param_id, e.value));
    }
    true
}

// ---- state streams -------------------------------------------------------------------------

/// Read side of an in-memory state blob.
struct ReadCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

unsafe extern "C" fn istream_read(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    // SAFETY: `ctx` is the `ReadCursor` on `load_state`'s stack frame, alive during the call.
    let Some(c) = (unsafe { ((*stream).ctx as *mut ReadCursor<'_>).as_mut() }) else { return -1 };
    let rest = c.data.get(c.pos..).unwrap_or(&[]);
    let n = rest.len().min(usize::try_from(size).unwrap_or(usize::MAX));
    if let Some(src) = rest.get(..n) {
        // SAFETY: the plugin provides `size` ≥ `n` writable bytes at `buffer`; `src` is `n`
        // readable bytes of our own slice; they cannot overlap.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buffer.cast::<u8>(), n) };
    }
    c.pos += n;
    i64::try_from(n).unwrap_or(-1)
}

unsafe extern "C" fn ostream_write(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    // SAFETY: `ctx` is the output Vec on `save_state`'s stack frame, alive during the call.
    let Some(v) = (unsafe { ((*stream).ctx as *mut Vec<u8>).as_mut() }) else { return -1 };
    let Ok(n) = usize::try_from(size) else { return -1 };
    if v.len().saturating_add(n) > MAX_STATE_BYTES {
        return -1;
    }
    // SAFETY: the plugin provides `size` readable bytes at `buffer` for the call.
    let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
    v.extend_from_slice(src);
    i64::try_from(n).unwrap_or(-1)
}

// ---- editor (clap.gui) ---------------------------------------------------------------------

/// True on the process's main thread (AppKit requires GUI calls there).
#[cfg(target_os = "macos")]
pub(crate) fn on_main_thread() -> bool {
    unsafe extern "C" {
        fn pthread_main_np() -> std::ffi::c_int;
    }
    // SAFETY: a libSystem function without arguments or preconditions.
    unsafe { pthread_main_np() == 1 }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn on_main_thread() -> bool {
    true
}

/// The windowing API of this platform.
fn window_api() -> Option<&'static CStr> {
    if cfg!(target_os = "macos") {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_COCOA)
    } else if cfg!(windows) {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_WIN32)
    } else if cfg!(all(unix, not(target_os = "android"))) {
        Some(clap_sys::ext::gui::CLAP_WINDOW_API_X11)
    } else {
        None
    }
}

/// Negotiates user resizes of an embedded editor's host window with the plugin (CLAP: when
/// `can_resize`, the host asks `adjust_size`, sizes the window to the answer, then `set_size`).
///
/// Shared by the editor link and the host window's procedure. Everything runs on the main thread;
/// the lock only guards against the instance clearing the plugin from another thread while a
/// resize calls into it. Loops are cut by `applying`: while the host applies a size (a plugin's
/// `request_resize`, or a user resize being constrained and handed to `set_size`), the window
/// notifications that this causes are ignored rather than negotiated again.
pub(crate) struct GuiSizer {
    target: Mutex<Option<SizeTarget>>,
    /// The thread inside a call into the plugin under `target`'s lock (see `with_target`).
    holder: Mutex<Option<std::thread::ThreadId>>,
    /// A target change requested re-entrantly by the holder thread, applied when its call returns.
    pending: Mutex<Option<Option<SizeTarget>>>,
    /// The plugin said `can_resize`: user resizes are negotiated (the window has a sizing border).
    resizable: AtomicBool,
    /// Nesting depth of resizes being applied by the host.
    applying: AtomicU32,
    /// The size last given to the plugin (`width << 32 | height`), to skip repeated `set_size`.
    last: AtomicU64,
}

// User resizes come from the Windows and macOS host windows only; elsewhere the plugin side is
// unused.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
/// The plugin and the two `clap.gui` calls a user resize needs.
#[derive(Clone, Copy)]
struct SizeTarget {
    plugin: *const clap_plugin,
    adjust_size: Option<unsafe extern "C" fn(*const clap_plugin, *mut u32, *mut u32) -> bool>,
    set_size: Option<unsafe extern "C" fn(*const clap_plugin, u32, u32) -> bool>,
}

// SAFETY: the plugin pointer is only called through on the main thread (the window procedure),
// under the sizer's lock, and the editor link clears it under that lock (or, re-entrantly, right
// after the call holding it) before the GUI goes.
unsafe impl Send for SizeTarget {}

/// Ends an "applying" section of a [`GuiSizer`] when dropped.
pub(crate) struct Applying<'a>(&'a AtomicU32);

impl Drop for Applying<'_> {
    fn drop(&mut self) {
        // Paired with the increment in `GuiSizer::applying`: never below zero.
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

// User resizes come from the Windows and macOS host windows only; elsewhere the plugin side is
// unused.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
/// A host-window size in whole pixels (non-finite or out-of-range sizes are clamped).
fn px(v: f64) -> u32 {
    if v.is_finite() { v.round().clamp(1.0, 16_384.0) as u32 } else { 1 }
}

// User resizes come from the Windows and macOS host windows only; elsewhere the plugin side is
// unused.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
impl GuiSizer {
    pub fn new(w: u32, h: u32) -> GuiSizer {
        let s = GuiSizer {
            target: Mutex::new(None),
            holder: Mutex::new(None),
            pending: Mutex::new(None),
            resizable: AtomicBool::new(false),
            applying: AtomicU32::new(0),
            last: AtomicU64::new(0),
        };
        s.set_last(w, h);
        s
    }

    fn holder(&self) -> std::sync::MutexGuard<'_, Option<std::thread::ThreadId>> {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<Option<SizeTarget>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the plugin under the target lock (`None` without one). A target change
    /// requested from inside `f` on this thread (a plugin pumping a nested event loop that closes
    /// the editor) is deferred until `f` returns instead of deadlocking on the lock.
    fn with_target<R>(&self, f: impl FnOnce(&SizeTarget) -> R) -> Option<R> {
        let mut g = self.target.lock().unwrap_or_else(PoisonError::into_inner);
        *self.holder() = Some(std::thread::current().id());
        let out = g.as_ref().map(f);
        *self.holder() = None;
        if let Some(t) = self.pending().take() {
            *g = t;
        }
        out
    }

    /// Hands the plugin over once its GUI is parented and shown (`can_resize` decides whether
    /// user resizes are negotiated); `None` takes it away again (before the GUI is destroyed).
    fn set_target(&self, target: Option<SizeTarget>, resizable: bool) {
        self.resizable.store(resizable && target.is_some(), Ordering::Relaxed);
        if *self.holder() == Some(std::thread::current().id()) {
            *self.pending() = Some(target);
            return;
        }
        *self.target.lock().unwrap_or_else(PoisonError::into_inner) = target;
    }

    pub fn resizable(&self) -> bool {
        self.resizable.load(Ordering::Relaxed)
    }

    /// Marks a host-applied resize; window notifications are ignored until the guard drops.
    pub fn applying(&self) -> Applying<'_> {
        self.applying.fetch_add(1, Ordering::Relaxed);
        Applying(&self.applying)
    }

    fn is_applying(&self) -> bool {
        self.applying.load(Ordering::Relaxed) > 0
    }

    fn set_last(&self, w: u32, h: u32) {
        self.last.store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
    }

    fn last(&self) -> (u32, u32) {
        let v = self.last.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    /// The size the plugin accepts instead of `w`×`h` (`adjust_size`), or `None` when no resize
    /// should be negotiated (it can't resize, is gone, or a resize is being applied).
    pub fn constrain(&self, w: f64, h: f64) -> Option<(f64, f64)> {
        if !self.resizable() || self.is_applying() {
            return None;
        }
        let _guard = self.applying();
        let (w, h) = self.with_target(|t| adjust(t, px(w), px(h)))?;
        Some((f64::from(w), f64::from(h)))
    }

    /// The window's client area was resized to `w`×`h` by the user (or the window manager):
    /// settles on the plugin's adjusted size, makes the window follow it through `set_window`
    /// when it differs, and tells the plugin with `set_size`. Ignored while the host applies a
    /// resize itself.
    pub fn user_resized(&self, w: f64, h: f64, set_window: impl FnOnce(f64, f64)) {
        if !self.resizable() || self.is_applying() {
            return;
        }
        let _guard = self.applying();
        self.with_target(|t| {
            let (w, h) = (px(w), px(h));
            let (aw, ah) = adjust(t, w, h);
            let last = self.last();
            let accepted = last == (aw, ah)
                || match t.set_size {
                    // SAFETY: live plugin with a created, parented GUI (the editor link takes the
                    // target away before destroying it, deferred to here when that happens inside
                    // this call); main thread.
                    Some(f) => unsafe { f(t.plugin, aw, ah) },
                    None => false,
                };
            if accepted {
                self.set_last(aw, ah);
                if (aw, ah) != (w, h) {
                    set_window(f64::from(aw), f64::from(ah));
                }
            } else {
                // The plugin refused the size: the window goes back to the last one it took.
                set_window(f64::from(last.0), f64::from(last.1));
            }
        });
    }
}

// User resizes come from the Windows and macOS host windows only; elsewhere the plugin side is
// unused.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
/// `adjust_size` for a proposed `w`×`h`; the proposal stands when the plugin can't adjust it.
fn adjust(t: &SizeTarget, w: u32, h: u32) -> (u32, u32) {
    let Some(f) = t.adjust_size else { return (w, h) };
    let (mut aw, mut ah) = (w, h);
    // SAFETY: live plugin with a created GUI (see `user_resized`); both out-pointers are locals.
    if unsafe { f(t.plugin, &mut aw, &mut ah) } && aw > 0 && ah > 0 { (aw.min(16_384), ah.min(16_384)) } else { (w, h) }
}

/// The window rect while the user drags `edge` (a Windows `WMSZ_*` value): moves only the dragged
/// edges so the outer size becomes `w`×`h`. Pure, so it's testable on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn sizing_rect(edge: usize, (left, top, right, bottom): (i32, i32, i32, i32), (w, h): (i32, i32)) -> (i32, i32, i32, i32) {
    // WMSZ_LEFT = 1, WMSZ_TOP = 3, WMSZ_TOPLEFT = 4, WMSZ_TOPRIGHT = 5, WMSZ_BOTTOMLEFT = 7.
    let (l, r) = if matches!(edge, 1 | 4 | 7) { (right.saturating_sub(w), right) } else { (left, left.saturating_add(w)) };
    let (t, b) = if matches!(edge, 3..=5) { (bottom.saturating_sub(h), bottom) } else { (top, top.saturating_add(h)) };
    (l, t, r, b)
}

struct GuiInner {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
    state: *const clap_plugin_state,
    /// The plugin instance exists (cleared by `Instance::drop` before it destroys the plugin).
    alive: bool,
    created: bool,
    visible: bool,
    /// The host window of an embedded editor (`None` for floating editors).
    window: Option<hostwin::HostWindow>,
    /// Negotiates user resizes of `window` (`None` for floating editors).
    sizer: Option<Arc<GuiSizer>>,
}

/// The link between an instance and its editor and state handles. The instance may process on
/// the audio thread while the editor is driven and the state saved from the main thread: CLAP
/// allows `clap.gui` and `clap.state` (main-thread) calls concurrently with audio-thread calls.
/// Every such call happens under this lock, and the instance clears `alive` (destroying any GUI)
/// under the same lock before it destroys the plugin, so a handle can never call into a destroyed
/// plugin.
pub(crate) struct GuiLink {
    inner: Mutex<GuiInner>,
    host: Arc<HostState>,
}

impl Default for GuiLink {
    fn default() -> Self {
        GuiLink {
            inner: Mutex::new(GuiInner {
                plugin: std::ptr::null(),
                gui: std::ptr::null(),
                state: std::ptr::null(),
                alive: false,
                created: false,
                visible: false,
                window: None,
                sizer: None,
            }),
            host: Arc::new(HostState::default()),
        }
    }
}

// SAFETY: the raw pointers are only dereferenced under the mutex, while `alive` guarantees the
// plugin exists; CLAP's threading rules (main-thread GUI calls) are checked at each call.
unsafe impl Send for GuiInner {}

impl GuiLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, GuiInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn available(&self) -> bool {
        let g = self.lock();
        g.alive && !g.gui.is_null()
    }

    pub fn host(&self) -> &HostState {
        &self.host
    }

    fn vt(g: &GuiInner) -> Option<clap_plugin_gui> {
        // SAFETY: `gui` is null or the plugin's static extension vtable, valid while `alive`.
        (g.alive).then(|| unsafe { g.gui.as_ref() }.copied()).flatten()
    }

    /// Opens (creates and shows) the plugin's floating editor window.
    pub fn open(&self, title: &str) -> Result<(), String> {
        if !on_main_thread() {
            return Err("plugin editors must be opened on the main thread".into());
        }
        let mut g = self.lock();
        let vt = GuiLink::vt(&g).ok_or_else(|| "this plugin has no editor".to_string())?;
        // The user closed the editor (its host window, or the plugin's floating window) since the
        // last `idle`: free it and open a fresh one rather than reviving a closed window.
        if g.created && (g.window.as_ref().is_some_and(|w| !w.is_visible()) || self.host.gui_closed.swap(false, Ordering::Relaxed)) {
            self.host.gui_destroyed.store(false, Ordering::Relaxed);
            GuiLink::destroy(&mut g);
        }
        if g.created {
            if let Some(show) = vt.show {
                // SAFETY: live plugin with a created GUI; main thread (checked).
                unsafe { show(g.plugin) };
            }
            if let Some(w) = &g.window {
                w.raise();
            }
            g.visible = true;
            return Ok(());
        }
        let api = window_api().ok_or_else(|| "plugin editors are not supported on this platform".to_string())?;
        let (Some(supported), Some(create)) = (vt.is_api_supported, vt.create) else { return Err("incomplete clap.gui extension".into()) };
        // SAFETY: live plugin; `api` is a static NUL-terminated string; main thread.
        if !unsafe { supported(g.plugin, api.as_ptr(), true) } {
            // SAFETY: as above.
            if hostwin::SUPPORTED && unsafe { supported(g.plugin, api.as_ptr(), false) } {
                return self.open_embedded(&mut g, vt, api, title);
            }
            return Err("this plugin offers only an embedded editor, which SoundCraft hosts on macOS and Windows only".into());
        }
        self.host.gui_closed.store(false, Ordering::Relaxed);
        self.host.gui_destroyed.store(false, Ordering::Relaxed);
        // SAFETY: as above; no GUI exists yet.
        if !unsafe { create(g.plugin, api.as_ptr(), true) } {
            return Err("the plugin could not create its editor".into());
        }
        g.created = true;
        if !cfg!(target_os = "macos")
            && let Some(f) = vt.set_scale
        {
            // SAFETY: live plugin with a created GUI. (Cocoa uses logical pixels: not called.)
            unsafe { f(g.plugin, 1.0) };
        }
        if let (Some(f), Ok(t)) = (vt.suggest_title, CString::new(title)) {
            // SAFETY: live plugin with a created floating GUI; `t` outlives the call.
            unsafe { f(g.plugin, t.as_ptr()) };
        }
        let shown = match vt.show {
            // SAFETY: as above.
            Some(f) => unsafe { f(g.plugin) },
            None => false,
        };
        if !shown {
            GuiLink::destroy(&mut g);
            return Err("the plugin could not show its editor".into());
        }
        g.visible = true;
        Ok(())
    }

    /// Embeds the editor in a host window of ours (plugins that offer no floating editor).
    fn open_embedded(&self, g: &mut GuiInner, vt: clap_plugin_gui, api: &CStr, title: &str) -> Result<(), String> {
        let (Some(create), Some(set_parent)) = (vt.create, vt.set_parent) else { return Err("incomplete clap.gui extension".into()) };
        self.host.gui_closed.store(false, Ordering::Relaxed);
        self.host.gui_destroyed.store(false, Ordering::Relaxed);
        self.host.gui_resize.store(0, Ordering::Relaxed);
        // SAFETY: live plugin; `api` is a static NUL-terminated string; main thread; no GUI yet.
        if !unsafe { create(g.plugin, api.as_ptr(), false) } {
            return Err("the plugin could not create its editor".into());
        }
        g.created = true;
        if !cfg!(target_os = "macos")
            && let Some(f) = vt.set_scale
        {
            // SAFETY: live plugin with a created GUI. (Cocoa uses logical pixels: not called.)
            unsafe { f(g.plugin, 1.0) };
        }
        let (mut w, mut h) = (640u32, 480u32);
        if let Some(f) = vt.get_size {
            let (mut a, mut b) = (0u32, 0u32);
            // SAFETY: live plugin with a created GUI; both out-pointers are valid locals.
            if unsafe { f(g.plugin, &mut a, &mut b) } && a > 0 && b > 0 {
                (w, h) = (a.min(16_384), b.min(16_384));
            }
        }
        let win = match hostwin::HostWindow::new(title, f64::from(w), f64::from(h)) {
            Ok(win) => win,
            Err(e) => {
                GuiLink::destroy(g);
                return Err(e);
            }
        };
        let handle = if cfg!(target_os = "macos") { clap_window_handle { cocoa: win.parent() } } else { clap_window_handle { win32: win.parent() } };
        if win.parent().is_null() {
            win.close();
            GuiLink::destroy(g);
            return Err("the editor window has no view to attach to".into());
        }
        let parent = clap_window { api: api.as_ptr(), specific: handle };
        // SAFETY: live plugin with a created embedded GUI; `parent` names our live window and
        // outlives the call.
        if !unsafe { set_parent(g.plugin, &parent) } {
            win.close();
            GuiLink::destroy(g);
            return Err("the editor could not attach to its window".into());
        }
        g.window = Some(win);
        let shown = match vt.show {
            // SAFETY: live plugin with a created, parented GUI.
            Some(f) => unsafe { f(g.plugin) },
            None => true,
        };
        if !shown {
            GuiLink::destroy(g);
            return Err("the plugin could not show its editor".into());
        }
        // The window follows `can_resize`: a sizing border, with user resizes negotiated through
        // `adjust_size` and `set_size`.
        // SAFETY: live plugin with a created, parented GUI; main thread.
        let resizable = vt.can_resize.is_some_and(|f| unsafe { f(g.plugin) });
        let sizer = Arc::new(GuiSizer::new(w, h));
        sizer.set_target(Some(SizeTarget { plugin: g.plugin, adjust_size: vt.adjust_size, set_size: vt.set_size }), resizable);
        if let Some(win) = &mut g.window {
            win.set_sizer(&sizer);
            // The frame change of the new sizing border is not a user resize: the window gives
            // the client area back to the plugin's size, with nothing negotiated meanwhile.
            let _applying = sizer.applying();
            win.set_resizable(resizable);
            win.raise();
        }
        g.sizer = Some(sizer);
        g.visible = true;
        Ok(())
    }

    fn destroy(g: &mut GuiInner) {
        // No more user resizes reach the plugin once its GUI is going.
        if let Some(s) = g.sizer.take() {
            s.set_target(None, false);
        }
        if g.created
            && let Some(f) = GuiLink::vt(g).and_then(|v| v.destroy)
        {
            // SAFETY: live plugin with a created GUI; destroyed exactly once.
            unsafe { f(g.plugin) };
        }
        g.created = false;
        g.visible = false;
        // The plugin's child window is gone; now its host window can go too.
        if let Some(w) = g.window.take() {
            w.close();
        }
    }

    /// `clap_plugin_state.save` into memory (`None` without the extension, on failure, or once the
    /// plugin is gone).
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        if !g.alive {
            return None;
        }
        // SAFETY: `state` is null or the plugin's static extension vtable, valid while `alive`.
        let save = unsafe { g.state.as_ref() }?.save?;
        let mut out: Vec<u8> = Vec::new();
        let stream = clap_ostream { ctx: (&mut out as *mut Vec<u8>).cast::<c_void>(), write: Some(ostream_write) };
        // SAFETY: live plugin (`alive`, under the lock); the stream and its context live on this
        // frame for the call.
        unsafe { save(g.plugin, &stream) }.then_some(out)
    }

    /// Closes (destroys) the editor.
    pub fn close(&self) {
        if !on_main_thread() {
            return;
        }
        GuiLink::destroy(&mut self.lock());
    }

    pub fn is_open(&self) -> bool {
        let g = self.lock();
        g.alive && g.created && g.visible && g.window.as_ref().is_none_or(hostwin::HostWindow::is_visible)
    }

    /// Main-thread housekeeping: runs a requested `on_main_thread`, follows the plugin's
    /// show/hide/closed requests, and returns the parameter edits reported since the last call.
    pub fn idle(&self) -> Vec<(u32, f64)> {
        if !on_main_thread() {
            return Vec::new();
        }
        let mut g = self.lock();
        if !g.alive {
            return Vec::new();
        }
        if self.host.callback.swap(false, Ordering::Relaxed) {
            // SAFETY: `plugin` is valid while `alive`; we copy its vtable struct.
            if let Some(f) = unsafe { (*g.plugin).on_main_thread } {
                // SAFETY: live plugin, main thread, requested via `request_callback`.
                unsafe { f(g.plugin) };
            }
        }
        if g.created {
            let resize = self.host.gui_resize.swap(0, Ordering::Relaxed);
            if resize != 0
                && let Some(win) = &g.window
            {
                let (w, h) = ((resize >> 32) as u32, resize as u32);
                // The window notifications this resize causes are not user resizes.
                let _applying = g.sizer.as_ref().map(|s| s.applying());
                hostwin::resize(win.raw(), f64::from(w), f64::from(h));
                if let Some(s) = &g.sizer {
                    s.set_last(w, h);
                }
                if let Some(f) = GuiLink::vt(&g).and_then(|v| v.set_size) {
                    // SAFETY: live plugin with a created embedded GUI, main thread.
                    unsafe { f(g.plugin, w, h) };
                }
            }
            if g.window.as_ref().is_some_and(|w| !w.is_visible()) {
                // The user closed the embedded editor's host window: free the editor.
                GuiLink::destroy(&mut g);
            } else if self.host.gui_closed.swap(false, Ordering::Relaxed) {
                // The user closed the floating window (or the plugin lost its GUI): free it.
                self.host.gui_destroyed.store(false, Ordering::Relaxed);
                GuiLink::destroy(&mut g);
            } else if let Some(vt) = GuiLink::vt(&g) {
                if self.host.gui_show.swap(false, Ordering::Relaxed)
                    && let Some(f) = vt.show
                {
                    // SAFETY: live plugin with a created GUI, main thread.
                    g.visible = unsafe { f(g.plugin) };
                }
                if self.host.gui_hide.swap(false, Ordering::Relaxed)
                    && let Some(f) = vt.hide
                {
                    // SAFETY: as above.
                    unsafe { f(g.plugin) };
                    g.visible = false;
                }
            }
        }
        drop(g);
        let mut e = self.host.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let out = e.clone();
        e.clear();
        out
    }

    /// Called by the instance before it destroys the plugin.
    fn kill(&self) {
        let mut g = self.lock();
        if g.created {
            if on_main_thread() {
                GuiLink::destroy(&mut g);
            } else {
                log::warn!("clap: editor still open while its plugin is destroyed off the main thread");
                g.created = false;
                // Window calls are main-thread only: leave the host window rather than crash, but
                // cut its resize handling off from the plugin.
                g.window = None;
                if let Some(s) = g.sizer.take() {
                    s.set_target(None, false);
                }
            }
        }
        g.alive = false;
    }
}

// ---- audio buffers -------------------------------------------------------------------------

/// Planar f32 buffers for every audio port in one direction, with the pointer tables CLAP needs.
/// Allocated in `new`; `process` only rewrites pointers in place (no allocation).
pub(crate) struct PortBuffers {
    data: Vec<Vec<Vec<f32>>>,
    ptrs: Vec<Vec<*mut f32>>,
    bufs: Vec<clap_audio_buffer>,
    frames: usize,
}

// SAFETY: the raw pointers only ever point into `data`, which this struct owns; moving the struct
// to another thread moves that ownership with it.
unsafe impl Send for PortBuffers {}

impl PortBuffers {
    /// One buffer set per port, `channels[p]` channels each (capped), `frames` long.
    pub fn new(channels: &[u32], frames: usize) -> PortBuffers {
        let data: Vec<Vec<Vec<f32>>> =
            channels.iter().take(MAX_PORTS as usize).map(|&c| (0..c.min(MAX_PORT_CHANNELS)).map(|_| vec![0.0; frames]).collect()).collect();
        let ptrs = data.iter().map(|p| vec![std::ptr::null_mut(); p.len()]).collect();
        let bufs = data
            .iter()
            .map(|_| clap_audio_buffer { data32: std::ptr::null_mut(), data64: std::ptr::null_mut(), channel_count: 0, latency: 0, constant_mask: 0 })
            .collect();
        PortBuffers { data, ptrs, bufs, frames }
    }

    pub fn channels(&self, port: usize) -> usize {
        self.data.get(port).map_or(0, Vec::len)
    }

    pub fn channel(&self, port: usize, ch: usize) -> Option<&[f32]> {
        self.data.get(port).and_then(|p| p.get(ch)).map(Vec::as_slice)
    }

    pub fn channel_mut(&mut self, port: usize, ch: usize) -> Option<&mut [f32]> {
        self.data.get_mut(port).and_then(|p| p.get_mut(ch)).map(Vec::as_mut_slice)
    }

    /// Points the CLAP tables at the current data (no allocation).
    fn refresh(&mut self) {
        for ((d, p), b) in self.data.iter_mut().zip(self.ptrs.iter_mut()).zip(self.bufs.iter_mut()) {
            for (dc, pc) in d.iter_mut().zip(p.iter_mut()) {
                *pc = dc.as_mut_ptr();
            }
            b.data32 = p.as_mut_ptr();
            b.data64 = std::ptr::null_mut();
            b.channel_count = u32::try_from(p.len()).unwrap_or(0);
            b.constant_mask = 0;
        }
    }
}

// ---- instances -----------------------------------------------------------------------------

/// What one audio port looks like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PortInfo {
    pub channels: u32,
    pub main: bool,
}

/// One parameter as reported by `clap_plugin_params.get_info`.
#[derive(Debug, Clone)]
pub(crate) struct RawParam {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub flags: u32,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

/// Note dialects of the plugin's first input note port.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NotePort {
    pub supported: u32,
    pub preferred: u32,
}

/// A live plugin instance plus the host struct it was created with.
pub(crate) struct Instance {
    plugin: *const clap_plugin,
    params: *const clap_plugin_params,
    ports: *const clap_plugin_audio_ports,
    latency: *const clap_plugin_latency,
    notes: *const clap_plugin_note_ports,
    state_ext: *const clap_plugin_state,
    gui: Arc<GuiLink>,
    raw: Vec<RawEvent>,
    /// Parameter values the plugin reported through output events in the current `process`.
    out_params: Vec<(u32, f64)>,
    active: bool,
    processing: bool,
    max_frames: usize,
    created: bool,
    // Field order matters for drop: the plugin is destroyed in `Drop::drop` first; then the host
    // struct and its state, then the bundle (which keeps the code mapped) go.
    _host: Box<clap_host>,
    state: Arc<HostState>,
    _bundle: Arc<Bundle>,
}

// SAFETY: an `Instance` is used through `&mut self` only, so calls into the plugin are always
// serialised. CLAP lets audio-thread functions run on any one thread at a time; SoundCraft creates
// instances on the engine/UI thread and then moves them into the mix engine (documented
// limitation: some main-thread calls such as `activate` may then happen on the audio thread).
unsafe impl Send for Instance {}

impl Instance {
    fn vt(&self) -> clap_plugin {
        // SAFETY: `plugin` is non-null (checked at creation) and alive until `Drop`.
        unsafe { *self.plugin }
    }

    fn extension<T>(&self, id: &CStr) -> *const T {
        match self.vt().get_extension {
            // SAFETY: valid instance; `id` is a static NUL-terminated string.
            Some(f) => unsafe { f(self.plugin, id.as_ptr()) }.cast::<T>(),
            None => std::ptr::null(),
        }
    }

    pub fn state(&self) -> &HostState {
        &self.state
    }

    /// The editor link shared with editor handles (`None` when the plugin has no `clap.gui`).
    pub fn gui(&self) -> Option<Arc<GuiLink>> {
        self.gui.available().then(|| Arc::clone(&self.gui))
    }

    /// The same link, for state handles (`None` when the plugin has no `clap.state`).
    pub fn state_link(&self) -> Option<Arc<GuiLink>> {
        (!self.state_ext.is_null()).then(|| Arc::clone(&self.gui))
    }

    /// `clap_plugin_state.load` from memory. `false` without the extension or when refused.
    pub fn load_state(&self, data: &[u8]) -> bool {
        // SAFETY: the extension pointer, when non-null, points to a static vtable in the plugin.
        let Some(load) = (unsafe { self.state_ext.as_ref() }).and_then(|s| s.load) else { return false };
        let mut cursor = ReadCursor { data, pos: 0 };
        let stream = clap_istream { ctx: (&mut cursor as *mut ReadCursor<'_>).cast::<c_void>(), read: Some(istream_read) };
        // SAFETY: valid instance; the stream and its cursor live on this frame for the call.
        unsafe { load(self.plugin, &stream) }
    }

    /// Runs `on_main_thread` if the plugin asked for a callback.
    pub fn service_callback(&mut self) {
        if self.state.callback.swap(false, Ordering::Relaxed)
            && let Some(f) = self.vt().on_main_thread
        {
            // SAFETY: valid instance; the plugin asked for this call via `request_callback`.
            unsafe { f(self.plugin) };
        }
    }

    pub fn params(&self) -> Vec<RawParam> {
        // SAFETY: the extension pointer, when non-null, points to a static vtable in the plugin.
        let Some(p) = (unsafe { self.params.as_ref() }) else { return Vec::new() };
        let (Some(count), Some(get_info)) = (p.count, p.get_info) else { return Vec::new() };
        // SAFETY: vtable call with the instance it belongs to.
        let n = unsafe { count(self.plugin) }.min(MAX_PARAMS);
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: `clap_param_info` is plain data (integers, floats, char arrays, a nullable
            // pointer); all-zero is a valid value.
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is a valid, writable struct.
            if !unsafe { get_info(self.plugin, i, &mut info) } {
                continue;
            }
            let (min, max) = (info.min_value, info.max_value);
            if !(min.is_finite() && max.is_finite()) || max < min {
                continue;
            }
            let default = if info.default_value.is_finite() { info.default_value.clamp(min, max) } else { min };
            out.push(RawParam { id: info.id, name: read_name(&info.name), module: read_name(&info.module), flags: info.flags, min, max, default });
        }
        out
    }

    pub fn param_value(&self, id: u32) -> Option<f64> {
        // SAFETY: see `params`.
        let p = unsafe { self.params.as_ref() }?;
        let get = p.get_value?;
        let mut v = 0.0f64;
        // SAFETY: valid instance; `v` is writable.
        (unsafe { get(self.plugin, id, &mut v) } && v.is_finite()).then_some(v)
    }

    pub fn value_text(&self, id: u32, value: f64) -> Option<String> {
        // SAFETY: see `params`.
        let p = unsafe { self.params.as_ref() }?;
        let f = p.value_to_text?;
        let mut buf = [0 as c_char; 128];
        // SAFETY: valid instance; we pass the buffer's true capacity.
        let ok = unsafe { f(self.plugin, id, value, buf.as_mut_ptr(), buf.len() as u32) };
        ok.then(|| read_name(&buf)).filter(|s| !s.is_empty())
    }

    /// The plugin's audio ports in one direction. A layout beyond `MAX_PORTS` ports or
    /// `MAX_PORT_CHANNELS` channels per port is an error: we never hand a plugin fewer ports or
    /// channels than it advertised.
    pub fn audio_ports(&self, is_input: bool) -> Result<Vec<PortInfo>, ClapError> {
        // SAFETY: see `params`.
        let Some(p) = (unsafe { self.ports.as_ref() }) else { return Ok(Vec::new()) };
        let (Some(count), Some(get)) = (p.count, p.get) else { return Ok(Vec::new()) };
        let dir = if is_input { "input" } else { "output" };
        // SAFETY: vtable call with its instance.
        let n = unsafe { count(self.plugin, is_input) };
        if n > MAX_PORTS {
            return Err(ClapError::Process(format!("{n} audio {dir} ports (at most {MAX_PORTS} supported)")));
        }
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: plain data; all-zero (null port_type pointer) is valid.
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is writable.
            if unsafe { get(self.plugin, i, is_input, &mut info) } {
                if info.channel_count > MAX_PORT_CHANNELS {
                    return Err(ClapError::Process(format!(
                        "audio {dir} port {i} has {} channels (at most {MAX_PORT_CHANNELS} supported)",
                        info.channel_count
                    )));
                }
                out.push(PortInfo { channels: info.channel_count, main: info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 });
            } else {
                // Keep port indices aligned: an unreadable port still needs a (silent) buffer slot.
                out.push(PortInfo { channels: 0, main: false });
            }
        }
        Ok(out)
    }

    pub fn note_port(&self) -> Option<NotePort> {
        // SAFETY: see `params`.
        let p = unsafe { self.notes.as_ref() }?;
        let (count, get) = (p.count?, p.get?);
        // SAFETY: vtable call with its instance.
        if unsafe { count(self.plugin, true) } == 0 {
            return None;
        }
        // SAFETY: plain data; all-zero is valid.
        let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
        // SAFETY: index 0 is below the non-zero count; `info` is writable.
        unsafe { get(self.plugin, 0, true, &mut info) }.then_some(NotePort { supported: info.supported_dialects, preferred: info.preferred_dialect })
    }

    pub fn latency(&self) -> u32 {
        // SAFETY: see `params`.
        match unsafe { self.latency.as_ref() }.and_then(|l| l.get) {
            // SAFETY: valid instance.
            Some(f) => unsafe { f(self.plugin) },
            None => 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<(), ClapError> {
        self.deactivate();
        let f = self.vt().activate.ok_or_else(|| ClapError::Process("plugin has no activate".into()))?;
        // SAFETY: valid, inactive instance; arguments are sane (checked by the caller).
        if !unsafe { f(self.plugin, sample_rate, min_frames, max_frames) } {
            return Err(ClapError::Process("activate failed".into()));
        }
        self.active = true;
        self.max_frames = max_frames as usize;
        Ok(())
    }

    pub fn deactivate(&mut self) {
        self.stop_processing();
        if self.active {
            if let Some(f) = self.vt().deactivate {
                // SAFETY: valid, active instance.
                unsafe { f(self.plugin) };
            }
            self.active = false;
        }
    }

    fn stop_processing(&mut self) {
        if self.processing {
            if let Some(f) = self.vt().stop_processing {
                // SAFETY: valid instance in the processing state.
                unsafe { f(self.plugin) };
            }
            self.processing = false;
        }
    }

    pub fn reset(&mut self) {
        if self.active
            && let Some(f) = self.vt().reset
        {
            // SAFETY: valid, active instance.
            unsafe { f(self.plugin) };
        }
    }

    /// Runs one `clap_process` call over `frames` frames. Events must be sorted by time; their
    /// times are made relative to `offset`. Returns the CLAP process status.
    pub fn process(
        &mut self,
        frames: usize,
        steady_time: i64,
        inputs: &mut PortBuffers,
        outputs: &mut PortBuffers,
        events: &[Event],
        offset: u32,
        transport: Option<&Transport>,
    ) -> Result<i32, ClapError> {
        if !self.active {
            return Err(ClapError::Process("not active".into()));
        }
        if frames == 0 || frames > self.max_frames || frames > inputs.frames || frames > outputs.frames {
            return Err(ClapError::Process("block larger than the activated size".into()));
        }
        if !self.processing {
            let ok = match self.vt().start_processing {
                // SAFETY: valid, active instance.
                Some(f) => unsafe { f(self.plugin) },
                None => true,
            };
            if !ok {
                return Err(ClapError::Process("start_processing failed".into()));
            }
            self.processing = true;
        }
        let process = self.vt().process.ok_or_else(|| ClapError::Process("plugin has no process".into()))?;
        self.raw.clear();
        for e in events.iter().take(self.raw.capacity()) {
            self.raw.push(RawEvent::from_event(e, offset));
        }
        inputs.refresh();
        outputs.refresh();
        let in_events = clap_input_events {
            ctx: (&self.raw as *const Vec<RawEvent>).cast_mut().cast::<c_void>(),
            size: Some(in_events_size),
            get: Some(in_events_get),
        };
        self.out_params.clear();
        let out_events =
            clap_output_events { ctx: (&mut self.out_params as *mut Vec<(u32, f64)>).cast::<c_void>(), try_push: Some(out_events_try_push) };
        let transport = transport.map(transport_event);
        let p = clap_process {
            steady_time,
            frames_count: u32::try_from(frames).unwrap_or(0),
            transport: transport.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
            audio_inputs: inputs.bufs.as_ptr(),
            audio_outputs: outputs.bufs.as_mut_ptr(),
            audio_inputs_count: u32::try_from(inputs.bufs.len()).unwrap_or(0),
            audio_outputs_count: u32::try_from(outputs.bufs.len()).unwrap_or(0),
            in_events: &in_events,
            out_events: &out_events,
        };
        // SAFETY: valid, processing instance. Every buffer pointer refers to `frames` (or more)
        // valid f32s owned by `inputs`/`outputs`, which are borrowed for the whole call; the event
        // lists and their contexts and the transport live on this stack frame / in `self.raw`,
        // untouched until the call returns.
        let status = unsafe { process(self.plugin, &p) };
        if !self.out_params.is_empty()
            && let Ok(mut edits) = self.state.edits.try_lock()
        {
            // Never blocks or grows: a busy UI or a full queue drops the edits.
            for &e in &self.out_params {
                if edits.len() < edits.capacity() {
                    edits.push(e);
                }
            }
        }
        self.out_params.clear();
        if status == CLAP_PROCESS_ERROR { Err(ClapError::Process("plugin reported a processing error".into())) } else { Ok(status) }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Editors first (CLAP: destroy the GUI before the plugin), and no handle may call in after.
        self.gui.kill();
        self.deactivate();
        if self.created
            && let Some(f) = self.vt().destroy
        {
            // SAFETY: valid instance, deactivated above; destroyed exactly once.
            unsafe { f(self.plugin) };
        }
        self.created = false;
    }
}

// ---- host window for embedded editors ------------------------------------------------------

/// The Windows host window for embedded editors: a top-level window owned by the app's active
/// window (no taskbar button) whose client area is the plugin's parent `HWND`. Closing it only
/// hides it; the editor link notices in `idle`, destroys the plugin's GUI, then the window.
/// Messages are dispatched by the app's own event loop on the main thread.
#[cfg(windows)]
mod hostwin {
    use super::{GuiSizer, sizing_rect};
    use std::ffi::c_void;
    use std::sync::{Arc, OnceLock};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWL_STYLE,
        GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, GetWindowRect, IDC_ARROW, IsWindow, IsWindowVisible, LoadCursorW, RegisterClassExW,
        SIZE_MINIMIZED, SW_HIDE, SW_SHOW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CLOSE, WM_NCDESTROY, WM_SIZE, WM_SIZING, WNDCLASSEXW, WS_CAPTION, WS_CLIPCHILDREN,
        WS_OVERLAPPED, WS_SYSMENU, WS_THICKFRAME,
    };

    pub const SUPPORTED: bool = true;

    /// Rust names the process's first thread "main"; the app runs its UI (and message loop) there.
    fn on_main_thread() -> bool {
        std::thread::current().name() == Some("main")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().filter(|&c| c != 0).chain(Some(0)).collect()
    }

    const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;

    /// The sizer stored in the window's user data by `set_sizer` (one `Arc` reference, released
    /// on `WM_NCDESTROY`), borrowed for one message.
    fn window_sizer(hwnd: HWND) -> Option<Arc<GuiSizer>> {
        // SAFETY: `hwnd` is the window the procedure was called for; its user data is 0 or a
        // pointer from `Arc::into_raw`, kept alive until `WM_NCDESTROY` (the last message).
        let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const GuiSizer;
        if p.is_null() {
            return None;
        }
        // SAFETY: as above; this message takes a reference of its own, so the sizer outlives it
        // even when a plugin destroys the window from a nested event loop meanwhile.
        unsafe { Arc::increment_strong_count(p) };
        // SAFETY: balances the increment just made.
        Some(unsafe { Arc::from_raw(p) })
    }

    /// The window's frame (outer minus client size), measured: right at any monitor's DPI, unlike
    /// `AdjustWindowRectEx` (system DPI). `None` when the window can't be measured.
    fn frame_size(hwnd: HWND) -> Option<(i32, i32)> {
        let (mut w, mut c) = (RECT { left: 0, top: 0, right: 0, bottom: 0 }, RECT { left: 0, top: 0, right: 0, bottom: 0 });
        // SAFETY: a window of this thread (or a stale handle, which fails); both rects are writable.
        if unsafe { GetWindowRect(hwnd, &mut w) } == 0 || unsafe { GetClientRect(hwnd, &mut c) } == 0 {
            return None;
        }
        let fw = w.right.saturating_sub(w.left).saturating_sub(c.right.saturating_sub(c.left));
        let fh = w.bottom.saturating_sub(w.top).saturating_sub(c.bottom.saturating_sub(c.top));
        Some((fw.max(0), fh.max(0)))
    }

    /// Outer size for a client area of `w`×`h`: measured frame, else `AdjustWindowRectEx`.
    fn outer_for(hwnd: HWND, w: f64, h: f64) -> (i32, i32) {
        match frame_size(hwnd) {
            Some((fw, fh)) => ((w as i32).saturating_add(fw), (h as i32).saturating_add(fh)),
            None => {
                // SAFETY: a window of this thread; style values are only read.
                let (style, ex) = unsafe { (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32, GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) };
                outer_size(style, ex, w, h)
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_CLOSE => {
                // SAFETY: `hwnd` is the window this procedure was called for.
                unsafe { ShowWindow(hwnd, SW_HIDE) };
                return 0;
            }
            // Live user resizes: keep the dragged edges at a size the plugin accepts.
            WM_SIZING => {
                // SAFETY: for WM_SIZING, `lparam` points to the window's writable drag rect.
                if let (Some(s), Some(r), Some((fw, fh))) = (window_sizer(hwnd), unsafe { (lparam as *mut RECT).as_mut() }, frame_size(hwnd)) {
                    let (w, h) = (r.right.saturating_sub(r.left).saturating_sub(fw), r.bottom.saturating_sub(r.top).saturating_sub(fh));
                    if let Some((cw, ch)) = s.constrain(f64::from(w.max(1)), f64::from(h.max(1))) {
                        let (ow, oh) = ((cw as i32).saturating_add(fw), (ch as i32).saturating_add(fh));
                        (r.left, r.top, r.right, r.bottom) = sizing_rect(wparam, (r.left, r.top, r.right, r.bottom), (ow, oh));
                        return 1;
                    }
                }
            }
            // Every completed resize: settle on the adjusted size and tell the plugin.
            WM_SIZE if wparam != SIZE_MINIMIZED as usize => {
                if let Some(s) = window_sizer(hwnd) {
                    let (w, h) = (f64::from((lparam & 0xFFFF) as u16), f64::from(((lparam >> 16) & 0xFFFF) as u16));
                    s.user_resized(w, h, |cw, ch| resize(hwnd, cw, ch));
                }
            }
            WM_NCDESTROY => {
                // SAFETY: the window's last message: take the user data back and release the
                // `Arc` reference `set_sizer` stored there; nothing reads it afterwards.
                let p = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *const GuiSizer;
                if !p.is_null() {
                    // SAFETY: `p` came from `Arc::into_raw` in `set_sizer`, released exactly once.
                    drop(unsafe { Arc::from_raw(p) });
                }
            }
            _ => {}
        }
        // SAFETY: default handling of a message for the window it was sent to.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Registers the window class once; the name lives for the whole process.
    fn class_name() -> Option<&'static [u16]> {
        static CLASS: OnceLock<Option<Vec<u16>>> = OnceLock::new();
        CLASS
            .get_or_init(|| {
                let name = wide("SoundCraftClapEditor");
                // SAFETY: null asks for this process's module handle.
                let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: instance,
                    hIcon: std::ptr::null_mut(),
                    // SAFETY: a system cursor (null instance + predefined id).
                    hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: name.as_ptr(),
                    hIconSm: std::ptr::null_mut(),
                };
                // SAFETY: a fully initialised class description whose strings outlive the call
                // (the name is kept for the process lifetime in this `OnceLock`).
                (unsafe { RegisterClassExW(&wc) } != 0).then_some(name)
            })
            .as_deref()
    }

    /// Outer window size for a client area of `w`×`h`.
    fn outer_size(style: u32, ex_style: u32, w: f64, h: f64) -> (i32, i32) {
        let mut r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        // SAFETY: `r` is a valid, writable rect.
        unsafe { AdjustWindowRectEx(&mut r, style, 0, ex_style) };
        (r.right.saturating_sub(r.left), r.bottom.saturating_sub(r.top))
    }

    /// Owns the window handle; destroyed in `close` (main thread only).
    pub struct HostWindow {
        hwnd: HWND,
    }

    // SAFETY: the handle is only used on the main thread (every method checks); the struct
    // merely carries it between threads' locks.
    unsafe impl Send for HostWindow {}

    impl HostWindow {
        fn live(&self) -> Option<HWND> {
            // SAFETY: `IsWindow` accepts any value and only reports whether it names a window.
            (on_main_thread() && unsafe { IsWindow(self.hwnd) } != 0).then_some(self.hwnd)
        }

        pub fn new(title: &str, w: f64, h: f64) -> Result<HostWindow, String> {
            if !on_main_thread() {
                return Err("plugin editors must be opened on the main thread".into());
            }
            let class = class_name().ok_or_else(|| "cannot register the editor window class".to_string())?;
            let title = wide(title);
            let (ow, oh) = outer_size(STYLE, 0, w, h);
            // SAFETY: main thread; the active window (or null) of this thread's queue.
            let owner = unsafe { GetActiveWindow() };
            // SAFETY: registered class and NUL-terminated strings that outlive the call; the
            // owner is null or a window of this thread.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    title.as_ptr(),
                    STYLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    ow,
                    oh,
                    owner,
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err("cannot create the editor window".into());
            }
            // `AdjustWindowRectEx` assumed the system DPI; size the client area by the real frame.
            resize(hwnd, w, h);
            Ok(HostWindow { hwnd })
        }

        /// The window handle (for resizes).
        pub fn raw(&self) -> *mut c_void {
            self.hwnd
        }

        /// Routes the window's resizes through `sizer` (the window procedure reads it from the
        /// window's user data). Once per window.
        pub fn set_sizer(&mut self, sizer: &Arc<GuiSizer>) {
            let Some(h) = self.live() else { return };
            if window_sizer(h).is_some() {
                return;
            }
            let p = Arc::into_raw(sizer.clone());
            // SAFETY: a live window of this thread; the pointer stays valid until `WM_NCDESTROY`
            // releases it.
            unsafe { SetWindowLongPtrW(h, GWLP_USERDATA, p as _) };
        }

        /// Gives the window a sizing border (the plugin said `can_resize`), or takes it away,
        /// keeping the client size.
        pub fn set_resizable(&self, resizable: bool) {
            let Some(h) = self.live() else { return };
            // SAFETY: a live window of this thread; the style is only read.
            if ((unsafe { GetWindowLongPtrW(h, GWL_STYLE) } as u32 & WS_THICKFRAME) != 0) == resizable {
                return;
            }
            let mut c = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: a live window of this thread; `c` is writable.
            unsafe { GetClientRect(h, &mut c) };
            // SAFETY: as above; only the sizing-border bit of the style changes.
            unsafe {
                let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
                let style = if resizable { style | WS_THICKFRAME } else { style & !WS_THICKFRAME };
                SetWindowLongPtrW(h, GWL_STYLE, style as _);
            }
            // SAFETY: as above; the frame is recomputed for the new style, nothing else changes.
            unsafe { SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED) };
            // The new frame took room from the client area: give it back.
            resize(h, f64::from(c.right), f64::from(c.bottom));
        }

        /// The `HWND` the plugin creates its child window in.
        pub fn parent(&self) -> *mut c_void {
            self.live().unwrap_or(std::ptr::null_mut())
        }

        pub fn raise(&self) {
            if let Some(h) = self.live() {
                // SAFETY: a live window of this thread.
                unsafe {
                    ShowWindow(h, SW_SHOW);
                    SetForegroundWindow(h);
                }
            }
        }

        pub fn is_visible(&self) -> bool {
            // SAFETY: a live window of this thread.
            self.live().is_some_and(|h| unsafe { IsWindowVisible(h) } != 0)
        }

        pub fn close(self) {
            if let Some(h) = self.live() {
                // SAFETY: our own live window; the plugin's GUI was destroyed first.
                unsafe { DestroyWindow(h) };
            }
        }
    }

    /// Resizes a host window's client area (`window` from [`HostWindow::raw`]); main thread only.
    pub fn resize(window: *mut c_void, w: f64, h: f64) {
        // SAFETY: `IsWindow` accepts any value.
        if !on_main_thread() || window.is_null() || unsafe { IsWindow(window) } == 0 {
            return;
        }
        let (ow, oh) = outer_for(window, w, h);
        // SAFETY: a live window of this thread; only the size changes.
        unsafe { SetWindowPos(window, std::ptr::null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
    }
}

/// The macOS host window for embedded editors: a floating, transient `NSWindow` whose content view
/// is the `NSView` the plugin adds its GUI's view to. Closing it only orders it out; the editor
/// link notices in `idle`, destroys the GUI, then closes the window. Modelled on the VST3 host's
/// window (same behaviour, `GuiSizer` in place of its `ViewSizer`).
#[cfg(target_os = "macos")]
mod hostwin {
    use super::GuiSizer;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSFloatingWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
    };
    use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub const SUPPORTED: bool = true;

    /// The delegate's state: the sizer, and whether the user closed the window.
    struct Ivars {
        sizer: Arc<GuiSizer>,
        closed: Arc<AtomicBool>,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `ResizeDelegate` implements no
        // `Drop` (its ivar, an `Arc`, is dropped by objc2 when the object is deallocated).
        // No `#[name]`: objc2 then generates a crate- and version-specific name and reuses an
        // already registered class of that name (e.g. a second copy of this crate in the process)
        // instead of panicking, and never collides with the VST3 host's delegate class.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = Ivars]
        struct ResizeDelegate;

        // SAFETY: NSObjectProtocol has no safety requirements.
        unsafe impl NSObjectProtocol for ResizeDelegate {}

        // SAFETY: the methods below match the NSWindowDelegate selectors' signatures.
        unsafe impl NSWindowDelegate for ResizeDelegate {
            /// Live user resizes: the window only ever takes a size the plugin accepts.
            #[unsafe(method(windowWillResize:toSize:))]
            fn window_will_resize(&self, sender: &NSWindow, frame_size: NSSize) -> NSSize {
                // Our own reference: the sizer stays alive even if the plugin closes the editor
                // from a nested event loop during this call.
                let sizer = self.ivars().sizer.clone();
                constrained_frame(&sizer, sender, NSRect::new(NSPoint::new(0.0, 0.0), frame_size)).size
            }

            /// The zoom button: zoom to the largest size the plugin accepts within the default
            /// (screen) frame, keeping the window's top edge.
            #[unsafe(method(windowWillUseStandardFrame:defaultFrame:))]
            fn window_standard_frame(&self, window: &NSWindow, default_frame: NSRect) -> NSRect {
                let sizer = self.ivars().sizer.clone();
                let mut frame = constrained_frame(&sizer, window, default_frame);
                frame.origin.y = default_frame.origin.y + default_frame.size.height - frame.size.height;
                frame
            }

            /// Every completed resize (drags, zoom): settles on an adjusted size and tells the
            /// plugin (`set_size`). Resizes the host applies itself are ignored by the sizer.
            #[unsafe(method(windowDidResize:))]
            fn window_did_resize(&self, notification: &NSNotification) {
                let sizer = self.ivars().sizer.clone();
                let Some(window) = notification.object().and_then(|o| o.downcast::<NSWindow>().ok()) else { return };
                let Some(view) = window.contentView() else { return };
                let size = view.frame().size;
                sizer.user_resized(size.width, size.height, |w, h| window.setContentSize(NSSize::new(w, h)));
            }

            /// The user closed the window (the host's own `close` clears the delegate first).
            /// Tracked here rather than read from `isVisible`, which is also false while the app
            /// is inactive and AppKit has hidden the window (`hidesOnDeactivate`).
            #[unsafe(method(windowWillClose:))]
            fn window_will_close(&self, _notification: &NSNotification) {
                self.ivars().closed.store(true, Ordering::Relaxed);
            }
        }
    );

    /// `frame` with its content size adjusted by the plugin (unchanged when nothing is
    /// negotiated); the origin stays.
    fn constrained_frame(sizer: &GuiSizer, window: &NSWindow, frame: NSRect) -> NSRect {
        let content = window.contentRectForFrameRect(frame);
        match sizer.constrain(content.size.width, content.size.height) {
            Some((w, h)) => {
                let size = window.frameRectForContentRect(NSRect::new(content.origin, NSSize::new(w, h))).size;
                NSRect::new(frame.origin, size)
            }
            None => frame,
        }
    }

    impl ResizeDelegate {
        fn new(mtm: MainThreadMarker, sizer: Arc<GuiSizer>, closed: Arc<AtomicBool>) -> Retained<ResizeDelegate> {
            let this = ResizeDelegate::alloc(mtm).set_ivars(Ivars { sizer, closed });
            // SAFETY: `init` of NSObject on a freshly allocated instance whose ivars are set.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// An `NSWindow` (titled, closable, floating above the app's windows) whose content view is
    /// the plugin GUI's parent. Holds one retain of the window; main thread only.
    pub struct HostWindow {
        window: *mut NSWindow,
        /// The window's delegate (one retain; the window only keeps a weak reference), or null.
        delegate: *mut ResizeDelegate,
        /// Set by the delegate when the user closes the window.
        closed: Arc<AtomicBool>,
    }

    // SAFETY: the pointers are only dereferenced on the main thread (every method checks
    // `MainThreadMarker`); the struct merely carries the retains between threads' locks.
    unsafe impl Send for HostWindow {}

    impl HostWindow {
        fn get(&self) -> Option<&NSWindow> {
            MainThreadMarker::new()?;
            // SAFETY: we hold a retain of this window (from `new`) until `close`.
            unsafe { self.window.as_ref() }
        }

        pub fn new(title: &str, w: f64, h: f64) -> Result<HostWindow, String> {
            let mtm = MainThreadMarker::new().ok_or_else(|| "plugin editors must be opened on the main thread".to_string())?;
            let _app = NSApplication::sharedApplication(mtm);
            let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(w, h));
            // Plugin editors are transient windows. In particular they must not be minimizable:
            // minimized NSWindows become persistent thumbnails in the macOS Dock.
            let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
            // SAFETY: a freshly allocated window on the main thread; valid rect and style.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false)
            };
            // SAFETY: we keep our own retain and release it in `close`, so AppKit must not
            // release the window when the user closes it.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.setExcludedFromWindowsMenu(true);
            window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
            window.center();
            Ok(HostWindow { window: Retained::into_raw(window), delegate: std::ptr::null_mut(), closed: Arc::new(AtomicBool::new(false)) })
        }

        /// The raw window pointer (for resizes).
        pub fn raw(&self) -> *mut c_void {
            self.window.cast()
        }

        /// Routes the window's resizes through `sizer` (an `NSWindowDelegate`). Once per window.
        pub fn set_sizer(&mut self, sizer: &Arc<GuiSizer>) {
            let (Some(mtm), Some(w)) = (MainThreadMarker::new(), self.get()) else { return };
            if !self.delegate.is_null() {
                return;
            }
            let delegate = ResizeDelegate::new(mtm, sizer.clone(), self.closed.clone());
            w.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            self.delegate = Retained::into_raw(delegate);
        }

        /// Gives the window a resize handle (the plugin said `can_resize`), or takes it away,
        /// keeping the content size.
        pub fn set_resizable(&self, resizable: bool) {
            let Some(w) = self.get() else { return };
            let style = w.styleMask();
            let style = if resizable { style | NSWindowStyleMask::Resizable } else { style & !NSWindowStyleMask::Resizable };
            if style == w.styleMask() {
                return;
            }
            let size = w.contentView().map(|v| v.frame().size);
            w.setStyleMask(style);
            // A resizable window gets a zoom button; keep it out of full screen (the editor is a
            // transient panel, and the sizer would fight a full-screen size the plugin refuses).
            if resizable {
                w.setCollectionBehavior(w.collectionBehavior() | NSWindowCollectionBehavior::FullScreenNone);
            }
            if let Some(size) = size {
                w.setContentSize(size);
            }
        }

        /// The content view (`NSView*`) the plugin adds its view to; null when unavailable.
        pub fn parent(&self) -> *mut c_void {
            self.get().and_then(|w| w.contentView()).map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v).cast_mut().cast())
        }

        pub fn raise(&self) {
            if let Some(w) = self.get() {
                w.makeKeyAndOrderFront(None);
            }
        }

        /// The window is open: not closed by the user. (Before the delegate is set, the window's
        /// own visibility.)
        pub fn is_visible(&self) -> bool {
            self.get().is_some_and(|w| if self.delegate.is_null() { w.isVisible() } else { !self.closed.load(Ordering::Relaxed) })
        }

        pub fn close(self) {
            if let Some(w) = self.get() {
                w.setDelegate(None);
                w.orderOut(None);
                w.close();
            }
            if MainThreadMarker::new().is_some() {
                // SAFETY: balances the retain taken in `new`; nothing uses the pointer after this.
                drop(unsafe { Retained::from_raw(self.window) });
                // SAFETY: null, or balances the retain taken in `set_sizer`; the window no longer
                // refers to it (delegate cleared above).
                if let Some(d) = unsafe { Retained::from_raw(self.delegate) } {
                    // Released when the event loop's autorelease pool drains, not now: `close` may
                    // run inside one of the delegate's own methods (a plugin pumping a nested loop).
                    let _ = Retained::autorelease_ptr(d);
                }
            }
        }
    }

    /// Resizes a host window's content (`window` from [`HostWindow::raw`]); main thread only.
    pub fn resize(window: *mut c_void, w: f64, h: f64) {
        if MainThreadMarker::new().is_none() {
            return;
        }
        // SAFETY: null or a window the editor link keeps retained while the GUI is embedded.
        if let Some(win) = unsafe { window.cast::<NSWindow>().as_ref() } {
            win.setContentSize(NSSize::new(w, h));
        }
    }
}

/// Other platforms: embedded editors are not hosted (floating ones still are).
#[cfg(not(any(windows, target_os = "macos")))]
mod hostwin {
    use std::ffi::c_void;

    pub const SUPPORTED: bool = false;

    pub struct HostWindow;

    impl HostWindow {
        pub fn new(_title: &str, _w: f64, _h: f64) -> Result<HostWindow, String> {
            Err("embedded plugin editors are not hosted on this platform".into())
        }
        pub fn raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn set_sizer(&mut self, _sizer: &std::sync::Arc<super::GuiSizer>) {}
        pub fn set_resizable(&self, _resizable: bool) {}
        pub fn parent(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn raise(&self) {}
        pub fn is_visible(&self) -> bool {
            false
        }
        pub fn close(self) {}
    }

    pub fn resize(_window: *mut c_void, _w: f64, _h: f64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_size` calls of the fake plugin below (one test uses it).
    static SET_SIZES: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
    /// When set, the fake `set_size` calls back into the sizer, the way a plugin resizing its
    /// parent from `set_size` makes the window notify the host again.
    static NESTED: Mutex<Option<Arc<GuiSizer>>> = Mutex::new(None);
    /// When set, the fake `set_size` closes the editor from inside the call (takes the target
    /// away), the way a plugin pumping a nested event loop can.
    static CLOSE_INSIDE: Mutex<Option<Arc<GuiSizer>>> = Mutex::new(None);
    /// The fake `set_size` refuses every size.
    static REFUSE: AtomicBool = AtomicBool::new(false);

    /// Accepts 200..=800 × 100..=600.
    unsafe extern "C" fn fake_adjust(_p: *const clap_plugin, w: *mut u32, h: *mut u32) -> bool {
        // SAFETY: the sizer passes valid out-pointers.
        unsafe {
            *w = (*w).clamp(200, 800);
            *h = (*h).clamp(100, 600);
        }
        true
    }

    unsafe extern "C" fn fake_set_size(_p: *const clap_plugin, w: u32, h: u32) -> bool {
        SET_SIZES.lock().unwrap().push((w, h));
        if let Some(s) = NESTED.lock().unwrap().clone() {
            s.user_resized(999.0, 999.0, |_, _| panic!("a nested notification is not negotiated"));
        }
        if let Some(s) = CLOSE_INSIDE.lock().unwrap().take() {
            s.set_target(None, false);
        }
        !REFUSE.load(Ordering::Relaxed)
    }

    #[test]
    fn user_resizes_are_negotiated_with_the_plugin_without_loops() {
        let target = SizeTarget { plugin: std::ptr::null(), adjust_size: Some(fake_adjust), set_size: Some(fake_set_size) };
        let sizes = || SET_SIZES.lock().unwrap().clone();
        let sizer = Arc::new(GuiSizer::new(320, 240));
        // `can_resize` false: nothing is negotiated.
        sizer.set_target(Some(target), false);
        assert_eq!(sizer.constrain(500.0, 400.0), None);
        sizer.user_resized(500.0, 400.0, |_, _| panic!("not resizable"));
        assert!(sizes().is_empty());

        sizer.set_target(Some(target), true);
        assert_eq!(sizer.constrain(50.0, 5000.0), Some((200.0, 600.0)), "adjust_size shapes the drag");
        let mut set = None;
        sizer.user_resized(1000.0, 50.0, |w, h| set = Some((w, h)));
        assert_eq!(set, Some((800.0, 100.0)), "the window follows the adjusted size");
        assert_eq!(sizes(), vec![(800, 100)]);
        sizer.user_resized(640.0, 480.0, |_, _| panic!("accepted as is"));
        sizer.user_resized(640.0, 480.0, |_, _| panic!("accepted as is"));
        assert_eq!(sizes(), vec![(800, 100), (640, 480)], "the same size is not repeated");

        // A plugin `request_resize` being applied: its window notifications are ignored.
        {
            let _applying = sizer.applying();
            assert_eq!(sizer.constrain(300.0, 300.0), None);
            sizer.user_resized(300.0, 300.0, |_, _| panic!("ignored while applying"));
            sizer.set_last(300, 300);
        }
        sizer.user_resized(300.0, 300.0, |_, _| panic!("accepted as is"));
        assert_eq!(sizes().len(), 2);

        // A plugin that resizes its parent from `set_size` doesn't start a loop (or deadlock).
        *NESTED.lock().unwrap() = Some(sizer.clone());
        sizer.user_resized(400.0, 400.0, |_, _| panic!("accepted as is"));
        *NESTED.lock().unwrap() = None;
        assert_eq!(sizes(), vec![(800, 100), (640, 480), (400, 400)]);

        // A plugin that refuses the size: the window goes back to the last accepted one.
        REFUSE.store(true, Ordering::Relaxed);
        let mut set = None;
        sizer.user_resized(500.0, 500.0, |w, h| set = Some((w, h)));
        REFUSE.store(false, Ordering::Relaxed);
        assert_eq!(set, Some((400.0, 400.0)));
        assert_eq!(sizer.last(), (400, 400));
        SET_SIZES.lock().unwrap().pop();

        // Closing the editor from inside `set_size` neither deadlocks nor leaves the plugin
        // reachable: the removal is applied when the call returns.
        *CLOSE_INSIDE.lock().unwrap() = Some(sizer.clone());
        sizer.user_resized(450.0, 450.0, |_, _| panic!("accepted as is"));
        assert!(!sizer.resizable());
        assert!(sizer.target.lock().unwrap().is_none());
        sizer.set_target(Some(target), true);

        // A plugin without `adjust_size` gets the user's size as is.
        sizer.set_target(Some(SizeTarget { adjust_size: None, ..target }), true);
        assert_eq!(sizer.constrain(5000.0, 10.0), Some((5000.0, 10.0)));

        // Once the GUI goes, nothing reaches the plugin any more.
        sizer.set_target(None, true);
        assert!(!sizer.resizable());
        sizer.user_resized(500.0, 500.0, |_, _| panic!("no plugin"));
        assert_eq!(sizes().len(), 4);
    }

    #[test]
    fn sizing_moves_only_the_dragged_edges() {
        let r = (100, 100, 500, 400);
        assert_eq!(sizing_rect(8, r, (300, 200)), (100, 100, 400, 300), "bottom-right");
        assert_eq!(sizing_rect(4, r, (300, 200)), (200, 200, 500, 400), "top-left");
        assert_eq!(sizing_rect(5, r, (300, 200)), (100, 200, 400, 400), "top-right");
        assert_eq!(sizing_rect(7, r, (300, 200)), (200, 100, 500, 300), "bottom-left");
        assert_eq!(sizing_rect(2, (i32::MAX - 1, 0, i32::MAX, 10), (100, 10)), (i32::MAX - 1, 0, i32::MAX, 10), "saturates");
    }

    #[test]
    fn the_transport_reaches_clap_in_fixed_point() {
        let t = Transport {
            playing: true,
            tempo: 90.0,
            numerator: 3,
            denominator: 4,
            samples: 96_000,
            seconds: 2.0,
            quarters: 3.5,
            bar_start: 3.0,
            bar: 1,
            ..Transport::default()
        };
        let e = transport_event(&t);
        assert_eq!((e.header.type_, e.header.size as usize), (CLAP_EVENT_TRANSPORT, size_of::<clap_event_transport>()));
        assert_eq!(e.flags & CLAP_TRANSPORT_IS_PLAYING, CLAP_TRANSPORT_IS_PLAYING);
        assert_eq!(e.flags & CLAP_TRANSPORT_IS_RECORDING, 0);
        assert_eq!((e.song_pos_beats, e.bar_start), (7 * CLAP_BEATTIME_FACTOR / 2, 3 * CLAP_BEATTIME_FACTOR));
        assert_eq!(e.song_pos_seconds, 2 * CLAP_SECTIME_FACTOR);
        assert_eq!((e.tempo, e.bar_number, e.tsig_num, e.tsig_denom), (90.0, 1, 3, 4));
        let nan = transport_event(&Transport { quarters: f64::NAN, ..t });
        assert_eq!(nan.song_pos_beats, 0);
    }
}
