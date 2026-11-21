//! Quant's screens, as a leptoskit guest.
//!
//! On a Mac the guest is the desktop window ([`ui`]). It shows what the
//! app's native service reports and sends it commands. It never talks to
//! the daemon and never sees a key or a session token; the passphrase passes
//! through it once, on its way to the service.
//!
//! On a phone (the `phone` feature, which `android` and `ios` turn on) the
//! guest is the collector's screen ([`phone`]). It pairs the phone with the
//! owner's vault, asks for the permissions the system grants only in
//! person, and drives the plugins that record location, app usage, battery
//! and thermal state.

pub mod bridge;
pub mod phone;
pub mod ui;
pub mod watch;

#[cfg(all(feature = "ui-preview", target_arch = "wasm32"))]
mod preview;

pub use ui::State;

/// Follow the service's snapshots for as long as the guest runs.
pub async fn follow(state: State) {
	let mut seen = 0;
	loop {
		match bridge::watch(seen).await {
			Ok(snapshot) => {
				seen = snapshot.version;
				leptoskit::prelude::Set::set(&state.snapshot, snapshot);
			}
			Err(error) => {
				leptoskit::prelude::Set::set(&state.problem, Some(error));
				return;
			}
		}
	}
}

/// The native Wasm runtime's entry points.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
#[allow(unsafe_code, reason = "the runtime calls these exported C names")]
mod entry {
	use std::sync::Arc;

	/// Mounts the screen and starts following the service.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_main() {
		#[cfg(feature = "phone")]
		phone();
		#[cfg(not(feature = "phone"))]
		desktop();
	}

	/// The phone or watch screen and its collector.
	#[cfg(feature = "phone")]
	fn phone() {
		use std::sync::OnceLock;

		use super::phone::State;
		use super::phone::collect;

		// The screen's actions need the state they act on, and the state is
		// built around the action handler, so the handler finds it here.
		let slot: Arc<OnceLock<State>> = Arc::new(OnceLock::new());
		let found = Arc::clone(&slot);
		let state = State::new(Arc::new(move |action| {
			if let Some(state) = found.get() {
				collect::act(state, action);
			}
		}));
		let _ = slot.set(state.clone());
		collect::start(&state);
		#[cfg(feature = "watch")]
		leptoskit::guest::mount(move || super::watch::WatchScreen(state.clone()));
		#[cfg(not(feature = "watch"))]
		leptoskit::guest::mount(move || super::phone::Screen(state.clone()));
	}

	/// The desktop window.
	#[cfg(not(feature = "phone"))]
	fn desktop() {
		use leptoskit::prelude::RwSignal;
		use leptoskit::prelude::Set;

		use super::State;
		use super::bridge;
		#[cfg(not(feature = "ui-preview"))]
		use super::follow;
		use super::ui;

		let problem: RwSignal<Option<String>> = RwSignal::new(None);
		let send: ui::Send = Arc::new(move |command| {
			leptoskit::guest::spawn(async move {
				let reply = bridge::send(&command).await;
				if !reply.ok {
					problem.set(reply.error);
				}
			});
		});
		let mut state = State::new(send);
		state.problem = problem;
		#[cfg(not(feature = "ui-preview"))]
		{
			leptoskit::guest::spawn(follow(state.clone()));
			leptoskit::guest::spawn(ui::touch_id::check(state.clone()));
		}
		#[cfg(feature = "ui-preview")]
		let state = super::preview::state();
		leptoskit::guest::mount(move || ui::Screen(state.clone()));
	}

	/// Forwards platform-neutral events to the tree's handlers.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_event(node: i64, kind: i32, ptr: i32, len: i32) -> i32 {
		leptoskit::guest::dispatch(node, kind, ptr, len)
	}

	/// Settles the values application state bound, after a turn.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_flush() {
		leptoskit::guest::flush();
	}

	/// Runs queued tasks, then settles what they wrote. The host calls this
	/// after a service reply.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_poll() {
		leptoskit::guest::poll();
	}

	/// Allocates the native runtime's event payload buffer.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_alloc(len: i32) -> i32 {
		leptoskit::guest::alloc(len)
	}

	/// Reports the native UI bridge ABI this guest implements.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_abi_version() -> i32 {
		leptoskit::guest::ABI_VERSION.cast_signed()
	}
}
