//! Open Quant at login, through `leptoskit_plugin_autostart`.
//!
//! The plugin validates and writes a per-user login entry for this app's own
//! executable. The window only toggles it; it never chooses the command, and
//! nothing runs at launch unless the owner turns it on.

use leptoskit_plugin_autostart::Autostart;

/// The app's identity for the login entry: its bundle identifier.
const IDENTITY: &str = "dev.leptoskit.quant";

/// The configured login registration for this app, when it can be built.
///
/// `Autostart::new` validates the identity and that the current executable is
/// a real file; it registers nothing yet.
fn manager() -> Option<Autostart> {
	let executable = std::env::current_exe().ok()?;
	Autostart::new(IDENTITY.to_owned(), &executable, Vec::new()).ok()
}

/// Whether Quant opens at login, when the system can say. `None` on
/// platforms without login registration, or when the entry can't be read.
pub fn is_registered() -> Option<bool> {
	manager()?.is_registered().ok()
}

/// Register opening at login, or stop. Returns the state afterwards.
pub fn set_registered(enabled: bool) -> Option<bool> {
	let manager = manager()?;
	let result = if enabled {
		manager.enable()
	} else {
		manager.disable()
	};
	if let Err(error) = result {
		eprintln!("quant: open at login: {error}");
	}
	manager.is_registered().ok()
}
