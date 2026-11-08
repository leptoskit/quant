//! A week of body and behaviour becomes a `memory.weekly` review.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::TimeRange;
use quant_crypto::VaultKey;
use quant_derive::Config;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;

#[test]
fn a_week_becomes_a_weekly_review() {
	let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
	let tz = TimeZone::UTC;
	// Monday 2026-11-02 through Saturday; the run happens on Sunday.
	let monday: Timestamp = "2026-11-02T00:00:00Z".parse().unwrap();
	let now: Timestamp = "2026-11-08T12:00:00Z".parse().unwrap();
	let at = |day: i64, h: i64, m: i64| {
		monday + SignedDuration::from_hours(24 * day + h) + SignedDuration::from_mins(m)
	};
	let watch = DeviceId::new();

	for day in 0..6 {
		// Eight hours of sleep the night ending on this day.
		store
			.insert_records(&[
				Record::new("health.sleep".parse().unwrap(), watch)
					.starting(at(day - 1, 23, 0))
					.ending(at(day, 3, 0))
					.with("stage", "light"),
				Record::new("health.sleep".parse().unwrap(), watch)
					.starting(at(day, 3, 0))
					.ending(at(day, 7, 0))
					.with("stage", "deep"),
			])
			.unwrap();
		// A half-hour focus span each morning.
		store
			.insert_records(std::slice::from_ref(
				&Record::new("apps.focus".parse().unwrap(), DeviceId::new())
					.starting(at(day, 9, 0))
					.ending(at(day, 9, 30))
					.with("app", "Zed"),
			))
			.unwrap();
		// Resting heart rate each afternoon.
		store
			.insert_series(&SeriesBatch {
				stream: "health.heart_rate".parse().unwrap(),
				device: watch,
				start: at(day, 13, 0),
				channels: vec!["bpm".into()],
				offsets_ms: (0..30).map(|i| i * 60_000).collect(),
				values: vec![55.0; 30],
			})
			.unwrap();
		// Steps each day.
		store
			.insert_series(&SeriesBatch {
				stream: "health.steps".parse().unwrap(),
				device: watch,
				start: at(day, 8, 0),
				channels: vec!["steps".into()],
				offsets_ms: vec![0, 3_600_000],
				values: vec![2_500.0, 2_500.0],
			})
			.unwrap();
	}

	let config = Config {
		posture_sources: Vec::new(),
		first_run_days: 1,
	};
	let report = quant_derive::run(&store, &config, now, &tz).unwrap();
	assert!(report.weeks > 0, "no weekly review written");

	let weekly = store
		.records(&RecordQuery::new(
			Selector::stream("memory.weekly".parse().unwrap()),
			TimeRange::new(monday, now).unwrap(),
		))
		.unwrap();
	assert_eq!(weekly.len(), 1);
	let review = &weekly[0];
	let text = review.text.as_deref().unwrap();
	assert!(text.starts_with("Week of 2026-11-02"), "{text}");
	assert!(
		text.contains("Slept on average 8h 00m per night (6 nights)."),
		"{text}"
	);
	assert!(
		text.contains("Walked 5000 steps a day on average (6 days)."),
		"{text}"
	);
	assert!(
		text.contains("Resting heart rate averaged 55 bpm (6 days)."),
		"{text}"
	);
	assert!(text.contains("Most screen time in Zed."), "{text}");
	assert!(
		text.contains("Focused for 3h 00m across 6 stretches of 25+ minutes."),
		"{text}"
	);
	assert!(text.contains("Focus peaks at 9:00 with 180m."), "{text}");
	// The structured data carries the same numbers.
	assert_eq!(review.data["sleep_seconds_avg"]["value"], 28_800.0);
	assert_eq!(review.data["sleep_seconds_avg"]["nights"], 6);
	assert_eq!(review.data["steps_avg"]["days"], 6);
	// The previous week has no sleep data, so no change is stated.
	assert!(review.data.get("sleep_change_seconds").is_none());

	// Running again replaces, never duplicates.
	quant_derive::run(&store, &config, now, &tz).unwrap();
	let again = store
		.records(&RecordQuery::new(
			Selector::stream("memory.weekly".parse().unwrap()),
			TimeRange::new(monday, now).unwrap(),
		))
		.unwrap();
	assert_eq!(again.len(), 1);
	assert_eq!(again[0].id, review.id);
}
