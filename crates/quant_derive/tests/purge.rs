//! A purge takes the derived records that quoted its sources, and the next
//! derivation run recomputes them from what remains.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::DeviceId;
use quant_core::Family;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::TimeRange;
use quant_crypto::VaultKey;
use quant_derive::Config;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;

/// Every derived record overlapping a day, from the memory family.
fn derived(store: &Store, day: Timestamp) -> Vec<Record> {
	store
		.records(&RecordQuery::new(
			Selector::family(Family::Memory),
			TimeRange::new(day, day + SignedDuration::from_hours(24)).unwrap(),
		))
		.unwrap()
}

fn texts(records: &[Record]) -> Vec<&str> {
	records
		.iter()
		.map(|record| record.text.as_deref().unwrap_or_default())
		.collect()
}

#[test]
fn purging_heart_rate_removes_it_from_derived_records() {
	let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
	let tz = TimeZone::UTC;
	let day: Timestamp = "2026-11-06T00:00:00Z".parse().unwrap();
	let at = |h: i64, m: i64| day + SignedDuration::from_hours(h) + SignedDuration::from_mins(m);
	let watch = DeviceId::new();

	// Heart rate through the 13:00 hour and a morning focus span, so the
	// day's summary and episodes have something to say.
	store
		.insert_series(&SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device: watch,
			start: at(13, 0),
			channels: vec!["bpm".into()],
			offsets_ms: (0..30).map(|i| i * 60_000).collect(),
			values: (0..30).map(|i| 60.0 + f64::from(i)).collect(),
		})
		.unwrap();
	let focus = Record::new("apps.focus".parse().unwrap(), DeviceId::new())
		.starting(at(9, 0))
		.ending(at(11, 0))
		.with("app", "Zed");
	store.insert_records(std::slice::from_ref(&focus)).unwrap();

	let config = Config {
		posture_sources: Vec::new(),
		first_run_days: 1,
	};
	let now = at(15, 0);
	quant_derive::run(&store, &config, now, &tz).unwrap();

	// The day's derived records quote heart rate.
	let before = derived(&store, day);
	assert!(
		before.iter().any(|record| {
			record
				.text
				.as_deref()
				.is_some_and(|text| text.contains("heart rate"))
		}),
		"{:?}",
		texts(&before)
	);

	// Purging the day's heart rate takes the derived records that quoted it
	// and resets the derivation watermark.
	let range = TimeRange::new(day, day + SignedDuration::from_hours(24)).unwrap();
	let (records, series, derived_gone) = store
		.purge(
			&Selector::stream("health.heart_rate".parse().unwrap()),
			range,
		)
		.unwrap();
	assert_eq!(records, 0);
	assert!(series > 0);
	assert!(derived_gone > 0, "no derived records went with the purge");
	assert!(
		store
			.meta("derive.episode")
			.unwrap()
			.and_then(|value| value.parse::<Timestamp>().ok())
			.is_some_and(|watermark| watermark <= day),
		"the episode watermark was not reset to the purge"
	);

	// Nothing derived from that day still mentions heart rate.
	let left = derived(&store, day);
	assert!(
		left.iter().all(|record| {
			!record
				.text
				.as_deref()
				.unwrap_or_default()
				.contains("heart rate")
		}),
		"{:?}",
		texts(&left)
	);

	// The next run recomputes from what remains: the focus span is back,
	// heart rate is not.
	quant_derive::run(&store, &config, now, &tz).unwrap();
	let recomputed = derived(&store, day);
	assert!(
		recomputed.iter().any(|record| {
			record
				.text
				.as_deref()
				.is_some_and(|text| text.contains("Zed"))
		}),
		"{:?}",
		texts(&recomputed)
	);
	assert!(
		recomputed.iter().all(|record| {
			!record
				.text
				.as_deref()
				.unwrap_or_default()
				.contains("heart rate")
		}),
		"{:?}",
		texts(&recomputed)
	);
}
