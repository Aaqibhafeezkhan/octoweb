//! The Objective-C objects WebKit's extension runtime talks to: the
//! controller delegate, and the objects it sees as tabs and windows. Each
//! method is a thin call into `extensions`, which owns the state they answer
//! from and turns requests into app actions.

use std::ptr::{null_mut, NonNull};

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSImage;
use objc2_foundation::{NSArray, NSDate, NSError, NSNotFound, NSSet, NSString, NSURL};
use objc2_web_kit::{
    WKSnapshotConfiguration, WKWebExtensionAction, WKWebExtensionContext, WKWebExtensionController,
    WKWebExtensionControllerDelegate, WKWebExtensionMatchPattern, WKWebExtensionPermission,
    WKWebExtensionTab, WKWebExtensionTabConfiguration, WKWebExtensionWindow,
    WKWebExtensionWindowConfiguration, WKWebView,
};

use crate::extensions;

type TabReply = DynBlock<dyn Fn(*mut ProtocolObject<dyn WKWebExtensionTab>, *mut NSError)>;
type WindowReply = DynBlock<dyn Fn(*mut ProtocolObject<dyn WKWebExtensionWindow>, *mut NSError)>;
type Done = DynBlock<dyn Fn(*mut NSError)>;

pub struct TabIvars {
    id: usize,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OctowebExtensionTab"]
    #[ivars = TabIvars]
    pub struct ExtTab;

    unsafe impl NSObjectProtocol for ExtTab {}

    unsafe impl WKWebExtensionTab for ExtTab {
        #[unsafe(method_id(windowForWebExtensionContext:))]
        fn window_for(
            &self,
            _context: &WKWebExtensionContext,
        ) -> Option<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> {
            extensions::tab_window(self.id())
        }

        #[unsafe(method(indexInWindowForWebExtensionContext:))]
        fn index_for(&self, _context: &WKWebExtensionContext) -> usize {
            extensions::tab_index(self.id()).unwrap_or(NSNotFound as usize)
        }

        #[unsafe(method_id(webViewForWebExtensionContext:))]
        fn web_view_for(&self, _context: &WKWebExtensionContext) -> Option<Retained<WKWebView>> {
            extensions::tab_webview(self.id())
        }

        #[unsafe(method_id(titleForWebExtensionContext:))]
        fn title_for(&self, _context: &WKWebExtensionContext) -> Option<Retained<NSString>> {
            extensions::tab_title(self.id()).map(|t| NSString::from_str(&t))
        }

        #[unsafe(method_id(urlForWebExtensionContext:))]
        fn url_for(&self, _context: &WKWebExtensionContext) -> Option<Retained<NSURL>> {
            extensions::tab_url(self.id())
                .and_then(|u| NSURL::URLWithString(&NSString::from_str(&u)))
        }

        #[unsafe(method(isLoadingCompleteForWebExtensionContext:))]
        fn loading_complete_for(&self, _context: &WKWebExtensionContext) -> bool {
            !extensions::tab_loading(self.id())
        }

        #[unsafe(method(isPlayingAudioForWebExtensionContext:))]
        fn playing_audio_for(&self, _context: &WKWebExtensionContext) -> bool {
            extensions::tab_playing_audio(self.id())
        }

        #[unsafe(method(isSelectedForWebExtensionContext:))]
        fn selected_for(&self, _context: &WKWebExtensionContext) -> bool {
            extensions::tab_selected(self.id())
        }

        #[unsafe(method(activateForWebExtensionContext:completionHandler:))]
        fn activate_for(&self, _context: &WKWebExtensionContext, completion: &Done) {
            extensions::activate_tab(self.id());
            completion.call((null_mut(),));
        }

        #[unsafe(method(closeForWebExtensionContext:completionHandler:))]
        fn close_for(&self, _context: &WKWebExtensionContext, completion: &Done) {
            extensions::close_tab(self.id());
            completion.call((null_mut(),));
        }

        #[unsafe(method(loadURL:forWebExtensionContext:completionHandler:))]
        fn load_url_for(&self, url: &NSURL, _context: &WKWebExtensionContext, completion: &Done) {
            match url.absoluteString() {
                Some(url) => {
                    extensions::load_tab(self.id(), url.to_string());
                    completion.call((null_mut(),));
                }
                None => {
                    let err = extensions::error("Invalid URL");
                    completion.call((Retained::as_ptr(&err).cast_mut(),));
                }
            }
        }

        #[unsafe(method(duplicateUsingConfiguration:forWebExtensionContext:completionHandler:))]
        fn duplicate_for(
            &self,
            configuration: &WKWebExtensionTabConfiguration,
            _context: &WKWebExtensionContext,
            completion: &TabReply,
        ) {
            let active = unsafe { configuration.shouldBeActive() };
            extensions::duplicate_tab(self.id(), active, completion.copy());
        }

        #[unsafe(method(takeSnapshotUsingConfiguration:forWebExtensionContext:completionHandler:))]
        fn snapshot_for(
            &self,
            configuration: &WKSnapshotConfiguration,
            _context: &WKWebExtensionContext,
            completion: &DynBlock<dyn Fn(*mut NSImage, *mut NSError)>,
        ) {
            match extensions::tab_webview(self.id()) {
                Some(webview) => unsafe {
                    webview.takeSnapshotWithConfiguration_completionHandler(
                        Some(configuration),
                        completion,
                    );
                },
                None => {
                    let err = extensions::error("The tab is not loaded");
                    completion.call((null_mut(), Retained::as_ptr(&err).cast_mut()));
                }
            }
        }
    }
);

impl ExtTab {
    pub fn new(mtm: MainThreadMarker, id: usize) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TabIvars { id });
        unsafe { msg_send![super(this), init] }
    }

    pub fn id(&self) -> usize {
        self.ivars().id
    }
}

pub struct WindowIvars {
    id: String,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OctowebExtensionWindow"]
    #[ivars = WindowIvars]
    pub struct ExtWindow;

    unsafe impl NSObjectProtocol for ExtWindow {}

    unsafe impl WKWebExtensionWindow for ExtWindow {
        #[unsafe(method_id(tabsForWebExtensionContext:))]
        fn tabs_for(
            &self,
            _context: &WKWebExtensionContext,
        ) -> Retained<NSArray<ProtocolObject<dyn WKWebExtensionTab>>> {
            extensions::window_tabs(self.id())
        }

        #[unsafe(method_id(activeTabForWebExtensionContext:))]
        fn active_tab_for(
            &self,
            _context: &WKWebExtensionContext,
        ) -> Option<Retained<ProtocolObject<dyn WKWebExtensionTab>>> {
            extensions::window_active_tab(self.id())
        }

        #[unsafe(method(focusForWebExtensionContext:completionHandler:))]
        fn focus_for(&self, _context: &WKWebExtensionContext, completion: &Done) {
            extensions::focus_window(self.id());
            completion.call((null_mut(),));
        }
    }
);

impl ExtWindow {
    pub fn new(mtm: MainThreadMarker, id: String) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars { id });
        unsafe { msg_send![super(this), init] }
    }

    /// Id of the workspace this window stands for.
    pub fn id(&self) -> &str {
        &self.ivars().id
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OctowebExtensionControllerDelegate"]
    pub struct ExtDelegate;

    unsafe impl NSObjectProtocol for ExtDelegate {}

    unsafe impl WKWebExtensionControllerDelegate for ExtDelegate {
        #[unsafe(method_id(webExtensionController:openWindowsForExtensionContext:))]
        fn open_windows(
            &self,
            _controller: &WKWebExtensionController,
            _context: &WKWebExtensionContext,
        ) -> Retained<NSArray<ProtocolObject<dyn WKWebExtensionWindow>>> {
            extensions::open_windows()
        }

        #[unsafe(method_id(webExtensionController:focusedWindowForExtensionContext:))]
        fn focused_window(
            &self,
            _controller: &WKWebExtensionController,
            _context: &WKWebExtensionContext,
        ) -> Option<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> {
            extensions::focused_window()
        }

        #[unsafe(method(webExtensionController:openNewTabUsingConfiguration:forExtensionContext:completionHandler:))]
        fn open_new_tab(
            &self,
            _controller: &WKWebExtensionController,
            configuration: &WKWebExtensionTabConfiguration,
            _context: &WKWebExtensionContext,
            completion: &TabReply,
        ) {
            extensions::open_tab(configuration, completion.copy());
        }

        #[unsafe(method(webExtensionController:openNewWindowUsingConfiguration:forExtensionContext:completionHandler:))]
        fn open_new_window(
            &self,
            _controller: &WKWebExtensionController,
            configuration: &WKWebExtensionWindowConfiguration,
            _context: &WKWebExtensionContext,
            completion: &WindowReply,
        ) {
            extensions::open_window(configuration, completion.copy());
        }

        #[unsafe(method(webExtensionController:promptForPermissions:inTab:forExtensionContext:completionHandler:))]
        fn prompt_permissions(
            &self,
            _controller: &WKWebExtensionController,
            permissions: &NSSet<WKWebExtensionPermission>,
            _tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            context: &WKWebExtensionContext,
            completion: &DynBlock<dyn Fn(NonNull<NSSet<WKWebExtensionPermission>>, *mut NSDate)>,
        ) {
            extensions::prompt_permissions(context, permissions, completion.copy());
        }

        // WebKit asks for sites on its own: each page an extension with the
        // `tabs` permission can see, each CORS failure in its pages. Chrome
        // never prompts for those — site access comes from host permissions
        // granted at install or asked for with `permissions.request()`.
        #[unsafe(method(webExtensionController:promptForPermissionToAccessURLs:inTab:forExtensionContext:completionHandler:))]
        fn prompt_urls(
            &self,
            _controller: &WKWebExtensionController,
            _urls: &NSSet<NSURL>,
            _tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            _context: &WKWebExtensionContext,
            completion: &DynBlock<dyn Fn(NonNull<NSSet<NSURL>>, *mut NSDate)>,
        ) {
            completion.call((NonNull::from(&*NSSet::new()), null_mut()));
        }

        #[unsafe(method(webExtensionController:promptForPermissionMatchPatterns:inTab:forExtensionContext:completionHandler:))]
        fn prompt_patterns(
            &self,
            _controller: &WKWebExtensionController,
            patterns: &NSSet<WKWebExtensionMatchPattern>,
            _tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            context: &WKWebExtensionContext,
            completion: &DynBlock<dyn Fn(NonNull<NSSet<WKWebExtensionMatchPattern>>, *mut NSDate)>,
        ) {
            extensions::prompt_patterns(context, patterns, completion.copy());
        }

        #[unsafe(method(webExtensionController:didUpdateAction:forExtensionContext:))]
        fn did_update_action(
            &self,
            _controller: &WKWebExtensionController,
            _action: &WKWebExtensionAction,
            _context: &WKWebExtensionContext,
        ) {
            extensions::toolbar_changed();
        }

        #[unsafe(method(webExtensionController:presentPopupForAction:forExtensionContext:completionHandler:))]
        fn present_popup(
            &self,
            _controller: &WKWebExtensionController,
            action: &WKWebExtensionAction,
            context: &WKWebExtensionContext,
            completion: &Done,
        ) {
            match extensions::present_popup(action, context) {
                Ok(()) => completion.call((null_mut(),)),
                Err(msg) => {
                    let err = extensions::error(&msg);
                    completion.call((Retained::as_ptr(&err).cast_mut(),));
                }
            }
        }
    }
);

impl ExtDelegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}
