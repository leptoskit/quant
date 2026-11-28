//! The collector: the only code that can reach the phone's plugins.
//!
//! The app's native service holds the encrypted spool and the vault
//! connection, and cannot call a plugin. The screen's guest can, so it runs
//! the collection: it fetches the spool key from the keystore, asks for
//! permissions, starts the background service and location updates, and
//! hands every plugin reply to the service as it arrived. It decodes a reply
//! only to learn whether to ask again.
//!
//! Android and iOS keep collecting in the background differently. Android
//! needs the background plugin's location-typed foreground service running
//! before it allows background location. iOS has no such service: location
//! updates themselves keep the app running once the owner chose "Always".
//! There the background plugin only schedules app refresh, which wakes the
//! service to deliver while Quant is suspended. The iOS app has no
//! notification to announce, so it leaves out the notifications plugin, and
//! that step and the battery step report themselves unavailable.
//!
//! The guest has no clock. Stretches of time come from the service
//! ([`Command::UsagePlan`]), and the service stamps what arrives.

use std::cell::Cell;
use std::cell::RefCell;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use leptoskit::guest::sleep;
use leptoskit::guest::spawn;
use leptoskit::prelude::GetUntracked;
use leptoskit::prelude::Set;
use leptoskit_bridge::Error;
#[cfg(any(feature = "android", feature = "ios"))]
use leptoskit_bridge::MethodId;
use leptoskit_bridge::guest::Client;
use leptoskit_plugin_app_usage as app_usage;
use leptoskit_plugin_app_usage::guest::AppUsage;
use leptoskit_plugin_background as background;
use leptoskit_plugin_background::guest::Background;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_battery::guest::Battery;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_ble::gatt;
use leptoskit_plugin_ble::guest::Ble;
use leptoskit_plugin_health as health;
use leptoskit_plugin_health::guest::Health;
use leptoskit_plugin_location as location;
use leptoskit_plugin_location::guest::Location;
use leptoskit_plugin_notifications as notifications;
use leptoskit_plugin_notifications::guest::Notifications;
use leptoskit_plugin_pedometer as pedometer;
use leptoskit_plugin_pedometer::guest::Pedometer;
use leptoskit_plugin_secure_store::Key;
use leptoskit_plugin_secure_store::guest::SecureStore;
use leptoskit_plugin_thermal as thermal;
use leptoskit_plugin_thermal::guest::Thermal;
use leptoskit_plugin_watch_sync::SyncStatus;
use leptoskit_plugin_watch_sync::guest::WatchSync;
use leptoskit_plugin_wearable as wearable;
use leptoskit_plugin_wearable::guest::Wearable;
use quant_model::phone::Command;
use quant_model::phone::HEALTH_SECONDS;
#[cfg(any(feature = "android", feature = "ios"))]
use quant_model::phone::LINK_NEXT;
#[cfg(any(feature = "android", feature = "ios"))]
use quant_model::phone::LINK_PLUGIN;
use quant_model::phone::Link;
use quant_model::phone::MAX_EVENT_PAGES;
use quant_model::phone::Reading;
use quant_model::phone::ReplyData;
use quant_model::phone::Storage;
use quant_model::phone::TICK_SECONDS;
use quant_model::phone::USAGE_SECONDS;

use super::Action;
use super::Grant;
#[cfg(any(feature = "android", feature = "ios"))]
use super::Invite;
use super::State;
use super::Step;
use super::bridge;

/// Where the spool key lives in the phone's keystore.
const KEY_SERVICE: &str = "dev.leptoskit.quant";
const KEY_NAME: &str = "spool-key";
/// How often location is sampled, and how far the phone must move.
const LOCATION_INTERVAL_MS: u32 = 60_000;
const LOCATION_DISTANCE_M: f64 = 25.0;
/// How long to wait before asking a plugin again after it failed.
const RETRY: Duration = Duration::from_secs(30);
/// How long to wait before asking for an inbound link again after a plugin
/// failure or an empty answer.
#[cfg(any(feature = "android", feature = "ios"))]
const LINK_RETRY: Duration = Duration::from_secs(5);
/// How often battery and thermal are read when the plugin can't wait for a
/// change.
const POWER_POLL: Duration = Duration::from_mins(5);
/// An app usage reply holding nothing: a count of zero.
const NO_SESSIONS: [u8; 4] = [0; 4];
/// Whether this is the iPhone app, which collects in the background through
/// the "Always" location grant instead of a foreground service.
const IOS: bool = cfg!(feature = "ios");
/// Whether this guest is running on a watch target.
const WATCH: bool = cfg!(feature = "watch");
/// Companion topic for pushing the vault pairing link from phone to watch.
const TOPIC_PAIR_LINK: &str = "/quant/pair";
/// Companion topic for ferrying a watch reading through a paired phone.
const TOPIC_READING_FRAME: &str = "/quant/reading";

thread_local! {
	/// What the collector has running. The guest is single-threaded.
	static RUN: Running = const { Running::new() };
}

struct Running {
	/// Whether the background service was started.
	background: Cell<bool>,
	/// Foreground service type bitmask currently active on Android / Wear OS.
	background_types: Cell<u32>,
	/// Whether location updates were started.
	location: Cell<bool>,
	/// Whether the running location updates continue in the background.
	location_background: Cell<bool>,
	/// Bumped whenever location updates start or stop, so a loop left over
	/// from an earlier start knows to end.
	generation: Cell<u32>,
	/// Whether an app usage read is under way.
	usage: Cell<bool>,
	/// Whether a health-store read is under way.
	health: Cell<bool>,
	/// Whether on-watch passive sensor monitoring is running.
	wearable: Cell<bool>,
	/// Whether a high-frequency live wrist-sensor / workout session is active.
	live_workout: Cell<bool>,
	/// Whether the wearable batch drainer task is running.
	wearable_loop_running: Cell<bool>,
	/// Whether BLE GATT notification draining is running.
	ble: Cell<bool>,
	/// BLE peripheral device IDs connected by this session, for clean
	/// disconnection on pause/stop.
	ble_connected: RefCell<Vec<String>>,
	/// Monotonic counter for durable watch companion transfer IDs.
	ferry_seq: Cell<u64>,
	/// Whether plugins are being started or stopped, and whether that needs
	/// doing again once it finishes.
	syncing: Cell<bool>,
	again: Cell<bool>,
	battery: Cell<Option<battery::Snapshot>>,
	thermal: Cell<Option<thermal::Snapshot>>,
}

impl Running {
	const fn new() -> Self {
		Self {
			background: Cell::new(false),
			background_types: Cell::new(0),
			location: Cell::new(false),
			location_background: Cell::new(false),
			generation: Cell::new(0),
			usage: Cell::new(false),
			health: Cell::new(false),
			wearable: Cell::new(false),
			live_workout: Cell::new(false),
			wearable_loop_running: Cell::new(false),
			ble: Cell::new(false),
			ble_connected: RefCell::new(Vec::new()),
			ferry_seq: Cell::new(0),
			syncing: Cell::new(false),
			again: Cell::new(false),
			battery: Cell::new(None),
			thermal: Cell::new(None),
		}
	}
}

/// Start following the service and collecting.
pub fn start(state: &State) {
	spawn(follow(state.clone()));
	spawn(run(state.clone()));
	#[cfg(any(feature = "android", feature = "ios"))]
	spawn(watch_links(state.clone()));
}

/// Carry out something the owner asked for on the screen.
pub fn act(state: &State, action: Action) {
	let state = state.clone();
	spawn(async move {
		match action {
			Action::Command(command) => {
				let reply = bridge::send(&command).await;
				if !reply.ok {
					state.problem.set(reply.error);
				}
			}
			Action::Allow(step) => {
				allow(&state, step).await;
				refresh(&state).await;
				sync(&state);
			}
			Action::ResetStorage => {
				open_storage(&state, true).await;
				sync(&state);
			}
			Action::PushPairLinkToWatch => {
				push_pair_link_to_watch(&state).await;
			}
			Action::ScanBleSensors => {
				scan_and_subscribe_ble(&state).await;
			}
			Action::ToggleLiveWorkout => {
				toggle_live_workout(&state).await;
			}
		}
	});
}

/// Wait for a link the operating system opened, and put it in front of the
/// owner.
///
/// The platform host holds the link until the guest takes it, so a cold
/// launch is not lost. Taking one changes no collection state: the card's
/// Connect action is the only thing that sends [`Command::Pair`], and Not now
/// drops the link. A host that has no link plugin answers `Unavailable`, and
/// the guest simply carries on with the paste field.
#[cfg(any(feature = "android", feature = "ios"))]
async fn watch_links(state: State) {
	loop {
		let client = match Client::new(LINK_PLUGIN) {
			Ok(client) => client,
			Err(Error::Unavailable) => return,
			Err(_) => {
				let _ = sleep(LINK_RETRY).await;
				continue;
			}
		};
		let call = match client.call(MethodId(LINK_NEXT), &[]) {
			Ok(call) => call.await,
			Err(error) => Err(error),
		};
		match call {
			// The host is closing this guest's plugin.
			Ok(bytes) if bytes.is_empty() => return,
			Ok(bytes) => {
				match std::str::from_utf8(&bytes).ok().and_then(Invite::parse) {
					Some(invite) => state.invite.set(Some(invite)),
					None => {
						state.problem.set(Some(
							"That link can't be used. Paste the link `quant link` shows instead."
								.to_owned(),
						))
					}
				}
			}
			Err(Error::Cancelled | Error::Unavailable) => return,
			Err(_) => {
				let _ = sleep(LINK_RETRY).await;
			}
		}
	}
}

/// Follow the service's snapshots, and start or stop the plugins when the
/// owner's choice or the storage changes.
async fn follow(state: State) {
	let mut seen = 0;
	let mut last = None;
	loop {
		match bridge::watch(seen).await {
			Ok(snapshot) => {
				seen = snapshot.version;
				let now = (snapshot.collecting, snapshot.storage == Storage::Open);
				state.snapshot.set(snapshot);
				if last != Some(now) {
					last = Some(now);
					sync(&state);
				}
			}
			Err(error) => {
				state.problem.set(Some(error));
				return;
			}
		}
	}
}

async fn run(state: State) {
	open_storage(&state, false).await;
	refresh(&state).await;
	sync(&state);
	spawn(power(state.clone()));
	spawn(watch_sync_loop(state.clone()));
	read_usage(&state);
	read_health(&state);
	read_pedometer(&state);
	let every_usage = (USAGE_SECONDS / TICK_SECONDS).max(1);
	let every_health = (HEALTH_SECONDS / TICK_SECONDS).max(1);
	let mut ticks: u64 = 0;
	loop {
		if sleep(Duration::from_secs(TICK_SECONDS)).await.is_err() {
			// Without a timer the background wake still delivers; only the
			// periodic reads are skipped.
			return;
		}
		ticks += 1;
		bridge::send(&Command::Tick).await;
		// The owner may have changed a permission in the system's settings.
		refresh(&state).await;
		sync(&state);
		if ticks.is_multiple_of(every_usage) {
			read_usage(&state);
		}
		if ticks.is_multiple_of(every_health) {
			read_health(&state);
			read_pedometer(&state);
		}
	}
}

/// Fetch the spool key from the keystore, making one on first launch, and
/// hand it to the service. With `reset`, replace a key the spool no longer
/// opens with.
async fn open_storage(state: &State, reset: bool) {
	if let Err(problem) = try_open_storage(reset).await {
		state.problem.set(Some(problem));
	}
}

async fn try_open_storage(reset: bool) -> Result<(), String> {
	let keystore = |error: Error| {
		format!("This phone's keystore can't be used, so nothing is stored: {error}")
	};
	let store = SecureStore::new().map_err(keystore)?;
	let name = Key::new(KEY_SERVICE, KEY_NAME).map_err(keystore)?;
	// Only a key the keystore says is absent is replaced. A keystore that
	// fails to answer must never cost the readings already waiting.
	let saved = if reset {
		None
	} else {
		store
			.get(name)
			.await
			.map_err(keystore)?
			.and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
	};
	let (key, fresh) = if let Some(key) = saved {
		(key, false)
	} else {
		let reply = bridge::send(&Command::NewStorageKey).await;
		let Some(ReplyData::StorageKey { key }) = reply.data else {
			return Err(reply
				.error
				.unwrap_or_else(|| "The service made no storage key".to_owned()));
		};
		// Saved before it is used: a key that seals readings and is then
		// lost would strand them.
		store.set(name, key.as_bytes()).await.map_err(keystore)?;
		(key, true)
	};
	let reply = bridge::send(&Command::OpenStorage { key, fresh }).await;
	if reply.ok {
		Ok(())
	} else {
		Err(reply
			.error
			.unwrap_or_else(|| "This phone's storage could not be opened".to_owned()))
	}
}

/// Ask the system where each permission stands. Nothing here prompts.
async fn refresh(state: &State) {
	let mut permissions = state.permissions.get_untracked();
	permissions.location = match Location::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.permission().await {
				Ok(permission) if permission.status == location::Status::Granted => Grant::Granted,
				Ok(_) => Grant::Needed,
				Err(Error::Unavailable) => Grant::Unavailable,
				Err(_) => permissions.location,
			}
		}
	};
	// The system offers no way to read the "all the time" grant without
	// asking for it, so it stays as the last request left it.
	if permissions.location != Grant::Granted && permissions.always == Grant::Granted {
		permissions.always = Grant::Unknown;
	}
	if permissions.location == Grant::Unavailable {
		permissions.always = Grant::Unavailable;
	}
	permissions.notifications = match Notifications::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.permission().await {
				Ok(notifications::Permission::Granted | notifications::Permission::Provisional) => {
					Grant::Granted
				}
				Ok(_) => Grant::Needed,
				Err(Error::Unavailable) => Grant::Unavailable,
				Err(_) => permissions.notifications,
			}
		}
	};
	permissions.usage = match AppUsage::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.usage_access_status().await {
				Ok(app_usage::UsageAccessStatus::Granted) => Grant::Granted,
				Ok(app_usage::UsageAccessStatus::Denied) => Grant::Needed,
				Ok(app_usage::UsageAccessStatus::Unknown) => Grant::Unknown,
				Err(Error::Unavailable) => Grant::Unavailable,
				Err(_) => permissions.usage,
			}
		}
	};
	permissions.battery = match Background::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.status().await {
				Ok(status) => battery_grant(status.battery_optimization()),
				Err(Error::Unavailable) => Grant::Unavailable,
				Err(_) => permissions.battery,
			}
		}
	};
	permissions.health = match Health::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.availability().await {
				Ok(health::Availability::Available) => {
					match plugin.granted_permissions().await {
						Ok(grants) if !grants.read.is_empty() => Grant::Granted,
						// HealthKit hides read authorization status for privacy
						// (`exact == false`); keep Granted if already prompted
						// this session, otherwise Unknown so the prompt button
						// remains available while queries still run on restart.
						Ok(grants) if !grants.exact => {
							if permissions.health == Grant::Granted {
								Grant::Granted
							} else {
								Grant::Unknown
							}
						}
						Ok(_) => Grant::Needed,
						Err(Error::Unavailable) => Grant::Unavailable,
						Err(_) => permissions.health,
					}
				}
				Ok(_) | Err(Error::Unavailable) => Grant::Unavailable,
				Err(_) => permissions.health,
			}
		}
	};
	permissions.wearable = match Wearable::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.permission().await {
				Ok(
					wearable::Permission::GrantedForeground
					| wearable::Permission::GrantedBackground,
				) => Grant::Granted,
				// watchOS HealthKit hides read authorization status.
				Ok(wearable::Permission::Unknown) => {
					if permissions.wearable == Grant::Granted {
						Grant::Granted
					} else {
						Grant::Unknown
					}
				}
				Ok(wearable::Permission::Unsupported) | Err(Error::Unavailable) => {
					Grant::Unavailable
				}
				Ok(_) => Grant::Needed,
				Err(_) => permissions.wearable,
			}
		}
	};
	permissions.bluetooth = match Ble::new() {
		Err(_) => Grant::Unavailable,
		Ok(plugin) => {
			match plugin.availability().await {
				Ok(ble::BleAvailability::PoweredOn) => Grant::Granted,
				// PoweredOff means Bluetooth is off, not ready to scan.
				Ok(ble::BleAvailability::PoweredOff) => Grant::Unknown,
				Ok(ble::BleAvailability::Unauthorized) => Grant::Needed,
				Ok(ble::BleAvailability::Unsupported) | Err(Error::Unavailable) => {
					Grant::Unavailable
				}
				Err(_) => permissions.bluetooth,
			}
		}
	};
	if permissions != state.permissions.get_untracked() {
		state.permissions.set(permissions);
	}
}

fn battery_grant(optimization: background::BatteryOptimization) -> Grant {
	match optimization {
		background::BatteryOptimization::Exempted => Grant::Granted,
		background::BatteryOptimization::NotExempted => Grant::Needed,
		// Only Android has battery optimisation to be exempt from.
		background::BatteryOptimization::Unknown => Grant::Unavailable,
	}
}

/// Ask the system for one permission. Each request shows the system's own
/// prompt or settings screen.
async fn allow(state: &State, step: Step) {
	let outcome: Result<Option<Grant>, Error> = match step {
		Step::Location => {
			match Location::new() {
				Ok(plugin) => plugin.request_permission(true).await.map(|_| None),
				Err(error) => Err(error),
			}
		}
		Step::Always => {
			match Location::new() {
				Ok(plugin) => {
					plugin
						.request_background_permission()
						.await
						.map(|permission| {
							Some(if permission.status == location::Status::Granted {
								Grant::Granted
							} else {
								Grant::Needed
							})
						})
				}
				Err(error) => Err(error),
			}
		}
		Step::Notifications => {
			match Notifications::new() {
				Ok(plugin) => plugin.request_permission().await.map(|_| None),
				Err(error) => Err(error),
			}
		}
		Step::Usage => {
			match AppUsage::new() {
				Ok(plugin) => plugin.open_usage_access_settings().await.map(|()| None),
				Err(error) => Err(error),
			}
		}
		Step::Battery => {
			match Background::new() {
				Ok(plugin) => {
					plugin
						.request_ignore_battery_optimizations()
						.await
						.map(|optimization| Some(battery_grant(optimization)))
				}
				Err(error) => Err(error),
			}
		}
		Step::Health => {
			match Health::new() {
				Ok(plugin) => {
					plugin
						.request_permissions(
							&health::DataType::ALL,
							health::Extra {
								background: true,
								history: true,
							},
						)
						.await
						.map(|grants| {
							Some(if !grants.read.is_empty() || !grants.exact {
								Grant::Granted
							} else {
								Grant::Needed
							})
						})
				}
				Err(error) => Err(error),
			}
		}
		Step::Wearable => {
			match Wearable::new() {
				Ok(plugin) => {
					plugin
						.request_permission(wearable::SensorMask::ALL)
						.await
						.map(|perm| {
							Some(match perm {
								wearable::Permission::GrantedForeground
								| wearable::Permission::GrantedBackground => Grant::Granted,
								// On watchOS HealthKit keeps read status
								// Unknown after prompting; mark Granted for
								// this session so sensors start immediately.
								wearable::Permission::Unknown => Grant::Granted,
								wearable::Permission::Unsupported => Grant::Unavailable,
								_ => Grant::Needed,
							})
						})
				}
				Err(error) => Err(error),
			}
		}
		Step::Bluetooth => {
			match Ble::new() {
				Ok(plugin) => {
					plugin.request_permission().await.map(|avail| {
						Some(match avail {
							ble::BleAvailability::PoweredOn => Grant::Granted,
							ble::BleAvailability::PoweredOff => Grant::Unknown,
							ble::BleAvailability::Unsupported => Grant::Unavailable,
							ble::BleAvailability::Unauthorized => Grant::Needed,
						})
					})
				}
				Err(error) => Err(error),
			}
		}
	};
	match outcome {
		Ok(Some(grant)) => {
			let mut permissions = state.permissions.get_untracked();
			permissions.set(step, grant);
			state.permissions.set(permissions);
		}
		Ok(None) | Err(Error::Cancelled) => {}
		Err(Error::InvalidArgument) if step == Step::Always => {
			state.problem.set(Some(
				"Allow location first, then allow it all the time".to_owned(),
			));
		}
		Err(error) => {
			state.problem.set(Some(format!(
				"The system did not open that request: {error}"
			)));
		}
	}
	if step == Step::Usage {
		// The grant happens on the system's screen, some time after this
		// returns. Look again for a while so the screen catches up.
		for _ in 0..30 {
			if sleep(Duration::from_secs(2)).await.is_err() {
				break;
			}
			refresh(state).await;
			if state.permissions.get_untracked().usage == Grant::Granted {
				read_usage(state);
				break;
			}
		}
	}
	if step == Step::Health {
		read_health(state);
	}
}

/// Start or stop the plugins to match what the owner chose. Calls made
/// while one is under way are folded into one more pass.
fn sync(state: &State) {
	if RUN.with(|run| run.syncing.replace(true)) {
		RUN.with(|run| run.again.set(true));
		return;
	}
	let state = state.clone();
	spawn(async move {
		loop {
			RUN.with(|run| run.again.set(false));
			sync_once(&state).await;
			if !RUN.with(|run| run.again.get()) {
				break;
			}
		}
		RUN.with(|run| run.syncing.set(false));
	});
}

async fn sync_once(state: &State) {
	let snapshot = state.snapshot.get_untracked();
	if !snapshot.collecting {
		stop(state).await;
		state.collector.set(None);
		return;
	}
	if snapshot.storage != Storage::Open {
		// Nothing can be queued yet; the storage card says why.
		return;
	}
	let perms = state.permissions.get_untracked();
	let mut problem = None;

	// Start the foreground / background service whenever collecting is active,
	// decoupled from Location so watches (and health/BLE-only users) still keep
	// collecting in the background using the appropriate service type.
	let desired_types = desired_background_types(perms.location == Grant::Granted);
	if let Some(types) = desired_types {
		let needs_start =
			RUN.with(|run| !run.background.get() || run.background_types.get() != types.bits());
		if needs_start {
			match start_background(types).await {
				Ok(()) => {
					RUN.with(|run| {
						run.background.set(true);
						run.background_types.set(types.bits());
					});
				}
				Err(Error::Unavailable) => {}
				Err(_) if IOS => {}
				Err(error) => {
					problem = Some(format!(
						"Quant can't keep collecting in the background on this device: {error}"
					));
				}
			}
		}
	}

	start_wearable_if_ready(state).await;

	match perms.location {
		Grant::Granted => {}
		Grant::Unavailable => {
			state.collector.set(problem);
			return;
		}
		// Not asked the system yet; the next pass knows.
		Grant::Unknown => return,
		Grant::Needed => {
			stop_location().await;
			if !WATCH && problem.is_none() {
				problem = Some(
					"Allow location below to record where this phone goes in the background."
						.to_owned(),
				);
			}
			state.collector.set(problem);
			return;
		}
	}

	let background = background_wanted(state);
	if RUN.with(|run| run.location.get() && run.location_background.get() != background) {
		// Started before the background service or the "Always" grant;
		// start again so updates continue in the background.
		stop_location().await;
	}
	if !RUN.with(|run| run.location.get()) {
		match start_location_for(state, background).await {
			// Busy means updates are already running from an earlier start.
			Ok(started) => {
				let generation = RUN.with(|run| {
					run.location.set(true);
					run.location_background.set(started);
					run.generation.set(run.generation.get().wrapping_add(1));
					run.generation.get()
				});
				spawn(location_loop(state.clone(), generation));
			}
			Err(error) => problem = Some(format!("Location updates could not start: {error}")),
		}
	}
	if IOS && problem.is_none() && !RUN.with(|run| run.location_background.get()) {
		problem = Some(
			"Allow location all the time to keep collecting while Quant is in the background. \
			 Until then this iPhone collects only while Quant is open."
				.to_owned(),
		);
	}
	state.collector.set(problem);
}

fn desired_background_types(location_granted: bool) -> Option<background::ForegroundServiceTypes> {
	if WATCH {
		Some(background::ForegroundServiceTypes::HEALTH)
	} else if location_granted {
		Some(background::ForegroundServiceTypes::LOCATION)
	} else {
		Some(background::ForegroundServiceTypes::DATA_SYNC)
	}
}

/// Whether location updates should continue in the background. Android
/// needs the background service running. iOS needs the "Always" grant, which
/// nothing can read without a prompt, so an unknown grant is tried and the
/// start reveals it.
fn background_wanted(state: &State) -> bool {
	if IOS {
		state.permissions.get_untracked().always != Grant::Needed
	} else {
		RUN.with(|run| {
			run.background.get()
				&& run.background_types.get() == background::ForegroundServiceTypes::LOCATION.bits()
		})
	}
}

/// Start location updates, in the background when `background`. Returns
/// whether they continue in the background. On iOS a refused background
/// start means the owner allowed location only while Quant is in use: that
/// is recorded, and updates start for the foreground instead.
async fn start_location_for(state: &State, background: bool) -> Result<bool, Error> {
	match start_location(background).await {
		Ok(()) | Err(Error::Busy) => {
			if IOS && background {
				note_always(state, Grant::Granted);
			}
			Ok(background)
		}
		Err(Error::PermissionDenied) if IOS && background => {
			note_always(state, Grant::Needed);
			match start_location(false).await {
				Ok(()) | Err(Error::Busy) => Ok(false),
				Err(error) => Err(error),
			}
		}
		Err(error) => Err(error),
	}
}

fn note_always(state: &State, grant: Grant) {
	let mut permissions = state.permissions.get_untracked();
	if permissions.always != grant {
		permissions.always = grant;
		state.permissions.set(permissions);
	}
}

async fn start_background(types: background::ForegroundServiceTypes) -> Result<(), Error> {
	let body = if WATCH {
		"Wrist sensors and health readings go to your vault."
	} else {
		"Health, location, app usage and battery readings go to your vault."
	};
	let options = background::Options::new(
		"Quant is collecting",
		body,
		types,
		u32::try_from(TICK_SECONDS).unwrap_or(background::DEFAULT_WAKE_INTERVAL),
	)?;
	Background::new()?.start(&options).await
}

async fn start_location(background: bool) -> Result<(), Error> {
	let options = location::UpdateOptions::new(location::UpdateAccuracy::Balanced)
		.with_interval_ms(LOCATION_INTERVAL_MS)?
		.with_distance_filter_m(LOCATION_DISTANCE_M)?
		.with_background(background)
		// iOS only. A paused update never resumes while the app is in the
		// background, so standing still would end collection until Quant is
		// opened. The blue indicator shows the owner Quant is collecting, as
		// Android's notification does.
		.with_pauses_automatically(false)
		.with_show_background_indicator(true);
	Location::new()?.start_updates(options).await
}

async fn stop(state: &State) {
	stop_location().await;
	if RUN.with(|run| run.live_workout.replace(false)) {
		state.live_workout.set(false);
		if let Ok(plugin) = Wearable::new() {
			let _ = plugin.stop_live().await;
		}
	}
	if RUN.with(|run| run.wearable.replace(false))
		&& let Ok(plugin) = Wearable::new()
	{
		let _ = plugin.stop_passive().await;
	}
	if RUN.with(|run| run.ble.replace(false)) {
		let ids = RUN.with(|run| std::mem::take(&mut *run.ble_connected.borrow_mut()));
		if let Ok(plugin) = Ble::new() {
			for id in ids {
				let _ = plugin.disconnect(&id).await;
			}
		}
	}
	if RUN.with(|run| run.background.replace(false)) {
		RUN.with(|run| run.background_types.set(0));
		if let Ok(plugin) = Background::new() {
			let _ = plugin.stop().await;
		}
	}
}

async fn stop_location() {
	let location = RUN.with(|run| {
		run.generation.set(run.generation.get().wrapping_add(1));
		run.location_background.set(false);
		run.location.replace(false)
	});
	if location && let Ok(plugin) = Location::new() {
		// The waiting loop is answered with what is buffered, then ends.
		let _ = plugin.stop_updates().await;
	}
}

/// Hand every batch of fixes to the service until updates stop.
async fn location_loop(state: State, generation: u32) {
	let current = || RUN.with(|run| run.generation.get()) == generation;
	let Ok(client) = Client::new(location::NAME) else {
		return;
	};
	while current() {
		let reply = match client.call(location::NEXT_UPDATES, &[]) {
			Ok(call) => call.await,
			Err(error) => Err(error),
		};
		match reply {
			Ok(bytes) => {
				// `None` is the stream's end. Anything else goes to the
				// service unread, including a reply it will refuse: the
				// service is what shows the owner why.
				if matches!(location::decode_updates(&bytes), Ok(None)) {
					break;
				}
				hand_over(&state, &Reading::Location(&bytes)).await;
			}
			// A loop from an earlier start is still waiting; it ends first.
			Err(Error::Busy) => {
				if sleep(Duration::from_secs(2)).await.is_err() {
					break;
				}
			}
			Err(error) => {
				if current() {
					state
						.collector
						.set(Some(format!("Location updates stopped: {error}")));
				}
				if sleep(RETRY).await.is_err() {
					break;
				}
			}
		}
	}
	if current() {
		// Updates ended on their own, as when the owner takes the permission
		// away. The next pass starts them again if it can.
		RUN.with(|run| run.location.set(false));
	}
}

/// Read the next stretch of app usage, unless a read is already under way.
fn read_usage(state: &State) {
	if state.permissions.get_untracked().usage != Grant::Granted
		|| RUN.with(|run| run.usage.replace(true))
	{
		return;
	}
	let state = state.clone();
	spawn(async move {
		if let Err(error) = usage(&state).await {
			state
				.collector
				.set(Some(format!("App usage could not be read: {error}")));
		}
		RUN.with(|run| run.usage.set(false));
	});
}

async fn usage(state: &State) -> Result<(), Error> {
	let reply = bridge::send(&Command::UsagePlan).await;
	let Some(ReplyData::UsageRange { since_ms, until_ms }) = reply.data else {
		return Ok(());
	};
	let client = Client::new(app_usage::NAME)?;
	let range = app_usage::UsageRange::new(since_ms, until_ms)?;
	let sessions = match client.call(app_usage::SESSIONS, &range.encode())?.await {
		Ok(bytes) => bytes.to_vec(),
		// iOS gives third-party apps no per-app usage; its lock and unlock
		// events still arrive below.
		Err(Error::Unavailable) => NO_SESSIONS.to_vec(),
		Err(error) => return Err(error),
	};
	// The system caps one reply, so a busy stretch arrives in pages. Each
	// page starts at the last event of the one before; the spool stores a
	// repeated event once.
	let mut pages = Vec::new();
	let mut from = since_ms;
	let mut reached = until_ms;
	while from <= until_ms {
		let mut payload = app_usage::UsageRange::new(from, until_ms)?
			.encode()
			.to_vec();
		payload.extend_from_slice(&app_usage::MAX_EVENTS.to_le_bytes());
		let bytes = client.call(app_usage::EVENTS, &payload)?.await?;
		let events = app_usage::decode_events(&bytes)?;
		let full = u32::try_from(events.len()).is_ok_and(|count| count >= app_usage::MAX_EVENTS);
		let last = events.last().map(|event| event.timestamp_ms);
		pages.push(bytes);
		let Some(last) = last.filter(|_| full) else {
			break;
		};
		if pages.len() == MAX_EVENT_PAGES {
			// More remains than one reading carries; the next read starts
			// where this one stopped.
			reached = last;
			break;
		}
		from = if last > from { last } else { from + 1 };
	}
	let reading = Reading::Usage {
		until_ms: reached,
		sessions: &sessions,
		events: pages.iter().map(|page| &page[..]).collect(),
	};
	hand_over(state, &reading).await;
	Ok(())
}

/// Report battery and thermal state now, then whenever either changes.
async fn power(state: State) {
	let battery = Battery::new().ok();
	let thermal = Thermal::new().ok();
	if let Some(plugin) = battery
		&& let Ok(snapshot) = plugin.snapshot().await
	{
		RUN.with(|run| run.battery.set(Some(snapshot)));
	}
	if let Some(plugin) = thermal
		&& let Ok(snapshot) = plugin.snapshot().await
	{
		RUN.with(|run| run.thermal.set(Some(snapshot)));
	}
	report_power(&state).await;
	if let Some(plugin) = battery {
		spawn(battery_loop(state.clone(), plugin));
	}
	if let Some(plugin) = thermal {
		spawn(thermal_loop(state, plugin));
	}
}

async fn battery_loop(state: State, plugin: Battery) {
	loop {
		let next = match RUN.with(|run| run.battery.get()) {
			Some(last) => plugin.wait_for_change(last).await,
			None => plugin.snapshot().await,
		};
		if let Ok(snapshot) = next {
			RUN.with(|run| run.battery.set(Some(snapshot)));
			report_power(&state).await;
		} else {
			// A platform that can't wait for a change is read now and then.
			// The last reading stays in place meanwhile, so the other
			// loop's reports never show this half as missing.
			if sleep(POWER_POLL).await.is_err() {
				return;
			}
			if let Ok(snapshot) = plugin.snapshot().await
				&& RUN.with(|run| run.battery.replace(Some(snapshot))) != Some(snapshot)
			{
				report_power(&state).await;
			}
		}
	}
}

async fn thermal_loop(state: State, plugin: Thermal) {
	loop {
		let next = match RUN.with(|run| run.thermal.get()) {
			Some(last) => plugin.wait_for_change(last).await,
			None => plugin.snapshot().await,
		};
		if let Ok(snapshot) = next {
			RUN.with(|run| run.thermal.set(Some(snapshot)));
			report_power(&state).await;
		} else {
			// A platform that can't wait for a change is read now and then.
			// The last reading stays in place meanwhile, so the other
			// loop's reports never show this half as missing.
			if sleep(POWER_POLL).await.is_err() {
				return;
			}
			if let Ok(snapshot) = plugin.snapshot().await
				&& RUN.with(|run| run.thermal.replace(Some(snapshot))) != Some(snapshot)
			{
				report_power(&state).await;
			}
		}
	}
}

/// The service wants the latest of both readings each time either changes.
async fn report_power(state: &State) {
	let battery = RUN
		.with(|run| run.battery.get())
		.map(battery::Snapshot::encode);
	let thermal = RUN
		.with(|run| run.thermal.get())
		.map(thermal::Snapshot::encode);
	if battery.is_none() && thermal.is_none() {
		return;
	}
	let reading = Reading::Power {
		battery: battery.as_deref(),
		thermal: thermal.as_deref(),
	};
	hand_over(state, &reading).await;
}

async fn hand_over(state: &State, reading: &Reading<'_>) {
	let Some(frame) = reading.encode() else {
		state.collector.set(Some(
			"A reading was too large to hand to the service".to_owned(),
		));
		return;
	};
	let reply = bridge::reading(&frame).await;
	if !reply.ok {
		state.collector.set(reply.error);
	}
	// On a watch whose vault is currently out of reach, ferry readings to the
	// paired phone over `watch-sync` as a companion fallback. Deterministic
	// BLAKE3 record IDs and series content hashes deduplicate cleanly when the
	// watch later reaches the vault directly.
	if WATCH
		&& !matches!(reading, Reading::WatchSync(_))
		&& matches!(
			state.snapshot.get_untracked().link,
			Link::Unreachable { .. } | Link::Unpaired
		) {
		ferry_reading_to_phone(state, &frame).await;
	}
}

/// Start on-watch passive wrist biometric monitoring when permitted (or when
/// watchOS HealthKit keeps read authorization status `Unknown`).
async fn start_wearable_if_ready(state: &State) {
	let grant = state.permissions.get_untracked().wearable;
	if !matches!(grant, Grant::Granted | Grant::Unknown) {
		return;
	}
	if RUN.with(|run| run.wearable.get()) {
		return;
	}
	let Ok(plugin) = Wearable::new() else {
		return;
	};
	let Ok(caps) = plugin.capabilities().await else {
		return;
	};
	if caps.sensors.is_empty() {
		return;
	}
	if plugin.start_passive(caps.sensors).await.is_ok() {
		RUN.with(|run| run.wearable.set(true));
		ensure_wearable_loop(state);
	}
}

/// Start or stop a live continuous wrist-sensor / workout session on a watch.
async fn toggle_live_workout(state: &State) {
	let Ok(plugin) = Wearable::new() else {
		state.problem.set(Some(
			"Wrist sensors are not available on this device".to_owned(),
		));
		return;
	};
	if RUN.with(|run| run.live_workout.get()) {
		match plugin.stop_live().await {
			Ok(()) => {
				RUN.with(|run| run.live_workout.set(false));
				state.live_workout.set(false);
			}
			Err(error) => {
				state.problem.set(Some(format!(
					"Could not stop live workout session: {error}"
				)));
			}
		}
		return;
	}
	let caps = match plugin.capabilities().await {
		Ok(caps) => caps,
		Err(error) => {
			state
				.problem
				.set(Some(format!("Could not inspect watch sensors: {error}")));
			return;
		}
	};
	if caps.sensors.is_empty() {
		state.problem.set(Some(
			"No live wrist sensors are supported on this watch".to_owned(),
		));
		return;
	}
	let options = wearable::LiveOptions {
		sensors: caps.sensors,
		interval_ms: 1_000,
		workout_activity: 0,
	};
	match plugin.start_live(options).await {
		Ok(()) => {
			RUN.with(|run| run.live_workout.set(true));
			state.live_workout.set(true);
			ensure_wearable_loop(state);
		}
		Err(error) => {
			state.problem.set(Some(format!(
				"Could not start live workout session: {error}"
			)));
		}
	}
}

fn ensure_wearable_loop(state: &State) {
	if !RUN.with(|run| run.wearable_loop_running.replace(true)) {
		spawn(wearable_loop(state.clone()));
	}
}

async fn wearable_loop(state: State) {
	let Ok(client) = Client::new(wearable::NAME) else {
		RUN.with(|run| run.wearable_loop_running.set(false));
		return;
	};
	while RUN.with(|run| run.wearable.get() || run.live_workout.get()) {
		let reply = match client.call(wearable::NEXT_BATCH, &[]) {
			Ok(call) => call.await,
			Err(error) => Err(error),
		};
		match reply {
			Ok(bytes) => {
				if !bytes.is_empty() {
					hand_over(&state, &Reading::Wearable(&bytes)).await;
				}
			}
			Err(Error::Unavailable) => {
				RUN.with(|run| {
					run.wearable.set(false);
					run.live_workout.set(false);
				});
				state.live_workout.set(false);
				break;
			}
			Err(_) => {
				if sleep(RETRY).await.is_err() {
					break;
				}
			}
		}
	}
	RUN.with(|run| run.wearable_loop_running.set(false));
}

/// Poll `HealthKit` / Health Connect / `HarmonyOS` Health for incremental
/// changes or windowed reads.
fn read_health(state: &State) {
	// Allow both Granted and Unknown (HealthKit hides read authorization
	// across restarts; Health Connect explicitly returns Needed when denied).
	let grant = state.permissions.get_untracked().health;
	if !matches!(grant, Grant::Granted | Grant::Unknown) || RUN.with(|run| run.health.replace(true))
	{
		return;
	}
	let state = state.clone();
	spawn(async move {
		if let Err(error) = health_sync_once(&state).await
			&& !matches!(error, Error::Unavailable | Error::PermissionDenied)
		{
			state
				.collector
				.set(Some(format!("Health store sync paused: {error}")));
		}
		RUN.with(|run| run.health.set(false));
	});
}

async fn health_sync_once(state: &State) -> Result<(), Error> {
	let reply = bridge::send(&Command::HealthPlan).await;
	let Some(ReplyData::HealthWindow {
		since_ms,
		until_ms,
		token,
	}) = reply.data
	else {
		return Ok(());
	};
	let client = Client::new(health::NAME)?;
	let plugin = Health::new()?;

	if let Some(saved_token) = token {
		let mut current = health::ChangesToken::new(saved_token);
		let mut expired = false;
		for _ in 0..16 {
			let request = health::changes_request(&current)?;
			let bytes = client.call(health::CHANGES, &request)?.await?;
			let decoded = health::Changes::decode(&bytes)?;
			hand_over(
				state,
				&Reading::Health {
					is_changes: true,
					payload: &bytes,
				},
			)
			.await;
			if decoded.token_expired {
				expired = true;
				break;
			}
			if !decoded.has_more {
				return Ok(());
			}
			let Some(next) = decoded.next_token else {
				return Ok(());
			};
			current = next;
		}
		if !expired {
			return Ok(());
		}
		// Token expired: fall through to capture a fresh token and backfill
		// the missed [since_ms, until_ms] window before saving it.
	}

	// Capture a fresh changes token BEFORE reading the window so samples
	// written during the windowed query are not lost; persist the token only
	// after the windowed backfill succeeds.
	let fresh_token = plugin
		.changes_token(&health::DataType::ALL)
		.await
		.ok()
		.map(|t| t.as_str().to_owned());

	pull_health_window(state, &client, since_ms, until_ms).await?;

	let _ = bridge::send(&Command::AdvanceHealthUntil { until_ms }).await;
	if let Some(fresh) = fresh_token {
		let _ = bridge::send(&Command::SetHealthToken { token: Some(fresh) }).await;
	}
	Ok(())
}

async fn pull_health_window(
	state: &State,
	client: &Client,
	since_ms: i64,
	until_ms: i64,
) -> Result<(), Error> {
	if since_ms >= until_ms {
		return Ok(());
	}
	for &data_type in &health::DataType::ALL {
		let mut page_token: Option<String> = None;
		for _ in 0..16 {
			let request =
				health::read_request(data_type, since_ms, until_ms, 200, page_token.as_deref())?;
			let Ok(call) = client.call(health::READ, &request) else {
				break;
			};
			let Ok(bytes) = call.await else {
				break;
			};
			let Ok(page) = health::Page::decode(&bytes) else {
				break;
			};
			if !page.samples.is_empty() {
				hand_over(
					state,
					&Reading::Health {
						is_changes: false,
						payload: &bytes,
					},
				)
				.await;
			}
			page_token = page.next_page_token;
			if page_token.is_none() {
				break;
			}
		}
	}
	Ok(())
}

/// Take a single bounded pedometer reading when the step counter is available.
fn read_pedometer(state: &State) {
	let state = state.clone();
	spawn(async move {
		let Ok(plugin) = Pedometer::new() else {
			return;
		};
		if !matches!(plugin.available().await, Ok(true)) {
			return;
		}
		if matches!(
			plugin.permission().await,
			Ok(pedometer::Permission::Denied | pedometer::Permission::Restricted)
		) {
			return;
		}
		let Ok(client) = Client::new(pedometer::NAME) else {
			return;
		};
		let timeout_ms: u32 = 5_000;
		if let Ok(call) = client.call(pedometer::NEXT, &timeout_ms.to_le_bytes())
			&& let Ok(bytes) = call.await
		{
			hand_over(&state, &Reading::Pedometer(&bytes)).await;
		}
	});
}

/// Listen for companion messages, durable transfers, and peer reachability
/// changes over `watch-sync`.
async fn watch_sync_loop(state: State) {
	let Ok(plugin) = WatchSync::new() else {
		return;
	};
	let Ok(status) = plugin.status().await else {
		return;
	};
	if !status.supported {
		return;
	}
	report_watch_status(&status).await;
	spawn(watch_status_loop(plugin, status));
	loop {
		match plugin.next_incoming().await {
			Ok(envelope) => {
				if !envelope.payload.is_empty() {
					hand_over(&state, &Reading::WatchSync(&envelope.payload)).await;
				}
				if !envelope.id.is_empty() {
					let _ = plugin.ack(&envelope.id).await;
				}
			}
			Err(Error::Unavailable) => return,
			Err(_) => {
				if sleep(RETRY).await.is_err() {
					return;
				}
			}
		}
	}
}

async fn watch_status_loop(plugin: WatchSync, mut last: SyncStatus) {
	loop {
		match plugin.watch_status(&last).await {
			Ok(next) => {
				report_watch_status(&next).await;
				last = next;
			}
			Err(Error::Unavailable) => return,
			Err(_) => {
				if sleep(RETRY).await.is_err() {
					return;
				}
				if let Ok(next) = plugin.status().await
					&& next != last
				{
					report_watch_status(&next).await;
					last = next;
				}
			}
		}
	}
}

async fn report_watch_status(status: &SyncStatus) {
	let _ = bridge::send(&Command::WatchPeerStatus {
		connected: status.paired && status.reachable,
		name: status.peer_name.clone(),
	})
	.await;
}

/// Push the phone's saved vault pairing link to the paired watch over
/// `watch-sync`.
async fn push_pair_link_to_watch(state: &State) {
	let reply = bridge::send(&Command::ExportPairLink).await;
	let Some(ReplyData::PairLink { link }) = reply.data else {
		state.problem.set(
			reply
				.error
				.or_else(|| Some("Connect this phone to your vault first".to_owned())),
		);
		return;
	};
	let Ok(plugin) = WatchSync::new() else {
		state.problem.set(Some(
			"Watch companion sync is not available on this device".to_owned(),
		));
		return;
	};
	let payload = serde_json::json!({
		"kind": "pair_link",
		"link": link,
	})
	.to_string();
	if let Err(error) = plugin
		.queue_transfer("quant-pair-link", TOPIC_PAIR_LINK, payload.as_bytes())
		.await
	{
		state.problem.set(Some(format!(
			"Could not send pairing link to watch: {error}"
		)));
	}
}

async fn ferry_reading_to_phone(state: &State, frame: &[u8]) {
	let Ok(plugin) = WatchSync::new() else {
		return;
	};
	let device_id = state.snapshot.get_untracked().device_id;
	let watch_name = if cfg!(feature = "watchos") {
		"Apple Watch"
	} else if cfg!(feature = "harmonyos-watch") {
		"Huawei Watch"
	} else {
		"Wear OS watch"
	};
	let payload = serde_json::json!({
		"kind": "reading_frame",
		"device_id": device_id,
		"watch_name": watch_name,
		"frame_b64": STANDARD.encode(frame),
	})
	.to_string();
	if plugin
		.send_message(TOPIC_READING_FRAME, payload.as_bytes())
		.await
		.is_err()
	{
		let seq = RUN.with(|run| {
			let next = run.ferry_seq.get().wrapping_add(1);
			run.ferry_seq.set(next);
			next
		});
		let transfer_id = format!("quant-reading-{seq}");
		let _ = plugin
			.queue_transfer(&transfer_id, TOPIC_READING_FRAME, payload.as_bytes())
			.await;
	}
}

/// Scan for nearby BLE GATT wearable sensors (heart-rate straps, pulse
/// oximeters, thermometers, battery, running cadence, cycling power) and
/// subscribe to their standard characteristics.
async fn scan_and_subscribe_ble(state: &State) {
	let Ok(plugin) = Ble::new() else {
		state.problem.set(Some(
			"Bluetooth sensors are not available on this device".to_owned(),
		));
		return;
	};
	if let Ok(ble::BleAvailability::PoweredOff) = plugin.availability().await {
		state.problem.set(Some(
			"Turn on Bluetooth to scan for straps and sensors".to_owned(),
		));
		return;
	}
	let options = ble::ScanOptions {
		service_uuids: vec![
			gatt::SERVICE_HEART_RATE.to_owned(),
			gatt::SERVICE_PULSE_OXIMETER.to_owned(),
			gatt::SERVICE_HEALTH_THERMOMETER.to_owned(),
			gatt::SERVICE_RUNNING_SPEED_CADENCE.to_owned(),
			gatt::SERVICE_CYCLING_POWER.to_owned(),
		],
		timeout_ms: 5_000,
	};
	let devices = match plugin.scan(&options).await {
		Ok(devices) => devices,
		Err(error) => {
			state
				.problem
				.set(Some(format!("Bluetooth scan failed: {error}")));
			return;
		}
	};
	if devices.is_empty() {
		state.problem.set(Some(
			"No Bluetooth heart-rate straps or fitness sensors found nearby".to_owned(),
		));
		return;
	}
	let mut subscribed_devices = 0usize;
	for dev in devices {
		if plugin.connect(&dev.id).await.is_err() {
			continue;
		}
		let mut dev_subscribed = false;
		for (service, ch) in [
			(gatt::SERVICE_HEART_RATE, gatt::CHAR_HEART_RATE_MEASUREMENT),
			(gatt::SERVICE_PULSE_OXIMETER, gatt::CHAR_PLX_CONTINUOUS),
			(gatt::SERVICE_PULSE_OXIMETER, gatt::CHAR_PLX_SPOT_CHECK),
			(
				gatt::SERVICE_HEALTH_THERMOMETER,
				gatt::CHAR_TEMPERATURE_MEASUREMENT,
			),
			(gatt::SERVICE_BATTERY, gatt::CHAR_BATTERY_LEVEL),
			(
				gatt::SERVICE_RUNNING_SPEED_CADENCE,
				gatt::CHAR_RSC_MEASUREMENT,
			),
			(
				gatt::SERVICE_CYCLING_POWER,
				gatt::CHAR_CYCLING_POWER_MEASUREMENT,
			),
		] {
			if plugin
				.subscribe(&ble::Subscription {
					device_id: &dev.id,
					service_uuid: service,
					characteristic_uuid: ch,
				})
				.await
				.is_ok()
			{
				dev_subscribed = true;
			}
		}
		if dev_subscribed {
			subscribed_devices += 1;
			RUN.with(|run| {
				let mut connected = run.ble_connected.borrow_mut();
				if !connected.contains(&dev.id) {
					connected.push(dev.id.clone());
				}
			});
		} else {
			let _ = plugin.disconnect(&dev.id).await;
		}
	}
	if subscribed_devices == 0 {
		state.problem.set(Some(
			"Found Bluetooth devices, but none offered supported health sensors".to_owned(),
		));
		return;
	}
	if !RUN.with(|run| run.ble.replace(true)) {
		spawn(ble_loop(state.clone()));
	}
}

async fn ble_loop(state: State) {
	let Ok(client) = Client::new(ble::NAME) else {
		RUN.with(|run| run.ble.set(false));
		return;
	};
	while RUN.with(|run| run.ble.get()) {
		let reply = match client.call(ble::NEXT_NOTIFICATIONS, &[]) {
			Ok(call) => call.await,
			Err(error) => Err(error),
		};
		match reply {
			Ok(bytes) => {
				if !bytes.is_empty() {
					hand_over(&state, &Reading::Ble(&bytes)).await;
				}
			}
			Err(Error::Unavailable) => {
				RUN.with(|run| run.ble.set(false));
				return;
			}
			Err(_) => {
				if sleep(RETRY).await.is_err() {
					return;
				}
			}
		}
	}
}
