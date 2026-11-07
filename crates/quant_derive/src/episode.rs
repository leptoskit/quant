//! One record per hour: what held attention, where it happened, and how the
//! body was doing.

use std::collections::BTreeMap;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::Record;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;
use quant_store::StoreError;
use serde_json::json;

use crate::DERIVED_DEVICE;
use crate::stable_id;

/// The meta key holding the latest hour computed.
pub const WATERMARK: &str = "derive.episode";

fn stream(name: &str) -> StreamId {
	name.parse().expect("built-in stream")
}

/// The start of the local hour containing `t`.
pub fn hour_start(t: Timestamp, tz: &TimeZone) -> Timestamp {
	let local = t.to_zoned(tz.clone());
	local
		.with()
		.minute(0)
		.second(0)
		.subsec_nanosecond(0)
		.build()
		.map_or(t, |hour| hour.timestamp())
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn minutes(seconds: f64) -> String {
	format!("{} min", (seconds / 60.0).round().max(0.0) as u64)
}

fn join_and(items: &[String]) -> String {
	match items {
		[] => String::new(),
		[one] => one.clone(),
		[a, b] => format!("{a} and {b}"),
		[rest @ .., last] => format!("{} and {last}", rest.join(", ")),
	}
}

fn place_name(record: &Record) -> Option<String> {
	["name", "label", "place"].iter().find_map(|key| {
		record
			.data
			.get(*key)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned)
	})
}

/// Summarise the local hour `[start, start+1h)` into a `memory.episode`
/// record, or return `None` when nothing was captured in it.
pub fn summarize_hour(
	store: &Store,
	start: Timestamp,
	tz: &TimeZone,
) -> Result<Option<Record>, StoreError> {
	let start = hour_start(start, tz);
	let end = start + SignedDuration::from_hours(1);
	let range = TimeRange { start, end };

	let app_totals = grouped(store, "apps.focus", range, "app")?;
	let mut apps: Vec<(String, f64)> = app_totals.into_iter().collect();
	apps.sort_by(|a, b| b.1.total_cmp(&a.1));
	apps.truncate(3);

	let visits = store.records(
		&RecordQuery::new(Selector::stream(stream("location.visit")), range).oldest_first(),
	)?;
	let place = visits.iter().find_map(place_name);

	let posture = grouped(store, "motion.posture", range, "posture")?;
	let mut postures: Vec<(&String, &f64)> = posture.iter().collect();
	postures.sort_by(|a, b| b.1.total_cmp(a.1));

	let heart = {
		let buckets = store.series_summary(
			&stream("health.heart_rate"),
			range,
			SignedDuration::from_mins(1),
		)?;
		let (mut count, mut sum) = (0u64, 0.0);
		let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
		for stat in buckets.iter().filter_map(|b| b.channels.get("bpm")) {
			count += stat.count;
			sum += stat.sum;
			min = min.min(stat.min);
			max = max.max(stat.max);
		}
		(count > 0).then(|| (min, sum / count as f64, max))
	};

	let steps_summary = store.series_summary(&stream("health.steps"), range, range.duration())?;
	let steps_seen = steps_summary
		.iter()
		.any(|b| b.channels.contains_key("steps"));
	let steps: f64 = steps_summary
		.iter()
		.filter_map(|b| b.channels.get("steps"))
		.map(|s| s.sum)
		.sum();

	let speech_seconds: f64 = store
		.durations(&stream("audio.transcript"), range, None, range.duration())?
		.iter()
		.map(|b| b.seconds)
		.sum();

	let clock = |t: Timestamp| t.to_zoned(tz.clone()).strftime("%H:%M").to_string();
	let mut parts: Vec<String> = Vec::new();
	if !apps.is_empty() {
		let top: Vec<String> = apps
			.iter()
			.map(|(app, seconds)| format!("{app} ({})", minutes(*seconds)))
			.collect();
		parts.push(format!("mostly {}", join_and(&top)));
	}
	if let Some(place) = &place {
		parts.push(format!("at {place}"));
	}
	if !postures.is_empty() {
		let shown: Vec<String> = postures
			.iter()
			.map(|(posture, seconds)| {
				format!("{} {}", posture.replace('_', " "), minutes(**seconds))
			})
			.collect();
		parts.push(shown.join(", "));
	}
	if let Some((min, mean, max)) = heart {
		parts.push(format!("heart rate {min:.0}–{max:.0} bpm (avg {mean:.0})"));
	}
	if steps_seen {
		parts.push(format!("{steps:.0} steps"));
	}
	if speech_seconds > 0.0 {
		parts.push(format!("{} of speech", minutes(speech_seconds)));
	}
	if parts.is_empty() {
		return Ok(None);
	}
	let text = format!("{}–{}: {}.", clock(start), clock(end), parts.join(", "));

	let mut record = Record::new(stream("memory.episode"), DERIVED_DEVICE)
		.starting(start)
		.ending(end)
		.with_text(text)
		.with("derived_from", "episode")
		.with(
			"apps",
			apps.iter()
				.map(|(app, seconds)| json!({ "app": app, "seconds": seconds }))
				.collect::<Vec<_>>(),
		)
		.with(
			"posture_seconds",
			serde_json::to_value(&posture).unwrap_or_default(),
		)
		.with("steps", steps)
		.with("speech_seconds", speech_seconds);
	if let Some(place) = place {
		record = record.with("place", place);
	}
	if let Some((min, mean, max)) = heart {
		record = record
			.with("hr_min", min)
			.with("hr_mean", mean)
			.with("hr_max", max);
	}
	record.id = stable_id(&["episode", &start.to_string()]);
	Ok(Some(record))
}

/// Recompute recent episodes, returning how many records were written.
///
/// The first run looks `first_run_days` back; later runs continue from the
/// stored watermark, always revisiting the current and previous hour. One run
/// processes at most `24 * first_run_days` hours, keeping the most recent.
pub fn update(
	store: &Store,
	now: Timestamp,
	first_run_days: i64,
	tz: &TimeZone,
) -> Result<usize, StoreError> {
	let max_hours = first_run_days.max(1).saturating_mul(24);
	let watermark = store
		.meta(WATERMARK)?
		.and_then(|value| value.parse::<Timestamp>().ok())
		.unwrap_or_else(|| now - SignedDuration::from_hours(max_hours));
	let current = hour_start(now, tz);
	let previous = current - SignedDuration::from_hours(1);
	let earliest = current - SignedDuration::from_hours(max_hours - 1);
	let from = hour_start(watermark, tz).clamp(earliest, previous);
	let mut written = 0;
	let mut start = from;
	while start <= current {
		if let Some(record) = summarize_hour(store, start, tz)? {
			written += store.replace_records(&[record])?.stored;
		}
		start += SignedDuration::from_hours(1);
	}
	store.set_meta(WATERMARK, &current.to_string())?;
	Ok(written)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hours_floor_to_local_boundaries() {
		let t: Timestamp = "2026-11-06T10:37:12Z".parse().unwrap();
		assert_eq!(
			hour_start(t, &TimeZone::UTC),
			"2026-11-06T10:00:00Z".parse::<Timestamp>().unwrap()
		);
		assert_eq!(
			hour_start("2026-11-06T10:00:00Z".parse().unwrap(), &TimeZone::UTC),
			"2026-11-06T10:00:00Z".parse::<Timestamp>().unwrap()
		);
		// India is UTC+05:30, so its hours start at half past in UTC.
		let kolkata = TimeZone::get("Asia/Kolkata").unwrap();
		assert_eq!(
			hour_start(t, &kolkata),
			"2026-11-06T10:30:00Z".parse::<Timestamp>().unwrap()
		);
	}

	#[test]
	fn minutes_round_and_never_go_negative() {
		assert_eq!(minutes(360.0), "6 min");
		assert_eq!(minutes(2_280.0), "38 min");
		assert_eq!(minutes(-10.0), "0 min");
	}

	#[test]
	fn lists_join_naturally() {
		assert_eq!(join_and(&[]), "");
		assert_eq!(join_and(&["a".to_owned()]), "a");
		assert_eq!(join_and(&["a".to_owned(), "b".to_owned()]), "a and b");
		assert_eq!(
			join_and(&["a".to_owned(), "b".to_owned(), "c".to_owned()]),
			"a, b and c"
		);
	}
}
