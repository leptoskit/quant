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
use std::time::Duration;

use leptoskit::guest::sleep;
use leptoskit::guest::spawn;
use leptoskit::prelude::GetUntracked;
use leptoskit::prelude::Set;
use leptoskit_bridge::Error;
use leptoskit_bridge::guest::Client;
use leptoskit_plugin_app_usage as app_usage;
use leptoskit_plugin_app_usage::guest::AppUsage;
use leptoskit_plugin_background as background;
use leptoskit_plugin_background::guest::Background;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_battery::guest::Battery;
use leptoskit_plugin_location as location;
use leptoskit_plugin_location::guest::Location;
use leptoskit_plugin_notifications as notifications;
use leptoskit_plugin_notifications::guest::Notifications;
use leptoskit_plugin_secure_store::Key;
use leptoskit_plugin_secure_store::guest::SecureStore;
use leptoskit_plugin_thermal as thermal;
use leptoskit_plugin_thermal::guest::Thermal;
use quant_model::phone::Command;
use quant_model::phone::MAX_EVENT_PAGES;
use quant_model::phone::Reading;
use quant_model::phone::ReplyData;
use quant_model::phone::Storage;
use quant_model::phone::TICK_SECONDS;
use quant_model::phone::USAGE_SECONDS;

use super::Action;
use super::Grant;
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
/// How often battery and thermal are read when the plugin can't wait for a
/// change.
const POWER_POLL: Duration = Duration::from_mins(5);
/// An app usage reply holding nothing: a count of zero.
const NO_SESSIONS: [u8; 4] = [0; 4];
/// Whether this is the iPhone app, which collects in the background through
/// the "Always" location grant instead of a foreground service.
const IOS: bool = cfg!(feature = "ios");

thread_local! {
	/// What the collector has running. The guest is single-threaded.
	static RUN: Running = const { Running::new() };
}

struct Running {
	/// Whether the background service was started.
	background: Cell<bool>,
	/// Whether location updates were started.
	location: Cell<bool>,
	/// Whether the running location updates continue in the background.
	location_background: Cell<bool>,
	/// Bumped whenever location updates start or stop, so a loop left over
	/// from an earlier start knows to end.
	generation: Cell<u32>,
	/// Whether an app usage read is under way.
	usage: Cell<bool>,
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
			location: Cell::new(false),
			location_background: Cell::new(false),
			generation: Cell::new(0),
			usage: Cell::new(false),
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
		}
	});
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
	read_usage(&state);
	let every = (USAGE_SECONDS / TICK_SECONDS).max(1);
	let mut ticks: u64 = 0;
	loop {
		if sleep(Duration::from_secs(TICK_SECONDS)).await.is_err() {
			// Without a timer the background wake still delivers; only the
			// periodic app usage read is lost.
			return;
		}
		ticks += 1;
		bridge::send(&Command::Tick).await;
		// The owner may have changed a permission in the system's settings.
		refresh(&state).await;
		sync(&state);
		if ticks.is_multiple_of(every) {
			read_usage(&state);
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
		stop().await;
		state.collector.set(None);
		return;
	}
	if snapshot.storage != Storage::Open {
		// Nothing can be queued yet; the storage card says why.
		return;
	}
	match state.permissions.get_untracked().location {
		Grant::Granted => {}
		Grant::Unavailable => {
			state.collector.set(None);
			return;
		}
		// Not asked the system yet; the next pass knows.
		Grant::Unknown => return,
		Grant::Needed => {
			stop().await;
			state.collector.set(Some(
				"Allow location below to collect in the background. Until then this phone \
				 collects only while Quant is open."
					.to_owned(),
			));
			return;
		}
	}
	let mut problem = None;
	// The background service comes first: Android refuses background
	// location updates unless a location-typed service is already running.
	// On iOS it schedules app refresh, which nothing else waits for, so a
	// refusal there only means fewer wakes and is tried again next time.
	if !RUN.with(|run| run.background.get()) {
		match start_background().await {
			Ok(()) => RUN.with(|run| run.background.set(true)),
			Err(_) if IOS => {}
			Err(error) => {
				problem = Some(format!(
					"Quant can't keep collecting in the background on this phone: {error}"
				));
			}
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

/// Whether location updates should continue in the background. Android
/// needs the background service running. iOS needs the "Always" grant, which
/// nothing can read without a prompt, so an unknown grant is tried and the
/// start reveals it.
fn background_wanted(state: &State) -> bool {
	if IOS {
		state.permissions.get_untracked().always != Grant::Needed
	} else {
		RUN.with(|run| run.background.get())
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

async fn start_background() -> Result<(), Error> {
	let options = background::Options::new(
		"Quant is collecting",
		"Location, app usage and battery readings go to your vault.",
		background::ForegroundServiceTypes::LOCATION,
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

async fn stop() {
	stop_location().await;
	if RUN.with(|run| run.background.replace(false))
		&& let Ok(plugin) = Background::new()
	{
		let _ = plugin.stop().await;
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
}
