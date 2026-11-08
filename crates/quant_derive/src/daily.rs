//! One record per day: where the time went, how the body did, how the
//! night went.

use std::collections::BTreeMap;
use std::fmt::Write;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use quant_core::Record;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::Selector;
use quant_store::Store;
use quant_store::StoreError;
use serde_json::json;

use crate::DERIVED_DEVICE;
use crate::stable_id;

fn stream(name: &str) -> StreamId {
	name.parse().expect("built-in stream")
}

pub(crate) fn day_range(date: Date, tz: &TimeZone) -> Result<TimeRange, StoreError> {
	let start = date
		.to_datetime(Time::midnight())
		.to_zoned(tz.clone())
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	let end = start
		.tomorrow()
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	TimeRange::new(start.timestamp(), end.timestamp())
		.map_err(|e| StoreError::Invalid(e.to_string()))
}

/// Sleep is counted for the night that ends on `date`: 18:00 the evening
/// before to 18:00 on the day.
pub(crate) fn night_range(day: TimeRange) -> TimeRange {
	let six_hours = SignedDuration::from_hours(6);
	TimeRange {
		start: day.start - six_hours,
		end: day.end - six_hours,
	}
}

fn grouped(
	store: &Store,
	name: &str,
	range: TimeRange,
	key: &str,
) -> Result<BTreeMap<String, f64>, StoreError> {
	let mut totals = BTreeMap::new();
	for bucket in store.durations(&stream(name), range, Some(key), range.duration())? {
		if let Some(group) = bucket.group {
			*totals.entry(group).or_insert(0.0) += bucket.seconds;
		}
	}
	Ok(totals)
}

fn hm(seconds: f64) -> String {
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let minutes = (seconds / 60.0).round().max(0.0) as u64;
	if minutes < 60 {
		format!("{minutes}m")
	} else {
		format!("{}h {:02}m", minutes / 60, minutes % 60)
	}
}

/// The resting heart rate estimate for `range`: the mean of the lowest
/// tenth of minute averages, if any heart rate was captured.
pub(crate) fn resting_heart_rate(
	store: &Store,
	range: TimeRange,
) -> Result<Option<f64>, StoreError> {
	let minutes = store.series_summary(
		&stream("health.heart_rate"),
		range,
		SignedDuration::from_mins(1),
	)?;
	let mut means: Vec<f64> = minutes
		.iter()
		.filter_map(|b| b.channels.get("bpm")?.mean())
		.collect();
	means.sort_by(f64::total_cmp);
	Ok((!means.is_empty()).then(|| {
		let lowest = &means[..means.len().div_ceil(10)];
		lowest.iter().sum::<f64>() / lowest.len() as f64
	}))
}

/// Summarise `date` (in `tz`) into a `memory.daily` record.
pub fn summarize(store: &Store, date: Date, tz: &TimeZone) -> Result<Record, StoreError> {
	let range = day_range(date, tz)?;
	let night = night_range(range);

	let mut apps: Vec<(String, f64)> = grouped(store, "apps.focus", range, "app")?
		.into_iter()
		.collect();
	apps.sort_by(|a, b| b.1.total_cmp(&a.1));
	let screen_seconds: f64 = apps.iter().map(|(_, s)| s).sum();
	apps.truncate(10);

	let posture = grouped(store, "motion.posture", range, "posture")?;
	let sleep = grouped(store, "health.sleep", night, "stage")?;
	let asleep: f64 = sleep
		.iter()
		.filter(|(stage, _)| stage.as_str() != "awake")
		.map(|(_, s)| s)
		.sum();
	let speech_seconds: f64 = store
		.durations(&stream("audio.transcript"), range, None, range.duration())?
		.iter()
		.map(|b| b.seconds)
		.sum();

	let steps: f64 = store
		.series_summary(&stream("health.steps"), range, range.duration())?
		.iter()
		.filter_map(|b| b.channels.get("steps"))
		.map(|s| s.sum)
		.sum();

	let minutes = store.series_summary(
		&stream("health.heart_rate"),
		range,
		SignedDuration::from_mins(1),
	)?;
	let heart = resting_heart_rate(store, range)?.map(|resting| {
		let min = minutes.iter().filter_map(|b| b.channels.get("bpm")).map(|s| s.min).fold(f64::INFINITY, f64::min);
		let max = minutes.iter().filter_map(|b| b.channels.get("bpm")).map(|s| s.max).fold(f64::NEG_INFINITY, f64::max);
		let (sum, count) = minutes
			.iter()
			.filter_map(|b| b.channels.get("bpm"))
			.fold((0.0, 0u64), |(sum, count), s| (sum + s.sum, count + s.count));
		json!({ "min": min, "max": max, "mean": sum / count.max(1) as f64, "resting_estimate": resting, "minutes": minutes.iter().filter(|b| b.channels.contains_key("bpm")).count() })
	});

	let frames = store
		.overview(&Selector::stream(stream("screen.frame")), range)?
		.iter()
		.map(|s| s.count)
		.sum::<u64>();

	let mut text = format!("{date}:");
	if screen_seconds > 0.0 {
		let top: Vec<String> = apps
			.iter()
			.take(3)
			.map(|(app, s)| format!("{app} {}", hm(*s)))
			.collect();
		let _ = write!(
			text,
			" {} on screen ({}).",
			hm(screen_seconds),
			top.join(", ")
		);
	}
	if steps > 0.0 {
		let _ = write!(text, " {steps:.0} steps.");
	}
	if let Some(heart) = &heart {
		let _ = write!(
			text,
			" Heart rate {:.0}–{:.0} bpm, resting about {:.0}.",
			heart["min"].as_f64().unwrap_or_default(),
			heart["max"].as_f64().unwrap_or_default(),
			heart["resting_estimate"].as_f64().unwrap_or_default()
		);
	}
	if asleep > 0.0 {
		let deep = sleep.get("deep").copied().unwrap_or_default();
		let _ = write!(text, " Slept {} (deep {}).", hm(asleep), hm(deep));
	}
	if !posture.is_empty() {
		let mut parts: Vec<(&String, &f64)> = posture.iter().collect();
		parts.sort_by(|a, b| b.1.total_cmp(a.1));
		let shown: Vec<String> = parts
			.iter()
			.take(4)
			.map(|(p, s)| format!("{} {}", p.replace('_', " "), hm(**s)))
			.collect();
		let _ = write!(text, " Posture: {}.", shown.join(", "));
	}
	if speech_seconds > 0.0 {
		let _ = write!(text, " Heard {} of speech.", hm(speech_seconds));
	}
	if text.ends_with(':') {
		text.push_str(" nothing captured.");
	}

	let mut record = Record::new(stream("memory.daily"), DERIVED_DEVICE)
		.starting(range.start)
		.ending(range.end)
		.with_text(text)
		.with("date", date.to_string())
		.with("activity", "day")
		.with("derived_from", "daily")
		.with("screen_seconds", screen_seconds)
		.with(
			"apps",
			apps.iter()
				.map(|(app, s)| json!({ "app": app, "seconds": s }))
				.collect::<Vec<_>>(),
		)
		.with(
			"posture_seconds",
			serde_json::to_value(&posture).unwrap_or_default(),
		)
		.with(
			"sleep_seconds",
			serde_json::to_value(&sleep).unwrap_or_default(),
		)
		.with("asleep_seconds", asleep)
		.with("steps", steps)
		.with("speech_seconds", speech_seconds)
		.with("screen_frames", frames);
	if let Some(heart) = heart {
		record = record.with("heart_rate", heart);
	}
	record.id = stable_id(&["daily", &date.to_string()]);
	Ok(record)
}

/// The local dates of `now` and the day before.
pub fn recent_dates(now: Timestamp, tz: &TimeZone) -> [Date; 2] {
	let today = now.to_zoned(tz.clone()).date();
	[today.yesterday().unwrap_or(today), today]
}
