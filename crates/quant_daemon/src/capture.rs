//! Running this machine's capture sources inside the daemon.
//!
//! The settings live in the vault under `capture.config`, so only an owner
//! session can change what is recorded or what is excluded. Pausing is an
//! owner action too: an agent that could stop capture could hide what it
//! does.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use jiff::Timestamp;
use quant_api::Paused;
use quant_capture_macos::CaptureConfig;
use quant_capture_macos::CaptureHandle;
use quant_core::DeviceId;
use quant_core::StreamId;
use quant_store::StoreError;

use crate::State;

const CONFIG: &str = "capture.config";
const DEVICE: &str = "capture.device";

/// Whether capture runs, and the handle of the running sources.
#[derive(Debug, Default)]
pub struct Control {
	inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
	/// Capture was started for this daemon (not `--no-capture`).
	enabled: bool,
	handle: Option<CaptureHandle>,
	/// Paused by the owner.
	paused: Option<Paused>,
}

impl Control {
	fn lock(&self) -> MutexGuard<'_, Inner> {
		self.inner
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Whether the owner paused capture.
	pub fn paused(&self) -> Option<Paused> {
		self.lock().paused
	}
}

/// The stored settings, or the defaults.
pub fn config(state: &State) -> Result<CaptureConfig, StoreError> {
	Ok(state
		.vault
		.store
		.meta(CONFIG)?
		.and_then(|json| serde_json::from_str(&json).ok())
		.unwrap_or_default())
}

/// Store new settings, restarting the sources if they are running.
pub fn set_config(state: &Arc<State>, config: &CaptureConfig) -> Result<(), StoreError> {
	let json = serde_json::to_string(config).expect("capture settings serialize");
	state.vault.store.set_meta(CONFIG, &json)?;
	state.set_media_retention(
		&StreamId::new("audio.buffer").expect("built-in stream"),
		Some(config.rewind.hours),
	);
	let mut inner = state.capture_control.lock();
	if inner.handle.is_some() {
		restart(state, &mut inner)?;
	}
	Ok(())
}

/// This machine's device id, made once and kept in the vault.
pub fn device(state: &State) -> Result<DeviceId, StoreError> {
	let store = &state.vault.store;
	if let Some(id) = store.meta(DEVICE)?.and_then(|v| v.parse().ok()) {
		return Ok(id);
	}
	let id = DeviceId::new();
	store.set_meta(DEVICE, &id.to_string())?;
	Ok(id)
}

/// Start capturing on this machine.
pub fn start(state: &Arc<State>) -> Result<(), StoreError> {
	let mut inner = state.capture_control.lock();
	inner.enabled = true;
	if inner.paused.is_none() {
		restart(state, &mut inner)?;
	}
	Ok(())
}

/// Stop the sources and wait for them to flush. Called at shutdown; the
/// sources hold the state, so they must be stopped before it can drop.
pub fn stop(state: &State) {
	let handle = {
		let mut inner = state.capture_control.lock();
		inner.enabled = false;
		inner.handle.take()
	};
	if let Some(handle) = handle {
		handle.stop();
	}
}

/// Pause capture, for `until` or until resumed.
pub fn pause(state: &State, until: Option<Timestamp>) {
	let handle = {
		let mut inner = state.capture_control.lock();
		inner.paused = Some(until.map_or(Paused::UntilResumed, Paused::Until));
		inner.handle.take()
	};
	if let Some(handle) = handle {
		handle.stop();
	}
	for source in quant_capture_macos::SOURCES {
		quant_core::Sink::status(state, source, false, Some("paused".to_owned()));
	}
}

/// Resume after a pause.
pub fn resume(state: &Arc<State>) -> Result<(), StoreError> {
	let mut inner = state.capture_control.lock();
	inner.paused = None;
	if inner.enabled && inner.handle.is_none() {
		restart(state, &mut inner)?;
	}
	Ok(())
}

/// End a timed pause once it is over.
pub fn tick(state: &Arc<State>, now: Timestamp) -> Result<(), StoreError> {
	let over = matches!(state.capture_control.paused(), Some(Paused::Until(until)) if until <= now);
	if over { resume(state) } else { Ok(()) }
}

fn restart(state: &Arc<State>, inner: &mut Inner) -> Result<(), StoreError> {
	if let Some(handle) = inner.handle.take() {
		handle.stop();
	}
	let config = config(state)?;
	let device = device(state)?;
	state
		.vault
		.store
		.touch_device(device, &host_name(), std::env::consts::OS)?;
	let sink: Arc<dyn quant_core::Sink> = Arc::clone(state) as Arc<dyn quant_core::Sink>;
	inner.handle = Some(quant_capture_macos::spawn(config, sink, device));
	Ok(())
}

fn host_name() -> String {
	std::process::Command::new("hostname")
		.output()
		.ok()
		.and_then(|out| String::from_utf8(out.stdout).ok())
		.map(|name| name.trim().to_owned())
		.filter(|name| !name.is_empty())
		.unwrap_or_else(|| "this machine".to_owned())
}
