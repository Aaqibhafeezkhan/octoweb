//! Browser extensions through WebKit's `WKWebExtension` API (macOS 15.4+).
//!
//! One `WKWebExtensionController` serves every normal tab; incognito tabs get
//! none, as in Safari. Workspaces are the extension "windows" and the active
//! one is focused. WebKit asks about tabs and windows through the objects in
//! `extension_objc`, answered from a [`Model`] that [`sync`] reconciles with
//! the live `WorkspaceManager` once per event-loop pass — so no open, close
//! or switch path in main.rs has to remember to tell extensions. Requests
//! that change app state go back to main.rs as [`Action`]s through the event
//! loop, never re-entrantly from inside a WebKit callback.
//!
//! Every WebKit object here is main-thread-only, and so is `STATE`. WebKit
//! calls straight back into `extension_objc`, which reads `STATE`, so nothing
//! may call into WebKit while `STATE` is borrowed: copy what you need out,
//! release, then call.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject, Sel};
use objc2::{msg_send, sel, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSImage,
    NSMenu, NSMenuItem, NSModalResponse, NSModalResponseOK, NSOpenPanel, NSView,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSCopying, NSDate, NSDictionary, NSError, NSLocalizedDescriptionKey,
    NSOperatingSystemVersion, NSProcessInfo, NSRectEdge, NSSet, NSString, NSURL, NSUUID,
};
use objc2_web_kit::{
    WKWebExtension, WKWebExtensionContext, WKWebExtensionContextPermissionStatus,
    WKWebExtensionController, WKWebExtensionControllerConfiguration, WKWebExtensionDataRecord,
    WKWebExtensionMatchPattern, WKWebExtensionPermission, WKWebExtensionTab,
    WKWebExtensionTabChangedProperties, WKWebExtensionTabConfiguration, WKWebExtensionWindow,
    WKWebExtensionWindowConfiguration, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};

use crate::extension_objc::{ExtDelegate, ExtTab, ExtWindow};
use crate::extension_store::{self, Record};
use crate::workspace::WorkspaceManager;
use wry::WebViewExtMacOS;

/// Toolbar buttons draw icons at 16 px; render twice that for Retina.
const TOOLBAR_ICON_PT: f64 = 32.0;
/// Settings rows draw icons at 24 px.
const SETTINGS_ICON_PT: f64 = 48.0;
const ALERT_ICON_PT: f64 = 64.0;

/// Work only main.rs can do — it owns the tabs, WebViews and chrome surfaces.
#[derive(Debug)]
pub enum Action {
    Activate(usize),
    Close(usize),
    Load(usize, String),
    /// Open a tab in `workspace` (`None` = the active one). When `reply` is
    /// set, report the new tab id with [`tab_opened`].
    OpenTab {
        url: Option<String>,
        workspace: Option<String>,
        active: bool,
        reply: Option<u64>,
    },
    FocusWindow(String),
    /// Actions on the toolbar changed: re-push [`toolbar_script`].
    Toolbar,
    /// The installed list changed: re-push the toolbar and [`settings_script`].
    Installed,
    Notify(String),
}

#[derive(Debug)]
pub enum Event {
    /// Chrome Web Store URL or id, or a path, typed into settings.
    Install(String),
    /// Choose a folder or archive with an open panel, then install it.
    PickAndInstall,
    SetEnabled(String, bool),
    Remove(String),
    OpenOptions(String),
    /// Toolbar button clicked; `anchor` is its x, y, width, height in the
    /// address bar's CSS pixels — where the popup points.
    Click {
        id: String,
        anchor: [f64; 4],
    },
    Downloaded {
        id: String,
        result: Result<PathBuf, String>,
    },
    /// Raised from a WebKit callback; handed to main.rs on the next turn.
    App(Action),
}

static SENDER: OnceLock<Box<dyn Fn(Event) + Send + Sync>> = OnceLock::new();

fn send(event: Event) {
    if let Some(sender) = SENDER.get() {
        sender(event);
    }
}

fn send_app(action: Action) {
    send(Event::App(action));
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    controller: Retained<WKWebExtensionController>,
    /// The controller holds its delegate weakly.
    _delegate: Retained<ExtDelegate>,
    installed: Vec<Installed>,
    controller_id: [u8; 16],
    model: Model,
    replies: Vec<Reply>,
    next_reply: u64,
    /// Address bar WebView — popups point at its buttons.
    toolbar: Option<Retained<NSView>>,
    anchors: HashMap<String, CGRect>,
}

struct Installed {
    record: Record,
    /// Parsed manifest and resources; `None` until parsed, or if parsing failed.
    extension: Option<Retained<WKWebExtension>>,
    /// Set while loaded into the controller.
    context: Option<Retained<WKWebExtensionContext>>,
    error: Option<String>,
}

impl State {
    fn find(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|i| i.record.id == id)
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut Installed> {
        self.installed.iter_mut().find(|i| i.record.id == id)
    }

    fn contexts(&self) -> Vec<Retained<WKWebExtensionContext>> {
        self.installed
            .iter()
            .filter_map(|i| i.context.clone())
            .collect()
    }

    fn save(&self) {
        extension_store::save(&extension_store::Store {
            controller_id: self.controller_id,
            extensions: self.installed.iter().map(|i| i.record.clone()).collect(),
        });
    }
}

fn with<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow().as_ref().map(f))
}

fn with_mut<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

type TabReplyBlock = RcBlock<dyn Fn(*mut ProtocolObject<dyn WKWebExtensionTab>, *mut NSError)>;
type WindowReplyBlock =
    RcBlock<dyn Fn(*mut ProtocolObject<dyn WKWebExtensionWindow>, *mut NSError)>;

/// An extension waiting for the tab (or window) it asked for.
struct Reply {
    id: u64,
    tab: Option<usize>,
    to: ReplyTo,
}

enum ReplyTo {
    Tab(TabReplyBlock),
    Window(WindowReplyBlock),
}

// ── Model: tabs and windows as extensions last heard of them ────────────────

struct TabEntry {
    obj: Retained<ExtTab>,
    window: String,
    title: String,
    url: String,
    loading: bool,
    audio: bool,
    webview: Option<Retained<WKWebView>>,
}

struct WindowEntry {
    id: String,
    obj: Retained<ExtWindow>,
    tabs: Vec<usize>,
    active: Option<usize>,
}

#[derive(Default)]
struct Model {
    /// In workspace order.
    windows: Vec<WindowEntry>,
    tabs: HashMap<usize, TabEntry>,
    focused: Option<String>,
}

enum Change {
    CloseTab(Retained<ExtTab>, bool),
    CloseWindow(Retained<ExtWindow>),
    OpenWindow(Retained<ExtWindow>),
    OpenTab(Retained<ExtTab>),
    Move(Retained<ExtTab>, usize, Retained<ExtWindow>),
    Props(Retained<ExtTab>, WKWebExtensionTabChangedProperties),
    Activate(Retained<ExtTab>, Option<Retained<ExtTab>>),
    Focus(Option<Retained<ExtWindow>>),
}

impl Model {
    fn window(&self, id: &str) -> Option<&WindowEntry> {
        self.windows.iter().find(|w| w.id == id)
    }

    fn focused_window(&self) -> Option<&WindowEntry> {
        self.focused.as_deref().and_then(|id| self.window(id))
    }

    fn active_tab(&self) -> Option<Retained<ExtTab>> {
        let id = self.focused_window()?.active?;
        self.tabs.get(&id).map(|t| t.obj.clone())
    }

    fn tab_by_webview(&self, webview: usize) -> Option<Retained<ExtTab>> {
        self.tabs
            .values()
            .find(|t| {
                t.webview
                    .as_ref()
                    .is_some_and(|w| Retained::as_ptr(w) as usize == webview)
            })
            .map(|t| t.obj.clone())
    }

    /// Bring the model in line with the live tabs and list what changed, in
    /// the order WebKit must hear it: closes first (while the old lists still
    /// say where things were), then opens, moves, property changes,
    /// activations and focus.
    fn reconcile(&mut self, wm: &WorkspaceManager, mtm: MainThreadMarker) -> Vec<Change> {
        let guards: Vec<_> = wm.list().iter().map(|ws| ws.tabs.lock().unwrap()).collect();
        let live_tabs: HashSet<usize> = guards
            .iter()
            .flat_map(|tm| tm.tabs().iter().filter(|t| !t.incognito).map(|t| t.id))
            .collect();
        let live_windows: HashSet<&str> = wm.list().iter().map(|ws| ws.id.as_str()).collect();

        let mut closes = Vec::new();
        let gone: Vec<usize> = self
            .tabs
            .keys()
            .filter(|id| !live_tabs.contains(id))
            .copied()
            .collect();
        for id in gone {
            let entry = self.tabs.remove(&id).expect("id came from the map");
            let window_closing = !live_windows.contains(entry.window.as_str());
            closes.push(Change::CloseTab(entry.obj, window_closing));
        }

        let mut opens = Vec::new();
        let mut moves = Vec::new();
        let mut props = Vec::new();
        for (ws, tm) in wm.list().iter().zip(&guards) {
            for tab in tm.tabs().iter().filter(|t| !t.incognito) {
                let webview = ws
                    .webviews
                    .get(&tab.id)
                    .map(|wv| Retained::into_super(wv.webview()));
                let loading = webview.as_ref().is_some_and(|wv| unsafe { wv.isLoading() });
                let Some(entry) = self.tabs.get_mut(&tab.id) else {
                    let obj = ExtTab::new(mtm, tab.id);
                    opens.push(Change::OpenTab(obj.clone()));
                    self.tabs.insert(
                        tab.id,
                        TabEntry {
                            obj,
                            window: ws.id.clone(),
                            title: tab.title.clone(),
                            url: tab.url.clone(),
                            loading,
                            audio: tab.is_playing_audio,
                            webview,
                        },
                    );
                    continue;
                };
                if entry.window != ws.id {
                    let from = self
                        .windows
                        .iter()
                        .find(|w| w.id == entry.window)
                        .and_then(|w| {
                            let index = w.tabs.iter().position(|&id| id == tab.id)?;
                            Some((index, w.obj.clone()))
                        });
                    if let Some((index, old_window)) = from {
                        moves.push(Change::Move(entry.obj.clone(), index, old_window));
                    }
                    entry.window.clone_from(&ws.id);
                }
                let mut changed = WKWebExtensionTabChangedProperties::None;
                if entry.title != tab.title {
                    entry.title.clone_from(&tab.title);
                    changed |= WKWebExtensionTabChangedProperties::Title;
                }
                if entry.url != tab.url {
                    entry.url.clone_from(&tab.url);
                    changed |= WKWebExtensionTabChangedProperties::URL;
                }
                if entry.loading != loading {
                    entry.loading = loading;
                    changed |= WKWebExtensionTabChangedProperties::Loading;
                }
                if entry.audio != tab.is_playing_audio {
                    entry.audio = tab.is_playing_audio;
                    changed |= WKWebExtensionTabChangedProperties::PlayingAudio;
                }
                entry.webview = webview;
                if !changed.is_empty() {
                    props.push(Change::Props(entry.obj.clone(), changed));
                }
            }
        }

        let mut windows = Vec::with_capacity(wm.list().len());
        let mut window_opens = Vec::new();
        let mut activations = Vec::new();
        for (ws, tm) in wm.list().iter().zip(&guards) {
            let mut window = match self.windows.iter().position(|w| w.id == ws.id) {
                Some(i) => self.windows.remove(i),
                None => {
                    let obj = ExtWindow::new(mtm, ws.id.clone());
                    window_opens.push(Change::OpenWindow(obj.clone()));
                    WindowEntry {
                        id: ws.id.clone(),
                        obj,
                        tabs: Vec::new(),
                        active: None,
                    }
                }
            };
            window.tabs.clear();
            window
                .tabs
                .extend(tm.tabs().iter().filter(|t| !t.incognito).map(|t| t.id));
            let active = tm.active_id().filter(|id| self.tabs.contains_key(id));
            if active != window.active {
                if let Some(tab) = active.and_then(|id| self.tabs.get(&id)) {
                    let previous = window
                        .active
                        .and_then(|id| self.tabs.get(&id))
                        .map(|t| t.obj.clone());
                    activations.push(Change::Activate(tab.obj.clone(), previous));
                }
                window.active = active;
            }
            windows.push(window);
        }
        for closed in self.windows.drain(..) {
            closes.push(Change::CloseWindow(closed.obj));
        }
        self.windows = windows;

        let focused = Some(wm.active().id.clone());
        if focused != self.focused {
            self.focused = focused;
            let window = self.focused_window().map(|w| w.obj.clone());
            activations.push(Change::Focus(window));
        }

        closes.extend(window_opens);
        closes.extend(opens);
        closes.extend(moves);
        closes.extend(props);
        closes.extend(activations);
        closes
    }
}

// ── Lifecycle ────────────────────────────────────────────────────────────────

pub fn supported() -> bool {
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: 15,
        minorVersion: 4,
        patchVersion: 0,
    })
}

/// Create the controller and start loading installed extensions. `sender`
/// delivers [`Event`]s back to the event loop (from any thread). Must run on
/// the main thread before the first tab WebView is built.
pub fn init(sender: impl Fn(Event) + Send + Sync + 'static) {
    let _ = SENDER.set(Box::new(sender));
    if !supported() {
        tracing::info!("browser extensions need macOS 15.4 or later");
        return;
    }
    let store = match extension_store::load() {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(error = %e, "extensions disabled: installed list unreadable");
            return;
        }
    };
    let mtm = MainThreadMarker::new().expect("extensions::init runs on the main thread");
    let controller = unsafe {
        let config = WKWebExtensionControllerConfiguration::configurationWithIdentifier(
            &NSUUID::from_bytes(store.controller_id),
            mtm,
        );
        WKWebExtensionController::initWithConfiguration(
            WKWebExtensionController::alloc(mtm),
            &config,
        )
    };
    let delegate = ExtDelegate::new(mtm);
    unsafe { controller.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    let records = store.extensions.clone();
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            controller,
            _delegate: delegate,
            installed: store
                .extensions
                .into_iter()
                .map(|record| Installed {
                    record,
                    extension: None,
                    context: None,
                    error: None,
                })
                .collect(),
            controller_id: store.controller_id,
            model: Model::default(),
            replies: Vec::new(),
            next_reply: 1,
            toolbar: None,
            anchors: HashMap::new(),
        })
    });
    for record in records {
        parse(&record.path.clone(), move |result| {
            let ok = result.is_ok();
            with_mut(|s| {
                if let Some(installed) = s.find_mut(&record.id) {
                    match result {
                        Ok(extension) => installed.extension = Some(extension),
                        Err(e) => {
                            tracing::warn!(id = %record.id, path = %record.path.display(), error = %e, "extension unreadable");
                            installed.error = Some(e);
                        }
                    }
                }
            });
            if ok && record.enabled {
                start(&record.id);
            }
            send_app(Action::Installed);
        });
    }
}

/// The address bar WebView, which popups point at.
pub fn set_toolbar_view(webview_ptr: usize) {
    let view = unsafe { Retained::retain(webview_ptr as *mut NSView) };
    with_mut(|s| s.toolbar = view);
}

/// WebKit parses an extension asynchronously; `done` runs on the main thread.
fn parse(path: &Path, done: impl Fn(Result<Retained<WKWebExtension>, String>) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let url = NSURL::fileURLWithPath_isDirectory(
        &NSString::from_str(&path.to_string_lossy()),
        path.is_dir(),
    );
    let block = RcBlock::new(move |extension: *mut WKWebExtension, error: *mut NSError| {
        match unsafe { Retained::retain(extension) } {
            Some(extension) => done(Ok(extension)),
            None => done(Err(describe(error))),
        }
    });
    unsafe { WKWebExtension::extensionWithResourceBaseURL_completionHandler(&url, &block, mtm) };
}

/// Load an installed, parsed extension into the controller.
fn start(id: &str) {
    let Some((controller, extension, record)) = with(|s| {
        let installed = s.find(id)?;
        Some((
            s.controller.clone(),
            installed.extension.clone()?,
            installed.record.clone(),
        ))
    })
    .flatten() else {
        return;
    };
    let context = unsafe { WKWebExtensionContext::contextForExtension(&extension) };
    let result = unsafe {
        context.setUniqueIdentifier(&NSString::from_str(&record.id));
        if let Some(base) = NSURL::URLWithString(&NSString::from_str(&format!(
            "webkit-extension://{}/",
            record.id
        ))) {
            context.setBaseURL(&base);
        }
        context.setInspectable(true);
        grant(&context, &record.permissions, &record.patterns);
        controller.loadExtensionContext_error(&context)
    };
    with_mut(|s| {
        if let Some(installed) = s.find_mut(id) {
            match result {
                Ok(()) => {
                    tracing::info!(id, path = %record.path.display(), "extension loaded");
                    installed.context = Some(context);
                    installed.error = None;
                }
                Err(e) => {
                    let error = e.localizedDescription().to_string();
                    tracing::warn!(id, %error, "extension failed to load");
                    installed.error = Some(error);
                }
            }
        }
    });
}

fn grant(context: &WKWebExtensionContext, permissions: &[String], patterns: &[String]) {
    unsafe {
        for permission in permissions {
            context.setPermissionStatus_forPermission(
                WKWebExtensionContextPermissionStatus::GrantedExplicitly,
                &NSString::from_str(permission),
            );
        }
        for pattern in patterns {
            match WKWebExtensionMatchPattern::initWithString_error(
                WKWebExtensionMatchPattern::alloc(context.mtm()),
                &NSString::from_str(pattern),
            ) {
                Ok(pattern) => context.setPermissionStatus_forMatchPattern(
                    WKWebExtensionContextPermissionStatus::GrantedExplicitly,
                    &pattern,
                ),
                Err(e) => {
                    tracing::warn!(%pattern, error = %e.localizedDescription(), "skipping invalid match pattern")
                }
            }
        }
    }
}

fn stop(context: &WKWebExtensionContext) {
    let Some(controller) = with(|s| s.controller.clone()) else {
        return;
    };
    if let Err(e) = unsafe { controller.unloadExtensionContext_error(context) } {
        tracing::warn!(error = %e.localizedDescription(), "failed to unload extension");
    }
}

/// Reconcile what extensions know with the live tabs. Called once per
/// event-loop pass; cheap when nothing changed, free with nothing installed.
pub fn sync(wm: &WorkspaceManager) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some((controller, changes)) = with_mut(|s| {
        if s.installed.is_empty() {
            return None;
        }
        Some((s.controller.clone(), s.model.reconcile(wm, mtm)))
    })
    .flatten() else {
        return;
    };
    if changes.is_empty() {
        return;
    }
    let toolbar = changes
        .iter()
        .any(|c| matches!(c, Change::Activate(..) | Change::Focus(_)));
    for change in changes {
        unsafe {
            match change {
                Change::CloseTab(tab, window_closing) => controller
                    .didCloseTab_windowIsClosing(ProtocolObject::from_ref(&*tab), window_closing),
                Change::CloseWindow(window) => {
                    controller.didCloseWindow(ProtocolObject::from_ref(&*window))
                }
                Change::OpenWindow(window) => {
                    controller.didOpenWindow(ProtocolObject::from_ref(&*window))
                }
                Change::OpenTab(tab) => controller.didOpenTab(ProtocolObject::from_ref(&*tab)),
                Change::Move(tab, index, from) => controller.didMoveTab_fromIndex_inWindow(
                    ProtocolObject::from_ref(&*tab),
                    index,
                    Some(ProtocolObject::from_ref(&*from)),
                ),
                Change::Props(tab, changed) => controller
                    .didChangeTabProperties_forTab(changed, ProtocolObject::from_ref(&*tab)),
                Change::Activate(tab, previous) => controller.didActivateTab_previousActiveTab(
                    ProtocolObject::from_ref(&*tab),
                    previous.as_deref().map(ProtocolObject::from_ref),
                ),
                Change::Focus(window) => {
                    controller.didFocusWindow(window.as_deref().map(ProtocolObject::from_ref))
                }
            }
        }
    }
    resolve_replies();
    if toolbar {
        send_app(Action::Toolbar);
    }
}

/// main.rs opened the tab an extension asked for.
pub fn tab_opened(reply: u64, tab: usize) {
    with_mut(|s| {
        if let Some(r) = s.replies.iter_mut().find(|r| r.id == reply) {
            r.tab = Some(tab);
        }
    });
}

/// Answer extensions whose tabs have reached the model.
fn resolve_replies() {
    let ready: Vec<(ReplyTo, Retained<ExtTab>, Option<Retained<ExtWindow>>)> = with_mut(|s| {
        let mut ready = Vec::new();
        let mut i = 0;
        while i < s.replies.len() {
            let tab = s.replies[i].tab.and_then(|id| s.model.tabs.get(&id));
            match tab {
                Some(tab) => {
                    let window = s.model.window(&tab.window).map(|w| w.obj.clone());
                    let obj = tab.obj.clone();
                    ready.push((s.replies.remove(i).to, obj, window));
                }
                None => i += 1,
            }
        }
        ready
    })
    .unwrap_or_default();
    for (to, tab, window) in ready {
        match to {
            ReplyTo::Tab(block) => {
                let tab = ProtocolObject::<dyn WKWebExtensionTab>::from_ref(&*tab);
                block.call((tab as *const _ as *mut _, null_mut()));
            }
            ReplyTo::Window(block) => {
                let window = window
                    .as_deref()
                    .map(ProtocolObject::<dyn WKWebExtensionWindow>::from_ref);
                block.call((
                    window.map_or(null_mut(), |w| w as *const _ as *mut _),
                    null_mut(),
                ));
            }
        }
    }
}

fn add_reply(to: ReplyTo) -> Option<u64> {
    with_mut(|s| {
        let id = s.next_reply;
        s.next_reply += 1;
        s.replies.push(Reply { id, tab: None, to });
        id
    })
}

// ── Answers for extension_objc ───────────────────────────────────────────────

fn tab_entry<R>(id: usize, f: impl FnOnce(&TabEntry) -> R) -> Option<R> {
    with(|s| s.model.tabs.get(&id).map(f)).flatten()
}

pub fn tab_window(id: usize) -> Option<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> {
    with(|s| {
        let window = &s.model.tabs.get(&id)?.window;
        s.model
            .window(window)
            .map(|w| ProtocolObject::from_retained(w.obj.clone()))
    })
    .flatten()
}

pub fn tab_index(id: usize) -> Option<usize> {
    with(|s| {
        let window = &s.model.tabs.get(&id)?.window;
        s.model.window(window)?.tabs.iter().position(|&t| t == id)
    })
    .flatten()
}

pub fn tab_webview(id: usize) -> Option<Retained<WKWebView>> {
    tab_entry(id, |t| t.webview.clone()).flatten()
}

pub fn tab_title(id: usize) -> Option<String> {
    tab_entry(id, |t| t.title.clone())
}

pub fn tab_url(id: usize) -> Option<String> {
    tab_entry(id, |t| t.url.clone()).filter(|u| !u.is_empty())
}

pub fn tab_loading(id: usize) -> bool {
    tab_entry(id, |t| t.loading).unwrap_or(false)
}

pub fn tab_playing_audio(id: usize) -> bool {
    tab_entry(id, |t| t.audio).unwrap_or(false)
}

pub fn tab_selected(id: usize) -> bool {
    with(|s| {
        let window = &s.model.tabs.get(&id)?.window;
        Some(s.model.window(window)?.active == Some(id))
    })
    .flatten()
    .unwrap_or(false)
}

pub fn activate_tab(id: usize) {
    send_app(Action::Activate(id));
}

pub fn close_tab(id: usize) {
    send_app(Action::Close(id));
}

pub fn load_tab(id: usize, url: String) {
    send_app(Action::Load(id, url));
}

pub fn duplicate_tab(id: usize, active: bool, reply: TabReplyBlock) {
    let workspace = tab_entry(id, |t| t.window.clone());
    send_app(Action::OpenTab {
        url: tab_url(id),
        workspace,
        active,
        reply: add_reply(ReplyTo::Tab(reply)),
    });
}

pub fn window_tabs(id: &str) -> Retained<NSArray<ProtocolObject<dyn WKWebExtensionTab>>> {
    let tabs: Vec<Retained<ProtocolObject<dyn WKWebExtensionTab>>> = with(|s| {
        s.model.window(id).map_or_else(Vec::new, |w| {
            w.tabs
                .iter()
                .filter_map(|t| s.model.tabs.get(t))
                .map(|t| ProtocolObject::from_retained(t.obj.clone()))
                .collect()
        })
    })
    .unwrap_or_default();
    NSArray::from_retained_slice(&tabs)
}

pub fn window_active_tab(id: &str) -> Option<Retained<ProtocolObject<dyn WKWebExtensionTab>>> {
    with(|s| {
        let active = s.model.window(id)?.active?;
        s.model
            .tabs
            .get(&active)
            .map(|t| ProtocolObject::from_retained(t.obj.clone()))
    })
    .flatten()
}

pub fn focus_window(id: &str) {
    send_app(Action::FocusWindow(id.to_string()));
}

/// Windows with the focused one first, as WebKit requires.
pub fn open_windows() -> Retained<NSArray<ProtocolObject<dyn WKWebExtensionWindow>>> {
    let windows: Vec<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> = with(|s| {
        let focused = s.model.focused_window();
        focused
            .into_iter()
            .chain(
                s.model
                    .windows
                    .iter()
                    .filter(|w| Some(w.id.as_str()) != s.model.focused.as_deref()),
            )
            .map(|w| ProtocolObject::from_retained(w.obj.clone()))
            .collect()
    })
    .unwrap_or_default();
    NSArray::from_retained_slice(&windows)
}

pub fn focused_window() -> Option<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> {
    with(|s| {
        s.model
            .focused_window()
            .map(|w| ProtocolObject::from_retained(w.obj.clone()))
    })
    .flatten()
}

fn window_id(window: &ProtocolObject<dyn WKWebExtensionWindow>) -> Option<String> {
    let ptr = window as *const ProtocolObject<dyn WKWebExtensionWindow> as usize;
    with(|s| {
        s.model
            .windows
            .iter()
            .find(|w| Retained::as_ptr(&w.obj) as usize == ptr)
            .map(|w| w.id.clone())
    })
    .flatten()
}

fn url_string(url: Option<Retained<NSURL>>) -> Option<String> {
    url.and_then(|u| u.absoluteString()).map(|s| s.to_string())
}

pub fn open_tab(configuration: &WKWebExtensionTabConfiguration, reply: TabReplyBlock) {
    let (url, window, active) = unsafe {
        (
            url_string(configuration.url()),
            configuration.window(),
            configuration.shouldBeActive(),
        )
    };
    send_app(Action::OpenTab {
        url,
        workspace: window.as_deref().and_then(window_id),
        active,
        reply: add_reply(ReplyTo::Tab(reply)),
    });
}

/// Octoweb has one browser window, so a new extension window opens its tabs
/// in the focused workspace and answers with that workspace's window.
pub fn open_window(configuration: &WKWebExtensionWindowConfiguration, reply: WindowReplyBlock) {
    let (urls, focus) = unsafe { (configuration.tabURLs(), configuration.shouldBeFocused()) };
    let urls: Vec<String> = urls.iter().filter_map(|u| url_string(Some(u))).collect();
    if urls.is_empty() {
        let window = focused_window();
        reply.call((
            window
                .as_deref()
                .map_or(null_mut(), |w| w as *const _ as *mut _),
            null_mut(),
        ));
        return;
    }
    let mut reply = Some(ReplyTo::Window(reply));
    for (i, url) in urls.into_iter().enumerate() {
        send_app(Action::OpenTab {
            url: Some(url),
            workspace: None,
            active: focus && i == 0,
            reply: reply.take().and_then(add_reply),
        });
    }
}

fn display_name(extension: &WKWebExtension) -> String {
    unsafe { extension.displayName() }
        .map_or_else(|| "This extension".to_string(), |n| n.to_string())
}

type GrantReply<T> = RcBlock<dyn Fn(NonNull<NSSet<T>>, *mut NSDate)>;

fn alert(
    title: &str,
    detail: &str,
    accept: &str,
    icon: Option<&NSImage>,
) -> Option<Retained<NSAlert>> {
    let alert = NSAlert::new(MainThreadMarker::new()?);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str(accept));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    if let Some(icon) = icon {
        unsafe { alert.setIcon(Some(icon)) };
    }
    Some(alert)
}

/// App-modal yes/no, for things the user just asked for (adding an extension).
fn confirm(title: &str, detail: &str, accept: &str, icon: Option<&NSImage>) -> bool {
    alert(title, detail, accept, icon).is_some_and(|a| a.runModal() == NSAlertFirstButtonReturn)
}

/// Ask whether `context` may have `items` (permission names or match
/// patterns) in a sheet on the browser window, then call `done` with the
/// answer. Never blocks: the request comes from an extension mid-flight, and
/// WebKit denies it on its own if unanswered for two minutes.
fn ask(
    context: &WKWebExtensionContext,
    verb: &str,
    items: &[String],
    done: impl FnOnce(bool) + 'static,
) {
    let window = with(|s| s.toolbar.as_ref().and_then(|v| v.window())).flatten();
    let extension = unsafe { context.webExtension() };
    let mut items = items.to_vec();
    items.sort();
    let alert = alert(
        &format!("Allow “{}” to {verb}?", display_name(&extension)),
        &items.join("\n"),
        "Allow",
        unsafe { extension.iconForSize(CGSize::new(ALERT_ICON_PT, ALERT_ICON_PT)) }.as_deref(),
    );
    let (Some(window), Some(alert)) = (window, alert) else {
        tracing::warn!("no browser window to ask for extension permissions in — denied");
        done(false);
        return;
    };
    let done = Cell::new(Some(done));
    let shown = alert.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        // Holds the alert until AppKit is done with it.
        let _ = &shown;
        if let Some(done) = done.take() {
            done(response == NSAlertFirstButtonReturn);
        }
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
}

/// `permissions.request()` for API permissions.
pub fn prompt_permissions(
    context: &WKWebExtensionContext,
    permissions: &NSSet<WKWebExtensionPermission>,
    reply: GrantReply<WKWebExtensionPermission>,
) {
    let names: Vec<String> = permissions.iter().map(|p| p.to_string()).collect();
    let (context, permissions) = (context.retain(), permissions.copy());
    ask(
        &context.clone(),
        "use more of the browser",
        &names.clone(),
        move |allowed| {
            let granted = if allowed {
                grant(&context, &names, &[]);
                save_grants(&context);
                permissions
            } else {
                NSSet::new()
            };
            reply.call((NonNull::from(&*granted), null_mut()));
        },
    );
}

/// `permissions.request()` for sites.
pub fn prompt_patterns(
    context: &WKWebExtensionContext,
    patterns: &NSSet<WKWebExtensionMatchPattern>,
    reply: GrantReply<WKWebExtensionMatchPattern>,
) {
    let names: Vec<String> = patterns
        .iter()
        .map(|p| unsafe { p.string() }.to_string())
        .collect();
    let (context, patterns) = (context.retain(), patterns.copy());
    ask(
        &context.clone(),
        "read and change these sites",
        &names.clone(),
        move |allowed| {
            let granted = if allowed {
                grant(&context, &[], &names);
                save_grants(&context);
                patterns
            } else {
                NSSet::new()
            };
            reply.call((NonNull::from(&*granted), null_mut()));
        },
    );
}

/// Persist what `context` has been granted, so the next launch restores it.
fn save_grants(context: &WKWebExtensionContext) {
    let (id, permissions, patterns) = unsafe {
        (
            context.uniqueIdentifier().to_string(),
            context
                .grantedPermissions()
                .allKeys()
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            context
                .grantedPermissionMatchPatterns()
                .allKeys()
                .iter()
                .map(|p| p.string().to_string())
                .collect::<Vec<_>>(),
        )
    };
    with_mut(|s| {
        if let Some(installed) = s.find_mut(&id) {
            installed.record.permissions = permissions;
            installed.record.patterns = patterns;
        }
        s.save();
    });
}

pub fn toolbar_changed() {
    send_app(Action::Toolbar);
}

pub fn present_popup(
    action: &objc2_web_kit::WKWebExtensionAction,
    context: &WKWebExtensionContext,
) -> Result<(), String> {
    let popover = unsafe { action.popupPopover() }.ok_or("The extension has no popup")?;
    let id = unsafe { context.uniqueIdentifier() }.to_string();
    let (view, anchor) = with(|s| (s.toolbar.clone(), s.anchors.get(&id).copied()))
        .ok_or("Extensions are not running")?;
    let view = view.ok_or("No toolbar to show the popup from")?;
    let bounds = view.bounds();
    // No click to point at (the extension opened its popup itself): point at
    // the right end of the bar, where the buttons are.
    let anchor = anchor.unwrap_or(CGRect::new(
        CGPoint::new(bounds.size.width - 40.0, 0.0),
        CGSize::new(1.0, bounds.size.height),
    ));
    let (rect, edge) = if view.isFlipped() {
        (anchor, NSRectEdge::MaxY)
    } else {
        let mut flipped = anchor;
        flipped.origin.y = bounds.size.height - anchor.origin.y - anchor.size.height;
        (flipped, NSRectEdge::MinY)
    };
    tracing::debug!(extension = %id, "showing extension popup");
    popover.showRelativeToRect_ofView_preferredEdge(rect, &view, edge);
    Ok(())
}

pub fn error(message: &str) -> Retained<NSError> {
    let description = NSString::from_str(message);
    let info = NSDictionary::from_slices(
        &[unsafe { NSLocalizedDescriptionKey }],
        &[description.as_ref() as &AnyObject],
    );
    unsafe {
        NSError::errorWithDomain_code_userInfo(
            &NSString::from_str("octoweb.extensions"),
            1,
            Some(&info),
        )
    }
}

fn describe(error: *mut NSError) -> String {
    unsafe { error.as_ref() }.map_or_else(
        || "Unknown error".to_string(),
        |e| e.localizedDescription().to_string(),
    )
}

// ── Tab WebViews ─────────────────────────────────────────────────────────────

/// Configuration for a normal tab's WKWebView. Extension pages need their own
/// context's configuration; every other page gets the shared controller on
/// the workspace's data store (wry leaves the store alone when handed a
/// configuration). `None` when extensions are unavailable.
pub fn tab_configuration(
    url: &str,
    data_store_id: Option<[u8; 16]>,
) -> Option<Retained<WKWebViewConfiguration>> {
    let mtm = MainThreadMarker::new()?;
    let controller = with(|s| s.controller.clone())?;
    unsafe {
        if url.starts_with("webkit-extension:") {
            let context = NSURL::URLWithString(&NSString::from_str(url))
                .and_then(|u| controller.extensionContextForURL(&u));
            if let Some(config) = context.and_then(|c| c.webViewConfiguration()) {
                return Some(config);
            }
        }
        let config = WKWebViewConfiguration::new(mtm);
        config.setWebExtensionController(Some(&controller));
        let store = match data_store_id {
            Some(id) => WKWebsiteDataStore::dataStoreForIdentifier(&NSUUID::from_bytes(id), mtm),
            None => WKWebsiteDataStore::defaultDataStore(mtm),
        };
        config.setWebsiteDataStore(&store);
        Some(config)
    }
}

/// Run a key-down `NSEvent` past extension keyboard commands; `true` when
/// one handled it.
pub fn perform_command(event: *mut AnyObject) -> bool {
    let Some(event) = (unsafe { (event as *const NSEvent).as_ref() }) else {
        return false;
    };
    let contexts = with(State::contexts).unwrap_or_default();
    contexts
        .iter()
        .any(|c| unsafe { c.performCommandForEvent(event) })
}

type WillOpenMenuImp = extern "C-unwind" fn(*mut AnyObject, Sel, *mut NSMenu, *mut NSEvent);

/// `-[NSView willOpenMenu:withEvent:]` as wry's WebView class had it.
static ORIGINAL_WILL_OPEN_MENU: AtomicUsize = AtomicUsize::new(0);

/// Add extension items to tab context menus. The hook goes on the WebView
/// class once; non-tab WebViews share the class and get nothing added.
pub fn hook_context_menu(webview_ptr: usize) {
    if ORIGINAL_WILL_OPEN_MENU.load(Ordering::SeqCst) != 0 || with(|_| ()).is_none() {
        return;
    }
    let sel = sel!(willOpenMenu:withEvent:);
    unsafe {
        let wv = webview_ptr as *mut AnyObject;
        let class: *const AnyClass = msg_send![&*wv, class];
        let previous = objc2::ffi::class_replaceMethod(
            class as *mut _,
            sel,
            std::mem::transmute::<WillOpenMenuImp, unsafe extern "C-unwind" fn()>(will_open_menu),
            c"v@:@@".as_ptr(),
        );
        // Not overridden on the class itself: chain to the inherited version.
        let original = previous.or_else(|| {
            let superclass = (*class).superclass()?;
            superclass.instance_method(sel).map(|m| m.implementation())
        });
        match original {
            Some(original) => ORIGINAL_WILL_OPEN_MENU.store(original as usize, Ordering::SeqCst),
            None => tracing::warn!(
                "WebView class has no willOpenMenu:withEvent: — no extension menu items"
            ),
        }
    }
}

extern "C-unwind" fn will_open_menu(
    this: *mut AnyObject,
    sel: Sel,
    menu: *mut NSMenu,
    event: *mut NSEvent,
) {
    let original = ORIGINAL_WILL_OPEN_MENU.load(Ordering::SeqCst);
    if original != 0 {
        let original: WillOpenMenuImp = unsafe { std::mem::transmute(original) };
        original(this, sel, menu, event);
    }
    let Some(menu) = (unsafe { menu.as_ref() }) else {
        return;
    };
    let Some((tab, contexts)) =
        with(|s| Some((s.model.tab_by_webview(this as usize)?, s.contexts()))).flatten()
    else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let mut separated = false;
    for context in contexts {
        let items = unsafe { context.menuItemsForTab(ProtocolObject::from_ref(&*tab)) };
        for item in items.iter() {
            if !separated {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                separated = true;
            }
            menu.addItem(&item);
        }
    }
}

// ── Settings and toolbar ─────────────────────────────────────────────────────

pub fn handle(event: Event) -> Vec<Action> {
    match event {
        Event::App(action) => vec![action],
        Event::Install(input) => install(input.trim()),
        Event::PickAndInstall => pick().map_or_else(Vec::new, |path| install_path(&path)),
        Event::Downloaded { id, result } => match result {
            Ok(path) => {
                confirm_install(id, path);
                Vec::new()
            }
            Err(e) => vec![Action::Notify(e)],
        },
        Event::SetEnabled(id, enabled) => {
            set_enabled(&id, enabled);
            vec![Action::Installed]
        }
        Event::Remove(id) => {
            remove(&id);
            vec![Action::Installed]
        }
        Event::OpenOptions(id) => options_url(&id)
            .map(|url| Action::OpenTab {
                url: Some(url),
                workspace: None,
                active: true,
                reply: None,
            })
            .into_iter()
            .collect(),
        Event::Click { id, anchor } => {
            click(&id, anchor);
            Vec::new()
        }
    }
}

fn install(input: &str) -> Vec<Action> {
    if input.is_empty() {
        return Vec::new();
    }
    if with(|_| ()).is_none() {
        return vec![Action::Notify("Extensions need macOS 15.4 or later".into())];
    }
    if let Some(id) = extension_store::store_id(input) {
        std::thread::spawn(move || {
            let result = extension_store::download_store_item(&id);
            send(Event::Downloaded { id, result });
        });
        return vec![Action::Notify("Downloading the extension…".into())];
    }
    let path = match input.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(input),
    };
    install_path(&path)
}

fn pick() -> Option<PathBuf> {
    let mtm = MainThreadMarker::new()?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(true);
    panel.setAllowsMultipleSelection(false);
    panel.setMessage(Some(&NSString::from_str(
        "Choose an extension folder, or a .zip, .xpi or .crx file",
    )));
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel
        .URL()
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
}

fn install_path(path: &Path) -> Vec<Action> {
    if with(|_| ()).is_none() {
        return vec![Action::Notify("Extensions need macOS 15.4 or later".into())];
    }
    // Reinstalling a folder that is already installed updates it in place.
    let id = with(|s| {
        s.installed
            .iter()
            .find(|i| i.record.path == path)
            .map(|i| i.record.id.clone())
    })
    .flatten()
    .unwrap_or_else(extension_store::new_id);
    match extension_store::import(path, &id) {
        Ok(stored) => {
            confirm_install(id, stored);
            Vec::new()
        }
        Err(e) => vec![Action::Notify(e)],
    }
}

/// Parse what was imported, show what it asks for, and install on yes. An
/// installed extension with the same id is replaced, keeping its data.
fn confirm_install(id: String, path: PathBuf) {
    parse(&path.clone(), move |result| {
        let extension = match result {
            Ok(extension) => extension,
            Err(e) => {
                extension_store::discard(&path);
                send_app(Action::Notify(format!("Couldn't add the extension: {e}")));
                return;
            }
        };
        let name = display_name(&extension);
        let (permissions, patterns) = unsafe {
            (
                extension
                    .requestedPermissions()
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>(),
                extension
                    .allRequestedMatchPatterns()
                    .iter()
                    .map(|p| p.string().to_string())
                    .collect::<Vec<_>>(),
            )
        };
        let mut detail = Vec::new();
        if !permissions.is_empty() {
            let mut sorted = permissions.clone();
            sorted.sort();
            detail.push(format!("It can use: {}", sorted.join(", ")));
        }
        if !patterns.is_empty() {
            let mut sorted = patterns.clone();
            sorted.sort();
            detail.push(format!("It can read and change:\n{}", sorted.join("\n")));
        }
        let icon = unsafe { extension.iconForSize(CGSize::new(ALERT_ICON_PT, ALERT_ICON_PT)) };
        if !confirm(
            &format!("Add “{name}”?"),
            &detail.join("\n\n"),
            "Add Extension",
            icon.as_deref(),
        ) {
            extension_store::discard(&path);
            return;
        }
        let replaced = with_mut(|s| {
            let i = s.installed.iter().position(|i| i.record.id == id)?;
            Some(s.installed.remove(i))
        })
        .flatten();
        if let Some(old) = replaced {
            if let Some(context) = &old.context {
                stop(context);
            }
            if old.record.path != path {
                extension_store::discard(&old.record.path);
            }
        }
        let record = Record {
            id: id.clone(),
            path: path.clone(),
            enabled: true,
            permissions,
            patterns,
        };
        with_mut(|s| {
            s.installed.push(Installed {
                record,
                extension: Some(extension.clone()),
                context: None,
                error: None,
            });
            s.save();
        });
        start(&id);
        send_app(Action::Installed);
        send_app(Action::Notify(format!("Added {name}")));
    });
}

fn set_enabled(id: &str, enabled: bool) {
    let context = with_mut(|s| {
        let installed = s.find_mut(id)?;
        installed.record.enabled = enabled;
        let context = if enabled {
            None
        } else {
            installed.context.take()
        };
        s.save();
        context
    })
    .flatten();
    match context {
        Some(context) => stop(&context),
        None if enabled => start(id),
        None => {}
    }
}

/// Uninstall: unload, delete its stored data and archive, forget it.
fn remove(id: &str) {
    let Some((removed, controller)) = with_mut(|s| {
        let i = s.installed.iter().position(|i| i.record.id == id)?;
        let removed = s.installed.remove(i);
        s.save();
        Some((removed, s.controller.clone()))
    })
    .flatten() else {
        return;
    };
    if let Some(context) = &removed.context {
        stop(context);
    }
    let context = removed.context.clone().or_else(|| {
        let extension = removed.extension.as_ref()?;
        let context = unsafe { WKWebExtensionContext::contextForExtension(extension) };
        unsafe { context.setUniqueIdentifier(&NSString::from_str(id)) };
        Some(context)
    });
    if let (Some(context), Some(mtm)) = (context, MainThreadMarker::new()) {
        unsafe {
            let types = WKWebExtensionController::allExtensionDataTypes(mtm);
            let remover = controller.clone();
            let types_for_removal = types.clone();
            let block = RcBlock::new(move |record: *mut WKWebExtensionDataRecord| {
                if let Some(record) = Retained::retain(record) {
                    remover.removeDataOfTypes_fromDataRecords_completionHandler(
                        &types_for_removal,
                        &NSArray::from_retained_slice(&[record]),
                        &RcBlock::new(|| {}),
                    );
                }
            });
            controller.fetchDataRecordOfTypes_forExtensionContext_completionHandler(
                &types, &context, &block,
            );
        }
    }
    extension_store::discard(&removed.record.path);
}

fn options_url(id: &str) -> Option<String> {
    let context = with(|s| s.find(id)?.context.clone()).flatten()?;
    url_string(unsafe { context.optionsPageURL() })
}

fn click(id: &str, anchor: [f64; 4]) {
    let rect = CGRect::new(
        CGPoint::new(anchor[0], anchor[1]),
        CGSize::new(anchor[2], anchor[3]),
    );
    let Some((context, tab)) = with_mut(|s| {
        s.anchors.insert(id.to_string(), rect);
        Some((s.find(id)?.context.clone()?, s.model.active_tab()))
    })
    .flatten() else {
        return;
    };
    unsafe { context.performActionForTab(tab.as_deref().map(ProtocolObject::from_ref)) };
}

fn png_data_uri(image: &NSImage) -> Option<String> {
    use base64::Engine;
    let tiff = image.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png.to_vec())
    ))
}

/// Script pushing the toolbar buttons — one per loaded extension with an
/// action, as it stands for the focused tab — into the address bar.
pub fn toolbar_script() -> String {
    let (contexts, tab) = with(|s| {
        let contexts: Vec<(String, Retained<WKWebExtensionContext>)> = s
            .installed
            .iter()
            .filter_map(|i| Some((i.record.id.clone(), i.context.clone()?)))
            .collect();
        (contexts, s.model.active_tab())
    })
    .unwrap_or_default();
    let buttons: Vec<serde_json::Value> = contexts
        .iter()
        .filter_map(|(id, context)| {
            let action =
                unsafe { context.actionForTab(tab.as_deref().map(ProtocolObject::from_ref)) }?;
            let icon = unsafe { action.iconForSize(CGSize::new(TOOLBAR_ICON_PT, TOOLBAR_ICON_PT)) };
            Some(serde_json::json!({
                "id": id,
                "label": unsafe { action.label() }.to_string(),
                "badge": unsafe { action.badgeText() }.to_string(),
                "enabled": unsafe { action.isEnabled() },
                "icon": icon.as_deref().and_then(png_data_uri),
            }))
        })
        .collect();
    tracing::debug!(buttons = ?buttons.iter().map(|b| (&b["label"], &b["badge"])).collect::<Vec<_>>(), "extension toolbar");
    format!(
        "window.__setExtensionButtons && window.__setExtensionButtons({})",
        serde_json::Value::Array(buttons)
    )
}

/// Script pushing the installed list into the settings panel.
pub fn settings_script() -> String {
    struct Row {
        record: Record,
        extension: Option<Retained<WKWebExtension>>,
        loaded: bool,
        error: Option<String>,
    }
    let rows: Vec<Row> = with(|s| {
        s.installed
            .iter()
            .map(|i| Row {
                record: i.record.clone(),
                extension: i.extension.clone(),
                loaded: i.context.is_some(),
                error: i.error.clone(),
            })
            .collect()
    })
    .unwrap_or_default();
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            let ext = row.extension.as_deref();
            let text = |f: unsafe fn(&WKWebExtension) -> Option<Retained<NSString>>| {
                ext.and_then(|e| unsafe { f(e) }).map(|s| s.to_string()).unwrap_or_default()
            };
            serde_json::json!({
                "id": row.record.id,
                "name": ext.map_or_else(|| row.record.path.display().to_string(), display_name),
                "version": text(WKWebExtension::displayVersion),
                "description": text(WKWebExtension::displayDescription),
                "enabled": row.record.enabled,
                "options": row.loaded && ext.is_some_and(|e| unsafe { e.hasOptionsPage() }),
                "error": row.error,
                "path": row.record.path.display().to_string(),
                "icon": ext
                    .and_then(|e| unsafe { e.iconForSize(CGSize::new(SETTINGS_ICON_PT, SETTINGS_ICON_PT)) })
                    .as_deref()
                    .and_then(png_data_uri),
            })
        })
        .collect();
    format!(
        "window.__setExtensions && window.__setExtensions({})",
        serde_json::json!({ "supported": with(|_| ()).is_some(), "items": items })
    )
}
