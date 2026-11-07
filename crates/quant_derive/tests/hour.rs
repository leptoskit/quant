//! Deriving an hour from screen, place, posture, and body.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::TimeRange;
use quant_crypto::VaultKey;
use quant_derive::Config;
use quant_derive::DERIVED_DEVICE;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;

fn at(hour: i64, minute: i64) -> Timestamp {
	"2026-11-06T00:00:00Z".parse::<Timestamp>().unwrap()
		+ SignedDuration::from_hours(hour)
		+ SignedDuration::from_mins(minute)
}

#[test]
fn an_hour_becomes_one_episode() {
	let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
	let tz = TimeZone::UTC;
	let device = DeviceId::new();
	let focus = |from: Timestamp, to: Timestamp, app: &str| {
		Record::new("apps.focus".parse().unwrap(), device)
			.starting(from)
			.ending(to)
			.with("app", app)
	};
	let posture = |from: Timestamp, to: Timestamp, label: &str| {
		Record::new("motion.posture".parse().unwrap(), device)
			.starting(from)
			.ending(to)
			.with("posture", label)
	};
	store
		.insert_records(&[
			focus(at(10, 0), at(10, 38), "VS Code"),
			focus(at(10, 38), at(10, 50), "Slack"),
			posture(at(10, 0), at(10, 50), "sitting"),
			Record::new("location.visit".parse().unwrap(), device)
				.starting(at(10, 0))
				.ending(at(11, 0))
				.with("name", "Office"),
			Record::new("audio.transcript".parse().unwrap(), device)
				.starting(at(10, 10))
				.ending(at(10, 13)),
			Record::new("audio.transcript".parse().unwrap(), device)
				.starting(at(10, 40))
				.ending(at(10, 43)),
		])
		.unwrap();
	store
		.insert_series(&SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device,
			start: at(10, 0),
			channels: vec!["bpm".into()],
			offsets_ms: (0..60).map(|m| m * 60_000).collect(),
			values: (0..60).map(|m| if m < 30 { 62.0 } else { 80.0 }).collect(),
		})
		.unwrap();
	store
		.insert_series(&SeriesBatch {
			stream: "health.steps".parse().unwrap(),
			device,
			start: at(10, 0),
			channels: vec!["steps".into()],
			offsets_ms: vec![0, 300_000],
			values: vec![220.0, 200.0],
		})
		.unwrap();

	let config = Config {
		posture_sources: vec![],
		first_run_days: 1,
	};
	let report = quant_derive::run(&store, &config, at(11, 5), &tz).unwrap();
	assert_eq!(report.episodes, 1);

	let range = TimeRange::new(at(0, 0), at(23, 0)).unwrap();
	let episodes = store
		.records(
			&RecordQuery::new(Selector::stream("memory.episode".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	assert_eq!(episodes.len(), 1);
	let episode = &episodes[0];
	assert_eq!(episode.start, at(10, 0));
	assert_eq!(episode.end, Some(at(11, 0)));
	assert_eq!(episode.device, DERIVED_DEVICE);
	assert_eq!(episode.data["derived_from"], "episode");
	assert_eq!(episode.data["apps"][0]["app"], "VS Code");
	assert_eq!(episode.data["apps"][0]["seconds"], 2_280.0);
	assert_eq!(episode.data["apps"][1]["app"], "Slack");
	assert_eq!(episode.data["apps"][1]["seconds"], 720.0);
	assert_eq!(episode.data["place"], "Office");
	assert_eq!(episode.data["posture_seconds"]["sitting"], 3_000.0);
	assert_eq!(episode.data["hr_min"], 62.0);
	assert_eq!(episode.data["hr_mean"], 71.0);
	assert_eq!(episode.data["hr_max"], 80.0);
	assert_eq!(episode.data["steps"], 420.0);
	assert_eq!(episode.data["speech_seconds"], 360.0);
	let text = episode.text.as_deref().unwrap();
	assert_eq!(
		text,
		"10:00–11:00: mostly VS Code (38 min) and Slack (12 min), at Office, sitting 50 min, \
		 heart rate 62–80 bpm (avg 71), 420 steps, 6 min of speech."
	);

	// The empty hour before it produced no episode.
	assert!(!episodes.iter().any(|e| e.start == at(9, 0)));

	// Running again replaces the same record rather than adding one.
	quant_derive::run(&store, &config, at(11, 5), &tz).unwrap();
	let again = store
		.records(
			&RecordQuery::new(Selector::stream("memory.episode".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	assert_eq!(again.len(), 1);
	assert_eq!(again[0].id, episode.id);
	assert_eq!(again[0].text, episode.text);
}
