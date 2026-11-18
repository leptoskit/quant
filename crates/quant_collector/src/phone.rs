//! Adapters from the Leptoskit phone plugins to Quant streams.
//!
//! The mobile host reads the location, app usage, battery and thermal
//! plugins and hands what they return to these methods, which turn each
//! reading into the stream it belongs to and queue it in the spool:
//!
//! | Plugin                       | Stream                         | Write scope      |
//! | ---------------------------- | ------------------------------ | ---------------- |
//! | location `next_updates`      | `location.fix` series          | `location:write` |
//! | app usage `sessions`         | `apps.focus` spans             | `apps:write`     |
//! | app usage `events`           | `device.state` transitions     | `device:write`   |
//! | battery and thermal snapshots | `device.battery`, `device.power` | `device:write` |
//!
//! Every record id is derived from the device and the reading itself, and
//! series batches are stored by content hash, so a host that hands the same
//! reading over twice (after a crash, or an overlapping query) stores it once.

use jiff::Timestamp;
use leptoskit_plugin_app_usage::EventKind;
use leptoskit_plugin_app_usage::UsageEvent;
use leptoskit_plugin_app_usage::UsageSession;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_location::Fix;
use leptoskit_plugin_location::Updates;
use leptoskit_plugin_thermal as thermal;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::SinkError;
use quant_core::StreamId;
use serde::Deserialize;
use serde::Serialize;

use crate::Spool;
use crate::UploadBudget;

/// Rows per `location.fix` batch: four channels stay well under the 16,384
/// value limit, and a batch never spans more than a day.
const MAX_FIX_ROWS: usize = 2_048;
const MAX_BATCH_SPAN_MS: i64 = 24 * 60 * 60 * 1000;
/// The `source` attribute on everything the phone collector writes.
const SOURCE: &str = "phone";

fn stream(name: &str) -> StreamId {
	name.parse().expect("built-in stream")
}

fn timestamp(ms: i64) -> Result<Timestamp, SinkError> {
	Timestamp::from_millisecond(ms).map_err(SinkError::from_display)
}

/// A stable record id for one reading, so replays store it once.
fn record_id(kind: &str, device: DeviceId, parts: &[&[u8]]) -> RecordId {
	let mut hash = blake3::Hasher::new_derive_key("quant phone record v1");
	hash.update(kind.as_bytes());
	hash.update(device.0.as_bytes());
	for part in parts {
		hash.update(&(part.len() as u64).to_le_bytes());
		hash.update(part);
	}
	hash.finalize().to_hex()[..32]
		.parse()
		.expect("hash is a UUID-sized hex string")
}

/// What one batch of location updates stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationReport {
	/// Fixes queued as `location.fix` rows.
	pub stored: usize,
	/// Fixes the platform marked as simulated (mock locations), not stored.
	pub simulated: usize,
	/// Fixes with impossible coordinates or times, not stored.
	pub invalid: usize,
	/// Fixes the plugin's bounded native buffer discarded before the host
	/// read them.
	pub dropped: u32,
}

impl Spool {
	/// Queue one `next_updates` batch from the location plugin as
	/// `location.fix` rows (latitude, longitude, horizontal accuracy and,
	/// when known, altitude). Visits and significant changes arrive through
	/// the same batches and are stored the same way.
	///
	/// Mock locations and invalid fixes are skipped. Fixes the native
	/// buffer dropped are reported on the `location` source status, so the
	/// owner can see the phone missed some.
	pub fn location_updates(
		&self,
		device: DeviceId,
		updates: &Updates,
	) -> Result<LocationReport, SinkError> {
		let mut report = LocationReport {
			dropped: updates.dropped,
			..LocationReport::default()
		};
		let mut fixes: Vec<(i64, Fix)> = Vec::with_capacity(updates.fixes.len());
		for fix in &updates.fixes {
			if fix.simulated == Some(true) {
				report.simulated += 1;
				continue;
			}
			let Ok(fix) = fix.validate() else {
				report.invalid += 1;
				continue;
			};
			#[allow(clippy::cast_possible_truncation)]
			let at = fix.timestamp_ms.round() as i64;
			if !fix.timestamp_ms.is_finite() || at <= 0 || timestamp(at).is_err() {
				report.invalid += 1;
				continue;
			}
			fixes.push((at, fix));
		}
		fixes.sort_by_key(|(at, _)| *at);

		// Batches name their channels, so a run of fixes without altitude
		// leaves the column out rather than sending a placeholder value.
		let mut run: Vec<(i64, Fix)> = Vec::new();
		for (at, fix) in fixes {
			let breaks = run.first().is_some_and(|(first, head)| {
				head.altitude_m.is_some() != fix.altitude_m.is_some()
					|| at - first > MAX_BATCH_SPAN_MS
					|| run.len() >= MAX_FIX_ROWS
			});
			if breaks {
				report.stored += self.fix_batch(device, &run)?;
				run.clear();
			}
			run.push((at, fix));
		}
		report.stored += self.fix_batch(device, &run)?;

		if report.dropped > 0 {
			self.status(
				"location",
				true,
				Some(format!(
					"the phone dropped {} location fixes before they could be saved",
					report.dropped
				)),
			);
		}
		Ok(report)
	}

	fn fix_batch(&self, device: DeviceId, run: &[(i64, Fix)]) -> Result<usize, SinkError> {
		let Some((first, head)) = run.first() else {
			return Ok(0);
		};
		let altitude = head.altitude_m.is_some();
		let mut channels = vec!["lat".to_owned(), "lon".to_owned(), "accuracy".to_owned()];
		if altitude {
			channels.push("altitude".into());
		}
		let mut offsets_ms = Vec::with_capacity(run.len());
		let mut values = Vec::with_capacity(run.len() * channels.len());
		for (at, fix) in run {
			offsets_ms.push(u32::try_from(at - first).map_err(SinkError::from_display)?);
			values.extend([fix.latitude, fix.longitude, fix.accuracy_m]);
			if let Some(metres) = fix.altitude_m {
				values.push(metres);
			}
		}
		self.series(SeriesBatch {
			stream: stream("location.fix"),
			device,
			start: timestamp(*first)?,
			channels,
			offsets_ms,
			values,
		})?;
		Ok(run.len())
	}

	/// Queue foreground sessions from the app usage plugin as `apps.focus`
	/// spans, carrying the app's label as `app` (as desktop focus spans do)
	/// and its package as `bundle_id`.
	///
	/// Sessions are clipped to the queried range, so a host should query
	/// contiguous ranges from a watermark: re-reading a range returns the
	/// same spans and stores nothing new. Empty sessions are skipped.
	/// Returns how many spans were queued.
	pub fn app_sessions(
		&self,
		device: DeviceId,
		sessions: &[UsageSession],
	) -> Result<usize, SinkError> {
		let mut stored = 0;
		for session in sessions {
			if session.package.is_empty() || session.end_ms <= session.start_ms {
				continue;
			}
			let label = session.label.trim();
			let app = if label.is_empty() {
				session.package.as_str()
			} else {
				label
			};
			let mut record = Record::new(stream("apps.focus"), device)
				.starting(timestamp(session.start_ms)?)
				.ending(timestamp(session.end_ms)?)
				.with("app", app)
				.with("bundle_id", session.package.as_str())
				.with("source", SOURCE);
			record.id = record_id(
				"apps.focus",
				device,
				&[
					session.package.as_bytes(),
					&session.start_ms.to_le_bytes(),
					&session.end_ms.to_le_bytes(),
				],
			);
			self.record(record)?;
			stored += 1;
		}
		Ok(stored)
	}

	/// Queue screen, lock and power transitions from the app usage plugin
	/// as `device.state` records, with the same state names the desktop
	/// uses (`display_on`, `display_off`, `locked`, `unlocked`) plus
	/// `startup` and `shutdown`. App foreground events are left out: the
	/// sessions already cover them. Returns how many records were queued.
	pub fn usage_events(
		&self,
		device: DeviceId,
		events: &[UsageEvent],
	) -> Result<usize, SinkError> {
		let mut stored = 0;
		for event in events {
			let Some(state) = device_state(event.kind) else {
				continue;
			};
			let mut record = Record::new(stream("device.state"), device)
				.starting(timestamp(event.timestamp_ms)?)
				.with("state", state)
				.with("source", SOURCE);
			record.id = record_id(
				"device.state",
				device,
				&[state.as_bytes(), &event.timestamp_ms.to_le_bytes()],
			);
			self.record(record)?;
			stored += 1;
		}
		Ok(stored)
	}
}

fn device_state(kind: EventKind) -> Option<&'static str> {
	match kind {
		EventKind::ActivityResumed | EventKind::ActivityPaused => None,
		EventKind::ScreenInteractive => Some("display_on"),
		EventKind::ScreenNonInteractive => Some("display_off"),
		EventKind::KeyguardShown | EventKind::Locked => Some("locked"),
		EventKind::KeyguardHidden | EventKind::Unlocked => Some("unlocked"),
		EventKind::DeviceStartup => Some("startup"),
		EventKind::DeviceShutdown => Some("shutdown"),
	}
}

fn charging_name(state: battery::State) -> &'static str {
	match state {
		battery::State::Unknown => "unknown",
		battery::State::Discharging => "discharging",
		battery::State::Charging => "charging",
		battery::State::Full => "full",
		battery::State::NotCharging => "not_charging",
	}
}

fn thermal_name(state: thermal::State) -> &'static str {
	match state {
		thermal::State::Unknown => "unknown",
		thermal::State::Nominal => "nominal",
		thermal::State::Fair => "fair",
		thermal::State::Serious => "serious",
		thermal::State::Critical => "critical",
	}
}

fn android_thermal_name(status: thermal::AndroidStatus) -> &'static str {
	match status {
		thermal::AndroidStatus::None => "none",
		thermal::AndroidStatus::Light => "light",
		thermal::AndroidStatus::Moderate => "moderate",
		thermal::AndroidStatus::Severe => "severe",
		thermal::AndroidStatus::Critical => "critical",
		thermal::AndroidStatus::Emergency => "emergency",
		thermal::AndroidStatus::Shutdown => "shutdown",
	}
}

/// The power state that a `device.power` record describes. Battery level
/// alone goes to `device.battery`; a change here is a new record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PowerState {
	charging: battery::State,
	low_power: Option<bool>,
	thermal: thermal::State,
	android_thermal: Option<thermal::AndroidStatus>,
}

/// What one power reading queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerReport {
	/// A `device.battery` row was queued.
	pub level: bool,
	/// The charging, low-power or thermal state changed, and a
	/// `device.power` record was queued.
	pub changed: bool,
}

/// Turns battery and thermal plugin snapshots into `device.battery` levels
/// and `device.power` state changes, and into the upload budget the phone
/// can afford right now.
///
/// The host keeps one monitor alive while collecting, and calls
/// [`Self::observe`] with the latest snapshot of both plugins each time
/// either `wait_for_change` returns. Unchanged readings store nothing.
#[derive(Debug, Default)]
pub struct PowerMonitor {
	battery: Option<battery::Snapshot>,
	thermal: Option<thermal::Snapshot>,
	level: Option<u16>,
	state: Option<PowerState>,
}

impl PowerMonitor {
	/// A monitor that has seen nothing yet; its first reading is always
	/// stored.
	pub fn new() -> Self {
		Self::default()
	}

	/// Record a reading taken at `at`. Either snapshot may be missing when
	/// its plugin is unavailable (the thermal plugin needs Android 10).
	pub fn observe(
		&mut self,
		spool: &Spool,
		device: DeviceId,
		at: Timestamp,
		battery: Option<battery::Snapshot>,
		thermal: Option<thermal::Snapshot>,
	) -> Result<PowerReport, SinkError> {
		let mut report = PowerReport::default();
		let charge = battery.and_then(|snapshot| snapshot.charge);
		let level = charge.map(battery::Charge::thousandths);
		if let Some(thousandths) = level
			&& self.level != Some(thousandths)
		{
			spool.series(SeriesBatch {
				stream: stream("device.battery"),
				device,
				start: at,
				channels: vec!["level".into()],
				offsets_ms: vec![0],
				values: vec![f64::from(thousandths) / 10.0],
			})?;
			report.level = true;
		}

		let state = PowerState {
			charging: battery.map_or(battery::State::Unknown, |snapshot| snapshot.state),
			low_power: battery.and_then(|snapshot| snapshot.power_save),
			thermal: thermal.map_or(thermal::State::Unknown, |snapshot| snapshot.state),
			android_thermal: thermal.and_then(|snapshot| snapshot.android_status),
		};
		if self.state != Some(state) {
			spool.record(power_record(device, at, state, level))?;
			report.changed = true;
		}

		self.battery = battery;
		self.thermal = thermal;
		if level.is_some() {
			self.level = level;
		}
		self.state = Some(state);
		Ok(report)
	}

	/// How much upload work the phone can afford in its latest state.
	pub fn budget(&self) -> UploadBudget {
		UploadBudget::for_power(self.battery, self.thermal)
	}
}

fn power_record(device: DeviceId, at: Timestamp, state: PowerState, level: Option<u16>) -> Record {
	let charging = charging_name(state.charging);
	let thermal = thermal_name(state.thermal);
	let mut text = match level {
		Some(thousandths) => format!("Phone battery {}%", (thousandths + 5) / 10),
		None => "Phone battery level unknown".to_owned(),
	};
	text.push_str(match state.charging {
		battery::State::Unknown => "",
		battery::State::Discharging => ", on battery",
		battery::State::Charging => ", charging",
		battery::State::Full => ", fully charged",
		battery::State::NotCharging => ", plugged in but not charging",
	});
	match state.low_power {
		Some(true) => text.push_str(", low power mode on"),
		Some(false) => text.push_str(", low power mode off"),
		None => {}
	}
	if state.thermal != thermal::State::Unknown {
		text.push_str(", thermal state ");
		text.push_str(thermal);
	}
	text.push('.');

	let mut record = Record::new(stream("device.power"), device)
		.starting(at)
		.with_text(text)
		.with("charging", charging)
		.with("thermal", thermal)
		.with("source", SOURCE);
	if let Some(low_power) = state.low_power {
		record = record.with("low_power", low_power);
	}
	if let Some(status) = state.android_thermal {
		record = record.with("android_thermal", android_thermal_name(status));
	}
	if let Some(thousandths) = level {
		record = record.with("level", f64::from(thousandths) / 10.0);
	}
	record.id = record_id(
		"device.power",
		device,
		&[
			&at.as_millisecond().to_le_bytes(),
			charging.as_bytes(),
			thermal.as_bytes(),
			&[match state.low_power {
				None => 0,
				Some(false) => 1,
				Some(true) => 2,
			}],
		],
	);
	record
}

impl UploadBudget {
	/// The budget a phone can afford given its battery and thermal state.
	///
	/// - Serious or critical thermal state: nothing uploads; the system is
	///   already throttling and the next pass waits for it to cool.
	/// - Fair thermal state, low-power mode, or under 20% and not on external
	///   power: a quarter of the default work, in at most four seconds.
	/// - Otherwise the default budget.
	pub fn for_power(
		battery: Option<battery::Snapshot>,
		thermal: Option<thermal::Snapshot>,
	) -> Self {
		let full = Self::default();
		let thermal = thermal.map_or(thermal::State::Unknown, |snapshot| snapshot.state);
		if thermal.is_throttled() {
			return Self {
				max_records: 0,
				max_batches: 0,
				max_media_bytes: 0,
				max_duration: std::time::Duration::ZERO,
			};
		}
		let low_power = battery.and_then(|snapshot| snapshot.power_save) == Some(true);
		let on_power = battery.is_some_and(|snapshot| {
			matches!(
				snapshot.state,
				battery::State::Charging | battery::State::Full | battery::State::NotCharging
			)
		});
		let low_battery = !on_power
			&& battery
				.and_then(|snapshot| snapshot.charge)
				.is_some_and(|charge| charge.thousandths() < 200);
		if thermal == thermal::State::Fair || low_power || low_battery {
			return Self {
				max_records: full.max_records / 4,
				max_batches: full.max_batches / 4,
				max_media_bytes: full.max_media_bytes / 4,
				max_duration: std::time::Duration::from_secs(4),
			};
		}
		full
	}
}
