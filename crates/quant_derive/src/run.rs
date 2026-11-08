//! Keeping derived records up to date.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;
use quant_store::StoreError;
use serde::Deserialize;
use serde::Serialize;

use crate::daily;
use crate::episode;
use crate::gait;
use crate::posture;
use crate::posture::Placement;
use crate::posture::Source;
use crate::weekly;

/// What to derive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
	/// Accelerometers to read posture from.
	pub posture_sources: Vec<Source>,
	/// How far back to look the first time.
	pub first_run_days: i64,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			posture_sources: vec![Source {
				stream: "motion.accelerometer".parse().expect("built-in stream"),
				placement: Placement::Loose,
				up: [0.0, 1.0, 0.0],
			}],
			first_run_days: 7,
		}
	}
}

/// What a run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
	/// Posture spans written.
	pub posture_spans: usize,
	/// Gait bouts written.
	pub gait_bouts: usize,
	/// Daily summaries written.
	pub days: usize,
	/// Hourly episodes written.
	pub episodes: usize,
	/// Weekly summaries written.
	pub weeks: usize,
}

const MAX_ROWS: usize = 2_000_000;

/// Bring posture spans, gait bouts, and the last two daily summaries up to
/// date.
pub fn run(
	store: &Store,
	config: &Config,
	now: Timestamp,
	tz: &TimeZone,
) -> Result<Report, StoreError> {
	let mut report = Report::default();
	let posture_stream: StreamId = "motion.posture".parse().expect("built-in stream");
	for source in &config.posture_sources {
		let key = format!("derive.posture.{}", source.stream);
		let watermark = store
			.meta(&key)?
			.and_then(|v| v.parse::<Timestamp>().ok())
			.unwrap_or_else(|| now - SignedDuration::from_hours(24 * config.first_run_days));
		// Leave the last minute alone: its samples may still be arriving.
		let end = now - SignedDuration::from_mins(1);
		if end <= watermark {
			continue;
		}
		let range = TimeRange {
			start: watermark,
			end,
		};
		let batches = store.series(&source.stream, range, MAX_ROWS)?;
		let mut devices: Vec<_> = batches.iter().map(|b| b.device).collect();
		devices.sort_unstable();
		devices.dedup();
		let mut resume = end;
		store.delete_derived(&posture_stream, source.stream.as_str(), watermark)?;
		for device in devices {
			let mine: Vec<_> = batches
				.iter()
				.filter(|b| b.device == device)
				.cloned()
				.collect();
			let windows = posture::windows(&mine, source.placement, source.up);
			let spans = posture::spans(
				&windows,
				SignedDuration::from_secs(30),
				SignedDuration::from_secs(20),
			);
			// The last span may continue; recompute it next time.
			if let Some(open) = spans.last() {
				resume = resume.min(open.start);
			}
			let records = posture::records(&spans, &source.stream, device);
			report.posture_spans += store.replace_records(&records)?.stored;
		}
		store.set_meta(&key, &resume.max(watermark).to_string())?;
	}
	let gait_stream: StreamId = "motion.gait".parse().expect("built-in stream");
	for source in &config.posture_sources {
		let key = format!("derive.gait.{}", source.stream);
		let watermark = store
			.meta(&key)?
			.and_then(|v| v.parse::<Timestamp>().ok())
			.unwrap_or_else(|| now - SignedDuration::from_hours(24 * config.first_run_days));
		let end = now - SignedDuration::from_mins(1);
		if end <= watermark {
			continue;
		}
		let range = TimeRange {
			start: watermark,
			end,
		};
		let batches = store.series(&source.stream, range, MAX_ROWS)?;
		let mut devices: Vec<_> = batches.iter().map(|b| b.device).collect();
		devices.sort_unstable();
		devices.dedup();
		let mut resume = end;
		store.delete_derived(&gait_stream, source.stream.as_str(), watermark)?;
		for device in devices {
			let mine: Vec<_> = batches
				.iter()
				.filter(|b| b.device == device)
				.cloned()
				.collect();
			let bouts = gait::bouts(&mine, source.placement, source.up);
			// The last bout may continue; recompute it next time.
			if let Some(open) = bouts.last() {
				resume = resume.min(open.start);
			}
			// The slow-down trend reads the day's earlier bouts, which may
			// sit before the watermark: read back what the last run wrote.
			// Derived records come from the nil device; the sensor is named
			// by `sensor_device`.
			let sensor = device.to_string();
			let earlier = store
				.records(&RecordQuery::new(
					Selector::stream(gait_stream.clone()),
					TimeRange {
						start: watermark - SignedDuration::from_hours(24),
						end: watermark,
					},
				))?
				.iter()
				.filter(|record| {
					record.data.get("sensor_device").and_then(|v| v.as_str())
						== Some(sensor.as_str())
						&& record.data.get("derived_from").and_then(|v| v.as_str())
							== Some(source.stream.as_str())
				})
				.filter_map(gait::GaitBout::stored)
				.collect::<Vec<_>>();
			let records = gait::records(&bouts, &earlier, &source.stream, device, tz);
			report.gait_bouts += store.replace_records(&records)?.stored;
		}
		store.set_meta(&key, &resume.max(watermark).to_string())?;
	}
	for date in daily::recent_dates(now, tz) {
		let record = daily::summarize(store, date, tz)?;
		report.days += store.replace_records(&[record])?.stored;
	}
	report.episodes = episode::update(store, now, config.first_run_days, tz)?;
	// The current week's summary, recomputed each run so purged sources
	// stop being quoted. No data means no record, and any earlier one for
	// the week goes away with it.
	let weekly_stream: StreamId = "memory.weekly".parse().expect("built-in stream");
	let monday = weekly::monday_of(now, tz);
	let start = daily::day_range(monday, tz)?.start;
	match weekly::summarize_week(store, monday, tz)? {
		Some(record) => report.weeks += store.replace_records(&[record])?.stored,
		None => {
			store.delete_derived(&weekly_stream, "weekly", start)?;
		}
	}
	Ok(report)
}
