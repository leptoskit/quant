//! The phone's worker against a live daemon: pairing from a pasted link over
//! pinned TLS, queueing plugin replies exactly as the plugins encode them,
//! and delivering them under the phone's write-only grant.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_app_usage::EventKind;
use leptoskit_plugin_app_usage::UsageEvent;
use leptoskit_plugin_app_usage::UsageSession;
use leptoskit_plugin_battery as battery;
use leptoskit_plugin_location::Fix;
use leptoskit_plugin_location::Provider;
use leptoskit_plugin_thermal as thermal;
use quant_api::ApproveRequest;
use quant_client::Endpoint;
use quant_client::OwnerSession;
use quant_core::SeriesBatch;
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
use quant_store::RecordQuery;
use quant_store::Selector;
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

/// A daemon on a local socket for its owner and on pinned TLS for phones.
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

/// The worker, driven the way the service drives it.
struct Phone {
	jobs: Option<UnboundedSender<Request>>,
	shared: Arc<Shared>,
	thread: Option<std::thread::JoinHandle<()>>,
	ticket: u64,
}

impl Phone {
	fn start(data: &Path) -> Self {
		let shared = Arc::new(Shared::default());
		let config = Config {
			data: data.to_path_buf(),
			name: "Seeker".into(),
			tz: TimeZone::UTC,
		};
		let (jobs, thread) = quant_phone::spawn(config, Arc::clone(&shared)).unwrap();
		Self {
			jobs: Some(jobs),
			shared,
			thread: Some(thread),
			ticket: 0,
		}
	}

	async fn ask(&mut self, job: Job) -> Reply {
		self.ticket += 1;
		let ticket = self.ticket;
		self.jobs
			.as_ref()
			.unwrap()
			.send(Request {
				job,
				ticket: Some(ticket),
			})
			.unwrap();
		for _ in 0..4000 {
			if let Some((_, reply)) = self
				.shared
				.take_replies()
				.into_iter()
				.find(|(answered, _)| *answered == ticket)
			{
				return reply;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("the worker never answered");
	}

	async fn command(&mut self, command: Command) -> Reply {
		self.ask(Job::Command(command)).await
	}

	async fn reading(&mut self, reading: Reading<'_>) -> Reply {
		self.ask(Job::Reading(reading.encode().unwrap())).await
	}

	fn snapshot(&self) -> Snapshot {
		self.shared.snapshot_since(u64::MAX).unwrap()
	}

	/// The snapshot once the worker has finished its first look at the vault.
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

	fn readings(&self, source: Source) -> u64 {
		self.snapshot()
			.sources
			.iter()
			.find(|row| row.source == source)
			.unwrap()
			.readings
	}

	/// Stop the worker the way closing the service does.
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

fn ms(at: Timestamp) -> i64 {
	at.as_millisecond()
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

/// A `next_updates` reply, as the location plugin's native side writes it.
fn updates(dropped: u32, fixes: &[Fix]) -> Vec<u8> {
	let mut bytes = dropped.to_le_bytes().to_vec();
	bytes.extend(u32::try_from(fixes.len()).unwrap().to_le_bytes());
	for fix in fixes {
		bytes.extend(fix.encode().unwrap());
	}
	bytes
}

/// A counted batch, as the app usage plugin's native side writes one.
fn batch(items: &[Vec<u8>]) -> Vec<u8> {
	let mut bytes = u32::try_from(items.len()).unwrap().to_le_bytes().to_vec();
	for item in items {
		bytes.extend(item);
	}
	bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_phone_pairs_from_a_link_spools_readings_and_delivers_them() {
	let daemon = daemon().await;
	let dir = tempfile::tempdir().unwrap();
	let mut phone = Phone::start(dir.path());
	let first = phone.settled().await;
	assert_eq!(first.link, Link::Unpaired);
	assert_eq!(first.storage, Storage::Locked);
	assert!(first.collecting);

	// Only a pairing link is accepted, and nothing is saved for a bad one.
	for refused in ["192.168.1.4:7717", "unix:/tmp/quant.sock", "quant://link"] {
		let reply = phone
			.command(Command::Pair {
				link: refused.into(),
			})
			.await;
		assert!(!reply.ok, "{refused}");
		assert_eq!(phone.snapshot().link, Link::Unpaired);
	}

	// Pairing asks the vault, which answers with a code the owner approves.
	let reply = phone
		.command(Command::Pair {
			link: format!("  {}\n", daemon.link),
		})
		.await;
	assert!(reply.ok, "{reply:?}");
	let pending = phone.snapshot();
	let Link::Pending { code } = &pending.link else {
		panic!("expected a pairing code, got {:?}", pending.link);
	};
	let approve = pending.approve.as_deref().unwrap();
	assert!(
		approve.starts_with(&format!("quant approve {code}")),
		"{approve}"
	);
	for scope in PHONE_SCOPES {
		assert!(approve.contains(&format!("--scope {scope}")), "{approve}");
	}
	assert!(pending.vault.is_some());
	daemon.approve(code).await;

	// Readings wait for storage: nothing is written before the key arrives.
	let now = Timestamp::from_millisecond(ms(Timestamp::now())).unwrap();
	let ago = |minutes: i64| now - SignedDuration::from_mins(minutes);
	let location = updates(1, &[fix(ago(30), 51.5010), fix(ago(20), 51.5020)]);
	assert!(phone.reading(Reading::Location(&location)).await.ok);
	assert_eq!(phone.readings(Source::Location), 0);
	assert_eq!(
		phone.command(Command::UsagePlan).await.data,
		None,
		"no app usage is read before storage opens"
	);

	// The screen fetches a key for the keystore, then opens storage with it.
	let reply = phone.command(Command::NewStorageKey).await;
	let Some(ReplyData::StorageKey { key }) = reply.data else {
		panic!("expected a storage key, got {reply:?}");
	};
	assert!(
		!phone
			.command(Command::OpenStorage {
				key: "not a key".into(),
				fresh: true,
			})
			.await
			.ok
	);
	let opened = phone
		.command(Command::OpenStorage {
			key: key.clone(),
			fresh: true,
		})
		.await;
	assert!(opened.ok, "{opened:?}");
	assert_eq!(phone.snapshot().storage, Storage::Open);

	// The first app usage read covers the last day.
	let reply = phone.command(Command::UsagePlan).await;
	let Some(ReplyData::UsageRange { since_ms, until_ms }) = reply.data else {
		panic!("expected a usage range, got {reply:?}");
	};
	assert!(until_ms >= ms(now) && until_ms - since_ms == 24 * 60 * 60 * 1000);

	// Location, with one fix the native buffer dropped.
	assert!(phone.reading(Reading::Location(&location)).await.ok);
	assert_eq!(phone.readings(Source::Location), 2);
	let row = phone.snapshot();
	let row = row
		.sources
		.iter()
		.find(|row| row.source == Source::Location)
		.unwrap();
	assert!(row.problem.as_deref().unwrap().contains("dropped 1"));
	assert!(row.last.is_some());

	// App usage: sessions, then the transitions in two pages.
	let sessions = batch(&[
		UsageSession {
			package: "com.android.chrome".into(),
			label: "Chrome".into(),
			start_ms: ms(ago(45)),
			end_ms: ms(ago(35)),
		}
		.encode()
		.unwrap(),
		UsageSession {
			package: "org.example.notes".into(),
			label: "Notes".into(),
			start_ms: ms(ago(35)),
			end_ms: ms(ago(25)),
		}
		.encode()
		.unwrap(),
	]);
	let unlocked = batch(&[UsageEvent {
		kind: EventKind::KeyguardHidden,
		timestamp_ms: ms(ago(46)),
		package: None,
	}
	.encode()
	.unwrap()]);
	let dark = batch(&[UsageEvent {
		kind: EventKind::ScreenNonInteractive,
		timestamp_ms: ms(ago(24)),
		package: None,
	}
	.encode()
	.unwrap()]);
	let usage = Reading::Usage {
		until_ms,
		sessions: &sessions,
		events: vec![&unlocked, &dark],
	};
	assert!(phone.reading(usage).await.ok);
	assert_eq!(phone.readings(Source::Usage), 4);
	// The stretch just read is not read again.
	assert_eq!(phone.command(Command::UsagePlan).await.data, None);

	// Power: the first reading stores a level and a state, a repeat nothing.
	let level = battery::Snapshot {
		present: Some(true),
		charge: Some(battery::Charge::new(812).unwrap()),
		state: battery::State::Charging,
		power_save: Some(false),
	}
	.encode();
	let cool = thermal::Snapshot::from_android(thermal::AndroidStatus::None).encode();
	let power = || {
		Reading::Power {
			battery: Some(&level),
			thermal: Some(&cool),
		}
	};
	assert!(phone.reading(power()).await.ok);
	assert_eq!(phone.readings(Source::Power), 2);
	assert!(phone.reading(power()).await.ok);
	assert_eq!(phone.readings(Source::Power), 2);
	// A reading nobody can decode marks its source and stops nothing.
	assert!(phone.reading(Reading::Location(&[1, 2, 3])).await.ok);
	let snapshot = phone.snapshot();
	assert!(
		snapshot.sources[0]
			.problem
			.as_deref()
			.unwrap()
			.contains("could not be read")
	);
	assert!(snapshot.waiting > 0);
	assert_eq!(snapshot.delivered, None);

	// A tick finds the approval and delivers everything.
	assert!(phone.command(Command::Upload).await.ok);
	let delivered = phone.snapshot();
	assert_eq!(delivered.link, Link::Ready, "{delivered:?}");
	assert_eq!(delivered.waiting, 0, "{delivered:?}");
	assert_eq!(delivered.upload_problem, None);
	assert!(delivered.delivered.is_some());
	assert_eq!(delivered.approve, None);

	let store = &daemon.state.vault.store;
	let range = TimeRange::new(ago(60), now + SignedDuration::from_mins(1)).unwrap();
	let fixes = store
		.series(&"location.fix".parse().unwrap(), range, 10_000)
		.unwrap();
	assert_eq!(fixes.iter().map(SeriesBatch::len).sum::<usize>(), 2);
	let focus = store
		.records(
			&RecordQuery::new(Selector::stream("apps.focus".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	let apps: Vec<_> = focus
		.iter()
		.map(|record| record.data["app"].as_str().unwrap())
		.collect();
	assert_eq!(apps, ["Chrome", "Notes"]);
	let states = store
		.records(
			&RecordQuery::new(Selector::stream("device.state".parse().unwrap()), range)
				.oldest_first(),
		)
		.unwrap();
	assert_eq!(states.len(), 2);
	let power = store
		.records(&RecordQuery::new(
			Selector::stream("device.power".parse().unwrap()),
			range,
		))
		.unwrap();
	assert_eq!(power.len(), 1);
	assert_eq!(power[0].data["charging"], "charging");

	// Everything the phone needs survives a restart, except the key, which
	// only the keystore holds.
	phone.stop();
	let mut phone = Phone::start(dir.path());
	let again = phone.settled().await;
	assert_eq!(again.link, Link::Ready);
	assert_eq!(again.storage, Storage::Locked);
	let wrong = phone.command(Command::NewStorageKey).await;
	let Some(ReplyData::StorageKey { key: wrong }) = wrong.data else {
		panic!("expected a storage key");
	};
	// The wrong key cannot open the spool the first key sealed.
	let refused = phone
		.command(Command::OpenStorage {
			key: wrong,
			fresh: false,
		})
		.await;
	assert!(!refused.ok);
	assert!(matches!(phone.snapshot().storage, Storage::Failed { .. }));
	assert!(
		phone
			.command(Command::OpenStorage { key, fresh: false })
			.await
			.ok
	);
	assert_eq!(phone.snapshot().storage, Storage::Open);
	assert_eq!(phone.command(Command::UsagePlan).await.data, None);

	// Switched off, the phone keeps nothing.
	assert!(phone.command(Command::Collect { enabled: false }).await.ok);
	let later = updates(0, &[fix(ago(5), 51.5030)]);
	assert!(phone.reading(Reading::Location(&later)).await.ok);
	assert_eq!(phone.snapshot().waiting, 0);
	assert!(!phone.snapshot().collecting);
	assert!(phone.command(Command::Collect { enabled: true }).await.ok);
	assert!(phone.reading(Reading::Location(&later)).await.ok);
	assert_eq!(phone.snapshot().waiting, 1);

	// Forgetting the vault keeps what is waiting and drops the identity.
	assert!(dir.path().join("phone.identity.json").exists());
	assert!(phone.command(Command::Forget).await.ok);
	let forgotten = phone.snapshot();
	assert_eq!(forgotten.link, Link::Unpaired);
	assert_eq!(forgotten.waiting, 1);
	assert!(!dir.path().join("phone.identity.json").exists());
	assert!(phone.command(Command::Upload).await.ok);
	assert_eq!(phone.snapshot().waiting, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vault_that_is_out_of_reach_is_said_plainly_and_retried() {
	let dir = tempfile::tempdir().unwrap();
	let mut phone = Phone::start(dir.path());
	phone.settled().await;
	let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let address = probe.local_addr().unwrap();
	drop(probe);
	let link = format!("quant://link?addr={address}&fp={}", "ab".repeat(32));
	let reply = phone.command(Command::Pair { link }).await;
	assert!(!reply.ok);
	let snapshot = phone.snapshot();
	assert!(matches!(snapshot.link, Link::Unreachable { .. }));
	assert!(snapshot.notice.is_some_and(|notice| notice.error));
	// The link is kept, so the next tick tries again without another paste.
	assert!(snapshot.vault.is_some());
	assert!(phone.command(Command::Tick).await.ok);
	assert!(matches!(phone.snapshot().link, Link::Unreachable { .. }));
}
