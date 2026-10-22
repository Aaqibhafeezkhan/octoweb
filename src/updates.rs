//! Updates through Sparkle 2: a daily check of octoweb.xyz/appcast.xml, a
//! silent download, and an install when Octoweb quits. See
//! docs/auto-updates.md for publishing.
//!
//! Sparkle.framework is loaded at runtime from `Contents/Frameworks`, where
//! build.sh embeds it, instead of being linked: a `cargo run` binary or CI's
//! bare executable has no bundle to find it in and must still start. It loads
//! only when Info.plist names a feed and a public key, so a `--dev` build,
//! whose feed build.sh removes, never touches it.
//!
//! A downloaded update never relaunches Octoweb on its own. The session brings
//! tabs back, but not text typed into pages, private tabs, session cookies or
//! terminal processes, so the install waits for a quit the user chose. After a
//! week without one Sparkle shows its own "install and relaunch" prompt.
//!
//! Sparkle runs on the main thread and calls its delegate there, so the state
//! lives in a thread-local.

use std::cell::RefCell;

use block2::DynBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, NSObject};
use objc2::{class, define_class, msg_send, sel, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSMenuItem;
use objc2_foundation::{NSError, NSString};

struct Updater {
	/// `SPUStandardUpdaterController`: Sparkle's standard UI around the updater.
	controller: Retained<AnyObject>,
	/// Sparkle holds its delegate weakly.
	_delegate: Retained<UpdaterDelegate>,
}

thread_local! {
	static UPDATER: RefCell<Option<Updater>> = const { RefCell::new(None) };
}

/// `SUNoUpdateError`: a check that found nothing newer.
const SU_NO_UPDATE_ERROR: isize = 1001;

define_class!(
	// SAFETY: NSObject has no subclassing requirements; no Drop impl.
	#[unsafe(super(NSObject))]
	#[thread_kind = MainThreadOnly]
	#[name = "OctowebUpdaterDelegate"]
	struct UpdaterDelegate;

	impl UpdaterDelegate {
		/// NO leaves the update to Sparkle: it installs at quit, and Sparkle's
		/// schedule keeps running so it can prompt after a week without a quit.
		#[unsafe(method(updater:willInstallUpdateOnQuit:immediateInstallationBlock:))]
		fn will_install_on_quit(
			&self,
			_updater: &AnyObject,
			item: &AnyObject,
			_install_now: &DynBlock<dyn Fn()>,
		) -> bool {
			let version: Option<Retained<NSString>> = unsafe { msg_send![item, displayVersionString] };
			let version = version.map(|v| v.to_string()).unwrap_or_default();
			tracing::info!(version, "update downloaded; installs when Octoweb quits");
			false
		}

		#[unsafe(method(updater:didAbortWithError:))]
		fn did_abort(&self, _updater: &AnyObject, error: &NSError) {
			if error.code() == SU_NO_UPDATE_ERROR {
				tracing::debug!("no update available");
			} else {
				tracing::warn!(error = %error.localizedDescription(), "update aborted");
			}
		}
	}
);

/// Start the daily schedule. Main thread, once NSApplication exists.
pub fn start() {
	let Some(mtm) = MainThreadMarker::new() else {
		return;
	};
	if UPDATER.with_borrow(Option::is_some) {
		return;
	}
	if info_string("SUFeedURL").is_empty() || info_string("SUPublicEDKey").is_empty() {
		tracing::info!("updates off: no feed or public key in this build");
		return;
	}
	let Some(controller_class) = load_sparkle() else {
		return;
	};

	let delegate: Retained<UpdaterDelegate> = unsafe { msg_send![UpdaterDelegate::alloc(mtm), init] };
	// Sparkle may ask whether its delegate conforms. The protocol is
	// registered only once the framework has loaded, hence at runtime.
	if let Some(protocol) = AnyProtocol::get(c"SPUUpdaterDelegate") {
		let class = UpdaterDelegate::class() as *const AnyClass as *mut AnyClass;
		// SAFETY: adding a protocol to our own registered class; idempotent.
		unsafe { objc2::ffi::class_addProtocol(class, protocol) };
	}

	let controller: Option<Retained<AnyObject>> = unsafe {
		let controller: Allocated<AnyObject> = msg_send![controller_class, alloc];
		msg_send![
			controller,
			initWithStartingUpdater: false,
			updaterDelegate: &*delegate,
			userDriverDelegate: std::ptr::null::<AnyObject>()
		]
	};
	let Some(controller) = controller else {
		tracing::error!("SPUStandardUpdaterController did not initialize");
		return;
	};
	// The updater's own start rather than the controller's: a broken feed or
	// key is logged, not put in front of the user in an alert.
	let started: Result<(), Retained<NSError>> = unsafe {
		let updater: *mut AnyObject = msg_send![&*controller, updater];
		msg_send![updater, startUpdater: _]
	};
	if let Err(e) = started {
		tracing::error!(error = %e.localizedDescription(), "updater did not start");
		return;
	}
	tracing::info!("updater started");
	UPDATER.with_borrow_mut(|u| {
		*u = Some(Updater {
			controller,
			_delegate: delegate,
		})
	});
}

/// "Check for Updates…" for the app menu, enabled while Sparkle can check.
/// `None` when the updater is off.
pub fn menu_item(mtm: MainThreadMarker) -> Option<Retained<NSMenuItem>> {
	let controller = controller()?;
	unsafe {
		let item = NSMenuItem::initWithTitle_action_keyEquivalent(
			mtm.alloc(),
			&NSString::from_str("Check for Updates…"),
			Some(sel!(checkForUpdates:)),
			&NSString::from_str(""),
		);
		// The menu item holds its target weakly; UPDATER keeps it alive.
		item.setTarget(Some(&controller));
		Some(item)
	}
}

/// Sparkle's window: the update it found, or that Octoweb is up to date.
pub fn check_now() {
	if let Some(controller) = controller() {
		let _: () = unsafe { msg_send![&*controller, checkForUpdates: std::ptr::null::<AnyObject>()] };
	}
}

/// The daily check. Sparkle keeps it, and the download setting below, in
/// Octoweb's user defaults rather than config.toml.
pub fn set_automatic_checks(on: bool) {
	if let Some(updater) = updater() {
		let _: () = unsafe { msg_send![&*updater, setAutomaticallyChecksForUpdates: on] };
		tracing::info!(on, "daily update check changed");
	}
}

/// Download found updates in the background, to install at quit.
pub fn set_automatic_downloads(on: bool) {
	if let Some(updater) = updater() {
		let _: () = unsafe { msg_send![&*updater, setAutomaticallyDownloadsUpdates: on] };
		tracing::info!(on, "automatic update download changed");
	}
}

/// `window.__setUpdates(…)` for the settings modal.
pub fn settings_script() -> String {
	let version = env!("CARGO_PKG_VERSION");
	let state = match updater() {
		Some(updater) => {
			let (checks, downloads): (bool, bool) = unsafe {
				(
					msg_send![&*updater, automaticallyChecksForUpdates],
					msg_send![&*updater, automaticallyDownloadsUpdates],
				)
			};
			serde_json::json!({ "available": true, "version": version, "checks": checks, "downloads": downloads })
		}
		None => serde_json::json!({ "available": false, "version": version }),
	};
	format!("window.__setUpdates && window.__setUpdates({state})")
}

fn controller() -> Option<Retained<AnyObject>> {
	UPDATER.with_borrow(|u| u.as_ref().map(|u| u.controller.clone()))
}

/// The controller's `SPUUpdater`.
fn updater() -> Option<Retained<AnyObject>> {
	let controller = controller()?;
	unsafe { msg_send![&*controller, updater] }
}

/// A string from the app's Info.plist, empty when missing.
fn info_string(key: &str) -> String {
	unsafe {
		let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
		let value: Option<Retained<AnyObject>> =
			msg_send![bundle, objectForInfoDictionaryKey: &*NSString::from_str(key)];
		value
			.and_then(|v| v.downcast_ref::<NSString>().map(|s| s.to_string()))
			.unwrap_or_default()
	}
}

/// Load the embedded Sparkle.framework and return its controller class.
fn load_sparkle() -> Option<&'static AnyClass> {
	unsafe {
		let main: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
		let frameworks: Option<Retained<NSString>> = msg_send![main, privateFrameworksPath];
		let path = NSString::from_str(&format!("{}/Sparkle.framework", frameworks?));
		let bundle: Option<Retained<AnyObject>> = msg_send![class!(NSBundle), bundleWithPath: &*path];
		let Some(bundle) = bundle else {
			tracing::warn!(path = %path, "Sparkle.framework is not in the app bundle; updates off");
			return None;
		};
		let loaded: Result<(), Retained<NSError>> = msg_send![&*bundle, loadAndReturnError: _];
		if let Err(e) = loaded {
			tracing::error!(error = %e.localizedDescription(), "Sparkle.framework did not load; updates off");
			return None;
		}
	}
	let class = AnyClass::get(c"SPUStandardUpdaterController");
	if class.is_none() {
		tracing::error!("Sparkle.framework has no SPUStandardUpdaterController; updates off");
	}
	class
}
