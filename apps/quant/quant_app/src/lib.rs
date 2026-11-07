//! The Quant desktop window, as a leptoskit guest.
//!
//! It shows what the app's native service reports and sends it commands.
//! It never talks to the daemon and never sees a key or a session token;
//! the passphrase passes through it once, on its way to the service.

pub mod bridge;
pub mod ui;

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

	use leptoskit::prelude::RwSignal;
	use leptoskit::prelude::Set;

	use super::State;
	use super::bridge;
	use super::follow;
	use super::ui;

	/// Mounts the window and starts following the service.
	#[unsafe(no_mangle)]
	pub extern "C" fn leptoskit_main() {
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
		leptoskit::guest::spawn(follow(state.clone()));
		leptoskit::guest::mount(move || ui::screen(&state));
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
