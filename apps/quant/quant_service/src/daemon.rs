//! The worker thread that talks to the daemon.
//!
//! It owns the signed [`Client`], the owner session, and a small Tokio
//! runtime. It waits on a channel of [`Job`]s with a timeout, so it sleeps
//! between polls rather than spinning: every 10 seconds while the menu or
//! the window is open, every 60 seconds otherwise. Each result lands in
//! [`Shared`], which wakes the UI thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Waker;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_access::Grant;
use quant_access::audit::Actor;
use quant_access::audit::AuditEntry;
use quant_access::audit::Outcome;
use quant_api::ApproveRequest;
use quant_api::AuditRequest;
use quant_api::CaptureState;
use quant_api::ClientKind;
use quant_api::ClientStatus;
use quant_api::ClientView;
use quant_api::PauseRequest;
use quant_api::Paused;
use quant_api::RecordsRequest;
use quant_api::RenameClip;
use quant_api::SaveAudio;
use quant_api::ServedRecord;
use quant_api::Status;
use quant_client::Client;
use quant_client::ClientError;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::MediaRef;
use quant_model::AuditRow;
use quant_model::GrantRow;
use quant_model::Link;
use quant_model::Moment;
use quant_model::Notice;
use quant_model::OwnerView;
use quant_model::PendingClient;
use quant_model::Reply;
use quant_model::Snapshot;
use quant_model::SourceRow;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::words;

/// How often to look while the menu or window is open.
pub const FAST: Duration = Duration::from_secs(10);
/// How often to look otherwise.
pub const SLOW: Duration = Duration::from_secs(60);
/// How long a notice stays in the menu.
const NOTICE_FOR: SignedDuration = SignedDuration::from_mins(2);
/// How far back saved moments are listed.
const MOMENTS_SINCE: SignedDuration = SignedDuration::from_hours(24 * 30);
/// The stream saved moments are records of.
const CLIP_STREAM: &str = "audio.clip";

/// Where the daemon is and who this app is.
#[derive(Clone, Debug)]
pub struct Config {
	/// The daemon's address.
	pub endpoint: Endpoint,
	/// The vault whose socket the endpoint is, to check its directory before
	/// sending a passphrase there.
	pub vault: Option<PathBuf>,
	/// This app's identity file.
	pub identity: PathBuf,
	/// Where played clips are written for the moment they play.
	pub cache: PathBuf,
	/// The time zone times are shown in.
	pub tz: TimeZone,
}

impl Config {
	/// `$QUANT_ADDR`, or the socket of `$QUANT_HOME` (or the default vault),
	/// and the `desktop` profile in `$QUANT_CONFIG` (or the default clients
	/// directory).
	pub fn from_env(cache: PathBuf) -> Self {
		let (endpoint, vault) = match std::env::var("QUANT_ADDR") {
			Ok(addr) if !addr.is_empty() => {
				let endpoint = Endpoint::parse(&addr).unwrap_or(Endpoint::Unix(addr.into()));
				(endpoint, None)
			}
			_ => {
				let root = quant_core::paths::default_vault_root();
				(
					Endpoint::Unix(quant_core::paths::socket_path(&root)),
					Some(root),
				)
			}
		};
		Self {
			endpoint,
			vault,
			identity: Identity::path(&Identity::default_dir(), "desktop"),
			cache,
			tz: TimeZone::system(),
		}
	}
}

/// Work for the daemon.
#[derive(Debug)]
pub enum Job {
	/// Look again; with `moments`, list saved moments too (an audited read).
	Refresh {
		/// Whether to list saved moments.
		moments: bool,
	},
	/// Open an owner session.
	Unlock(Zeroizing<String>),
	/// End it.
	Lock,
	/// Pause capture, for minutes or until resumed.
	Pause(Option<u32>),
	/// Resume capture.
	Resume,
	/// Keep the last minutes of the rolling audio buffer.
	Save(u32),
	/// Approve a pending client.
	Approve {
		/// Its client id.
		client: String,
		/// The scopes granted.
		scopes: Vec<String>,
		/// Its lookback in hours, or no limit.
		lookback_hours: Option<u32>,
	},
	/// Refuse a pending client.
	Deny(String),
	/// Revoke a grant.
	Revoke(String),
	/// Play a saved moment, or stop it if it is playing.
	Play(String),
	/// Retitle a saved moment.
	Rename {
		/// Its record id.
		id: String,
		/// The new title.
		title: String,
	},
}

impl Job {
	fn needs_owner(&self) -> Option<String> {
		Some(match self {
			Job::Pause(Some(minutes)) => {
				format!("Unlock to pause capture for {}", minutes_words(*minutes))
			}
			Job::Pause(None) => "Unlock to pause capture".to_owned(),
			Job::Resume => "Unlock to resume capture".to_owned(),
			Job::Approve { .. } | Job::Deny(_) | Job::Revoke(_) => {
				"Unlock to manage agents".to_owned()
			}
			_ => return None,
		})
	}
}

fn minutes_words(minutes: u32) -> String {
	match minutes {
		60 => "1 hour".to_owned(),
		m if m % 60 == 0 => format!("{} hours", m / 60),
		m => format!("{m} minutes"),
	}
}

/// A job, and the guest call waiting for its answer.
#[derive(Debug)]
pub struct Request {
	/// The work.
	pub job: Job,
	/// The guest call to answer, if any.
	pub ticket: Option<u64>,
}

/// What the worker and the UI thread share.
#[derive(Default)]
pub struct Shared {
	inner: Mutex<Inner>,
	/// Whether the menu or the window is open.
	pub fast: AtomicBool,
}

#[derive(Default)]
struct Inner {
	snapshot: Snapshot,
	replies: Vec<(u64, Reply)>,
	open_window: bool,
	/// The window's service, while the window's guest runs.
	waker: Option<Waker>,
	/// The menu-bar core.
	notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Shared {
	fn lock(&self) -> MutexGuard<'_, Inner> {
		self.inner
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Wake the window's service with this waker on the next change.
	pub fn set_waker(&self, waker: &Waker) {
		let mut inner = self.lock();
		if !inner.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
			inner.waker = Some(waker.clone());
		}
	}

	/// Stop waking the window's service.
	pub fn clear_waker(&self) {
		self.lock().waker = None;
	}

	/// Call `notify` on every change, to poll the menu-bar core.
	pub fn set_notify(&self, notify: Arc<dyn Fn() + Send + Sync>) {
		self.lock().notify = Some(notify);
	}

	/// Wake the UI thread's owners, from any thread. Each only enqueues.
	pub fn wake(&self) {
		let (waker, notify) = {
			let inner = self.lock();
			(inner.waker.clone(), inner.notify.clone())
		};
		if let Some(waker) = waker {
			waker.wake();
		}
		if let Some(notify) = notify {
			notify();
		}
	}

	/// The snapshot, when its version is not `seen`.
	pub fn snapshot_since(&self, seen: u64) -> Option<Snapshot> {
		let inner = self.lock();
		(inner.snapshot.version != seen).then(|| inner.snapshot.clone())
	}

	/// Answers for the window's calls.
	pub fn take_replies(&self) -> Vec<(u64, Reply)> {
		std::mem::take(&mut self.lock().replies)
	}

	/// Whether the window should open, to ask for the passphrase.
	pub fn take_open_window(&self) -> bool {
		std::mem::take(&mut self.lock().open_window)
	}

	/// Change the snapshot, bumping its version if anything changed.
	pub fn update(&self, change: impl FnOnce(&mut Snapshot)) {
		let changed = {
			let mut inner = self.lock();
			let mut next = inner.snapshot.clone();
			change(&mut next);
			next.version = inner.snapshot.version;
			let changed = next != inner.snapshot;
			if changed {
				next.version += 1;
				inner.snapshot = next;
			}
			changed
		};
		if changed {
			self.wake();
		}
	}

	fn finish(&self, ticket: Option<u64>, reply: Reply, open_window: bool) {
		{
			let mut inner = self.lock();
			if let Some(ticket) = ticket {
				inner.replies.push((ticket, reply));
			}
			inner.open_window |= open_window;
		}
		self.wake();
	}
}

/// Start the worker. It stops when every sender is dropped.
pub fn spawn(
	config: Config,
	shared: Arc<Shared>,
) -> std::io::Result<(mpsc::UnboundedSender<Request>, std::thread::JoinHandle<()>)> {
	let (sender, receiver) = mpsc::unbounded_channel();
	let thread = std::thread::Builder::new()
		.name("quant-daemon".into())
		.spawn(move || {
			let runtime = tokio::runtime::Builder::new_current_thread()
				.enable_io()
				.enable_time()
				.build();
			match runtime {
				Ok(runtime) => runtime.block_on(run(config, Arc::clone(&shared), receiver)),
				Err(error) => {
					shared.update(|s| {
						s.link = Link::Unreachable {
							reason: error.to_string(),
						};
					});
				}
			}
		})?;
	Ok((sender, thread))
}

async fn run(config: Config, shared: Arc<Shared>, mut jobs: mpsc::UnboundedReceiver<Request>) {
	let mut worker = Worker::new(config, Arc::clone(&shared));
	worker.clear_cache();
	worker.refresh(false).await;
	loop {
		let wait = if shared.fast.load(Ordering::Acquire) {
			FAST
		} else {
			SLOW
		};
		tokio::select! {
			request = jobs.recv() => {
				let Some(request) = request else { break };
				worker.handle(request).await;
			}
			() = tokio::time::sleep(wait) => worker.refresh(false).await,
		}
	}
	worker.stop_playing();
	worker.clear_cache();
	if let Some(owner) = worker.owner.take() {
		let _ = tokio::time::timeout(Duration::from_secs(2), owner.lock()).await;
	}
}

struct OwnerData {
	status: Option<Status>,
	clients: Vec<ClientView>,
	grants: Vec<Grant>,
	audit: Vec<AuditEntry>,
	chain_ok: Option<bool>,
}

struct Worker {
	config: Config,
	shared: Arc<Shared>,
	client: Option<Client>,
	link: Link,
	capture: Option<CaptureState>,
	owner: Option<OwnerSession>,
	owner_data: Option<OwnerData>,
	/// An owner action waiting for the passphrase.
	deferred: Option<Request>,
	unlock_reason: Option<String>,
	moments: Vec<Moment>,
	media: HashMap<String, MediaRef>,
	notice: Option<(Notice, Timestamp)>,
	playing: Option<(String, Child, PathBuf)>,
}

impl Worker {
	fn new(config: Config, shared: Arc<Shared>) -> Self {
		Self {
			config,
			shared,
			client: None,
			link: Link::Starting,
			capture: None,
			owner: None,
			owner_data: None,
			deferred: None,
			unlock_reason: None,
			moments: Vec::new(),
			media: HashMap::new(),
			notice: None,
			playing: None,
		}
	}

	async fn handle(&mut self, request: Request) {
		let Request { job, ticket } = request;
		if let Some(reason) = job.needs_owner()
			&& !self.unlocked()
		{
			self.unlock_reason = Some(reason.clone());
			self.deferred = Some(Request { job, ticket: None });
			self.publish();
			self.shared.finish(ticket, failed(reason), true);
			return;
		}
		let moments = matches!(
			job,
			Job::Save(_) | Job::Rename { .. } | Job::Refresh { moments: true }
		);
		let outcome = self.run(job).await;
		let reply = match outcome {
			Ok(Some(text)) => {
				self.say(text, false);
				ok()
			}
			Ok(None) => ok(),
			Err(text) => {
				self.say(text.clone(), true);
				failed(text)
			}
		};
		self.refresh(moments).await;
		self.shared.finish(ticket, reply, false);
		if self.unlocked()
			&& let Some(deferred) = self.deferred.take()
		{
			Box::pin(self.handle(deferred)).await;
		}
	}

	/// Run one job: a notice to show on success, or what went wrong.
	async fn run(&mut self, job: Job) -> Result<Option<String>, String> {
		match job {
			Job::Refresh { .. } => Ok(None),
			Job::Unlock(passphrase) => self.unlock(&passphrase).await.map(Some),
			Job::Lock => {
				self.deferred = None;
				self.unlock_reason = None;
				self.owner_data = None;
				if let Some(owner) = self.owner.take() {
					let _ = owner.lock().await;
				}
				Ok(Some("Locked".to_owned()))
			}
			Job::Pause(minutes) => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				let status = owner
					.pause_capture(&PauseRequest {
						paused: true,
						minutes,
					})
					.await
					.map_err(|e| problem(&e))?;
				Ok(Some(match status.paused {
					Some(Paused::Until(until)) => {
						format!("Paused until {}", words::clock(until, &self.config.tz))
					}
					_ => "Paused until you resume".to_owned(),
				}))
			}
			Job::Resume => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				owner
					.pause_capture(&PauseRequest {
						paused: false,
						minutes: None,
					})
					.await
					.map_err(|e| problem(&e))?;
				Ok(Some("Recording again".to_owned()))
			}
			Job::Save(minutes) => self.save(minutes).await.map(Some),
			Job::Approve {
				client,
				scopes,
				lookback_hours,
			} => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				let approved = owner
					.approve(&ApproveRequest {
						client,
						scopes: Some(scopes),
						label: None,
						lookback_hours,
						expires_in_hours: None,
						limits: None,
						unredacted: false,
					})
					.await
					.map_err(|e| problem(&e))?;
				Ok(Some(format!("Approved {}", approved.name)))
			}
			Job::Deny(client) => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				owner.deny(&client).await.map_err(|e| problem(&e))?;
				Ok(Some("Refused the pairing".to_owned()))
			}
			Job::Revoke(grant) => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				let revoked = owner.revoke(&grant).await.map_err(|e| problem(&e))?;
				Ok(Some(format!("Revoked {}", revoked.label)))
			}
			Job::Play(id) => self.play(&id).await,
			Job::Rename { id, title } => {
				let title = title.trim().to_owned();
				if title.is_empty() {
					return Err("A moment needs a title".to_owned());
				}
				let client = self.ready_client()?;
				let id = id.parse().map_err(|_| "No such moment".to_owned())?;
				client
					.rename_clip(&RenameClip { id, title })
					.await
					.map_err(|e| problem(&e))?;
				Ok(Some("Renamed".to_owned()))
			}
		}
	}

	fn unlocked(&self) -> bool {
		self.owner
			.as_ref()
			.is_some_and(|owner| owner.expires_at > Timestamp::now())
	}

	async fn unlock(&mut self, passphrase: &str) -> Result<String, String> {
		self.secure_socket()?;
		let session = OwnerSession::open(self.config.endpoint.clone(), passphrase)
			.await
			.map_err(|error| {
				match error.code() {
					Some("unauthenticated") => "Wrong passphrase".to_owned(),
					_ => problem(&error),
				}
			})?;
		let until = words::clock(session.expires_at, &self.config.tz);
		self.owner = Some(session);
		self.unlock_reason = None;
		Ok(format!("Unlocked until {until}"))
	}

	async fn save(&mut self, minutes: u32) -> Result<String, String> {
		let client = self.ready_client()?;
		let now = Timestamp::now();
		let (from, to) = words::save_window(minutes, now);
		let clip = client
			.save_audio(&SaveAudio {
				from,
				to,
				title: Some(words::moment_title(now, &self.config.tz)),
			})
			.await
			.map_err(|e| {
				match &e {
					// The daemon answers `not_found` when the buffer holds
					// nothing in the window; a bare 404 means no such route.
					ClientError::Api { error, .. } if error.code == "not_found" => {
						"Nothing recorded in that window".to_owned()
					}
					ClientError::Api { status: 404, .. } => {
						"This version of Quant can't save audio yet".to_owned()
					}
					_ => problem(&e),
				}
			})?;
		let (text, empty) = words::saved(minutes, &clip);
		if empty { Err(text) } else { Ok(text) }
	}

	async fn play(&mut self, id: &str) -> Result<Option<String>, String> {
		let same = self
			.playing
			.as_ref()
			.is_some_and(|(playing, ..)| playing == id);
		self.stop_playing();
		if same {
			return Ok(None);
		}
		let media = self
			.media
			.get(id)
			.cloned()
			.ok_or("This moment has no audio to play")?;
		let client = self.ready_client()?;
		let (_, bytes) = client.media(&media.hash).await.map_err(|e| problem(&e))?;
		let path = crate::playback::write(&self.config.cache, id, &bytes)
			.map_err(|e| format!("Couldn't prepare the clip: {e}"))?;
		let child = crate::playback::play(&path).map_err(|e| {
			let _ = std::fs::remove_file(&path);
			format!("Couldn't play the clip: {e}")
		})?;
		self.playing = Some((id.to_owned(), child, path));
		let title = self
			.moments
			.iter()
			.find(|m| m.id == id)
			.map_or("the moment", |m| m.title.as_str());
		Ok(Some(format!("Playing {title}")))
	}

	fn stop_playing(&mut self) {
		if let Some((_, mut child, path)) = self.playing.take() {
			let _ = child.kill();
			let _ = child.wait();
			let _ = std::fs::remove_file(path);
		}
	}

	fn clear_cache(&self) {
		crate::playback::clear(&self.config.cache);
	}

	fn say(&mut self, text: String, error: bool) {
		self.notice = Some((Notice { text, error }, Timestamp::now()));
	}

	fn secure_socket(&self) -> Result<(), String> {
		if let (Endpoint::Unix(_), Some(root)) = (&self.config.endpoint, &self.config.vault) {
			quant_core::paths::secure_socket_dir(root).map_err(|e| e.to_string())?;
		}
		Ok(())
	}

	fn ready_client(&self) -> Result<Client, String> {
		match (&self.link, &self.client) {
			(Link::Ready, Some(client)) => Ok(client.clone()),
			(Link::Pending { code }, _) => Err(format!("Approve this app first (code {code})")),
			(Link::Revoked, _) => Err("The owner revoked this app".to_owned()),
			_ => Err("Quant isn't running".to_owned()),
		}
	}

	fn client(&mut self) -> Result<&mut Client, String> {
		if self.client.is_none() {
			let identity = match Identity::load(&self.config.identity) {
				Ok(identity) => identity,
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
					Identity::generate(device_name())
				}
				Err(error) => {
					return Err(format!(
						"can't read {}: {error}",
						self.config.identity.display()
					));
				}
			};
			self.client = Some(Client::new(self.config.endpoint.clone(), identity));
		}
		Ok(self.client.as_mut().expect("just set"))
	}

	async fn refresh(&mut self, moments: bool) {
		if self
			.owner
			.as_ref()
			.is_some_and(|owner| owner.expires_at <= Timestamp::now())
		{
			self.owner = None;
			self.owner_data = None;
		}
		self.link = self.check().await;
		if self.link != Link::Ready {
			self.capture = None;
			self.moments.clear();
			self.media.clear();
		} else if moments {
			self.load_moments().await;
		}
		if self.owner.is_some() {
			self.load_owner().await;
		}
		if self
			.notice
			.as_ref()
			.is_some_and(|(_, at)| Timestamp::now().duration_since(*at) > NOTICE_FOR)
		{
			self.notice = None;
		}
		self.publish();
	}

	/// How this app stands with the daemon, pairing on first contact.
	async fn check(&mut self) -> Link {
		if let Err(reason) = self.secure_socket() {
			return Link::Unreachable { reason };
		}
		let identity_path = self.config.identity.clone();
		let client = match self.client() {
			Ok(client) => client,
			Err(reason) => return Link::Unreachable { reason },
		};
		if client.identity().client.is_some() {
			match client.capture_status().await {
				Ok(state) => {
					self.capture = Some(state);
					return Link::Ready;
				}
				Err(error) if error.code() == Some("unauthenticated") => {}
				Err(error) => {
					return Link::Unreachable {
						reason: error.to_string(),
					};
				}
			}
		}
		let scopes: Vec<String> = quant_model::DESKTOP_SCOPES
			.iter()
			.map(|s| (*s).to_owned())
			.collect();
		let before = client.identity().client;
		let pairing = match client.pair(ClientKind::Device, &scopes).await {
			Ok(pairing) => pairing,
			Err(error) => {
				return Link::Unreachable {
					reason: error.to_string(),
				};
			}
		};
		if before != Some(pairing.client)
			&& let Err(error) = client.identity().save(&identity_path)
		{
			return Link::Unreachable {
				reason: format!("can't save {}: {error}", identity_path.display()),
			};
		}
		match pairing.status {
			ClientStatus::Pending => {
				Link::Pending {
					code: pairing.pairing_code,
				}
			}
			ClientStatus::Revoked => Link::Revoked,
			ClientStatus::Active => {
				match client.capture_status().await {
					Ok(state) => {
						self.capture = Some(state);
						Link::Ready
					}
					// Approved without a live grant: expired or revoked since.
					Err(_) => Link::Revoked,
				}
			}
		}
	}

	async fn load_moments(&mut self) {
		let Some(client) = self.client.clone() else {
			return;
		};
		let now = Timestamp::now();
		let request = RecordsRequest {
			streams: vec![CLIP_STREAM.to_owned()],
			from: Some(now - MOMENTS_SINCE),
			limit: Some(50),
			..RecordsRequest::default()
		};
		let records = match client.records(&request).await {
			Ok(records) => records.records,
			// No clips yet, or a daemon without them: nothing to list.
			Err(_) => Vec::new(),
		};
		self.media.clear();
		self.moments = records
			.into_iter()
			.map(|record| {
				if let Some(media) = &record.media {
					self.media.insert(record.id.to_string(), media.clone());
				}
				moment(&record, now, &self.config.tz)
			})
			.collect();
	}

	async fn load_owner(&mut self) {
		let Some(owner) = self.owner.clone() else {
			return;
		};
		let status = owner.status().await;
		if let Err(error) = &status
			&& error.code() == Some("unauthenticated")
		{
			// The session lapsed or the daemon restarted.
			self.owner = None;
			self.owner_data = None;
			return;
		}
		let audit = owner
			.audit(&AuditRequest {
				limit: Some(50),
				newest: true,
				..AuditRequest::default()
			})
			.await
			.ok();
		self.owner_data = Some(OwnerData {
			status: status.ok(),
			clients: owner.clients().await.unwrap_or_default(),
			grants: owner.grants().await.unwrap_or_default(),
			chain_ok: audit.as_ref().and_then(|a| a.chain_ok),
			audit: audit.map(|a| a.entries).unwrap_or_default(),
		});
	}

	fn publish(&self) {
		let now = Timestamp::now();
		let tz = &self.config.tz;
		let headline = words::headline(&self.link, self.capture.as_ref(), tz);
		let status = self.owner_data.as_ref().and_then(|o| o.status.as_ref());
		let sources = sources(self.capture.as_ref(), status);
		let owner = self
			.owner
			.as_ref()
			.zip(self.owner_data.as_ref())
			.map(|(session, data)| owner_view(session, data, now, tz));
		let daemon = status
			.map(|s| s.version.clone())
			.or_else(|| self.capture.as_ref().map(|c| c.version.clone()));
		let paused = self.capture.as_ref().is_some_and(|c| c.paused.is_some());
		let notice = self.notice.as_ref().map(|(notice, _)| notice.clone());
		let unlock_reason = if self.unlocked() {
			None
		} else {
			self.unlock_reason.clone()
		};
		let link = self.link.clone();
		let moments = self.moments.clone();
		self.shared.update(move |snapshot| {
			snapshot.headline = headline;
			snapshot.link = link;
			snapshot.daemon = daemon;
			snapshot.paused = paused;
			snapshot.sources = sources;
			snapshot.moments = moments;
			snapshot.owner = owner;
			snapshot.unlock_reason = unlock_reason;
			snapshot.notice = notice;
		});
	}
}

/// A failed call, for people.
fn problem(error: &ClientError) -> String {
	match error {
		ClientError::Unreachable { .. } => "Quant isn't running".to_owned(),
		ClientError::Api { error, .. } if error.code == "locked" => error.message.clone(),
		ClientError::Api { error, .. } if error.code == "forbidden" => {
			format!("Not allowed: {}", error.message)
		}
		ClientError::Api { error, .. } if !error.message.is_empty() => error.message.clone(),
		error => error.to_string(),
	}
}

fn ok() -> Reply {
	Reply {
		ok: true,
		error: None,
	}
}

fn failed(error: String) -> Reply {
	Reply {
		ok: false,
		error: Some(error),
	}
}

fn device_name() -> String {
	let host = std::process::Command::new("/bin/hostname")
		.arg("-s")
		.output()
		.ok()
		.and_then(|out| String::from_utf8(out.stdout).ok())
		.map(|name| name.trim().to_owned())
		.filter(|name| !name.is_empty());
	match host {
		Some(host) => format!("Quant desktop on {host}"),
		None => "Quant desktop".to_owned(),
	}
}

/// Capture sources: the owner's detailed view when unlocked, otherwise the
/// running state any client may see.
fn sources(capture: Option<&CaptureState>, status: Option<&Status>) -> Vec<SourceRow> {
	let settings = |permission| {
		match permission {
			quant_api::Permission::ScreenRecording => quant_model::Settings::ScreenRecording,
			quant_api::Permission::Microphone => quant_model::Settings::Microphone,
		}
	};
	let mut rows: Vec<SourceRow> = if let Some(status) = status {
		status
			.capture
			.iter()
			.map(|source| {
				SourceRow {
					name: source.source.clone(),
					running: source.running,
					settings: source
						.detail
						.as_deref()
						.and_then(quant_api::Permission::from_detail)
						.map(settings),
					detail: source.detail.clone().or_else(|| {
						(source.captured > 0).then(|| format!("{} captured", source.captured))
					}),
				}
			})
			.collect()
	} else {
		capture
			.map(|c| &c.sources[..])
			.unwrap_or_default()
			.iter()
			.map(|source| {
				SourceRow {
					name: source.source.clone(),
					running: source.running,
					settings: source.needs.map(settings),
					detail: source
						.needs
						.map(|needs| format!("needs {} permission", settings(needs).name())),
				}
			})
			.collect()
	};
	rows.sort_by(|a, b| a.name.cmp(&b.name));
	rows
}

fn moment(record: &ServedRecord, now: Timestamp, tz: &TimeZone) -> Moment {
	let title = record
		.data
		.get("title")
		.and_then(|t| t.as_str())
		.filter(|t| !t.trim().is_empty())
		.map_or_else(|| "Untitled moment".to_owned(), ToOwned::to_owned);
	let length = record
		.end
		.map(|end| end.duration_since(record.start))
		.unwrap_or_default();
	Moment {
		id: record.id.to_string(),
		title,
		when: words::when(record.start, now, tz),
		duration: words::duration(length),
		snippet: record.text.as_deref().and_then(|t| words::snippet(t, 120)),
		playable: record.media.is_some(),
	}
}

fn owner_view(
	session: &OwnerSession,
	data: &OwnerData,
	now: Timestamp,
	tz: &TimeZone,
) -> OwnerView {
	let names: HashMap<String, &str> = data
		.clients
		.iter()
		.map(|c| (c.id.to_string(), c.name.as_str()))
		.collect();
	let pending = data
		.clients
		.iter()
		.filter(|c| c.status == ClientStatus::Pending)
		.map(|c| {
			PendingClient {
				id: c.id.to_string(),
				name: c.name.clone(),
				kind: serde_json::to_value(c.kind)
					.ok()
					.and_then(|v| v.as_str().map(ToOwned::to_owned))
					.unwrap_or_default(),
				code: c.pairing_code.clone(),
				requested: c.requested.clone(),
				asked: words::when(c.created_at, now, tz),
			}
		})
		.collect();
	let grants = data
		.grants
		.iter()
		.filter(|g| g.revoked_at.is_none() && g.expires_at.is_none_or(|at| at > now))
		.map(|g| {
			GrantRow {
				id: g.id.to_string(),
				label: g.label.clone(),
				scopes: g.scopes.to_strings(),
				lookback: words::lookback(g.lookback),
				expires: g.expires_at.map(|at| words::when(at, now, tz)),
			}
		})
		.collect();
	let audit = data
		.audit
		.iter()
		.rev()
		.map(|entry| {
			AuditRow {
				seq: entry.seq,
				at: words::when(entry.at, now, tz),
				actor: match &entry.actor {
					Actor::Owner => "owner".to_owned(),
					Actor::System => "system".to_owned(),
					Actor::Client { client, .. } => {
						names
							.get(&client.to_string())
							.map_or_else(|| client.to_string(), |name| (*name).to_owned())
					}
					Actor::Unknown { claimed } => {
						claimed.clone().unwrap_or_else(|| "unknown".to_owned())
					}
				},
				action: entry.action.clone(),
				target: entry.target.clone(),
				outcome: match &entry.outcome {
					Outcome::Allowed { records: 0, .. } => "allowed".to_owned(),
					Outcome::Allowed { records, .. } => format!("allowed · {records}"),
					Outcome::Denied { reason } => format!("denied: {reason}"),
					Outcome::Failed => "failed".to_owned(),
				},
			}
		})
		.collect();
	OwnerView {
		until: words::clock(session.expires_at, tz),
		pending,
		grants,
		audit,
		chain_ok: data.chain_ok,
	}
}
