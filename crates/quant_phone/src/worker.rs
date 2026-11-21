//! The worker thread that owns the spool and talks to the vault.
//!
//! It owns the encrypted [`Spool`], the signed [`Client`] and a small Tokio
//! runtime, and does nothing until a [`Job`] arrives: a reading to queue, a
//! command from the screen, or a tick. There is no timer here. Ticks come
//! from the screen's own timer and from the platform's background wake, so
//! a phone whose app is not running costs nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_app_usage as app_usage;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_health as health;
use leptoskit_plugin_location as location;
use leptoskit_plugin_pedometer as pedometer;
use leptoskit_plugin_thermal as thermal;
use leptoskit_plugin_wearable as wearable;
use leptoskit_sensor as sensor;
use quant_api::ClientKind;
use quant_api::ClientStatus;
use quant_client::Client;
use quant_client::ClientError;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_collector::CompanionEnvelope;
use quant_collector::DrainError;
use quant_collector::PowerMonitor;
use quant_collector::Spool;
use quant_collector::Uploader;
use quant_core::DeviceId;
use quant_crypto::VaultKey;
use quant_model::Notice;
use quant_model::phone::Command;
use quant_model::phone::Link;
use quant_model::phone::PHONE_SCOPES;
use quant_model::phone::Reading;
use quant_model::phone::Reply;
use quant_model::phone::ReplyData;
use quant_model::phone::Snapshot;
use quant_model::phone::Source;
use quant_model::phone::SourceRow;
use quant_model::phone::Storage;
use quant_model::phone::WATCH_SCOPES;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::saved::Saved;
use crate::words;

/// The shortest wait between two deliveries, and the first wait after one
/// fails. The collector asks for at least a minute between bursts.
const UPLOAD_PAUSE: Duration = Duration::from_secs(60);
/// The longest wait after repeated failures.
const LONGEST_PAUSE: Duration = Duration::from_mins(15);
/// How long a link that answered is trusted before asking again.
const LINK_FRESH: Duration = Duration::from_mins(5);
/// How long a notice stays on the screen.
const NOTICE_FOR: SignedDuration = SignedDuration::from_mins(2);
/// How far back the first app usage read goes.
const FIRST_USAGE: SignedDuration = SignedDuration::from_hours(24);
/// The longest stretch of app usage read at once, so one reply stays small.
const USAGE_SPAN: SignedDuration = SignedDuration::from_hours(24);
/// Stretches shorter than this are not worth a read yet.
const USAGE_MINIMUM: SignedDuration = SignedDuration::from_secs(60);

/// Where the phone or watch keeps its files, and how it words times.
#[derive(Clone, Debug)]
pub struct Config {
	/// The app's private data directory.
	pub data: PathBuf,
	/// The name the owner sees when approving this device.
	pub name: String,
	/// The time zone times are shown in.
	pub tz: TimeZone,
	/// Whether this host is a watch target (requests [`WATCH_SCOPES`] instead
	/// of [`PHONE_SCOPES`]).
	pub is_watch: bool,
}

impl Config {
	/// The configuration for an app whose private data lives in `data`.
	pub fn for_app(data: PathBuf) -> Self {
		let is_watch = cfg!(any(target_os = "watchos", feature = "watch"));
		let name = if cfg!(target_os = "watchos") {
			"Apple Watch"
		} else if cfg!(target_env = "ohos") {
			if is_watch {
				"Huawei Watch"
			} else {
				"HarmonyOS phone"
			}
		} else if cfg!(target_os = "android") {
			if is_watch {
				"Wear OS watch"
			} else {
				"Android phone"
			}
		} else if cfg!(target_os = "ios") {
			"iPhone"
		} else if is_watch {
			"Watch"
		} else {
			"Phone"
		};
		Self {
			data,
			name: name.to_owned(),
			tz: TimeZone::try_system().unwrap_or(TimeZone::UTC),
			is_watch,
		}
	}

	fn scopes(&self) -> &'static [&'static str] {
		if self.is_watch {
			&WATCH_SCOPES
		} else {
			&PHONE_SCOPES
		}
	}

	fn saved(&self) -> PathBuf {
		self.data.join("phone.json")
	}

	fn identity(&self) -> PathBuf {
		self.data.join("phone.identity.json")
	}

	fn spool(&self) -> PathBuf {
		self.data.join("spool")
	}
}

/// Work for the worker.
#[derive(Debug)]
pub enum Job {
	/// Something the screen asked for.
	Command(Command),
	/// An encoded [`Reading`] from a plugin.
	Reading(Vec<u8>),
	/// The platform woke the app in the background.
	Wake,
}

/// A job, and the guest call waiting for its answer.
#[derive(Debug)]
pub struct Request {
	/// The work.
	pub job: Job,
	/// The guest call to answer, if any.
	pub ticket: Option<u64>,
}

/// What the worker and the guest's thread share.
#[derive(Default)]
pub struct Shared {
	inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
	snapshot: Snapshot,
	replies: Vec<(u64, Reply)>,
	/// The service, while the screen's guest runs.
	waker: Option<Waker>,
	/// Whether the worker has stopped.
	stopped: bool,
}

impl Shared {
	fn lock(&self) -> MutexGuard<'_, Inner> {
		self.inner
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Wake the service with this waker on the next change.
	pub fn set_waker(&self, waker: &Waker) {
		let mut inner = self.lock();
		if !inner.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
			inner.waker = Some(waker.clone());
		}
	}

	fn wake(&self) {
		let waker = self.lock().waker.clone();
		if let Some(waker) = waker {
			waker.wake();
		}
	}

	/// The snapshot, when its version is not `seen`.
	pub fn snapshot_since(&self, seen: u64) -> Option<Snapshot> {
		let inner = self.lock();
		(inner.snapshot.version != seen).then(|| inner.snapshot.clone())
	}

	/// Answers for the screen's calls.
	pub fn take_replies(&self) -> Vec<(u64, Reply)> {
		std::mem::take(&mut self.lock().replies)
	}

	/// Whether the worker has stopped; otherwise `waker` is woken when it
	/// does.
	pub fn stopped(&self, waker: &Waker) -> bool {
		let mut inner = self.lock();
		if !inner.stopped {
			inner.waker = Some(waker.clone());
		}
		inner.stopped
	}

	fn stop(&self) {
		self.lock().stopped = true;
		self.wake();
	}

	fn update(&self, mut next: Snapshot) {
		let changed = {
			let mut inner = self.lock();
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

	fn finish(&self, ticket: Option<u64>, reply: Reply) {
		if let Some(ticket) = ticket {
			self.lock().replies.push((ticket, reply));
			self.wake();
		}
	}
}

/// Start the worker. It stops when every sender is dropped, after the job
/// it is on.
pub fn spawn(
	config: Config,
	shared: Arc<Shared>,
) -> std::io::Result<(mpsc::UnboundedSender<Request>, std::thread::JoinHandle<()>)> {
	let (sender, receiver) = mpsc::unbounded_channel();
	let thread = std::thread::Builder::new()
		.name("quant-phone".into())
		.spawn(move || {
			let runtime = tokio::runtime::Builder::new_current_thread()
				.enable_io()
				.enable_time()
				.build();
			match runtime {
				Ok(runtime) => runtime.block_on(run(config, Arc::clone(&shared), receiver)),
				Err(error) => {
					shared.update(Snapshot {
						headline: format!("Quant could not start: {error}"),
						..Snapshot::default()
					});
				}
			}
			shared.stop();
		})?;
	Ok((sender, thread))
}

async fn run(config: Config, shared: Arc<Shared>, mut jobs: mpsc::UnboundedReceiver<Request>) {
	let mut worker = Worker::new(config, shared);
	worker.publish();
	worker.check_link().await;
	worker.publish();
	while let Some(request) = jobs.recv().await {
		worker.handle(request).await;
	}
}

/// When something may be tried again after failing.
#[derive(Debug, Default)]
struct Pause {
	failures: u32,
	until: Option<Instant>,
}

impl Pause {
	fn over(&self) -> bool {
		self.until.is_none_or(|until| Instant::now() >= until)
	}

	fn rest(&mut self) {
		self.failures = 0;
		self.until = Some(Instant::now() + UPLOAD_PAUSE);
	}

	fn failed(&mut self) {
		let wait = UPLOAD_PAUSE
			.saturating_mul(1 << self.failures.min(4))
			.min(LONGEST_PAUSE);
		self.failures = self.failures.saturating_add(1);
		self.until = Some(Instant::now() + wait);
	}

	fn clear(&mut self) {
		*self = Self::default();
	}
}

#[derive(Debug, Default)]
struct Counter {
	readings: u64,
	last: Option<Timestamp>,
	problem: Option<String>,
}

struct Worker {
	config: Config,
	shared: Arc<Shared>,
	saved: Saved,
	client: Option<Client>,
	link: Link,
	link_checked: Option<Instant>,
	link_pause: Pause,
	spool: Option<Spool>,
	storage: Storage,
	power: PowerMonitor,
	sources: BTreeMap<Source, Counter>,
	delivered: Option<Timestamp>,
	upload_pause: Pause,
	upload_problem: Option<String>,
	notice: Option<(Notice, Timestamp)>,
	latest_heart_rate_bpm: Option<u16>,
	steps_today: u64,
	steps_day: Option<String>,
	watch_connected: bool,
	watch_name: Option<String>,
	ble_devices: Vec<String>,
}

impl Worker {
	fn new(config: Config, shared: Arc<Shared>) -> Self {
		let (saved, problem) = match Saved::load(&config.saved()) {
			Ok(saved) => (saved, None),
			Err(error) => (
				Saved::default(),
				Some(format!("This phone's settings could not be read: {error}")),
			),
		};
		let mut worker = Self {
			link: if saved.link.is_some() {
				Link::Connecting
			} else {
				Link::Unpaired
			},
			config,
			shared,
			saved,
			client: None,
			link_checked: None,
			link_pause: Pause::default(),
			spool: None,
			storage: Storage::Locked,
			power: PowerMonitor::new(),
			sources: BTreeMap::new(),
			delivered: None,
			upload_pause: Pause::default(),
			upload_problem: None,
			notice: None,
			latest_heart_rate_bpm: None,
			steps_today: 0,
			steps_day: None,
			watch_connected: false,
			watch_name: None,
			ble_devices: Vec::new(),
		};
		if let Some(problem) = problem {
			worker.say(problem, true);
		}
		worker
	}

	async fn handle(&mut self, request: Request) {
		let Request { job, ticket } = request;
		let reply = match job {
			Job::Reading(frame) => {
				self.reading(&frame).await;
				Reply::ok()
			}
			Job::Wake => {
				self.tick(false).await;
				Reply::ok()
			}
			Job::Command(command) => match self.command(command).await {
				Ok(Some(data)) => Reply::data(data),
				Ok(None) => Reply::ok(),
				Err(problem) => {
					self.say(problem.clone(), true);
					Reply::failed(problem)
				}
			},
		};
		self.publish();
		self.shared.finish(ticket, reply);
	}

	async fn command(&mut self, command: Command) -> Result<Option<ReplyData>, String> {
		match command {
			Command::Tick => self.tick(false).await,
			Command::Upload => self.tick(true).await,
			Command::Pair { link } => self.pair(&link).await?,
			Command::Forget => self.forget()?,
			Command::NewStorageKey => {
				let key = VaultKey::generate();
				return Ok(Some(ReplyData::StorageKey {
					key: STANDARD.encode(key.as_bytes()),
				}));
			}
			Command::OpenStorage { key, fresh } => self.open_storage(&key, fresh)?,
			Command::Collect { enabled } => {
				self.saved.collecting = enabled;
				self.save()?;
				self.say(
					if enabled {
						"Collecting on this device"
					} else {
						"Stopped collecting on this device"
					},
					false,
				);
			}
			Command::UsagePlan => return Ok(self.usage_plan(Timestamp::now())),
			Command::HealthPlan => return Ok(self.health_plan(Timestamp::now())),
			Command::SetHealthToken { token } => {
				self.saved.health_token = token;
				self.save()?;
			}
			Command::AdvanceHealthUntil { until_ms } => {
				let now_ms = Timestamp::now().as_millisecond();
				if until_ms <= now_ms
					&& self
						.saved
						.health_until_ms
						.is_none_or(|seen| until_ms > seen)
				{
					self.saved.health_until_ms = Some(until_ms);
					self.save()?;
				}
			}
			Command::WatchPeerStatus { connected, name } => {
				self.watch_connected = connected;
				if let Some(name) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
					self.watch_name = Some(name);
				}
			}
			Command::ExportPairLink => {
				let link = self.saved.link.clone().ok_or_else(|| {
					"Connect to your vault first before pairing a watch".to_owned()
				})?;
				return Ok(Some(ReplyData::PairLink { link }));
			}
		}
		Ok(None)
	}

	/// Accept a pairing link and ask the vault to pair.
	async fn pair(&mut self, link: &str) -> Result<(), String> {
		let link = link.trim();
		endpoint(link)?;
		if self.saved.link.as_deref() != Some(link) || self.link == Link::Revoked {
			// Another vault or a revoked grant needs a fresh client identity.
			self.forget_identity()?;
			self.saved.link = Some(link.to_owned());
			self.save()?;
		}
		self.link = Link::Connecting;
		self.link_pause.clear();
		self.check_link().await;
		match &self.link {
			Link::Unreachable { reason } => Err(format!("The vault did not answer: {reason}")),
			_ => Ok(()),
		}
	}

	fn forget(&mut self) -> Result<(), String> {
		self.forget_identity()?;
		self.saved.link = None;
		self.save()?;
		self.link = Link::Unpaired;
		self.link_checked = None;
		self.upload_problem = None;
		self.say(
			"Forgot the vault. What is waiting stays on this phone.",
			false,
		);
		Ok(())
	}

	fn forget_identity(&mut self) -> Result<(), String> {
		self.client = None;
		match std::fs::remove_file(self.config.identity()) {
			Ok(()) => Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(error) => Err(format!(
				"This phone's identity could not be removed: {error}"
			)),
		}
	}

	fn open_storage(&mut self, key: &str, fresh: bool) -> Result<(), String> {
		if self.spool.is_some() {
			return Ok(());
		}
		let bytes = Zeroizing::new(
			STANDARD
				.decode(key)
				.map_err(|_| "The storage key is not valid".to_owned())?,
		);
		let bytes: [u8; 32] = bytes
			.as_slice()
			.try_into()
			.map_err(|_| "The storage key is not valid".to_owned())?;
		let key = VaultKey::from_bytes(bytes);
		let dir = self.config.spool();
		if fresh && dir.exists() {
			// A new key means the old one is gone from the keystore, so
			// whatever the old spool held can never be read again.
			std::fs::remove_dir_all(&dir)
				.map_err(|error| format!("The old storage could not be cleared: {error}"))?;
		}
		match Spool::open(&dir, &key) {
			Ok(spool) => {
				self.spool = Some(spool);
				self.storage = Storage::Open;
				Ok(())
			}
			Err(error) => {
				let reason = error.to_string();
				self.storage = Storage::Failed {
					reason: reason.clone(),
				};
				Err(format!(
					"This phone's storage could not be opened: {reason}"
				))
			}
		}
	}

	fn save(&self) -> Result<(), String> {
		self.saved
			.save(&self.config.saved())
			.map_err(|error| format!("This phone's settings could not be saved: {error}"))
	}

	/// The next stretch of app usage to read, when one is due.
	fn usage_plan(&self, now: Timestamp) -> Option<ReplyData> {
		if !self.saved.collecting || self.spool.is_none() {
			return None;
		}
		let earliest = now - FIRST_USAGE;
		let since = self
			.saved
			.usage_until_ms
			.and_then(|ms| Timestamp::from_millisecond(ms).ok())
			.filter(|since| *since <= now)
			.map_or(earliest, |since| since.max(now - FIRST_USAGE * 7));
		let until = now.min(since + USAGE_SPAN);
		(until.duration_since(since) >= USAGE_MINIMUM).then(|| ReplyData::UsageRange {
			since_ms: since.as_millisecond(),
			until_ms: until.as_millisecond(),
		})
	}

	/// The next health-store sync window and saved incremental changes token.
	fn health_plan(&self, now: Timestamp) -> Option<ReplyData> {
		if !self.saved.collecting || self.spool.is_none() {
			return None;
		}
		let earliest = now - FIRST_USAGE;
		let since = self
			.saved
			.health_until_ms
			.and_then(|ms| Timestamp::from_millisecond(ms).ok())
			.filter(|since| *since <= now)
			.map_or(earliest, |since| since.max(now - FIRST_USAGE * 7));
		let until = now.min(since + USAGE_SPAN);
		Some(ReplyData::HealthWindow {
			since_ms: since.as_millisecond(),
			until_ms: until.as_millisecond(),
			token: self.saved.health_token.clone(),
		})
	}

	/// Queue one reading. A problem is shown on its source; it never stops
	/// the others.
	async fn reading(&mut self, frame: &[u8]) {
		// Handle companion watch-sync envelopes at top level so PairLink can
		// trigger async pairing and ReadingFrame can ferry an inner frame
		// under the watch's own DeviceId without mutating phone state.
		if let Some(Reading::WatchSync(inner)) = Reading::decode(frame) {
			if let Ok(envelope) = serde_json::from_slice::<CompanionEnvelope>(inner) {
				match &envelope {
					CompanionEnvelope::PairLink { link } => {
						self.watch_connected = true;
						let trimmed = link.trim();
						let should_pair = self.saved.link.is_none()
							|| self.link == Link::Revoked
							|| self.saved.link.as_deref() != Some(trimmed);
						if should_pair && let Err(problem) = self.pair(trimmed).await {
							self.say(problem, true);
						}
						return;
					}
					CompanionEnvelope::ReadingFrame {
						device_id,
						watch_name,
						..
					} => {
						self.watch_connected = true;
						let trimmed_name = watch_name.trim();
						if !trimmed_name.is_empty() {
							self.watch_name = Some(trimmed_name.to_owned());
						}
						let watch_device = device_id.unwrap_or_else(|| {
							watch_fallback_device_id(self.saved.device, trimmed_name)
						});
						let Some(nested) = envelope.decoded_frame() else {
							let counter = self.sources.entry(Source::WatchSync).or_default();
							counter.problem =
								Some("The companion watch frame could not be decoded".to_owned());
							return;
						};
						self.reading_from_watch(watch_device, &nested);
						return;
					}
				}
			}
			// Fallback: treat raw inner bytes as a ferried Reading frame from a
			// companion watch with a distinct derived DeviceId.
			self.watch_connected = true;
			let watch_device = watch_fallback_device_id(
				self.saved.device,
				self.watch_name.as_deref().unwrap_or("watch"),
			);
			self.reading_from_watch(watch_device, inner);
			return;
		}
		let _ = self.reading_local(frame);
	}

	/// Queue one ferried reading from a companion watch under `watch_device`.
	///
	/// Isolated from phone state: runs even when local phone collection is
	/// paused, never overwrites the phone's `PowerMonitor`, health sync token,
	/// app-usage watermark, boot step counter, or `Source::Wearable` counts.
	fn reading_from_watch(&mut self, watch_device: DeviceId, frame: &[u8]) {
		let Some(reading) = Reading::decode(frame) else {
			let counter = self.sources.entry(Source::WatchSync).or_default();
			counter.problem = Some("The companion watch reading could not be parsed".to_owned());
			return;
		};
		let now = Timestamp::now();
		let mut new_bpm: Option<u16> = None;
		let outcome = match &self.spool {
			None => Err("This device's storage is not open yet".to_owned()),
			Some(spool) => {
				match reading {
					Reading::Location(updates) => location_reading(spool, watch_device, updates)
						.map(|(stored, problem, _)| (stored, problem)),
					Reading::Usage {
						sessions, events, ..
					} => usage_reading(spool, watch_device, sessions, &events)
						.map(|stored| (stored, None)),
					Reading::Power { battery, thermal } => {
						// Use a local monitor so watch battery/thermal state
						// never alters the phone's upload thermal budget.
						let mut watch_power = PowerMonitor::new();
						power_reading(spool, &mut watch_power, watch_device, now, battery, thermal)
							.map(|stored| (stored, None))
					}
					Reading::Health {
						is_changes,
						payload,
					} => health_reading(spool, watch_device, is_changes, payload)
						.map(|(stored, _)| (stored, None)),
					Reading::Wearable(batch) => wearable_reading(spool, watch_device, batch).map(
						|(stored, problem, bpm, _)| {
							new_bpm = bpm;
							(stored, problem)
						},
					),
					Reading::Pedometer(payload) => {
						let mut ignored_boot = None;
						pedometer_reading(
							spool,
							watch_device,
							&mut ignored_boot,
							now,
							payload,
							false,
						)
						.map(|(stored, _)| (stored, None))
					}
					Reading::Motion { kind_code, batch } => spool
						.motion_batch(watch_device, now, kind_code, batch)
						.map(|stored| (stored as u64, None))
						.map_err(|error| error.to_string()),
					Reading::Ble(batch) => {
						ble_reading(spool, watch_device, batch).map(|(stored, bpm, _)| {
							new_bpm = bpm;
							(stored, None)
						})
					}
					Reading::WatchSync(_) => Ok((0, None)),
				}
			}
		};
		let counter = self.sources.entry(Source::WatchSync).or_default();
		match outcome {
			Ok((stored, problem)) => {
				counter.readings += stored;
				if stored > 0 {
					counter.last = Some(now);
				}
				counter.problem = problem;
				if let Some(bpm) = new_bpm {
					self.latest_heart_rate_bpm = Some(bpm);
				}
			}
			Err(problem) => counter.problem = Some(problem),
		}
	}

	fn reading_local(&mut self, frame: &[u8]) -> u64 {
		let Some(reading) = Reading::decode(frame) else {
			return 0;
		};
		if !self.saved.collecting {
			return 0;
		}
		let source = reading.source();
		let now = Timestamp::now();
		let mut health_token_update = HealthTokenUpdate::Unchanged;
		let mut new_bpm: Option<u16> = None;
		let mut steps_delta: u64 = 0;
		let mut seen_ble: Vec<String> = Vec::new();
		let mut boot_steps_dirty = false;

		let outcome = match &self.spool {
			None => Err("This device's storage is not open yet".to_owned()),
			Some(spool) => {
				match reading {
					Reading::Location(updates) => {
						location_reading(spool, self.saved.device, updates)
					}
					Reading::Usage {
						until_ms,
						sessions,
						events,
					} => usage_reading(spool, self.saved.device, sessions, &events)
						.map(|stored| (stored, None, Some(until_ms))),
					Reading::Power { battery, thermal } => power_reading(
						spool,
						&mut self.power,
						self.saved.device,
						now,
						battery,
						thermal,
					)
					.map(|stored| (stored, None, None)),
					Reading::Health {
						is_changes,
						payload,
					} => health_reading(spool, self.saved.device, is_changes, payload).map(
						|(stored, update)| {
							health_token_update = update;
							(stored, None, None)
						},
					),
					Reading::Wearable(batch) => wearable_reading(spool, self.saved.device, batch)
						.map(|(stored, problem, bpm, steps)| {
							new_bpm = bpm;
							steps_delta = steps;
							(stored, problem, None)
						}),
					Reading::Pedometer(payload) => {
						let before = self.saved.pedometer_last_boot_steps;
						let res = pedometer_reading(
							spool,
							self.saved.device,
							&mut self.saved.pedometer_last_boot_steps,
							now,
							payload,
							true,
						)
						.map(|(stored, steps)| {
							steps_delta = steps;
							(stored, None, None)
						});
						if self.saved.pedometer_last_boot_steps != before {
							boot_steps_dirty = true;
						}
						res
					}
					Reading::Motion { kind_code, batch } => spool
						.motion_batch(self.saved.device, now, kind_code, batch)
						.map(|stored| (stored as u64, None, None))
						.map_err(|error| error.to_string()),
					Reading::Ble(batch) => ble_reading(spool, self.saved.device, batch).map(
						|(stored, bpm, devices)| {
							new_bpm = bpm;
							seen_ble = devices;
							(stored, None, None)
						},
					),
					Reading::WatchSync(_) => {
						// Nested WatchSync frames are ignored to prevent
						// recursion.
						Ok((0, None, None))
					}
				}
			}
		};

		let mut total_stored = 0;
		let counter = self.sources.entry(source).or_default();
		match outcome {
			Ok((stored, problem, usage_until)) => {
				total_stored = stored;
				counter.readings += stored;
				if stored > 0 {
					counter.last = Some(now);
				}
				counter.problem = problem;
				if let Some(bpm) = new_bpm {
					self.latest_heart_rate_bpm = Some(bpm);
				}
				if steps_delta > 0 {
					let today = now.to_zoned(self.config.tz.clone()).date().to_string();
					if self.steps_day.as_deref() != Some(today.as_str()) {
						self.steps_today = 0;
						self.steps_day = Some(today);
					}
					self.steps_today = self.steps_today.saturating_add(steps_delta);
				}
				for dev in seen_ble {
					if !self.ble_devices.contains(&dev) && self.ble_devices.len() < 16 {
						self.ble_devices.push(dev);
					}
				}
				let mut dirty = boot_steps_dirty;
				if let Some(until_ms) = usage_until
					&& until_ms <= now.as_millisecond()
					&& self.saved.usage_until_ms.is_none_or(|seen| until_ms > seen)
				{
					self.saved.usage_until_ms = Some(until_ms);
					dirty = true;
				}
				match health_token_update {
					HealthTokenUpdate::Unchanged => {}
					HealthTokenUpdate::Clear => {
						self.saved.health_token = None;
						dirty = true;
					}
					HealthTokenUpdate::Set(token) => {
						self.saved.health_token = Some(token);
						self.saved.health_until_ms = Some(now.as_millisecond());
						dirty = true;
					}
				}
				if dirty && let Err(problem) = self.save() {
					self.say(problem, true);
				}
			}
			Err(problem) => counter.problem = Some(problem),
		}
		total_stored
	}

	/// Look at the vault if that is due, then deliver what is waiting.
	async fn tick(&mut self, now: bool) {
		if now {
			self.link_pause.clear();
			self.upload_pause.clear();
		}
		let stale = self
			.link_checked
			.is_none_or(|at| at.elapsed() >= LINK_FRESH);
		if self.saved.link.is_some()
			&& (now || self.link != Link::Ready || stale)
			&& self.link_pause.over()
		{
			self.check_link().await;
		}
		self.upload().await;
	}

	async fn upload(&mut self) {
		let (Some(spool), Some(client)) = (&self.spool, &self.client) else {
			return;
		};
		if self.link != Link::Ready || !self.upload_pause.over() {
			return;
		}
		let waiting = spool
			.pending()
			.map_or(0, |pending| pending.records + pending.batches);
		if waiting == 0 {
			self.upload_problem = None;
			return;
		}
		let budget = self.power.budget();
		if budget.max_duration.is_zero() {
			self.upload_problem =
				Some("Waiting for the phone to cool down before uploading".to_owned());
			self.upload_pause.rest();
			return;
		}
		match Uploader::new(spool, client).drain_with_budget(budget).await {
			Ok(delivered) => {
				if delivered.records + delivered.batches > 0 {
					self.delivered = Some(Timestamp::now());
				}
				self.upload_problem = delivered
					.throttled
					.then(|| "The vault asked this phone to slow down".to_owned());
				self.upload_pause.rest();
			}
			Err(DrainError::Remote(error)) => {
				self.upload_pause.failed();
				self.upload_problem = Some(problem(&error));
				match &error {
					ClientError::Unreachable { .. } => {
						self.link = Link::Unreachable {
							reason: error.to_string(),
						};
					}
					// The grant may have been revoked or have run out.
					_ => self.link_checked = None,
				}
			}
			Err(error) => {
				self.upload_pause.failed();
				self.upload_problem = Some(error.to_string());
			}
		}
	}

	fn client(&mut self) -> Result<&mut Client, String> {
		if self.client.is_none() {
			let link = self
				.saved
				.link
				.as_deref()
				.ok_or("No pairing link yet".to_owned())?;
			let endpoint = endpoint(link)?;
			let path = self.config.identity();
			let identity = match Identity::load(&path) {
				Ok(identity) => identity,
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
					Identity::generate(self.config.name.clone())
				}
				Err(error) => return Err(format!("can't read {}: {error}", path.display())),
			};
			self.client = Some(Client::new(endpoint, identity));
		}
		Ok(self.client.as_mut().expect("just set"))
	}

	/// How this phone stands with the vault, pairing on first contact.
	async fn check_link(&mut self) {
		if self.saved.link.is_none() {
			self.link = Link::Unpaired;
			return;
		}
		let link = self.ask_vault().await;
		if matches!(link, Link::Unreachable { .. }) {
			self.link_pause.failed();
		} else {
			self.link_pause.clear();
			self.link_checked = Some(Instant::now());
		}
		if link == Link::Ready && self.link != Link::Ready {
			self.upload_pause.clear();
		}
		self.link = link;
	}

	async fn ask_vault(&mut self) -> Link {
		let identity_path = self.config.identity();
		let scopes: Vec<String> = self
			.config
			.scopes()
			.iter()
			.map(|s| (*s).to_owned())
			.collect();
		let client = match self.client() {
			Ok(client) => client,
			Err(reason) => return Link::Unreachable { reason },
		};
		if client.identity().client.is_some() {
			match client.me().await {
				Ok(me) if me.status == ClientStatus::Active && me.grant.is_some() => {
					return Link::Ready;
				}
				Ok(me) if me.status != ClientStatus::Pending => return Link::Revoked,
				// Pending, or a vault that does not know this client: ask
				// again for the pairing code.
				Ok(_) => {}
				Err(error) if error.code() == Some("unauthenticated") => {}
				Err(error) => {
					return Link::Unreachable {
						reason: error.to_string(),
					};
				}
			}
		}
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
			ClientStatus::Pending => Link::Pending {
				code: pairing.pairing_code,
			},
			ClientStatus::Revoked => Link::Revoked,
			ClientStatus::Active => {
				match client.me().await {
					Ok(me) if me.grant.is_some() => Link::Ready,
					// Approved without a live grant: expired or revoked since.
					_ => Link::Revoked,
				}
			}
		}
	}

	fn say(&mut self, text: impl Into<String>, error: bool) {
		self.notice = Some((
			Notice {
				text: text.into(),
				error,
			},
			Timestamp::now(),
		));
	}

	fn publish(&mut self) {
		let now = Timestamp::now();
		if self
			.notice
			.as_ref()
			.is_some_and(|(_, at)| now.duration_since(*at) > NOTICE_FOR)
		{
			self.notice = None;
		}
		let today = now.to_zoned(self.config.tz.clone()).date().to_string();
		if self.steps_day.as_deref() != Some(today.as_str()) {
			self.steps_today = 0;
			self.steps_day = Some(today);
		}
		let waiting = self.spool.as_ref().map_or(0, |spool| {
			spool
				.pending()
				.map_or(0, |pending| (pending.records + pending.batches) as u64)
		});
		let tz = &self.config.tz;
		let sources = Source::ALL
			.into_iter()
			.map(|source| {
				let counter = self.sources.get(&source);
				SourceRow {
					source,
					readings: counter.map_or(0, |c| c.readings),
					last: counter.and_then(|c| c.last).map(|at| words::clock(at, tz)),
					problem: counter.and_then(|c| c.problem.clone()),
				}
			})
			.collect();
		let approve = match &self.link {
			Link::Pending { code } => Some(words::approve_for(code, self.config.scopes())),
			_ => None,
		};
		self.shared.update(Snapshot {
			version: 0,
			headline: words::headline(&self.link, &self.storage, self.saved.collecting, waiting),
			link: self.link.clone(),
			vault: self
				.saved
				.link
				.as_deref()
				.and_then(|link| endpoint(link).ok())
				.map(|endpoint| words::vault(&endpoint)),
			approve,
			storage: self.storage.clone(),
			collecting: self.saved.collecting,
			waiting,
			delivered: self.delivered.map(|at| words::when(at, now, tz)),
			upload_problem: self.upload_problem.clone(),
			sources,
			notice: self.notice.as_ref().map(|(notice, _)| notice.clone()),
			latest_heart_rate_bpm: self.latest_heart_rate_bpm,
			steps_today: (self.steps_today > 0).then_some(self.steps_today),
			watch_connected: self.watch_connected,
			watch_name: self.watch_name.clone(),
			ble_devices: self.ble_devices.clone(),
			battery_percent: self.power.battery_percent(),
			device_id: Some(self.saved.device.0.to_string()),
		});
	}
}

/// Derive a stable, distinct [`DeviceId`] for a legacy companion watch frame
/// that did not carry an explicit `device_id`.
fn watch_fallback_device_id(phone: DeviceId, watch_name: &str) -> DeviceId {
	let mut hash = blake3::Hasher::new_derive_key("quant companion watch device v1");
	hash.update(phone.0.as_bytes());
	hash.update(watch_name.as_bytes());
	hash.finalize().to_hex()[..32]
		.parse()
		.expect("hash is a UUID-sized hex string")
}

/// The endpoint a pairing link names. A phone only ever talks to a vault
/// over TLS pinned to the link's certificate fingerprint, directly or
/// through a relay; nothing else is accepted, whatever the text says.
fn endpoint(link: &str) -> Result<Endpoint, String> {
	if !link.starts_with("quant://link") {
		return Err("Paste the link `quant link` shows; it starts with quant://link".to_owned());
	}
	match Endpoint::parse(link).map_err(|error| format!("That link can't be used: {error}"))? {
		endpoint @ (Endpoint::Tls { .. } | Endpoint::Relay { .. }) => Ok(endpoint),
		_ => Err("That link can't be used: it names no pinned certificate".to_owned()),
	}
}

type Stored = (u64, Option<String>, Option<i64>);

fn location_reading(spool: &Spool, device: DeviceId, updates: &[u8]) -> Result<Stored, String> {
	let Some(updates) = location::decode_updates(updates)
		.map_err(|error| format!("The location reading could not be read: {error}"))?
	else {
		return Ok((0, Some("Location updates stopped".to_owned()), None));
	};
	let report = spool
		.location_updates(device, &updates)
		.map_err(|error| error.to_string())?;
	let problem = (report.dropped > 0).then(|| {
		format!(
			"The phone dropped {} location fixes before they could be saved",
			report.dropped
		)
	});
	Ok((report.stored as u64, problem, None))
}

fn usage_reading(
	spool: &Spool,
	device: DeviceId,
	sessions: &[u8],
	events: &[&[u8]],
) -> Result<u64, String> {
	let unreadable = |error| format!("The app usage reading could not be read: {error}");
	let sessions = app_usage::decode_sessions(sessions).map_err(unreadable)?;
	let mut stored = spool
		.app_sessions(device, &sessions)
		.map_err(|error| error.to_string())?;
	for page in events {
		let events = app_usage::decode_events(page).map_err(unreadable)?;
		stored += spool
			.usage_events(device, &events)
			.map_err(|error| error.to_string())?;
	}
	Ok(stored as u64)
}

fn power_reading(
	spool: &Spool,
	power: &mut PowerMonitor,
	device: DeviceId,
	now: Timestamp,
	battery: Option<&[u8]>,
	thermal: Option<&[u8]>,
) -> Result<u64, String> {
	let battery = battery
		.map(battery::Snapshot::decode)
		.transpose()
		.map_err(|error| format!("The battery reading could not be read: {error}"))?;
	let thermal = thermal
		.map(thermal::Snapshot::decode)
		.transpose()
		.map_err(|error| format!("The thermal reading could not be read: {error}"))?;
	if battery.is_none() && thermal.is_none() {
		return Ok(0);
	}
	let report = power
		.observe(spool, device, now, battery, thermal)
		.map_err(|error| error.to_string())?;
	Ok(u64::from(report.level) + u64::from(report.changed))
}

enum HealthTokenUpdate {
	Unchanged,
	Clear,
	Set(String),
}

fn health_reading(
	spool: &Spool,
	device: DeviceId,
	is_changes: bool,
	payload: &[u8],
) -> Result<(u64, HealthTokenUpdate), String> {
	if is_changes {
		let changes = health::Changes::decode(payload)
			.map_err(|error| format!("The health changes reply could not be read: {error}"))?;
		let report = spool
			.health_samples(device, &changes.upserted)
			.map_err(|error| error.to_string())?;
		let next = if changes.token_expired {
			HealthTokenUpdate::Clear
		} else if let Some(token) = changes.next_token {
			HealthTokenUpdate::Set(token.as_str().to_owned())
		} else {
			HealthTokenUpdate::Unchanged
		};
		Ok((report.stored as u64, next))
	} else {
		let page = health::Page::decode(payload)
			.map_err(|error| format!("The health page could not be read: {error}"))?;
		let report = spool
			.health_samples(device, &page.samples)
			.map_err(|error| error.to_string())?;
		Ok((report.stored as u64, HealthTokenUpdate::Unchanged))
	}
}

fn wearable_reading(
	spool: &Spool,
	device: DeviceId,
	payload: &[u8],
) -> Result<(u64, Option<String>, Option<u16>, u64), String> {
	let batch = wearable::Batch::decode(payload)
		.map_err(|error| format!("The watch sensor batch could not be read: {error}"))?;
	let report = spool
		.wearable_batch(device, &batch)
		.map_err(|error| error.to_string())?;
	let problem = (report.dropped > 0).then(|| {
		format!(
			"The watch dropped {} sensor readings before they could be saved",
			report.dropped
		)
	});
	Ok((
		report.stored as u64,
		problem,
		report.latest_heart_rate_bpm,
		report.steps_delta,
	))
}

fn pedometer_reading(
	spool: &Spool,
	device: DeviceId,
	last_boot_steps: &mut Option<u64>,
	now: Timestamp,
	payload: &[u8],
	use_boot_delta: bool,
) -> Result<(u64, u64), String> {
	// `pedometer::NEXT` replies with a 4-byte `bounded_reply` tag followed by
	// the 32-byte `Reading` (or `[1, 0, 0, 0]` on timeout). Accept raw 32-byte
	// readings too for backwards compatibility with older callers.
	let raw = if payload.len() == 32 {
		payload
	} else {
		match sensor::bounded_reply(payload) {
			Ok(inner) => inner,
			Err(sensor::SensorError::TimedOut) => return Ok((0, 0)),
			Err(error) => {
				return Err(format!("The pedometer reading could not be read: {error}"));
			}
		}
	};
	let reading = pedometer::Reading::decode(raw)
		.map_err(|error| format!("The pedometer reading could not be read: {error}"))?;
	match reading {
		pedometer::Reading::Interval { reported_steps, .. } => {
			let stored = spool
				.pedometer_reading(device, now, &reading)
				.map_err(|error| error.to_string())?;
			Ok((stored as u64, reported_steps))
		}
		pedometer::Reading::BootCounter { reported_steps, .. } => {
			if !use_boot_delta {
				// Ferried boot-counter readings lack phone-side boot state;
				// ignore raw cumulative counters rather than inflating steps.
				return Ok((0, 0));
			}
			let delta = match *last_boot_steps {
				// First observation establishes the baseline since boot without
				// attributing historical boot steps to this moment.
				None => 0,
				Some(prev) if reported_steps >= prev => reported_steps - prev,
				// Counter rolled back after a device reboot.
				Some(_) => reported_steps,
			};
			*last_boot_steps = Some(reported_steps);
			let stored = spool
				.pedometer_delta_steps(device, now, delta)
				.map_err(|error| error.to_string())?;
			Ok((stored as u64, delta))
		}
	}
}

fn ble_reading(
	spool: &Spool,
	device: DeviceId,
	payload: &[u8],
) -> Result<(u64, Option<u16>, Vec<String>), String> {
	let batch = ble::NotificationBatch::decode(payload)
		.map_err(|error| format!("The Bluetooth sensor batch could not be read: {error}"))?;
	let mut devices = Vec::new();
	for notif in &batch.notifications {
		if !notif.device_id.is_empty() && !devices.contains(&notif.device_id) {
			devices.push(notif.device_id.clone());
		}
	}
	let report = spool
		.ble_notifications(device, &batch)
		.map_err(|error| error.to_string())?;
	Ok((report.stored as u64, report.latest_heart_rate_bpm, devices))
}

fn problem(error: &ClientError) -> String {
	match error {
		ClientError::Unreachable { .. } => "The vault is out of reach".to_owned(),
		ClientError::Api { error, .. } if error.code == "forbidden" => {
			format!("Not allowed: {}", error.message)
		}
		ClientError::Api { error, .. } if !error.message.is_empty() => error.message.clone(),
		error => error.to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_pinned_pairing_links_are_accepted() {
		let fp = "ab".repeat(32);
		assert!(matches!(
			endpoint(&format!(
				"quant://link?addr=192.168.1.4:7717&fp={fp}&vault=v1"
			)),
			Ok(Endpoint::Tls { .. })
		));
		assert!(matches!(
			endpoint(&format!(
				"quant://link?fp={fp}&relay=wss%3A%2F%2Frelay.example&rid={}",
				"abcdefghijklmnopqrstuvwxyz"
			)),
			Ok(Endpoint::Relay { .. })
		));
		for refused in [
			"unix:/tmp/quant.sock",
			"/tmp/quant.sock",
			"tcp://192.168.1.4:7717",
			"192.168.1.4:7717",
			"quant://link?addr=192.168.1.4:7717",
			"quant://link",
			"",
		] {
			assert!(endpoint(refused).is_err(), "{refused}");
		}
		// Even a pinned address is refused unless it came as a pairing link.
		assert!(endpoint(&format!("tls://192.168.1.4:7717#{fp}")).is_err());
	}

	#[test]
	fn failures_wait_longer_each_time_up_to_a_ceiling() {
		let mut pause = Pause::default();
		assert!(pause.over());
		let mut last = Duration::ZERO;
		for _ in 0..8 {
			pause.failed();
			let wait = pause.until.unwrap() - Instant::now();
			assert!(wait + Duration::from_secs(1) >= last);
			assert!(wait <= LONGEST_PAUSE);
			last = wait;
			assert!(!pause.over());
		}
		assert!(last + Duration::from_secs(1) >= LONGEST_PAUSE);
		pause.clear();
		assert!(pause.over());
		pause.rest();
		assert!(!pause.over());
	}
}
