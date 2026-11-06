//! Cheap questions about the session: who has focus, is the screen locked,
//! how long since the last input.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSRunningApplication;
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::CFArray;
use objc2_core_foundation::CFDictionary;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGDisplayIsAsleep;
use objc2_core_graphics::CGEventSource;
use objc2_core_graphics::CGEventSourceStateID;
use objc2_core_graphics::CGEventType;
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_graphics::CGWindowListCopyWindowInfo;
use objc2_core_graphics::CGWindowListOption;
use objc2_core_graphics::kCGWindowAlpha;
use objc2_core_graphics::kCGWindowLayer;
use objc2_core_graphics::kCGWindowName;
use objc2_core_graphics::kCGWindowOwnerPID;
use objc2_foundation::NSArray;
use objc2_foundation::NSDictionary;
use objc2_foundation::NSNumber;
use objc2_foundation::NSString;

use crate::focus::Focus;
use crate::focus::Presence;

unsafe extern "C" {
	/// Undocumented in the SDK headers since 10.x but still exported: the
	/// current login session's attributes, including whether it is locked.
	fn CGSessionCopyCurrentDictionary() -> Option<NonNull<CFDictionary>>;
	fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// `QOS_CLASS_UTILITY`: long-running work the user is not waiting for.
const QOS_CLASS_UTILITY: u32 = 0x11;

/// Run the calling thread at utility quality of service, so the scheduler
/// prefers efficiency cores and capture never competes with the user's work.
pub(crate) fn lower_priority() {
	// SAFETY: affects only the calling thread; failure leaves it unchanged.
	let result = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) };
	if result != 0 {
		tracing::debug!(result, "could not lower capture thread priority");
	}
}

/// Seconds since any keyboard, mouse or trackpad input in this session.
pub(crate) fn idle_seconds() -> f64 {
	// `kCGAnyInputEventType` is `~0`.
	CGEventSource::seconds_since_last_event_type(
		CGEventSourceStateID::CombinedSessionState,
		CGEventType(u32::MAX),
	)
}

/// The session's presence, counting more than `idle_after` seconds without
/// input as idle. Also returns the idle time itself.
pub(crate) fn presence(idle_after: f64) -> (Presence, f64) {
	let idle = idle_seconds();
	let state = Presence {
		locked: screen_locked(),
		display_asleep: display_asleep(),
		idle: idle >= idle_after,
	};
	(state, idle)
}

/// Whether the screen is locked or this session is not on the console (fast
/// user switching).
pub(crate) fn screen_locked() -> bool {
	// SAFETY: the function takes no arguments and returns a +1 dictionary or
	// null.
	let Some(raw) = (unsafe { CGSessionCopyCurrentDictionary() }) else {
		// No window server session at all: nobody can be looking.
		return true;
	};
	// SAFETY: the pointer is a valid, owned CFDictionary.
	let session = unsafe { CFRetained::from_raw(raw) };
	let session = bridge_dictionary(&session);
	let flag = |key: &str| {
		session
			.objectForKey(&NSString::from_str(key))
			.and_then(|value| value.downcast_ref::<NSNumber>().map(NSNumber::boolValue))
	};
	flag("CGSSessionScreenIsLocked").unwrap_or(false)
		|| !flag("kCGSSessionOnConsoleKey").unwrap_or(true)
}

/// Whether the main display is asleep.
pub(crate) fn display_asleep() -> bool {
	CGDisplayIsAsleep(CGMainDisplayID())
}

/// Whether this process may read other apps' window contents and titles.
/// Never prompts.
pub(crate) fn screen_capture_allowed() -> bool {
	CGPreflightScreenCaptureAccess()
}

/// The frontmost app and, when readable, its front window's title.
///
/// The app is read from the window server's front-to-back window order
/// rather than `NSWorkspace.frontmostApplication`: `AppKit` only refreshes
/// that property while the main run loop runs, which a daemon whose main
/// thread belongs to an async runtime never does. `NSWorkspace` is the
/// fallback when no normal window is on screen.
pub(crate) fn frontmost() -> Option<Focus> {
	let (pid, title) = match front_window() {
		Some(front) => front,
		None => {
			(
				NSWorkspace::sharedWorkspace()
					.frontmostApplication()?
					.processIdentifier(),
				None,
			)
		}
	};
	let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid);
	let bundle_id = app
		.as_ref()
		.and_then(|app| app.bundleIdentifier())
		.map(|id| id.to_string());
	let name = app
		.as_ref()
		.and_then(|app| app.localizedName())
		.map(|name| name.to_string())
		.or_else(|| bundle_id.clone())
		.unwrap_or_else(|| format!("pid {pid}"));
	Some(Focus {
		app: name,
		bundle_id,
		title,
	})
}

/// The owner of the frontmost visible normal (layer 0) window, and the title
/// of that app's frontmost titled window. Titles of other apps' windows are
/// only visible with Screen Recording permission; without it the title is
/// `None`.
fn front_window() -> Option<(i32, Option<String>)> {
	let windows = CGWindowListCopyWindowInfo(
		CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
		0,
	)?;
	let windows = bridge_array(&windows);
	// SAFETY: the CoreGraphics keys are immutable CFStrings, which are
	// toll-free bridged to NSString.
	let (owner_key, layer_key, name_key, alpha_key) = unsafe {
		(
			cf_string_key(kCGWindowOwnerPID),
			cf_string_key(kCGWindowLayer),
			cf_string_key(kCGWindowName),
			cf_string_key(kCGWindowAlpha),
		)
	};
	let number = |window: &NSDictionary<NSString, AnyObject>, key: &NSString| {
		window
			.objectForKey(key)
			.and_then(|v| v.downcast_ref::<NSNumber>().map(NSNumber::doubleValue))
	};
	let mut owner = None;
	for window in windows {
		if number(&window, layer_key) != Some(0.0) || number(&window, alpha_key) == Some(0.0) {
			continue;
		}
		#[allow(clippy::cast_possible_truncation)]
		let Some(pid) = number(&window, owner_key).map(|pid| pid as i32) else {
			continue;
		};
		if *owner.get_or_insert(pid) != pid {
			continue;
		}
		let name = window
			.objectForKey(name_key)
			.and_then(|v| v.downcast_ref::<NSString>().map(ToString::to_string))
			.filter(|name| !name.trim().is_empty());
		if name.is_some() {
			return Some((pid, name));
		}
	}
	owner.map(|pid| (pid, None))
}

/// View a CoreFoundation dictionary as its toll-free bridged Foundation
/// counterpart.
fn bridge_dictionary(dictionary: &CFDictionary) -> &NSDictionary<NSString, AnyObject> {
	// SAFETY: CFDictionary and NSDictionary are toll-free bridged; keys of the
	// dictionaries read here are strings and values are objects.
	unsafe { &*std::ptr::from_ref(dictionary).cast::<NSDictionary<NSString, AnyObject>>() }
}

/// View a CoreFoundation array of dictionaries as an `NSArray`.
fn bridge_array(array: &CFArray) -> Retained<NSArray<NSDictionary<NSString, AnyObject>>> {
	let pointer = std::ptr::from_ref(array).cast::<NSArray<NSDictionary<NSString, AnyObject>>>();
	// SAFETY: CFArray and NSArray are toll-free bridged and the window list
	// holds dictionaries; retaining keeps it alive independently.
	unsafe { Retained::retain(pointer.cast_mut()) }.expect("non-null array")
}

/// A static `CFString` key as an `NSString`.
unsafe fn cf_string_key(key: &'static objc2_core_foundation::CFString) -> &'static NSString {
	// SAFETY: CFString is toll-free bridged to NSString; the caller passes a
	// static constant.
	unsafe { &*std::ptr::from_ref(key).cast::<NSString>() }
}
