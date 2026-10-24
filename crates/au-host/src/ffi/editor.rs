//! Audio Unit editor views (AUv2, Cocoa): the unit's own view (`kAudioUnitProperty_CocoaUI`, an
//! `AUCocoaUIBase` factory in a bundle) or CoreAudioKit's `AUGenericView`, in a SoundCraft host
//! window, with an `AUEventListener` reporting the parameter edits made in the view.
//!
//! Part of the `ffi` module (`src/ffi.rs` and this file are the only places with `unsafe`). Everything here runs on the main
//! thread, except [`EditorLink::kill`] which may run wherever the instance is dropped.
//!
//! Lifetime rules (an Audio Unit view calls into its unit for as long as it exists):
//! - teardown order: the view leaves the window and is released (inside an autorelease pool), the
//!   window closes, the listener is disposed; only then may the instance dispose of the unit;
//! - an instance dropped off the main thread while its editor is open cannot tear the view down
//!   (AppKit is main-thread only), so the unit, the view and the window are leaked instead of
//!   disposing the unit under a live view; likewise when the editor is busy in another call
//!   (the view being made) as the instance goes.
//!
//! Objective-C exceptions are not caught (catching them needs objc2's `exception` feature, a C
//! helper this crate doesn't build): a view that throws while it is created or torn down takes the
//! process down.
//!
//! The listener reports only edits announced through the AU parameter-listener API (views do
//! that); `AudioUnitSetParameter` notifies nobody, so automation the host applies that way is not
//! shown in the view (only [`EditorLink::set_param`] is).

use super::{AudioUnit, CFTypeRef, OSStatus, SCOPE_GLOBAL, release};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSFloatingWindowLevel, NSView, NSViewFrameDidChangeNotification, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

/// `kAudioUnitProperty_CocoaUI`: an `AudioUnitCocoaViewInfo`.
const PROP_COCOA_UI: u32 = 31;
/// Most view-factory class names read from one `AudioUnitCocoaViewInfo`.
const MAX_VIEW_CLASSES: usize = 16;
/// Most parameter edits queued between two `idle` calls (one entry per parameter: the latest).
const EDIT_CAP: usize = 1024;
/// Most parameters the listener watches.
const MAX_WATCHED: usize = 4096;
/// Largest editor window side (points).
const MAX_SIDE: f64 = 16_384.0;

const EVENT_PARAMETER_VALUE_CHANGE: u32 = 0;
const EVENT_END_PARAMETER_CHANGE_GESTURE: u32 = 2;

type AUEventListenerRef = *mut c_void;
type CFRunLoopRef = *mut c_void;

/// `AudioUnitParameter`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioUnitParameter {
    audio_unit: AudioUnit,
    parameter_id: u32,
    scope: u32,
    element: u32,
}

/// `AudioUnitEvent` (the argument is a union of `AudioUnitParameter` and `AudioUnitProperty`,
/// which have the same layout; only parameter events are registered).
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioUnitEvent {
    event_type: u32,
    parameter: AudioUnitParameter,
}

type AUEventListenerProc = unsafe extern "C" fn(user: *mut c_void, object: *mut c_void, event: *const AudioUnitEvent, host_time: u64, value: f32);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AUEventListenerCreate(
        proc_: AUEventListenerProc,
        user: *mut c_void,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFTypeRef,
        notification_interval: f32,
        value_change_granularity: f32,
        out: *mut AUEventListenerRef,
    ) -> OSStatus;
    fn AUEventListenerAddEventType(listener: AUEventListenerRef, object: *mut c_void, event: *const AudioUnitEvent) -> OSStatus;
    // Event listeners are disposed with the parameter-listener call (AudioUnitUtilities.h).
    fn AUListenerDispose(listener: AUEventListenerRef) -> OSStatus;
    fn AUParameterSet(listener: AUEventListenerRef, object: *mut c_void, parameter: *const AudioUnitParameter, value: f32, offset: u32) -> OSStatus;
    fn AudioUnitGetParameter(unit: AudioUnit, id: u32, scope: u32, element: u32, value: *mut f32) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    static kCFRunLoopCommonModes: CFTypeRef;
}

// `AUGenericView` lives here; also loaded on demand (see `generic_view`).
#[link(name = "CoreAudioKit", kind = "framework")]
unsafe extern "C" {}

/// How many view-factory class names an `AudioUnitCocoaViewInfo` of `size` bytes holds: one
/// `CFURLRef` (the bundle) followed by the `CFStringRef` class names. Capped.
pub(super) fn cocoa_view_class_count(size: u32) -> usize {
    let ptr = std::mem::size_of::<*const c_void>();
    ((size as usize).saturating_sub(ptr) / ptr).min(MAX_VIEW_CLASSES)
}

// ---- parameter edits -----------------------------------------------------------------------

/// The parameter edits the listener saw since the last `idle` (latest value per parameter).
struct Edits {
    unit: AudioUnit,
    queue: Mutex<Vec<(u32, f32)>>,
}

impl Edits {
    fn push(&self, id: u32, value: f32) {
        if !value.is_finite() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = q.iter_mut().find(|e| e.0 == id) {
            e.1 = value;
        } else if q.len() < EDIT_CAP {
            q.push((id, value));
        }
    }

    fn take(&self) -> Vec<(u32, f32)> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

unsafe extern "C" fn param_event(user: *mut c_void, _object: *mut c_void, event: *const AudioUnitEvent, _host_time: u64, value: f32) {
    // SAFETY: `user` is the `Edits` the listener was created with, released only after the
    // listener is disposed; `event` is null or the event being delivered.
    let (Some(edits), Some(event)) = (unsafe { (user as *const Edits).as_ref() }, unsafe { event.as_ref() }) else { return };
    let p = event.parameter;
    if p.scope != SCOPE_GLOBAL || p.audio_unit != edits.unit {
        return;
    }
    match event.event_type {
        EVENT_PARAMETER_VALUE_CHANGE => edits.push(p.parameter_id, value),
        // A gesture's end carries no value: read it (some views only notify gestures).
        EVENT_END_PARAMETER_CHANGE_GESTURE => {
            let mut v = 0f32;
            // SAFETY: the unit outlives the listener (disposed first, or leaked with it); `v`
            // is a local.
            if unsafe { AudioUnitGetParameter(edits.unit, p.parameter_id, SCOPE_GLOBAL, 0, &mut v) } == 0 {
                edits.push(p.parameter_id, v);
            }
        }
        _ => {}
    }
}

/// An `AUEventListener` for the unit's global-scope parameters and the edits it collects.
struct Listener {
    listener: AUEventListenerRef,
    /// `Box::into_raw` of the listener's user data, freed after the listener is disposed.
    edits: *const Edits,
}

impl Listener {
    /// Watches parameters `ids` of `unit`, delivering on `run_loop` in the common modes.
    fn new(unit: AudioUnit, ids: &[u32], run_loop: CFRunLoopRef) -> Result<Listener, String> {
        let edits: *const Edits = Box::into_raw(Box::new(Edits { unit, queue: Mutex::new(Vec::new()) }));
        let mut listener: AUEventListenerRef = std::ptr::null_mut();
        // SAFETY: `param_event` matches `AUEventListenerProc`; `edits` stays alive until after
        // the listener is disposed (`Drop`); the run loop and mode are CoreFoundation's own;
        // `listener` is a local out pointer.
        let st = unsafe { AUEventListenerCreate(param_event, edits.cast_mut().cast(), run_loop, kCFRunLoopCommonModes, 0.02, 0.0, &mut listener) };
        let this = Listener { listener, edits };
        if st != 0 || listener.is_null() {
            return Err(format!("AUEventListenerCreate failed ({st})"));
        }
        for &id in ids.iter().take(MAX_WATCHED) {
            let parameter = AudioUnitParameter { audio_unit: unit, parameter_id: id, scope: SCOPE_GLOBAL, element: 0 };
            for event_type in [EVENT_PARAMETER_VALUE_CHANGE, EVENT_END_PARAMETER_CHANGE_GESTURE] {
                let event = AudioUnitEvent { event_type, parameter };
                // SAFETY: a live listener; `event` is a local describing a live unit's parameter.
                unsafe { AUEventListenerAddEventType(listener, std::ptr::null_mut(), &event) };
            }
        }
        Ok(this)
    }

    fn edits(&self) -> &Edits {
        // SAFETY: `edits` is a live `Box` (freed only in `Drop`).
        unsafe { &*self.edits }
    }

    /// Sets a parameter and tells every other listener (the view), not this one: no echo.
    fn set(&self, unit: AudioUnit, id: u32, value: f32) {
        let parameter = AudioUnitParameter { audio_unit: unit, parameter_id: id, scope: SCOPE_GLOBAL, element: 0 };
        // SAFETY: a live listener and unit; `parameter` is a local.
        unsafe { AUParameterSet(self.listener, std::ptr::null_mut(), &parameter, value, 0) };
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if !self.listener.is_null() {
            // SAFETY: our live listener, disposed once; no callback runs after this returns
            // (callbacks are delivered on the main run loop, which is this thread).
            unsafe { AUListenerDispose(self.listener) };
        }
        // SAFETY: balances the `Box::into_raw` in `new`; the listener no longer refers to it.
        drop(unsafe { Box::from_raw(self.edits.cast_mut()) });
    }
}

// ---- views -----------------------------------------------------------------------------------

/// `[obj sel]` → object, without encoding checks (raw `objc_msgSend`).
///
/// # Safety
/// `obj` is a live object or class that responds to `sel`, which takes no arguments and returns
/// an object pointer.
unsafe fn send_obj(obj: *const c_void, sel: Sel) -> *mut AnyObject {
    // SAFETY: per the contract; `objc_msgSend` is called with the signature of the method.
    unsafe {
        let f: unsafe extern "C-unwind" fn(*const c_void, Sel) -> *mut AnyObject =
            std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
        f(obj, sel)
    }
}

/// The unit's own Cocoa view (`kAudioUnitProperty_CocoaUI`), retained; `None` when it has none
/// or it can't be made. Call inside an autorelease pool.
fn cocoa_view(unit: AudioUnit) -> Option<Retained<NSView>> {
    let mut size = 0u32;
    let mut writable = 0u8;
    // SAFETY: a live unit; locals as out pointers.
    let st = unsafe { super::AudioUnitGetPropertyInfo(unit, PROP_COCOA_UI, SCOPE_GLOBAL, 0, &mut size, &mut writable) };
    let n = cocoa_view_class_count(size);
    if st != 0 || n == 0 {
        return None;
    }
    // `{ CFURLRef bundle; CFStringRef classes[n]; }`, every pointer returned +1.
    let mut info: Vec<CFTypeRef> = vec![std::ptr::null(); n + 1];
    let mut size = ((n + 1) * std::mem::size_of::<CFTypeRef>()) as u32;
    // SAFETY: `info` has room for `size` bytes of pointers.
    let st = unsafe { super::AudioUnitGetProperty(unit, PROP_COCOA_UI, SCOPE_GLOBAL, 0, info.as_mut_ptr().cast(), &mut size) };
    if st != 0 {
        info.iter().for_each(|&cf| release(cf));
        return None;
    }
    let got = cocoa_view_class_count(size).min(n);
    let view = (|| {
        let url = *info.first()?;
        if url.is_null() {
            return None;
        }
        // SAFETY: a CFURL is toll-free bridged to NSURL; `bundleWithURL:` takes it and returns
        // an autoreleased bundle or nil.
        let bundle: Option<Retained<AnyObject>> = unsafe { msg_send![objc2::class!(NSBundle), bundleWithURL: url.cast::<AnyObject>()] };
        let bundle = bundle?;
        info.iter()
            .skip(1)
            .take(got)
            .find_map(|&name| {
                if name.is_null() {
                    return None;
                }
                // SAFETY: a live bundle; a CFString is toll-free bridged to NSString. `classNamed:`
                // loads the bundle's code and returns the class or nil.
                let class: *const AnyClass = unsafe { msg_send![&*bundle, classNamed: name.cast::<AnyObject>()] };
                view_from_factory(class, unit)
            })
            .or_else(|| {
                // SAFETY: as above; the bundle's principal class or nil.
                let class: *const AnyClass = unsafe { msg_send![&*bundle, principalClass] };
                view_from_factory(class, unit)
            })
    })();
    for &cf in &info {
        release(cf);
    }
    view
}

/// A view from the `AUCocoaUIBase` view factory `class` (null: none).
fn view_from_factory(class: *const AnyClass, unit: AudioUnit) -> Option<Retained<NSView>> {
    // SAFETY: null, or a class registered with the runtime (classes live for the process).
    let class = unsafe { class.as_ref() }?;
    let selector = sel!(uiViewForAudioUnit:withSize:);
    // SAFETY: `instancesRespondToSelector:` is an NSObject class method.
    let responds: Bool = unsafe { msg_send![class, instancesRespondToSelector: selector] };
    if !responds.as_bool() {
        log::warn!("au: view factory {} does not implement uiViewForAudioUnit:withSize:", class.name().to_string_lossy());
        return None;
    }
    // SAFETY: `new` on an NSObject subclass returns a +1 instance or nil.
    let factory: Option<Retained<AnyObject>> = unsafe { msg_send![class, new] };
    let factory = factory?;
    // SAFETY: the factory implements `-(NSView*)uiViewForAudioUnit:(AudioUnit)withSize:(NSSize)`
    // (checked above); the unit is live. The view comes back autoreleased (we are in a pool).
    let view = unsafe {
        let f: unsafe extern "C-unwind" fn(*const AnyObject, Sel, AudioUnit, NSSize) -> *mut AnyObject =
            std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
        f(Retained::as_ptr(&factory), selector, unit, NSSize::new(0.0, 0.0))
    };
    // SAFETY: null or a live (autoreleased) object; retaining it makes it ours.
    let view = unsafe { Retained::retain(view) }?;
    // The factory is released here; the view keeps whatever it needs.
    drop(factory);
    view.downcast::<NSView>().ok()
}

/// CoreAudioKit's generic view for the unit (sliders for its parameters), retained.
fn generic_view(unit: AudioUnit) -> Option<Retained<NSView>> {
    let class = AnyClass::get(c"AUGenericView").or_else(|| {
        // Not linked in (or stripped): load the framework.
        let path = NSString::from_str("/System/Library/Frameworks/CoreAudioKit.framework");
        // SAFETY: `bundleWithPath:` returns an autoreleased bundle or nil.
        let bundle: Option<Retained<AnyObject>> = unsafe { msg_send![objc2::class!(NSBundle), bundleWithPath: &*path] };
        // SAFETY: `load` on a live bundle returns BOOL.
        let _: Bool = unsafe { msg_send![&*bundle?, load] };
        AnyClass::get(c"AUGenericView")
    })?;
    // SAFETY: `alloc` on a class returns an uninitialised instance (or nil); `initWithAudioUnit:`
    // takes a live unit and returns the +1 initialised view or nil (releasing the allocation).
    let view = unsafe {
        let obj = send_obj((class as *const AnyClass).cast(), sel!(alloc));
        if obj.is_null() {
            return None;
        }
        let f: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, AudioUnit) -> *mut AnyObject =
            std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
        f(obj, sel!(initWithAudioUnit:), unit)
    };
    // SAFETY: null or a +1 object from an `init` method: ownership moves to the `Retained`.
    let view = unsafe { Retained::from_raw(view) }?;
    view.downcast::<NSView>().ok()
}

/// A usable view size: finite, positive, capped; `None` otherwise.
fn usable(size: NSSize) -> Option<NSSize> {
    let ok = |v: f64| v.is_finite() && v >= 1.0;
    (ok(size.width) && ok(size.height)).then(|| NSSize::new(size.width.min(MAX_SIDE), size.height.min(MAX_SIDE)))
}

// ---- host window -----------------------------------------------------------------------------

/// The delegate's state: whether the user closed the window.
struct Ivars {
    closed: Arc<AtomicBool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `EditorDelegate` implements no `Drop`
    // (its ivars are dropped by objc2 when the object is deallocated). No `#[name]`: objc2 then
    // generates a crate- and version-specific name (no clash with the other hosts' delegates or a
    // second copy of this crate).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct EditorDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for EditorDelegate {}

    // SAFETY: the method below matches the NSWindowDelegate selector's signature.
    unsafe impl NSWindowDelegate for EditorDelegate {
        /// The user closed the window (our own `close` clears the delegate first). Tracked here
        /// rather than read from `isVisible`, which is also false while the app is inactive and
        /// AppKit has hidden the window (`hidesOnDeactivate`).
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.ivars().closed.store(true, Ordering::Relaxed);
        }
    }

    impl EditorDelegate {
        /// The view changed its own size: the window follows it.
        #[unsafe(method(viewFrameDidChange:))]
        fn view_frame_did_change(&self, notification: &NSNotification) {
            let Some(view) = notification.object().and_then(|o| o.downcast::<NSView>().ok()) else { return };
            fit_window(&view);
        }
    }
);

impl EditorDelegate {
    fn new(mtm: MainThreadMarker, closed: Arc<AtomicBool>) -> Retained<EditorDelegate> {
        let this = EditorDelegate::alloc(mtm).set_ivars(Ivars { closed });
        // SAFETY: `init` of NSObject on a freshly allocated instance whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

/// Sizes the view's window to the view, keeping the view at the content view's origin.
fn fit_window(view: &NSView) {
    let (Some(window), Some(size)) = (view.window(), usable(view.frame().size)) else { return };
    let Some(content) = window.contentView() else { return };
    if view.frame().origin != NSPoint::new(0.0, 0.0) {
        view.setFrameOrigin(NSPoint::new(0.0, 0.0));
    }
    if content.frame().size != size {
        // The view already has its new size: don't let the content view resize it again.
        let autoresizes = content.autoresizesSubviews();
        content.setAutoresizesSubviews(false);
        window.setContentSize(size);
        content.setAutoresizesSubviews(autoresizes);
    }
}

/// An `NSWindow` (titled, closable, floating above the app's windows, never minimized) holding
/// the unit's view. Holds one retain of the window and of its delegate.
struct HostWindow {
    window: Retained<NSWindow>,
    delegate: Retained<EditorDelegate>,
    closed: Arc<AtomicBool>,
}

impl HostWindow {
    fn new(mtm: MainThreadMarker, title: &str, size: NSSize) -> HostWindow {
        let _app = NSApplication::sharedApplication(mtm);
        let rect = NSRect::new(NSPoint::new(200.0, 200.0), size);
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        // SAFETY: a freshly allocated window on the main thread; valid rect and style.
        let window =
            unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false) };
        // SAFETY: we keep our own retain and release it after `close`, so AppKit must not release
        // the window when the user closes it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        window.setLevel(NSFloatingWindowLevel);
        window.setHidesOnDeactivate(true);
        window.setExcludedFromWindowsMenu(true);
        window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
        window.center();
        let closed = Arc::new(AtomicBool::new(false));
        let delegate = EditorDelegate::new(mtm, closed.clone());
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        HostWindow { window, delegate, closed }
    }

    /// Puts `view` in the window, sized to it, and follows its size changes. The window is
    /// user-resizable only when the view resizes with its superview in both directions
    /// (`autoresizingMask`); a view that stretches one way only gets a fixed window.
    fn attach(&self, view: &NSView) {
        let Some(content) = self.window.contentView() else { return };
        content.addSubview(view);
        view.setFrameOrigin(NSPoint::new(0.0, 0.0));
        let sizable = NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable;
        if view.autoresizingMask().contains(sizable) {
            self.window.setStyleMask(self.window.styleMask() | NSWindowStyleMask::Resizable);
            // A resizable window gets a zoom button; keep it out of full screen (a transient panel).
            self.window.setCollectionBehavior(self.window.collectionBehavior() | NSWindowCollectionBehavior::FullScreenNone);
        }
        view.setPostsFrameChangedNotifications(true);
        // SAFETY: the delegate implements `viewFrameDidChange:` (one `NSNotification*` argument);
        // it stays registered until `detach`, before the delegate or the view goes away.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &self.delegate,
                sel!(viewFrameDidChange:),
                Some(NSViewFrameDidChangeNotification),
                Some(view),
            );
        }
        fit_window(view);
    }

    /// Stops following `view` and takes it out of the window.
    fn detach(&self, view: &NSView) {
        // SAFETY: removes the registration made in `attach` (a no-op when there is none).
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.delegate) };
        view.removeFromSuperview();
    }

    fn raise(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    /// Not closed by the user (a window hidden while the app is inactive is still open).
    fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    fn close(self) {
        self.window.setDelegate(None);
        self.window.orderOut(None);
        self.window.close();
        // Released when the event loop's autorelease pool drains, not now: `close` may run inside
        // one of the delegate's own methods.
        let _ = Retained::autorelease_ptr(self.delegate);
    }
}

// ---- the link --------------------------------------------------------------------------------

/// An open editor: the view in its window, and the listener.
struct Open {
    window: HostWindow,
    view: Retained<NSView>,
    listener: Option<Listener>,
}

/// Tears an editor down in the safe order (main thread only; see the module docs).
fn teardown(o: Open) {
    let Open { window, view, listener } = o;
    autoreleasepool(|_| {
        window.detach(&view);
        // The view (and whatever it autoreleases while going) is gone when the pool drains.
        drop(view);
    });
    window.close();
    drop(listener);
}

struct Inner {
    /// The unit (cleared by the instance before it disposes of it).
    unit: Option<AudioUnit>,
    open: Option<Open>,
}

// SAFETY: the unit pointer is only used under the mutex while the instance keeps it alive; the
// AppKit objects in `open` are only touched on the main thread (every entry point checks), and
// off the main thread they are leaked without being touched (`kill`).
unsafe impl Send for Inner {}

/// The link between an instance and its editor handles (see the module docs).
pub(crate) struct EditorLink(Mutex<Inner>);

impl EditorLink {
    pub(super) fn new(unit: AudioUnit) -> EditorLink {
        EditorLink(Mutex::new(Inner { unit: Some(unit), open: None }))
    }

    /// The lock, or `None` when this thread already holds it (a view calling back into the host
    /// from inside one of our calls): the caller then does nothing rather than deadlock.
    fn try_lock(&self) -> Option<std::sync::MutexGuard<'_, Inner>> {
        match self.0.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            // Held elsewhere: by `kill` on another thread (wait for it), or by this thread (skip).
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Opens (or raises) the editor: the unit's own Cocoa view, else the generic view. `ids` are
    /// the global-scope parameters whose edits `idle` reports.
    pub fn open(&self, title: &str, ids: &[u32]) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or_else(|| "plugin editors must be opened on the main thread".to_string())?;
        let mut g = self.try_lock().ok_or_else(|| "the editor is busy".to_string())?;
        let unit = g.unit.ok_or_else(|| "the plugin instance is gone".to_string())?;
        if let Some(o) = &g.open {
            if o.window.is_open() {
                o.window.raise();
                return Ok(());
            }
            if let Some(o) = g.open.take() {
                teardown(o);
            }
        }
        let view =
            autoreleasepool(|_| cocoa_view(unit).or_else(|| generic_view(unit))).ok_or_else(|| "the Audio Unit has no editor view".to_string())?;
        let size = usable(view.frame().size).unwrap_or_else(|| {
            view.setFrameSize(NSSize::new(400.0, 300.0));
            NSSize::new(400.0, 300.0)
        });
        // SAFETY: the main run loop exists for the life of the process.
        let listener = match Listener::new(unit, ids, unsafe { CFRunLoopGetMain() }) {
            Ok(l) => Some(l),
            Err(e) => {
                log::warn!("au: editor edits won't be reported: {e}");
                None
            }
        };
        let window = HostWindow::new(mtm, title, size);
        window.attach(&view);
        window.raise();
        g.open = Some(Open { window, view, listener });
        Ok(())
    }

    /// Closes the editor (main thread only; no-op elsewhere).
    pub fn close(&self) {
        if MainThreadMarker::new().is_none() {
            return;
        }
        let Some(mut g) = self.try_lock() else { return };
        if let Some(o) = g.open.take() {
            teardown(o);
        }
    }

    pub fn is_open(&self) -> bool {
        if MainThreadMarker::new().is_none() {
            return false;
        }
        self.try_lock().is_some_and(|g| g.open.as_ref().is_some_and(|o| o.window.is_open()))
    }

    /// Main-thread housekeeping: frees an editor the user closed and returns the parameter edits
    /// made in the view since the last call (`(id, value)`, the unit's own values).
    pub fn idle(&self) -> Vec<(u32, f32)> {
        if MainThreadMarker::new().is_none() {
            return Vec::new();
        }
        let Some(mut g) = self.try_lock() else { return Vec::new() };
        let edits = g.open.as_ref().and_then(|o| o.listener.as_ref()).map(|l| l.edits().take()).unwrap_or_default();
        if g.open.as_ref().is_some_and(|o| !o.window.is_open())
            && let Some(o) = g.open.take()
        {
            teardown(o);
        }
        edits
    }

    /// Shows a value set elsewhere in the open view (without reporting it back as an edit).
    pub fn set_param(&self, id: u32, value: f32) {
        if MainThreadMarker::new().is_none() || !value.is_finite() {
            return;
        }
        let Some(g) = self.try_lock() else { return };
        if let (Some(unit), Some(l)) = (g.unit, g.open.as_ref().and_then(|o| o.listener.as_ref())) {
            l.set(unit, id, value);
        }
    }

    /// Called by the instance before it disposes of the unit: closes the editor. Returns `true`
    /// when that was impossible (an open editor off the main thread, or the editor busy in
    /// another call): the instance must then leak the unit, which the view may still use.
    pub(super) fn kill(&self) -> bool {
        let mut g = match self.0.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            // The main thread is inside one of our calls (making the view, say), possibly this
            // very thread from a nested event loop: never wait for it, and never dispose of the
            // unit under it. The editor is freed with the link (`Inner`'s `Drop`).
            Err(TryLockError::WouldBlock) => {
                log::warn!("au: instance dropped while its editor is busy; leaking the unit");
                return true;
            }
        };
        g.unit = None;
        let Some(o) = g.open.take() else { return false };
        if MainThreadMarker::new().is_some() {
            teardown(o);
            false
        } else {
            log::warn!("au: editor still open while its instance is dropped off the main thread; leaking the unit and its view");
            std::mem::forget(o);
            true
        }
    }
}

impl Drop for Inner {
    /// The last handle went with an editor still open (after `kill` had to leave it, see there):
    /// tear it down on the main thread, leak it elsewhere. The unit itself was leaked by `kill`.
    fn drop(&mut self) {
        if let Some(o) = self.open.take() {
            if MainThreadMarker::new().is_some() {
                teardown(o);
            } else {
                std::mem::forget(o);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cocoa_view_info_size_gives_the_class_count() {
        let p = std::mem::size_of::<*const c_void>() as u32;
        assert_eq!(cocoa_view_class_count(0), 0);
        assert_eq!(cocoa_view_class_count(p), 0, "a bundle URL without classes");
        assert_eq!(cocoa_view_class_count(2 * p), 1);
        assert_eq!(cocoa_view_class_count(3 * p + 3), 2, "a partial pointer is ignored");
        assert_eq!(cocoa_view_class_count(u32::MAX), MAX_VIEW_CLASSES, "capped");
        assert_eq!(std::mem::size_of::<AudioUnitParameter>(), 24);
        assert_eq!(std::mem::size_of::<AudioUnitEvent>(), 32);
        assert_eq!(std::mem::offset_of!(AudioUnitEvent, parameter), 8);
    }

    #[test]
    fn edits_keep_the_latest_value_per_parameter_and_are_bounded() {
        let e = Edits { unit: std::ptr::null_mut(), queue: Mutex::new(Vec::new()) };
        e.push(1, 0.5);
        e.push(2, 0.25);
        e.push(1, 0.75);
        e.push(3, f32::NAN);
        assert_eq!(e.take(), vec![(1, 0.75), (2, 0.25)]);
        assert!(e.take().is_empty());
        for id in 0..(EDIT_CAP as u32 + 10) {
            e.push(id, 1.0);
        }
        assert_eq!(e.take().len(), EDIT_CAP);
    }
}
