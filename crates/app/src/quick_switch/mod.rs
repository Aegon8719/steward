//! Automatically attach a directory picker to the foreground open/save dialog.
//! Directory I/O and Explorer COM calls stay off the UI thread.

mod native;
mod search;

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use gpui::{App, AsyncApp, Context, Window};
use steward_ui_components::ResultItem;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

use crate::{
    i18n::Localization,
    launcher::{LauncherState, StewardApp},
    platform,
    window::{hide_window, show_launcher, sync_directory_picker_bounds},
};
use native::DialogTarget;

pub(crate) const STATUS_HEIGHT: f32 = 24.0;
const HISTORY_LIMIT: usize = 50;
type SearchRequest = (u64, String, Vec<PathBuf>);

pub(crate) struct QuickSwitch {
    pub(crate) target: Option<DialogTarget>,
    /// True while the bar is attached but does not hold the keyboard (a dialog
    /// the picker already attached to, or a refused focus hand-off); clicking
    /// the bar clears it.
    pub(crate) passive: bool,
    pub(crate) navigating: bool,
    pub(crate) saved_query: Option<String>,
    pub(crate) status: String,
    /// The supported dialog that owns the foreground, refreshed every 100 ms.
    foreground_dialog: Option<DialogTarget>,
    /// The dialog the picker attached to last: the first attach to a dialog
    /// takes the keyboard, later attaches leave it with the dialog.
    last_target: Option<DialogTarget>,
    checked_at: Instant,
    history: Arc<Mutex<Vec<PathBuf>>>,
    stopped: Arc<AtomicBool>,
    search_request: Arc<Mutex<Option<SearchRequest>>>,
    search_wake: Sender<()>,
    search_results: Receiver<(u64, Vec<PathBuf>)>,
    generation: u64,
    navigation: Option<Receiver<Result<(), String>>>,
    cancelled: Arc<AtomicBool>,
}

impl QuickSwitch {
    pub(crate) fn new() -> Self {
        let history = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let (worker_history, worker_stop) = (history.clone(), stopped.clone());
        std::thread::spawn(move || {
            let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            if !initialized {
                return;
            }
            while !worker_stop.load(Ordering::Relaxed) {
                if let Some(path) = native::explorer_directory() {
                    remember(&mut worker_history.lock().unwrap(), path);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            unsafe { CoUninitialize() };
        });

        // One worker, one replaceable request: typing cannot spawn an unbounded
        // number of filesystem operations. Generations reject late replies.
        let search_request = Arc::new(Mutex::new(None::<SearchRequest>));
        let worker_request = search_request.clone();
        let (search_wake, wake) = bounded(1);
        let (results, search_results) = unbounded();
        std::thread::spawn(move || {
            while wake.recv().is_ok() {
                let request = worker_request.lock().unwrap().take();
                if let Some((generation, query, recent)) = request {
                    let paths = search::search_directories(&query, &recent);
                    if results.send((generation, paths)).is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            target: None,
            passive: false,
            navigating: false,
            saved_query: None,
            status: "quick-switch-hint".into(),
            foreground_dialog: None,
            last_target: None,
            checked_at: Instant::now(),
            history,
            stopped,
            search_request,
            search_wake,
            search_results,
            generation: 0,
            navigation: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Refresh the foreground independently of the launcher's hotkey binding.
    /// Returns whether the session and geometry should be checked on this tick.
    fn update_foreground(&mut self) -> bool {
        if self.checked_at.elapsed() < Duration::from_millis(100) {
            return false;
        }
        self.checked_at = Instant::now();
        self.foreground_dialog = DialogTarget::foreground();
        true
    }

    fn request_search(&mut self, query: String) {
        self.generation += 1;
        self.status = "quick-switch-searching".into();
        let recent = self.history.lock().unwrap().clone();
        *self.search_request.lock().unwrap() = Some((self.generation, query, recent));
        let _ = self.search_wake.try_send(());
    }

    /// End the current session. A foreground dialog can attach a new one.
    pub(crate) fn cancel(&mut self) -> (Option<DialogTarget>, Option<String>) {
        self.cancelled.store(true, Ordering::Release);
        self.generation += 1;
        *self.search_request.lock().unwrap() = None;
        self.navigation = None;
        self.navigating = false;
        self.passive = false;
        (self.target.take(), self.saved_query.take())
    }
}

impl Drop for QuickSwitch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.cancelled.store(true, Ordering::Release);
    }
}

fn remember(history: &mut Vec<PathBuf>, path: PathBuf) {
    let key = path.to_string_lossy();
    history.retain(|old| !old.to_string_lossy().eq_ignore_ascii_case(&key));
    history.insert(0, path);
    history.truncate(HISTORY_LIMIT);
}

/// Attach the bar under the dialog that just took the foreground. A dialog
/// seen for the first time hands the keyboard to the bar, so the query goes
/// here instead of into the dialog's own path box; a dialog the picker
/// already attached to (re-shown after Esc, a navigation or a click into the
/// dialog) keeps its focus and the bar waits passively for a click.
fn attach_picker(
    state: &Rc<RefCell<LauncherState>>,
    i18n: Rc<Localization>,
    cx: &mut AsyncApp,
    target: DialogTarget,
) {
    let (prefill, take_keyboard) = {
        let state = state.borrow();
        let mut picker = state.quick_switch.borrow_mut();
        let (_, saved_query) = picker.cancel();
        picker.saved_query = saved_query;
        let take_keyboard = takes_keyboard(picker.last_target, target);
        picker.last_target = Some(target);
        picker.target = Some(target);
        picker.passive = !take_keyboard;
        // Bind before returning: the history guard must drop while `picker` is
        // still alive.
        let prefill = picker
            .history
            .lock()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or_default();
        (prefill, take_keyboard)
    };
    show_launcher(state, i18n, cx, take_keyboard);
    // Windows can refuse the focus hand-off (a dialog owned by an elevated
    // process, for example): the bar then falls back to click-to-type.
    if take_keyboard && !launcher_is_foreground(state, cx) {
        state.borrow().quick_switch.borrow_mut().passive = true;
    }
    let handle = state
        .borrow()
        .window
        .and_then(|h| h.downcast::<StewardApp>());
    if let Some(handle) = handle {
        let _ = handle.update(cx, |app, window, cx| {
            app.begin_directory_picker(prefill, window, cx);
        });
    }
    sync_directory_picker_bounds(state, cx);
}

/// Whether a dialog that just took the foreground should hand its keyboard to
/// the picker: only the first time the picker sees it. A dialog the picker
/// already attached to is one the user returned to on purpose (Esc, a
/// completed navigation or a click), so it keeps the input.
fn takes_keyboard(last_target: Option<DialogTarget>, target: DialogTarget) -> bool {
    last_target != Some(target)
}

/// Whether the launcher window owns the foreground, i.e. the activation in
/// [`show_launcher`] actually succeeded.
fn launcher_is_foreground(state: &Rc<RefCell<LauncherState>>, cx: &mut AsyncApp) -> bool {
    let handle = state
        .borrow()
        .window
        .and_then(|h| h.downcast::<StewardApp>());
    let launcher_hwnd = handle.and_then(|handle| {
        handle
            .update(cx, |_, window, _| platform::hwnd(window))
            .ok()
            .flatten()
    });
    launcher_hwnd == Some(platform::foreground_hwnd())
}

/// What the poll does with the picker session on this tick.
#[derive(Debug, PartialEq, Eq)]
enum SessionAction {
    /// The dialog just took the foreground: attach the bar to it.
    Attach(DialogTarget),
    /// The dialog is gone, or the user moved on: end the session and hide.
    Detach,
    /// Leave the session alone.
    Keep,
}

/// Both passive and activated sessions follow their dialog. A hidden launcher
/// reattaches whenever the dialog is foreground, including after cancellation.
fn session_action(
    target: Option<DialogTarget>,
    foreground_dialog: Option<DialogTarget>,
    launcher_visible: bool,
    target_valid: bool,
    launcher_foreground: bool,
    navigating: bool,
) -> SessionAction {
    match target {
        Some(_) if !target_valid => SessionAction::Detach,
        Some(target) if foreground_dialog == Some(target) => {
            if !launcher_visible && !navigating {
                SessionAction::Attach(target)
            } else {
                SessionAction::Keep
            }
        }
        Some(_) if launcher_visible && launcher_foreground => SessionAction::Keep,
        Some(_) => SessionAction::Detach,
        None if !launcher_visible => foreground_dialog
            .map(SessionAction::Attach)
            .unwrap_or(SessionAction::Keep),
        None => SessionAction::Keep,
    }
}

/// Called by the foreground event pump; all work here is nonblocking.
pub(crate) fn poll(state: &Rc<RefCell<LauncherState>>, i18n: Rc<Localization>, cx: &mut AsyncApp) {
    let (mut paths, mut navigation, foreground_checked) = {
        let state = state.borrow();
        let mut picker = state.quick_switch.borrow_mut();
        let foreground_checked = picker.update_foreground();
        let mut paths = None;
        while let Ok((generation, result)) = picker.search_results.try_recv() {
            if picker.target.is_some() && generation == picker.generation {
                picker.status = if result.is_empty() {
                    "quick-switch-no-results"
                } else {
                    "quick-switch-hint"
                }
                .into();
                paths = Some(result);
            }
        }
        let navigation = picker.navigation.as_ref().and_then(|rx| rx.try_recv().ok());
        if navigation.is_some() {
            picker.navigation = None;
        }
        (paths, navigation, foreground_checked)
    };
    let handle = state
        .borrow()
        .window
        .and_then(|h| h.downcast::<StewardApp>());
    if foreground_checked {
        let launcher_hwnd = handle.and_then(|handle| {
            handle
                .update(cx, |_, window, _| platform::hwnd(window))
                .ok()
                .flatten()
        });
        let visible = launcher_hwnd.is_some_and(platform::is_hwnd_visible);
        let action = {
            let state = state.borrow();
            let picker = state.quick_switch.borrow();
            session_action(
                picker.target,
                picker.foreground_dialog,
                visible,
                picker.target.is_some_and(|target| target.valid()),
                launcher_hwnd == Some(platform::foreground_hwnd()),
                picker.navigating,
            )
        };
        match action {
            SessionAction::Attach(target) => {
                paths = None;
                navigation = None;
                attach_picker(state, i18n.clone(), cx, target);
            }
            SessionAction::Detach => {
                paths = None;
                navigation = None;
                if let Some(handle) = handle {
                    let _ = handle.update(cx, |app, window, cx| {
                        app.cancel_directory_picker(window, cx, false);
                        hide_window(window, cx);
                    });
                } else {
                    state.borrow().quick_switch.borrow_mut().cancel();
                }
            }
            SessionAction::Keep => {
                if state.borrow().quick_switch.borrow().target.is_some() {
                    sync_directory_picker_bounds(state, cx);
                }
            }
        }
    }
    if let (Some(handle), Some(paths)) = (handle, paths) {
        let _ = handle.update(cx, |app, window, cx| {
            app.apply_directory_results(paths, window, cx)
        });
    }
    if let Some(result) = navigation {
        match result {
            Ok(()) => {
                if let Some(handle) = handle {
                    let _ = handle.update(cx, |app, window, cx| {
                        app.cancel_directory_picker(window, cx, false);
                        hide_window(window, cx);
                    });
                } else {
                    state.borrow().quick_switch.borrow_mut().cancel();
                }
            }
            Err(error) => {
                {
                    let state = state.borrow();
                    let mut picker = state.quick_switch.borrow_mut();
                    picker.navigating = false;
                    picker.status = error;
                }
                // Return the editable query with a visible, localized error.
                // Do not steal focus if the user has already switched away.
                let target = state.borrow().quick_switch.borrow().target;
                if target.is_some() && target == DialogTarget::foreground() {
                    show_launcher(state, i18n, cx, true);
                }
                if let Some(handle) = handle {
                    let _ = handle.update(cx, |_, _, cx| cx.notify());
                }
            }
        }
    }
}

pub(crate) fn confirm_directory(state: &Rc<RefCell<LauncherState>>, path: PathBuf, cx: &mut App) {
    let (target, cancelled, tx) = {
        let state = state.borrow();
        let mut picker = state.quick_switch.borrow_mut();
        if picker.navigating {
            return;
        }
        let Some(target) = picker.target else {
            return;
        };
        let (tx, rx) = bounded(1);
        picker.cancelled = Arc::new(AtomicBool::new(false));
        picker.navigation = Some(rx);
        picker.navigating = true;
        picker.status = "quick-switch-navigating".into();
        (target, picker.cancelled.clone(), tx)
    };
    // Leave the current result-list callback before focus/activation callbacks
    // re-enter the launcher. Keep native I/O out of GPUI's foreground task.
    cx.defer(move |_| {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = target.focus_address() {
            let _ = tx.send(Err(error.to_string()));
            return;
        }
        std::thread::spawn(move || {
            let result = target
                .navigate(&path, &cancelled)
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
    });
}

impl StewardApp {
    /// Clicking a bar that does not hold the keyboard hands it the input.
    /// Returns whether the picker needed promoting.
    pub(crate) fn promote_passive_picker(&mut self) -> bool {
        #[cfg(target_os = "windows")]
        {
            let state = self.state.borrow();
            let mut picker = state.quick_switch.borrow_mut();
            if !picker.passive {
                return false;
            }
            picker.passive = false;
            picker.status = "quick-switch-hint".into();
            true
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    fn begin_directory_picker(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state
            .borrow()
            .quick_switch
            .borrow_mut()
            .saved_query
            .get_or_insert_with(|| self.input.query.clone());
        self.input.query = path.to_string_lossy().into_owned();
        self.input.marked = None;
        self.input.select_all();
        self.mouse_selecting = false;
        self.search(window, cx);
    }

    pub(crate) fn search_directory_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        {
            let state = self.state.borrow();
            state.plugin_gen.set(state.plugin_gen.get() + 1);
            state.plugin_hits.borrow_mut().clear();
            state.plugin_views.borrow_mut().clear();
            state.plugin_pending.borrow_mut().clear();
            *state.plugin_calendar.borrow_mut() = None;
            state
                .quick_switch
                .borrow_mut()
                .request_search(self.input.query.clone());
        }
        // Clear the old selection immediately: Enter while the new query is
        // pending must never navigate to a result of the previous query.
        self.apply_directory_results(Vec::new(), window, cx);
    }

    fn apply_directory_results(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.base_items = paths
            .into_iter()
            .map(|path| ResultItem::Directory {
                title: path.to_string_lossy().into_owned(),
                subtitle: self.i18n.translate("quick-switch-directory"),
                path,
            })
            .collect();
        self.base_icons = vec![None; self.base_items.len()];
        self.builtin_count = 0;
        self.render_merged(window, cx);
    }

    pub(crate) fn cancel_directory_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        restore: bool,
    ) {
        let (target, query) = self.state.borrow().quick_switch.borrow_mut().cancel();
        if let Some(query) = query {
            self.input.query = query;
            self.input.marked = None;
            self.input.set_cursor(self.input.char_count());
            self.search(window, cx);
        }
        if restore {
            hide_window(window, cx);
            if let Some(target) = target {
                target.restore_focus();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_recent_first_deduplicated_and_bounded() {
        let mut history = Vec::new();
        for i in 0..70 {
            remember(&mut history, PathBuf::from(format!(r"C:\folder{i}")));
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        remember(&mut history, PathBuf::from(r"c:\FOLDER65"));
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0], PathBuf::from(r"c:\FOLDER65"));
        assert_eq!(history[1], PathBuf::from(r"C:\folder69"));
    }

    #[test]
    fn only_the_first_attach_to_a_dialog_takes_the_keyboard() {
        let dialog = DialogTarget::for_test(0x1000);
        let other = DialogTarget::for_test(0x2000);
        assert!(takes_keyboard(None, dialog));
        assert!(!takes_keyboard(Some(dialog), dialog));
        assert!(takes_keyboard(Some(other), dialog));
    }

    #[test]
    fn foreground_dialog_always_reattaches_after_cancellation_or_hidden_window() {
        let dialog = DialogTarget::for_test(0x1000);
        assert_eq!(
            session_action(None, Some(dialog), false, false, false, false),
            SessionAction::Attach(dialog)
        );
        assert_eq!(
            session_action(Some(dialog), Some(dialog), false, true, false, false),
            SessionAction::Attach(dialog)
        );
        // An ordinary visible launcher still belongs to its current session.
        assert_eq!(
            session_action(None, Some(dialog), true, false, false, false),
            SessionAction::Keep
        );
    }

    #[test]
    fn attached_session_follows_focus_and_dialog_lifetime() {
        let dialog = DialogTarget::for_test(0x1000);
        let other = DialogTarget::for_test(0x2000);
        // Clicking the picker and then its dialog keeps the same attachment.
        assert_eq!(
            session_action(Some(dialog), None, true, true, true, false),
            SessionAction::Keep
        );
        assert_eq!(
            session_action(Some(dialog), Some(dialog), true, true, false, false),
            SessionAction::Keep
        );
        // Navigation may hide the picker while it sends input to the dialog.
        assert_eq!(
            session_action(Some(dialog), Some(dialog), false, true, false, true),
            SessionAction::Keep
        );
        // Closing the dialog also ends a session whose picker owns focus.
        assert_eq!(
            session_action(Some(dialog), None, true, false, true, false),
            SessionAction::Detach
        );
        assert_eq!(
            session_action(Some(dialog), Some(other), true, true, false, false),
            SessionAction::Detach
        );
        assert_eq!(
            session_action(Some(dialog), None, true, true, false, false),
            SessionAction::Detach
        );
    }
}
