//! Integration tests for the opt-in phone companion against a live `quantd`:
//! - pinned write-only collector scopes vs separate companion capabilities
//! - stats-only, ask-only, and explicit owner-approved capability changes on
//!   one companion identity
//! - bounded catalog paging (> 24 streams, custom streams) and on-demand stream
//!   detail paging (> 8 channels, > 8 duration groups) with exact units/values
//! - end-to-end Ask messaging with a real signed agent, idempotent dedupe
//!   retry, cited replies, and zero disk/spool leakage

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use leptoskit_plugin_location::Fix;
use leptoskit_plugin_location::Provider;
use quant_access::AgentIdentity;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::CompanionAnswer;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::Channel;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::StreamKind;
use quant_core::StreamSpec;
use quant_crypto::KdfParams;
use quant_model::phone::Command;
use quant_model::phone::CompanionAgentsReply;
use quant_model::phone::CompanionCapability;
use quant_model::phone::CompanionMessageReply;
use quant_model::phone::CompanionMessagesReply;
use quant_model::phone::CompanionReplyStatus;
use quant_model::phone::Link;
use quant_model::phone::MAX_STATS_CHANNELS;
use quant_model::phone::MAX_STATS_GROUPS;
use quant_model::phone::MAX_STATS_STREAMS;
use quant_model::phone::PHONE_SCOPES;
use quant_model::phone::Reading;
use quant_model::phone::Reply;
use quant_model::phone::ReplyData;
use quant_model::phone::Snapshot;
use quant_model::phone::StatsAccess;
use quant_model::phone::StatsDetail;
use quant_model::phone::StatsReply;
use quant_model::phone::StatsWindow;
use quant_model::phone::Storage;
use quant_model::phone::StreamDetailReply;
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
	tls: Endpoint,
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

	let link = format!("quant://link?addr={address}&fp={fingerprint}&vault=test");
	let tls = Endpoint::parse(&link).unwrap();
	Daemon {
		state,
		owner: Endpoint::Unix(socket),
		tls,
		link,
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

	async fn pair_agent(&self, name: &str, scopes: &[&str]) -> Client {
		let mut client = Client::new(self.tls.clone(), Identity::generate(name));
		let requested: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
		let agent = AgentIdentity {
			product: "Claude Code".into(),
			vendor: "Anthropic".into(),
			model: "claude-opus".into(),
			version: "1.0".into(),
			host: "macbook".into(),
			operator: None,
		};
		let pairing = client
			.pair(ClientKind::Agent, &requested, Some(&agent))
			.await
			.unwrap();
		self.approve_with(&pairing.pairing_code, scopes).await;
		client
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
			name: "Pixel".into(),
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

	async fn pair_collector(&mut self, daemon: &Daemon) {
		assert!(
			self.command(Command::Pair {
				link: daemon.link.clone(),
			})
			.await
			.ok
		);
		let Link::Pending { code } = self.snapshot().link else {
			panic!("expected collector pending code");
		};
		daemon.approve_with(&code, &PHONE_SCOPES).await;
		assert!(self.command(Command::Upload).await.ok);
		assert_eq!(self.snapshot().link, Link::Ready);
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
		other => panic!("expected stats reply, got {other:?}"),
	}
}

fn detail_reply(reply: &Reply) -> StreamDetailReply {
	match &reply.data {
		Some(ReplyData::VaultStreamDetail { detail }) => detail.as_ref().clone(),
		other => panic!("expected stream detail reply, got {other:?}"),
	}
}

fn agents_reply(reply: &Reply) -> CompanionAgentsReply {
	match &reply.data {
		Some(ReplyData::CompanionAgents { reply }) => reply.clone(),
		other => panic!("expected agents reply, got {other:?}"),
	}
}

fn messages_reply(reply: &Reply) -> CompanionMessagesReply {
	match &reply.data {
		Some(ReplyData::CompanionMessages { reply }) => reply.clone(),
		other => panic!("expected messages reply, got {other:?}"),
	}
}

fn message_reply(reply: &Reply) -> CompanionMessageReply {
	match &reply.data {
		Some(ReplyData::CompanionMessage { reply }) => reply.clone(),
		other => panic!("expected message reply, got {other:?}"),
	}
}

fn pending_code(access: &StatsAccess) -> &str {
	match access {
		StatsAccess::Pending { code } => code,
		other => panic!("expected pending access, got {other:?}"),
	}
}

#[allow(clippy::cast_precision_loss)]
fn location_frame(at: Timestamp) -> Vec<u8> {
	let fix = Fix {
		timestamp_ms: at.as_millisecond() as f64,
		latitude: 51.501,
		longitude: -0.127,
		accuracy_m: 10.0,
		altitude_m: None,
		altitude_accuracy_m: None,
		heading_degrees: None,
		speed_mps: None,
		provider: Provider::Fused,
		simulated: Some(false),
	};
	let mut bytes = 0u32.to_le_bytes().to_vec();
	bytes.extend(1u32.to_le_bytes());
	bytes.extend(fix.encode().unwrap());
	bytes
}

fn contains_bytes_under(dir: &Path, needle: &[u8]) -> bool {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return false;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_dir() {
			if contains_bytes_under(&path, needle) {
				return true;
			}
		} else if let Ok(bytes) = std::fs::read(&path)
			&& bytes.windows(needle.len()).any(|window| window == needle)
		{
			return true;
		}
	}
	false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn all_permitted_streams_channels_and_duration_groups_are_reachable_by_paging() {
	let daemon = daemon().await;
	let owner = daemon.owner().await;

	// Register 12 custom streams so the vault holds well over MAX_STATS_STREAMS
	// (24), including a custom multi-channel stream with 10 channels (>
	// MAX_STATS_CHANNELS = 8).
	let multi_channels: Vec<Channel> = (0..10)
		.map(|idx| {
			Channel {
				name: format!("ch_{idx:02}"),
				unit: format!("u{idx}"),
			}
		})
		.collect();
	owner
		.register_stream(&StreamSpec {
			id: "sensor.custom.vest_panel".parse().unwrap(),
			kind: StreamKind::Series,
			description: "Ten-channel custom vest telemetry.".into(),
			channels: multi_channels.clone(),
			summary_keys: Vec::new(),
			classes: Vec::new(),
			media_retention_hours: None,
		})
		.await
		.unwrap();
	for idx in 0..11 {
		owner
			.register_stream(&StreamSpec {
				id: format!("sensor.custom.probe_{idx:02}").parse().unwrap(),
				kind: StreamKind::Series,
				description: format!("Custom probe {idx}."),
				channels: vec![Channel {
					name: "reading".into(),
					unit: "mV".into(),
				}],
				summary_keys: Vec::new(),
				classes: Vec::new(),
				media_retention_hours: None,
			})
			.await
			.unwrap();
	}

	let now = Timestamp::now();
	let watch = DeviceId::new();
	let store = &daemon.state.vault.store;

	// Cross-device heart rate: 1 sample 2 hours ago (in Day & Week), 1 sample 3
	// days ago (Week only).
	store
		.insert_series(&SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device: watch,
			start: now - SignedDuration::from_hours(2),
			channels: vec!["bpm".into()],
			offsets_ms: vec![0, 60_000],
			values: vec![64.0, 80.0],
		})
		.unwrap();
	store
		.insert_series(&SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device: watch,
			start: now - SignedDuration::from_hours(72),
			channels: vec!["bpm".into()],
			offsets_ms: vec![0],
			values: vec![56.0],
		})
		.unwrap();

	// Ten-channel custom stream sample batch.
	let channel_names: Vec<String> = multi_channels.iter().map(|c| c.name.clone()).collect();
	let values: Vec<f64> = (0..10).map(|idx| f64::from(idx + 1) * 10.0).collect();
	store
		.insert_series(&SeriesBatch {
			stream: "sensor.custom.vest_panel".parse().unwrap(),
			device: watch,
			start: now - SignedDuration::from_mins(20),
			channels: channel_names,
			offsets_ms: vec![0],
			values,
		})
		.unwrap();

	// Ten distinct apps in apps.focus (> MAX_STATS_GROUPS = 8).
	let mut focus_records = Vec::new();
	for idx in 0..10 {
		let start = now - SignedDuration::from_mins(200 - i64::from(idx) * 15);
		let end = start + SignedDuration::from_mins(i64::from(10 - idx));
		focus_records.push(
			Record::new("apps.focus".parse().unwrap(), watch)
				.starting(start)
				.ending(end)
				.with("app", format!("App{idx:02}")),
		);
	}
	store.insert_records(&focus_records).unwrap();

	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;
	phone.pair_collector(&daemon).await;

	assert!(
		phone
			.command(Command::RequestCompanion {
				capabilities: vec![CompanionCapability::StatsSummary],
			})
			.await
			.ok
	);
	let code = pending_code(&phone.snapshot().stats.access).to_owned();
	daemon.approve_with(&code, &["*:summary"]).await;

	// Page through the ENTIRE permitted catalog without missing any stream.
	let mut offset = 0usize;
	let mut seen = Vec::new();
	let mut total_expected;
	loop {
		let reply = phone
			.command(Command::VaultCatalog {
				window: StatsWindow::Day,
				offset,
				limit: MAX_STATS_STREAMS,
			})
			.await;
		assert!(reply.ok, "{reply:?}");
		let stats = stats_reply(&reply);
		assert_eq!(stats.access, StatsAccess::Ready);
		let page = stats.summary.expect("catalog page");
		assert!(page.streams.len() <= MAX_STATS_STREAMS);
		total_expected = page.total_streams;
		seen.extend(page.streams.into_iter().map(|s| s.stream));
		match page.next_offset {
			Some(next) => {
				assert!(next > offset);
				offset = next;
			}
			None => break,
		}
	}
	assert!(
		total_expected > MAX_STATS_STREAMS,
		"expected > 24 streams, got {total_expected}"
	);
	assert_eq!(seen.len(), total_expected);
	assert!(seen.contains(&"sensor.custom.vest_panel".to_owned()));
	assert!(seen.contains(&"sensor.custom.probe_10".to_owned()));
	assert!(seen.contains(&"health.heart_rate".to_owned()));

	// On-demand detail for a stream beyond the initial 24-stream cap (empty
	// custom probe).
	let probe_detail = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "sensor.custom.probe_10".into(),
				window: StatsWindow::Day,
				channel_offset: 0,
				group_offset: 0,
			})
			.await,
	);
	let probe_stream = probe_detail.stream.expect("custom probe detail");
	assert_eq!(probe_stream.count, 0);
	assert_eq!(probe_stream.count_unit, "samples");
	let StatsDetail::Numeric { channels, .. } = probe_stream.detail else {
		panic!("expected numeric detail");
	};
	assert_eq!(channels[0].unit, "mV");
	assert_eq!(channels[0].mean, None);

	// On-demand channel paging for 10-channel custom stream: reaches channels
	// 0..8 and 8..10.
	let ch_p1 = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "sensor.custom.vest_panel".into(),
				window: StatsWindow::Day,
				channel_offset: 0,
				group_offset: 0,
			})
			.await,
	);
	assert_eq!(ch_p1.total_channels, 10);
	assert_eq!(ch_p1.next_channel_offset, Some(MAX_STATS_CHANNELS));
	let StatsDetail::Numeric {
		channels: first_channels,
		..
	} = ch_p1.stream.unwrap().detail
	else {
		panic!("expected numeric channels");
	};
	assert_eq!(first_channels.len(), MAX_STATS_CHANNELS);
	assert_eq!(first_channels[0].unit, "u0");

	let ch_p2 = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "sensor.custom.vest_panel".into(),
				window: StatsWindow::Day,
				channel_offset: MAX_STATS_CHANNELS,
				group_offset: 0,
			})
			.await,
	);
	assert_eq!(ch_p2.next_channel_offset, None);
	let StatsDetail::Numeric {
		channels: rest_channels,
		..
	} = ch_p2.stream.unwrap().detail
	else {
		panic!("expected remaining numeric channels");
	};
	assert_eq!(rest_channels.len(), 2);
	assert_eq!(rest_channels[0].name, "ch_08");
	assert_eq!(rest_channels[0].unit, "u8");
	assert_eq!(rest_channels[1].name, "ch_09");
	assert_eq!(rest_channels[1].unit, "u9");

	// On-demand duration-group paging for apps.focus: reaches all 10 groups.
	let dur_p1 = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "apps.focus".into(),
				window: StatsWindow::Day,
				channel_offset: 0,
				group_offset: 0,
			})
			.await,
	);
	assert_eq!(dur_p1.total_groups, 10);
	assert_eq!(dur_p1.next_group_offset, Some(MAX_STATS_GROUPS));
	let dur_p2 = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "apps.focus".into(),
				window: StatsWindow::Day,
				channel_offset: 0,
				group_offset: MAX_STATS_GROUPS,
			})
			.await,
	);
	assert_eq!(dur_p2.next_group_offset, None);
	let StatsDetail::Durations { groups, .. } = dur_p2.stream.unwrap().detail else {
		panic!("expected remaining duration groups");
	};
	assert_eq!(groups.len(), 2);

	// Range change: heart rate has 2 samples in Day (mean 72 bpm) and 3 in
	// Week.
	let hr_day = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "health.heart_rate".into(),
				window: StatsWindow::Day,
				channel_offset: 0,
				group_offset: 0,
			})
			.await,
	)
	.stream
	.unwrap();
	let hr_week = detail_reply(
		&phone
			.command(Command::VaultStreamDetail {
				stream: "health.heart_rate".into(),
				window: StatsWindow::Week,
				channel_offset: 0,
				group_offset: 0,
			})
			.await,
	)
	.stream
	.unwrap();
	assert_eq!(hr_day.count, 2);
	assert_eq!(hr_week.count, 3);
	let StatsDetail::Numeric {
		channels: day_ch, ..
	} = hr_day.detail
	else {
		panic!("expected numeric hr");
	};
	assert_eq!(day_ch[0].unit, "bpm");
	assert!((day_ch[0].mean.unwrap() - 72.0).abs() < 1e-6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn stats_only_ask_only_and_explicit_capability_changes_share_one_companion_identity() {
	let daemon = daemon().await;
	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;
	phone.pair_collector(&daemon).await;

	// Verify collector scopes stay strictly pinned to write-only PHONE_SCOPES.
	let clients = daemon.owner().await.clients().await.unwrap();
	assert_eq!(clients.len(), 1);
	assert_eq!(
		clients[0].requested,
		PHONE_SCOPES
			.iter()
			.map(|s| (*s).to_owned())
			.collect::<Vec<_>>()
	);
	assert!(
		clients[0]
			.requested
			.iter()
			.all(|s| s.ends_with(":write") && s != "companion:write")
	);

	// Request Ask-only on the companion identity.
	assert!(
		phone
			.command(Command::RequestCompanion {
				capabilities: vec![CompanionCapability::AskAgents],
			})
			.await
			.ok
	);
	let pending = phone.snapshot();
	let code = pending_code(&pending.ask.access).to_owned();
	assert_eq!(pending.stats.access, StatsAccess::NotRequested);
	assert_eq!(
		pending.ask.approve.as_deref(),
		Some(format!("quant approve {code} --scope companion:write").as_str())
	);

	// Approve Ask-only.
	daemon.approve_with(&code, &["companion:write"]).await;
	let agents = agents_reply(&phone.command(Command::CompanionAgents).await);
	assert_eq!(agents.access, StatsAccess::Ready);
	assert_eq!(phone.snapshot().ask.access, StatsAccess::Ready);
	assert_eq!(phone.snapshot().stats.access, StatsAccess::Restricted);

	// Calling VaultCatalog on an Ask-only grant reports Restricted without
	// destroying the shared companion identity or breaking Ask.
	let stats = stats_reply(
		&phone
			.command(Command::VaultCatalog {
				window: StatsWindow::Day,
				offset: 0,
				limit: MAX_STATS_STREAMS,
			})
			.await,
	);
	assert_eq!(stats.access, StatsAccess::Restricted);
	assert!(phone.companion_identity().exists());
	assert_eq!(phone.snapshot().ask.access, StatsAccess::Ready);

	// Request adding Stats alongside Ask on the same identity.
	assert!(
		phone
			.command(Command::RequestCompanion {
				capabilities: vec![
					CompanionCapability::StatsSummary,
					CompanionCapability::AskAgents,
				],
			})
			.await
			.ok
	);
	let changing = phone.snapshot();
	assert_eq!(changing.ask.access, StatsAccess::Ready);
	let change = changing
		.companion_change
		.expect("pending explicit capability change");
	assert_eq!(change.code, code);
	assert_eq!(
		change.approve,
		format!("quant approve {code} --scope '*:summary' --scope companion:write")
	);

	// Owner approves narrowed read scope + companion:write on the same code.
	daemon
		.approve_with(&code, &["apps:summary", "companion:write"])
		.await;
	assert!(phone.command(Command::RefreshCompanion).await.ok);
	let both = phone.snapshot();
	assert_eq!(both.stats.access, StatsAccess::Ready);
	assert_eq!(both.ask.access, StatsAccess::Ready);
	assert_eq!(both.companion_change, None);

	// Owner removes Ask while keeping narrowed apps:summary.
	let gen_before = both.vault_generation;
	daemon.approve_with(&code, &["apps:summary"]).await;
	let ask_after = agents_reply(&phone.command(Command::CompanionAgents).await);
	assert_eq!(ask_after.access, StatsAccess::Restricted);
	let stats_only = phone.snapshot();
	assert_eq!(stats_only.stats.access, StatsAccess::Ready);
	assert_eq!(stats_only.ask.access, StatsAccess::Restricted);
	assert!(stats_only.vault_generation > gen_before);
	assert!(phone.companion_identity().exists());

	// Requesting Stats + Ask again preserves the owner's narrowed
	// `apps:summary` instead of silently widening to `*:summary`.
	assert!(
		phone
			.command(Command::RequestCompanion {
				capabilities: vec![
					CompanionCapability::StatsSummary,
					CompanionCapability::AskAgents,
				],
			})
			.await
			.ok
	);
	let preserved = phone
		.snapshot()
		.companion_change
		.expect("capability update");
	assert_eq!(
		preserved.approve,
		format!("quant approve {code} --scope apps:summary --scope companion:write")
	);

	// Collector stays write-only and keeps queuing/delivering throughout.
	let loc = location_frame(Timestamp::now() - SignedDuration::from_mins(2));
	phone.reading(Reading::Location(&loc)).await;
	assert!(phone.snapshot().waiting > 0);
	assert!(phone.command(Command::Upload).await.ok);
	assert_eq!(phone.snapshot().waiting, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn ask_end_to_end_with_real_signed_agent_dedupe_citations_and_no_disk_leak() {
	let daemon = daemon().await;
	let agent = daemon.pair_agent("Health Coach", &["health:read"]).await;

	// Seed a real vault record readable by the agent so it can cite it.
	let now = Timestamp::now();
	let rec = Record::new("health.sleep".parse().unwrap(), DeviceId::new())
		.starting(now - SignedDuration::from_hours(8))
		.ending(now - SignedDuration::from_hours(1))
		.with("stage", "deep");
	let cited_id = rec.id;
	daemon.state.vault.store.insert_records(&[rec]).unwrap();

	let mut phone = Phone::start();
	phone.settled().await;
	phone.open_storage().await;
	phone.pair_collector(&daemon).await;

	// Request & approve Ask capability on the companion identity.
	assert!(
		phone
			.command(Command::RequestCompanion {
				capabilities: vec![CompanionCapability::AskAgents],
			})
			.await
			.ok
	);
	let code = pending_code(&phone.snapshot().ask.access).to_owned();
	daemon.approve_with(&code, &["companion:write"]).await;

	// Fetch approved agent directory.
	let directory = agents_reply(&phone.command(Command::CompanionAgents).await);
	assert_eq!(directory.access, StatsAccess::Ready);
	assert_eq!(directory.agents.len(), 1);
	assert_eq!(directory.agents[0].name, "Health Coach");
	let agent_id = directory.agents[0].id.clone();

	// Send question with an idempotency key, then retry with the SAME dedupe
	// key: daemon must return the same message without creating a duplicate.
	let question = "SECRET_PHONE_QUESTION_HOW_WAS_MY_SLEEP";
	let dedupe = format!("m-{}-1", phone.snapshot().session_nonce);
	let first = message_reply(
		&phone
			.command(Command::SendCompanionMessage {
				to: agent_id.clone(),
				question: question.into(),
				dedupe: dedupe.clone(),
			})
			.await,
	);
	assert_eq!(first.access, StatsAccess::Ready);
	let sent = first.message.expect("sent message");
	assert_eq!(sent.status, CompanionReplyStatus::Waiting);
	assert_eq!(sent.answer, None);

	let retry = message_reply(
		&phone
			.command(Command::SendCompanionMessage {
				to: agent_id,
				question: question.into(),
				dedupe,
			})
			.await,
	);
	let retried = retry.message.expect("retried message");
	assert_eq!(retried.id, sent.id);

	// Real signed agent pulls from its inbox and submits a real cited answer.
	let inbox = agent.inbox(None).await.unwrap();
	assert_eq!(inbox.len(), 1);
	assert_eq!(inbox[0].id.to_string(), sent.id);
	assert_eq!(inbox[0].question, question);

	let answer_text = "SECRET_AGENT_ANSWER_SEVEN_HOURS_DEEP";
	agent
		.answer_message(
			inbox[0].id,
			&CompanionAnswer {
				answer: answer_text.into(),
				citations: vec![cited_id],
			},
		)
		.await
		.unwrap();

	// Phone refreshes the single message and the list, seeing the real answer
	// and citation.
	let checked = message_reply(
		&phone
			.command(Command::CompanionMessage {
				id: sent.id.clone(),
			})
			.await,
	)
	.message
	.expect("answered message");
	assert_eq!(checked.status, CompanionReplyStatus::Answered);
	assert_eq!(checked.answer.as_deref(), Some(answer_text));
	assert_eq!(checked.citations, vec![cited_id.to_string()]);

	let history = messages_reply(
		&phone
			.command(Command::CompanionMessages { limit: Some(50) })
			.await,
	);
	assert_eq!(history.messages.len(), 1);
	assert_eq!(history.messages[0].answer.as_deref(), Some(answer_text));

	// Verify neither the typed question nor the agent answer touched local
	// disk or the encrypted spool.
	assert!(!contains_bytes_under(&phone.data(), question.as_bytes()));
	assert!(!contains_bytes_under(&phone.data(), answer_text.as_bytes()));

	// Revoking the companion ends Ask cleanly while collection continues.
	daemon.revoke(&code).await;
	let revoked = messages_reply(
		&phone
			.command(Command::CompanionMessages { limit: Some(50) })
			.await,
	);
	assert_eq!(revoked.access, StatsAccess::Revoked);
	assert!(revoked.messages.is_empty());
	assert!(!phone.companion_identity().exists());
	assert_eq!(phone.snapshot().link, Link::Ready);
}
