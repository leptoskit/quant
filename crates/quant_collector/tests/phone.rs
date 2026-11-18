//! Location, app usage, battery and thermal readings from the Leptoskit
//! plugins, queued on the phone and drained into a live daemon.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use leptoskit_plugin_app_usage::EventKind;
use leptoskit_plugin_app_usage::UsageEvent;
use leptoskit_plugin_app_usage::UsageSession;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_location::Fix;
use leptoskit_plugin_location::Provider;
use leptoskit_plugin_location::Updates;
use leptoskit_plugin_thermal as thermal;
use quant_access::AgentIdentity;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::RecordsRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_client::Purpose;
use quant_collector::PowerMonitor;
use quant_collector::Spool;
use quant_collector::UploadBudget;
use quant_collector::Uploader;
use quant_core::Class;
use quant_core::DeviceId;
use quant_core::SeriesBatch;
use quant_core::TimeRange;
use quant_crypto::KdfParams;
use quant_crypto::VaultKey;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Vault;
use quant_store::VaultPaths;

const PASS: &str = "correct horse battery staple";

struct Daemon {
	state: Arc<quant_daemon::State>,
	endpoint: Endpoint,
	_dir: tempfile::TempDir,
}

async fn daemon() -> Daemon {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("v"));
	let state = Arc::new(
		quant_daemon::State::new(
			Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap(),
		)
		.unwrap(),
	);
	let socket = paths.socket();
	let serving = Arc::clone(&state);
	let path = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &path, std::future::pending())
			.await
			.unwrap();
	});
	while !socket.exists() {
		tokio::time::sleep(std::time::Duration::from_millis(5)).await;
	}
	Daemon {
		state,
		endpoint: Endpoint::Unix(socket),
		_dir: dir,
	}
}

impl Daemon {
	async fn paired(
		&self,
		name: &str,
		kind: ClientKind,
		scopes: &[&str],
		deny: &[Class],
	) -> Client {
		let mut client = Client::new(self.endpoint.clone(), Identity::generate(name));
		let agent = (kind == ClientKind::Agent).then(|| {
			AgentIdentity {
				product: name.to_owned(),
				vendor: "Quant tests".into(),
				model: "test-model".into(),
				version: env!("CARGO_PKG_VERSION").into(),
				host: "test-host".into(),
				operator: None,
			}
		});
		let pairing = client.pair(kind, &[], agent.as_ref()).await.unwrap();
		OwnerSession::open(self.endpoint.clone(), PASS)
			.await
			.unwrap()
			.approve(&ApproveRequest {
				client: pairing.pairing_code,
				scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
				label: None,
				lookback_hours: None,
				expires_in_hours: None,
				limits: None,
				unredacted: false,
				deny_classes: (!deny.is_empty()).then(|| deny.to_vec()),
			})
			.await
			.unwrap();
		client
	}
}

fn purpose() -> Purpose {
	Purpose::purpose("checking where the owner's phone has been today")
}

fn ms(at: Timestamp) -> i64 {
	at.as_millisecond()
}

#[allow(clippy::cast_precision_loss)]
fn fix(at: Timestamp, latitude: f64, altitude: Option<f64>) -> Fix {
	Fix {
		timestamp_ms: ms(at) as f64,
		latitude,
		longitude: -0.1276,
		accuracy_m: 12.0,
		altitude_m: altitude,
		altitude_accuracy_m: altitude.map(|_| 4.0),
		heading_degrees: None,
		speed_mps: Some(1.2),
		provider: Provider::Fused,
		simulated: Some(false),
	}
}

fn battery(thousandths: u16, state: battery::State, power_save: bool) -> battery::Snapshot {
	battery::Snapshot {
		present: Some(true),
		charge: Some(battery::Charge::new(thousandths).unwrap()),
		state,
		power_save: Some(power_save),
	}
}

#[tokio::test]
async fn phone_plugin_readings_reach_the_vault_under_their_grants() {
	let daemon = daemon().await;
	let phone = daemon
		.paired(
			"Pixel",
			ClientKind::Device,
			&["location:write", "apps:write", "device:write"],
			&[],
		)
		.await;
	let dir = tempfile::tempdir().unwrap();
	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	let device = DeviceId::new();
	let now = Timestamp::from_millisecond(ms(Timestamp::now())).unwrap();
	let ago = |minutes: i64| now - SignedDuration::from_mins(minutes);

	// Location: fixes arrive out of order, one is a mock location, and the
	// native buffer dropped two before the host read the batch.
	let mut mock = fix(ago(40), 51.6, Some(30.0));
	mock.simulated = Some(true);
	let updates = Updates {
		dropped: 2,
		fixes: vec![
			fix(ago(30), 51.5010, Some(31.0)),
			fix(ago(50), 51.5000, Some(30.0)),
			mock,
			fix(ago(20), 51.5020, None),
		],
	};
	let report = spool.location_updates(device, &updates).unwrap();
	assert_eq!(
		(
			report.stored,
			report.simulated,
			report.invalid,
			report.dropped
		),
		(3, 1, 0, 2)
	);
	assert!(
		spool.sources()["location"]
			.detail
			.as_deref()
			.unwrap()
			.contains("dropped 2")
	);
	// Replaying the same batch is harmless: batches are stored by content.
	spool.location_updates(device, &updates).unwrap();

	// App usage: two sessions and the transitions around them.
	let sessions = vec![
		UsageSession {
			package: "com.android.chrome".into(),
			label: "Chrome".into(),
			start_ms: ms(ago(45)),
			end_ms: ms(ago(35)),
		},
		UsageSession {
			package: "org.example.notes".into(),
			label: String::new(),
			start_ms: ms(ago(35)),
			end_ms: ms(ago(25)),
		},
		// Empty sessions carry no time and are skipped.
		UsageSession {
			package: "org.example.blip".into(),
			label: "Blip".into(),
			start_ms: ms(ago(25)),
			end_ms: ms(ago(25)),
		},
	];
	assert_eq!(spool.app_sessions(device, &sessions).unwrap(), 2);
	assert_eq!(spool.app_sessions(device, &sessions).unwrap(), 2);
	let events = vec![
		UsageEvent {
			kind: EventKind::KeyguardHidden,
			timestamp_ms: ms(ago(46)),
			package: None,
		},
		UsageEvent {
			kind: EventKind::ActivityResumed,
			timestamp_ms: ms(ago(45)),
			package: Some("com.android.chrome".into()),
		},
		UsageEvent {
			kind: EventKind::ScreenNonInteractive,
			timestamp_ms: ms(ago(24)),
			package: None,
		},
	];
	assert_eq!(spool.usage_events(device, &events).unwrap(), 2);
	assert_eq!(spool.usage_events(device, &events).unwrap(), 2);

	// Power: levels go to device.battery, state changes to device.power,
	// and an unchanged reading stores nothing.
	let mut power = PowerMonitor::new();
	let cool = thermal::Snapshot::from_android(thermal::AndroidStatus::None);
	let warm = thermal::Snapshot::from_android(thermal::AndroidStatus::Moderate);
	let first = power
		.observe(
			&spool,
			device,
			ago(50),
			Some(battery(812, battery::State::Discharging, false)),
			Some(cool),
		)
		.unwrap();
	assert!(first.level && first.changed);
	let same = power
		.observe(
			&spool,
			device,
			ago(45),
			Some(battery(812, battery::State::Discharging, false)),
			Some(cool),
		)
		.unwrap();
	assert!(!same.level && !same.changed);
	let level_only = power
		.observe(
			&spool,
			device,
			ago(40),
			Some(battery(805, battery::State::Discharging, false)),
			Some(cool),
		)
		.unwrap();
	assert!(level_only.level && !level_only.changed);
	let hot = power
		.observe(
			&spool,
			device,
			ago(30),
			Some(battery(801, battery::State::Charging, true)),
			Some(warm),
		)
		.unwrap();
	assert!(hot.level && hot.changed);
	// Warm and in low-power mode: a quarter of the default upload work.
	let budget = power.budget();
	assert_eq!(budget.max_records, UploadBudget::default().max_records / 4);

	let pending = spool.pending().unwrap();
	assert_eq!((pending.records, pending.batches), (6, 5));

	let delivered = Uploader::new(&spool, &phone)
		.drain_with_budget(UploadBudget::default())
		.await
		.unwrap();
	assert_eq!((delivered.records, delivered.batches), (6, 5));
	assert_eq!(
		spool.pending().unwrap(),
		quant_collector::Pending::default()
	);

	// The vault holds what the plugins reported.
	let store = &daemon.state.vault.store;
	let range = TimeRange::new(ago(60), now).unwrap();
	let fixes = store
		.series(&"location.fix".parse().unwrap(), range, 10_000)
		.unwrap();
	assert_eq!(fixes.iter().map(SeriesBatch::len).sum::<usize>(), 3);
	let with_altitude = fixes
		.iter()
		.find(|batch| batch.channels.len() == 4)
		.unwrap();
	assert_eq!(with_altitude.start, ago(50));
	assert_eq!(with_altitude.offsets_ms, vec![0, 20 * 60 * 1000]);
	assert_eq!(&with_altitude.values[..4], &[51.5, -0.1276, 12.0, 30.0]);
	let without = fixes
		.iter()
		.find(|batch| batch.channels.len() == 3)
		.unwrap();
	assert_eq!(without.channels, vec!["lat", "lon", "accuracy"]);

	let focus = store
		.records(
			&RecordQuery::new(Selector::stream("apps.focus".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	let apps: Vec<_> = focus
		.iter()
		.map(|r| {
			(
				r.data["app"].as_str().unwrap(),
				r.data["bundle_id"].as_str().unwrap(),
			)
		})
		.collect();
	assert_eq!(
		apps,
		vec![
			("Chrome", "com.android.chrome"),
			("org.example.notes", "org.example.notes")
		]
	);
	let states = store
		.records(
			&RecordQuery::new(Selector::stream("device.state".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	let states: Vec<_> = states
		.iter()
		.map(|r| r.data["state"].as_str().unwrap())
		.collect();
	assert_eq!(states, vec!["unlocked", "display_off"]);

	let levels: Vec<f64> = store
		.series(&"device.battery".parse().unwrap(), range, 100)
		.unwrap()
		.iter()
		.flat_map(|batch| batch.values.clone())
		.collect();
	assert_eq!(levels.len(), 3);
	assert!((levels[0] - 81.2).abs() < 0.01, "{levels:?}");
	let power_records = store
		.records(
			&RecordQuery::new(Selector::stream("device.power".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	assert_eq!(power_records.len(), 2);
	assert_eq!(power_records[1].data["charging"], "charging");
	assert_eq!(power_records[1].data["thermal"], "fair");
	assert_eq!(power_records[1].data["android_thermal"], "moderate");
	assert_eq!(power_records[1].data["low_power"], true);
	assert_eq!(
		power_records[1].text.as_deref(),
		Some("Phone battery 80%, charging, low power mode on, thermal state fair.")
	);

	// A grant that denies location sees the phone's apps and power state but
	// never where it has been; the same scopes without the denial do.
	let cautious = daemon
		.paired(
			"Cautious",
			ClientKind::Agent,
			&["location:read", "apps:read", "device:read"],
			&[Class::Location],
		)
		.await;
	let read = cautious
		.records(
			&RecordsRequest {
				streams: vec![
					"location.fix".into(),
					"apps.focus".into(),
					"device.power".into(),
				],
				..Default::default()
			},
			Some(&purpose()),
		)
		.await
		.unwrap();
	assert_eq!(read.withheld, vec!["location.fix"]);
	assert_eq!(read.records.len(), 4);
	let fixes_request = SeriesRequest {
		stream: "location.fix".into(),
		from: Some(ago(60)),
		to: Some(now),
		bucket_seconds: None,
		raw: true,
		max_rows: None,
	};
	assert_eq!(
		cautious
			.series(&fixes_request, Some(&purpose()))
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);
	let trusted = daemon
		.paired(
			"Trusted",
			ClientKind::Agent,
			&["location:read", "apps:read", "device:read"],
			&[],
		)
		.await;
	assert!(
		trusted
			.series(&fixes_request, Some(&purpose()))
			.await
			.is_ok()
	);

	// The phone's device grant writes; it never reads back.
	let own = phone
		.records(&RecordsRequest::default(), None)
		.await
		.map(|read| read.records.len());
	assert!(matches!(own, Ok(0) | Err(_)), "{own:?}");
}

#[test]
fn hot_phones_pause_uploads_and_cool_charging_phones_upload_fully() {
	let critical = Some(thermal::Snapshot::from_android(
		thermal::AndroidStatus::Severe,
	));
	let paused = UploadBudget::for_power(None, critical);
	assert_eq!(
		(
			paused.max_records,
			paused.max_batches,
			paused.max_media_bytes
		),
		(0, 0, 0)
	);
	assert!(paused.max_duration.is_zero());

	let low = UploadBudget::for_power(Some(battery(150, battery::State::Discharging, false)), None);
	assert_eq!(low.max_records, UploadBudget::default().max_records / 4);

	let plugged = UploadBudget::for_power(
		Some(battery(150, battery::State::Charging, false)),
		Some(thermal::Snapshot::from_android(
			thermal::AndroidStatus::None,
		)),
	);
	assert_eq!(plugged.max_records, UploadBudget::default().max_records);
	assert_eq!(
		UploadBudget::for_power(None, None).max_batches,
		UploadBudget::default().max_batches
	);
}

#[test]
fn invalid_fixes_are_counted_not_stored() {
	let dir = tempfile::tempdir().unwrap();
	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	let mut bad = fix(Timestamp::now(), 91.0, None);
	bad.longitude = 0.0;
	let mut timeless = fix(Timestamp::now(), 10.0, None);
	timeless.timestamp_ms = f64::NAN;
	let report = spool
		.location_updates(
			DeviceId::new(),
			&Updates {
				dropped: 0,
				fixes: vec![bad, timeless],
			},
		)
		.unwrap();
	assert_eq!((report.stored, report.invalid), (0, 2));
	assert_eq!(spool.pending().unwrap().batches, 0);
	assert!(!spool.sources().contains_key("location"));
}
