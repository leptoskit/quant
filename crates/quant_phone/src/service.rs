//! The [`AppService`] the host polls on the guest's thread while the screen
//! runs. It routes the screen's calls to the worker and answers them as
//! results arrive.

use std::collections::HashMap;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::thread::JoinHandle;

use leptoskit_bridge::Bytes;
use leptoskit_bridge::CallId;
use leptoskit_bridge::Error;
use leptoskit_bridge::MethodId;
use leptoskit_bridge::PluginId;
use leptoskit_bridge::Request as Call;
use leptoskit_bridge_ffi::app::AppService;
use leptoskit_bridge_ffi::app::ServiceClose;
use leptoskit_bridge_ffi::app::ServiceWakeReason;
use leptoskit_bridge_ffi::app::ServiceWork;
use leptoskit_native::HeadlessBackend;
use leptoskit_wasm::WasmHost;
use quant_model::phone;
use quant_model::phone::Command;
use quant_model::phone::Reading;
use quant_model::phone::Reply;
use quant_model::phone::Snapshot;
use tokio::sync::mpsc::UnboundedSender;

use crate::worker;
use crate::worker::Config;
use crate::worker::Job;
use crate::worker::Request;
use crate::worker::Shared;

/// Register the screen's capability and start the worker.
pub fn start(
	host: &mut WasmHost<HeadlessBackend>,
	config: Config,
) -> Result<Box<dyn AppService>, Error> {
	let methods = [
		MethodId(phone::WATCH),
		MethodId(phone::COMMAND),
		MethodId(phone::READING),
	];
	let bridge = host.plugins();
	let plugin = bridge.register(phone::SERVICE, methods)?;
	for method in methods {
		bridge.grant(plugin, method, true)?;
	}
	let shared = Arc::new(Shared::default());
	let (jobs, thread) = worker::spawn(config, Arc::clone(&shared)).map_err(|_| Error::Native)?;
	Ok(Box::new(Service {
		shared,
		jobs,
		thread,
		plugin,
		watchers: Vec::new(),
		commands: HashMap::new(),
		next_ticket: 0,
		snapshot: Snapshot::default(),
		woken: false,
	}))
}

struct Service {
	shared: Arc<Shared>,
	jobs: UnboundedSender<Request>,
	thread: JoinHandle<()>,
	plugin: PluginId,
	/// Watch calls, with the version each has seen.
	watchers: Vec<(CallId, u64)>,
	/// Command calls waiting for the worker, by ticket.
	commands: HashMap<u64, CallId>,
	next_ticket: u64,
	snapshot: Snapshot,
	/// Whether the platform's background wake caused this poll.
	woken: bool,
}

impl Service {
	fn send(&self, job: Job, ticket: Option<u64>) {
		let _ = self.jobs.send(Request { job, ticket });
	}

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
			phone::WATCH => {
				let seen = call
					.payload
					.get(..8)
					.and_then(|b| b.try_into().ok())
					.map_or(0, u64::from_le_bytes);
				self.watchers.push((call.id, seen));
				Ok(false)
			}
			phone::COMMAND => {
				let Ok(command) = serde_json::from_slice::<Command>(&call.payload) else {
					host.plugins()
						.complete(call.id, Err(Error::InvalidArgument))?;
					return Ok(true);
				};
				self.next_ticket += 1;
				self.commands.insert(self.next_ticket, call.id);
				self.send(Job::Command(command), Some(self.next_ticket));
				Ok(false)
			}
			phone::READING => {
				// The frame is checked here so a malformed one fails its
				// call; the plugin replies inside it are decoded by the
				// worker, off the guest's thread.
				if Reading::decode(&call.payload).is_none() {
					host.plugins()
						.complete(call.id, Err(Error::InvalidArgument))?;
					return Ok(true);
				}
				self.send(Job::Reading(call.payload.to_vec()), None);
				reply(host, call.id, &Reply::ok())?;
				Ok(true)
			}
			_ => {
				host.plugins()
					.complete(call.id, Err(Error::InvalidArgument))?;
				Ok(true)
			}
		}
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
		self.shared.set_waker(context.waker());
		if std::mem::take(&mut self.woken) {
			// The platform's background wake: deliver what is waiting, even
			// when the screen's own timer is not running.
			self.send(Job::Wake, None);
		}
		let mut work = ServiceWork::default();
		let mut answered = self.admit(host, &mut work)?;
		for (ticket, answer) in self.shared.take_replies() {
			if let Some(call) = self.commands.remove(&ticket) {
				reply(host, call, &answer)?;
				answered = true;
			}
		}
		if let Some(snapshot) = self.shared.snapshot_since(self.snapshot.version) {
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

	fn set_wake_reason(&mut self, reason: ServiceWakeReason) {
		self.woken |= reason == ServiceWakeReason::Background;
	}

	fn close(self: Box<Self>) -> ServiceClose {
		let Self {
			shared,
			jobs,
			thread,
			..
		} = *self;
		// The worker finishes the job it is on, which a delivery's deadline
		// bounds, and stops when it finds the channel closed.
		drop(jobs);
		let mut thread = Some(thread);
		Box::pin(std::future::poll_fn(move |context| {
			if !shared.stopped(context.waker()) {
				return Poll::Pending;
			}
			if let Some(thread) = thread.take() {
				let _ = thread.join();
			}
			Poll::Ready(Ok(()))
		}))
	}
}
