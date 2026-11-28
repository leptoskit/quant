//! The Quant phone app's native side.
//!
//! A phone records where it is, which apps were in front, and its battery
//! and thermal state, and delivers them to the owner's vault. Three pieces do
//! that, and this crate is the middle one:
//!
//! - The screen (`quant_app::phone`, a Wasm guest) is the only code that can
//!   reach the Leptoskit plugins. It asks for permissions, starts the plugins,
//!   and hands each plugin reply over unread.
//! - This service decodes those replies, queues them in the encrypted spool
//!   (`quant_collector`), and uploads them with the phone's own signing key
//!   over TLS pinned to the vault's certificate.
//! - The vault's daemon accepts them through the same signed ingest path as
//!   every other device.
//!
//! The phone pairs with write scopes only (`location:write`, `apps:write`,
//! `device:write`): it can add to the vault and read nothing back. The spool
//! key lives in the phone's keystore, and reaches the service once per launch
//! through the screen.
//!
//! The host activates the service through [`from_app_dirs`], and the
//! service's worker thread sleeps until the screen or the platform's
//! background wake gives it something to do.

mod companion;
mod saved;
mod service;
mod stats;
mod words;
mod worker;

use std::path::Path;

pub use companion::Companion;
use leptoskit_bridge::Error;
use leptoskit_bridge_ffi::app::ServiceInstaller;
pub use worker::Config;
pub use worker::Job;
pub use worker::Request;
pub use worker::Shared;
pub use worker::spawn;

/// The service factory named in this crate's leptoskit metadata. It only
/// reads its configuration; the worker starts when the host activates the
/// installer.
pub fn from_app_dirs(data: &Path, _cache: &Path) -> Result<ServiceInstaller, Error> {
	let config = Config::for_app(data.to_path_buf());
	Ok(Box::new(move |host| service::start(host, config)))
}
