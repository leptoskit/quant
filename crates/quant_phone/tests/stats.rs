//! The opt-in stats companion against a live `quantd`: a separate identity
//! and grant, bounded signed summaries, restricted and revoked grants, and a
//! write-only collector that keeps recording throughout.

use std::sync::Arc;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_location::Fix;
use leptoskit_plugin_location::Provider;
use quant_api::ApproveRequest;
use quant_api::ClientStatus;
use quant_client::Endpoint;
use quant_client::OwnerSession;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_crypto::KdfParams;
use quant_model::phone::Command;
use quant_model::phone::Link;
use quant_model::phone::PHONE_SCOPES;
use quant_model::phone::Reading;
use quant_model::phone::Reply;
use quant_model::phone::ReplyData;
use quant_model::phone::Snapshot;
use quant_model::phone::StatsAccess;
use quant_model::phone::StatsDetail;
use quant_model::phone::StatsKind;
use quant_model::phone::StatsReply;
use quant_model::phone::StatsWindow;
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
	async fn owner(&self) -> OwnerSession {
		OwnerSession::open(self.owner.clone(), PASS).await.unwrap()
	}

	async fn approve_with(&self, code: &str, scopes: &[&str]) {
		self.owner()
			.await
			.approve(&ApproveRequest {
				client: code.to_owned(),
				scopes: Some(scopes.iter().map(|scope| (*scope).to_owned()).collect()),
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
		self.owner().await.revoke(code).await.unwrap();
	}
}

struct Phone {
	jobs: Option<UnboundedSender<Request>>,
	shared: Arc<Shared>,
	thread: Option<std::thread::JoinHandle<()>>,
	next: u64,
	dir: tempfile::TempDir,
}

impl Phone {
	fn start() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let shared = Arc::new(Shared::default());
		let config = Config {
			data: dir.path().join("data"),
			name: "Seeker".into(),
			tz: TimeZone::UTC,
			is_watch: false,
		};
		let (jobs, thread) = quant_phone::spawn(config, Arc::clone(&shared)).unwrap();
		Self {
			jobs: Some(jobs),
			shared,
			thread: Some(thread),
			next: 1,
			dir,
		}
	}

	fn data(&self) -> std::path::PathBuf {
		self.dir.path().join("data")
	}

	fn companion_identity(&self) -> std::path::PathBuf {
		self.data().join("phone.companion.identity.json")
	}

	fn collector_identity(&self) -> std::path::PathBuf {
		self.data().join("phone.identity.json")
	}

	async fn open_storage(&mut self) {
		let reply = self.command(Command::NewStorageKey).await;
		let Some(ReplyData::StorageKey { key }) = reply.data else {
			panic!("expected a storage key, got {reply:?}");
		};
		assert!(
			self.command(Command::OpenStorage { key, fresh: true })
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
		for _ in 0..4000 {
			for (id, reply) in self.shared.take_replies() {
				if id == ticket {
					return reply;
				}
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("the worker never answered");
	}

	async fn command(&mut self, command: Command) -> Reply {
		self.send(Job::Command(command)).await
	}

	async fn reading(&mut self, reading: Reading<'_>) {
		let reply = self.send(Job::Reading(reading.encode().unwrap())).await;
		assert!(reply.ok, "{reply:?}");
	}

	fn snapshot(&self) -> Snapshot {
		self.shared.snapshot_since(u64::MAX).unwrap()
	}

	async fn settled(&self) -> Snapshot {
		for _ in 0..4000 {
			let snapshot = self.snapshot();
			if snapshot.version > 0 && snapshot.link != Link::Connecting {
				return snapshot;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("the worker never settled");
	}

	fn stop(&mut self) {
		self.jobs = None;
		if let Some(thread) = self.thread.take() {
			thread.join().unwrap();
		}
	}
}

impl Drop for Phone {
	fn drop(&mut self) {
		self.stop();
	}
}

fn stats_reply(reply: &Reply) -> StatsReply {
	match &reply.data {
		Some(ReplyData::VaultStats { stats }) => stats.clone(),
		other => panic!("expected a stats reply, got {other:?}"),
	}
}

fn pending_code(access: &StatsAccess) -> &str {
	match access {
		StatsAccess::Pending { code } => code,
		other => panic!("expected a pending stats companion, got {other:?}"),
	}
}

fn ms(at: Timestamp) -> i64 {
	at.as_millisecond()
}

fn close(value: Option<f64>, expected: f64) -> bool {
	value.is_some_and(|value| (value - expected).abs() < 1e-9)
}

#[allow(clippy::cast_precision_loss)]
fn fix(at: Timestamp, latitude: f64) -> Fix {
	Fix {
		timestamp_ms: ms(at) as f64,
		latitude,
		longitude: -0.1276,
		accuracy_m: 12.0,
		altitude_m: Some(30.0),
		altitude_accuracy_m: Some(4.0),
		heading_degrees: None,
		speed_mps: Some(1.2),
		provider: Provider::Fused,
		simulated: Some(false),
	}
}

fn updates(fixes: &[Fix]) -> Vec<u8> {
	let mut bytes = 0u32.to_le_bytes().to_vec();
	bytes.extend(u32::try_from(fixes.len()).unwrap().to_le_bytes());
	for fix in fixes {
		bytes.extend(fix.encode().unwrap());
	}
	bytes
}

fn series(stream: &str, device: DeviceId, start: Timestamp, samples: &[(u32, f64)]) -> SeriesBatch {
	SeriesBatch {
		stream: stream.parse().unwrap(),
		device,
		start,
		channels: vec![
			match stream {
				"health.steps" => "steps",
				"health.heart_rate" => "bpm",
				other => panic!("no sample channel for {other}"),
			}
			.into(),
		],
		offsets_ms: samples.iter().map(|(offset, _)| *offset).collect(),
		values: samples.iter().map(|(_, value)| *value).collect(),
	}
}

fn record(
	stream: &str,
	device: DeviceId,
	start: Timestamp,
	end: Option<Timestamp>,
	attributes: &[(&str, &str)],
) -> Record {
	let mut record = Record::new(stream.parse().unwrap(), device).starting(start);
	if let Some(end) = end {
		record = record.ending(end);
	}
	for (key, value) in attributes {
		record = record.with(*key, *value);
	}
	record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_stats_companion_reads_cross_device_streams_without_touching_the_collector() {
	let daemon = daemon().await;
	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;
	let phone_device: DeviceId = phone
		.snapshot()
		.device_id
		.as_deref()
		.expect("device id")
		.parse()
		.unwrap();

	// The existing write-only phone pairs exactly as before.
	assert!(
		phone
			.command(Command::Pair {
				link: daemon.link.clone(),
			})
			.await
			.ok
	);
	let Link::Pending { code } = phone.snapshot().link else {
		panic!("expected the collector's pairing code");
	};
	daemon.approve_with(&code, &PHONE_SCOPES).await;
	assert!(phone.command(Command::Upload).await.ok);
	assert_eq!(phone.snapshot().link, Link::Ready);

	let now = Timestamp::now();
	let location = updates(&[fix(now - SignedDuration::from_mins(30), 51.5010)]);
	phone.reading(Reading::Location(&location)).await;
	let waiting = phone.snapshot().waiting;
	assert!(waiting > 0, "the collector has something to deliver");

	// Synthetic streams from two devices, as if a watch and the phone had
	// already delivered them. The stats companion sees the vault, never the
	// spool.
	let watch = DeviceId::new();
	let store = &daemon.state.vault.store;
	store
		.insert_series(&series(
			"health.steps",
			watch,
			now - SignedDuration::from_hours(1),
			&[(0, 100.0), (600_000, 200.0), (1_200_000, 300.0)],
		))
		.unwrap();
	store
		.insert_series(&series(
			"health.heart_rate",
			phone_device,
			now - SignedDuration::from_mins(30),
			&[(0, 60.0), (60_000, 80.0)],
		))
		.unwrap();
	store
		.insert_records(&[
			record(
				"apps.focus",
				phone_device,
				now - SignedDuration::from_mins(40),
				Some(now - SignedDuration::from_mins(30)),
				&[("app", "Chrome")],
			),
			record(
				"apps.focus",
				phone_device,
				now - SignedDuration::from_mins(30),
				Some(now - SignedDuration::from_mins(25)),
				&[("app", "Notes")],
			),
			record(
				"memory.note",
				watch,
				now - SignedDuration::from_hours(2),
				None,
				&[],
			),
		])
		.unwrap();

	// An explicit request makes a second, separate key and asks for
	// aggregates only.
	assert!(phone.command(Command::RequestStats).await.ok);
	let requested = phone.snapshot();
	let code = pending_code(&requested.stats.access).to_owned();
	let approve = requested
		.stats
		.approve
		.as_deref()
		.expect("approval command");
	assert_eq!(approve, format!("quant approve {code} --scope '*:summary'"));
	assert_eq!(requested.link, Link::Ready);
	assert_eq!(requested.waiting, waiting, "the spool is untouched");
	assert!(phone.collector_identity().exists());
	assert!(phone.companion_identity().exists());

	let clients = daemon.owner().await.clients().await.unwrap();
	let pending = clients
		.iter()
		.find(|client| client.pairing_code == code)
		.expect("the stats companion is waiting");
	assert_eq!(pending.requested, vec!["*:summary".to_owned()]);
	assert_eq!(pending.status, ClientStatus::Pending);

	daemon.approve_with(&code, &["*:summary"]).await;
	let reply = phone
		.command(Command::VaultStats {
			window: StatsWindow::Week,
		})
		.await;
	assert!(reply.ok, "{reply:?}");
	let stats = stats_reply(&reply);
	assert_eq!(stats.access, StatsAccess::Ready);
	assert_eq!(stats.problem, None);
	let summary = stats.summary.expect("the vault answered");
	assert_eq!(summary.window, StatsWindow::Week);
	assert_eq!(summary.span, "Past 7 days");
	assert!(summary.from_ms <= now.as_millisecond() - StatsWindow::Week.span_ms() + 60_000);
	assert!(summary.to_ms >= now.as_millisecond() - 60_000);
	assert!(summary.to_ms - summary.from_ms >= StatsWindow::Week.span_ms() - 60_000);
	assert!(!summary.streams.is_empty());
	assert!(summary.streams.len() <= quant_model::phone::MAX_STATS_STREAMS);

	let steps = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "health.steps")
		.expect("steps");
	assert_eq!(steps.kind, StatsKind::Series);
	assert_eq!(steps.count, 3);
	assert_eq!(steps.count_unit, "samples");
	let StatsDetail::Numeric {
		channels, buckets, ..
	} = &steps.detail
	else {
		panic!("expected numeric steps, got {:?}", steps.detail);
	};
	assert!(!buckets.is_empty());
	assert_eq!(buckets[0].channels[0].name, "steps");
	assert_eq!(buckets[0].channels[0].unit, "count");
	assert_eq!(buckets[0].channels[0].samples, 3);
	let steps_channel = channels
		.iter()
		.find(|channel| channel.name == "steps")
		.expect("steps channel");
	assert_eq!(steps_channel.unit, "count");
	assert_eq!(steps_channel.samples, 3);
	assert!(close(steps_channel.min, 100.0));
	assert!(close(steps_channel.max, 300.0));
	assert!(close(steps_channel.mean, 200.0));
	// The sample count is not the step total: three samples averaged 200.
	assert_eq!(steps.count, steps_channel.samples);
	assert_eq!(steps.count_unit, "samples");

	let heart = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "health.heart_rate")
		.expect("heart rate");
	let StatsDetail::Numeric { channels, .. } = &heart.detail else {
		panic!("expected numeric heart rate");
	};
	assert_eq!(channels[0].unit, "bpm");
	assert!(close(channels[0].mean, 70.0));

	let focus = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "apps.focus")
		.expect("focus");
	assert_eq!(focus.kind, StatsKind::Event);
	assert_eq!(focus.count, 2);
	assert_eq!(focus.count_unit, "records");
	let StatsDetail::Durations {
		group_by, groups, ..
	} = &focus.detail
	else {
		panic!("expected focus durations, got {:?}", focus.detail);
	};
	assert_eq!(group_by, "app");
	let chrome = groups
		.iter()
		.find(|group| group.group.as_deref() == Some("Chrome"))
		.expect("Chrome");
	assert!((chrome.seconds - 600.0).abs() < 1.0);
	assert_eq!(chrome.duration, "10m");
	let notes = groups
		.iter()
		.find(|group| group.group.as_deref() == Some("Notes"))
		.expect("Notes");
	assert!((notes.seconds - 300.0).abs() < 1.0);

	let note = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "memory.note")
		.expect("note");
	assert_eq!(note.count, 1);
	assert_eq!(note.count_unit, "records");
	assert_eq!(note.detail, StatsDetail::Counted);

	// A readable stream with no data is listed, with its units and no
	// invented values.
	let hrv = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "health.hrv")
		.expect("empty HRV stream");
	assert_eq!(hrv.count, 0);
	assert_eq!(hrv.count_unit, "samples");
	let StatsDetail::Numeric {
		channels, buckets, ..
	} = &hrv.detail
	else {
		panic!("expected empty numeric HRV");
	};
	assert!(buckets.is_empty());
	assert_eq!(channels[0].unit, "ms");
	assert_eq!(channels[0].samples, 0);
	assert_eq!(channels[0].mean, None);

	// The collector still holds and delivers everything, and its snapshot
	// never learns about the stats read.
	let after = phone.snapshot();
	assert_eq!(after.link, Link::Ready);
	assert!(after.collecting);
	assert_eq!(after.waiting, waiting);
	assert_eq!(after.upload_problem, None);
	assert_eq!(after.stats.access, StatsAccess::Ready);
	assert!(phone.command(Command::Upload).await.ok);
	let delivered = phone.snapshot();
	assert_eq!(delivered.waiting, 0);
	assert!(delivered.delivered.is_some());

	// The two identities are separate keys, and the collector's grant is
	// still write-only.
	let collector_key: serde_json::Value =
		serde_json::from_slice(&std::fs::read(phone.collector_identity()).unwrap()).unwrap();
	let companion_key: serde_json::Value =
		serde_json::from_slice(&std::fs::read(phone.companion_identity()).unwrap()).unwrap();
	assert_ne!(collector_key["key"], companion_key["key"]);
	let clients = daemon.owner().await.clients().await.unwrap();
	let collector = clients
		.iter()
		.find(|client| {
			client
				.requested
				.iter()
				.any(|scope| scope == "location:write")
		})
		.expect("collector client");
	assert_eq!(
		collector.requested,
		PHONE_SCOPES
			.iter()
			.map(|scope| (*scope).to_owned())
			.collect::<Vec<_>>()
	);
	let stats_client = clients
		.iter()
		.find(|client| client.requested == vec!["*:summary".to_owned()])
		.expect("stats client");
	assert_ne!(collector.id, stats_client.id);
	assert_eq!(
		stats_client.grant.as_ref().unwrap().scopes.to_strings(),
		vec!["*:summary".to_owned()]
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_restricted_grant_stays_restricted_and_a_lost_grant_is_never_repaired_silently() {
	let daemon = daemon().await;
	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;
	let phone_device: DeviceId = phone
		.snapshot()
		.device_id
		.as_deref()
		.unwrap()
		.parse()
		.unwrap();
	assert!(
		phone
			.command(Command::Pair {
				link: daemon.link.clone(),
			})
			.await
			.ok
	);
	let Link::Pending { code } = phone.snapshot().link else {
		panic!("expected collector pairing");
	};
	daemon.approve_with(&code, &PHONE_SCOPES).await;
	assert!(phone.command(Command::Upload).await.ok);

	let now = Timestamp::now();
	let store = &daemon.state.vault.store;
	store
		.insert_records(&[
			record(
				"apps.focus",
				phone_device,
				now - SignedDuration::from_mins(20),
				Some(now - SignedDuration::from_mins(10)),
				&[("app", "Chrome")],
			),
			record(
				"apps.focus",
				phone_device,
				now - SignedDuration::from_mins(10),
				Some(now - SignedDuration::from_mins(5)),
				&[("app", "Notes")],
			),
		])
		.unwrap();
	store
		.insert_series(&series(
			"health.steps",
			phone_device,
			now - SignedDuration::from_mins(15),
			&[(0, 250.0)],
		))
		.unwrap();
	let location = updates(&[fix(now - SignedDuration::from_mins(2), 51.5020)]);
	phone.reading(Reading::Location(&location)).await;
	let waiting = phone.snapshot().waiting;
	assert!(waiting > 0);

	assert!(phone.command(Command::RequestStats).await.ok);
	let code = pending_code(&phone.snapshot().stats.access).to_owned();
	// The owner deliberately shares only apps summaries.
	daemon.approve_with(&code, &["apps:summary"]).await;
	let reply = phone
		.command(Command::VaultStats {
			window: StatsWindow::Day,
		})
		.await;
	let stats = stats_reply(&reply);
	assert_eq!(stats.access, StatsAccess::Ready);
	let summary = stats.summary.expect("restricted summary");
	assert!(
		summary
			.streams
			.iter()
			.all(|stream| stream.stream.starts_with("apps."))
	);
	let focus = summary
		.streams
		.iter()
		.find(|stream| stream.stream == "apps.focus")
		.expect("focus");
	assert_eq!(focus.count, 2);
	assert!(matches!(&focus.detail, StatsDetail::Durations { .. }));
	assert!(
		summary
			.streams
			.iter()
			.all(|stream| stream.stream != "health.steps")
	);
	assert_eq!(phone.snapshot().waiting, waiting);

	// Revoking the stats grant leaves the collector's grant and spool alone.
	daemon.revoke(&code).await;
	let pending_before = pending_count(&daemon).await;
	let reply = phone
		.command(Command::VaultStats {
			window: StatsWindow::Day,
		})
		.await;
	let revoked = stats_reply(&reply);
	assert_eq!(revoked.access, StatsAccess::Revoked);
	assert_eq!(revoked.summary, None);
	assert!(revoked.problem.is_some());
	assert!(!phone.companion_identity().exists());
	assert_eq!(phone.snapshot().link, Link::Ready);
	assert_eq!(phone.snapshot().waiting, waiting);
	assert_eq!(pending_count(&daemon).await, pending_before);

	// Asking again after the loss reports the same state; it does not make
	// a new key or a new pairing request.
	let again = stats_reply(
		&phone
			.command(Command::VaultStats {
				window: StatsWindow::Day,
			})
			.await,
	);
	assert_eq!(again.access, StatsAccess::Revoked);
	assert_eq!(again.summary, None);
	assert_eq!(pending_count(&daemon).await, pending_before);

	// The collector still delivers while stats access is gone.
	assert!(phone.command(Command::Upload).await.ok);
	assert_eq!(phone.snapshot().waiting, 0);
	assert_eq!(phone.snapshot().link, Link::Ready);

	// A later explicit request is the only thing that makes a fresh key.
	assert!(phone.command(Command::RequestStats).await.ok);
	let fresh = phone.snapshot();
	let fresh_code = pending_code(&fresh.stats.access).to_owned();
	assert_ne!(fresh_code, code);
	assert!(phone.companion_identity().exists());
	assert_eq!(fresh.link, Link::Ready);

	// Forget stats removes the key but leaves the collector and spool.
	assert!(phone.command(Command::ForgetStats).await.ok);
	assert_eq!(phone.snapshot().stats.access, StatsAccess::NotRequested);
	assert!(!phone.companion_identity().exists());
	assert!(phone.collector_identity().exists());
}

async fn pending_count(daemon: &Daemon) -> usize {
	daemon
		.owner()
		.await
		.clients()
		.await
		.unwrap()
		.iter()
		.filter(|client| client.status == ClientStatus::Pending)
		.count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_vaults_clears_the_companion_but_keeps_what_is_waiting() {
	let first = daemon().await;
	let second = daemon().await;
	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;

	assert!(
		phone
			.command(Command::Pair {
				link: first.link.clone(),
			})
			.await
			.ok
	);
	let Link::Pending { code } = phone.snapshot().link else {
		panic!("expected the first collector pairing");
	};
	first.approve_with(&code, &PHONE_SCOPES).await;
	assert!(phone.command(Command::Upload).await.ok);

	assert!(phone.command(Command::RequestStats).await.ok);
	let stats_code = pending_code(&phone.snapshot().stats.access).to_owned();
	first.approve_with(&stats_code, &["*:summary"]).await;
	assert_eq!(
		stats_reply(
			&phone
				.command(Command::VaultStats {
					window: StatsWindow::Day,
				})
				.await
		)
		.access,
		StatsAccess::Ready
	);
	let now = Timestamp::now();
	phone
		.reading(Reading::Location(&updates(&[fix(
			now - SignedDuration::from_mins(3),
			51.5030,
		)])))
		.await;
	let waiting = phone.snapshot().waiting;
	assert!(waiting > 0);

	// A different vault link clears the companion key and starts a new
	// collector pairing. It does not carry stats access across.
	assert!(
		phone
			.command(Command::Pair {
				link: second.link.clone(),
			})
			.await
			.ok
	);
	let switched = phone.snapshot();
	assert_eq!(switched.stats.access, StatsAccess::NotRequested);
	assert_eq!(switched.stats.approve, None);
	assert!(!phone.companion_identity().exists());
	assert_eq!(switched.waiting, waiting);
	let Link::Pending { code } = switched.link else {
		panic!("expected a fresh collector pairing");
	};
	second.approve_with(&code, &PHONE_SCOPES).await;
	assert!(phone.command(Command::Upload).await.ok);
	assert_eq!(phone.snapshot().waiting, 0);

	// Forgetting the vault clears both identities and keeps the spool.
	phone
		.reading(Reading::Location(&updates(&[fix(
			Timestamp::now() - SignedDuration::from_mins(1),
			51.5040,
		)])))
		.await;
	let waiting = phone.snapshot().waiting;
	assert!(waiting > 0);
	assert!(phone.command(Command::Forget).await.ok);
	assert_eq!(phone.snapshot().link, Link::Unpaired);
	assert_eq!(phone.snapshot().stats.access, StatsAccess::Unpaired);
	assert_eq!(phone.snapshot().waiting, waiting);
	assert!(!phone.collector_identity().exists());
	assert!(!phone.companion_identity().exists());
}

#[tokio::test]
async fn stats_are_unpaired_until_a_vault_and_an_explicit_request_exist() {
	let mut phone = Phone::start();
	phone.settled().await;
	let reply = phone.command(Command::RequestStats).await;
	assert!(!reply.ok);
	assert_eq!(phone.snapshot().stats.access, StatsAccess::Unpaired);
	assert!(!phone.companion_identity().exists());

	let reply = phone
		.command(Command::VaultStats {
			window: StatsWindow::Day,
		})
		.await;
	assert!(reply.ok);
	let stats = stats_reply(&reply);
	assert_eq!(stats.access, StatsAccess::Unpaired);
	assert_eq!(stats.summary, None);
	assert!(stats.problem.is_some());
}
