//! Adapters from Leptoskit wearable, health-store, pedometer, motion, and BLE
//! GATT plugins to Quant streams.
//!
//! Every record id is derived deterministically via keyed BLAKE3, and series
//! batches are stored by content hash, so overlapping queries, crash replays,
//! or duplicate companion deliveries store each measurement once.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::SignedDuration;
use jiff::Timestamp;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_ble::gatt;
use leptoskit_plugin_health as health;
use leptoskit_plugin_pedometer as pedometer;
use leptoskit_plugin_wearable as wearable;
use leptoskit_sensor as sensor;
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

/// Companion `watch-sync` topic for forwarding a vault pairing link from phone
/// to watch.
pub const TOPIC_PAIR_LINK: &str = "/quant/pair";
/// Companion `watch-sync` topic for ferrying a binary `Reading` frame from
/// watch to phone when the watch is offline.
pub const TOPIC_READING_FRAME: &str = "/quant/reading";

/// Maximum rows per single-channel series batch.
const MAX_SERIES_ROWS: usize = 2_048;
/// Maximum rows per 3-channel vector motion batch (`4096 * 3 = 12288 <=
/// 16384`).
const MAX_VECTOR_ROWS: usize = 4_096;
/// Maximum rows per 2-channel BLE pair batch (`2048 * 2 = 4096 <= 16384`).
const MAX_PAIR_ROWS: usize = 2_048;
/// Maximum time span of a single series batch (24 hours).
const MAX_BATCH_SPAN_MS: i64 = 24 * 60 * 60 * 1000;

fn stream(name: &str) -> StreamId {
	name.parse().expect("built-in or valid wearable stream")
}

fn timestamp(ms: i64) -> Result<Timestamp, SinkError> {
	Timestamp::from_millisecond(ms).map_err(SinkError::from_display)
}

fn record_id(kind: &str, device: DeviceId, parts: &[&[u8]]) -> RecordId {
	let mut hash = blake3::Hasher::new_derive_key("quant wearable record v1");
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

/// Companion phone ↔ watch envelope carried over `watch-sync`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanionEnvelope {
	/// Pairing link pushed from the phone to the watch so the watch can pair
	/// directly with the owner's vault without typing on the wrist.
	PairLink {
		/// `quant://link?...` URI.
		link: String,
	},
	/// Binary `quant_model::phone::Reading` frame ferried through the phone
	/// when the watch has no direct Wi-Fi/LTE route to the vault.
	ReadingFrame {
		/// Stable device id of the originating watch, so ferried readings are
		/// attributed to the watch rather than the phone.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		device_id: Option<DeviceId>,
		/// Display name of the originating watch.
		watch_name: String,
		/// Compact base64-encoded `Reading` frame.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		frame_b64: Option<String>,
		/// Legacy JSON byte array encoding of the `Reading` frame, kept for
		/// backwards compatibility with older watch builds.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		frame: Vec<u8>,
	},
}

impl CompanionEnvelope {
	/// Build a compact base64-encoded [`CompanionEnvelope::ReadingFrame`].
	pub fn reading_frame(
		device_id: Option<DeviceId>,
		watch_name: impl Into<String>,
		frame: &[u8],
	) -> Self {
		Self::ReadingFrame {
			device_id,
			watch_name: watch_name.into(),
			frame_b64: Some(STANDARD.encode(frame)),
			frame: Vec::new(),
		}
	}

	/// Decode the inner `Reading` bytes from either `frame_b64` or legacy
	/// `frame`.
	pub fn decoded_frame(&self) -> Option<Vec<u8>> {
		match self {
			Self::PairLink { .. } => None,
			Self::ReadingFrame {
				frame_b64, frame, ..
			} => {
				if let Some(encoded) = frame_b64 {
					STANDARD.decode(encoded).ok()
				} else if !frame.is_empty() {
					Some(frame.clone())
				} else {
					None
				}
			}
		}
	}
}

/// What one batch of `HealthKit` / Health Connect / `HarmonyOS` Health samples
/// queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
	/// Records or series points queued into the spool.
	pub stored: usize,
	/// Invalid or empty samples skipped.
	pub skipped: usize,
}

/// What one direct on-watch sensor batch queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WearableReport {
	/// Readings queued into the spool.
	pub stored: usize,
	/// Readings discarded by the watch's native ring buffer before draining.
	pub dropped: u32,
	/// Latest valid heart rate in BPM observed in this batch.
	pub latest_heart_rate_bpm: Option<u16>,
	/// Steps counted across `Steps` readings in this batch.
	pub steps_delta: u64,
}

/// What one BLE GATT notification batch queued.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BleReport {
	/// Decoded sensor measurements queued into the spool.
	pub stored: usize,
	/// Unrecognized or malformed notifications skipped.
	pub skipped: usize,
	/// Notifications dropped by the native buffer before draining.
	pub dropped: u32,
	/// Latest valid heart rate in BPM observed in this batch.
	pub latest_heart_rate_bpm: Option<u16>,
}

impl Spool {
	/// Queue samples from `leptoskit_plugin_health` (`HealthKit`, Health
	/// Connect, or `HarmonyOS` Health Service Kit) into their Quant `health.*`
	/// streams.
	pub fn health_samples(
		&self,
		device: DeviceId,
		samples: &[health::Sample],
	) -> Result<HealthReport, SinkError> {
		let mut report = HealthReport::default();
		let mut hr_points: Vec<(i64, f64)> = Vec::new();
		let mut rhr_points: Vec<(i64, f64)> = Vec::new();
		let mut hrv_rmssd_points: Vec<(i64, f64)> = Vec::new();
		let mut hrv_sdnn_points: Vec<(i64, f64)> = Vec::new();
		let mut step_points: Vec<(i64, f64)> = Vec::new();
		let mut spo2_points: Vec<(i64, f64)> = Vec::new();
		let mut resp_points: Vec<(i64, f64)> = Vec::new();
		let mut temp_points: Vec<(i64, f64)> = Vec::new();
		let mut cal_points: Vec<(i64, f64)> = Vec::new();
		let mut dist_points: Vec<(i64, f64)> = Vec::new();
		let mut floor_points: Vec<(i64, f64)> = Vec::new();

		for sample in samples {
			if sample.start_ms <= 0 || sample.end_ms < sample.start_ms {
				report.skipped += 1;
				continue;
			}
			match &sample.value {
				health::Value::HeartRate(points) => {
					for pt in points {
						if pt.time_ms > 0 && pt.bpm.is_finite() && pt.bpm > 0.0 {
							hr_points.push((pt.time_ms, pt.bpm));
						} else {
							report.skipped += 1;
						}
					}
				}
				health::Value::RestingHeartRate { bpm } => {
					if bpm.is_finite() && *bpm > 0.0 {
						rhr_points.push((sample.start_ms, *bpm));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::HeartRateVariabilityRmssd { milliseconds } => {
					if milliseconds.is_finite() && *milliseconds >= 0.0 {
						hrv_rmssd_points.push((sample.start_ms, *milliseconds));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::HeartRateVariabilitySdnn { milliseconds } => {
					if milliseconds.is_finite() && *milliseconds >= 0.0 {
						hrv_sdnn_points.push((sample.start_ms, *milliseconds));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::Steps { count } => {
					step_points.push((sample.start_ms, *count as f64));
				}
				health::Value::OxygenSaturation { percent } => {
					if percent.is_finite() && (0.0..=100.0).contains(percent) {
						spo2_points.push((sample.start_ms, *percent));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::RespiratoryRate { breaths_per_minute } => {
					if breaths_per_minute.is_finite() && *breaths_per_minute > 0.0 {
						resp_points.push((sample.start_ms, *breaths_per_minute));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::SkinTemperature { celsius } => {
					if celsius.is_finite() {
						temp_points.push((sample.start_ms, *celsius));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::ActiveCalories { kilocalories } => {
					if kilocalories.is_finite() && *kilocalories >= 0.0 {
						cal_points.push((sample.start_ms, *kilocalories));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::Distance { meters } => {
					if meters.is_finite() && *meters >= 0.0 {
						dist_points.push((sample.start_ms, *meters));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::FloorsClimbed { floors } => {
					if floors.is_finite() && *floors >= 0.0 {
						floor_points.push((sample.start_ms, *floors));
					} else {
						report.skipped += 1;
					}
				}
				health::Value::Sleep { stages } => {
					if stages.is_empty() {
						self.queue_sleep_stage(
							device,
							sample.start_ms,
							sample.end_ms,
							health::Stage::Asleep,
							&sample.source,
						)?;
						report.stored += 1;
					} else {
						for st in stages {
							if st.start_ms <= 0 || st.end_ms < st.start_ms {
								report.skipped += 1;
								continue;
							}
							self.queue_sleep_stage(
								device,
								st.start_ms,
								st.end_ms,
								st.stage,
								&sample.source,
							)?;
							report.stored += 1;
						}
					}
				}
				health::Value::Exercise {
					activity,
					platform_code,
					title,
				} => {
					let activity_name = activity_str(*activity);
					let mut record = Record::new(stream("health.workout"), device)
						.starting(timestamp(sample.start_ms)?)
						.ending(timestamp(sample.end_ms)?)
						.with("activity", activity_name)
						.with("platform_code", *platform_code)
						.with("source", "health_store");
					if let Some(title) = title.as_deref().filter(|t| !t.trim().is_empty()) {
						record = record.with_text(title.trim());
					}
					record = attach_health_source(record, &sample.source);
					record.id = record_id(
						"health.workout",
						device,
						&[
							activity_name.as_bytes(),
							&sample.start_ms.to_le_bytes(),
							&sample.end_ms.to_le_bytes(),
							&platform_code.to_le_bytes(),
						],
					);
					self.record(record)?;
					report.stored += 1;
				}
			}
		}

		report.stored += self.queue_scalar_series(device, "health.heart_rate", "bpm", hr_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.resting_heart_rate", "bpm", rhr_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.hrv", "rmssd", hrv_rmssd_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.hrv_sdnn", "sdnn", hrv_sdnn_points)?;
		report.stored += self.queue_scalar_series(device, "health.steps", "steps", step_points)?;
		report.stored += self.queue_scalar_series(device, "health.spo2", "percent", spo2_points)?;
		report.stored += self.queue_scalar_series(
			device,
			"health.respiratory_rate",
			"breaths_per_min",
			resp_points,
		)?;
		report.stored +=
			self.queue_scalar_series(device, "health.skin_temperature", "celsius", temp_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.active_calories", "kcal", cal_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.distance", "meters", dist_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.floors", "floors", floor_points)?;

		Ok(report)
	}

	/// Queue one direct on-watch sensor batch from `leptoskit_plugin_wearable`
	/// (Wear OS Health Services, Samsung Galaxy Watch sensors, watchOS live
	/// workouts, or Huawei Watch Sensor Kit).
	pub fn wearable_batch(
		&self,
		device: DeviceId,
		batch: &wearable::Batch,
	) -> Result<WearableReport, SinkError> {
		let mut report = WearableReport {
			dropped: batch.dropped,
			..WearableReport::default()
		};
		let mut hr_points: Vec<(i64, f64)> = Vec::new();
		let mut hrv_points: Vec<(i64, f64)> = Vec::new();
		let mut spo2_points: Vec<(i64, f64)> = Vec::new();
		let mut temp_points: Vec<(i64, f64)> = Vec::new();
		let mut step_points: Vec<(i64, f64)> = Vec::new();
		let mut floor_points: Vec<(i64, f64)> = Vec::new();
		let mut cal_points: Vec<(i64, f64)> = Vec::new();
		let mut dist_points: Vec<(i64, f64)> = Vec::new();
		let mut stress_points: Vec<(i64, f64)> = Vec::new();

		for reading in &batch.readings {
			match reading {
				wearable::Reading::HeartRate {
					time_ms,
					bpm,
					accuracy,
				} => {
					if *accuracy != wearable::Accuracy::NoContact
						&& *time_ms > 0
						&& bpm.is_finite()
						&& *bpm > 0.0
					{
						hr_points.push((*time_ms, *bpm));
						#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
						let rounded = bpm.round().clamp(1.0, f64::from(u16::MAX)) as u16;
						report.latest_heart_rate_bpm = Some(rounded);
					}
				}
				wearable::Reading::HrvIbi {
					time_ms,
					rmssd_ms,
					rr_intervals_ms,
				} => {
					let rmssd = rmssd_ms.or_else(|| rmssd_from_rr(rr_intervals_ms));
					if let Some(val) = rmssd
						&& *time_ms > 0
						&& val.is_finite()
						&& val >= 0.0
					{
						hrv_points.push((*time_ms, val));
					}
				}
				wearable::Reading::SpO2 { time_ms, percent } => {
					if *time_ms > 0 && percent.is_finite() && (0.0..=100.0).contains(percent) {
						spo2_points.push((*time_ms, *percent));
					}
				}
				wearable::Reading::SkinTemperature { time_ms, celsius } => {
					if *time_ms > 0 && celsius.is_finite() {
						temp_points.push((*time_ms, *celsius));
					}
				}
				wearable::Reading::Steps {
					start_ms, count, ..
				} => {
					if *start_ms > 0 {
						step_points.push((*start_ms, *count as f64));
						report.steps_delta = report.steps_delta.saturating_add(*count);
					}
				}
				wearable::Reading::Floors {
					start_ms, floors, ..
				} => {
					if *start_ms > 0 && floors.is_finite() && *floors >= 0.0 {
						floor_points.push((*start_ms, *floors));
					}
				}
				wearable::Reading::Calories {
					start_ms,
					kilocalories,
					..
				} => {
					if *start_ms > 0 && kilocalories.is_finite() && *kilocalories >= 0.0 {
						cal_points.push((*start_ms, *kilocalories));
					}
				}
				wearable::Reading::Distance {
					start_ms, meters, ..
				} => {
					if *start_ms > 0 && meters.is_finite() && *meters >= 0.0 {
						dist_points.push((*start_ms, *meters));
					}
				}
				wearable::Reading::WearState { time_ms, on_wrist } => {
					if *time_ms > 0 {
						let state = if *on_wrist { "on_wrist" } else { "off_wrist" };
						let mut record = Record::new(stream("device.wear_state"), device)
							.starting(timestamp(*time_ms)?)
							.with("state", state)
							.with("source", "wearable");
						record.id = record_id(
							"device.wear_state",
							device,
							&[state.as_bytes(), &time_ms.to_le_bytes()],
						);
						self.record(record)?;
						report.stored += 1;
					}
				}
				wearable::Reading::Stress { time_ms, score } => {
					if *time_ms > 0 && score.is_finite() && *score >= 0.0 {
						stress_points.push((*time_ms, *score));
					}
				}
				wearable::Reading::BodyComposition {
					time_ms,
					body_fat_percent,
					skeletal_muscle_kg,
					water_percent,
				} => {
					let fat = body_fat_percent.filter(|v| v.is_finite());
					let muscle = skeletal_muscle_kg.filter(|v| v.is_finite());
					let water = water_percent.filter(|v| v.is_finite());
					if *time_ms > 0 && (fat.is_some() || muscle.is_some() || water.is_some()) {
						let mut record = Record::new(stream("health.body_composition"), device)
							.starting(timestamp(*time_ms)?)
							.with("source", "wearable");
						if let Some(fat) = fat {
							record = record.with("body_fat_percent", fat);
						}
						if let Some(muscle) = muscle {
							record = record.with("skeletal_muscle_kg", muscle);
						}
						if let Some(water) = water {
							record = record.with("water_percent", water);
						}
						record.id =
							record_id("health.body_composition", device, &[&time_ms.to_le_bytes()]);
						self.record(record)?;
						report.stored += 1;
					}
				}
				wearable::Reading::Ecg {
					time_ms,
					classification,
					average_bpm,
				} => {
					if *time_ms > 0 {
						let class_name = ecg_classification_str(*classification);
						let mut record = Record::new(stream("health.ecg"), device)
							.starting(timestamp(*time_ms)?)
							.with("classification", class_name)
							.with("source", "wearable");
						if let Some(bpm) = average_bpm.filter(|v| v.is_finite() && *v > 0.0) {
							record = record.with("avg_heart_rate_bpm", bpm);
							#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
							let rounded = bpm.round().clamp(1.0, f64::from(u16::MAX)) as u16;
							report.latest_heart_rate_bpm = Some(rounded);
						}
						record.id = record_id(
							"health.ecg",
							device,
							&[class_name.as_bytes(), &time_ms.to_le_bytes()],
						);
						self.record(record)?;
						report.stored += 1;
					}
				}
			}
		}

		report.stored += self.queue_scalar_series(device, "health.heart_rate", "bpm", hr_points)?;
		report.stored += self.queue_scalar_series(device, "health.hrv", "rmssd", hrv_points)?;
		report.stored += self.queue_scalar_series(device, "health.spo2", "percent", spo2_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.skin_temperature", "celsius", temp_points)?;
		report.stored += self.queue_scalar_series(device, "health.steps", "steps", step_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.floors", "floors", floor_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.active_calories", "kcal", cal_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.distance", "meters", dist_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.stress", "score", stress_points)?;

		if report.dropped > 0 {
			self.status(
				"wearable",
				true,
				Some(format!(
					"the watch dropped {} sensor readings before they could be saved",
					report.dropped
				)),
			);
		}
		Ok(report)
	}

	/// Queue one pedometer interval or boot-counter observation into
	/// `health.steps`.
	///
	/// For [`pedometer::Reading::BootCounter`], callers with persisted state
	/// should compute the step delta since the last reading (handling reboots)
	/// and call [`Self::pedometer_delta_steps`] so cumulative steps-since-boot
	/// are not double-counted across intervals.
	pub fn pedometer_reading(
		&self,
		device: DeviceId,
		now: Timestamp,
		reading: &pedometer::Reading,
	) -> Result<usize, SinkError> {
		let (at_ms, steps) = match *reading {
			pedometer::Reading::Interval {
				start_ms,
				reported_steps,
				..
			} => {
				#[allow(clippy::cast_possible_truncation)]
				let ms = start_ms.round() as i64;
				if !start_ms.is_finite() || ms <= 0 {
					return Ok(0);
				}
				(ms, reported_steps)
			}
			pedometer::Reading::BootCounter { reported_steps, .. } => {
				(now.as_millisecond(), reported_steps)
			}
		};
		if steps == 0 {
			return Ok(0);
		}
		self.queue_scalar_series(device, "health.steps", "steps", vec![(at_ms, steps as f64)])
	}

	/// Queue an already-differenced step delta into `health.steps`.
	pub fn pedometer_delta_steps(
		&self,
		device: DeviceId,
		at: Timestamp,
		steps: u64,
	) -> Result<usize, SinkError> {
		if steps == 0 {
			return Ok(0);
		}
		self.queue_scalar_series(
			device,
			"health.steps",
			"steps",
			vec![(at.as_millisecond(), steps as f64)],
		)
	}

	/// Queue one `leptoskit_sensor` motion or barometer batch into
	/// `motion.accelerometer`, `motion.gyroscope`, or `motion.barometer`.
	pub fn motion_batch(
		&self,
		device: DeviceId,
		at: Timestamp,
		kind_code: u32,
		payload: &[u8],
	) -> Result<usize, SinkError> {
		let kind = sensor::Kind::from_code(kind_code)
			.map_err(|_| SinkError::from_display("unknown motion sensor kind"))?;
		match kind {
			sensor::Kind::Accelerometer | sensor::Kind::UserAccelerometer => {
				let batch = sensor::Batch::<sensor::VectorReading>::decode(payload, 0)
					.map_err(|_| SinkError::from_display("invalid accelerometer batch"))?;
				self.queue_vector_series(device, at, "motion.accelerometer", &batch.samples)
			}
			sensor::Kind::Gyroscope => {
				let batch = sensor::Batch::<sensor::VectorReading>::decode(payload, 0)
					.map_err(|_| SinkError::from_display("invalid gyroscope batch"))?;
				self.queue_vector_series(device, at, "motion.gyroscope", &batch.samples)
			}
			sensor::Kind::Barometer => {
				let batch = sensor::Batch::<sensor::PressureReading>::decode(payload, 0)
					.map_err(|_| SinkError::from_display("invalid barometer batch"))?;
				self.queue_barometer_series(device, at, &batch.samples)
			}
			sensor::Kind::Magnetometer => Ok(0),
		}
	}

	/// Queue BLE GATT characteristic notifications from paired chest straps,
	/// smart rings, pulse oximeters, thermometers, and fitness sensors.
	pub fn ble_notifications(
		&self,
		device: DeviceId,
		batch: &ble::NotificationBatch,
	) -> Result<BleReport, SinkError> {
		let mut report = BleReport {
			dropped: batch.dropped,
			..BleReport::default()
		};
		let mut hr_points: Vec<(i64, f64)> = Vec::new();
		let mut hrv_points: Vec<(i64, f64)> = Vec::new();
		let mut spo2_points: Vec<(i64, f64)> = Vec::new();
		let mut temp_points: Vec<(i64, f64)> = Vec::new();
		let mut ble_batt_points: Vec<(i64, f64)> = Vec::new();
		let mut rsc_points: Vec<(i64, [f64; 2])> = Vec::new();
		let mut power_points: Vec<(i64, f64)> = Vec::new();

		for notif in &batch.notifications {
			if notif.time_ms <= 0 {
				report.skipped += 1;
				continue;
			}
			let uuid = notif.characteristic_uuid.to_ascii_lowercase();
			if matches_uuid(&uuid, gatt::CHAR_HEART_RATE_MEASUREMENT, "2a37") {
				match gatt::HeartRateMeasurement::decode(&notif.value) {
					Ok(hr) if hr.bpm > 0 => {
						hr_points.push((notif.time_ms, f64::from(hr.bpm)));
						report.latest_heart_rate_bpm = Some(hr.bpm);
						if let Some(rmssd) = hr.rmssd_ms() {
							hrv_points.push((notif.time_ms, rmssd));
						}
					}
					_ => report.skipped += 1,
				}
			} else if matches_uuid(&uuid, gatt::CHAR_PLX_SPOT_CHECK, "2a5e")
				|| matches_uuid(&uuid, gatt::CHAR_PLX_CONTINUOUS, "2a5f")
			{
				match gatt::PulseOximeterMeasurement::decode(&notif.value) {
					Ok(plx) => {
						if (0.0..=100.0).contains(&plx.spo2_percent) {
							spo2_points.push((notif.time_ms, plx.spo2_percent));
						}
						if plx.pulse_bpm > 0.0 {
							hr_points.push((notif.time_ms, plx.pulse_bpm));
							#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
							let rounded =
								plx.pulse_bpm.round().clamp(1.0, f64::from(u16::MAX)) as u16;
							report.latest_heart_rate_bpm = Some(rounded);
						}
					}
					Err(_) => report.skipped += 1,
				}
			} else if matches_uuid(&uuid, gatt::CHAR_TEMPERATURE_MEASUREMENT, "2a1c") {
				match gatt::TemperatureMeasurement::decode(&notif.value) {
					Ok(temp) => temp_points.push((notif.time_ms, temp.celsius)),
					Err(_) => report.skipped += 1,
				}
			} else if matches_uuid(&uuid, gatt::CHAR_BATTERY_LEVEL, "2a19") {
				match gatt::decode_battery_level(&notif.value) {
					Ok(level) => ble_batt_points.push((notif.time_ms, f64::from(level))),
					Err(_) => report.skipped += 1,
				}
			} else if matches_uuid(&uuid, gatt::CHAR_RSC_MEASUREMENT, "2a53") {
				match gatt::RunningSpeedCadence::decode(&notif.value) {
					Ok(rsc) if rsc.speed_mps.is_finite() => {
						rsc_points
							.push((notif.time_ms, [rsc.speed_mps, f64::from(rsc.cadence_spm)]));
					}
					_ => report.skipped += 1,
				}
			} else if matches_uuid(&uuid, gatt::CHAR_CYCLING_POWER_MEASUREMENT, "2a63") {
				match gatt::CyclingPower::decode(&notif.value) {
					Ok(cp) => power_points.push((notif.time_ms, f64::from(cp.watts))),
					Err(_) => report.skipped += 1,
				}
			} else {
				report.skipped += 1;
			}
		}

		report.stored += self.queue_scalar_series(device, "health.heart_rate", "bpm", hr_points)?;
		report.stored += self.queue_scalar_series(device, "health.hrv", "rmssd", hrv_points)?;
		report.stored += self.queue_scalar_series(device, "health.spo2", "percent", spo2_points)?;
		report.stored +=
			self.queue_scalar_series(device, "health.skin_temperature", "celsius", temp_points)?;
		report.stored +=
			self.queue_scalar_series(device, "sensor.ble.battery", "level", ble_batt_points)?;
		report.stored += self.queue_pair_series(
			device,
			"sensor.ble.running_cadence",
			["speed_mps", "cadence_spm"],
			rsc_points,
		)?;
		report.stored +=
			self.queue_scalar_series(device, "sensor.ble.cycling_power", "watts", power_points)?;

		Ok(report)
	}

	fn queue_sleep_stage(
		&self,
		device: DeviceId,
		start_ms: i64,
		end_ms: i64,
		stage: health::Stage,
		source: &health::Source,
	) -> Result<(), SinkError> {
		let stage_name = sleep_stage_str(stage);
		let mut record = Record::new(stream("health.sleep"), device)
			.starting(timestamp(start_ms)?)
			.ending(timestamp(end_ms)?)
			.with("stage", stage_name)
			.with("source", "health_store");
		record = attach_health_source(record, source);
		record.id = record_id(
			"health.sleep",
			device,
			&[
				stage_name.as_bytes(),
				&start_ms.to_le_bytes(),
				&end_ms.to_le_bytes(),
			],
		);
		self.record(record)
	}

	fn queue_vector_series(
		&self,
		device: DeviceId,
		at: Timestamp,
		stream_name: &str,
		samples: &[sensor::VectorReading],
	) -> Result<usize, SinkError> {
		let valid: Vec<&sensor::VectorReading> = samples
			.iter()
			.filter(|s| {
				s.timestamp.is_finite() && s.x.is_finite() && s.y.is_finite() && s.z.is_finite()
			})
			.collect();
		let Some(first) = valid.first() else {
			return Ok(0);
		};
		let origin = first.timestamp;
		let mut stored = 0;
		for chunk in valid.chunks(MAX_VECTOR_ROWS) {
			let chunk_base = chunk[0].timestamp;
			#[allow(clippy::cast_possible_truncation)]
			let chunk_delta_ms = ((chunk_base - origin).max(0.0) * 1000.0).round() as i64;
			let chunk_start = at + SignedDuration::from_millis(chunk_delta_ms);
			let mut offsets_ms = Vec::with_capacity(chunk.len());
			let mut values = Vec::with_capacity(chunk.len() * 3);
			let mut last_offset = 0u32;
			for sample in chunk {
				#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
				let raw = ((sample.timestamp - chunk_base).max(0.0) * 1000.0)
					.round()
					.min(f64::from(u32::MAX)) as u32;
				let offset = raw.max(last_offset);
				last_offset = offset;
				offsets_ms.push(offset);
				values.extend([sample.x, sample.y, sample.z]);
			}
			self.series(SeriesBatch {
				stream: stream(stream_name),
				device,
				start: chunk_start,
				channels: vec!["x".into(), "y".into(), "z".into()],
				offsets_ms,
				values,
			})?;
			stored += chunk.len();
		}
		Ok(stored)
	}

	fn queue_barometer_series(
		&self,
		device: DeviceId,
		at: Timestamp,
		samples: &[sensor::PressureReading],
	) -> Result<usize, SinkError> {
		let valid: Vec<&sensor::PressureReading> = samples
			.iter()
			.filter(|s| s.timestamp.is_finite() && s.hectopascals.is_finite())
			.collect();
		let Some(first) = valid.first() else {
			return Ok(0);
		};
		let origin = first.timestamp;
		let mut stored = 0;
		for chunk in valid.chunks(MAX_VECTOR_ROWS) {
			let chunk_base = chunk[0].timestamp;
			#[allow(clippy::cast_possible_truncation)]
			let chunk_delta_ms = ((chunk_base - origin).max(0.0) * 1000.0).round() as i64;
			let chunk_start = at + SignedDuration::from_millis(chunk_delta_ms);
			let mut offsets_ms = Vec::with_capacity(chunk.len());
			let mut values = Vec::with_capacity(chunk.len());
			let mut last_offset = 0u32;
			for sample in chunk {
				#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
				let raw = ((sample.timestamp - chunk_base).max(0.0) * 1000.0)
					.round()
					.min(f64::from(u32::MAX)) as u32;
				let offset = raw.max(last_offset);
				last_offset = offset;
				offsets_ms.push(offset);
				values.push(sample.hectopascals);
			}
			self.series(SeriesBatch {
				stream: stream("motion.barometer"),
				device,
				start: chunk_start,
				channels: vec!["hpa".into()],
				offsets_ms,
				values,
			})?;
			stored += chunk.len();
		}
		Ok(stored)
	}

	fn queue_pair_series(
		&self,
		device: DeviceId,
		stream_name: &str,
		channels: [&str; 2],
		mut points: Vec<(i64, [f64; 2])>,
	) -> Result<usize, SinkError> {
		if points.is_empty() {
			return Ok(0);
		}
		points.sort_by_key(|(at, _)| *at);
		let mut stored = 0;
		let mut run: Vec<(i64, [f64; 2])> = Vec::new();
		for (at, pair) in points {
			let breaks = run.first().is_some_and(|(first, _)| {
				at - first > MAX_BATCH_SPAN_MS || run.len() >= MAX_PAIR_ROWS
			});
			if breaks {
				stored += self.flush_pair_run(device, stream_name, channels, &run)?;
				run.clear();
			}
			run.push((at, pair));
		}
		stored += self.flush_pair_run(device, stream_name, channels, &run)?;
		Ok(stored)
	}

	fn flush_pair_run(
		&self,
		device: DeviceId,
		stream_name: &str,
		channels: [&str; 2],
		run: &[(i64, [f64; 2])],
	) -> Result<usize, SinkError> {
		let Some((first, _)) = run.first() else {
			return Ok(0);
		};
		let mut offsets_ms = Vec::with_capacity(run.len());
		let mut values = Vec::with_capacity(run.len() * 2);
		for (at, pair) in run {
			offsets_ms.push(u32::try_from(at - first).map_err(SinkError::from_display)?);
			values.extend_from_slice(pair);
		}
		self.series(SeriesBatch {
			stream: stream(stream_name),
			device,
			start: timestamp(*first)?,
			channels: vec![channels[0].to_owned(), channels[1].to_owned()],
			offsets_ms,
			values,
		})?;
		Ok(run.len())
	}

	fn queue_scalar_series(
		&self,
		device: DeviceId,
		stream_name: &str,
		channel: &str,
		mut points: Vec<(i64, f64)>,
	) -> Result<usize, SinkError> {
		if points.is_empty() {
			return Ok(0);
		}
		points.sort_by_key(|(at, _)| *at);
		let mut stored = 0;
		let mut run: Vec<(i64, f64)> = Vec::new();
		for (at, value) in points {
			let breaks = run.first().is_some_and(|(first, _)| {
				at - first > MAX_BATCH_SPAN_MS || run.len() >= MAX_SERIES_ROWS
			});
			if breaks {
				stored += self.flush_scalar_run(device, stream_name, channel, &run)?;
				run.clear();
			}
			run.push((at, value));
		}
		stored += self.flush_scalar_run(device, stream_name, channel, &run)?;
		Ok(stored)
	}

	fn flush_scalar_run(
		&self,
		device: DeviceId,
		stream_name: &str,
		channel: &str,
		run: &[(i64, f64)],
	) -> Result<usize, SinkError> {
		let Some((first, _)) = run.first() else {
			return Ok(0);
		};
		let mut offsets_ms = Vec::with_capacity(run.len());
		let mut values = Vec::with_capacity(run.len());
		for (at, value) in run {
			offsets_ms.push(u32::try_from(at - first).map_err(SinkError::from_display)?);
			values.push(*value);
		}
		self.series(SeriesBatch {
			stream: stream(stream_name),
			device,
			start: timestamp(*first)?,
			channels: vec![channel.to_owned()],
			offsets_ms,
			values,
		})?;
		Ok(run.len())
	}
}

fn attach_health_source(mut record: Record, source: &health::Source) -> Record {
	if let Some(app_id) = source.app_id.as_deref() {
		record = record.with("health_app", app_id);
	}
	if let Some(device_name) = source.device.as_deref().or(source.app_name.as_deref()) {
		record = record.with("health_device", device_name);
	}
	record
}

fn sleep_stage_str(stage: health::Stage) -> &'static str {
	match stage {
		health::Stage::Unknown => "unknown",
		health::Stage::Awake => "awake",
		health::Stage::Asleep => "asleep",
		health::Stage::Light => "light",
		health::Stage::Deep => "deep",
		health::Stage::Rem => "rem",
		health::Stage::InBed => "in_bed",
	}
}

fn activity_str(activity: health::Activity) -> &'static str {
	match activity {
		health::Activity::Other => "other",
		health::Activity::Walking => "walking",
		health::Activity::Running => "running",
		health::Activity::Cycling => "cycling",
		health::Activity::Swimming => "swimming",
		health::Activity::Hiking => "hiking",
		health::Activity::StrengthTraining => "strength_training",
		health::Activity::Yoga => "yoga",
		health::Activity::HighIntensityIntervalTraining => "hiit",
		health::Activity::Rowing => "rowing",
		health::Activity::Elliptical => "elliptical",
		health::Activity::StairClimbing => "stair_climbing",
		health::Activity::Dancing => "dancing",
		health::Activity::Pilates => "pilates",
		health::Activity::MartialArts => "martial_arts",
	}
}

fn ecg_classification_str(classification: wearable::EcgClassification) -> &'static str {
	match classification {
		wearable::EcgClassification::Unknown => "unknown",
		wearable::EcgClassification::SinusRhythm => "sinus_rhythm",
		wearable::EcgClassification::AtrialFibrillation => "atrial_fibrillation",
		wearable::EcgClassification::LowOrHighHeartRate => "low_or_high_heart_rate",
		wearable::EcgClassification::Inconclusive => "inconclusive",
		wearable::EcgClassification::PoorRecording => "poor_recording",
	}
}

fn rmssd_from_rr(rr_intervals_ms: &[f32]) -> Option<f64> {
	if rr_intervals_ms.len() < 2 {
		return None;
	}
	let mut sum_sq = 0.0_f64;
	for pair in rr_intervals_ms.windows(2) {
		let diff = f64::from(pair[1] - pair[0]);
		sum_sq += diff * diff;
	}
	Some((sum_sq / (rr_intervals_ms.len() - 1) as f64).sqrt())
}

fn matches_uuid(uuid: &str, canonical: &str, short_hex: &str) -> bool {
	uuid == canonical || uuid == short_hex || uuid == format!("0x{short_hex}")
}
