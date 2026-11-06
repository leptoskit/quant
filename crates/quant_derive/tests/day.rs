//! Deriving a realistic stretch of a day.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::TimeRange;
use quant_crypto::VaultKey;
use quant_derive::Config;
use quant_derive::posture::Placement;
use quant_derive::posture::Source;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;

const G: f64 = 9.81;

fn accel(
	device: DeviceId,
	start: Timestamp,
	seconds: u32,
	gravity: [f64; 3],
	shake: f64,
) -> SeriesBatch {
	let hz = 10;
	let n = seconds * hz;
	let mut values = Vec::new();
	for i in 0..n {
		let phase = f64::from(i) * 0.8;
		values.extend([
			gravity[0] + shake * phase.sin(),
			gravity[1] + shake * phase.cos(),
			gravity[2],
		]);
	}
	SeriesBatch {
		stream: "sensor.vest.accel".parse().unwrap(),
		device,
		start,
		channels: vec!["x".into(), "y".into(), "z".into()],
		offsets_ms: (0..n).map(|i| i * 1000 / hz).collect(),
		values,
	}
}

#[test]
fn a_day_becomes_posture_spans_and_a_summary() {
	let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
	let tz = TimeZone::UTC;
	let day: Timestamp = "2026-11-06T00:00:00Z".parse().unwrap();
	let at = |h: i64, m: i64| day + SignedDuration::from_hours(h) + SignedDuration::from_mins(m);
	let vest = DeviceId::new();
	let watch = DeviceId::new();

	// 13:00–13:30 lying, 13:30–13:40 walking, 13:40–14:00 upright.
	store
		.insert_series(&accel(vest, at(13, 0), 1800, [0.0, 0.0, G], 0.02))
		.unwrap();
	store
		.insert_series(&accel(vest, at(13, 30), 600, [0.0, G, 0.0], 1.5))
		.unwrap();
	store
		.insert_series(&accel(vest, at(13, 40), 1200, [0.0, G, 0.0], 0.02))
		.unwrap();

	// Heart rate once a minute, low while lying.
	let bpm: Vec<f64> = (0..60).map(|m| if m < 30 { 54.0 } else { 88.0 }).collect();
	store
		.insert_series(&SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device: watch,
			start: at(13, 0),
			channels: vec!["bpm".into()],
			offsets_ms: (0..60).map(|m| m * 60_000).collect(),
			values: bpm,
		})
		.unwrap();
	store
		.insert_series(&SeriesBatch {
			stream: "health.steps".parse().unwrap(),
			device: watch,
			start: at(13, 30),
			channels: vec!["steps".into()],
			offsets_ms: vec![0, 300_000],
			values: vec![600.0, 550.0],
		})
		.unwrap();
	let focus = |from: (i64, i64), to: (i64, i64), app: &str| {
		Record::new("apps.focus".parse().unwrap(), DeviceId::new())
			.starting(at(from.0, from.1))
			.ending(at(to.0, to.1))
			.with("app", app)
	};
	let sleep = |from: Timestamp, to: Timestamp, stage: &str| {
		Record::new("health.sleep".parse().unwrap(), watch)
			.starting(from)
			.ending(to)
			.with("stage", stage)
	};
	store
		.insert_records(&[
			focus((9, 0), (11, 0), "Zed"),
			focus((11, 0), (11, 30), "Safari"),
			sleep(day - SignedDuration::from_hours(1), at(2, 0), "light"),
			sleep(at(2, 0), at(3, 30), "deep"),
			sleep(at(3, 30), at(6, 0), "light"),
		])
		.unwrap();

	let config = Config {
		posture_sources: vec![Source {
			stream: "sensor.vest.accel".parse().unwrap(),
			placement: Placement::Torso,
			up: [0.0, 1.0, 0.0],
		}],
		first_run_days: 1,
	};
	let now = at(15, 0);
	let report = quant_derive::run(&store, &config, now, &tz).unwrap();
	assert_eq!(report.days, 2);

	let range = TimeRange::new(at(12, 0), at(15, 0)).unwrap();
	let postures = store
		.records(
			&RecordQuery::new(Selector::stream("motion.posture".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	let labels: Vec<_> = postures
		.iter()
		.map(|r| r.data["posture"].as_str().unwrap().to_owned())
		.collect();
	assert_eq!(labels, ["lying", "walking", "upright"]);
	assert_eq!(postures[0].start, at(13, 0));

	// Running again changes nothing.
	quant_derive::run(&store, &config, now, &tz).unwrap();
	let again = store
		.records(
			&RecordQuery::new(Selector::stream("motion.posture".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	assert_eq!(again.len(), 3);

	let daily = store
		.records(&RecordQuery::new(
			Selector::stream("memory.daily".parse().unwrap()),
			TimeRange::new(day, at(23, 0)).unwrap(),
		))
		.unwrap();
	let today = daily
		.iter()
		.find(|r| r.data["date"] == "2026-11-06")
		.unwrap();
	let text = today.text.as_deref().unwrap();
	assert!(
		text.contains("2h 30m on screen (Zed 2h 00m, Safari 30m)"),
		"{text}"
	);
	assert!(text.contains("1150 steps"), "{text}");
	assert!(text.contains("resting about 54"), "{text}");
	assert!(text.contains("Slept 7h 00m (deep 1h 30m)"), "{text}");
	assert!(text.contains("lying 30m"), "{text}");
	assert_eq!(today.data["heart_rate"]["min"], 54.0);
}
