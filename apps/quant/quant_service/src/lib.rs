//! The Quant desktop app's native side.
//!
//! The daemon does the capturing; this is its remote control. A worker
//! thread holds the app's own signed client (profile `desktop`, kind
//! `device`, scopes `audio:write audio:read audio:raw`) and, once the owner
//! unlocks it, an owner session kept only in memory. The menu-bar item and
//! the window's guest both go through it; neither touches the daemon.
//!
//! The macOS host starts the resident core (the worker and the menu-bar
//! item) at launch, and creates the window's guest only when the window
//! first opens. The guest's leptoskit service, from [`from_app_dirs`],
//! attaches to that core; on a host without a menu bar it starts the core.

mod daemon;
mod desktop;
mod playback;
mod resident;
mod service;
mod words;

use std::path::Path;

use leptoskit_bridge::Error;
use leptoskit_bridge_ffi::app::ServiceInstaller;

/// The service factory named in this crate's leptoskit metadata. It only
/// reads its configuration; the worker and the menu-bar item start when the
/// host activates the installer.
pub fn from_app_dirs(_data: &Path, cache: &Path) -> Result<ServiceInstaller, Error> {
	let config = daemon::Config::from_env(cache.to_path_buf());
	Ok(Box::new(move |host| service::start(host, config)))
}
