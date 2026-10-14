//! Native input for the mail pane (spec READ-3, TRI-11).
//!
//! Mail HTML renders in a sandboxed iframe without scripts, and WebKit runs
//! none of the page's listeners inside such a frame, so everything a user does
//! in a message is handled here, on the AppKit side:
//!
//! - **Links.** The pane itself never navigates, so any http(s) or `mailto:`
//!   navigation in the mail WebView is a link: a click, `target=_blank`, or the
//!   link menu. `webView:decidePolicyForNavigationAction:decisionHandler:` is
//!   swapped — same runtime patching as `site_proxy` — to hand it to `on_link`
//!   and cancel it. The link menu's "Open Link in New Window" skips that policy
//!   and asks the UI delegate for a window, so
//!   `webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:`
//!   is swapped too and returns none. Meta refresh and forms can't navigate a
//!   script-less sandboxed frame, so nothing else reaches these paths.
//! - **History.** The same policy refuses back and forward: the page has no
//!   history worth a step, and stepping back blanks the pane.
//! - **Menu.** `willOpenMenu:withEvent:` renames WebKit's new-window items
//!   to what they do here and drops download, back, forward and reload.
//! - **Keys.** A key typed while the message frame has focus never reaches the
//!   page's handler; `frame_key` turns it into a `window.__mail.key` call.
//! - **Swipe.** A scroll-wheel monitor follows two-finger gestures by phase.
//!   It owns the gesture — when it arms, the trackpad tick as it does, and
//!   whether the release commits — while the page draws it and decides what
//!   committing means there: Done on a row, back on the open thread.

use std::cell::{OnceCell, RefCell};
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSPoint, NSRect};

/// `WKNavigationActionPolicy` — Cancel = 0.
const POLICY_CANCEL: isize = 0;
/// `WKNavigationTypeLinkActivated`.
const NAV_LINK_ACTIVATED: isize = 0;
/// `WKNavigationTypeBackForward`.
const NAV_BACK_FORWARD: isize = 2;
/// `NSEventModifierFlagCommand`.
const FLAG_COMMAND: usize = 1 << 20;

// `WKMenuItemIdentifier*` values.
const MENU_OPEN_LINK: &str = "WKMenuItemIdentifierOpenLink";
const MENU_OPEN_LINK_NEW_WINDOW: &str = "WKMenuItemIdentifierOpenLinkInNewWindow";
const MENU_OPEN_IMAGE_NEW_WINDOW: &str = "WKMenuItemIdentifierOpenImageInNewWindow";
const MENU_DOWNLOAD_LINK: &str = "WKMenuItemIdentifierDownloadLinkedFile";
const MENU_GO_BACK: &str = "WKMenuItemIdentifierGoBack";
const MENU_GO_FORWARD: &str = "WKMenuItemIdentifierGoForward";
const MENU_RELOAD: &str = "WKMenuItemIdentifierReload";

/// `NSEventMaskScrollWheel` = 1 << `NSEventTypeScrollWheel` (22).
const MASK_SCROLL_WHEEL: u64 = 1 << 22;
// `NSEventPhase` bits.
const PHASE_BEGAN: usize = 1;
const PHASE_CHANGED: usize = 4;
const PHASE_ENDED: usize = 8;
const PHASE_CANCELLED: usize = 16;
/// Finger travel (points) before a gesture counts as horizontal or vertical.
const SWIPE_LOCK_PT: f64 = 8.0;
/// Finger travel (points) past which releasing a swipe marks Done.
const SWIPE_DONE_PT: f64 = 120.0;
/// A flick commits short of that: this far, still moving this fast (points
/// per event) when the fingers lift.
const SWIPE_FLICK_PT: f64 = 40.0;
const SWIPE_FLICK_SPEED_PT: f64 = 12.0;
/// `NSHapticFeedbackPatternAlignment`, `NSHapticFeedbackPerformanceTimeNow`.
const HAPTIC_ALIGNMENT: isize = 1;
const HAPTIC_NOW: usize = 1;

type PolicyImp =
    extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject, *mut AnyObject);
type CreateWebViewImp = extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
    *mut AnyObject,
) -> *mut AnyObject;
type WillOpenMenuImp = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject);

/// The mail WKWebView; the patched methods leave every other WebView alone.
static MAIL_VIEW: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_POLICY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CREATE_WEB_VIEW: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_WILL_OPEN_MENU: AtomicUsize = AtomicUsize::new(0);
/// The message frame has keyboard focus inside the mail page.
static FRAME_FOCUSED: AtomicBool = AtomicBool::new(false);
/// The page found nothing to swipe: the gesture goes back to scrolling.
static SWIPE_MISSED: AtomicBool = AtomicBool::new(false);

thread_local! {
    // AppKit calls the patched methods on the main thread only.
    static ON_LINK: OnceCell<Box<dyn Fn(Link)>> = const { OnceCell::new() };
}

/// A link the user opened in a message.
#[derive(Debug)]
pub struct Link {
    pub url: String,
    /// ⌘-click, or a new-window item of the link menu.
    pub background: bool,
}

/// Patch link navigation and the context menu of `mail_view`. Call once.
pub fn install(mail_view: usize, on_link: impl Fn(Link) + 'static) {
    MAIL_VIEW.store(mail_view, Ordering::SeqCst);
    ON_LINK.with(|cell| {
        assert!(
            cell.set(Box::new(on_link)).is_ok(),
            "mail_patch::install called twice"
        );
    });
    unsafe {
        let wv = mail_view as *mut AnyObject;
        let delegate: *mut AnyObject = msg_send![&*wv, navigationDelegate];
        let delegate_class: *const AnyClass = msg_send![&*delegate, class];
        let previous = objc2::ffi::class_replaceMethod(
            delegate_class as *mut _,
            sel!(webView:decidePolicyForNavigationAction:decisionHandler:),
            std::mem::transmute::<PolicyImp, unsafe extern "C-unwind" fn()>(decide_policy),
            c"v@:@@@?".as_ptr(),
        );
        let previous =
            previous.expect("wry's navigation delegate implements decidePolicyForNavigationAction");
        ORIGINAL_POLICY.store(previous as usize, Ordering::SeqCst);

        let ui_delegate: *mut AnyObject = msg_send![&*wv, UIDelegate];
        let ui_class: *const AnyClass = msg_send![&*ui_delegate, class];
        let previous = objc2::ffi::class_replaceMethod(
            ui_class as *mut _,
            sel!(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:),
            std::mem::transmute::<CreateWebViewImp, unsafe extern "C-unwind" fn()>(create_web_view),
            c"@@:@@@@".as_ptr(),
        );
        let previous =
            previous.expect("wry's UI delegate implements createWebViewWithConfiguration");
        ORIGINAL_CREATE_WEB_VIEW.store(previous as usize, Ordering::SeqCst);

        let menu_sel = sel!(willOpenMenu:withEvent:);
        let view_class: *const AnyClass = msg_send![&*wv, class];
        let previous = objc2::ffi::class_replaceMethod(
            view_class as *mut _,
            menu_sel,
            std::mem::transmute::<WillOpenMenuImp, unsafe extern "C-unwind" fn()>(will_open_menu),
            c"v@:@@".as_ptr(),
        );
        // Not overridden on the class itself: chain to the inherited version.
        let previous = previous
            .or_else(|| {
                let superclass = (*view_class).superclass()?;
                superclass
                    .instance_method(menu_sel)
                    .map(|m| m.implementation())
            })
            .expect("NSView implements willOpenMenu:withEvent:");
        ORIGINAL_WILL_OPEN_MENU.store(previous as usize, Ordering::SeqCst);
    }
}

extern "C-unwind" fn decide_policy(
    this: *mut AnyObject,
    sel: Sel,
    webview: *mut AnyObject,
    action: *mut AnyObject,
    handler: *mut AnyObject,
) {
    if webview as usize == MAIL_VIEW.load(Ordering::SeqCst)
        && unsafe { back_forward(action) || opened(action) }
    {
        unsafe {
            let block: &block2::Block<dyn Fn(isize)> =
                &*(handler as *const block2::Block<dyn Fn(isize)>);
            block.call((POLICY_CANCEL,));
        }
        return;
    }
    let original: PolicyImp =
        unsafe { std::mem::transmute(ORIGINAL_POLICY.load(Ordering::SeqCst)) };
    original(this, sel, webview, action, handler);
}

extern "C-unwind" fn create_web_view(
    this: *mut AnyObject,
    sel: Sel,
    webview: *mut AnyObject,
    configuration: *mut AnyObject,
    action: *mut AnyObject,
    features: *mut AnyObject,
) -> *mut AnyObject {
    if webview as usize == MAIL_VIEW.load(Ordering::SeqCst) {
        // A tab instead of a window, or nothing at all.
        unsafe { opened(action) };
        return std::ptr::null_mut();
    }
    let original: CreateWebViewImp =
        unsafe { std::mem::transmute(ORIGINAL_CREATE_WEB_VIEW.load(Ordering::SeqCst)) };
    original(this, sel, webview, configuration, action, features)
}

/// A move through the pane's history — Backspace, the Back menu item. Each
/// message frame's load leaves an entry, and going back to one loads its
/// `about:srcdoc` into the page itself: an empty document that leaves the
/// transparent pane invisible. The pane never has a page to go back to.
unsafe fn back_forward(action: *mut AnyObject) -> bool {
    let kind: isize = msg_send![&*action, navigationType];
    kind == NAV_BACK_FORWARD
}

/// Hand the link behind `action` to `on_link`; `false` when it is not one.
unsafe fn opened(action: *mut AnyObject) -> bool {
    let Some(link) = link_of(action) else {
        return false;
    };
    tracing::debug!(url = %link.url, background = link.background, "mail link");
    ON_LINK.with(|cell| cell.get().expect("set by install")(link));
    true
}

/// The link behind a navigation of the mail WebView, if it targets the web or
/// a mail address. The page itself loads as `about:blank`, its frames as
/// `about:srcdoc` and attachment previews as `blob:` — none of them a link.
unsafe fn link_of(action: *mut AnyObject) -> Option<Link> {
    let request: *mut AnyObject = msg_send![&*action, request];
    let url: *mut AnyObject = msg_send![&*request, URL];
    if url.is_null() {
        return None;
    }
    let url = ns_string(msg_send![&*url, absoluteString])?;
    let scheme = url
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return None;
    }
    let flags: usize = msg_send![&*action, modifierFlags];
    // WebKit's "Open Link in New Window" asks for a new window (no target
    // frame) without a click; a `target=_blank` click comes as link-activated.
    let target: *mut AnyObject = msg_send![&*action, targetFrame];
    let kind: isize = msg_send![&*action, navigationType];
    let background = flags & FLAG_COMMAND != 0 || (target.is_null() && kind != NAV_LINK_ACTIVATED);
    Some(Link { url, background })
}

unsafe fn ns_string(s: *mut AnyObject) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let bytes: *const u8 = msg_send![&*s, UTF8String];
    (!bytes.is_null()).then(|| CStr::from_ptr(bytes.cast()).to_string_lossy().into_owned())
}

extern "C-unwind" fn will_open_menu(
    this: *mut AnyObject,
    sel: Sel,
    menu: *mut AnyObject,
    event: *mut AnyObject,
) {
    let original: WillOpenMenuImp =
        unsafe { std::mem::transmute(ORIGINAL_WILL_OPEN_MENU.load(Ordering::SeqCst)) };
    original(this, sel, menu, event);
    if this as usize != MAIL_VIEW.load(Ordering::SeqCst) || menu.is_null() {
        return;
    }
    unsafe {
        let count: isize = msg_send![&*menu, numberOfItems];
        // Backwards: removing an item shifts the ones after it.
        for i in (0..count).rev() {
            let item: *mut AnyObject = msg_send![&*menu, itemAtIndex: i];
            let Some(id) = ns_string(msg_send![&*item, identifier]) else {
                continue;
            };
            let title = match id.as_str() {
                MENU_OPEN_LINK => "Open in New Tab",
                MENU_OPEN_LINK_NEW_WINDOW => "Open in Background Tab",
                MENU_OPEN_IMAGE_NEW_WINDOW => "Open Image in Background Tab",
                MENU_DOWNLOAD_LINK | MENU_GO_BACK | MENU_GO_FORWARD | MENU_RELOAD => {
                    let _: () = msg_send![&*menu, removeItemAtIndex: i];
                    continue;
                }
                _ => continue,
            };
            let title = objc2_foundation::NSString::from_str(title);
            let _: () = msg_send![&*item, setTitle: &*title];
        }
    }
}

/// The page reports whether the message frame holds keyboard focus.
pub fn set_frame_focused(focused: bool) {
    FRAME_FOCUSED.store(focused, Ordering::SeqCst);
}

/// The `window.__mail.key(…)` call for a key-down without ⌘/⌃/⌥ typed while
/// the message frame has focus, or `None` to leave the key to the frame:
/// Space and the page keys still scroll the message.
pub fn frame_key(event: *mut AnyObject, keycode: u16, shift: bool) -> Option<String> {
    if !FRAME_FOCUSED.load(Ordering::SeqCst) {
        return None;
    }
    let key = match keycode {
        // Space, Page Up, Page Down, Home, End.
        49 | 116 | 121 | 115 | 119 => return None,
        53 => "Escape".to_string(),
        36 | 76 => "Enter".to_string(),
        123 => "ArrowLeft".to_string(),
        124 => "ArrowRight".to_string(),
        125 => "ArrowDown".to_string(),
        126 => "ArrowUp".to_string(),
        _ => unsafe { ns_string(msg_send![&*event, characters]) }.filter(|c| !c.is_empty())?,
    };
    Some(format!(
        "window.__mail && window.__mail.key({})",
        serde_json::json!({ "key": key, "shift": shift })
    ))
}

/// The page found no row or thread under the swipe that just began.
pub fn swipe_missed() {
    SWIPE_MISSED.store(true, Ordering::SeqCst);
}

#[derive(Clone, Copy)]
enum Gesture {
    Idle,
    /// Fingers down, not yet clearly horizontal or vertical.
    Pending {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    /// `speed`: the finger travel of the latest event.
    Swiping {
        dx: f64,
        speed: f64,
    },
    /// A vertical gesture, or one outside the pane: left to scrolling.
    Ignored,
}

/// Follow two-finger gestures over `mail_view` and drive the page's
/// `window.__mail.swipe` for a horizontal one. Call once.
pub fn monitor_swipes(mail_view: usize) {
    let gesture = RefCell::new(Gesture::Idle);
    let handler = block2::RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
        let mut g = gesture.borrow_mut();
        // SAFETY: the scroll-wheel mask guarantees an NSEvent of that type,
        // and the mail view lives as long as the app.
        let consumed = unsafe { track(&mut g, event, mail_view as *mut AnyObject) };
        if consumed {
            std::ptr::null_mut()
        } else {
            event
        }
    });
    unsafe {
        let _: *mut AnyObject = msg_send![
            class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: MASK_SCROLL_WHEEL,
            handler: &*handler,
        ];
    }
    // AppKit copied the block; this reference only has to stay alive.
    std::mem::forget(handler);
}

/// Advance `g` by one scroll event; `true` when the event is the swipe's and
/// must not scroll the page.
unsafe fn track(g: &mut Gesture, event: *mut AnyObject, view: *mut AnyObject) -> bool {
    if matches!(g, Gesture::Swiping { .. }) && SWIPE_MISSED.load(Ordering::SeqCst) {
        *g = Gesture::Ignored;
    }
    let phase: usize = msg_send![&*event, phase];
    let momentum: usize = msg_send![&*event, momentumPhase];
    if phase == 0 {
        // Momentum after the fingers lift belongs to the swipe that made it;
        // a wheel mouse has no phases and never swipes.
        return momentum != 0 && matches!(g, Gesture::Swiping { .. });
    }
    if phase & PHASE_BEGAN != 0 {
        *g = match point_in(view, event) {
            Some((x, y)) => Gesture::Pending {
                x,
                y,
                dx: 0.0,
                dy: 0.0,
            },
            None => Gesture::Ignored,
        };
        return false;
    }
    let delta_x: f64 = msg_send![&*event, scrollingDeltaX];
    let delta_y: f64 = msg_send![&*event, scrollingDeltaY];
    // With natural scrolling the content follows the fingers; either way a
    // positive `finger` is the fingers moving right.
    let natural: bool = msg_send![&*event, isDirectionInvertedFromDevice];
    let finger = if natural { delta_x } else { -delta_x };
    match *g {
        Gesture::Pending { x, y, dx, dy } if phase & PHASE_CHANGED != 0 => {
            let (dx, dy) = (dx + finger, dy + delta_y.abs());
            if dx.abs() + dy < SWIPE_LOCK_PT {
                *g = Gesture::Pending { x, y, dx, dy };
                return false;
            }
            if dx <= 2.0 * dy {
                *g = Gesture::Ignored;
                return false;
            }
            tracing::debug!(x, y, dx, natural, "mail swipe");
            SWIPE_MISSED.store(false, Ordering::SeqCst);
            call(
                view,
                &format!("window.__mail && window.__mail.swipe('begin', {x}, {y})"),
            );
            moved(view, 0.0, dx);
            *g = Gesture::Swiping { dx, speed: finger };
            true
        }
        Gesture::Swiping { dx, .. } if phase & PHASE_CHANGED != 0 => {
            moved(view, dx, dx + finger);
            *g = Gesture::Swiping {
                dx: dx + finger,
                speed: finger,
            };
            true
        }
        // Stays Swiping so the momentum that follows is swallowed too.
        Gesture::Swiping { dx, speed } if phase & PHASE_ENDED != 0 => {
            let done =
                dx >= SWIPE_DONE_PT || (dx >= SWIPE_FLICK_PT && speed >= SWIPE_FLICK_SPEED_PT);
            call(
                view,
                &format!("window.__mail && window.__mail.swipe('end', {done})"),
            );
            true
        }
        Gesture::Swiping { .. } if phase & PHASE_CANCELLED != 0 => {
            call(view, "window.__mail && window.__mail.swipe('end', false)");
            *g = Gesture::Ignored;
            true
        }
        Gesture::Swiping { .. } => true,
        _ => false,
    }
}

/// The swipe went from `from` to `to`: show it, and tick the trackpad as it
/// crosses the point where releasing marks Done, either way.
unsafe fn moved(view: *mut AnyObject, from: f64, to: f64) {
    let armed = to >= SWIPE_DONE_PT;
    if armed != (from >= SWIPE_DONE_PT) {
        let performer: *mut AnyObject =
            msg_send![class!(NSHapticFeedbackManager), defaultPerformer];
        let _: () = msg_send![
            &*performer,
            performFeedbackPattern: HAPTIC_ALIGNMENT,
            performanceTime: HAPTIC_NOW,
        ];
    }
    call(
        view,
        &format!("window.__mail && window.__mail.swipe('move', {to}, {armed})"),
    );
}

/// Run `js` in the mail page straight from the event monitor: no event-loop
/// hop between the fingers and the frame they move.
unsafe fn call(view: *mut AnyObject, js: &str) {
    let js = objc2_foundation::NSString::from_str(js);
    let _: () = msg_send![
        &*view,
        evaluateJavaScript: &*js,
        completionHandler: None::<&block2::Block<dyn Fn(*mut AnyObject, *mut AnyObject)>>,
    ];
}

/// The event's position in `view`, in its top-left-origin coordinates (CSS px
/// at page zoom 1), when the view is on screen and under the pointer.
unsafe fn point_in(view: *mut AnyObject, event: *mut AnyObject) -> Option<(f64, f64)> {
    let hidden: bool = msg_send![&*view, isHiddenOrHasHiddenAncestor];
    let event_window: *mut AnyObject = msg_send![&*event, window];
    let view_window: *mut AnyObject = msg_send![&*view, window];
    if hidden || event_window.is_null() || event_window != view_window {
        return None;
    }
    let in_window: NSPoint = msg_send![&*event, locationInWindow];
    let p: NSPoint =
        msg_send![&*view, convertPoint: in_window, fromView: std::ptr::null::<AnyObject>()];
    let bounds: NSRect = msg_send![&*view, bounds];
    let flipped: bool = msg_send![&*view, isFlipped];
    let y = if flipped {
        p.y
    } else {
        bounds.size.height - p.y
    };
    let inside = p.x >= 0.0 && p.x < bounds.size.width && y >= 0.0 && y < bounds.size.height;
    inside.then_some((p.x, y))
}
