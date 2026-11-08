//! One record per local week: how the week went, and the plain correlations
//! between body and behaviour that a person asks about ("how did I sleep
//! after late screen time?").

use std::collections::BTreeMap;
use std::fmt::Write;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::Date;
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
use crate::daily::day_range;
use crate::daily::night_range;
use crate::daily::resting_heart_rate;
use crate::stable_id;

/// A finding is not stated with fewer points than this.
const MIN_POINTS: usize = 5;

/// How many earlier weeks the correlations draw on, the current week
/// included.
const WINDOW_WEEKS: i64 = 4;

/// The minutes a focus span must reach to count as a focus stretch.
const FOCUS_MINUTES: f64 = 25.0;

fn stream(name: &str) -> StreamId {
	name.parse().expect("built-in stream")
}

/// The Monday on which the local week containing `t` starts.
pub fn monday_of(t: Timestamp, tz: &TimeZone) -> Date {
	week_start(t.to_zoned(tz.clone()).date())
}

/// The Monday on or before `date`.
pub fn week_start(date: Date) -> Date {
	date - SignedDuration::from_hours(24 * i64::from(date.weekday().to_monday_zero_offset()))
}

/// The span of the week starting `monday`, in `tz`.
fn week_range(monday: Date, tz: &TimeZone) -> Result<TimeRange, StoreError> {
	let start = day_range(monday, tz)?.start;
	TimeRange::new(start, start + SignedDuration::from_hours(24 * 7))
		.map_err(|e| StoreError::Invalid(e.to_string()))
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

/// Total seconds per group of one event stream over a range.
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

/// The average of `values` (their mean) and how many there are.
fn average(values: &[f64]) -> (f64, usize) {
	let n = values.len();
	if n == 0 {
		(0.0, 0)
	} else {
		(values.iter().sum::<f64>() / n as f64, n)
	}
}

/// The sleep total of each night ending on a day in the week starting
/// `monday`: `None` where no sleep was recorded at all.
fn sleep_nights(
	store: &Store,
	monday: Date,
	tz: &TimeZone,
) -> Result<Vec<Option<f64>>, StoreError> {
	let mut nights = Vec::new();
	let mut date = monday;
	for _ in 0..7 {
		nights.push(sleep_night(store, date, tz)?);
		date = date
			.tomorrow()
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
	}
	Ok(nights)
}

/// The sleep total of the night ending on `date`, if any sleep was
/// recorded.
fn sleep_night(store: &Store, date: Date, tz: &TimeZone) -> Result<Option<f64>, StoreError> {
	let night = night_range(day_range(date, tz)?);
	let stages = grouped(store, "health.sleep", night, "stage")?;
	if stages.is_empty() {
		Ok(None)
	} else {
		Ok(Some(
			stages
				.iter()
				.filter(|(stage, _)| stage.as_str() != "awake")
				.map(|(_, seconds)| seconds)
				.sum(),
		))
	}
}

/// The steps walked on `date`, or `None` where none were captured.
fn steps_on(store: &Store, date: Date, tz: &TimeZone) -> Result<Option<f64>, StoreError> {
	let range = day_range(date, tz)?;
	let buckets = store.series_summary(&stream("health.steps"), range, range.duration())?;
	let seen = buckets.iter().any(|b| b.channels.contains_key("steps"));
	Ok(seen.then(|| {
		buckets
			.iter()
			.filter_map(|b| b.channels.get("steps"))
			.map(|s| s.sum)
			.sum()
	}))
}

/// The steps walked on each day of the week starting `monday`: `None` where
/// no steps were captured.
fn steps_days(store: &Store, monday: Date, tz: &TimeZone) -> Result<Vec<Option<f64>>, StoreError> {
	let mut days = Vec::new();
	let mut date = monday;
	for _ in 0..7 {
		days.push(steps_on(store, date, tz)?);
		date = date
			.tomorrow()
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
	}
	Ok(days)
}

/// The resting heart rate of each day of the week starting `monday`: `None`
/// where no heart rate was captured.
fn resting_days(
	store: &Store,
	monday: Date,
	tz: &TimeZone,
) -> Result<Vec<Option<f64>>, StoreError> {
	let mut days = Vec::new();
	let mut date = monday;
	for _ in 0..7 {
		days.push(resting_heart_rate(store, day_range(date, tz)?)?);
		date = date
			.tomorrow()
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
	}
	Ok(days)
}

/// Whether the night ending on `date` saw screen use after 23:00 (until
/// 02:00, the small hours).
fn late_screen(store: &Store, date: Date, tz: &TimeZone) -> Result<bool, StoreError> {
	let day = day_range(date, tz)?;
	let window = TimeRange::new(
		day.start + SignedDuration::from_hours(23),
		day.start + SignedDuration::from_hours(26),
	)
	.map_err(|e| StoreError::Invalid(e.to_string()))?;
	Ok(!store
		.records(&RecordQuery::new(Selector::stream(stream("screen.frame")), window).limit(1))?
		.is_empty())
}

/// The Pearson correlation of paired values, or `None` where one side has
/// no variance.
fn pearson(pairs: &[(f64, f64)]) -> Option<f64> {
	let n = pairs.len() as f64;
	let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
	for &(x, y) in pairs {
		sx += x;
		sy += y;
		sxx += x * x;
		syy += y * y;
		sxy += x * y;
	}
	let numerator = n * sxy - sx * sy;
	let denominator = ((n * sxx - sx * sx) * (n * syy - sy * sy)).sqrt();
	if denominator == 0.0 {
		None
	} else {
		Some(numerator / denominator)
	}
}

/// Focus spans of at least [`FOCUS_MINUTES`] minutes within `range`, as
/// `(start, seconds)`.
fn focus_spans(store: &Store, range: TimeRange) -> Result<Vec<(Timestamp, f64)>, StoreError> {
	let spans = store.records(&RecordQuery::new(
		Selector::stream(stream("apps.focus")),
		range,
	))?;
	Ok(spans
		.into_iter()
		.filter_map(|record| {
			let seconds = record.end?.duration_since(record.start).as_secs_f64();
			(seconds >= FOCUS_MINUTES * 60.0).then_some((record.start, seconds))
		})
		.collect())
}

/// The minutes of focus per local hour over the five-week window ending
/// with the week starting `monday`, best hours first.
fn focus_by_hour(
	store: &Store,
	monday: Date,
	tz: &TimeZone,
) -> Result<Vec<(u8, f64, usize)>, StoreError> {
	let start = monday - SignedDuration::from_hours(24 * 7 * WINDOW_WEEKS);
	let window = TimeRange::new(day_range(start, tz)?.start, week_range(monday, tz)?.end)
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	let spans = focus_spans(store, window)?;
	let mut hours: BTreeMap<u8, f64> = BTreeMap::new();
	for (start, seconds) in &spans {
		let hour = u8::try_from(start.to_zoned(tz.clone()).hour()).unwrap_or_default();
		*hours.entry(hour).or_insert(0.0) += seconds / 60.0;
	}
	let mut ranked: Vec<(u8, f64, usize)> = hours
		.into_iter()
		.map(|(hour, minutes)| (hour, minutes, spans.len()))
		.collect();
	ranked.sort_by(|(_, a, _), (_, b, _)| b.total_cmp(a));
	Ok(ranked)
}

/// Summarise the week starting `monday` (in `tz`) into a `memory.weekly`
/// record, or `None` when nothing is reportable. Findings state their
/// numbers and how many days they rest on, and stay silent below
/// [`MIN_POINTS`] data points.
pub fn summarize_week(
	store: &Store,
	monday: Date,
	tz: &TimeZone,
) -> Result<Option<Record>, StoreError> {
	let monday = week_start(monday);
	let range = week_range(monday, tz)?;
	let mut parts: Vec<String> = Vec::new();
	let mut data = serde_json::Map::new();
	data.insert("date".into(), monday.to_string().into());

	// Average sleep, and its change from the previous week.
	let nights = sleep_nights(store, monday, tz)?;
	let this_week: Vec<f64> = nights.iter().flatten().copied().collect();
	let (sleep_avg, sleep_count) = average(&this_week);
	if sleep_count >= MIN_POINTS {
		parts.push(format!(
			"Slept on average {} per night ({sleep_count} nights).",
			hm(sleep_avg)
		));
		data.insert(
			"sleep_seconds_avg".into(),
			json!({ "value": sleep_avg, "nights": sleep_count }),
		);
		let previous = sleep_nights(store, monday - SignedDuration::from_hours(24 * 7), tz)?;
		let previous_week: Vec<f64> = previous.iter().flatten().copied().collect();
		let (previous_avg, previous_count) = average(&previous_week);
		if previous_count >= MIN_POINTS {
			let change = sleep_avg - previous_avg;
			let direction = if change >= 0.0 { "more" } else { "less" };
			parts.push(format!(
				"That is {} {direction} than the week before ({previous_count} nights).",
				hm(change.abs())
			));
			data.insert(
				"sleep_change_seconds".into(),
				json!({ "value": change, "nights": previous_count }),
			);
		}
	}

	// Resting heart rate, over the days with data.
	let resting: Vec<f64> = resting_days(store, monday, tz)?
		.into_iter()
		.flatten()
		.collect();
	let (resting_avg, resting_days) = average(&resting);
	if resting_days >= MIN_POINTS {
		parts.push(format!(
			"Resting heart rate averaged {resting_avg:.0} bpm ({resting_days} days)."
		));
		data.insert(
			"resting_hr_avg".into(),
			json!({ "value": resting_avg, "days": resting_days }),
		);
	}

	// Steps.
	let steps: Vec<f64> = steps_days(store, monday, tz)?
		.into_iter()
		.flatten()
		.collect();
	let (steps_avg, steps_days) = average(&steps);
	if steps_days >= MIN_POINTS {
		parts.push(format!(
			"Walked {steps_avg:.0} steps a day on average ({steps_days} days)."
		));
		data.insert(
			"steps_avg".into(),
			json!({ "value": steps_avg, "days": steps_days }),
		);
	}

	// Where the screen time went.
	let mut apps: Vec<(String, f64)> = grouped(store, "apps.focus", range, "app")?
		.into_iter()
		.collect();
	apps.sort_by(|a, b| b.1.total_cmp(&a.1));
	let top: Vec<String> = apps.iter().take(5).map(|(app, _)| app.clone()).collect();
	if !top.is_empty() {
		let joined = top.join(", ");
		parts.push(format!("Most screen time in {joined}."));
		data.insert(
			"top_apps".into(),
			json!(
				apps.iter()
					.map(|(app, seconds)| json!({ "app": app, "seconds": seconds }))
					.collect::<Vec<_>>()
			),
		);
	}

	// Focus stretches of 25+ minutes.
	let spans = focus_spans(store, range)?;
	let focus_seconds: f64 = spans.iter().map(|(_, seconds)| seconds).sum();
	if spans.len() >= MIN_POINTS {
		parts.push(format!(
			"Focused for {} across {} stretches of {FOCUS_MINUTES}+ minutes.",
			hm(focus_seconds),
			spans.len()
		));
		data.insert(
			"focus_seconds".into(),
			json!({ "value": focus_seconds, "spans": spans.len() }),
		);
	}

	// Sleep on nights with screen use past 23:00 versus other nights, over
	// the last five weeks.
	{
		let window_start = monday - SignedDuration::from_hours(24 * 7 * WINDOW_WEEKS);
		let mut late: Vec<f64> = Vec::new();
		let mut other: Vec<f64> = Vec::new();
		let mut date = window_start;
		while date < monday + SignedDuration::from_hours(24 * 7) {
			if let Some(night) = sleep_night(store, date, tz)? {
				if late_screen(store, date, tz)? {
					late.push(night);
				} else {
					other.push(night);
				}
			}
			date = date
				.tomorrow()
				.map_err(|e| StoreError::Invalid(e.to_string()))?;
		}
		if late.len() + other.len() >= MIN_POINTS && !late.is_empty() && !other.is_empty() {
			let (late_avg, late_nights) = average(&late);
			let (other_avg, other_nights) = average(&other);
			parts.push(format!(
				"After screen use past 23:00 slept {} ({late_nights} nights), otherwise {} \
				 ({other_nights} nights).",
				hm(late_avg),
				hm(other_avg)
			));
			data.insert(
				"sleep_after_late_screen".into(),
				json!({
					"late_seconds_avg": late_avg,
					"late_nights": late_nights,
					"other_seconds_avg": other_avg,
					"other_nights": other_nights,
				}),
			);
		}
	}

	// Resting heart rate versus the previous day's steps, over the last
	// five weeks.
	{
		let window_start = monday - SignedDuration::from_hours(24 * 7 * WINDOW_WEEKS);
		let mut pairs: Vec<(f64, f64)> = Vec::new();
		let mut date = window_start;
		while date < monday + SignedDuration::from_hours(24 * 7) {
			let previous = date
				.yesterday()
				.map_err(|e| StoreError::Invalid(e.to_string()))?;
			if let (Some(resting), Some(steps)) = (
				resting_heart_rate(store, day_range(date, tz)?)?,
				steps_on(store, previous, tz)?,
			) {
				pairs.push((resting, steps));
			}
			date = date
				.tomorrow()
				.map_err(|e| StoreError::Invalid(e.to_string()))?;
		}
		if pairs.len() >= MIN_POINTS
			&& let Some(r) = pearson(&pairs)
		{
			parts.push(format!(
				"Resting heart rate and the previous day's steps correlate at r={r:.2} ({} days).",
				pairs.len()
			));
			data.insert(
				"resting_hr_vs_prior_steps".into(),
				json!({ "r": r, "days": pairs.len() }),
			);
		}
	}

	// The best two hours for focus, over the last five weeks.
	let by_hour = focus_by_hour(store, monday, tz)?;
	if by_hour
		.first()
		.is_some_and(|(_, _, points)| *points >= MIN_POINTS)
	{
		let best = &by_hour[..2.min(by_hour.len())];
		let shown: Vec<String> = best
			.iter()
			.map(|(hour, minutes, _)| format!("{hour}:00 with {minutes:.0}m"))
			.collect();
		parts.push(format!("Focus peaks at {}.", shown.join(" and ")));
		data.insert(
			"focus_by_hour".into(),
			json!(
				best.iter()
					.map(
						|(hour, minutes, points)| json!({ "hour": hour, "minutes": minutes, "spans": points })
					)
					.collect::<Vec<_>>()
			),
		);
	}

	if parts.is_empty() {
		return Ok(None);
	}
	let mut text = format!("Week of {monday}:");
	for part in parts {
		let _ = write!(text, " {part}");
	}

	let mut record = Record::new(stream("memory.weekly"), DERIVED_DEVICE)
		.starting(range.start)
		.ending(range.end)
		.with_text(text)
		.with("date", monday.to_string())
		.with("activity", "week")
		.with("derived_from", "weekly");
	for (key, value) in data {
		record = record.with(&key, value);
	}
	record.id = stable_id(&["weekly", &monday.to_string()]);
	Ok(Some(record))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn weeks_start_on_monday_in_the_owners_zone() {
		// 2026-11-08 is a Sunday; its week started on the 2nd.
		let sunday: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
		assert_eq!(monday_of(sunday, &TimeZone::UTC).to_string(), "2026-11-02");
		// 16:00 UTC is already Monday 01:00 in Tokyo, so the same instant
		// belongs to the next week there.
		let evening: Timestamp = "2026-11-08T16:00:00Z".parse().unwrap();
		assert_eq!(
			monday_of(evening, &TimeZone::get("Asia/Tokyo").unwrap()).to_string(),
			"2026-11-09"
		);
		assert_eq!(
			week_start("2026-11-06".parse::<Date>().unwrap()).to_string(),
			"2026-11-02"
		);
	}

	#[test]
	fn averages_count_their_points() {
		assert_eq!(average(&[]), (0.0, 0));
		assert_eq!(average(&[2.0, 4.0, 6.0]), (4.0, 3));
	}

	#[test]
	fn pearson_needs_variance_on_both_sides() {
		assert_eq!(pearson(&[(1.0, 2.0), (2.0, 4.0), (3.0, 6.0)]), Some(1.0));
		assert_eq!(pearson(&[(1.0, 2.0), (2.0, 2.0), (3.0, 2.0)]), None);
		assert_eq!(pearson(&[(1.0, 2.0), (2.0, 3.0), (3.0, 4.0)]), Some(1.0));
		assert_eq!(pearson(&[(1.0, 4.0), (2.0, 3.0), (3.0, 2.0)]), Some(-1.0));
		assert_eq!(pearson(&[]), None);
	}

	#[test]
	fn minutes_read_as_hours_and_minutes() {
		assert_eq!(hm(3_600.0), "1h 00m");
		assert_eq!(hm(7_500.0), "2h 05m");
		assert_eq!(hm(90.0), "2m");
	}
}
