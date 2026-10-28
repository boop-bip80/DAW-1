//! Main-thread app ownership that releases its borrow before native menu tracking.

use crate::SoundApp;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    static HOSTS: RefCell<HashMap<u64, Weak<RefCell<SoundApp>>>> = RefCell::new(HashMap::new());
    static PRESENTERS: RefCell<HashMap<u64, Rc<dyn crate::context_menu::Presenter>>> = RefCell::new(HashMap::new());
    static SCHEDULED_MENUS: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
}

static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

pub struct NativeUiHost {
    pub state: Rc<RefCell<SoundApp>>,
    id: u64,
}

impl NativeUiHost {
    pub fn set_context_menu_presenter(&self, presenter: Rc<dyn crate::context_menu::Presenter>) -> Result<(), String> {
        let mut app = self.state.try_borrow_mut().map_err(|_| "UI host busy")?;
        PRESENTERS.with(|presenters| {
            presenters.try_borrow_mut().map_err(|_| "Context menu registry busy")?.insert(self.id, presenter);
            Ok::<(), String>(())
        })?;
        app.context_menus.enabled = true;
        Ok(())
    }

    pub fn new(app: SoundApp) -> Result<Self, String> {
        let mut id = NEXT_HOST.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).ok_or("UI host ids exhausted")?;
            match NEXT_HOST.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => id = current,
            }
        }
        let state = Rc::new(RefCell::new(app));
        HOSTS.with(|hosts| {
            hosts.try_borrow_mut().map_err(|_| "UI host registry busy")?.insert(id, Rc::downgrade(&state));
            Ok::<(), String>(())
        })?;
        Ok(Self { state, id })
    }

    pub fn ui(&self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        {
            let Ok(mut app) = self.state.try_borrow_mut() else {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
                return;
            };
            crate::dropdown::prepare(&app, &ctx);
            app.guarded(|app| app.ui(ui));
            crate::dropdown::collect(&mut app, &ctx);
        }
        present_pending(self.id, &ctx);
    }

    pub fn raw_input_hook(&self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let presenter = PRESENTERS.with(|p| p.try_borrow().ok().and_then(|p| p.get(&self.id).cloned()));
        if let Some(presenter) = presenter {
            presenter.capture_input(raw);
        }
        if let Ok(mut app) = self.state.try_borrow_mut() {
            app.raw_input_hook(ctx, raw);
        }
    }
}

fn present_pending(id: u64, ctx: &egui::Context) {
    let state = HOSTS.with(|hosts| hosts.try_borrow().ok().and_then(|hosts| hosts.get(&id).and_then(Weak::upgrade)));
    let presenter = PRESENTERS.with(|p| p.try_borrow().ok().and_then(|p| p.get(&id).cloned()));
    let (Some(state), Some(presenter)) = (state, presenter) else { return };
    let wanted = state.try_borrow().is_ok_and(|app| !app.context_menus.active && app.context_menus.pending.is_some());
    if !wanted || !SCHEDULED_MENUS.with(|scheduled| scheduled.try_borrow_mut().is_ok_and(|mut scheduled| scheduled.insert(id))) {
        return;
    }
    let repaint = ctx.clone();
    let run = Box::new(move || {
        let scheduled = SCHEDULED_MENUS.with(|scheduled| scheduled.try_borrow_mut().is_ok_and(|mut scheduled| scheduled.remove(&id)));
        if !scheduled {
            return; // The host was dropped before its queued callback ran.
        }
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| present_now(id, &repaint))).is_err() {
            fail_menu(id, &repaint, "Native context menu failed; using in-window menus".into());
        }
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| presenter.schedule(run)))
        .unwrap_or_else(|_| Err("Native context menu scheduling failed; using in-window menus".into()));
    if let Err(error) = result {
        SCHEDULED_MENUS.with(|scheduled| {
            if let Ok(mut scheduled) = scheduled.try_borrow_mut() {
                scheduled.remove(&id);
            }
        });
        fail_menu(id, ctx, error);
    }
}

fn fail_menu(id: u64, ctx: &egui::Context, error: String) {
    log::warn!("Native context menu: {error}");
    let state = HOSTS.with(|hosts| hosts.try_borrow().ok().and_then(|hosts| hosts.get(&id).and_then(Weak::upgrade)));
    if let Some(state) = state
        && let Ok(mut app) = state.try_borrow_mut()
    {
        app.context_menus.active = false;
        app.context_menus.pending = None;
        app.context_menus.enabled = false;
        app.ui.status = error;
    }
    ctx.request_repaint();
}

fn present_now(id: u64, ctx: &egui::Context) {
    let state = HOSTS.with(|hosts| hosts.try_borrow().ok().and_then(|hosts| hosts.get(&id).and_then(Weak::upgrade)));
    let presenter = PRESENTERS.with(|p| p.try_borrow().ok().and_then(|p| p.get(&id).cloned()));
    let (Some(state), Some(presenter)) = (state, presenter) else { return };
    let request = {
        let Ok(mut app) = state.try_borrow_mut() else {
            ctx.request_repaint(); // Leave the request queued for the next available UI pass.
            return;
        };
        if app.context_menus.active {
            return;
        }
        let Some(request) = app.context_menus.pending.take() else { return };
        if request.epoch != app.context_menus.epoch || !crate::context_menu::valid(&app, &request.target) {
            return;
        }
        app.context_menus.active = true;
        request
    };
    // AppKit tracks the menu in a nested event loop. Neither the application nor the
    // registries may be borrowed here: another viewport may repaint or close.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| presenter.present(&request)))
        .unwrap_or_else(|_| Err("Native context menu failed; using in-window menus".into()));
    if let Ok(mut app) = state.try_borrow_mut() {
        app.context_menus.active = false;
        match result {
            Ok(Some(selection)) if request.epoch == app.context_menus.epoch && crate::context_menu::valid(&app, &request.target) => {
                if let crate::context_menu::Action::Choice { control, index, label } = selection.action {
                    let pass = ctx.cumulative_pass_nr();
                    ctx.data_mut(|data| data.insert_temp(control, crate::dropdown::Pick { index, label, epoch: request.epoch, pass }));
                } else {
                    app.guarded(|app| crate::context_menu::execute(app, selection.action, selection.modifiers, request.viewport));
                }
            }
            Err(error) => {
                log::warn!("Native context menu: {error}");
                app.context_menus.enabled = false;
                app.ui.status = error;
            }
            _ => {}
        }
    }
    ctx.request_repaint_of(request.viewport);
    ctx.request_repaint_of(egui::ViewportId::ROOT);
}

impl Drop for NativeUiHost {
    fn drop(&mut self) {
        SCHEDULED_MENUS.with(|scheduled| {
            if let Ok(mut scheduled) = scheduled.try_borrow_mut() {
                scheduled.remove(&self.id);
            }
        });
        HOSTS.with(|hosts| {
            if let Ok(mut hosts) = hosts.try_borrow_mut() {
                hosts.remove(&self.id);
            }
        });
        PRESENTERS.with(|p| {
            if let Ok(mut p) = p.try_borrow_mut() {
                p.remove(&self.id);
            }
        });
    }
}

#[cfg(test)]
mod context_menu_tests {
    use super::*;
    use crate::context_menu::{self, Action, Presenter, Request, Selection, Target};
    use egui::{Modifiers, ViewportId};
    use serde_json::json;
    use std::cell::Cell;

    struct Mock {
        state: Weak<RefCell<SoundApp>>,
        requests: RefCell<Vec<Request>>,
        result: RefCell<Result<Option<Selection>, String>>,
        replace_session: bool,
    }

    impl Presenter for Mock {
        fn present(&self, request: &Request) -> Result<Option<Selection>, String> {
            // Reentrant callbacks must be able to borrow the app while tracking.
            let state = self.state.upgrade().unwrap();
            let mut app = state.try_borrow_mut().expect("host must release the app before tracking");
            assert!(app.context_menus.active);
            if self.replace_session {
                app.run("session.new", json!({})).unwrap();
            }
            self.requests.borrow_mut().push(request.clone());
            self.result.borrow_mut().clone()
        }
    }

    fn setup(result: Result<Option<Selection>, String>, replace_session: bool) -> (NativeUiHost, Rc<Mock>, soundcraft_model::TrackId) {
        let mut engine = soundcraft_engine::Engine::default();
        engine.execute("track.new", &json!({"name": "Voice"})).unwrap();
        let track = engine.session().tracks[0].id;
        let host = NativeUiHost::new(SoundApp::new(engine, None, crate::Services::default())).unwrap();
        let presenter =
            Rc::new(Mock { state: Rc::downgrade(&host.state), requests: RefCell::new(vec![]), result: RefCell::new(result), replace_session });
        host.set_context_menu_presenter(presenter.clone()).unwrap();
        let mut app = host.state.borrow_mut();
        let target = Target::Track(track);
        app.context_menus.pending = Some(Request {
            entries: context_menu::describe(&app, &target),
            target,
            viewport: egui::ViewportId::from_hash_of("soundcraft.mix"),
            epoch: app.context_menus.epoch,
            anchor: crate::context_menu::Anchor::Pointer,
        });
        drop(app);
        (host, presenter, track)
    }

    struct Deferred {
        state: Weak<RefCell<SoundApp>>,
        queued: RefCell<Vec<Box<dyn FnOnce()>>>,
        in_ui: Cell<bool>,
        calls: Cell<usize>,
        frames: Cell<usize>,
        ctx: egui::Context,
    }

    impl Presenter for Deferred {
        fn schedule(&self, callback: Box<dyn FnOnce()>) -> Result<(), String> {
            self.queued.borrow_mut().push(callback);
            Ok(())
        }

        fn present(&self, _: &Request) -> Result<Option<Selection>, String> {
            assert!(!self.in_ui.get(), "native tracking must start after the original UI callback returns");
            let state = self.state.upgrade().unwrap();
            assert!(state.borrow().context_menus.active);
            self.calls.set(self.calls.get() + 1);
            // Model AppKit's nested loop delivering more frames while the menu stays open.
            // This Context is the one that was rendering when the request was queued.
            for _ in 0..3 {
                let mut output = self.ctx.run_ui(egui::RawInput::default(), |ui| {
                    let mut app = state.try_borrow_mut().expect("menu tracking must not own the app");
                    app.logic(ui.ctx());
                    app.ui(ui);
                    self.frames.set(self.frames.get() + 1);
                });
                output.textures_delta.clear();
            }
            Ok(None)
        }
    }

    fn deferred() -> (NativeUiHost, Rc<Deferred>) {
        let (host, _, _) = setup(Ok(None), false);
        let presenter = Rc::new(Deferred {
            state: Rc::downgrade(&host.state),
            queued: RefCell::new(vec![]),
            in_ui: Cell::new(false),
            calls: Cell::new(0),
            frames: Cell::new(0),
            ctx: egui::Context::default(),
        });
        host.set_context_menu_presenter(presenter.clone()).unwrap();
        (host, presenter)
    }

    #[test]
    fn deferred_tracking_leaves_the_ui_callback_and_allows_frames_while_open() {
        let (host, presenter) = deferred();
        presenter.in_ui.set(true);
        let mut output = presenter.ctx.run_ui(egui::RawInput::default(), |_| {
            present_pending(host.id, &presenter.ctx);
            present_pending(host.id, &presenter.ctx);
            assert_eq!(presenter.calls.get(), 0);
            assert!(host.state.borrow().context_menus.pending.is_some());
        });
        output.textures_delta.clear();
        presenter.in_ui.set(false);
        assert_eq!(presenter.queued.borrow().len(), 1, "multipass rendering queues only one callback");
        let callback = presenter.queued.borrow_mut().pop().unwrap();
        callback();
        assert_eq!(presenter.calls.get(), 1);
        assert!(presenter.frames.get() >= 3);
        assert!(!host.state.borrow().context_menus.active);
        assert!(host.state.borrow().context_menus.pending.is_none());
    }

    #[test]
    fn deferred_requests_revalidate_targets_and_stop_when_the_host_is_dropped() {
        let (host, presenter) = deferred();
        present_pending(host.id, &presenter.ctx);
        host.state.borrow_mut().run("session.new", json!({})).unwrap();
        let callback = presenter.queued.borrow_mut().pop().unwrap();
        callback();
        assert_eq!(presenter.calls.get(), 0);
        assert!(!host.state.borrow().context_menus.active);

        let (host, presenter) = deferred();
        present_pending(host.id, &presenter.ctx);
        drop(host);
        let callback = presenter.queued.borrow_mut().pop().unwrap();
        callback();
        assert_eq!(presenter.calls.get(), 0);
    }

    #[test]
    fn scheduling_failure_restores_the_in_window_fallback_without_running_the_menu() {
        struct Refused;
        impl Presenter for Refused {
            fn schedule(&self, _: Box<dyn FnOnce()>) -> Result<(), String> {
                Err("queue unavailable".into())
            }
            fn present(&self, _: &Request) -> Result<Option<Selection>, String> {
                panic!("must not track after a scheduling failure")
            }
        }
        let (host, _, _) = setup(Ok(None), false);
        host.set_context_menu_presenter(Rc::new(Refused)).unwrap();
        present_pending(host.id, &egui::Context::default());
        let app = host.state.borrow();
        assert!(!app.context_menus.active);
        assert!(!app.context_menus.enabled);
        assert!(app.context_menus.pending.is_none());
        assert!(app.ui.status.contains("queue unavailable"));
    }

    #[test]
    fn dispatches_once_after_tracking_without_borrowing_the_app() {
        let (host, presenter, track) = setup(Ok(None), false);
        *presenter.result.borrow_mut() =
            Ok(Some(Selection { action: Action::Command("track.hide".into(), json!({"tracks": [track.0]})), modifiers: Modifiers::NONE }));
        let before = host.state.borrow().engine.undo_history().len();
        let ctx = egui::Context::default();
        present_pending(host.id, &ctx);
        present_pending(host.id, &ctx);
        let app = host.state.borrow();
        assert!(app.engine.session().track(track).unwrap().hidden);
        assert_eq!(app.engine.undo_history().len(), before + 1);
        assert!(!app.context_menus.active);
        assert_eq!(presenter.requests.borrow().len(), 1);
        assert_eq!(presenter.requests.borrow()[0].viewport, egui::ViewportId::from_hash_of("soundcraft.mix"));
    }

    #[test]
    fn cancellation_does_not_execute_and_failure_enables_the_fallback() {
        for result in [Ok(None), Err("Test presenter unavailable".into())] {
            let failed = result.is_err();
            let (host, presenter, _) = setup(result, false);
            let before = host.state.borrow().engine.undo_history().len();
            present_pending(host.id, &egui::Context::default());
            let app = host.state.borrow();
            assert_eq!(app.engine.undo_history().len(), before);
            assert_eq!(app.context_menus.enabled, !failed);
            assert!(!app.context_menus.active);
            assert_eq!(presenter.requests.borrow().len(), 1);
        }
    }

    #[test]
    fn ignores_results_from_a_replaced_session_and_deleted_targets() {
        let (host, presenter, _) =
            setup(Ok(Some(Selection { action: Action::Command("track.new".into(), json!({"name": "Stale"})), modifiers: Modifiers::NONE })), true);
        present_pending(host.id, &egui::Context::default());
        assert!(host.state.borrow().engine.session().tracks.is_empty());
        assert_eq!(presenter.requests.borrow().len(), 1);

        let (host, presenter, track) = setup(Ok(None), false);
        host.state.borrow_mut().run("track.delete", json!({"tracks": [track.0]})).unwrap();
        present_pending(host.id, &egui::Context::default());
        assert!(presenter.requests.borrow().is_empty());
    }

    #[test]
    fn search_is_opened_on_the_requesting_viewport() {
        let (host, presenter, track) = setup(Ok(None), false);
        let viewport = ViewportId::from_hash_of("search origin");
        host.state.borrow_mut().context_menus.pending.as_mut().unwrap().viewport = viewport;
        *presenter.result.borrow_mut() = Ok(Some(Selection {
            action: Action::Command("window.plugin_search".into(), json!({"track": track.0, "slot": 0})),
            modifiers: Modifiers::NONE,
        }));
        present_pending(host.id, &egui::Context::default());
        assert_eq!(host.state.borrow().context_menus.search.as_ref().unwrap().viewport, viewport);
    }

    #[test]
    fn native_choices_are_delivered_without_reentering_the_context_lock() {
        let (host, presenter, _) = setup(Ok(None), false);
        let control = egui::Id::unique("form-control");
        *presenter.result.borrow_mut() =
            Ok(Some(Selection { action: Action::Choice { control, index: 1, label: "Stereo".into() }, modifiers: Modifiers::NONE }));
        let ctx = egui::Context::default();
        present_pending(host.id, &ctx);
        let pick = ctx.data(|data| data.get_temp::<crate::dropdown::Pick>(control)).unwrap();
        assert_eq!(pick.index, 1);
        assert_eq!(pick.label, "Stereo");
        assert!(!host.state.borrow().context_menus.active);
    }
}
