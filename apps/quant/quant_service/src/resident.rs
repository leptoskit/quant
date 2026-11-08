//! The resident core: the daemon worker and the menu-bar item, owned by the
//! UI thread for the life of the app.
//!
//! It runs without the window. The macOS host starts it at launch through
//! [`quant_desktop_start`], so the menu bar works while the window's Wasm
//! guest does not exist yet; the guest is created the first time the window
//! opens and reaches the core through the [`AppService`] in `service.rs`.
//! Hosts without a menu bar start it from that service instead.
//!
//! [`AppService`]: leptoskit_bridge_ffi::app::AppService

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use tokio::sync::mpsc::UnboundedSender;

use crate::daemon;
use crate::daemon::Config;
use crate::daemon::Job;
use crate::daemon::Request;
use crate::daemon::Shared;
use crate::desktop::Action;
use crate::desktop::Desktop;

/// What the window's service needs from the core.
#[derive(Clone)]
pub struct Handle {
	/// State shared with the worker.
	pub shared: Arc<Shared>,
	jobs: UnboundedSender<Request>,
}

impl Handle {
	/// Queue work for the daemon.
	pub fn send(&self, job: Job, ticket: Option<u64>) {
		let _ = self.jobs.send(Request { job, ticket });
	}
}

struct Core {
	handle: Handle,
	worker: Option<JoinHandle<()>>,
	desktop: Desktop,
	seen: u64,
	was_fast: bool,
}

thread_local! {
	static CORE: RefCell<Option<Core>> = const { RefCell::new(None) };
}

/// Start the core on the UI thread, or return the running one.
pub fn start(config: Config) -> std::io::Result<Handle> {
	if let Some(handle) = handle() {
		return Ok(handle);
	}
	let shared = Arc::new(Shared::default());
	shared.update(|s| s.opens_at_login = crate::autostart::is_registered());
	shared.set_notify(Arc::new(crate::desktop::poll_soon));
	let desktop = Desktop::install(Arc::clone(&shared));
	let (jobs, worker) = daemon::spawn(config, Arc::clone(&shared))?;
	let handle = Handle { shared, jobs };
	CORE.with_borrow_mut(|core| {
		*core = Some(Core {
			handle: handle.clone(),
			worker: Some(worker),
			desktop,
			seen: 0,
			was_fast: false,
		});
	});
	Ok(handle)
}

/// The running core's handle.
pub fn handle() -> Option<Handle> {
	CORE.with_borrow(|core| core.as_ref().map(|core| core.handle.clone()))
}

/// Bring the menu bar up to date and act on what was chosen. Runs on the
/// UI thread whenever the worker, the status item, or the menu wakes it.
pub fn poll() {
	// A wake queued before `stop` may still arrive, and a core already
	// polling picks up whatever woke it.
	let _ = CORE.try_with(|cell| {
		if let Ok(mut core) = cell.try_borrow_mut()
			&& let Some(core) = core.as_mut()
		{
			core.poll();
		}
	});
}

/// Stop the worker, which ends the owner session, and remove the status
/// item. Blocks for at most a couple of seconds, while the session locks.
pub fn stop() {
	let core = CORE.with_borrow_mut(Option::take);
	if let Some(mut core) = core {
		core.desktop.close();
		let Core { handle, worker, .. } = core;
		drop(handle);
		if let Some(worker) = worker {
			let _ = worker.join();
		}
	}
}

impl Core {
	fn poll(&mut self) {
		for action in self.desktop.drain() {
			self.act(action);
		}
		let shared = &self.handle.shared;
		if let Some(snapshot) = shared.snapshot_since(self.seen) {
			self.seen = snapshot.version;
			self.desktop.show(&snapshot);
		}
		if shared.take_open_window() {
			crate::desktop::open_window();
		}
		let fast = self.desktop.is_open();
		shared.fast.store(fast, Ordering::Release);
		if fast && !self.was_fast {
			self.handle.send(Job::Refresh { moments: false }, None);
		}
		self.was_fast = fast;
	}

	fn act(&mut self, action: Action) {
		let job = match action {
			Action::Looked => Job::Refresh { moments: false },
			Action::Open => {
				crate::desktop::open_window();
				Job::Refresh { moments: true }
			}
			Action::Save(minutes) => Job::Save(minutes),
			Action::Pause(minutes) => Job::Pause(minutes),
			Action::Resume => Job::Resume,
			Action::Settings(pane) => return crate::desktop::open_url(pane.url()),
			Action::Quit => return crate::desktop::quit(),
		};
		self.handle.send(job, None);
	}
}

/// Start the menu-bar core. The macOS host calls this once, on the main
/// thread, after `NSApplication` has finished launching; `cache` is a
/// directory for clips while they play. Returns 0, or -1 if it could not
/// start.
///
/// # Safety
///
/// `cache` must be null or a NUL-terminated UTF-8 path valid for the call.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "the Swift host calls this C entry point")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn quant_desktop_start(cache: *const std::ffi::c_char) -> i32 {
	let cache = if cache.is_null() {
		std::env::temp_dir()
	} else {
		// SAFETY: the caller passes a NUL-terminated path valid for the call.
		let cache = unsafe { std::ffi::CStr::from_ptr(cache) };
		PathBuf::from(cache.to_string_lossy().into_owned())
	};
	match start(Config::from_env(cache)) {
		Ok(_) => 0,
		Err(error) => {
			eprintln!("quant: could not start: {error}");
			-1
		}
	}
}

/// Stop the menu-bar core before the app quits.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "the Swift host calls this C entry point")]
#[unsafe(no_mangle)]
pub extern "C" fn quant_desktop_stop() {
	stop();
}
