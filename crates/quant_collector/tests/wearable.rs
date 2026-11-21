//! Wearable, health-store, pedometer, motion, and BLE GATT readings queued on
//! a watch or phone and drained into a live `quantd` daemon.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_ble::gatt;
use leptoskit_plugin_health as health;
use leptoskit_plugin_pedometer as pedometer;
use leptoskit_plugin_wearable as wearable;
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
use quant_collector::Spool;
use quant_collector::Uploader;
use quant_core::Class;
use quant_core::DeviceId;
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
		let agent = (kind == ClientKind::Agent).then(|| AgentIdentity {
			product: name.to_owned(),
			vendor: "Quant tests".into(),
			model: "test-model".into(),
			version: env!("CARGO_PKG_VERSION").into(),
			host: "test-host".into(),
			operator: None,
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
	Purpose::purpose("inspecting wearable health and sensor readings")
}

#[tokio::test]
async fn watch_health_ble_and_pedometer_drain_and_enforce_health_denial() {
	let daemon = daemon().await;
	let watch_client = daemon
		.paired(
			"Apple Watch Ultra",
			ClientKind::Device,
			&[
				"health:write",
				"motion:write",
				"device:write",
				"sensor:write",
			],
			&[],
		)
		.await;
	let denied_agent = daemon
		.paired(
			"Non-health assistant",
			ClientKind::Agent,
			&["health:read", "device:read", "sensor:read"],
			&[Class::Health],
		)
		.await;
	let health_agent = daemon
		.paired(
			"Health assistant",
			ClientKind::Agent,
			&["health:read", "device:read", "sensor:read"],
			&[],
		)
		.await;

	let spool_dir = tempfile::tempdir().unwrap();
	let spool = Spool::open(spool_dir.path(), &VaultKey::generate()).unwrap();
	let device = DeviceId::new();
	let now = Timestamp::now();
	let t0 = (now - SignedDuration::from_mins(10)).as_millisecond();
	let t1 = (now - SignedDuration::from_mins(5)).as_millisecond();

	// 1. Direct watch batch (HR, HRV IBI, SpO2, skin temp, steps, wear state,
	//    stress, BIA, empty BIA skipped, ECG).
	let watch_batch = wearable::Batch {
		dropped: 0,
		readings: vec![
			wearable::Reading::HeartRate {
				time_ms: t0,
				bpm: 72.0,
				accuracy: wearable::Accuracy::High,
			},
			wearable::Reading::HeartRate {
				time_ms: t0 + 1_000,
				bpm: 88.0,
				accuracy: wearable::Accuracy::NoContact,
			},
			wearable::Reading::HrvIbi {
				time_ms: t0,
				rmssd_ms: None,
				rr_intervals_ms: vec![820.0, 860.0, 830.0],
			},
			wearable::Reading::SpO2 {
				time_ms: t0,
				percent: 98.5,
			},
			wearable::Reading::SkinTemperature {
				time_ms: t0,
				celsius: 36.4,
			},
			wearable::Reading::Steps {
				start_ms: t0,
				end_ms: t1,
				count: 420,
			},
			wearable::Reading::WearState {
				time_ms: t0,
				on_wrist: true,
			},
			wearable::Reading::Stress {
				time_ms: t0,
				score: 24.0,
			},
			wearable::Reading::BodyComposition {
				time_ms: t0,
				body_fat_percent: Some(16.2),
				skeletal_muscle_kg: Some(37.8),
				water_percent: Some(61.0),
			},
			// Empty BodyComposition must be skipped rather than storing an
			// empty record.
			wearable::Reading::BodyComposition {
				time_ms: t0 + 5_000,
				body_fat_percent: None,
				skeletal_muscle_kg: None,
				water_percent: None,
			},
			wearable::Reading::Ecg {
				time_ms: t0 + 10_000,
				classification: wearable::EcgClassification::SinusRhythm,
				average_bpm: Some(64.0),
			},
		],
	};
	let w_report = spool.wearable_batch(device, &watch_batch).unwrap();
	assert_eq!(w_report.latest_heart_rate_bpm, Some(64));
	assert_eq!(w_report.steps_delta, 420);
	assert_eq!(w_report.stored, 9); // NoContact HR + empty BIA skipped

	// Replaying the identical batch is idempotent.
	let w_replay = spool.wearable_batch(device, &watch_batch).unwrap();
	assert_eq!(w_replay.stored, 9);

	// 2. Health store samples (resting HR, sleep, workout, RMSSD vs SDNN HRV).
	let health_samples = vec![
		health::Sample {
			id: "rhr-1".into(),
			start_ms: t0,
			end_ms: t0,
			source: health::Source {
				app_id: Some("com.apple.health".into()),
				app_name: Some("Apple Watch".into()),
				device: Some("Watch7,1".into()),
			},
			value: health::Value::RestingHeartRate { bpm: 54.0 },
		},
		health::Sample {
			id: "sleep-1".into(),
			start_ms: t0,
			end_ms: t1,
			source: health::Source::default(),
			value: health::Value::Sleep {
				stages: vec![health::SleepStage {
					start_ms: t0,
					end_ms: t1,
					stage: health::Stage::Deep,
				}],
			},
		},
		health::Sample {
			id: "workout-1".into(),
			start_ms: t0,
			end_ms: t1,
			source: health::Source::default(),
			value: health::Value::Exercise {
				activity: health::Activity::Running,
				platform_code: 37,
				title: Some("Morning run".into()),
			},
		},
		health::Sample {
			id: "hrv-rmssd-1".into(),
			start_ms: t0,
			end_ms: t0,
			source: health::Source::default(),
			value: health::Value::HeartRateVariabilityRmssd { milliseconds: 48.0 },
		},
		health::Sample {
			id: "hrv-sdnn-1".into(),
			start_ms: t0,
			end_ms: t0,
			source: health::Source::default(),
			value: health::Value::HeartRateVariabilitySdnn { milliseconds: 72.0 },
		},
	];
	let h_report = spool.health_samples(device, &health_samples).unwrap();
	assert_eq!(h_report.stored, 5);
	assert_eq!(h_report.skipped, 0);

	// 3. BLE chest strap heart rate (0x2A37) with RR intervals, cycling power
	//    (0x2A63), BLE battery (0x2A19 -> sensor.ble.battery), and two RSC
	//    notifications (0x2A53 -> batched into one SeriesBatch).
	let ble_batch = ble::NotificationBatch {
		dropped: 0,
		notifications: vec![
			ble::GattNotification {
				device_id: "polar-h10".into(),
				service_uuid: gatt::SERVICE_HEART_RATE.into(),
				characteristic_uuid: gatt::CHAR_HEART_RATE_MEASUREMENT.into(),
				time_ms: t1,
				// 8-bit BPM = 142, RR present (flags = 0x10): 800 ticks, 820 ticks.
				value: vec![0x10, 142, 0x20, 0x03, 0x34, 0x03],
			},
			ble::GattNotification {
				device_id: "power-meter".into(),
				service_uuid: gatt::SERVICE_CYCLING_POWER.into(),
				characteristic_uuid: gatt::CHAR_CYCLING_POWER_MEASUREMENT.into(),
				time_ms: t1,
				// Flags = 0x0000, instant power = 245 W.
				value: vec![0x00, 0x00, 245, 0x00],
			},
			ble::GattNotification {
				device_id: "polar-h10".into(),
				service_uuid: gatt::SERVICE_BATTERY.into(),
				characteristic_uuid: gatt::CHAR_BATTERY_LEVEL.into(),
				time_ms: t1,
				value: vec![85],
			},
			ble::GattNotification {
				device_id: "footpod".into(),
				service_uuid: gatt::SERVICE_RUNNING_SPEED_CADENCE.into(),
				characteristic_uuid: gatt::CHAR_RSC_MEASUREMENT.into(),
				time_ms: t1,
				// Flags = 0x00, speed = 3.0 m/s (0x0300), cadence = 176 spm.
				value: vec![0x00, 0x00, 0x03, 176],
			},
			ble::GattNotification {
				device_id: "footpod".into(),
				service_uuid: gatt::SERVICE_RUNNING_SPEED_CADENCE.into(),
				characteristic_uuid: gatt::CHAR_RSC_MEASUREMENT.into(),
				time_ms: t1 + 1_000,
				// Flags = 0x00, speed = 3.25 m/s (0x0340), cadence = 178 spm.
				value: vec![0x00, 0x40, 0x03, 178],
			},
		],
	};
	let b_report = spool.ble_notifications(device, &ble_batch).unwrap();
	assert_eq!(b_report.latest_heart_rate_bpm, Some(142));
	assert_eq!(b_report.stored, 6); // HR + HRV + cycling power + BLE battery + 2 RSC rows

	// 4. Pedometer reading (delta steps + zero-step guard).
	assert_eq!(spool.pedometer_delta_steps(device, now, 0).unwrap(), 0);
	let ped = pedometer::Reading::BootCounter {
		timestamp: 120.0,
		reported_steps: 1024,
	};
	assert_eq!(spool.pedometer_reading(device, now, &ped).unwrap(), 1);

	// 5. Motion accelerometer & barometer batches.
	let accel_payload = leptoskit_sensor::Batch {
		cursor: 64,
		dropped: 0,
		samples: (0..64)
			.map(|i| leptoskit_sensor::VectorReading {
				timestamp: 10.0 + f64::from(i) * 0.02,
				x: 0.1,
				y: 9.8,
				z: 0.2,
			})
			.collect(),
	}
	.encode()
	.unwrap();
	assert_eq!(
		spool
			.motion_batch(
				device,
				now,
				leptoskit_sensor::Kind::Accelerometer.code(),
				&accel_payload,
			)
			.unwrap(),
		64
	);

	// Drain to live daemon.
	let delivered = Uploader::new(&spool, &watch_client).drain().await.unwrap();
	assert!(delivered.records >= 5);
	assert!(delivered.batches >= 10);
	assert_eq!(spool.pending().unwrap().records, 0);
	assert_eq!(spool.pending().unwrap().batches, 0);

	// Verify stored records in vault (no duplicates from replay).
	let from = now - SignedDuration::from_hours(1);
	let to = now + SignedDuration::from_mins(5);
	let range = TimeRange::new(from, to).unwrap();
	let wear_records = daemon
		.state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream("device.wear_state".parse().unwrap()),
			range,
		))
		.unwrap();
	assert_eq!(wear_records.len(), 1);

	let ecg_records = daemon
		.state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream("health.ecg".parse().unwrap()),
			range,
		))
		.unwrap();
	assert_eq!(ecg_records.len(), 1);
	assert_eq!(ecg_records[0].data["classification"], "sinus_rhythm");

	let bia_records = daemon
		.state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream("health.body_composition".parse().unwrap()),
			range,
		))
		.unwrap();
	assert_eq!(bia_records.len(), 1);

	// Verify RMSSD and SDNN landed in distinct streams.
	let sdnn_batches = daemon
		.state
		.vault
		.store
		.series(&"health.hrv_sdnn".parse().unwrap(), range, 10_000)
		.unwrap();
	assert_eq!(sdnn_batches.len(), 1);
	assert_eq!(sdnn_batches[0].channels, vec!["sdnn"]);
	assert_eq!(sdnn_batches[0].values, vec![72.0]);

	// Verify BLE battery landed on `sensor.ble.battery` (not `device.battery`),
	// and the two RSC readings coalesced into a single SeriesBatch.
	let ble_batt_batches = daemon
		.state
		.vault
		.store
		.series(&"sensor.ble.battery".parse().unwrap(), range, 10_000)
		.unwrap();
	assert_eq!(ble_batt_batches.len(), 1);
	assert_eq!(ble_batt_batches[0].values, vec![85.0]);
	let phone_batt_batches = daemon
		.state
		.vault
		.store
		.series(&"device.battery".parse().unwrap(), range, 10_000)
		.unwrap();
	assert!(phone_batt_batches.is_empty());

	let rsc_batches = daemon
		.state
		.vault
		.store
		.series(
			&"sensor.ble.running_cadence".parse().unwrap(),
			range,
			10_000,
		)
		.unwrap();
	assert_eq!(rsc_batches.len(), 1);
	assert_eq!(rsc_batches[0].offsets_ms, vec![0, 1_000]);
	assert_eq!(rsc_batches[0].channels, vec!["speed_mps", "cadence_spm"]);

	// Agent with --deny health gets forbidden on health.heart_rate and
	// health.hrv_sdnn, while health_agent sees them!
	for stream_name in ["health.heart_rate", "health.hrv_sdnn"] {
		let req = SeriesRequest {
			stream: stream_name.into(),
			from: Some(from),
			to: Some(to),
			bucket_seconds: None,
			raw: true,
			max_rows: None,
		};
		assert_eq!(
			denied_agent
				.series(&req, Some(&purpose()))
				.await
				.unwrap_err()
				.code(),
			Some("forbidden"),
			"{stream_name} should be denied"
		);
		let allowed = health_agent.series(&req, Some(&purpose())).await.unwrap();
		assert_eq!(allowed.stream.as_str(), stream_name);
	}

	// Non-health device.wear_state is still readable by denied_agent while
	// health.sleep and health.ecg are withheld.
	let wear_resp = denied_agent
		.records(
			&RecordsRequest {
				streams: vec![
					"health.sleep".into(),
					"health.ecg".into(),
					"device.wear_state".into(),
				],
				..Default::default()
			},
			Some(&purpose()),
		)
		.await
		.unwrap();
	assert!(wear_resp.withheld.contains(&"health.sleep".to_owned()));
	assert!(wear_resp.withheld.contains(&"health.ecg".to_owned()));
	assert_eq!(wear_resp.records.len(), 1);
}
