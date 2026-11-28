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

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_access::Grant;
use quant_access::OwnerDevicePolicy;
use quant_access::audit::Actor;
use quant_access::audit::AuditEntry;
use quant_access::audit::Outcome;
use quant_api::ApproveRequest;
use quant_api::AuditRequest;
use quant_api::CaptureState;
use quant_api::ClientKind;
use quant_api::ClientStatus;
use quant_api::ClientView;
use quant_api::EnrolDeviceRequest;
use quant_api::Hello;
use quant_api::PauseRequest;
use quant_api::Paused;
use quant_api::QuestionListRequest;
use quant_api::QuestionView;
use quant_api::RecordsRequest;
use quant_api::RenameClip;
use quant_api::SaveAudio;
use quant_api::ServedRecord;
use quant_api::SessionSource;
use quant_api::Status;
use quant_client::Client;
use quant_client::ClientError;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::MediaRef;
use quant_core::OwnerDeviceId;
use quant_model::AuditRow;
use quant_model::GrantRow;
use quant_model::Link;
use quant_model::Moment;
use quant_model::Notice;
use quant_model::OwnerDeviceRow;
use quant_model::OwnerView;
use quant_model::PendingClient;
use quant_model::PhoneLink;
use quant_model::PhoneQr;
use quant_model::QuestionRow;
use quant_model::Reply;
use quant_model::ReplyData;
use quant_model::Snapshot;
use quant_model::SourceRow;
use quant_relay::link::Link as PairingLink;
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
	/// Where the Touch ID owner device id is kept (app-private, not a secret).
	pub touch_id: PathBuf,
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
		let identity = Identity::path(&Identity::default_dir(), "desktop");
		Self {
			endpoint,
			vault,
			touch_id: identity.with_file_name("desktop.touch-id.json"),
			identity,
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
		/// Sensitivity classes withheld from this grant.
		deny_classes: Vec<String>,
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
	/// Fetch a challenge for this Mac's Touch ID key to sign.
	TouchIdChallenge,
	/// Open an owner session with an enrolled device's signature.
	UnlockWithDevice {
		/// The enrolled device id.
		device: String,
		/// The challenge the signature answers, base64, as the daemon gave it.
		challenge: String,
		/// The ECDSA P-256/SHA-256 signature over the unlock message, DER.
		signature: Vec<u8>,
	},
	/// Enrol this Mac's Touch ID key as an owner device.
	EnrolTouchId {
		/// The key's uncompressed SEC1 public point (65 bytes).
		public_key: Vec<u8>,
	},
	/// Turn off Touch ID on this Mac: revoke the device and forget it.
	TouchIdOff,
	/// Revoke an enrolled owner device.
	RevokeDevice(String),
	/// Make the pairing link and QR code the window shows for a phone.
	PhoneLink,
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
			Job::EnrolTouchId { .. } => "Unlock with your passphrase to set up Touch ID".to_owned(),
			Job::TouchIdOff | Job::RevokeDevice(_) => "Unlock to manage Touch ID".to_owned(),
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
	devices: Vec<quant_api::OwnerDeviceView>,
	audit: Vec<AuditEntry>,
	chain_ok: Option<bool>,
	questions: Vec<QuestionView>,
}

/// What a job finished with: an optional notice, and optional reply data.
enum Done {
	/// Nothing to say, nothing to return.
	Quiet,
	/// A notice to show.
	Say(String),
	/// Reply data to hand back to the window.
	Data(ReplyData),
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
	/// This Mac's enrolled Touch ID owner device id, when set up.
	touch_id: Option<String>,
}

impl Worker {
	fn new(config: Config, shared: Arc<Shared>) -> Self {
		let touch_id = read_touch_id(&config.touch_id);
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
			touch_id,
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
			Ok(Done::Say(text)) => {
				self.say(text, false);
				ok()
			}
			Ok(Done::Quiet) => ok(),
			Ok(Done::Data(data)) => {
				Reply {
					ok: true,
					error: None,
					data: Some(data),
				}
			}
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

	/// Run one job: a notice to show on success, reply data to hand back, or
	/// what went wrong.
	async fn run(&mut self, job: Job) -> Result<Done, String> {
		match job {
			Job::Refresh { .. } => Ok(Done::Quiet),
			Job::Unlock(passphrase) => self.unlock(&passphrase).await.map(Done::Say),
			Job::Lock => {
				self.deferred = None;
				self.unlock_reason = None;
				self.owner_data = None;
				if let Some(owner) = self.owner.take() {
					let _ = owner.lock().await;
				}
				Ok(Done::Say("Locked".to_owned()))
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
				Ok(Done::Say(match status.paused {
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
				Ok(Done::Say("Recording again".to_owned()))
			}
			Job::Save(minutes) => self.save(minutes).await.map(Done::Say),
			Job::Approve {
				client,
				scopes,
				lookback_hours,
				deny_classes,
			} => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				let mut denied = Vec::with_capacity(deny_classes.len());
				for name in &deny_classes {
					let class = quant_core::Class::parse(name)
						.ok_or_else(|| format!("Unknown sensitivity class: {name}"))?;
					if !denied.contains(&class) {
						denied.push(class);
					}
				}
				let approved = owner
					.approve(&ApproveRequest {
						client,
						scopes: Some(scopes),
						label: None,
						lookback_hours,
						expires_in_hours: None,
						limits: None,
						unredacted: false,
						deny_classes: (!denied.is_empty()).then_some(denied),
					})
					.await
					.map_err(|e| problem(&e))?;
				Ok(Done::Say(format!("Approved {}", approved.name)))
			}
			Job::Deny(client) => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				owner.deny(&client).await.map_err(|e| problem(&e))?;
				Ok(Done::Say("Refused the pairing".to_owned()))
			}
			Job::Revoke(grant) => {
				let owner = self.owner.as_ref().ok_or("Unlock first")?;
				let revoked = owner.revoke(&grant).await.map_err(|e| problem(&e))?;
				Ok(Done::Say(format!("Revoked {}", revoked.label)))
			}
			Job::Play(id) => {
				self.play(&id)
					.await
					.map(|notice| notice.map_or(Done::Quiet, Done::Say))
			}
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
				Ok(Done::Say("Renamed".to_owned()))
			}
			Job::TouchIdChallenge => self.touch_id_challenge().await,
			Job::UnlockWithDevice {
				device,
				challenge,
				signature,
			} => {
				self.unlock_with_device(&device, &challenge, &signature)
					.await
			}
			Job::EnrolTouchId { public_key } => self.enrol_touch_id(&public_key).await,
			Job::TouchIdOff => self.touch_id_off().await,
			Job::RevokeDevice(device) => self.revoke_device(&device).await,
			Job::PhoneLink => self.phone_link().await,
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

	/// The challenge and message the window's Touch ID key signs.
	async fn touch_id_challenge(&mut self) -> Result<Done, String> {
		let client = self.client()?.clone();
		let hello = client.hello().await.map_err(|e| problem(&e))?;
		let challenge = client.owner_challenge().await.map_err(|e| problem(&e))?;
		let message = quant_api::owner_unlock_message(&hello.vault_id, &challenge.challenge);
		Ok(Done::Data(ReplyData::TouchIdChallenge {
			challenge: challenge.challenge,
			message,
		}))
	}

	/// Open an owner session with an enrolled device's signature.
	async fn unlock_with_device(
		&mut self,
		device: &str,
		challenge: &str,
		signature: &[u8],
	) -> Result<Done, String> {
		let device: OwnerDeviceId = device
			.parse()
			.map_err(|_| "No such Touch ID device".to_owned())?;
		self.secure_socket()?;
		let result = OwnerSession::open_with_device_signature(
			self.config.endpoint.clone(),
			device,
			challenge.to_owned(),
			URL_SAFE_NO_PAD.encode(signature),
		)
		.await;
		let session = match result {
			Ok(session) => session,
			Err(error) => {
				let text = match error.code() {
					// The daemon refused: the challenge lapsed while the
					// prompt was open, the device was revoked, or the
					// signature was wrong — it does not say which. Keep the
					// key; a revoked device is forgotten when the device list
					// next loads.
					Some("unauthenticated") => {
						"Touch ID didn't unlock Quant; try again or use your passphrase".to_owned()
					}
					_ => problem(&error),
				};
				return Err(text);
			}
		};
		let until = words::clock(session.expires_at, &self.config.tz);
		self.owner = Some(session);
		self.unlock_reason = None;
		Ok(Done::Say(format!("Unlocked until {until}")))
	}

	/// Enrol this Mac's Touch ID key. Only a passphrase session may, so a
	/// stolen device cannot gather more keys under the owner's name.
	async fn enrol_touch_id(&mut self, public_key: &[u8]) -> Result<Done, String> {
		let owner = self.owner.clone().ok_or("Unlock first")?;
		let passphrase_session = self
			.owner_data
			.as_ref()
			.and_then(|data| data.status.as_ref())
			.is_some_and(|status| status.session == SessionSource::Passphrase);
		if !passphrase_session {
			return Err("Set up Touch ID with your passphrase session".to_owned());
		}
		let enrolled = owner
			.owner_device_add(&EnrolDeviceRequest {
				label: touch_id_label(),
				public_key: URL_SAFE_NO_PAD.encode(public_key),
				policy: OwnerDevicePolicy::Biometry,
			})
			.await
			.map_err(|e| problem(&e))?;
		let id = enrolled.id.to_string();
		write_touch_id(&self.config.touch_id, Some(&id))?;
		self.touch_id = Some(id);
		Ok(Done::Say(format!("Touch ID is on for {}", enrolled.label)))
	}

	/// Turn off Touch ID on this Mac: revoke the device (best effort) and
	/// forget it here.
	async fn touch_id_off(&mut self) -> Result<Done, String> {
		if let (Some(owner), Some(device)) = (self.owner.clone(), self.touch_id.as_deref())
			&& let Ok(id) = device.parse::<OwnerDeviceId>()
		{
			let _ = owner.owner_device_revoke(id).await;
		}
		self.forget_touch_id();
		Ok(Done::Say("Touch ID is off".to_owned()))
	}

	/// Revoke an enrolled owner device, by id.
	async fn revoke_device(&mut self, device: &str) -> Result<Done, String> {
		let owner = self.owner.clone().ok_or("Unlock first")?;
		let id: OwnerDeviceId = device.parse().map_err(|_| "No such device".to_owned())?;
		let revoked = owner
			.owner_device_revoke(id)
			.await
			.map_err(|e| problem(&e))?;
		if self.touch_id.as_deref() == Some(device) {
			self.forget_touch_id();
		}
		Ok(Done::Say(format!("Revoked {}", revoked.label)))
	}

	/// Forget this Mac's Touch ID device, on disk and here.
	fn forget_touch_id(&mut self) {
		self.touch_id = None;
		let _ = write_touch_id(&self.config.touch_id, None);
	}

	/// The pairing link and locally drawn QR code for a phone.
	async fn phone_link(&mut self) -> Result<Done, String> {
		let client = self.client()?.clone();
		let hello = client
			.hello()
			.await
			.map_err(|error| phone_hello_problem(&error, &self.config.endpoint))?;
		let link = pairing_link(&hello, &self.config.endpoint)?;
		Ok(Done::Data(ReplyData::PhoneLink(link)))
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
		let (_, bytes) = client
			.media(&media.hash, None)
			.await
			.map_err(|e| problem(&e))?;
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
		let pairing = match client.pair(ClientKind::Device, &scopes, None).await {
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
		let records = match client.records(&request, None).await {
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
		let devices = owner.owner_devices().await.unwrap_or_default();
		// If this Mac's device was revoked on the daemon (from the CLI or
		// another device), forget it here so the window stops offering it.
		let touch_id = self.touch_id.clone();
		if let Some(device) = touch_id
			&& let Ok(id) = device.parse::<OwnerDeviceId>()
			&& !devices.iter().any(|d| d.id == id && d.revoked_at.is_none())
		{
			self.forget_touch_id();
		}
		let audit = owner
			.audit(&AuditRequest {
				limit: Some(50),
				newest: true,
				..AuditRequest::default()
			})
			.await
			.ok();
		let questions = owner
			.questions(&QuestionListRequest {
				limit: Some(30),
				client: None,
			})
			.await
			.unwrap_or_default();
		self.owner_data = Some(OwnerData {
			questions,
			status: status.ok(),
			clients: owner.clients().await.unwrap_or_default(),
			grants: owner.grants().await.unwrap_or_default(),
			devices,
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
		let touch_id = self.touch_id.clone();
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
			snapshot.touch_id.device_id = touch_id;
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

/// What to do when the daemon cannot answer the pairing-link request.
fn phone_hello_problem(error: &ClientError, endpoint: &Endpoint) -> String {
	let port = endpoint_port(endpoint).unwrap_or(quant_relay::link::DEFAULT_PORT);
	match error {
		ClientError::Unreachable { .. } => {
			format!(
				"Quant isn't running. Start it with `quant service install --tcp 0.0.0.0:{port}` \
				 (or `quant daemon --tcp 0.0.0.0:{port}`), then try again."
			)
		}
		error => {
			format!(
				"{} · Make sure Quant serves phones with `quant service install --tcp \
				 0.0.0.0:{port}`, then try again.",
				problem(error)
			)
		}
	}
}

/// What to do when the daemon has no TLS listener for phones.
fn local_only(port: u16) -> String {
	format!(
		"Quant is running, but it isn't listening for phones. Run `quant service install --tcp \
		 0.0.0.0:{port}` (or `quant daemon --tcp 0.0.0.0:{port}`), then try again."
	)
}

/// The pairing link for a hello answer, verified against the daemon's
/// advertised TLS bind address and the endpoint this app used to reach it.
fn pairing_link(hello: &Hello, endpoint: &Endpoint) -> Result<PhoneLink, String> {
	pairing_link_with(
		hello,
		endpoint,
		quant_relay::link::lan_address,
		quant_relay::link::lan_address_v6,
	)
}

fn pairing_link_with(
	hello: &Hello,
	endpoint: &Endpoint,
	lan_v4: impl FnOnce() -> Option<String>,
	lan_v6: impl FnOnce() -> Option<String>,
) -> Result<PhoneLink, String> {
	let fallback_port = endpoint_port(endpoint).unwrap_or(quant_relay::link::DEFAULT_PORT);
	let fingerprint = hello
		.tls_fingerprint
		.as_deref()
		.ok_or_else(|| local_only(fallback_port))?;
	let valid_relay = hello
		.relay
		.as_deref()
		.zip(hello.relay_rid.as_deref())
		.filter(|(_, rid)| quant_relay::key::valid_rid(rid));

	let direct_endpoint = reachable_tls_endpoint(endpoint);
	let link = match hello.tls_address.as_deref() {
		Some(advertised) => {
			if let Some((host, port)) = direct_endpoint {
				PairingLink::from_host_port(
					&host,
					port,
					fingerprint,
					&hello.vault_id,
					hello.relay.clone(),
					hello.relay_rid.clone(),
				)
			} else if let Some((adv_host, adv_port)) =
				quant_relay::link::split_host_port(advertised)
			{
				if let Ok(ip) = adv_host.parse::<std::net::IpAddr>() {
					if ip.is_loopback() {
						if let Some((relay, rid)) = valid_relay {
							PairingLink::relay_only(fingerprint, &hello.vault_id, relay, rid)
						} else {
							return Err(loopback_only(&adv_host, adv_port));
						}
					} else if ip.is_unspecified() {
						let lan = if ip.is_ipv6() {
							lan_v6().or_else(lan_v4)
						} else {
							lan_v4()
						};
						if let Some(host) = lan {
							PairingLink::from_host_port(
								&host,
								adv_port,
								fingerprint,
								&hello.vault_id,
								hello.relay.clone(),
								hello.relay_rid.clone(),
							)
						} else if let Some((relay, rid)) = valid_relay {
							PairingLink::relay_only(fingerprint, &hello.vault_id, relay, rid)
						} else {
							return Err(format!(
								"This Mac has no network address right now. Join the same network \
								 as your phone, then try again; `quant link --host <address> \
								 --port {adv_port}` can print a link for an address you choose."
							));
						}
					} else {
						PairingLink::from_host_port(
							&adv_host,
							adv_port,
							fingerprint,
							&hello.vault_id,
							hello.relay.clone(),
							hello.relay_rid.clone(),
						)
					}
				} else if quant_relay::link::is_reachable_host(&adv_host) {
					PairingLink::from_host_port(
						&adv_host,
						adv_port,
						fingerprint,
						&hello.vault_id,
						hello.relay.clone(),
						hello.relay_rid.clone(),
					)
				} else if let Some((relay, rid)) = valid_relay {
					PairingLink::relay_only(fingerprint, &hello.vault_id, relay, rid)
				} else {
					return Err(loopback_only(&adv_host, adv_port));
				}
			} else if let Some((relay, rid)) = valid_relay {
				PairingLink::relay_only(fingerprint, &hello.vault_id, relay, rid)
			} else {
				return Err(format!(
					"Quant reported an unusable network address ({advertised}). Check its `--tcp` \
					 setting or run `quant link --host <address> --port <port>`."
				));
			}
		}
		None => {
			if let Some((host, port)) = direct_endpoint {
				PairingLink::from_host_port(
					&host,
					port,
					fingerprint,
					&hello.vault_id,
					hello.relay.clone(),
					hello.relay_rid.clone(),
				)
			} else if let Some((relay, rid)) = valid_relay {
				PairingLink::relay_only(fingerprint, &hello.vault_id, relay, rid)
			} else {
				return Err(
					"Quant is listening over TLS, but this daemon didn't report its bind address \
					 or port, so the app can't verify a pairing link. Update and restart the \
					 daemon, or run `quant link --host <address> --port <port>` in a terminal."
						.to_owned(),
				);
			}
		}
	};

	let url = link.to_url();
	let qr = phone_qr(&url);
	Ok(PhoneLink { link: url, qr })
}

/// What to do when the daemon's TLS listener is bound only to loopback.
fn loopback_only(host: &str, port: u16) -> String {
	let bound = quant_relay::link::format_host_port(host, port);
	format!(
		"Quant's network listener is bound only to {bound} (loopback), so a phone cannot reach \
		 it. Restart it with `quant service install --tcp 0.0.0.0:{port}` (or `quant daemon --tcp \
		 0.0.0.0:{port}`), then try again."
	)
}

/// The QR code for a link, rendered via `qrcode`'s own renderer (with its
/// four-module quiet zone) into a single SVG path covering all dark modules.
fn phone_qr(link: &str) -> Option<PhoneQr> {
	let code = qrcode::QrCode::new(link.as_bytes()).ok()?;
	let qr = code.render::<PathPixel>().quiet_zone(true).build();
	(qr.side > 0 && !qr.path.is_empty()).then_some(qr)
}

#[derive(Clone, Copy)]
struct PathPixel;

impl qrcode::render::Pixel for PathPixel {
	type Canvas = PathCanvas;
	type Image = PhoneQr;

	fn default_unit_size() -> (u32, u32) {
		(1, 1)
	}

	fn default_color(_: qrcode::Color) -> Self {
		Self
	}
}

struct PathCanvas {
	side: u16,
	path: String,
	pending: Option<(u32, u32, u32, u32)>,
}

impl PathCanvas {
	fn flush(&mut self) {
		use std::fmt::Write;

		if let Some((left, top, width, height)) = self.pending.take() {
			let _ = write!(self.path, "M{left} {top}h{width}v{height}h-{width}Z");
		}
	}
}

impl qrcode::render::Canvas for PathCanvas {
	type Image = PhoneQr;
	type Pixel = PathPixel;

	fn new(width: u32, _height: u32, _: PathPixel, _: PathPixel) -> Self {
		Self {
			side: u16::try_from(width).unwrap_or(0),
			path: String::new(),
			pending: None,
		}
	}

	fn draw_dark_pixel(&mut self, x: u32, y: u32) {
		self.draw_dark_rect(x, y, 1, 1);
	}

	fn draw_dark_rect(&mut self, left: u32, top: u32, width: u32, height: u32) {
		if let Some((pl, pt, pw, ph)) = self.pending
			&& pt == top
			&& ph == height
			&& pl + pw == left
		{
			self.pending = Some((pl, pt, pw + width, ph));
			return;
		}
		self.flush();
		self.pending = Some((left, top, width, height));
	}

	fn into_image(mut self) -> PhoneQr {
		self.flush();
		PhoneQr {
			side: self.side,
			path: self.path,
		}
	}
}

/// The port an endpoint names, when it names one.
fn endpoint_port(endpoint: &Endpoint) -> Option<u16> {
	match endpoint {
		Endpoint::Tcp(address) | Endpoint::Tls { address, .. } => {
			quant_relay::link::split_host_port(address).map(|(_, port)| port)
		}
		Endpoint::Relay {
			address: Some(address),
			..
		} => quant_relay::link::split_host_port(address).map(|(_, port)| port),
		_ => None,
	}
}

/// The verified direct TLS `host:port` this app already used, when a phone on
/// the same network could also reach it.
fn reachable_tls_endpoint(endpoint: &Endpoint) -> Option<(String, u16)> {
	let address = match endpoint {
		Endpoint::Tls { address, .. }
		| Endpoint::Relay {
			address: Some(address),
			..
		} => address.as_str(),
		_ => return None,
	};
	let (host, port) = quant_relay::link::split_host_port(address)?;
	quant_relay::link::is_reachable_host(&host).then_some((host, port))
}

fn ok() -> Reply {
	Reply {
		ok: true,
		error: None,
		data: None,
	}
}

fn failed(error: String) -> Reply {
	Reply {
		ok: false,
		error: Some(error),
		data: None,
	}
}

/// The machine's short hostname, or nothing when `/bin/hostname` is missing.
fn short_hostname() -> Option<String> {
	std::process::Command::new("/bin/hostname")
		.arg("-s")
		.output()
		.ok()
		.and_then(|out| String::from_utf8(out.stdout).ok())
		.map(|name| name.trim().to_owned())
		.filter(|name| !name.is_empty())
}

fn device_name() -> String {
	match short_hostname() {
		Some(host) => format!("Quant desktop on {host}"),
		None => "Quant desktop".to_owned(),
	}
}

/// The label this Mac's Touch ID key is enrolled under, at most 64 characters.
fn touch_id_label() -> String {
	let host = short_hostname().unwrap_or_else(|| "This Mac".to_owned());
	format!("{host} Touch ID").chars().take(64).collect()
}

/// The Touch ID settings on disk: the enrolled owner device id, or none.
fn read_touch_id(path: &std::path::Path) -> Option<String> {
	let bytes = std::fs::read(path).ok()?;
	let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
	value.get("device_id")?.as_str().map(ToOwned::to_owned)
}

/// Store this Mac's Touch ID device id, or clear the settings when `None`.
fn write_touch_id(path: &std::path::Path, device_id: Option<&str>) -> Result<(), String> {
	let Some(device_id) = device_id else {
		// Clearing removes the file; a missing file is not an error.
		let _ = std::fs::remove_file(path);
		return Ok(());
	};
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
	}
	let bytes = serde_json::to_vec(&serde_json::json!({ "device_id": device_id }))
		.map_err(|e| e.to_string())?;
	std::fs::write(path, bytes).map_err(|e| e.to_string())
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
				agent: c.agent.as_ref().map(quant_access::AgentIdentity::summary),
				code: c.pairing_code.clone(),
				requested: c.requested.clone(),
				asked: words::when(c.created_at, now, tz),
			}
		})
		.collect();
	// Grants show the agent identity of the client holding them, when the
	// client is still known.
	let agent_of: HashMap<String, String> = data
		.clients
		.iter()
		.filter_map(|c| {
			c.agent
				.as_ref()
				.map(|agent| (c.id.to_string(), agent.summary()))
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
				agent: agent_of.get(&g.client.to_string()).cloned(),
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
				purpose: entry.purpose.clone(),
			}
		})
		.collect();
	let devices = data
		.devices
		.iter()
		.filter(|device| device.revoked_at.is_none())
		.map(|device| {
			OwnerDeviceRow {
				id: device.id.to_string(),
				label: device.label.clone(),
				fingerprint: device.fingerprint.clone(),
				last_used: device.last_used.map(|at| words::when(at, now, tz)),
			}
		})
		.collect();
	// A question's asker is the client's name, with what an agent said it
	// is, so the owner can tell two "Claude Code"s apart.
	let questions = data
		.questions
		.iter()
		.map(|q| {
			let id = q.client.to_string();
			let name = names.get(&id).map_or(id.as_str(), |name| name);
			QuestionRow {
				id: q.id.to_string(),
				asker: match agent_of.get(&id) {
					Some(agent) => format!("{name} · {agent}"),
					None => name.to_owned(),
				},
				asked: words::when(q.asked_at, now, tz),
				question: q.question.clone(),
				purpose: q.purpose.clone(),
				model: q.model.clone(),
				status: q.status.as_str().to_owned(),
				answer: q.answer.clone(),
				citations: q.citations.len(),
				served: words::served(q.records, q.bytes),
			}
		})
		.collect();
	let passphrase_session = data
		.status
		.as_ref()
		.is_some_and(|status| status.session == SessionSource::Passphrase);
	OwnerView {
		until: words::clock(session.expires_at, tz),
		pending,
		grants,
		audit,
		chain_ok: data.chain_ok,
		devices,
		passphrase_session,
		questions,
	}
}

#[cfg(test)]
mod tests {
	use leptoskit_native::vector::PathCommand;
	use leptoskit_native::vector::PathData;

	use super::*;

	fn hello(tls_address: Option<&str>) -> Hello {
		Hello {
			service: "quant".into(),
			version: "0.1.0".into(),
			vault_id: "v1".into(),
			tls_fingerprint: Some("abc123".into()),
			tls_address: tls_address.map(str::to_owned),
			relay: None,
			relay_rid: None,
		}
	}

	fn coord(value: f32) -> usize {
		assert!(
			value.is_finite() && value >= 0.0,
			"expected non-negative finite coordinate, got {value}"
		);
		#[allow(
			clippy::cast_possible_truncation,
			clippy::cast_sign_loss,
			reason = "non-negative integer module coordinate validated above"
		)]
		{
			value.round() as usize
		}
	}

	/// Rasterize the rendered SVG path (`PathData::parse`) onto a `side x side`
	/// grid and verify every dark module respects the 4-module quiet zone.
	fn rasterize_qr_path(qr: &PhoneQr) -> Vec<Vec<bool>> {
		let side = usize::from(qr.side);
		assert!(side >= 29, "minimum QR (21) + 8 quiet modules = 29");
		let parsed = PathData::parse(&qr.path).expect("valid SVG path data");
		let mut grid = vec![vec![false; side]; side];
		let commands = parsed.commands();
		assert!(!commands.is_empty());
		let mut i = 0;
		while i < commands.len() {
			let PathCommand::MoveTo(p0) = commands[i] else {
				panic!("expected MoveTo at {i}, got {:?}", commands[i]);
			};
			let PathCommand::LineTo(p1) = commands[i + 1] else {
				panic!("expected LineTo at {}", i + 1);
			};
			let PathCommand::LineTo(p2) = commands[i + 2] else {
				panic!("expected LineTo at {}", i + 2);
			};
			let PathCommand::LineTo(p3) = commands[i + 3] else {
				panic!("expected LineTo at {}", i + 3);
			};
			assert_eq!(commands[i + 4], PathCommand::Close);
			i += 5;

			let x0 = coord(p0.x);
			let y0 = coord(p0.y);
			let x1 = coord(p1.x);
			let y2 = coord(p2.y);
			assert_eq!(coord(p1.y), y0);
			assert_eq!(coord(p2.x), x1);
			assert_eq!(coord(p3.x), x0);
			assert_eq!(coord(p3.y), y2);
			assert!(x1 > x0 && y2 > y0);
			for row in &mut grid[y0..y2] {
				for cell in &mut row[x0..x1] {
					*cell = true;
				}
			}
		}
		// Verify a clean 4-module quiet zone on every side.
		for (y, row) in grid.iter().enumerate() {
			for (x, &dark) in row.iter().enumerate() {
				if x < 4 || y < 4 || x >= side - 4 || y >= side - 4 {
					assert!(!dark, "quiet zone module ({x}, {y}) must be light");
				}
			}
		}
		grid
	}

	/// ISO/IEC 18004 QR decoder for versions 1..=12 (`EcLevel::M`, the default
	/// of `qrcode::QrCode::new`), reading directly from the rasterized
	/// `PathData` bitmap after verifying the 4-module quiet zone.
	fn decode_qr_from_path(qr: &PhoneQr) -> String {
		const ALNUM: &[u8; 45] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";

		let full = rasterize_qr_path(qr);
		let side = full.len();
		let n = side - 8;
		assert_eq!((n - 17) % 4, 0, "invalid QR size {n}");
		let version = (n - 17) / 4;
		let matrix: Vec<Vec<bool>> = full[4..(4 + n)]
			.iter()
			.map(|row| row[4..(4 + n)].to_vec())
			.collect();

		// Verify all three 7x7 finder patterns.
		for (fx, fy) in [(0, 0), (n - 7, 0), (0, n - 7)] {
			assert!(matrix[fy][fx]);
			assert!(matrix[fy][fx + 6] && matrix[fy + 6][fx]);
			assert!(!matrix[fy + 1][fx + 1]);
			assert!(matrix[fy + 3][fx + 3]);
		}

		// Read 15-bit format information (top-left) and unmask with 0x5412.
		let format_coords = [
			(0, 8),
			(1, 8),
			(2, 8),
			(3, 8),
			(4, 8),
			(5, 8),
			(7, 8),
			(8, 8),
			(8, 7),
			(8, 5),
			(8, 4),
			(8, 3),
			(8, 2),
			(8, 1),
			(8, 0),
		];
		let mut raw_fmt: u16 = 0;
		for &(x, y) in &format_coords {
			raw_fmt = (raw_fmt << 1) | u16::from(matrix[y][x]);
		}
		let fmt = raw_fmt ^ 0x5412;
		let ec_bits = (fmt >> 13) & 0b11;
		assert_eq!(ec_bits, 0b00, "expected EcLevel::M format bits");
		let mask = usize::from((fmt >> 10) & 0b111);

		// Mark function modules (finders, separators, format, timing, dark,
		// alignment, and version info).
		let mut function = vec![vec![false; n]; n];
		for (y, row) in function.iter_mut().enumerate() {
			for (x, slot) in row.iter_mut().enumerate() {
				let top_left = x <= 8 && y <= 8;
				let top_right = x >= n - 8 && y <= 8;
				let bottom_left = x <= 8 && y >= n - 8;
				let timing = x == 6 || y == 6;
				if top_left || top_right || bottom_left || timing {
					*slot = true;
				}
			}
		}
		let align_centers: &[usize] = match version {
			1 => &[],
			2 => &[6, 18],
			3 => &[6, 22],
			4 => &[6, 26],
			5 => &[6, 30],
			6 => &[6, 34],
			7 => &[6, 22, 38],
			8 => &[6, 24, 42],
			9 => &[6, 26, 46],
			10 => &[6, 28, 50],
			11 => &[6, 30, 54],
			12 => &[6, 32, 58],
			_ => panic!("unsupported test QR version {version}"),
		};
		for &cy in align_centers {
			for &cx in align_centers {
				let hits_top_finder = cy <= 8 && (cx <= 8 || cx >= n - 8);
				let hits_bottom_finder = cx <= 8 && cy >= n - 8;
				if hits_top_finder || hits_bottom_finder {
					continue;
				}
				for row in &mut function[(cy - 2)..=(cy + 2)] {
					for slot in &mut row[(cx - 2)..=(cx + 2)] {
						*slot = true;
					}
				}
			}
		}
		if version >= 7 {
			for y in 0..6 {
				for x in (n - 11)..(n - 8) {
					function[y][x] = true;
					function[x][y] = true;
				}
			}
		}

		// Read unmasked data bits in standard zig-zag order.
		let mut bits = Vec::new();
		let mut col: isize = n.cast_signed() - 1;
		let mut upward = true;
		while col > 0 {
			if col == 6 {
				col -= 1;
			}
			for step in 0..n {
				let r = if upward { n - 1 - step } else { step };
				for dc in 0..=1 {
					let c = (col - dc).cast_unsigned();
					if function[r][c] {
						continue;
					}
					let flip = match mask {
						0 => (r + c) % 2 == 0,
						1 => r % 2 == 0,
						2 => c % 3 == 0,
						3 => (r + c) % 3 == 0,
						4 => ((r / 2) + (c / 3)) % 2 == 0,
						5 => (r * c) % 2 + (r * c) % 3 == 0,
						6 => ((r * c) % 2 + (r * c) % 3) % 2 == 0,
						7 => ((r + c) % 2 + (r * c) % 3) % 2 == 0,
						_ => unreachable!(),
					};
					bits.push(matrix[r][c] ^ flip);
				}
			}
			col -= 2;
			upward = !upward;
		}

		// Group into raw codewords and de-interleave data blocks for
		// `EcLevel::M`.
		let codewords: Vec<u8> = bits
			.as_chunks::<8>()
			.0
			.iter()
			.map(|chunk| chunk.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)))
			.collect();
		let block_lens: &[usize] = match version {
			1 => &[16],
			2 => &[28],
			3 => &[44],
			4 => &[32, 32],
			5 => &[43, 43],
			6 => &[27, 27, 27, 27],
			7 => &[31, 31, 31, 31],
			8 => &[38, 38, 39, 39],
			9 => &[36, 36, 36, 37, 37],
			10 => &[43, 43, 43, 43, 44],
			11 => &[50, 51, 51, 51, 51],
			12 => &[36, 36, 36, 36, 36, 36, 37, 37],
			_ => panic!("unsupported test QR version {version}"),
		};
		let max_len = *block_lens.iter().max().unwrap();
		let mut blocks: Vec<Vec<u8>> = block_lens.iter().map(|&l| Vec::with_capacity(l)).collect();
		let mut idx = 0;
		for pos in 0..max_len {
			for (b, &len) in block_lens.iter().enumerate() {
				if pos < len {
					blocks[b].push(codewords[idx]);
					idx += 1;
				}
			}
		}
		let data: Vec<u8> = blocks.into_iter().flatten().collect();
		let data_bits: Vec<bool> = data
			.iter()
			.flat_map(|&byte| (0..8).rev().map(move |shift| ((byte >> shift) & 1) == 1))
			.collect();
		let read_u16 = |offset: usize, count: usize| -> u16 {
			data_bits[offset..offset + count]
				.iter()
				.fold(0u16, |acc, &b| (acc << 1) | u16::from(b))
		};
		let mut out = Vec::new();
		let mut pos = 0;
		while pos + 4 <= data_bits.len() {
			let mode = read_u16(pos, 4);
			pos += 4;
			match mode {
				0b0000 => break, // Terminator
				0b0001 => {
					// Numeric mode
					let count_bits = if version <= 9 { 10 } else { 12 };
					let mut remaining = usize::from(read_u16(pos, count_bits));
					pos += count_bits;
					while remaining >= 3 {
						let v = read_u16(pos, 10);
						pos += 10;
						out.extend_from_slice(format!("{v:03}").as_bytes());
						remaining -= 3;
					}
					if remaining == 2 {
						let v = read_u16(pos, 7);
						pos += 7;
						out.extend_from_slice(format!("{v:02}").as_bytes());
					} else if remaining == 1 {
						let v = read_u16(pos, 4);
						pos += 4;
						out.extend_from_slice(format!("{v:01}").as_bytes());
					}
				}
				0b0010 => {
					// Alphanumeric mode
					let count_bits = if version <= 9 { 9 } else { 11 };
					let mut remaining = usize::from(read_u16(pos, count_bits));
					pos += count_bits;
					while remaining >= 2 {
						let v = usize::from(read_u16(pos, 11));
						pos += 11;
						out.push(ALNUM[v / 45]);
						out.push(ALNUM[v % 45]);
						remaining -= 2;
					}
					if remaining == 1 {
						let v = usize::from(read_u16(pos, 6));
						pos += 6;
						out.push(ALNUM[v]);
					}
				}
				0b0100 => {
					// Byte mode
					let count_bits = if version <= 9 { 8 } else { 16 };
					let len = usize::from(read_u16(pos, count_bits));
					pos += count_bits;
					for _ in 0..len {
						out.push(u8::try_from(read_u16(pos, 8)).expect("8 bits fit in u8"));
						pos += 8;
					}
				}
				other => panic!("unsupported QR segment mode {other:#b}"),
			}
		}
		String::from_utf8(out).expect("QR payload is valid UTF-8")
	}

	#[test]
	fn unix_endpoint_uses_advertised_non_default_port_and_decodable_qr() {
		let link = pairing_link_with(
			&hello(Some("0.0.0.0:9000")),
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| Some("192.168.1.4".into()),
			|| None,
		)
		.unwrap();
		assert_eq!(
			link.link,
			"quant://link?addr=192.168.1.4:9000&fp=abc123&vault=v1"
		);
		let qr = link.qr.expect("the link fits a QR code");
		assert_eq!(decode_qr_from_path(&qr), link.link);
	}

	#[test]
	fn relay_link_qr_round_trips_and_decodes_with_quiet_zone() {
		let mut h = hello(Some("0.0.0.0:9000"));
		h.tls_fingerprint = Some("0123456789abcdef".repeat(4));
		h.relay = Some("wss://relay.example:8443".into());
		h.relay_rid = Some("abcdefghijklmnopqrstuvwxyz".into());
		let link = pairing_link_with(
			&h,
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| Some("192.168.1.4".into()),
			|| None,
		)
		.unwrap();
		assert_eq!(
			link.link,
			format!(
				"quant://link?addr=192.168.1.4:9000&fp={}&vault=v1&relay=wss%3A%2F%2Frelay.\
				 example%3A8443&rid=abcdefghijklmnopqrstuvwxyz",
				"0123456789abcdef".repeat(4)
			)
		);
		let qr = link.qr.expect("relayed link fits a QR code");
		assert_eq!(decode_qr_from_path(&qr), link.link);
	}

	#[test]
	fn advertised_specific_ipv6_bind_is_bracketed_and_decodable() {
		let link = pairing_link_with(
			&hello(Some("[2001:db8::1]:9000")),
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| None,
			|| None,
		)
		.unwrap();
		assert_eq!(
			link.link,
			"quant://link?addr=[2001:db8::1]:9000&fp=abc123&vault=v1"
		);
		let qr = link.qr.expect("IPv6 link fits a QR code");
		assert_eq!(decode_qr_from_path(&qr), link.link);
	}

	#[test]
	fn advertised_loopback_bind_without_relay_refuses_to_make_broken_qr() {
		let error = pairing_link_with(
			&hello(Some("127.0.0.1:9000")),
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| Some("192.168.1.4".into()),
			|| None,
		)
		.unwrap_err();
		assert!(error.contains("127.0.0.1:9000"), "{error}");
		assert!(error.contains("loopback"), "{error}");
		assert!(
			error.contains("quant service install --tcp 0.0.0.0:9000"),
			"{error}"
		);
		assert!(!error.contains("quant://link"), "{error}");
	}

	#[test]
	fn advertised_loopback_bind_with_relay_builds_relay_only_link() {
		let mut h = hello(Some("127.0.0.1:9000"));
		h.relay = Some("wss://relay.example".into());
		h.relay_rid = Some("abcdefghijklmnopqrstuvwxyz".into());
		let link = pairing_link_with(
			&h,
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| Some("192.168.1.4".into()),
			|| None,
		)
		.unwrap();
		assert_eq!(
			link.link,
			"quant://link?fp=abc123&vault=v1&relay=wss%3A%2F%2Frelay.example&\
			 rid=abcdefghijklmnopqrstuvwxyz"
		);
		assert_eq!(decode_qr_from_path(link.qr.as_ref().unwrap()), link.link);
	}

	#[test]
	fn unknown_tls_address_on_unix_endpoint_returns_actionable_fallback() {
		let error = pairing_link_with(
			&hello(None),
			&Endpoint::Unix("/tmp/quant.sock".into()),
			|| Some("192.168.1.4".into()),
			|| None,
		)
		.unwrap_err();
		assert!(error.contains("quant link --host"), "{error}");
		assert!(!error.contains("quant://link"), "{error}");
	}

	#[test]
	fn unknown_tls_address_on_verified_tls_endpoint_uses_that_endpoint_and_brackets_ipv6() {
		let endpoint = Endpoint::Tls {
			address: "[2001:db8::5]:9000".into(),
			fingerprint: [0; 32],
		};
		let link = pairing_link_with(&hello(None), &endpoint, || None, || None).unwrap();
		assert_eq!(
			link.link,
			"quant://link?addr=[2001:db8::5]:9000&fp=abc123&vault=v1"
		);
	}

	#[test]
	fn a_local_only_vault_says_how_to_listen() {
		let mut h = hello(None);
		h.tls_fingerprint = None;
		let error = pairing_link(&h, &Endpoint::Unix("/tmp/quant.sock".into())).unwrap_err();
		assert!(error.contains("quant service install --tcp 0.0.0.0:7717"));
		assert!(!error.contains("quant://link"), "{error}");
	}

	#[test]
	fn a_local_only_vault_on_a_non_default_port_says_that_port() {
		let mut h = hello(None);
		h.tls_fingerprint = None;
		let endpoint = Endpoint::Tls {
			address: "192.168.1.4:9000".into(),
			fingerprint: [0; 32],
		};
		let error = pairing_link(&h, &endpoint).unwrap_err();
		assert!(error.contains("quant service install --tcp 0.0.0.0:9000"));
	}

	#[test]
	fn an_offline_vault_says_how_to_start_it() {
		let error = ClientError::Unreachable {
			endpoint: "unix:/tmp/quant.sock".into(),
			source: std::io::Error::other("gone"),
		};
		let text = phone_hello_problem(&error, &Endpoint::Unix("/tmp/quant.sock".into()));
		assert!(text.contains("quant service install"), "{text}");
		assert!(!text.contains("quant://link"), "{text}");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn worker_phone_link_reads_advertised_tls_address_over_unix_socket() {
		use tokio::io::AsyncReadExt;
		use tokio::io::AsyncWriteExt;

		let dir = PathBuf::from(format!("/tmp/qpl-{}", std::process::id()));
		let _ = std::fs::create_dir_all(&dir);
		let sock = dir.join("q.sock");
		let _ = std::fs::remove_file(&sock);
		let listener = tokio::net::UnixListener::bind(&sock).unwrap();

		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			let mut buf = [0u8; 1024];
			let _ = stream.read(&mut buf).await.unwrap();
			let body = serde_json::to_string(&Hello {
				service: "quant".into(),
				version: "0.1.0".into(),
				vault_id: "v1".into(),
				tls_fingerprint: Some("deadbeef".into()),
				tls_address: Some("192.168.1.9:9443".into()),
				relay: Some("wss://relay.example".into()),
				relay_rid: Some("abcdefghijklmnopqrstuvwxyz".into()),
			})
			.unwrap();
			let resp = format!(
				"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
				 {}\r\nconnection: close\r\n\r\n{}",
				body.len(),
				body
			);
			stream.write_all(resp.as_bytes()).await.unwrap();
		});

		let mut worker = Worker::new(
			Config {
				endpoint: Endpoint::Unix(sock.clone()),
				vault: None,
				identity: dir.join("desktop.identity.json"),
				touch_id: dir.join("desktop.touch-id.json"),
				cache: dir.join("cache"),
				tz: TimeZone::UTC,
			},
			Arc::new(Shared::default()),
		);
		let done = worker.phone_link().await.unwrap();
		server.await.unwrap();
		let _ = std::fs::remove_dir_all(&dir);

		let Done::Data(ReplyData::PhoneLink(link)) = done else {
			panic!("expected ReplyData::PhoneLink");
		};
		assert_eq!(
			link.link,
			"quant://link?addr=192.168.1.9:9443&fp=deadbeef&vault=v1&relay=wss%3A%2F%2Frelay.\
			 example&rid=abcdefghijklmnopqrstuvwxyz"
		);
		assert_eq!(decode_qr_from_path(link.qr.as_ref().unwrap()), link.link);
	}
}
