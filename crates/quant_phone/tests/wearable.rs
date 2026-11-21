//! Wearable, BLE, and watch-companion sync flows through the `quant_phone`
//! worker against a live pinned-TLS `quantd` daemon.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_ble::gatt;
use leptoskit_plugin_wearable as wearable;
use quant_api::ApproveRequest;
use quant_client::Endpoint;
use quant_client::OwnerSession;
use quant_collector::CompanionEnvelope;
use quant_core::DeviceId;
use quant_core::TimeRange;
use quant_crypto::KdfParams;
use quant_model::phone::Command;
use quant_model::phone::Link;
use quant_model::phone::PHONE_SCOPES;
use quant_model::phone::Reading;
use quant_model::phone::Reply;
use quant_model::phone::ReplyData;
use quant_model::phone::Snapshot;
use quant_model::phone::Source;
use quant_model::phone::Storage;
use quant_model::phone::WATCH_SCOPES;
use quant_phone::Config;
use quant_phone::Job;
use quant_phone::Request;
use quant_phone::Shared;
use quant_store::Vault;
use quant_store::VaultPaths;
use tokio::sync::mpsc::UnboundedSender;

const PASS: &str = "correct horse battery staple";

struct Daemon {
	state: Arc<quant_daemon::State>,
	owner: Endpoint,
	link: String,
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
		tokio::time::sleep(Duration::from_millis(5)).await;
	}

	let identity = quant_daemon::tls::TlsIdentity::load_or_create(&state.vault.store).unwrap();
	let fingerprint = identity.fingerprint();
	let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let address = probe.local_addr().unwrap();
	drop(probe);
	let serving = Arc::clone(&state);
	tokio::spawn(async move {
		quant_daemon::serve_tls(serving, address, &identity, std::future::pending())
			.await
			.unwrap();
	});
	while tokio::net::TcpStream::connect(address).await.is_err() {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}

	Daemon {
		state,
		owner: Endpoint::Unix(socket),
		link: format!("quant://link?addr={address}&fp={fingerprint}&vault=test"),
		_dir: dir,
	}
}

impl Daemon {
	async fn approve_with(&self, code: &str, scopes: &[&str]) {
		OwnerSession::open(self.owner.clone(), PASS)
			.await
			.unwrap()
			.approve(&ApproveRequest {
				client: code.to_owned(),
				scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
				label: None,
				lookback_hours: None,
				expires_in_hours: None,
				limits: None,
				unredacted: false,
				deny_classes: None,
			})
			.await
			.unwrap();
	}

	async fn revoke(&self, code: &str) {
		OwnerSession::open(self.owner.clone(), PASS)
			.await
			.unwrap()
			.revoke(code)
			.await
			.unwrap();
	}
}

struct WorkerHarness {
	jobs: Option<UnboundedSender<Request>>,
	thread: Option<std::thread::JoinHandle<()>>,
	shared: Arc<Shared>,
	next: u64,
	_dir: tempfile::TempDir,
}

impl WorkerHarness {
	fn start(name: &str, is_watch: bool) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let shared = Arc::new(Shared::default());
		let config = Config {
			data: dir.path().join("data"),
			name: name.into(),
			tz: TimeZone::UTC,
			is_watch,
		};
		let (jobs, thread) = quant_phone::spawn(config, Arc::clone(&shared)).unwrap();
		Self {
			jobs: Some(jobs),
			thread: Some(thread),
			shared,
			next: 1,
			_dir: dir,
		}
	}

	async fn open_storage(&mut self) {
		let key_reply = self.send(Job::Command(Command::NewStorageKey)).await;
		let Some(ReplyData::StorageKey { key }) = key_reply.data else {
			panic!("expected storage key");
		};
		assert!(
			self.send(Job::Command(Command::OpenStorage { key, fresh: true }))
				.await
				.ok
		);
		assert_eq!(self.snapshot().storage, Storage::Open);
	}

	async fn send(&mut self, job: Job) -> Reply {
		let ticket = self.next;
		self.next += 1;
		self.jobs
			.as_ref()
			.unwrap()
			.send(Request {
				job,
				ticket: Some(ticket),
			})
			.unwrap();
		loop {
			for (id, reply) in self.shared.take_replies() {
				if id == ticket {
					return reply;
				}
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
	}

	fn snapshot(&self) -> Snapshot {
		self.shared.snapshot_since(0).unwrap()
	}
}

impl Drop for WorkerHarness {
	fn drop(&mut self) {
		drop(self.jobs.take());
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

fn bounded_boot_counter(timestamp: f64, reported_steps: u64) -> Vec<u8> {
	let mut out = Vec::with_capacity(36);
	out.extend_from_slice(&0u32.to_le_bytes()); // bounded_reply OK
	out.extend_from_slice(&1u32.to_le_bytes()); // scope = BootCounter
	out.extend_from_slice(&[0u8; 4]); // reserved
	out.extend_from_slice(&reported_steps.to_le_bytes());
	out.extend_from_slice(&timestamp.to_le_bytes());
	out.extend_from_slice(&0.0_f64.to_le_bytes());
	out
}

#[tokio::test]
async fn watch_and_companion_sync_pair_collect_and_deliver() {
	let daemon = daemon().await;
	let mut worker = WorkerHarness::start("Pixel 9", false);
	worker.open_storage().await;
	let phone_device: DeviceId = worker
		.snapshot()
		.device_id
		.as_deref()
		.expect("device_id populated")
		.parse()
		.unwrap();

	// Pair via companion PairLink envelope.
	let pair_env = serde_json::to_vec(&CompanionEnvelope::PairLink {
		link: daemon.link.clone(),
	})
	.unwrap();
	let pair_frame = Reading::WatchSync(&pair_env).encode().unwrap();
	assert!(worker.send(Job::Reading(pair_frame.clone())).await.ok);
	let snap = worker.snapshot();
	assert!(snap.watch_connected);
	let Link::Pending { code } = snap.link else {
		panic!("expected Link::Pending after PairLink, got {:?}", snap.link);
	};

	// ExportPairLink returns the saved link.
	let export = worker.send(Job::Command(Command::ExportPairLink)).await;
	assert_eq!(
		export.data,
		Some(ReplyData::PairLink {
			link: daemon.link.clone()
		})
	);

	// Approve on daemon, then tick to become Ready.
	daemon.approve_with(&code, &PHONE_SCOPES).await;
	assert!(worker.send(Job::Command(Command::Upload)).await.ok);
	assert_eq!(worker.snapshot().link, Link::Ready);

	// HealthPlan, AdvanceHealthUntil & SetHealthToken round-trip.
	let until_ms = Timestamp::now().as_millisecond() - 10_000;
	assert!(
		worker
			.send(Job::Command(Command::AdvanceHealthUntil { until_ms }))
			.await
			.ok
	);
	assert!(
		worker
			.send(Job::Command(Command::SetHealthToken {
				token: Some("hc-token-1".into()),
			}))
			.await
			.ok
	);
	let plan = worker.send(Job::Command(Command::HealthPlan)).await;
	match plan.data {
		Some(ReplyData::HealthWindow {
			since_ms, token, ..
		}) => {
			assert_eq!(since_ms, until_ms);
			assert_eq!(token.as_deref(), Some("hc-token-1"));
		}
		other => panic!("expected HealthWindow, got {other:?}"),
	}

	// Power reading surfaces battery_percent on Snapshot.
	let batt = battery::Snapshot {
		present: Some(true),
		charge: Some(battery::Charge::new(840).unwrap()),
		state: battery::State::Discharging,
		power_save: Some(false),
	}
	.encode();
	let power_frame = Reading::Power {
		battery: Some(&batt),
		thermal: None,
	}
	.encode()
	.unwrap();
	assert!(worker.send(Job::Reading(power_frame)).await.ok);
	assert_eq!(worker.snapshot().battery_percent, Some(84));

	// Queue a direct wearable batch, a BLE strap batch, and an isolated
	// ferried watch reading under a distinct watch DeviceId (base64 encoded).
	let now_ms = Timestamp::now().as_millisecond();
	let w_batch = wearable::Batch {
		dropped: 0,
		readings: vec![
			wearable::Reading::HeartRate {
				time_ms: now_ms - 5_000,
				bpm: 68.0,
				accuracy: wearable::Accuracy::High,
			},
			wearable::Reading::Steps {
				start_ms: now_ms - 60_000,
				end_ms: now_ms,
				count: 350,
			},
		],
	}
	.encode()
	.unwrap();
	let w_frame = Reading::Wearable(&w_batch).encode().unwrap();
	assert!(worker.send(Job::Reading(w_frame.clone())).await.ok);

	let ble_batch = ble::NotificationBatch {
		dropped: 0,
		notifications: vec![ble::GattNotification {
			device_id: "Polar-H10".into(),
			service_uuid: gatt::SERVICE_HEART_RATE.into(),
			characteristic_uuid: gatt::CHAR_HEART_RATE_MEASUREMENT.into(),
			time_ms: now_ms - 2_000,
			value: vec![0x00, 128],
		}],
	}
	.encode()
	.unwrap();
	let ble_frame = Reading::Ble(&ble_batch).encode().unwrap();
	assert!(worker.send(Job::Reading(ble_frame)).await.ok);

	// Pause local phone collection — companion watch ferrying must still
	// succeed without mutating phone `steps_today` or `Source::Wearable`.
	assert!(
		worker
			.send(Job::Command(Command::Collect { enabled: false }))
			.await
			.ok
	);
	let watch_device = DeviceId::new();
	let ferry = serde_json::to_vec(&CompanionEnvelope::reading_frame(
		Some(watch_device),
		"Pixel Watch 3",
		&w_frame,
	))
	.unwrap();
	let ferry_frame = Reading::WatchSync(&ferry).encode().unwrap();
	assert!(worker.send(Job::Reading(ferry_frame)).await.ok);

	let snap = worker.snapshot();
	assert_eq!(snap.latest_heart_rate_bpm, Some(68));
	// Only the local wearable reading counts toward phone `steps_today`.
	assert_eq!(snap.steps_today, Some(350));
	assert_eq!(snap.watch_name.as_deref(), Some("Pixel Watch 3"));
	assert_eq!(snap.ble_devices, vec!["Polar-H10".to_owned()]);
	assert!(
		snap.sources
			.iter()
			.any(|r| r.source == Source::Wearable && r.readings == 2)
	);
	assert!(
		snap.sources
			.iter()
			.any(|r| r.source == Source::WatchSync && r.readings == 2)
	);
	assert!(
		snap.sources
			.iter()
			.any(|r| r.source == Source::Ble && r.readings == 1)
	);

	// WatchPeerStatus updates live watch connectivity and name.
	assert!(
		worker
			.send(Job::Command(Command::WatchPeerStatus {
				connected: false,
				name: Some("Pixel Watch 3".into()),
			}))
			.await
			.ok
	);
	assert!(!worker.snapshot().watch_connected);

	// Deliver to the vault and confirm zero waiting + series stored under BOTH
	// the phone's DeviceId and the watch's DeviceId.
	assert!(worker.send(Job::Command(Command::Upload)).await.ok);
	assert_eq!(worker.snapshot().waiting, 0);

	let range = TimeRange::new(
		Timestamp::now() - SignedDuration::from_hours(1),
		Timestamp::now() + SignedDuration::from_mins(5),
	)
	.unwrap();
	let hr_batches = daemon
		.state
		.vault
		.store
		.series(&"health.heart_rate".parse().unwrap(), range, 10_000)
		.unwrap();
	let devices: BTreeSet<DeviceId> = hr_batches.iter().map(|b| b.device).collect();
	assert!(devices.contains(&phone_device));
	assert!(devices.contains(&watch_device));

	// Revoking the grant and resending PairLink re-pairs cleanly.
	daemon.revoke(&code).await;
	assert!(worker.send(Job::Command(Command::Upload)).await.ok);
	assert_eq!(worker.snapshot().link, Link::Revoked);
	assert!(worker.send(Job::Reading(pair_frame)).await.ok);
	assert!(matches!(worker.snapshot().link, Link::Pending { .. }));
}

#[tokio::test]
async fn watch_worker_requests_watch_scopes_and_tracks_bounded_pedometer_deltas() {
	let daemon = daemon().await;
	let mut watch = WorkerHarness::start("Galaxy Watch 7", true);
	watch.open_storage().await;

	// Pair as a watch and verify the daemon sees WATCH_SCOPES (no apps:write).
	assert!(
		watch
			.send(Job::Command(Command::Pair {
				link: daemon.link.clone(),
			}))
			.await
			.ok
	);
	let Link::Pending { code } = watch.snapshot().link else {
		panic!("expected pending link");
	};
	assert!(
		watch
			.snapshot()
			.approve
			.as_deref()
			.is_some_and(|cmd| !cmd.contains("apps:write") && cmd.contains("health:write"))
	);
	let clients = OwnerSession::open(daemon.owner.clone(), PASS)
		.await
		.unwrap()
		.clients()
		.await
		.unwrap();
	let pending = clients.iter().find(|c| c.pairing_code == code).unwrap();
	let expected_scopes: Vec<String> = WATCH_SCOPES.iter().map(|s| (*s).to_owned()).collect();
	assert_eq!(pending.requested, expected_scopes);

	// 1. Timeout bounded_reply ([1, 0, 0, 0]) is silently ignored (0 steps).
	let timeout_frame = Reading::Pedometer(&[1, 0, 0, 0]).encode().unwrap();
	assert!(watch.send(Job::Reading(timeout_frame)).await.ok);
	assert_eq!(watch.snapshot().steps_today, None);
	assert!(
		watch
			.snapshot()
			.sources
			.iter()
			.find(|r| r.source == Source::Motion)
			.is_none_or(|r| r.problem.is_none())
	);

	// 2. First BootCounter reading sets baseline (1000 steps -> 0 delta).
	let b1 = bounded_boot_counter(10.0, 1000);
	assert!(
		watch
			.send(Job::Reading(Reading::Pedometer(&b1).encode().unwrap()))
			.await
			.ok
	);
	assert_eq!(watch.snapshot().steps_today, None);

	// 3. Second BootCounter reading (1250 steps since boot -> +250 delta).
	let b2 = bounded_boot_counter(70.0, 1250);
	assert!(
		watch
			.send(Job::Reading(Reading::Pedometer(&b2).encode().unwrap()))
			.await
			.ok
	);
	assert_eq!(watch.snapshot().steps_today, Some(250));

	// 4. Reboot rollback (reported_steps = 80 < 1250 -> +80 delta).
	let b3 = bounded_boot_counter(15.0, 80);
	assert!(
		watch
			.send(Job::Reading(Reading::Pedometer(&b3).encode().unwrap()))
			.await
			.ok
	);
	assert_eq!(watch.snapshot().steps_today, Some(330));
}
