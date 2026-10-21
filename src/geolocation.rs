//! Positions for `navigator.geolocation`.
//!
//! WKWebView on macOS has no location source of its own (Safari brings one),
//! so a granted request never got a position. This registers a provider on
//! the tabs' process pool through WebKit's C API — on Cocoa a `WKContextRef`
//! is the `WKProcessPool` object itself — and feeds it from one
//! `CLLocationManager`, running only while a page is watching. macOS asks
//! once for Location Services access (`NSLocationUsageDescription`).
//!
//! WebKit calls the provider on the main thread and CoreLocation delivers to
//! its delegate on the thread that created the manager (main), so state lives
//! in a thread-local.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{class, define_class, msg_send, sel, Encode, Encoding, MainThreadMarker, MainThreadOnly};

type WKRef = *const c_void;

#[repr(C)]
struct ProviderBase {
	version: i32,
	client_info: *const c_void,
}

/// `WKGeolocationProviderV1`.
#[repr(C)]
struct Provider {
	base: ProviderBase,
	start_updating: extern "C" fn(WKRef, *const c_void),
	stop_updating: extern "C" fn(WKRef, *const c_void),
	set_enable_high_accuracy: extern "C" fn(WKRef, bool, *const c_void),
}

// SAFETY: function pointers and a null client pointer, never written.
unsafe impl Sync for Provider {}

static PROVIDER: Provider = Provider {
	base: ProviderBase {
		version: 1,
		client_info: std::ptr::null(),
	},
	start_updating,
	stop_updating,
	set_enable_high_accuracy,
};

#[link(name = "WebKit", kind = "framework")]
extern "C" {
	fn WKContextGetGeolocationManager(context: WKRef) -> WKRef;
	fn WKGeolocationManagerSetProvider(manager: WKRef, provider: *const ProviderBase);
	fn WKGeolocationManagerProviderDidChangePosition(manager: WKRef, position: WKRef);
	fn WKGeolocationManagerProviderDidFailToDeterminePosition(manager: WKRef);
	fn WKGeolocationPositionCreate(timestamp: f64, latitude: f64, longitude: f64, accuracy: f64) -> WKRef;
	fn WKRelease(object: WKRef);
}

#[link(name = "CoreLocation", kind = "framework")]
extern "C" {
	static kCLLocationAccuracyBest: f64;
	static kCLLocationAccuracyHundredMeters: f64;
}

/// `CLAuthorizationStatus` — NotDetermined = 0, Restricted = 1, Denied = 2.
const AUTH_NOT_DETERMINED: i32 = 0;
const AUTH_RESTRICTED: i32 = 1;
const AUTH_DENIED: i32 = 2;

/// `kCLErrorLocationUnknown` — transient; CoreLocation keeps trying.
const CL_ERROR_LOCATION_UNKNOWN: isize = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct Coordinate {
	latitude: f64,
	longitude: f64,
}

unsafe impl Encode for Coordinate {
	const ENCODING: Encoding = Encoding::Struct("CLLocationCoordinate2D", &[f64::ENCODING, f64::ENCODING]);
}

#[derive(Default)]
struct State {
	/// Process pools given the provider, kept alive so a new pool can never
	/// reuse an address we already handled.
	pools: Vec<Retained<AnyObject>>,
	/// Geolocation managers with at least one page watching.
	watching: Vec<usize>,
	location: Option<Retained<AnyObject>>,
	delegate: Option<Retained<LocationDelegate>>,
}

thread_local! {
	static STATE: RefCell<State> = RefCell::new(State::default());
}

define_class!(
	// SAFETY: NSObject has no subclassing requirements; no Drop impl.
	#[unsafe(super(NSObject))]
	#[thread_kind = MainThreadOnly]
	#[name = "OctowebLocationDelegate"]
	struct LocationDelegate;

	impl LocationDelegate {
		#[unsafe(method(locationManager:didUpdateLocations:))]
		fn did_update(&self, _manager: &AnyObject, locations: &AnyObject) {
			unsafe {
				let location: *mut AnyObject = msg_send![locations, lastObject];
				if location.is_null() {
					return;
				}
				let coordinate: Coordinate = msg_send![location, coordinate];
				let accuracy: f64 = msg_send![location, horizontalAccuracy];
				let date: *mut AnyObject = msg_send![location, timestamp];
				let timestamp: f64 = if date.is_null() {
					0.0
				} else {
					msg_send![date, timeIntervalSince1970]
				};
				for manager in watching() {
					let position =
						WKGeolocationPositionCreate(timestamp, coordinate.latitude, coordinate.longitude, accuracy);
					WKGeolocationManagerProviderDidChangePosition(manager as WKRef, position);
					WKRelease(position);
				}
			}
		}

		#[unsafe(method(locationManager:didFailWithError:))]
		fn did_fail(&self, _manager: &AnyObject, error: &AnyObject) {
			let code: isize = unsafe { msg_send![error, code] };
			tracing::debug!(code, "CoreLocation failed");
			if code != CL_ERROR_LOCATION_UNKNOWN {
				fail_watchers();
			}
		}

		#[unsafe(method(locationManagerDidChangeAuthorization:))]
		fn did_change_authorization(&self, _manager: &AnyObject) {
			let denied = system_denied();
			tracing::debug!(denied, "Location Services authorization changed");
			if denied {
				fail_watchers();
			}
		}
	}
);

/// Give the process pool of `wv_ptr` our location provider (once per pool).
pub fn install(wv_ptr: usize) {
	unsafe {
		let wv = wv_ptr as *mut AnyObject;
		let config: *mut AnyObject = msg_send![wv, configuration];
		if config.is_null() {
			return;
		}
		let pool: *mut AnyObject = msg_send![config, processPool];
		let Some(pool) = Retained::retain(pool) else {
			return;
		};
		let known = STATE.with_borrow(|s| s.pools.iter().any(|p| Retained::as_ptr(p) == Retained::as_ptr(&pool)));
		if known {
			return;
		}
		let manager = WKContextGetGeolocationManager(Retained::as_ptr(&pool) as WKRef);
		if manager.is_null() {
			tracing::warn!("process pool has no geolocation manager — pages get no location");
			return;
		}
		WKGeolocationManagerSetProvider(manager, &PROVIDER.base);
		tracing::debug!("geolocation provider installed");
		STATE.with_borrow_mut(|s| s.pools.push(pool));
	}
}

/// Location Services is off, or macOS denied Octoweb access to it.
pub fn system_denied() -> bool {
	unsafe {
		let enabled: bool = msg_send![class!(CLLocationManager), locationServicesEnabled];
		if !enabled {
			return true;
		}
		let status = STATE.with_borrow(|s| s.location.clone()).map(|m| authorization(&m));
		let status = status.unwrap_or_else(|| msg_send![class!(CLLocationManager), authorizationStatus]);
		status == AUTH_DENIED || status == AUTH_RESTRICTED
	}
}

unsafe fn authorization(manager: &AnyObject) -> i32 {
	let responds: bool = msg_send![manager, respondsToSelector: sel!(authorizationStatus)];
	if responds {
		msg_send![manager, authorizationStatus]
	} else {
		msg_send![class!(CLLocationManager), authorizationStatus]
	}
}

fn watching() -> Vec<usize> {
	STATE.with_borrow(|s| s.watching.clone())
}

fn fail_watchers() {
	for manager in watching() {
		unsafe { WKGeolocationManagerProviderDidFailToDeterminePosition(manager as WKRef) };
	}
}

/// The shared `CLLocationManager`, created on first use.
fn location_manager() -> Option<Retained<AnyObject>> {
	if let Some(m) = STATE.with_borrow(|s| s.location.clone()) {
		return Some(m);
	}
	let mtm = MainThreadMarker::new()?;
	let delegate: Retained<LocationDelegate> = unsafe { msg_send![LocationDelegate::alloc(mtm), init] };
	let manager: Retained<AnyObject> = unsafe { msg_send![class!(CLLocationManager), new] };
	unsafe {
		let _: () = msg_send![&*manager, setDelegate: &*delegate];
		let _: () = msg_send![&*manager, setDesiredAccuracy: kCLLocationAccuracyHundredMeters];
	}
	STATE.with_borrow_mut(|s| {
		s.location = Some(manager.clone());
		s.delegate = Some(delegate);
	});
	Some(manager)
}

extern "C" fn start_updating(manager: WKRef, _client_info: *const c_void) {
	STATE.with_borrow_mut(|s| {
		if !s.watching.contains(&(manager as usize)) {
			s.watching.push(manager as usize);
		}
	});
	let Some(location) = location_manager() else {
		return;
	};
	if system_denied() {
		fail_watchers();
		return;
	}
	unsafe {
		if authorization(&location) == AUTH_NOT_DETERMINED {
			let _: () = msg_send![&*location, requestWhenInUseAuthorization];
		}
		let _: () = msg_send![&*location, startUpdatingLocation];
	}
	tracing::debug!("location updates started");
}

extern "C" fn stop_updating(manager: WKRef, _client_info: *const c_void) {
	let idle = STATE.with_borrow_mut(|s| {
		s.watching.retain(|m| *m != manager as usize);
		s.watching.is_empty()
	});
	if idle {
		if let Some(location) = STATE.with_borrow(|s| s.location.clone()) {
			unsafe {
				let _: () = msg_send![&*location, stopUpdatingLocation];
			}
			tracing::debug!("location updates stopped");
		}
	}
}

extern "C" fn set_enable_high_accuracy(_manager: WKRef, enabled: bool, _client_info: *const c_void) {
	if let Some(location) = location_manager() {
		let accuracy = unsafe {
			if enabled {
				kCLLocationAccuracyBest
			} else {
				kCLLocationAccuracyHundredMeters
			}
		};
		unsafe {
			let _: () = msg_send![&*location, setDesiredAccuracy: accuracy];
		}
	}
}
