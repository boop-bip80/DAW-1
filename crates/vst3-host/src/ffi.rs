//! The FFI boundary: the only module of this crate allowed to use `unsafe`.
//!
//! It loads VST3 binaries (`libloading`), runs the platform module entry, calls the COM
//! interfaces of the factory, component, audio processor and edit controller (`vst3` raw
//! bindings), and implements the host-side COM objects (host context, parameter changes, event
//! lists, a memory stream). Everything it exposes to the rest of the crate is a safe API.
//!
//! Soundness rests on the VST3 contract (the plugin honours the COM vtable signatures and the
//! documented pointer lifetimes). A plugin that violates it can still crash the process: hosting
//! in-process can't prevent that. What this module guarantees is that *SoundCraft* never hands
//! the plugin dangling or mis-sized pointers and never reads plugin strings beyond their bounds.

// VST3 enum constants are `c_int` on Windows and `c_uint` elsewhere, so casts that are no-ops on
// one platform are needed on another.
#![allow(clippy::unnecessary_cast)]

use crate::{Vst3Error, scan};
use soundcraft_dsp::Transport;
use std::ffi::{c_char, c_int, c_void};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, TryLockError, mpsc};
use std::time::Duration;
use vst3::Steinberg::Vst::{self as sv};
use vst3::Steinberg::Vst::{
    IAudioProcessorTrait, IComponentHandlerTrait, IComponentTrait, IConnectionPointTrait, IEditControllerTrait, IEventListTrait,
    IHostApplicationTrait, IMidiMappingTrait, IParamValueQueueTrait, IParameterChangesTrait,
};
use vst3::Steinberg::{self as sb};
use vst3::Steinberg::{FUnknown, IBStreamTrait, IPlugFrameTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory2Trait, IPluginFactoryTrait};
use vst3::{Class, ComPtr, ComWrapper, Interface};

/// Most classes one factory may report (hostile-input cap).
const MAX_CLASSES: i32 = 1024;
/// Most parameters read from one controller.
pub(crate) const MAX_PARAMS: i32 = 4096;
/// Most audio buses per direction we provide buffers for.
const MAX_BUSES: i32 = 16;
/// Most channels per audio bus.
const MAX_BUS_CHANNELS: u32 = 64;
/// Capacity of the per-block event lists (preallocated: `process` never allocates).
pub(crate) const EVENT_CAP: usize = 1024;
/// Most parameter queues per block (preallocated).
pub(crate) const PARAM_QUEUE_CAP: usize = 1024;
/// Largest plugin state we buffer (component or controller state).
const MAX_STATE_BYTES: usize = 64 << 20;
/// Most editor parameter edits buffered between two UI frames.
const EDIT_CAP: usize = 256;
/// Magic of SoundCraft's state container: `SCV3` + u32 LE component length + component state +
/// u32 LE controller length + controller state.
const STATE_MAGIC: &[u8; 4] = b"SCV3";

// ---- strings -------------------------------------------------------------------------------

/// Reads a fixed-size `char8[N]` buffer, stopping at the first NUL or the end of the array.
fn read_c8(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Reads a fixed-size UTF-16 `String128`, stopping at the first NUL or the end of the array.
fn read_c16(buf: &[u16]) -> String {
    let units: Vec<u16> = buf.iter().take_while(|c| **c != 0).copied().collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// Writes `s` into a UTF-16 buffer, truncated and always NUL-terminated.
fn write_c16(s: &str, dst: &mut [u16]) {
    let Some(room) = dst.len().checked_sub(1) else { return };
    let mut n = 0;
    for (d, u) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = u;
        n += 1;
    }
    if let Some(z) = dst.get_mut(n) {
        *z = 0;
    }
}

fn ok(r: sb::tresult) -> bool {
    r == sb::kResultOk
}

/// VST3's numbers for the MIDI controllers that aren't CCs.
pub(crate) const AFTERTOUCH: usize = sv::ControllerNumbers_::kAfterTouch as usize;
pub(crate) const PITCH_BEND: usize = sv::ControllerNumbers_::kPitchBend as usize;

// ---- loaded binaries -----------------------------------------------------------------------

/// One class as reported by the factory.
#[derive(Debug, Clone)]
pub(crate) struct RawClass {
    pub cid: [u8; 16],
    pub category: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub sdk_version: String,
    pub sub_categories: String,
}

/// A loaded VST3 binary and its plugin factory. Loaded binaries are cached for the life of the
/// process (see [`Bundle::load`]): plugin code must never be unmapped while an instance may run,
/// and VST3 modules are not required to support being re-entered after their exit function, which
/// therefore runs once, at process exit (see [`exit_modules`]).
pub(crate) struct Bundle {
    factory: ComPtr<sb::IPluginFactory>,
    /// The module exit matching the entry that ran at load (`bundleExit`, `ExitDll`, `ModuleExit`).
    exit: Option<ModuleExit>,
    /// The thread that ran the module entry, and so must run the exit.
    entered_on: EntryThread,
    /// Set when plugin objects were leaked on purpose (see [`EditorLink::kill`]): the module exit
    /// must not run while any of its objects are alive.
    leaked: AtomicBool,
    // Never closed: code stays mapped for the process lifetime, even after the module exit.
    _lib: ManuallyDrop<libloading::Library>,
}

/// Loaded binaries, oldest first (module exits run newest first, the reverse of loading).
fn loaded() -> &'static Mutex<Vec<(PathBuf, Arc<Bundle>)>> {
    static L: OnceLock<Mutex<Vec<(PathBuf, Arc<Bundle>)>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set once the module exits have run: loading another module after that is refused.
static MODULES_EXITED: AtomicBool = AtomicBool::new(false);

/// Where a module's entry ran. Its exit must run on the same thread: Qt-based modules tie their
/// application object to it (Native Instruments' Massive X crashes in `bundleExit` on another
/// thread). Modules wanted on the main thread are entered there; any other thread's requests go
/// to the module thread, because the asking thread may be gone by the time the process exits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryThread {
    Main,
    Modules,
}

/// Whether this is the main thread. Not for exit handlers, where `std::thread::current` panics
/// (its thread-local data is gone by then): compare [`os_thread`] with [`MAIN_OS_THREAD`] there.
fn on_main_thread() -> bool {
    std::thread::current().name() == Some("main")
}

/// The OS id of the main thread, recorded when it loads a module (0 until then).
static MAIN_OS_THREAD: AtomicUsize = AtomicUsize::new(0);

/// The OS id of the calling thread, which (unlike `std::thread::current`) exit handlers can use.
fn os_thread() -> usize {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn pthread_self() -> usize;
        }
        // SAFETY: `pthread_self` takes no arguments and cannot fail; `pthread_t` is an integer or
        // a pointer of pointer size on the Unix systems SoundCraft builds for.
        unsafe { pthread_self() }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        // SAFETY: `GetCurrentThreadId` takes no arguments and cannot fail.
        unsafe { GetCurrentThreadId() as usize }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// The module thread (started on first use): it runs jobs until the process exits.
fn module_thread() -> Option<&'static mpsc::Sender<Job>> {
    static TX: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("vst3-modules".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
        });
        spawned.ok().map(|_| tx)
    })
    .as_ref()
}

/// Runs `f` on the module thread and waits for its result (at most `timeout`, when given).
fn on_module_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static, timeout: Option<Duration>) -> Option<R> {
    let (rtx, rrx) = mpsc::sync_channel(1);
    module_thread()?
        .send(Box::new(move || {
            let _ = rtx.send(f());
        }))
        .ok()?;
    match timeout {
        Some(t) => rrx.recv_timeout(t).ok(),
        None => rrx.recv().ok(),
    }
}

type GetFactory = unsafe extern "system" fn() -> *mut sb::IPluginFactory;
/// `bundleExit` (macOS), `ExitDll` (Windows) or `ModuleExit` (Linux/BSD). `extern "system"` is the
/// C ABI everywhere except 32-bit Windows, where it is the `PLUGIN_API` (stdcall) `ExitDll` uses.
type ModuleExit = unsafe extern "system" fn() -> bool;

#[cfg(target_os = "macos")]
mod cf {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, is_dir: u8) -> *const c_void;
        pub fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *mut c_void;
        pub fn CFRelease(cf: *const c_void);
    }
}

/// Opens the shared library and runs the platform module entry (`bundleEntry` with a
/// `CFBundleRef` on macOS, `InitDll` on Windows, `ModuleEntry` with the `dlopen` handle on Linux).
/// Also returns the matching module exit, when the entry ran and the module exports one.
fn open_library(bundle: &Path, exe: &Path) -> Result<(libloading::Library, Option<ModuleExit>), String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use libloading::os::unix::{Library as UnixLib, RTLD_LOCAL, RTLD_NOW};
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in VST3 plugin folders or named explicitly.
        let lib = unsafe { UnixLib::open(Some(exe), RTLD_NOW | RTLD_LOCAL) }.map_err(|e| e.to_string())?;
        type ModuleEntry = unsafe extern "C" fn(*mut c_void) -> bool;
        // SAFETY: `ModuleEntry` is the VST3-mandated Linux entry with this signature.
        let entry = unsafe { lib.get::<ModuleEntry>(b"ModuleEntry\0") }.ok().map(|s| *s);
        let raw = lib.into_raw();
        // SAFETY: `raw` is the handle we just took out of `lib`; it is rewrapped right away.
        let lib = unsafe { UnixLib::from_raw(raw) };
        let mut exit = None;
        if let Some(f) = entry {
            // SAFETY: called once, with the module's own `dlopen` handle, as the spec requires.
            if !unsafe { f(raw) } {
                return Err("ModuleEntry failed".into());
            }
            // SAFETY: `ModuleExit` is the VST3-mandated Linux exit with this signature; the
            // pointer stays valid because loaded modules are never unmapped.
            exit = unsafe { lib.get::<ModuleExit>(b"ModuleExit\0") }.ok().map(|s| *s);
        }
        let _ = bundle;
        Ok((lib.into(), exit))
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        // SAFETY: loading a shared library runs its static initialisers. That is inherent to
        // hosting plugins; we only load files found in VST3 plugin folders or named explicitly.
        let lib = unsafe { libloading::Library::new(exe) }.map_err(|e| e.to_string())?;
        let mut exit: Option<ModuleExit> = None;
        #[cfg(windows)]
        {
            let _ = bundle;
            type InitDll = unsafe extern "system" fn() -> bool;
            // SAFETY: `InitDll` is the VST3-mandated Windows entry with this signature.
            if let Ok(f) = unsafe { lib.get::<InitDll>(b"InitDll\0") } {
                // SAFETY: called once after loading, as the spec requires.
                if !unsafe { f() } {
                    return Err("InitDll failed".into());
                }
                // SAFETY: `ExitDll` is the VST3-mandated Windows exit with this signature; the
                // pointer stays valid because loaded modules are never unmapped.
                exit = unsafe { lib.get::<ModuleExit>(b"ExitDll\0") }.ok().map(|s| *s);
            }
        }
        #[cfg(target_os = "macos")]
        {
            type BundleEntry = unsafe extern "C" fn(*mut c_void) -> bool;
            // SAFETY: `bundleEntry`/`BundleEntry` is the VST3-mandated macOS entry with this signature.
            let entry = unsafe { lib.get::<BundleEntry>(b"bundleEntry\0") }
                .or_else(|_| {
                    // SAFETY: as above (older SDKs export the capitalised name).
                    unsafe { lib.get::<BundleEntry>(b"BundleEntry\0") }
                })
                .ok()
                .map(|s| *s);
            if let Some(f) = entry
                && bundle.is_dir()
            {
                use std::os::unix::ffi::OsStrExt;
                let bytes = bundle.as_os_str().as_bytes();
                let len = isize::try_from(bytes.len()).map_err(|_| "path too long".to_string())?;
                // SAFETY: `bytes` is a valid buffer of `len` bytes for the duration of the call;
                // a null allocator means the default one.
                let url = unsafe { cf::CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), len, 1) };
                if url.is_null() {
                    return Err("cannot make a CFURL for the bundle".into());
                }
                // SAFETY: `url` is a valid CFURL created above.
                let cfbundle = unsafe { cf::CFBundleCreate(std::ptr::null(), url) };
                // SAFETY: we own `url` (Create rule) and no longer need it.
                unsafe { cf::CFRelease(url) };
                if cfbundle.is_null() {
                    return Err("not a loadable bundle".into());
                }
                // SAFETY: called once with a valid CFBundleRef. The bundle ref is intentionally
                // kept (never released): the module may hold on to it for its whole life, and
                // loaded modules are never unloaded.
                if !unsafe { f(cfbundle) } {
                    return Err("bundleEntry failed".into());
                }
                // SAFETY: `bundleExit`/`BundleExit` is the VST3-mandated macOS exit with this
                // signature; the pointer stays valid because loaded modules are never unmapped.
                exit = unsafe { lib.get::<ModuleExit>(b"bundleExit\0") }
                    .or_else(|_| {
                        // SAFETY: as above (older SDKs export the capitalised name).
                        unsafe { lib.get::<ModuleExit>(b"BundleExit\0") }
                    })
                    .ok()
                    .map(|s| *s);
            }
        }
        Ok((lib, exit))
    }
}

/// Runs the module exits at process exit, newest module first, each on the thread that ran its
/// entry. VST3 modules expect their exit before their static destructors run, and some crash in
/// those destructors without it (seen with Steinberg's HALion Sonic, after a plain scan). A module
/// with a live instance or leaked objects keeps running and is not exited, and so is one entered
/// on the main thread when the process exits from another thread. Runs once; later loads are
/// refused.
pub(crate) fn exit_modules() {
    if MODULES_EXITED.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut cache = match loaded().try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        // Another thread is loading a module right now: leave every module as it is.
        Err(TryLockError::WouldBlock) => return,
    };
    let main = MAIN_OS_THREAD.load(Ordering::Acquire) == os_thread();
    let (mut kept, mut here, mut there) = (Vec::new(), Vec::new(), Vec::new());
    while let Some((path, b)) = cache.pop() {
        if b.leaked.load(Ordering::Acquire) || (b.entered_on == EntryThread::Main && !main) {
            kept.push((path, b));
            continue;
        }
        match Arc::try_unwrap(b) {
            Ok(bundle) if bundle.entered_on == EntryThread::Main => here.push(bundle),
            Ok(bundle) => there.push(bundle),
            Err(b) => kept.push((path, b)),
        }
    }
    kept.reverse();
    *cache = kept;
    drop(cache);
    here.into_iter().for_each(Bundle::exit);
    if !there.is_empty() {
        // Bounded: a module thread stuck inside a plugin must not hang the process exit.
        let _ = on_module_thread(move || there.into_iter().for_each(Bundle::exit), Some(Duration::from_secs(5)));
    }
}

/// Registers [`exit_modules`] to run at process exit. Exit handlers run in reverse order of
/// registration, C++ static destructors included, so registering again after each module load
/// puts the exits ahead of the destructors of every module loaded so far.
fn register_exit_hook() {
    unsafe extern "C" {
        fn atexit(f: extern "C" fn()) -> c_int;
    }
    extern "C" fn hook() {
        // Nothing may unwind out of an exit handler.
        let _ = std::panic::catch_unwind(exit_modules);
    }
    // SAFETY: `atexit` is the C library function with this signature; `hook` is a plain function
    // of this binary, so it lives as long as the process.
    if unsafe { atexit(hook) } != 0 {
        log::warn!("vst3: cannot register the module exit handler");
    }
}

impl Bundle {
    /// Loads (once per path, then cached for the life of the process) a `.vst3`.
    pub fn load(path: &Path) -> Result<Arc<Bundle>, Vst3Error> {
        let mut cache = loaded().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, b)) = cache.iter().find(|(p, _)| p == path) {
            return Ok(b.clone());
        }
        let err = |m: &str| Vst3Error::Load(path.display().to_string(), m.to_string());
        if MODULES_EXITED.load(Ordering::Acquire) {
            return Err(err("plugin hosting has shut down"));
        }
        let b = if on_main_thread() {
            MAIN_OS_THREAD.store(os_thread(), Ordering::Release);
            Bundle::open(path, EntryThread::Main)?
        } else {
            let p = path.to_path_buf();
            on_module_thread(move || Bundle::open(&p, EntryThread::Modules), None).ok_or_else(|| err("the module thread is not running"))??
        };
        let b = Arc::new(b);
        cache.push((path.to_path_buf(), b.clone()));
        // After every new module, so that the exits run before its static destructors.
        register_exit_hook();
        Ok(b)
    }

    fn open(path: &Path, entered_on: EntryThread) -> Result<Bundle, Vst3Error> {
        let err = |m: String| Vst3Error::Load(path.display().to_string(), m);
        let exe = scan::bundle_binary(path).ok_or_else(|| err("no loadable binary".into()))?;
        let (lib, exit) = open_library(path, &exe).map_err(err)?;
        // The module entry ran: balance it before giving up on the module.
        let fail = |m: String| {
            if let Some(f) = exit {
                // SAFETY: balanced with the entry that ran in `open_library`; no plugin object exists.
                unsafe { f() };
            }
            err(m)
        };
        // SAFETY: `GetPluginFactory` is the VST3-mandated export with this signature.
        let get = match unsafe { lib.get::<GetFactory>(b"GetPluginFactory\0") } {
            Ok(sym) => *sym,
            Err(e) => return Err(fail(format!("not a VST3 plugin: {e}"))),
        };
        // SAFETY: the module is loaded and its entry function (if any) has run.
        let raw = unsafe { get() };
        // SAFETY: a non-null result is a factory pointer with one reference owned by the caller
        // (VST3 contract), which the `ComPtr` takes over.
        let factory = unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| fail("GetPluginFactory returned null".into()))?;
        Ok(Bundle { factory, exit, entered_on, leaked: AtomicBool::new(false), _lib: ManuallyDrop::new(lib) })
    }

    /// Releases the factory, then runs the module exit, as the VST3 SDK's own host does.
    fn exit(self) {
        let Bundle { factory, exit, .. } = self;
        drop(factory);
        if let Some(f) = exit {
            // SAFETY: balanced with the entry that ran when the module was loaded; called once
            // (`self` is consumed), after every instance is gone (each holds an `Arc` to the
            // bundle) and the factory released. The code stays mapped (`_lib` is never closed).
            unsafe { f() };
        }
    }

    /// Plugin objects of this module were leaked on purpose: never run its module exit.
    pub fn mark_leaked(&self) {
        self.leaked.store(true, Ordering::Release);
    }

    /// The vendor from the factory info (fallback for classes without one).
    fn factory_vendor(&self) -> String {
        // SAFETY: `PFactoryInfo` is plain data (char arrays and an int); all-zero is valid.
        let mut info: sb::PFactoryInfo = unsafe { std::mem::zeroed() };
        // SAFETY: valid factory; `info` is a writable struct of the right type.
        if ok(unsafe { self.factory.getFactoryInfo(&mut info) }) { read_c8(&info.vendor) } else { String::new() }
    }

    /// Every class this binary exports.
    pub fn classes(&self) -> Vec<RawClass> {
        // SAFETY: valid factory.
        let n = unsafe { self.factory.countClasses() }.clamp(0, MAX_CLASSES);
        let f2 = self.factory.cast::<sb::IPluginFactory2>();
        let vendor = self.factory_vendor();
        let mut out = Vec::new();
        for i in 0..n {
            if let Some(f2) = &f2 {
                // SAFETY: plain data; all-zero is valid.
                let mut c: sb::PClassInfo2 = unsafe { std::mem::zeroed() };
                // SAFETY: index below the count; `c` is writable.
                if ok(unsafe { f2.getClassInfo2(i, &mut c) }) {
                    let v = read_c8(&c.vendor);
                    out.push(RawClass {
                        cid: tuid_bytes(&c.cid),
                        category: read_c8(&c.category),
                        name: read_c8(&c.name),
                        vendor: if v.is_empty() { vendor.clone() } else { v },
                        version: read_c8(&c.version),
                        sdk_version: read_c8(&c.sdkVersion),
                        sub_categories: read_c8(&c.subCategories),
                    });
                    continue;
                }
            }
            // SAFETY: plain data; all-zero is valid.
            let mut c: sb::PClassInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `c` is writable.
            if ok(unsafe { self.factory.getClassInfo(i, &mut c) }) {
                out.push(RawClass {
                    cid: tuid_bytes(&c.cid),
                    category: read_c8(&c.category),
                    name: read_c8(&c.name),
                    vendor: vendor.clone(),
                    version: String::new(),
                    sdk_version: String::new(),
                    sub_categories: String::new(),
                });
            }
        }
        out
    }

    /// Creates an object of class `cid` and queries interface `I` on it.
    fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = std::ptr::null_mut();
        let iid = I::IID;
        // SAFETY: valid factory; `cid` and `iid` point to 16 readable bytes each (VST3 passes TUIDs
        // as `FIDString`); `obj` is writable.
        let r = unsafe { self.factory.createInstance(cid.as_ptr().cast::<c_char>(), iid.as_ptr().cast::<c_char>(), &mut obj) };
        if !ok(r) {
            return None;
        }
        // SAFETY: on success `obj` holds an `I` pointer with one reference owned by us.
        unsafe { ComPtr::from_raw(obj.cast::<I>()) }
    }

    /// Creates, initialises and wires up one plugin instance.
    pub fn instantiate(self: &Arc<Self>, cid: &[u8; 16]) -> Result<Instance, Vst3Error> {
        let name = hex(cid);
        let err = |m: &str| Vst3Error::Instantiate(name.clone(), m.to_string());
        let host = ComWrapper::new(HostContext::default());
        let host_unknown = host.as_com_ref::<sv::IHostApplication>().ok_or_else(|| err("host context"))?.upcast::<FUnknown>().as_ptr();
        let component = self.create::<sv::IComponent>(cid).ok_or_else(|| err("factory cannot create the component"))?;
        // SAFETY: fresh component; the host context outlives it (owned by the returned Instance).
        if !ok(unsafe { component.initialize(host_unknown) }) {
            return Err(err("component initialize failed"));
        }
        let mut inst = Instance {
            processor: None,
            controller: None,
            single: false,
            connections: None,
            component_initialized: true,
            controller_initialized: false,
            active: false,
            processing: false,
            max_frames: 0,
            in_params: ComWrapper::new(ParamChanges::new(PARAM_QUEUE_CAP)),
            out_params: ComWrapper::new(ParamChanges::new(PARAM_QUEUE_CAP)),
            in_events: ComWrapper::new(EventList::new(EVENT_CAP)),
            out_events: ComWrapper::new(EventList::new(EVENT_CAP)),
            // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
            context: unsafe { std::mem::zeroed() },
            event_in: false,
            editor: Arc::new(EditorLink::default()),
            component,
            host,
            bundle: self.clone(),
        };
        let processor = inst.component.cast::<sv::IAudioProcessor>().ok_or_else(|| err("no IAudioProcessor"))?;
        inst.processor = Some(processor);
        // Edit controller: the component itself (single-component plugin) or a separate class.
        if let Some(c) = inst.component.cast::<sv::IEditController>() {
            inst.controller = Some(c);
            inst.single = true;
        } else {
            let mut ccid: sb::TUID = [0; 16];
            // SAFETY: valid component; `ccid` is a writable 16-byte TUID.
            if ok(unsafe { inst.component.getControllerClassId(&mut ccid) })
                && let Some(c) = self.create::<sv::IEditController>(&tuid_bytes(&ccid))
            {
                // SAFETY: fresh controller; the host context outlives it.
                if ok(unsafe { c.initialize(host_unknown) }) {
                    inst.controller = Some(c);
                    inst.controller_initialized = true;
                } else {
                    log::warn!("vst3 {name}: controller initialize failed; parameters unavailable");
                }
            }
        }
        if let Some(c) = &inst.controller {
            if !inst.single
                && let (Some(a), Some(b)) = (inst.component.cast::<sv::IConnectionPoint>(), c.cast::<sv::IConnectionPoint>())
            {
                // SAFETY: both connection points are valid for as long as `inst` lives; they are
                // disconnected in `Drop` before either side terminates.
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                inst.connections = Some((a, b));
            }
            inst.sync_controller_state();
            if let Some(h) = inst.host.as_com_ref::<sv::IComponentHandler>() {
                // SAFETY: valid controller; the handler (host context) outlives it.
                unsafe { c.setComponentHandler(h.as_ptr()) };
            }
        }
        inst.editor = Arc::new(EditorLink {
            inner: Mutex::new(EditorInner { component: Some(inst.component.clone()), controller: inst.controller.clone(), ..EditorInner::default() }),
        });
        Ok(inst)
    }
}

/// A `TUID` (`[i8; 16]`) as raw bytes.
fn tuid_bytes(t: &sb::TUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (o, b) in out.iter_mut().zip(t.iter()) {
        *o = *b as u8;
    }
    out
}

/// Upper-case hex of a class id (the form used in `vst3:<hex>` ids).
pub(crate) fn hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

// ---- host-side COM objects -----------------------------------------------------------------

/// The host context handed to `initialize` (`IHostApplication`) and to the controller
/// (`IComponentHandler`: parameter edits made in the plugin's editor are queued for the UI, which
/// writes them into the session, from where they reach the processor).
pub(crate) struct HostContext {
    pub latency_changed: AtomicBool,
    pub restart_requested: AtomicBool,
    /// `performEdit` values (normalized), bounded by `EDIT_CAP`.
    pub edits: Mutex<Vec<(u32, f64)>>,
}

impl Default for HostContext {
    fn default() -> Self {
        HostContext {
            latency_changed: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            edits: Mutex::new(Vec::with_capacity(EDIT_CAP)),
        }
    }
}

impl HostContext {
    /// Takes the queued editor edits.
    pub fn take_edits(&self) -> Vec<(u32, f64)> {
        let mut e = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let out = e.clone();
        e.clear();
        out
    }
}

impl Class for HostContext {
    type Interfaces = (sv::IHostApplication, sv::IComponentHandler);
}

impl IHostApplicationTrait for HostContext {
    unsafe fn getName(&self, name: *mut sv::String128) -> sb::tresult {
        // SAFETY: the plugin passes a pointer to a writable String128 (or null, handled).
        match unsafe { name.as_mut() } {
            Some(n) => {
                write_c16("SoundCraft", n);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }

    /// Creates the objects plugins may ask the host for: `IMessage` (what a controller and its
    /// processor send each other through `IConnectionPoint::notify`) and `IAttributeList`.
    /// Plugins that talk between their halves this way fail without them (Line 6 Helix
    /// reported "Failed to get content database recovery info (Out of memory condition)").
    unsafe fn createInstance(&self, cid: *mut sb::TUID, iid: *mut sb::TUID, obj: *mut *mut c_void) -> sb::tresult {
        // SAFETY: `obj` is the plugin's out-pointer (or null, handled).
        let Some(out) = (unsafe { obj.as_mut() }) else { return sb::kInvalidArgument };
        *out = std::ptr::null_mut();
        // SAFETY: `cid` and `iid` point to 16-byte TUIDs (or are null, handled).
        let (Some(cid), Some(iid)) = (unsafe { cid.as_ref() }, unsafe { iid.as_ref() }) else { return sb::kInvalidArgument };
        let (cid, iid) = (tuid_bytes(cid), tuid_bytes(iid));
        let wants = |class: [u8; 16]| cid == class && (iid == class || iid == FUnknown::IID);
        let made = if wants(sv::IMessage::IID) {
            ComWrapper::new(HostMessage::default()).to_com_ptr::<sv::IMessage>().map(|p| p.into_raw().cast::<c_void>())
        } else if wants(sv::IAttributeList::IID) {
            ComWrapper::new(HostAttributes::default()).to_com_ptr::<sv::IAttributeList>().map(|p| p.into_raw().cast::<c_void>())
        } else {
            None
        };
        match made {
            Some(p) => {
                *out = p;
                sb::kResultOk
            }
            None => sb::kResultFalse,
        }
    }
}

impl IComponentHandlerTrait for HostContext {
    unsafe fn beginEdit(&self, _id: sv::ParamID) -> sb::tresult {
        sb::kResultOk
    }
    unsafe fn performEdit(&self, id: sv::ParamID, value: sv::ParamValue) -> sb::tresult {
        if value.is_finite() {
            let mut e = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
            let v = value.clamp(0.0, 1.0);
            if let Some(x) = e.iter_mut().find(|(i, _)| *i == id) {
                x.1 = v;
            } else if e.len() < EDIT_CAP {
                e.push((id, v));
            }
        }
        sb::kResultOk
    }
    unsafe fn endEdit(&self, _id: sv::ParamID) -> sb::tresult {
        sb::kResultOk
    }
    unsafe fn restartComponent(&self, flags: sb::int32) -> sb::tresult {
        if flags as u32 & sv::RestartFlags_::kLatencyChanged as u32 != 0 {
            self.latency_changed.store(true, Ordering::Relaxed);
        }
        self.restart_requested.store(true, Ordering::Relaxed);
        sb::kResultOk
    }
}

/// Longest string attribute kept, in UTF-16 units (longer ones are cut).
const ATTR_STRING_MAX: usize = 1 << 20;
/// Largest binary attribute kept.
const ATTR_BINARY_MAX: usize = 256 << 20;

/// One `IAttributeList` value. Strings and blobs live in their own allocation, so a pointer
/// handed out by `getBinary` stays valid while the list holds the value.
enum Attr {
    Int(i64),
    Float(f64),
    Str(Box<[u16]>),
    Bin(Box<[u8]>),
}

/// The host's `IAttributeList`: what a plugin puts in an `IMessage`.
#[derive(Default)]
pub(crate) struct HostAttributes {
    values: Mutex<std::collections::HashMap<Vec<u8>, Attr>>,
}

impl Class for HostAttributes {
    type Interfaces = (sv::IAttributeList,);
}

/// An attribute id's bytes (`None` for a null id).
fn attr_key(id: sv::IAttributeList_::AttrID) -> Option<Vec<u8>> {
    // SAFETY: a non-null AttrID is a NUL-terminated C string owned by the caller.
    (!id.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(id) }.to_bytes().to_vec())
}

impl HostAttributes {
    fn values(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<Vec<u8>, Attr>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, id: sv::IAttributeList_::AttrID, v: Attr) -> sb::tresult {
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        self.values().insert(k, v);
        sb::kResultOk
    }
}

impl sv::IAttributeListTrait for HostAttributes {
    unsafe fn setInt(&self, id: sv::IAttributeList_::AttrID, value: sb::int64) -> sb::tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: sv::IAttributeList_::AttrID, value: *mut sb::int64) -> sb::tresult {
        // SAFETY: `value` is the caller's out-pointer (or null, handled).
        match (
            attr_key(id).and_then(|k| match self.values().get(&k) {
                Some(Attr::Int(v)) => Some(*v),
                _ => None,
            }),
            unsafe { value.as_mut() },
        ) {
            (Some(v), Some(out)) => {
                *out = v;
                sb::kResultOk
            }
            _ => sb::kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: sv::IAttributeList_::AttrID, value: f64) -> sb::tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: sv::IAttributeList_::AttrID, value: *mut f64) -> sb::tresult {
        // SAFETY: `value` is the caller's out-pointer (or null, handled).
        match (
            attr_key(id).and_then(|k| match self.values().get(&k) {
                Some(Attr::Float(v)) => Some(*v),
                _ => None,
            }),
            unsafe { value.as_mut() },
        ) {
            (Some(v), Some(out)) => {
                *out = v;
                sb::kResultOk
            }
            _ => sb::kResultFalse,
        }
    }

    unsafe fn setString(&self, id: sv::IAttributeList_::AttrID, string: *const sv::TChar) -> sb::tresult {
        if string.is_null() {
            return sb::kInvalidArgument;
        }
        let mut s = Vec::new();
        // SAFETY: a non-null string is NUL-terminated UTF-16 owned by the caller; reading stops at
        // the NUL or at ATTR_STRING_MAX units.
        while s.len() < ATTR_STRING_MAX {
            let c = unsafe { *string.add(s.len()) };
            if c == 0 {
                break;
            }
            s.push(c);
        }
        self.set(id, Attr::Str(s.into_boxed_slice()))
    }

    unsafe fn getString(&self, id: sv::IAttributeList_::AttrID, string: *mut sv::TChar, size_in_bytes: sb::uint32) -> sb::tresult {
        let units = size_in_bytes as usize / 2;
        if string.is_null() || units == 0 {
            return sb::kInvalidArgument;
        }
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        let values = self.values();
        let Some(Attr::Str(s)) = values.get(&k) else { return sb::kResultFalse };
        let n = s.len().min(units - 1);
        // SAFETY: the caller's buffer holds `units` UTF-16 units; `n + 1 <= units` are written.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
            *string.add(n) = 0;
        }
        sb::kResultOk
    }

    unsafe fn setBinary(&self, id: sv::IAttributeList_::AttrID, data: *const c_void, size_in_bytes: sb::uint32) -> sb::tresult {
        let n = size_in_bytes as usize;
        if n > ATTR_BINARY_MAX {
            return sb::kOutOfMemory;
        }
        let bytes: Box<[u8]> = if n == 0 || data.is_null() {
            Box::new([])
        } else {
            // SAFETY: the caller's `data` holds `n` readable bytes.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), n) }.into()
        };
        self.set(id, Attr::Bin(bytes))
    }

    unsafe fn getBinary(&self, id: sv::IAttributeList_::AttrID, data: *mut *const c_void, size_in_bytes: *mut sb::uint32) -> sb::tresult {
        let Some(k) = attr_key(id) else { return sb::kInvalidArgument };
        let values = self.values();
        let Some(Attr::Bin(b)) = values.get(&k) else { return sb::kResultFalse };
        // SAFETY: both are the caller's out-pointers (or null, handled). The pointer handed out
        // stays valid until this attribute is replaced or the list is released (VST3 contract).
        match (unsafe { data.as_mut() }, unsafe { size_in_bytes.as_mut() }) {
            (Some(d), Some(sz)) => {
                *d = b.as_ptr().cast::<c_void>();
                *sz = u32::try_from(b.len()).unwrap_or(u32::MAX);
                sb::kResultOk
            }
            _ => sb::kInvalidArgument,
        }
    }
}

/// The host's `IMessage`: an id and an attribute list, created for a plugin through
/// `IHostApplication::createInstance`.
pub(crate) struct HostMessage {
    id: Mutex<std::ffi::CString>,
    attributes: ComWrapper<HostAttributes>,
}

impl Default for HostMessage {
    fn default() -> Self {
        HostMessage { id: Mutex::new(std::ffi::CString::default()), attributes: ComWrapper::new(HostAttributes::default()) }
    }
}

impl Class for HostMessage {
    type Interfaces = (sv::IMessage,);
}

impl sv::IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> sb::FIDString {
        // The string's buffer lives until the next `setMessageID`, as the interface expects.
        self.id.lock().unwrap_or_else(PoisonError::into_inner).as_ptr()
    }

    unsafe fn setMessageID(&self, id: sb::FIDString) {
        // SAFETY: a non-null id is a NUL-terminated C string owned by the caller.
        let s = if id.is_null() { std::ffi::CString::default() } else { unsafe { std::ffi::CStr::from_ptr(id) }.to_owned() };
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }

    unsafe fn getAttributes(&self) -> *mut sv::IAttributeList {
        // Not add-ref'ed: the list belongs to the message (VST3 contract).
        self.attributes.as_com_ref::<sv::IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

/// One parameter's points for one block. SoundCraft sends at most one point per block; a
/// plugin writing output changes may add more, of which only the last is kept.
#[derive(Default)]
pub(crate) struct ParamQueue {
    id: AtomicU32,
    offset: AtomicI32,
    value: AtomicU64,
    points: AtomicI32,
}

impl Class for ParamQueue {
    type Interfaces = (sv::IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> sv::ParamID {
        self.id.load(Ordering::Relaxed)
    }
    unsafe fn getPointCount(&self) -> sb::int32 {
        self.points.load(Ordering::Relaxed)
    }
    unsafe fn getPoint(&self, index: sb::int32, offset: *mut sb::int32, value: *mut sv::ParamValue) -> sb::tresult {
        if index != 0 || self.points.load(Ordering::Relaxed) < 1 {
            return sb::kInvalidArgument;
        }
        // SAFETY: out-pointers from the plugin (or null, handled).
        if let (Some(o), Some(v)) = (unsafe { offset.as_mut() }, unsafe { value.as_mut() }) {
            *o = self.offset.load(Ordering::Relaxed);
            *v = f64::from_bits(self.value.load(Ordering::Relaxed));
            sb::kResultOk
        } else {
            sb::kInvalidArgument
        }
    }
    unsafe fn addPoint(&self, offset: sb::int32, value: sv::ParamValue, index: *mut sb::int32) -> sb::tresult {
        self.offset.store(offset, Ordering::Relaxed);
        self.value.store(value.to_bits(), Ordering::Relaxed);
        self.points.store(1, Ordering::Relaxed);
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(i) = unsafe { index.as_mut() } {
            *i = 0;
        }
        sb::kResultOk
    }
}

/// A fixed pool of parameter queues (`IParameterChanges`), reused every block.
pub(crate) struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    count: AtomicI32,
}

impl Class for ParamChanges {
    type Interfaces = (sv::IParameterChanges,);
}

impl ParamChanges {
    fn new(cap: usize) -> ParamChanges {
        ParamChanges { queues: (0..cap).map(|_| ComWrapper::new(ParamQueue::default())).collect(), count: AtomicI32::new(0) }
    }

    fn clear(&self) {
        self.count.store(0, Ordering::Relaxed);
    }

    /// Finds or adds the queue for `id`; `None` when the pool is full.
    fn queue_for(&self, id: u32) -> Option<(i32, &ParamQueue)> {
        let n = self.count.load(Ordering::Relaxed).max(0);
        for (i, q) in self.queues.iter().enumerate().take(n as usize) {
            if q.id.load(Ordering::Relaxed) == id {
                return Some((i32::try_from(i).ok()?, q));
            }
        }
        let q = self.queues.get(n as usize)?;
        q.id.store(id, Ordering::Relaxed);
        q.points.store(0, Ordering::Relaxed);
        self.count.store(n + 1, Ordering::Relaxed);
        Some((n, q))
    }

    /// Host side: one point for `id` at `offset` (replacing an earlier one this block).
    fn set(&self, id: u32, offset: i32, value: f64) {
        if let Some((_, q)) = self.queue_for(id) {
            q.offset.store(offset, Ordering::Relaxed);
            q.value.store(value.to_bits(), Ordering::Relaxed);
            q.points.store(1, Ordering::Relaxed);
        }
    }

    fn queue_ptr(&self, index: usize) -> *mut sv::IParamValueQueue {
        self.queues.get(index).and_then(|q| q.as_com_ref::<sv::IParamValueQueue>()).map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> sb::int32 {
        self.count.load(Ordering::Relaxed)
    }
    unsafe fn getParameterData(&self, index: sb::int32) -> *mut sv::IParamValueQueue {
        if index < 0 || index >= self.count.load(Ordering::Relaxed) {
            return std::ptr::null_mut();
        }
        self.queue_ptr(index as usize)
    }
    unsafe fn addParameterData(&self, id: *const sv::ParamID, index: *mut sb::int32) -> *mut sv::IParamValueQueue {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&id) = (unsafe { id.as_ref() }) else { return std::ptr::null_mut() };
        let Some((i, _)) = self.queue_for(id) else { return std::ptr::null_mut() };
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(out) = unsafe { index.as_mut() } {
            *out = i;
        }
        self.queue_ptr(i as usize)
    }
}

/// A bounded event list (`IEventList`). Only ever touched by one thread at a time (the host
/// fills it, then the plugin reads it inside `process`), so the lock is never contended; the
/// plugin side uses `try_lock` and never blocks.
pub(crate) struct EventList {
    events: Mutex<Vec<sv::Event>>,
    cap: usize,
}

impl Class for EventList {
    type Interfaces = (sv::IEventList,);
}

impl EventList {
    fn new(cap: usize) -> EventList {
        EventList { events: Mutex::new(Vec::with_capacity(cap)), cap }
    }
    fn with<R>(&self, f: impl FnOnce(&mut Vec<sv::Event>) -> R) -> Option<R> {
        match self.events.try_lock() {
            Ok(mut g) => Some(f(&mut g)),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(f(&mut p.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> sb::int32 {
        self.with(|v| i32::try_from(v.len()).unwrap_or(i32::MAX)).unwrap_or(0)
    }
    unsafe fn getEvent(&self, index: sb::int32, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: out-pointer from the plugin (or null, handled).
        let Some(out) = (unsafe { e.as_mut() }) else { return sb::kInvalidArgument };
        let Ok(i) = usize::try_from(index) else { return sb::kInvalidArgument };
        match self.with(|v| v.get(i).copied()).flatten() {
            Some(ev) => {
                *out = ev;
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut sv::Event) -> sb::tresult {
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(&ev) = (unsafe { e.as_ref() }) else { return sb::kInvalidArgument };
        let cap = self.cap;
        // Output events (e.g. MIDI out) are accepted while there is room; SoundCraft ignores them.
        match self.with(|v| {
            if v.len() < cap {
                v.push(ev);
                true
            } else {
                false
            }
        }) {
            Some(true) => sb::kResultOk,
            _ => sb::kResultFalse,
        }
    }
}

/// An in-memory `IBStream`, used to hand the component's state to its controller.
pub(crate) struct MemoryStream {
    inner: Mutex<(Vec<u8>, usize)>,
}

impl Class for MemoryStream {
    type Interfaces = (sb::IBStream,);
}

impl MemoryStream {
    fn new(data: Vec<u8>) -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream { inner: Mutex::new((data, 0)) })
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.lock().0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Vec<u8>, usize)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: sb::int32, num_read: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let want = usize::try_from(num_bytes).unwrap_or(0);
        let avail = data.len().saturating_sub(*pos);
        let n = want.min(avail);
        if n > 0 && !buffer.is_null() {
            let Some(src) = data.get(*pos..*pos + n) else { return sb::kInternalError };
            // SAFETY: the plugin provides `buffer` with room for `num_bytes` ≥ `n` bytes; `src`
            // is `n` valid bytes; the regions cannot overlap (ours is a private Vec).
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buffer.cast::<u8>(), n) };
            *pos += n;
        }
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { num_read.as_mut() } {
            *r = i32::try_from(n).unwrap_or(0);
        }
        sb::kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: sb::int32, num_written: *mut sb::int32) -> sb::tresult {
        let mut g = self.lock();
        let (data, pos) = &mut *g;
        let n = usize::try_from(num_bytes).unwrap_or(0);
        let end = pos.saturating_add(n);
        if buffer.is_null() || end > MAX_STATE_BYTES {
            return sb::kResultFalse;
        }
        if data.len() < end {
            data.resize(end, 0);
        }
        // SAFETY: the plugin provides `n` readable bytes at `buffer`; the destination range was
        // just made valid.
        let src = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        if let Some(dst) = data.get_mut(*pos..end) {
            dst.copy_from_slice(src);
        }
        *pos = end;
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(w) = unsafe { num_written.as_mut() } {
            *w = num_bytes;
        }
        sb::kResultOk
    }
    unsafe fn seek(&self, pos: sb::int64, mode: sb::int32, result: *mut sb::int64) -> sb::tresult {
        let mut g = self.lock();
        let len = i64::try_from(g.0.len()).unwrap_or(i64::MAX);
        let cur = i64::try_from(g.1).unwrap_or(0);
        let base = match mode as u32 {
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekSet as u32 => 0,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekCur as u32 => cur,
            x if x == sb::IBStream_::IStreamSeekMode_::kIBSeekEnd as u32 => len,
            _ => return sb::kInvalidArgument,
        };
        let new = base.saturating_add(pos).clamp(0, len);
        g.1 = usize::try_from(new).unwrap_or(0);
        // SAFETY: out-pointer from the plugin (or null, handled).
        if let Some(r) = unsafe { result.as_mut() } {
            *r = new;
        }
        sb::kResultOk
    }
    unsafe fn tell(&self, pos: *mut sb::int64) -> sb::tresult {
        let g = self.lock();
        // SAFETY: out-pointer from the plugin (or null, handled).
        match unsafe { pos.as_mut() } {
            Some(p) => {
                *p = i64::try_from(g.1).unwrap_or(0);
                sb::kResultOk
            }
            None => sb::kInvalidArgument,
        }
    }
}

// ---- events and buffers --------------------------------------------------------------------

/// A host → plugin note event, safe to build anywhere; converted inside `process`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NoteEvent {
    On { time: u32, key: u8, velocity: f32 },
    Off { time: u32, key: u8 },
    Pressure { time: u32, key: u8, pressure: f32 },
}

impl NoteEvent {
    pub fn time(&self) -> u32 {
        match *self {
            NoteEvent::On { time, .. } | NoteEvent::Off { time, .. } | NoteEvent::Pressure { time, .. } => time,
        }
    }
    pub fn set_time(&mut self, t: u32) {
        match self {
            NoteEvent::On { time, .. } | NoteEvent::Off { time, .. } | NoteEvent::Pressure { time, .. } => *time = t,
        }
    }

    fn to_raw(self, offset: u32) -> sv::Event {
        // SAFETY: `Event` is plain data (numbers and a union of plain structs); all-zero is valid.
        let mut e: sv::Event = unsafe { std::mem::zeroed() };
        e.busIndex = 0;
        e.sampleOffset = i32::try_from(self.time().saturating_sub(offset)).unwrap_or(0);
        e.flags = sv::Event_::EventFlags_::kIsLive as u16;
        match self {
            NoteEvent::On { key, velocity, .. } => {
                e.r#type = sv::Event_::EventTypes_::kNoteOnEvent as u16;
                e.__field0.noteOn = sv::NoteOnEvent { channel: 0, pitch: i16::from(key), tuning: 0.0, velocity, length: 0, noteId: -1 };
            }
            NoteEvent::Off { key, .. } => {
                e.r#type = sv::Event_::EventTypes_::kNoteOffEvent as u16;
                e.__field0.noteOff = sv::NoteOffEvent { channel: 0, pitch: i16::from(key), velocity: 0.0, noteId: -1, tuning: 0.0 };
            }
            NoteEvent::Pressure { key, pressure, .. } => {
                e.r#type = sv::Event_::EventTypes_::kPolyPressureEvent as u16;
                e.__field0.polyPressure = sv::PolyPressureEvent { channel: 0, pitch: i16::from(key), pressure, noteId: -1 };
            }
        }
        e
    }
}

/// Planar f32 buffers for every audio bus in one direction, with the pointer tables VST3 needs.
/// Allocated in `new`; `process` only rewrites pointers in place (no allocation).
pub(crate) struct BusBuffers {
    data: Vec<Vec<Vec<f32>>>,
    ptrs: Vec<Vec<*mut f32>>,
    bufs: Vec<sv::AudioBusBuffers>,
    frames: usize,
}

// SAFETY: the raw pointers only ever point into `data`, which this struct owns; moving the struct
// to another thread moves that ownership with it.
unsafe impl Send for BusBuffers {}

impl BusBuffers {
    /// One buffer set per bus, `channels[b]` channels each (capped), `frames` long.
    pub fn new(channels: &[u32], frames: usize) -> BusBuffers {
        let data: Vec<Vec<Vec<f32>>> =
            channels.iter().take(MAX_BUSES as usize).map(|&c| (0..c.min(MAX_BUS_CHANNELS)).map(|_| vec![0.0; frames]).collect()).collect();
        let ptrs = data.iter().map(|p| vec![std::ptr::null_mut(); p.len()]).collect();
        let bufs = data
            .iter()
            .map(|_| sv::AudioBusBuffers {
                numChannels: 0,
                silenceFlags: 0,
                __field0: sv::AudioBusBuffers__type0 { channelBuffers32: std::ptr::null_mut() },
            })
            .collect();
        BusBuffers { data, ptrs, bufs, frames }
    }

    pub fn channels(&self, bus: usize) -> usize {
        self.data.get(bus).map_or(0, Vec::len)
    }

    pub fn channel(&self, bus: usize, ch: usize) -> Option<&[f32]> {
        self.data.get(bus).and_then(|p| p.get(ch)).map(Vec::as_slice)
    }

    pub fn channel_mut(&mut self, bus: usize, ch: usize) -> Option<&mut [f32]> {
        self.data.get_mut(bus).and_then(|p| p.get_mut(ch)).map(Vec::as_mut_slice)
    }

    /// Points the VST3 tables at the current data (no allocation).
    fn refresh(&mut self) {
        for ((d, p), b) in self.data.iter_mut().zip(self.ptrs.iter_mut()).zip(self.bufs.iter_mut()) {
            for (dc, pc) in d.iter_mut().zip(p.iter_mut()) {
                *pc = dc.as_mut_ptr();
            }
            b.numChannels = i32::try_from(p.len()).unwrap_or(0);
            b.silenceFlags = 0;
            b.__field0.channelBuffers32 = p.as_mut_ptr();
        }
    }
}

// ---- instances -----------------------------------------------------------------------------

/// One parameter as reported by `IEditController::getParameterInfo`, plus its plain range.
#[derive(Debug, Clone)]
pub(crate) struct RawParam {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub step_count: i32,
    pub default_normalized: f64,
    pub flags: i32,
}

pub(crate) const PARAM_HIDDEN: i32 = sv::ParameterInfo_::ParameterFlags_::kIsHidden;
pub(crate) const PARAM_READ_ONLY: i32 = sv::ParameterInfo_::ParameterFlags_::kIsReadOnly;
pub(crate) const PARAM_BYPASS: i32 = sv::ParameterInfo_::ParameterFlags_::kIsBypass;

/// The audio bus layout chosen by [`Instance::configure`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Layout {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub event_input: bool,
}

fn apply_transport(c: &mut sv::ProcessContext, t: &Transport) {
    use sv::ProcessContext_::StatesAndFlags_::*;
    let mut state = kTempoValid | kTimeSigValid | kProjectTimeMusicValid | kBarPositionValid | kContTimeValid;
    if t.playing {
        state |= kPlaying;
    }
    if t.recording {
        state |= kRecording;
    }
    // The flag constants are the C default enum type: i32 on Windows, u32 elsewhere.
    #[allow(clippy::unnecessary_cast)]
    let state = state as u32;
    c.state = state;
    c.tempo = t.tempo;
    c.timeSigNumerator = i32::try_from(t.numerator).unwrap_or(4);
    c.timeSigDenominator = i32::try_from(t.denominator).unwrap_or(4);
    c.projectTimeSamples = t.samples;
    c.projectTimeMusic = t.quarters;
    c.barPositionMusic = t.bar_start;
}

/// A live plugin: component + audio processor (+ edit controller) and the host objects.
pub(crate) struct Instance {
    processor: Option<ComPtr<sv::IAudioProcessor>>,
    controller: Option<ComPtr<sv::IEditController>>,
    /// The controller is the component itself (single-component plugin).
    single: bool,
    connections: Option<(ComPtr<sv::IConnectionPoint>, ComPtr<sv::IConnectionPoint>)>,
    component_initialized: bool,
    controller_initialized: bool,
    active: bool,
    processing: bool,
    max_frames: usize,
    in_params: ComWrapper<ParamChanges>,
    out_params: ComWrapper<ParamChanges>,
    in_events: ComWrapper<EventList>,
    out_events: ComWrapper<EventList>,
    context: sv::ProcessContext,
    event_in: bool,
    editor: Arc<EditorLink>,
    // Field order matters for drop: `Drop::drop` shuts the plugin down first; then the component
    // and host objects are released, and the bundle (which keeps the code mapped) goes last.
    component: ComPtr<sv::IComponent>,
    host: ComWrapper<HostContext>,
    bundle: Arc<Bundle>,
}

// SAFETY: an `Instance` is used through `&mut self` only, so calls into the plugin are always
// serialised. SoundCraft creates instances on the engine/UI thread and then moves them into the
// mix engine (documented limitation: VST3 expects `setActive`, controller calls etc. on the UI
// thread; they may then happen on the audio thread).
unsafe impl Send for Instance {}

impl Instance {
    fn processor(&self) -> Option<&ComPtr<sv::IAudioProcessor>> {
        self.processor.as_ref()
    }

    pub fn host(&self) -> &HostContext {
        &self.host
    }

    /// A counted reference to the host context (editor handles read its edit queue).
    pub fn host_ref(&self) -> ComWrapper<HostContext> {
        self.host.clone()
    }

    /// The editor link shared with editor handles (`None` without an edit controller).
    pub fn editor(&self) -> Option<Arc<EditorLink>> {
        self.editor.has_controller().then(|| Arc::clone(&self.editor))
    }

    /// The same link, for state handles.
    pub fn state_link(&self) -> Arc<EditorLink> {
        Arc::clone(&self.editor)
    }

    /// Writes one state into a fresh memory stream.
    fn read_state(f: impl FnOnce(*mut sb::IBStream) -> sb::tresult) -> Option<Vec<u8>> {
        let stream = MemoryStream::new(Vec::new());
        let s = stream.as_com_ref::<sb::IBStream>()?;
        ok(f(s.as_ptr())).then(|| stream.take())
    }

    /// Restores a [`EditorLink::save_state`] blob (or a bare component state): component
    /// `setState`, then the controller's `setComponentState` and `setState`. `false` when the
    /// blob is malformed or the component refuses it.
    pub fn load_state(&self, data: &[u8]) -> bool {
        let Some((comp, ctrl)) = split_state(data) else { return false };
        let stream = MemoryStream::new(comp.to_vec());
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return false };
        // SAFETY: valid component; the stream lives on this frame for the call.
        if !ok(unsafe { self.component.setState(s.as_ptr()) }) {
            return false;
        }
        if let Some(c) = &self.controller {
            stream.lock().1 = 0;
            // SAFETY: valid controller; the same stream, rewound.
            unsafe { c.setComponentState(s.as_ptr()) };
            if !ctrl.is_empty() {
                let cs = MemoryStream::new(ctrl.to_vec());
                if let Some(r) = cs.as_com_ref::<sb::IBStream>() {
                    // SAFETY: valid controller; the stream lives on this frame for the call.
                    unsafe { c.setState(r.as_ptr()) };
                }
            }
        }
        true
    }

    /// Copies the component's state into the controller (`getState` → `setComponentState`).
    fn sync_controller_state(&self) {
        let Some(c) = &self.controller else { return };
        let stream = ComWrapper::new(MemoryStream { inner: Mutex::new((Vec::new(), 0)) });
        let Some(s) = stream.as_com_ref::<sb::IBStream>() else { return };
        // SAFETY: valid component; the stream lives on this frame for the whole call.
        if ok(unsafe { self.component.getState(s.as_ptr()) }) {
            stream.lock().1 = 0;
            // SAFETY: valid controller; same stream, rewound.
            unsafe { c.setComponentState(s.as_ptr()) };
        }
    }

    pub fn params(&self) -> Vec<RawParam> {
        let Some(c) = &self.controller else { return Vec::new() };
        // SAFETY: valid controller.
        let n = unsafe { c.getParameterCount() }.clamp(0, MAX_PARAMS);
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: `ParameterInfo` is plain data; all-zero is valid.
            let mut info: sv::ParameterInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; `info` is writable.
            if !ok(unsafe { c.getParameterInfo(i, &mut info) }) {
                continue;
            }
            let d = info.defaultNormalizedValue;
            out.push(RawParam {
                id: info.id,
                title: read_c16(&info.title),
                units: read_c16(&info.units),
                step_count: info.stepCount.max(0),
                default_normalized: if d.is_finite() { d.clamp(0.0, 1.0) } else { 0.0 },
                flags: info.flags,
            });
        }
        out
    }

    /// Normalized → plain (`None` without a controller or for a non-finite answer).
    pub fn to_plain(&self, id: u32, normalized: f64) -> Option<f64> {
        let c = self.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.normalizedParamToPlain(id, normalized.clamp(0.0, 1.0)) };
        v.is_finite().then_some(v)
    }

    /// Plain → normalized, clamped to 0..1.
    pub fn to_normalized(&self, id: u32, plain: f64) -> Option<f64> {
        let c = self.controller.as_ref()?;
        if !plain.is_finite() {
            return None;
        }
        // SAFETY: valid controller.
        let v = unsafe { c.plainParamToNormalized(id, plain) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    pub fn normalized_value(&self, id: u32) -> Option<f64> {
        let c = self.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.getParamNormalized(id) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// The parameter each MIDI controller drives on channel 1 (`IMidiMapping`), indexed by VST3's
    /// controller numbers: CCs 0–127, then [`AFTERTOUCH`] and [`PITCH_BEND`]. Main thread only.
    pub fn midi_mapping(&self) -> Vec<Option<u32>> {
        let Some(m) = self.controller.as_ref().and_then(|c| c.cast::<sv::IMidiMapping>()) else { return Vec::new() };
        (0..=PITCH_BEND)
            .map(|n| {
                let mut id: sv::ParamID = 0;
                let n = i16::try_from(n).unwrap_or(0);
                // SAFETY: valid mapping interface on the main thread; `id` is a local.
                ok(unsafe { m.getMidiControllerAssignment(0, 0, n, &mut id) }).then_some(id)
            })
            .collect()
    }

    pub fn value_text(&self, id: u32, normalized: f64) -> Option<String> {
        let c = self.controller.as_ref()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller; `s` is a writable String128.
        ok(unsafe { c.getParamStringByValue(id, normalized.clamp(0.0, 1.0), &mut s) }).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    pub fn latency(&self) -> u32 {
        // SAFETY: valid processor.
        self.processor().map_or(0, |p| unsafe { p.getLatencySamples() })
    }

    pub fn tail(&self) -> u32 {
        // SAFETY: valid processor.
        self.processor().map_or(0, |p| unsafe { p.getTailSamples() })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn bus_count(&self, media: sv::MediaTypes, dir: sv::BusDirections) -> i32 {
        // SAFETY: valid component.
        unsafe { self.component.getBusCount(media as sv::MediaType, dir as sv::BusDirection) }.clamp(0, MAX_BUSES)
    }

    fn arrangement(&self, dir: sv::BusDirections, index: i32) -> Option<sv::SpeakerArrangement> {
        let p = self.processor()?;
        let mut arr: sv::SpeakerArrangement = 0;
        // SAFETY: valid processor; `arr` is writable.
        ok(unsafe { p.getBusArrangement(dir as sv::BusDirection, index, &mut arr) }).then_some(arr)
    }

    fn bus_channels(&self, dir: sv::BusDirections, index: i32) -> u32 {
        if let Some(arr) = self.arrangement(dir, index) {
            return arr.count_ones().min(MAX_BUS_CHANNELS);
        }
        // SAFETY: plain data; all-zero is valid.
        let mut info: sv::BusInfo = unsafe { std::mem::zeroed() };
        // SAFETY: valid component; index below the bus count; `info` is writable.
        if ok(unsafe { self.component.getBusInfo(sv::MediaTypes_::kAudio as sv::MediaType, dir as sv::BusDirection, index, &mut info) }) {
            u32::try_from(info.channelCount).unwrap_or(0).min(MAX_BUS_CHANNELS)
        } else {
            0
        }
    }

    /// Stops processing, deactivates, negotiates the bus layout (main buses mono or stereo as
    /// asked, falling back to stereo and then to the plugin's own choice), activates the main
    /// buses, sets up 32-bit processing and activates the component.
    pub fn configure(&mut self, sample_rate: f64, max_frames: usize, mono: bool) -> Result<Layout, Vst3Error> {
        self.shutdown();
        let audio = sv::MediaTypes_::kAudio;
        let event = sv::MediaTypes_::kEvent;
        let (din, dout) = (sv::BusDirections_::kInput, sv::BusDirections_::kOutput);
        let nin = self.bus_count(audio, din);
        let nout = self.bus_count(audio, dout);
        let current = |s: &Self, dir: sv::BusDirections, n: i32| -> Vec<sv::SpeakerArrangement> {
            (0..n).map(|i| s.arrangement(dir, i).unwrap_or(sv::SpeakerArr::kStereo)).collect()
        };
        let want = if mono { sv::SpeakerArr::kMono } else { sv::SpeakerArr::kStereo };
        let p = self.processor().ok_or_else(|| Vst3Error::Process("no audio processor".into()))?.clone();
        let mut tried_ok = false;
        for main in [want, sv::SpeakerArr::kStereo] {
            let mut ins = current(self, din, nin);
            let mut outs = current(self, dout, nout);
            if let Some(a) = ins.first_mut() {
                *a = main;
            }
            if let Some(a) = outs.first_mut() {
                *a = main;
            }
            // SAFETY: valid processor; the arrays hold exactly `nin`/`nout` arrangements and
            // outlive the call.
            if ok(unsafe { p.setBusArrangements(ins.as_mut_ptr(), nin, outs.as_mut_ptr(), nout) }) {
                tried_ok = true;
                break;
            }
            if main == sv::SpeakerArr::kStereo {
                break;
            }
        }
        if !tried_ok {
            log::debug!("vst3: plugin refused mono/stereo bus arrangements; using its own");
        }
        let layout = Layout {
            inputs: (0..nin).map(|i| self.bus_channels(din, i)).collect(),
            outputs: (0..nout).map(|i| self.bus_channels(dout, i)).collect(),
            event_input: self.bus_count(event, din) > 0,
        };
        // Main buses on, aux buses off (VST3 still gets buffers for every bus).
        for (dir, n) in [(din, nin), (dout, nout)] {
            for i in 0..n {
                // SAFETY: valid component; index below the bus count.
                unsafe { self.component.activateBus(audio as sv::MediaType, dir as sv::BusDirection, i, u8::from(i == 0)) };
            }
        }
        if layout.event_input {
            // SAFETY: valid component; event input bus 0 exists.
            unsafe { self.component.activateBus(event as sv::MediaType, din as sv::BusDirection, 0, 1) };
        }
        self.event_in = layout.event_input;
        let sample32 = sv::SymbolicSampleSizes_::kSample32 as i32;
        // SAFETY: valid processor.
        if !ok(unsafe { p.canProcessSampleSize(sample32) }) {
            return Err(Vst3Error::Process("plugin cannot process 32-bit float".into()));
        }
        let max = i32::try_from(max_frames).unwrap_or(i32::MAX);
        let mut setup = sv::ProcessSetup {
            processMode: sv::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: sample32,
            maxSamplesPerBlock: max,
            sampleRate: sample_rate,
        };
        // SAFETY: valid processor, inactive; `setup` is a valid struct for the call.
        if !ok(unsafe { p.setupProcessing(&mut setup) }) {
            return Err(Vst3Error::Process("setupProcessing failed".into()));
        }
        // SAFETY: valid component, configured above.
        if !ok(unsafe { self.component.setActive(1) }) {
            return Err(Vst3Error::Process("setActive failed".into()));
        }
        self.active = true;
        self.max_frames = max_frames;
        self.context.sampleRate = sample_rate;
        self.context.tempo = 120.0;
        self.context.timeSigNumerator = 4;
        self.context.timeSigDenominator = 4;
        // The flag constants are the C default enum type: i32 on Windows, u32 elsewhere.
        #[allow(clippy::unnecessary_cast)]
        let flags = (sv::ProcessContext_::StatesAndFlags_::kTempoValid | sv::ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;
        self.context.state = flags;
        self.host.latency_changed.store(false, Ordering::Relaxed);
        Ok(layout)
    }

    /// `setProcessing(false)` + `setActive(false)` when needed.
    pub fn shutdown(&mut self) {
        if self.processing {
            if let Some(p) = self.processor() {
                // SAFETY: valid processor in the processing state.
                unsafe { p.setProcessing(0) };
            }
            self.processing = false;
        }
        if self.active {
            // SAFETY: valid, active component.
            unsafe { self.component.setActive(0) };
            self.active = false;
        }
    }

    /// Runs one `process` call over `frames` frames: `params` become one point each at offset 0,
    /// `controls` (time, id, value) one point at their time when it falls in this call (the last
    /// one wins per parameter), and `notes` (sorted by time) have their times made relative to
    /// `offset`.
    pub fn process(
        &mut self,
        frames: usize,
        inputs: &mut BusBuffers,
        outputs: &mut BusBuffers,
        params: &[(u32, f64)],
        controls: &[(u32, u32, f64)],
        notes: &[NoteEvent],
        offset: u32,
        project_time: i64,
        transport: Option<&Transport>,
    ) -> Result<(), Vst3Error> {
        if !self.active {
            return Err(Vst3Error::Process("not active".into()));
        }
        if frames == 0 || frames > self.max_frames || frames > inputs.frames || frames > outputs.frames {
            return Err(Vst3Error::Process("block larger than the configured size".into()));
        }
        let p = self.processor.clone().ok_or_else(|| Vst3Error::Process("no audio processor".into()))?;
        if !self.processing {
            // Many plugins return kNotImplemented here; that is not an error.
            // SAFETY: valid, active processor.
            unsafe { p.setProcessing(1) };
            self.processing = true;
        }
        self.in_params.clear();
        for &(id, v) in params.iter().take(PARAM_QUEUE_CAP) {
            self.in_params.set(id, 0, v.clamp(0.0, 1.0));
        }
        for &(time, id, v) in controls {
            if let Some(at) = time.checked_sub(offset).filter(|&t| (t as usize) < frames) {
                self.in_params.set(id, i32::try_from(at).unwrap_or(0), v.clamp(0.0, 1.0));
            }
        }
        self.out_params.clear();
        let event_in = self.event_in;
        self.in_events.with(|v| {
            v.clear();
            if event_in {
                for n in notes.iter().take(EVENT_CAP) {
                    v.push(n.to_raw(offset));
                }
            }
        });
        self.out_events.with(Vec::clear);
        inputs.refresh();
        outputs.refresh();
        self.context.continousTimeSamples = project_time;
        match transport {
            Some(t) => apply_transport(&mut self.context, t),
            None => self.context.projectTimeSamples = project_time,
        }
        let mut data = sv::ProcessData {
            processMode: sv::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: sv::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: i32::try_from(frames).unwrap_or(0),
            numInputs: i32::try_from(inputs.bufs.len()).unwrap_or(0),
            numOutputs: i32::try_from(outputs.bufs.len()).unwrap_or(0),
            inputs: if inputs.bufs.is_empty() { std::ptr::null_mut() } else { inputs.bufs.as_mut_ptr() },
            outputs: if outputs.bufs.is_empty() { std::ptr::null_mut() } else { outputs.bufs.as_mut_ptr() },
            inputParameterChanges: self.in_params.as_com_ref::<sv::IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputParameterChanges: self.out_params.as_com_ref::<sv::IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            inputEvents: self.in_events.as_com_ref::<sv::IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            outputEvents: self.out_events.as_com_ref::<sv::IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            processContext: &mut self.context,
        };
        // SAFETY: valid, active processor. Every channel pointer refers to `frames` (or more) valid
        // f32s owned by `inputs`/`outputs`, borrowed for the whole call; the bus tables, the
        // parameter/event objects and the context are owned by `self` and untouched until the
        // call returns.
        let r = unsafe { p.process(&mut data) };
        if ok(r) { Ok(()) } else { Err(Vst3Error::Process(format!("process returned {r}"))) }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Close any editor and cut its handles off before the controller goes away.
        if self.editor.kill() {
            self.bundle.mark_leaked();
        }
        self.shutdown();
        if let Some((a, b)) = self.connections.take() {
            // SAFETY: the pair was connected in `instantiate`; disconnect before terminating.
            unsafe {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
        }
        if let Some(c) = self.controller.take() {
            // SAFETY: valid controller; detach our handler before it goes away.
            unsafe { c.setComponentHandler(std::ptr::null_mut()) };
            if self.controller_initialized {
                // SAFETY: separately created controller, initialised once; terminated once.
                unsafe { c.terminate() };
            }
        }
        self.processor = None;
        if self.component_initialized {
            // SAFETY: valid component, inactive; terminated exactly once.
            unsafe { self.component.terminate() };
            self.component_initialized = false;
        }
    }
}

/// Splits a state blob into (component, controller) parts. A blob without SoundCraft's container
/// header is taken as a bare component state.
fn split_state(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.len() > MAX_STATE_BYTES.saturating_mul(2) {
        return None;
    }
    let Some(rest) = data.strip_prefix(STATE_MAGIC) else { return (!data.is_empty()).then_some((data, &[][..])) };
    let take = |b: &[u8]| -> Option<(usize, usize)> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        (n <= MAX_STATE_BYTES && b.len() >= 4 + n).then_some((4, 4 + n))
    };
    let (a, b) = take(rest)?;
    let comp = rest.get(a..b)?;
    let rest = rest.get(b..)?;
    let (c, d) = take(rest)?;
    let ctrl = rest.get(c..d)?;
    (d == rest.len()).then_some((comp, ctrl))
}

// ---- editor (IPlugView) --------------------------------------------------------------------

/// The `IPlugFrame` handed to a plugin view: `resizeView` resizes our host window.
pub(crate) struct PlugFrame {
    /// The host window (an `NSWindow*` on macOS), not owned; null when there is none.
    window: std::sync::atomic::AtomicPtr<c_void>,
    /// Shared with the host window's resize handling (delegate / window procedure).
    sizer: Arc<ViewSizer>,
}

impl Class for PlugFrame {
    type Interfaces = (sb::IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut sb::IPlugView, new_size: *mut sb::ViewRect) -> sb::tresult {
        // Like Steinberg's editorhost: only the attached view may resize the window, and not
        // again from inside its own resize (a view that re-requests a size from `onSize` would
        // otherwise recurse until the stack overflows).
        if view.is_null() || view != self.sizer.attached() {
            return sb::kInvalidArgument;
        }
        // SAFETY: in-pointer from the plugin (or null, handled).
        let Some(r) = (unsafe { new_size.as_mut() }) else { return sb::kInvalidArgument };
        let Some(_resizing) = self.sizer.plugin_resize() else { return sb::kResultFalse };
        let (w, h) = rect_size(r);
        // The window notifications this resize causes are not user resizes: don't negotiate them.
        let _guard = self.sizer.applying();
        window::resize(self.window.load(Ordering::Relaxed), w, h);
        self.sizer.set_last(w, h);
        // SAFETY: the plugin passes its own live view (checked against the attached one above).
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            // SAFETY: valid view; `r` is the plugin's own writable rect.
            unsafe { v.onSize(r) };
        }
        sb::kResultOk
    }
}

/// Width and height of a view rect, clamped to a sane window size.
fn rect_size(r: &sb::ViewRect) -> (f64, f64) {
    let w = r.right.saturating_sub(r.left).clamp(1, 16_384);
    let h = r.bottom.saturating_sub(r.top).clamp(1, 16_384);
    (f64::from(w), f64::from(h))
}

/// A view rect of `w`×`h` at the origin (non-finite or out-of-range sizes are clamped).
fn size_rect(w: f64, h: f64) -> sb::ViewRect {
    let px = |v: f64| if v.is_finite() { v.round().clamp(1.0, 16_384.0) as i32 } else { 1 };
    sb::ViewRect { left: 0, top: 0, right: px(w), bottom: px(h) }
}

type ViewSlot = Option<ComPtr<sb::IPlugView>>;

/// Negotiates user resizes of a host window with its plugin view (VST3: the host asks
/// `checkSizeConstraint`, sizes the window to the answer, then calls `onSize`).
///
/// Shared by the plug frame (plugin-initiated `resizeView`) and the platform window's resize
/// handling (an `NSWindowDelegate` on macOS, the window procedure on Windows). Everything runs on
/// the main thread; the view lock only guards against the instance clearing the view from another
/// thread while a resize calls into it. Loops are cut in two places: while the host applies a
/// size (a plugin `resizeView`, or a user resize being constrained and handed to `onSize`), the
/// window notifications this causes are ignored rather than negotiated again (`applying`), and a
/// `resizeView` from inside another one is refused (`plugin_resizing`).
pub(crate) struct ViewSizer {
    view: Mutex<ViewSlot>,
    /// The thread inside a call into the view under `view`'s lock (see `with_view`).
    holder: Mutex<Option<std::thread::ThreadId>>,
    /// A view change requested re-entrantly by the holder thread, applied when its call returns.
    pending: Mutex<Option<ViewSlot>>,
    /// The attached view (identity only, never dereferenced): the one allowed to `resizeView`.
    attached: std::sync::atomic::AtomicPtr<sb::IPlugView>,
    /// The view said `canResize`: user resizes are negotiated (the window has a resize handle).
    resizable: AtomicBool,
    /// Nesting depth of resizes being applied by the host.
    applying: AtomicU32,
    /// A plugin `resizeView` is in progress.
    plugin_resizing: AtomicBool,
    /// The size last given to the view (`width << 32 | height`), to skip repeated `onSize`.
    last: AtomicU64,
}

/// Ends an "applying" section of a [`ViewSizer`] when dropped.
pub(crate) struct Applying<'a>(&'a AtomicU32);

impl Drop for Applying<'_> {
    fn drop(&mut self) {
        // Paired with the increment in `ViewSizer::applying`: never below zero.
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Ends a plugin `resizeView` of a [`ViewSizer`] when dropped.
pub(crate) struct PluginResize<'a>(&'a AtomicBool);

impl Drop for PluginResize<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl ViewSizer {
    pub fn new(w: f64, h: f64) -> ViewSizer {
        let s = ViewSizer {
            view: Mutex::new(None),
            holder: Mutex::new(None),
            pending: Mutex::new(None),
            attached: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            resizable: AtomicBool::new(false),
            applying: AtomicU32::new(0),
            plugin_resizing: AtomicBool::new(false),
            last: AtomicU64::new(0),
        };
        s.set_last(w, h);
        s
    }

    fn holder(&self) -> std::sync::MutexGuard<'_, Option<std::thread::ThreadId>> {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<ViewSlot>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the view under its lock (`None` without a view). A view change requested from
    /// inside `f` on this thread (a plugin pumping a nested event loop that closes the editor) is
    /// deferred until `f` returns instead of deadlocking on the lock.
    fn with_view<R>(&self, f: impl FnOnce(&ComPtr<sb::IPlugView>) -> R) -> Option<R> {
        let mut g = self.view.lock().unwrap_or_else(PoisonError::into_inner);
        *self.holder() = Some(std::thread::current().id());
        let out = g.as_ref().map(f);
        *self.holder() = None;
        if let Some(v) = self.pending().take() {
            *g = v;
        }
        out
    }

    /// Replaces the view, or defers that when this thread is inside `with_view` (see there).
    /// Returns the previous view when it was replaced right away.
    fn replace_view(&self, view: ViewSlot) -> ViewSlot {
        self.attached.store(view.as_ref().map_or(std::ptr::null_mut(), |v| v.as_ptr()), Ordering::Relaxed);
        if *self.holder() == Some(std::thread::current().id()) {
            *self.pending() = Some(view);
            return None;
        }
        let mut g = self.view.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *g, view)
    }

    /// Names the view about to be attached: from now on it may `resizeView` (plugins often do
    /// from `attached`), before it is handed over with [`ViewSizer::set_view`].
    pub fn expect_view(&self, view: &ComPtr<sb::IPlugView>) {
        self.attached.store(view.as_ptr(), Ordering::Relaxed);
    }

    fn attached(&self) -> *mut sb::IPlugView {
        self.attached.load(Ordering::Relaxed)
    }

    /// Hands the attached view over (or takes it away again: `None`).
    pub fn set_view(&self, view: ViewSlot, resizable: bool) {
        self.resizable.store(resizable && view.is_some(), Ordering::Relaxed);
        drop(self.replace_view(view));
    }

    /// Takes the view away (stops negotiating) and hands its reference to the caller (`None`
    /// when the removal had to be deferred, see `with_view`).
    pub fn take_view(&self) -> ViewSlot {
        self.resizable.store(false, Ordering::Relaxed);
        self.replace_view(None)
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

    /// Starts a plugin `resizeView`; `None` while one is already in progress.
    fn plugin_resize(&self) -> Option<PluginResize<'_>> {
        (!self.plugin_resizing.swap(true, Ordering::Relaxed)).then_some(PluginResize(&self.plugin_resizing))
    }

    fn set_last(&self, w: f64, h: f64) {
        let r = size_rect(w, h);
        self.last.store((u64::from(r.right as u32) << 32) | u64::from(r.bottom as u32), Ordering::Relaxed);
    }

    fn last(&self) -> (f64, f64) {
        let v = self.last.load(Ordering::Relaxed);
        (f64::from((v >> 32) as u32), f64::from(v as u32))
    }

    /// The size the view accepts instead of `w`×`h` (`checkSizeConstraint`), or `None` when no
    /// resize should be negotiated (the view can't resize, is gone, or a resize is being applied).
    pub fn constrain(&self, w: f64, h: f64) -> Option<(f64, f64)> {
        if !self.resizable() || self.is_applying() {
            return None;
        }
        let _guard = self.applying();
        self.with_view(|view| constrain_view(view, w, h))
    }

    /// The window's content was resized to `w`×`h` by the user (or the window manager): settles
    /// on the view's constrained size, makes the window follow it through `set_window` when it
    /// differs, and tells the view with `onSize`. Ignored while the host applies a resize itself.
    pub fn user_resized(&self, w: f64, h: f64, set_window: impl FnOnce(f64, f64)) {
        if !self.resizable() || self.is_applying() {
            return;
        }
        let _guard = self.applying();
        self.with_view(|view| {
            let (cw, ch) = constrain_view(view, w, h);
            let want = size_rect(w, h);
            let mut r = size_rect(cw, ch);
            if (r.right, r.bottom) != (want.right, want.bottom) {
                set_window(cw, ch);
            }
            if self.last() != (cw, ch) {
                self.set_last(cw, ch);
                // SAFETY: valid, attached view (the editor link takes it away before `removed`,
                // deferred to here when that happens inside this call); main thread; `r` is a
                // writable local rect.
                unsafe { view.onSize(&mut r) };
            }
        });
    }
}

/// `checkSizeConstraint` for a proposed `w`×`h`: the plugin adjusts the rect in place to a size it
/// supports. A plugin that doesn't implement it leaves the rect alone (the proposal stands).
fn constrain_view(view: &ComPtr<sb::IPlugView>, w: f64, h: f64) -> (f64, f64) {
    let mut r = size_rect(w, h);
    // SAFETY: valid view; `r` is a writable local rect.
    unsafe { view.checkSizeConstraint(&mut r) };
    rect_size(&r)
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

#[derive(Default)]
struct EditorInner {
    /// `None` once the instance is gone.
    component: Option<ComPtr<sv::IComponent>>,
    /// `None` once the instance is gone, or without an edit controller.
    controller: Option<ComPtr<sv::IEditController>>,
    view: Option<ComPtr<sb::IPlugView>>,
    frame: Option<ComWrapper<PlugFrame>>,
    window: Option<window::HostWindow>,
}

/// The link between an instance and its editor and state handles. The instance may process on
/// the audio thread while the editor (the edit controller's `IPlugView`) is driven and the state
/// read from the main thread, which is VST3's own threading model (controller, views and
/// `getState` on the UI thread). All such calls happen under this lock, and the instance closes
/// the view and drops the component and controller references under the same lock before it
/// terminates, so a handle never calls into a dead plugin.
#[derive(Default)]
pub(crate) struct EditorLink {
    inner: Mutex<EditorInner>,
}

impl EditorLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, EditorInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_controller(&self) -> bool {
        self.lock().controller.is_some()
    }

    fn create_view(g: &EditorInner) -> Result<ComPtr<sb::IPlugView>, String> {
        let c = g.controller.as_ref().ok_or_else(|| "the plugin is gone".to_string())?;
        // SAFETY: valid controller; the name is a static NUL-terminated string.
        let raw = unsafe { c.createView(c"editor".as_ptr()) };
        // SAFETY: a non-null view comes with one reference owned by the caller.
        unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| "this plugin has no editor".to_string())
    }

    /// Creates the view, checks it supports this platform's window type and reads its size,
    /// without attaching it anywhere. For diagnostics and tests.
    pub fn probe(&self) -> Result<(u32, u32), String> {
        let g = self.lock();
        let view = EditorLink::create_view(&g)?;
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "plugin editors are not supported on this platform".to_string())?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let mut r = sb::ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: valid view; `r` is writable.
        if !ok(unsafe { view.getSize(&mut r) }) {
            return Err("the editor reports no size".into());
        }
        let (w, h) = rect_size(&r);
        Ok((w as u32, h as u32))
    }

    /// Creates the view and asks it how it would take a user resize to `w`×`h`, without
    /// attaching it: `None` when it can't resize (`canResize`), else the size `checkSizeConstraint`
    /// settles on. The same negotiation the host window runs, except that `canResize` and
    /// `checkSizeConstraint` are asked before `attached` (the window asks once attached), which
    /// some plugins answer differently. For diagnostics and tests.
    pub fn probe_resize(&self, w: u32, h: u32) -> Result<Option<(u32, u32)>, String> {
        let g = self.lock();
        let view = EditorLink::create_view(&g)?;
        // SAFETY: valid view.
        let resizable = unsafe { view.canResize() } == sb::kResultTrue;
        let sizer = ViewSizer::new(f64::from(w), f64::from(h));
        sizer.set_view(Some(view), resizable);
        let out = sizer.constrain(f64::from(w), f64::from(h)).map(|(a, b)| (a as u32, b as u32));
        sizer.set_view(None, false);
        Ok(out)
    }

    /// Opens (or raises) the editor in a host window. Main thread only.
    pub fn open(&self, title: &str) -> Result<(), String> {
        if !window::on_main_thread() {
            return Err("plugin editors must be opened on the main thread".into());
        }
        let platform = window::PLATFORM_TYPE.ok_or_else(|| "VST3 editors are not hosted on this platform yet".to_string())?;
        let mut g = self.lock();
        if let Some(w) = &g.window {
            if w.is_visible() {
                w.raise();
                return Ok(());
            }
            // The user closed the window: start over.
            EditorLink::teardown(&mut g);
        }
        let view = EditorLink::create_view(&g)?;
        // SAFETY: valid view; `platform` is a static NUL-terminated string.
        if unsafe { view.isPlatformTypeSupported(platform) } != sb::kResultTrue {
            return Err("the editor does not support this platform's windows".into());
        }
        let mut r = sb::ViewRect { left: 0, top: 0, right: 400, bottom: 300 };
        // SAFETY: valid view; `r` is writable. On failure the default size stays.
        unsafe { view.getSize(&mut r) };
        let (w, h) = rect_size(&r);
        let mut win = window::HostWindow::new(title, w, h)?;
        let sizer = Arc::new(ViewSizer::new(w, h));
        win.set_sizer(&sizer);
        let frame = ComWrapper::new(PlugFrame { window: std::sync::atomic::AtomicPtr::new(win.raw()), sizer: sizer.clone() });
        if let Some(f) = frame.as_com_ref::<sb::IPlugFrame>() {
            // SAFETY: valid view; the frame outlives the view's attachment (kept in `g.frame`
            // and detached in `teardown`).
            unsafe { view.setFrame(f.as_ptr()) };
        }
        sizer.expect_view(&view);
        // SAFETY: valid view; the parent is the live content view of `win`, of the type named.
        if !ok(unsafe { view.attached(win.parent(), platform) }) {
            // SAFETY: valid view; detach our frame again.
            unsafe { view.setFrame(std::ptr::null_mut()) };
            win.close();
            return Err("the editor could not attach to its window".into());
        }
        // Asked once attached: some plugins only know whether their editor resizes by then.
        // SAFETY: valid, attached view.
        let resizable = unsafe { view.canResize() } == sb::kResultTrue;
        sizer.set_view(Some(view.clone()), resizable);
        win.set_resizable(resizable);
        win.raise();
        g.view = Some(view);
        g.frame = Some(frame);
        g.window = Some(win);
        Ok(())
    }

    fn teardown(g: &mut EditorInner) {
        // No more user resizes reach the view once it is being removed.
        if let Some(f) = &g.frame {
            f.sizer.set_view(None, false);
        }
        if let Some(view) = g.view.take() {
            // SAFETY: valid, attached view; main thread (callers check). `removed` before the
            // window goes, and the frame is detached before it is released.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
        }
        g.frame = None;
        if let Some(w) = g.window.take() {
            w.close();
        }
    }

    /// Hands a host-set value to the controller before a state save (`setParamNormalized`,
    /// under the lock like [`EditorLink::save_state`]). No-op once the instance is gone.
    pub fn sync_param(&self, id: u32, n: f64) {
        if !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller (the instance clears it under this lock before it terminates).
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }

    /// The component state plus the controller state, in SoundCraft's container (see
    /// `STATE_MAGIC`). `None` when the component refuses `getState` or the instance is gone.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        let component = g.component.as_ref()?;
        // SAFETY: valid component (the instance clears it under this lock before it terminates);
        // the stream lives in `read_state` for the whole call.
        let comp = Instance::read_state(|s| unsafe { component.getState(s) })?;
        let ctrl = match &g.controller {
            // SAFETY: valid controller; as above.
            Some(c) => Instance::read_state(|s| unsafe { c.getState(s) }).unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::with_capacity(12 + comp.len() + ctrl.len());
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&u32::try_from(comp.len()).ok()?.to_le_bytes());
        out.extend_from_slice(&comp);
        out.extend_from_slice(&u32::try_from(ctrl.len()).ok()?.to_le_bytes());
        out.extend_from_slice(&ctrl);
        Some(out)
    }

    /// Closes the editor. Main thread only (no-op elsewhere).
    pub fn close(&self) {
        if window::on_main_thread() {
            EditorLink::teardown(&mut self.lock());
        }
    }

    pub fn is_open(&self) -> bool {
        let g = self.lock();
        match &g.window {
            Some(w) if window::on_main_thread() => w.is_visible(),
            Some(_) => true,
            None => false,
        }
    }

    /// Main-thread housekeeping: tears the view down when the user closed its window.
    pub fn idle(&self) {
        if !window::on_main_thread() {
            return;
        }
        let mut g = self.lock();
        if g.window.as_ref().is_some_and(|w| !w.is_visible()) {
            EditorLink::teardown(&mut g);
        }
    }

    /// Normalized → plain through the controller (main thread).
    pub fn to_plain(&self, id: u32, n: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        // SAFETY: valid controller.
        let v = unsafe { c.normalizedParamToPlain(id, n.clamp(0.0, 1.0)) };
        v.is_finite().then_some(v)
    }

    /// Plain → normalized through the controller (main thread).
    pub fn to_normalized(&self, id: u32, plain: f64) -> Option<f64> {
        let g = self.lock();
        let c = g.controller.as_ref()?;
        if !plain.is_finite() {
            return None;
        }
        // SAFETY: valid controller.
        let v = unsafe { c.plainParamToNormalized(id, plain) };
        v.is_finite().then(|| v.clamp(0.0, 1.0))
    }

    /// The plugin's own text for a normalized value (`getParamStringByValue`). Main thread only
    /// (`None` elsewhere, and once the instance is gone).
    pub fn value_text(&self, id: u32, n: f64) -> Option<String> {
        if !window::on_main_thread() || !n.is_finite() {
            return None;
        }
        let g = self.lock();
        let c = g.controller.as_ref()?;
        let mut s: sv::String128 = [0; 128];
        // SAFETY: valid controller (the instance clears it under this lock before it terminates),
        // main thread; `s` is a writable String128.
        ok(unsafe { c.getParamStringByValue(id, n.clamp(0.0, 1.0), &mut s) }).then(|| read_c16(&s)).filter(|t| !t.is_empty())
    }

    /// Shows a value change in the editor (`setParamNormalized`, main thread).
    pub fn set_normalized(&self, id: u32, n: f64) {
        if !window::on_main_thread() || !n.is_finite() {
            return;
        }
        let g = self.lock();
        if let Some(c) = &g.controller {
            // SAFETY: valid controller, main thread.
            unsafe { c.setParamNormalized(id, n.clamp(0.0, 1.0)) };
        }
    }

    /// Called by the instance before it terminates: closes the editor and cuts the handles off.
    /// Returns whether plugin objects had to be leaked.
    fn kill(&self) -> bool {
        let mut g = self.lock();
        let mut leaked = false;
        if g.view.is_some() || g.window.is_some() {
            if window::on_main_thread() {
                EditorLink::teardown(&mut g);
            } else {
                // Window calls are main-thread only; leak the view and window rather than crash.
                log::warn!("vst3: editor still open while its plugin is destroyed off the main thread");
                if let Some(v) = g.view.take() {
                    std::mem::forget(v);
                    leaked = true;
                }
                // `HostWindow` has no destructor: dropping it leaks its retain on purpose. The
                // leaked window keeps its resize handling, so cut it off from the view first.
                g.window = None;
                if let Some(f) = g.frame.take() {
                    std::mem::forget(f.sizer.take_view());
                    std::mem::forget(f);
                }
            }
        }
        g.component = None;
        g.controller = None;
        leaked
    }
}

/// The platform host window for plugin views.
#[cfg(target_os = "macos")]
mod window {
    use super::ViewSizer;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSFloatingWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
    };
    use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
    use std::ffi::{c_char, c_void};
    use std::sync::Arc;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `ResizeDelegate` implements no
        // `Drop` (its ivar, an `Arc`, is dropped by objc2 when the object is deallocated).
        // No `#[name]`: objc2 then generates a crate- and version-specific name and reuses an
        // already registered class of that name (e.g. a second copy of this crate in the process)
        // instead of panicking.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = Arc<ViewSizer>]
        struct ResizeDelegate;

        // SAFETY: NSObjectProtocol has no safety requirements.
        unsafe impl NSObjectProtocol for ResizeDelegate {}

        // SAFETY: the methods below match the NSWindowDelegate selectors' signatures.
        unsafe impl NSWindowDelegate for ResizeDelegate {
            /// Live user resizes: the window only ever takes a size the view accepts.
            #[unsafe(method(windowWillResize:toSize:))]
            fn window_will_resize(&self, sender: &NSWindow, frame_size: NSSize) -> NSSize {
                // Our own reference: the sizer stays alive even if the plugin closes the editor
                // from a nested event loop during this call.
                let sizer = self.ivars().clone();
                constrained_frame(&sizer, sender, NSRect::new(NSPoint::new(0.0, 0.0), frame_size)).size
            }

            /// The zoom button: zoom to the largest size the view accepts within the default
            /// (screen) frame, keeping the window's top edge.
            #[unsafe(method(windowWillUseStandardFrame:defaultFrame:))]
            fn window_standard_frame(&self, window: &NSWindow, default_frame: NSRect) -> NSRect {
                let sizer = self.ivars().clone();
                let mut frame = constrained_frame(&sizer, window, default_frame);
                frame.origin.y = default_frame.origin.y + default_frame.size.height - frame.size.height;
                frame
            }

            /// Every completed resize (drags, zoom): settles on a constrained size and tells the
            /// view. Resizes the host applies itself are ignored by the sizer.
            #[unsafe(method(windowDidResize:))]
            fn window_did_resize(&self, notification: &NSNotification) {
                let sizer = self.ivars().clone();
                let Some(window) = notification.object().and_then(|o| o.downcast::<NSWindow>().ok()) else { return };
                let Some(view) = window.contentView() else { return };
                let size = view.frame().size;
                sizer.user_resized(size.width, size.height, |w, h| window.setContentSize(NSSize::new(w, h)));
            }
        }
    );

    /// `frame` with its content size constrained by the view (unchanged when nothing is
    /// negotiated); the origin stays.
    fn constrained_frame(sizer: &ViewSizer, window: &NSWindow, frame: NSRect) -> NSRect {
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
        fn new(mtm: MainThreadMarker, sizer: Arc<ViewSizer>) -> Retained<ResizeDelegate> {
            let this = ResizeDelegate::alloc(mtm).set_ivars(sizer);
            // SAFETY: `init` of NSObject on a freshly allocated instance whose ivars are set.
            unsafe { msg_send![super(this), init] }
        }
    }

    pub const PLATFORM_TYPE: Option<*const c_char> = Some(vst3::Steinberg::kPlatformTypeNSView);

    pub fn on_main_thread() -> bool {
        MainThreadMarker::new().is_some()
    }

    /// An `NSWindow` (titled, closable, floating above the app's windows) whose content view is
    /// the plugin view's parent. Holds one retain of the window; main thread only.
    pub struct HostWindow {
        window: *mut NSWindow,
        /// The window's delegate (one retain; the window only keeps a weak reference), or null.
        delegate: *mut ResizeDelegate,
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
            let mtm = MainThreadMarker::new().ok_or_else(|| "not on the main thread".to_string())?;
            let _app = NSApplication::sharedApplication(mtm);
            let rect = NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(w, h));
            // Plugin editors are transient windows. In particular they must not be
            // minimizable: minimized NSWindows become persistent thumbnails in the macOS Dock,
            // which made every editor visited in a session look like another running app.
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
            Ok(HostWindow { window: Retained::into_raw(window), delegate: std::ptr::null_mut() })
        }

        /// Routes the window's resizes through `sizer` (an `NSWindowDelegate`). Once per window.
        pub fn set_sizer(&mut self, sizer: &Arc<ViewSizer>) {
            let (Some(mtm), Some(w)) = (MainThreadMarker::new(), self.get()) else { return };
            if !self.delegate.is_null() {
                return;
            }
            let delegate = ResizeDelegate::new(mtm, sizer.clone());
            w.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            self.delegate = Retained::into_raw(delegate);
        }

        /// Gives the window a resize handle (the view said `canResize`), or takes it away.
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
            // transient panel, and the sizer would fight a full-screen size the view refuses).
            if resizable {
                w.setCollectionBehavior(w.collectionBehavior() | NSWindowCollectionBehavior::FullScreenNone);
            }
            if let Some(size) = size {
                w.setContentSize(size);
            }
        }

        /// The raw window pointer (for the plug frame's resizes).
        pub fn raw(&self) -> *mut c_void {
            self.window.cast()
        }

        /// The content view (`NSView*`) the plugin view attaches to; null when unavailable.
        pub fn parent(&self) -> *mut c_void {
            self.get().and_then(|w| w.contentView()).map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v).cast_mut().cast())
        }

        pub fn raise(&self) {
            if let Some(w) = self.get() {
                w.makeKeyAndOrderFront(None);
            }
        }

        pub fn is_visible(&self) -> bool {
            self.get().is_some_and(|w| w.isVisible())
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
        // SAFETY: null or a window the editor link keeps retained while the frame is attached.
        if let Some(win) = unsafe { window.cast::<NSWindow>().as_ref() } {
            win.setContentSize(NSSize::new(w, h));
        }
    }
}

/// The Windows host window: a top-level window owned by the app's active window (so it stays
/// above it and has no taskbar button) whose client area is the plugin view's `HWND` parent.
/// Closing it only hides it; the editor link notices in `idle`, detaches the view, then destroys
/// the window. Messages are dispatched by the app's own event loop on the main thread.
#[cfg(windows)]
mod window {
    use super::{ViewSizer, sizing_rect};
    use std::ffi::{c_char, c_void};
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

    pub const PLATFORM_TYPE: Option<*const c_char> = Some(vst3::Steinberg::kPlatformTypeHWND);

    /// Rust names the process's first thread "main"; the app runs its UI (and message loop) there.
    pub fn on_main_thread() -> bool {
        std::thread::current().name() == Some("main")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().filter(|&c| c != 0).chain(Some(0)).collect()
    }

    const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;

    /// The sizer stored in the window's user data by `set_sizer` (one `Arc` reference, released
    /// on `WM_NCDESTROY`), borrowed for one message.
    fn window_sizer(hwnd: HWND) -> Option<Arc<ViewSizer>> {
        // SAFETY: `hwnd` is the window the procedure was called for; its user data is 0 or a
        // pointer from `Arc::into_raw`, kept alive until `WM_NCDESTROY` (the last message).
        let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ViewSizer;
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
            // Live user resizes: keep the dragged edges at a size the view accepts.
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
            // Every completed resize: settle on the constrained size and tell the view.
            WM_SIZE if wparam != SIZE_MINIMIZED as usize => {
                if let Some(s) = window_sizer(hwnd) {
                    let (w, h) = (f64::from((lparam & 0xFFFF) as u16), f64::from(((lparam >> 16) & 0xFFFF) as u16));
                    s.user_resized(w, h, |cw, ch| resize(hwnd, cw, ch));
                }
            }
            WM_NCDESTROY => {
                // SAFETY: the window's last message: take the user data back and release the
                // `Arc` reference `set_sizer` stored there; nothing reads it afterwards.
                let p = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *const ViewSizer;
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
                let name = wide("SoundCraftVst3Editor");
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
                return Err("not on the main thread".into());
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

        /// The window handle (for the plug frame's resizes).
        pub fn raw(&self) -> *mut c_void {
            self.hwnd
        }

        /// Routes the window's resizes through `sizer` (the window procedure reads it from the
        /// window's user data). Once per window.
        pub fn set_sizer(&mut self, sizer: &Arc<ViewSizer>) {
            let Some(h) = self.live() else { return };
            if window_sizer(h).is_some() {
                return;
            }
            let p = Arc::into_raw(sizer.clone());
            // SAFETY: a live window of this thread; the pointer stays valid until `WM_NCDESTROY`
            // releases it.
            unsafe { SetWindowLongPtrW(h, GWLP_USERDATA, p as _) };
        }

        /// Gives the window a sizing border (the view said `canResize`), or takes it away,
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

        /// The `HWND` the plugin view creates its child window in.
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
                // SAFETY: our own live window; the view was detached before (see `teardown`).
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

/// Other platforms: no editor windows yet (opening reports "unsupported").
#[cfg(not(any(target_os = "macos", windows)))]
mod window {
    use std::ffi::{c_char, c_void};

    pub const PLATFORM_TYPE: Option<*const c_char> = None;

    pub fn on_main_thread() -> bool {
        true
    }

    pub struct HostWindow;

    impl HostWindow {
        pub fn new(_title: &str, _w: f64, _h: f64) -> Result<HostWindow, String> {
            Err("VST3 editors are not hosted on this platform yet".into())
        }
        pub fn raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
        pub fn set_sizer(&mut self, _sizer: &std::sync::Arc<super::ViewSizer>) {}
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
    use sv::{IAttributeListTrait as _, IMessageTrait as _};

    fn tuid(iid: [u8; 16]) -> sb::TUID {
        iid.map(|b| b as c_char)
    }

    /// Asks the host for an object the way a plugin does.
    fn create<I: Interface>(host: &ComWrapper<HostContext>, cid: [u8; 16]) -> Option<ComPtr<I>> {
        let app = host.to_com_ptr::<sv::IHostApplication>()?;
        let (mut c, mut i) = (tuid(cid), tuid(I::IID));
        let mut obj: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid TUIDs and out-pointer, as a plugin passes them.
        let r = unsafe { app.createInstance(&mut c, &mut i, &mut obj) };
        // SAFETY: on success `obj` is an owned (add-ref'ed) pointer to `I`.
        (r == sb::kResultOk).then(|| unsafe { ComPtr::from_raw(obj.cast::<I>()) }).flatten()
    }

    #[test]
    fn plugins_get_messages_and_attribute_lists_from_the_host() {
        let host = ComWrapper::new(HostContext::default());
        let msg = create::<sv::IMessage>(&host, sv::IMessage::IID).expect("IMessage");
        // SAFETY: calls through the interface with valid C strings and buffers.
        unsafe {
            msg.setMessageID(c"DatabaseInfo".as_ptr());
            assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()).to_str(), Ok("DatabaseInfo"));
            let attrs = vst3::ComRef::from_raw(msg.getAttributes()).expect("attributes");
            assert_eq!(attrs.setInt(c"count".as_ptr(), 42), sb::kResultOk);
            assert_eq!(attrs.setFloat(c"gain".as_ptr(), 0.5), sb::kResultOk);
            let text: Vec<u16> = "Helix".encode_utf16().chain([0]).collect();
            assert_eq!(attrs.setString(c"name".as_ptr(), text.as_ptr()), sb::kResultOk);
            assert_eq!(attrs.setBinary(c"blob".as_ptr(), [1u8, 2, 3].as_ptr().cast(), 3), sb::kResultOk);

            let (mut i, mut f) = (0i64, 0f64);
            assert_eq!(attrs.getInt(c"count".as_ptr(), &mut i), sb::kResultOk);
            assert_eq!(attrs.getFloat(c"gain".as_ptr(), &mut f), sb::kResultOk);
            assert_eq!((i, f), (42, 0.5));
            assert_eq!(attrs.getInt(c"gain".as_ptr(), &mut i), sb::kResultFalse, "a float is not an int");
            assert_eq!(attrs.getInt(c"missing".as_ptr(), &mut i), sb::kResultFalse);
            // A buffer of 4 units takes 3 characters and the NUL.
            let mut buf = [0xFFFFu16; 4];
            assert_eq!(attrs.getString(c"name".as_ptr(), buf.as_mut_ptr(), 8), sb::kResultOk);
            assert_eq!(buf, ['H' as u16, 'e' as u16, 'l' as u16, 0]);
            let (mut p, mut n): (*const c_void, u32) = (std::ptr::null(), 0);
            assert_eq!(attrs.getBinary(c"blob".as_ptr(), &mut p, &mut n), sb::kResultOk);
            assert_eq!(std::slice::from_raw_parts(p.cast::<u8>(), n as usize), [1, 2, 3]);
            assert_eq!(attrs.setInt(std::ptr::null(), 1), sb::kInvalidArgument, "a null id is refused");
        }
        assert!(create::<sv::IAttributeList>(&host, sv::IAttributeList::IID).is_some());
        // Anything else is still not provided.
        assert!(create::<sv::IMessage>(&host, sv::IComponentHandler::IID).is_none());
    }

    /// A plugin view that accepts 200..=800 × 100..=600 and records what the host tells it. With
    /// `nested`, it calls back into the sizer from `onSize`, the way a plugin calling
    /// `resizeView` from `onSize` makes the window notify the host again.
    struct SizedView {
        sizes: Mutex<Vec<(i32, i32)>>,
        nested: Mutex<Option<Arc<ViewSizer>>>,
        /// Closes the editor (takes the view away) from inside `onSize`, once.
        close_inside: Mutex<Option<Arc<ViewSizer>>>,
        /// Re-requests its size through this frame from inside `onSize` (as `view`, once per
        /// `onSize`), recording what the host answered.
        frame: Mutex<Option<(ComPtr<sb::IPlugFrame>, usize)>>,
        answers: Mutex<Vec<sb::tresult>>,
    }

    impl SizedView {
        fn new() -> SizedView {
            SizedView {
                sizes: Mutex::new(Vec::new()),
                nested: Mutex::new(None),
                close_inside: Mutex::new(None),
                frame: Mutex::new(None),
                answers: Mutex::new(Vec::new()),
            }
        }
    }

    impl Class for SizedView {
        type Interfaces = (sb::IPlugView,);
    }

    impl IPlugViewTrait for SizedView {
        unsafe fn isPlatformTypeSupported(&self, _t: sb::FIDString) -> sb::tresult {
            sb::kResultTrue
        }
        unsafe fn attached(&self, _p: *mut c_void, _t: sb::FIDString) -> sb::tresult {
            sb::kResultOk
        }
        unsafe fn removed(&self) -> sb::tresult {
            sb::kResultOk
        }
        unsafe fn onWheel(&self, _d: f32) -> sb::tresult {
            sb::kResultFalse
        }
        unsafe fn onKeyDown(&self, _k: sb::char16, _c: i16, _m: i16) -> sb::tresult {
            sb::kResultFalse
        }
        unsafe fn onKeyUp(&self, _k: sb::char16, _c: i16, _m: i16) -> sb::tresult {
            sb::kResultFalse
        }
        unsafe fn getSize(&self, _s: *mut sb::ViewRect) -> sb::tresult {
            sb::kResultOk
        }
        unsafe fn onSize(&self, s: *mut sb::ViewRect) -> sb::tresult {
            let r = unsafe { &*s };
            self.sizes.lock().unwrap().push((r.right - r.left, r.bottom - r.top));
            if let Some(sizer) = self.nested.lock().unwrap().clone() {
                sizer.user_resized(999.0, 999.0, |_, _| panic!("a nested notification is not negotiated"));
            }
            if let Some(sizer) = self.close_inside.lock().unwrap().take() {
                sizer.set_view(None, false);
            }
            let frame = self.frame.lock().unwrap().clone();
            if let Some((f, me)) = frame {
                let mut r = sb::ViewRect { left: 0, top: 0, right: 333, bottom: 222 };
                let answer = unsafe { f.resizeView(me as *mut sb::IPlugView, &mut r) };
                self.answers.lock().unwrap().push(answer);
            }
            sb::kResultOk
        }
        unsafe fn onFocus(&self, _s: sb::TBool) -> sb::tresult {
            sb::kResultOk
        }
        unsafe fn setFrame(&self, _f: *mut sb::IPlugFrame) -> sb::tresult {
            sb::kResultOk
        }
        unsafe fn canResize(&self) -> sb::tresult {
            sb::kResultTrue
        }
        unsafe fn checkSizeConstraint(&self, r: *mut sb::ViewRect) -> sb::tresult {
            let r = unsafe { &mut *r };
            r.right = r.left + (r.right - r.left).clamp(200, 800);
            r.bottom = r.top + (r.bottom - r.top).clamp(100, 600);
            sb::kResultTrue
        }
    }

    #[test]
    fn user_resizes_are_negotiated_with_the_view_without_loops() {
        let fake = ComWrapper::new(SizedView::new());
        let view = fake.to_com_ptr::<sb::IPlugView>().unwrap();
        let sizer = Arc::new(ViewSizer::new(320.0, 240.0));
        // Not resizable (yet): nothing is negotiated and the view isn't told.
        sizer.set_view(Some(view.clone()), false);
        assert_eq!(sizer.constrain(500.0, 400.0), None);
        sizer.user_resized(500.0, 400.0, |_, _| panic!("not resizable"));
        assert!(fake.sizes.lock().unwrap().is_empty());

        sizer.set_view(Some(view), true);
        assert_eq!(sizer.constrain(500.0, 400.0), Some((500.0, 400.0)));
        assert_eq!(sizer.constrain(50.0, 5000.0), Some((200.0, 600.0)), "checkSizeConstraint adjusts the drag");
        // A size the view refuses: the window is set back to the constrained size, then `onSize`.
        let mut set = None;
        sizer.user_resized(1000.0, 50.0, |w, h| set = Some((w, h)));
        assert_eq!(set, Some((800.0, 100.0)));
        assert_eq!(*fake.sizes.lock().unwrap(), vec![(800, 100)]);
        // An accepted size leaves the window alone; the same size again is not repeated.
        sizer.user_resized(640.0, 480.0, |_, _| panic!("accepted as is"));
        sizer.user_resized(640.0, 480.0, |_, _| panic!("accepted as is"));
        assert_eq!(*fake.sizes.lock().unwrap(), vec![(800, 100), (640, 480)]);

        // Notifications caused by a resize the host applies (a plugin `resizeView`) are ignored…
        {
            let _applying = sizer.applying();
            assert_eq!(sizer.constrain(300.0, 300.0), None);
            sizer.user_resized(300.0, 300.0, |_, _| panic!("ignored while applying"));
            sizer.set_last(300.0, 300.0);
        }
        assert_eq!(fake.sizes.lock().unwrap().len(), 2);
        // …and the plugin's new size is remembered: the window notification for it is a no-op.
        sizer.user_resized(300.0, 300.0, |_, _| panic!("accepted as is"));
        assert_eq!(fake.sizes.lock().unwrap().len(), 2);

        // A view that resizes itself from `onSize` doesn't start a loop (or deadlock).
        *fake.nested.lock().unwrap() = Some(sizer.clone());
        sizer.user_resized(400.0, 400.0, |_, _| panic!("accepted as is"));
        assert_eq!(fake.sizes.lock().unwrap().last(), Some(&(400, 400)));
        assert_eq!(fake.sizes.lock().unwrap().len(), 3, "the nested notification was not negotiated");
        *fake.nested.lock().unwrap() = None;

        // Closing the editor from inside `onSize` (a plugin pumping a nested event loop) neither
        // deadlocks nor leaves the view reachable: the removal is applied when the call returns.
        *fake.close_inside.lock().unwrap() = Some(sizer.clone());
        sizer.user_resized(450.0, 450.0, |_, _| panic!("accepted as is"));
        assert_eq!(fake.sizes.lock().unwrap().len(), 4);
        assert!(!sizer.resizable());
        assert!(sizer.view.lock().unwrap().is_none(), "removed once onSize returned");
        assert!(sizer.attached().is_null());
        sizer.set_view(fake.to_com_ptr::<sb::IPlugView>(), true);

        // Once the view is taken away (editor closing), nothing reaches it any more.
        assert!(sizer.take_view().is_some());
        assert_eq!(sizer.constrain(500.0, 500.0), None);
        sizer.user_resized(500.0, 500.0, |_, _| panic!("no view"));
        assert_eq!(fake.sizes.lock().unwrap().len(), 4);
    }

    #[test]
    fn plugin_resizes_come_from_the_attached_view_and_do_not_recurse() {
        let fake = ComWrapper::new(SizedView::new());
        let view = fake.to_com_ptr::<sb::IPlugView>().unwrap();
        let other = ComWrapper::new(SizedView::new());
        let other = other.to_com_ptr::<sb::IPlugView>().unwrap();
        let sizer = Arc::new(ViewSizer::new(320.0, 240.0));
        // No window (null): only the negotiation runs.
        let frame = ComWrapper::new(PlugFrame { window: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()), sizer: sizer.clone() });
        let f = frame.to_com_ptr::<sb::IPlugFrame>().unwrap();
        let mut r = sb::ViewRect { left: 0, top: 0, right: 500, bottom: 400 };
        // SAFETY: calls through our own implementation with valid pointers (or null, handled).
        unsafe {
            assert_eq!(f.resizeView(view.as_ptr(), &mut r), sb::kInvalidArgument, "nothing attached yet");
            sizer.expect_view(&view);
            assert_eq!(f.resizeView(other.as_ptr(), &mut r), sb::kInvalidArgument, "not the attached view");
            assert_eq!(f.resizeView(std::ptr::null_mut(), &mut r), sb::kInvalidArgument);
            assert_eq!(f.resizeView(view.as_ptr(), std::ptr::null_mut()), sb::kInvalidArgument);
            // The attached view resizes; asking again from inside its `onSize` is refused instead
            // of recursing.
            *fake.frame.lock().unwrap() = Some((f.clone(), view.as_ptr() as usize));
            assert_eq!(f.resizeView(view.as_ptr(), &mut r), sb::kResultOk);
            assert_eq!(*fake.sizes.lock().unwrap(), vec![(500, 400)]);
            assert_eq!(*fake.answers.lock().unwrap(), vec![sb::kResultFalse]);
            assert_eq!(sizer.last(), (500.0, 400.0));
            *fake.frame.lock().unwrap() = None;
            // The guard is released: the next request goes through.
            let mut r = sb::ViewRect { left: 0, top: 0, right: 640, bottom: 480 };
            assert_eq!(f.resizeView(view.as_ptr(), &mut r), sb::kResultOk);
            assert_eq!(sizer.last(), (640.0, 480.0));
            // Detached: refused again.
            sizer.set_view(None, false);
            assert_eq!(f.resizeView(view.as_ptr(), &mut r), sb::kInvalidArgument);
        }
    }

    #[test]
    fn sizing_moves_only_the_dragged_edges() {
        let r = (100, 100, 500, 400);
        assert_eq!(sizing_rect(8, r, (300, 200)), (100, 100, 400, 300), "bottom-right");
        assert_eq!(sizing_rect(4, r, (300, 200)), (200, 200, 500, 400), "top-left");
        assert_eq!(sizing_rect(1, r, (300, 300)), (200, 100, 500, 400), "left (height follows the constraint downwards)");
        assert_eq!(sizing_rect(5, r, (300, 200)), (100, 200, 400, 400), "top-right");
        assert_eq!(sizing_rect(7, r, (300, 200)), (200, 100, 500, 300), "bottom-left");
        assert_eq!(sizing_rect(2, (i32::MAX - 1, 0, i32::MAX, 10), (100, 10)), (i32::MAX - 1, 0, i32::MAX, 10), "saturates");
    }

    #[test]
    fn the_transport_fills_the_process_context() {
        use sv::ProcessContext_::StatesAndFlags_::*;
        // SAFETY: `ProcessContext` is plain data (numbers and small structs); all-zero is valid.
        let mut c: sv::ProcessContext = unsafe { std::mem::zeroed() };
        let t = Transport {
            playing: true,
            tempo: 90.0,
            numerator: 3,
            denominator: 4,
            samples: 96_000,
            quarters: 3.5,
            bar_start: 3.0,
            ..Transport::default()
        };
        apply_transport(&mut c, &t);
        let playing = kPlaying as u32;
        assert_eq!(c.state & playing, playing);
        assert_eq!(c.state & kRecording as u32, 0);
        assert_eq!((c.tempo, c.timeSigNumerator, c.timeSigDenominator), (90.0, 3, 4));
        assert_eq!((c.projectTimeSamples, c.projectTimeMusic, c.barPositionMusic), (96_000, 3.5, 3.0));
        apply_transport(&mut c, &Transport { playing: false, ..t });
        assert_eq!(c.state & playing, 0);
    }

    #[test]
    fn state_container_parses_strictly() {
        let mut blob = b"SCV3".to_vec();
        blob.extend_from_slice(&3u32.to_le_bytes());
        blob.extend_from_slice(b"abc");
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(b"xy");
        assert_eq!(split_state(&blob), Some((&b"abc"[..], &b"xy"[..])));
        assert_eq!(split_state(b"raw component"), Some((&b"raw component"[..], &[][..])), "bare component state");
        assert_eq!(split_state(b""), None);
        let mut trailing = blob.clone();
        trailing.push(0);
        assert_eq!(split_state(&trailing), None);
        assert_eq!(split_state(&blob[..blob.len() - 1]), None);
        let mut huge = b"SCV3".to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(split_state(&huge), None);
        assert_eq!(split_state(b"SCV3"), None);
    }

    #[test]
    fn strings_are_bounded() {
        let mut b = [0u16; 4];
        write_c16("SoundCraft", &mut b);
        assert_eq!(read_c16(&b), "Sou");
        let full: Vec<u16> = "abcd".encode_utf16().collect();
        assert_eq!(read_c16(&full), "abcd");
        let c: Vec<c_char> = b"Fx|EQ\0junk".iter().map(|&x| x as c_char).collect();
        assert_eq!(read_c8(&c), "Fx|EQ");
        write_c16("x", &mut []);
    }

    #[test]
    fn host_objects_behave() {
        let pc = ParamChanges::new(2);
        pc.set(5, 0, 0.25);
        pc.set(5, 0, 0.5);
        pc.set(6, 0, 1.0);
        pc.set(7, 0, 1.0);
        // SAFETY: test calls through our own implementation.
        unsafe {
            assert_eq!(pc.getParameterCount(), 2, "full pool drops extra params");
            assert!(pc.getParameterData(2).is_null());
            assert!(pc.getParameterData(-1).is_null());
            let q = pc.queues[0].as_com_ref::<sv::IParamValueQueue>().unwrap();
            let (mut o, mut v) = (9, 0.0);
            assert_eq!(q.getPoint(0, &mut o, &mut v), sb::kResultOk);
            assert_eq!((o, v), (0, 0.5));
            assert_ne!(q.getPoint(1, &mut o, &mut v), sb::kResultOk);
            assert_ne!(q.getPoint(0, std::ptr::null_mut(), &mut v), sb::kResultOk);
        }
        let el = EventList::new(1);
        let mut e = NoteEvent::On { time: 5, key: 60, velocity: 1.0 }.to_raw(2);
        // SAFETY: as above.
        unsafe {
            assert_eq!(el.addEvent(&mut e), sb::kResultOk);
            assert_ne!(el.addEvent(&mut e), sb::kResultOk, "bounded");
            assert_eq!(el.getEventCount(), 1);
            let mut out: sv::Event = std::mem::zeroed();
            assert_eq!(el.getEvent(0, &mut out), sb::kResultOk);
            assert_eq!(out.sampleOffset, 3);
            assert_eq!(out.__field0.noteOn.pitch, 60);
            assert_ne!(el.getEvent(1, &mut out), sb::kResultOk);
        }
        let s = MemoryStream { inner: Mutex::new((Vec::new(), 0)) };
        let mut buf = *b"hello";
        // SAFETY: as above; buffers are valid for the given sizes.
        unsafe {
            let mut n = 0;
            assert_eq!(s.write(buf.as_mut_ptr().cast(), 5, &mut n), sb::kResultOk);
            assert_eq!(n, 5);
            let mut pos = 0i64;
            assert_eq!(s.seek(1, sb::IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos), sb::kResultOk);
            let mut out = [0u8; 8];
            assert_eq!(s.read(out.as_mut_ptr().cast(), 8, &mut n), sb::kResultOk);
            assert_eq!((n, &out[..4]), (4, &b"ello"[..]));
            assert_eq!(s.seek(-100, sb::IBStream_::IStreamSeekMode_::kIBSeekEnd as i32, &mut pos), sb::kResultOk);
            assert_eq!(pos, 0);
            assert_eq!(s.write(std::ptr::null_mut(), 5, &mut n), sb::kResultFalse);
        }
        let h = HostContext::default();
        let mut name: sv::String128 = [0; 128];
        // SAFETY: as above.
        unsafe {
            assert_eq!(h.getName(&mut name), sb::kResultOk);
            assert_eq!(h.restartComponent(sv::RestartFlags_::kLatencyChanged as i32), sb::kResultOk);
        }
        assert_eq!(read_c16(&name), "SoundCraft");
        assert!(h.latency_changed.load(Ordering::Relaxed));
        assert_eq!(hex(&[0xAB; 16]), "AB".repeat(16));
    }
}
