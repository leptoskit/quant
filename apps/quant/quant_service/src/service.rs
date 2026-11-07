//! The [`AppService`] the host polls on the UI thread while the window's
//! guest runs. It routes the guest's calls to the core's worker and answers
//! them as results arrive.

use std::collections::HashMap;
use std::task::Context;

use leptoskit_bridge::Bytes;
use leptoskit_bridge::CallId;
use leptoskit_bridge::Error;
use leptoskit_bridge::MethodId;
use leptoskit_bridge::PluginId;
use leptoskit_bridge::Request as Call;
use leptoskit_bridge_ffi::app::AppService;
use leptoskit_bridge_ffi::app::ServiceClose;
use leptoskit_bridge_ffi::app::ServiceWork;
use leptoskit_native::HeadlessBackend;
use leptoskit_wasm::WasmHost;
use quant_model::Command;
use quant_model::Reply;
use quant_model::Snapshot;
use zeroize::Zeroizing;

use crate::daemon::Config;
use crate::daemon::Job;
use crate::resident::Handle;

/// Register the window's capability and attach to the core, starting it if
/// the host has not.
pub fn start(
	host: &mut WasmHost<HeadlessBackend>,
	config: Config,
) -> Result<Box<dyn AppService>, Error> {
	let methods = [MethodId(quant_model::WATCH), MethodId(quant_model::COMMAND)];
	let bridge = host.plugins();
	let plugin = bridge.register(quant_model::SERVICE, methods)?;
	for method in methods {
		bridge.grant(plugin, method, true)?;
	}
	let core = crate::resident::start(config).map_err(|_| Error::Native)?;
	Ok(Box::new(Service {
		core,
		plugin,
		watchers: Vec::new(),
		commands: HashMap::new(),
		next_ticket: 0,
		snapshot: Snapshot::default(),
	}))
}

struct Service {
	core: Handle,
	plugin: PluginId,
	/// Watch calls, with the version each has seen.
	watchers: Vec<(CallId, u64)>,
	/// Command calls waiting for the worker, by ticket.
	commands: HashMap<u64, CallId>,
	next_ticket: u64,
	snapshot: Snapshot,
}

impl Service {
	/// Take this capability's calls and cancellations; pass on the rest.
	/// Returns whether any call was answered on the spot.
	fn admit(
		&mut self,
		host: &mut WasmHost<HeadlessBackend>,
		work: &mut ServiceWork,
	) -> Result<bool, Error> {
		while let Some(call) = host.plugins().next_cancelled() {
			let before = self.watchers.len() + self.commands.len();
			self.watchers.retain(|(id, _)| *id != call);
			self.commands.retain(|_, id| *id != call);
			if before == self.watchers.len() + self.commands.len() {
				work.cancelled.push(call);
			}
		}
		let mut answered = false;
		while let Some(call) = host.plugins().next_request() {
			if call.plugin == self.plugin {
				answered |= self.call(host, &call)?;
			} else {
				work.requests.push(call);
			}
		}
		Ok(answered)
	}

	/// Route one call. Returns whether it was answered on the spot.
	fn call(&mut self, host: &mut WasmHost<HeadlessBackend>, call: &Call) -> Result<bool, Error> {
		match call.method.0 {
			quant_model::WATCH => {
				let seen = call
					.payload
					.get(..8)
					.and_then(|b| b.try_into().ok())
					.map_or(0, u64::from_le_bytes);
				self.watchers.push((call.id, seen));
				Ok(false)
			}
			quant_model::COMMAND => {
				let Ok(command) = serde_json::from_slice::<Command>(&call.payload) else {
					host.plugins()
						.complete(call.id, Err(Error::InvalidArgument))?;
					return Ok(true);
				};
				if let Some(job) = self.command(command) {
					self.next_ticket += 1;
					self.commands.insert(self.next_ticket, call.id);
					self.core.send(job, Some(self.next_ticket));
					Ok(false)
				} else {
					reply(
						host,
						call.id,
						&Reply {
							ok: true,
							error: None,
						},
					)?;
					Ok(true)
				}
			}
			_ => {
				host.plugins()
					.complete(call.id, Err(Error::InvalidArgument))?;
				Ok(true)
			}
		}
	}

	/// The worker's job for a command, or `None` when it is done here.
	fn command(&self, command: Command) -> Option<Job> {
		Some(match command {
			Command::Refresh => Job::Refresh { moments: true },
			Command::Unlock { passphrase } => Job::Unlock(Zeroizing::new(passphrase)),
			Command::Lock => Job::Lock,
			Command::Pause { minutes } => Job::Pause(minutes),
			Command::Resume => Job::Resume,
			Command::Save { minutes } => Job::Save(minutes),
			Command::Approve {
				client,
				scopes,
				lookback_hours,
			} => {
				Job::Approve {
					client,
					scopes,
					lookback_hours,
				}
			}
			Command::Deny { client } => Job::Deny(client),
			Command::Revoke { grant } => Job::Revoke(grant),
			Command::Play { id } => Job::Play(id),
			Command::Rename { id, title } => Job::Rename { id, title },
			Command::OpenSettings { pane } => {
				crate::desktop::open_url(pane.url());
				return None;
			}
			Command::OpenAtLogin { enabled } => {
				let now = crate::desktop::set_opens_at_login(enabled);
				self.core.shared.update(|s| s.opens_at_login = now);
				return None;
			}
		})
	}

	/// Answer every watch that has not seen the current snapshot.
	fn answer_watchers(&mut self, host: &mut WasmHost<HeadlessBackend>) -> Result<bool, Error> {
		let version = self.snapshot.version;
		if !self.watchers.iter().any(|(_, seen)| *seen != version) {
			return Ok(false);
		}
		let bytes = Bytes::from(serde_json::to_vec(&self.snapshot).map_err(|_| Error::Native)?);
		let (ready, waiting): (Vec<_>, Vec<_>) = self
			.watchers
			.drain(..)
			.partition(|(_, seen)| *seen != version);
		self.watchers = waiting;
		for (call, _) in ready {
			host.plugins().complete(call, Ok(bytes.clone()))?;
		}
		Ok(true)
	}
}

fn reply(host: &mut WasmHost<HeadlessBackend>, call: CallId, reply: &Reply) -> Result<(), Error> {
	let bytes = serde_json::to_vec(reply).map_err(|_| Error::Native)?;
	host.plugins().complete(call, Ok(Bytes::from(bytes)))
}

impl AppService for Service {
	fn poll(
		&mut self,
		host: &mut WasmHost<HeadlessBackend>,
		context: &mut Context<'_>,
	) -> Result<ServiceWork, Error> {
		self.core.shared.set_waker(context.waker());
		// Hosts without a menu bar have no other owner to poll the core.
		crate::resident::poll();
		let mut work = ServiceWork::default();
		let mut answered = self.admit(host, &mut work)?;
		for (ticket, answer) in self.core.shared.take_replies() {
			if let Some(call) = self.commands.remove(&ticket) {
				reply(host, call, &answer)?;
				answered = true;
			}
		}
		if let Some(snapshot) = self.core.shared.snapshot_since(self.snapshot.version) {
			self.snapshot = snapshot;
		}
		answered |= self.answer_watchers(host)?;

		// Waking runs the guest's tasks, which ask again at once (the next
		// watch). Take those calls now: left in the queue, they would reach
		// the platform's router, which does not know this capability.
		while answered {
			host.wake_plugins().map_err(|_| Error::Native)?;
			answered = self.admit(host, &mut work)?;
			answered |= self.answer_watchers(host)?;
		}
		Ok(work)
	}

	fn close(self: Box<Self>) -> ServiceClose {
		// The core outlives the window; it stops when the app quits.
		self.core.shared.clear_waker();
		Box::pin(std::future::ready(Ok(())))
	}
}
