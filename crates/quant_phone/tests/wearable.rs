//! Wearable, BLE, and watch-companion sync flows through the `quant_phone`
//! worker against a live pinned-TLS `quantd` daemon.

use std::sync::Arc;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_ble as ble;
use leptoskit_plugin_ble::gatt;
use leptoskit_plugin_wearable as wearable;
use quant_api::ApproveRequest;
use quant_client::Endpoint;
use quant_client::OwnerSession;
use quant_collector::CompanionEnvelope;
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
	async fn approve(&self, code: &str) {
		OwnerSession::open(self.owner.clone(), PASS)
			.await
			.unwrap()
			.approve(&ApproveRequest {
				client: code.to_owned(),
				scopes: Some(PHONE_SCOPES.iter().map(|s| (*s).to_owned()).collect()),
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
}

struct WorkerHarness {
	jobs: Option<UnboundedSender<Request>>,
	thread: Option<std::thread::JoinHandle<()>>,
	shared: Arc<Shared>,
	next: u64,
	_dir: tempfile::TempDir,
}

impl WorkerHarness {
	fn start() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let shared = Arc::new(Shared::default());
		let config = Config {
			data: dir.path().join("data"),
			name: "Galaxy Watch 7".into(),
			tz: TimeZone::UTC,
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

#[tokio::test]
async fn watch_and_companion_sync_pair_collect_and_deliver() {
	let daemon = daemon().await;
	let mut worker = WorkerHarness::start();

	// Open encrypted spool.
	let key_reply = worker.send(Job::Command(Command::NewStorageKey)).await;
	let Some(ReplyData::StorageKey { key }) = key_reply.data else {
		panic!("expected storage key");
	};
	assert!(
		worker
			.send(Job::Command(Command::OpenStorage { key, fresh: true }))
			.await
			.ok
	);
	assert_eq!(worker.snapshot().storage, Storage::Open);

	// Pair via companion PairLink envelope (simulating phone pushing link to
	// watch).
	let pair_env = serde_json::to_vec(&CompanionEnvelope::PairLink {
		link: daemon.link.clone(),
	})
	.unwrap();
	let pair_frame = Reading::WatchSync(&pair_env).encode().unwrap();
	assert!(worker.send(Job::Reading(pair_frame)).await.ok);
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
	daemon.approve(&code).await;
	assert!(worker.send(Job::Command(Command::Upload)).await.ok);
	assert_eq!(worker.snapshot().link, Link::Ready);

	// HealthPlan & SetHealthToken round-trip.
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
		Some(ReplyData::HealthWindow { token, .. }) => {
			assert_eq!(token.as_deref(), Some("hc-token-1"));
		}
		other => panic!("expected HealthWindow, got {other:?}"),
	}

	// Queue a direct wearable batch + BLE strap batch + ferried watch reading.
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

	let ferry = serde_json::to_vec(&CompanionEnvelope::ReadingFrame {
		watch_name: "Pixel Watch 3".into(),
		frame: w_frame,
	})
	.unwrap();
	let ferry_frame = Reading::WatchSync(&ferry).encode().unwrap();
	assert!(worker.send(Job::Reading(ferry_frame)).await.ok);

	let snap = worker.snapshot();
	assert_eq!(snap.latest_heart_rate_bpm, Some(68));
	assert_eq!(snap.steps_today, Some(700));
	assert_eq!(snap.watch_name.as_deref(), Some("Pixel Watch 3"));
	assert_eq!(snap.ble_devices, vec!["Polar-H10".to_owned()]);
	assert!(
		snap.sources
			.iter()
			.any(|r| r.source == Source::Wearable && r.readings >= 2)
	);
	assert!(
		snap.sources
			.iter()
			.any(|r| r.source == Source::Ble && r.readings == 1)
	);

	// Deliver to the vault and confirm zero waiting + series stored.
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
	assert!(!hr_batches.is_empty());
}
