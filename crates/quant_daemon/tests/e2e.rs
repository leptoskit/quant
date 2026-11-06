//! The daemon end to end: a real vault, a real socket, signed requests.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Limits;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::ClientStatus;
use quant_api::DurationsRequest;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_api::OverviewRequest;
use quant_api::PurgeRequest;
use quant_api::RecordsRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_crypto::KdfParams;
use quant_daemon::State;
use quant_store::Vault;
use quant_store::VaultPaths;

const PASS: &str = "correct horse battery staple";

struct Harness {
	_dir: tempfile::TempDir,
	endpoint: Endpoint,
	state: Arc<State>,
	_stop: tokio::sync::oneshot::Sender<()>,
}

async fn start() -> Harness {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("v"));
	let vault = Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap();
	let state = Arc::new(State::new(vault).unwrap());
	let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
	let socket = paths.socket();
	let serving = Arc::clone(&state);
	let path = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &path, async {
			let _ = stopped.await;
		})
		.await
		.unwrap();
	});
	for _ in 0..100 {
		if socket.exists() {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	Harness {
		_dir: dir,
		endpoint: Endpoint::Unix(socket),
		state,
		_stop: stop,
	}
}

impl Harness {
	async fn owner(&self) -> OwnerSession {
		OwnerSession::open(self.endpoint.clone(), PASS)
			.await
			.unwrap()
	}

	async fn paired(&self, name: &str, kind: ClientKind, approve: ApproveRequest) -> Client {
		let mut client = Client::new(self.endpoint.clone(), Identity::generate(name));
		let pairing = client.pair(kind, &[]).await.unwrap();
		self.owner()
			.await
			.approve(&ApproveRequest {
				client: pairing.pairing_code,
				..approve
			})
			.await
			.unwrap();
		client
	}
}

fn approve(scopes: &[&str]) -> ApproveRequest {
	ApproveRequest {
		client: String::new(),
		scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
		label: None,
		lookback_hours: None,
		expires_in_hours: None,
		limits: None,
		unredacted: false,
	}
}

fn seed(state: &State) -> DeviceId {
	let device = DeviceId::new();
	let now = Timestamp::now();
	let ago = |mins: i64| now - SignedDuration::from_mins(mins);
	let jpeg = state
		.media(&"screen.frame".parse().unwrap(), b"fake jpeg", "image/jpeg")
		.unwrap();
	for record in [
		Record::new("screen.frame".parse().unwrap(), device)
			.starting(ago(30))
			.with_text(concat!(
				"Quarterly budget review. API key sk-",
				"ant-api03-abcdefghijklmnopqrstuvwx"
			))
			.with("app", "Numbers")
			.with("title", "Budget 2027")
			.with_media(jpeg),
		Record::new("apps.focus".parse().unwrap(), device)
			.starting(ago(60))
			.ending(ago(20))
			.with("app", "Numbers")
			.with("title", "Budget 2027"),
		Record::new("apps.focus".parse().unwrap(), device)
			.starting(ago(20))
			.ending(ago(5))
			.with("app", "Zed")
			.with("title", "secret-project/main.rs"),
	] {
		state.record(record).unwrap();
	}
	state
		.series(SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device,
			start: ago(30),
			channels: vec!["bpm".into()],
			offsets_ms: (0..30).map(|i| i * 60_000).collect(),
			values: (0..30).map(|i| 60.0 + f64::from(i)).collect(),
		})
		.unwrap();
	device
}

#[tokio::test]
async fn unpaired_and_pending_clients_see_nothing() {
	let h = start().await;
	let mut client = Client::new(h.endpoint.clone(), Identity::generate("Stranger"));
	assert!(client.records(&RecordsRequest::default()).await.is_err());

	let pairing = client
		.pair(ClientKind::Agent, &["*:raw".into()])
		.await
		.unwrap();
	assert_eq!(pairing.status, ClientStatus::Pending);
	let me = client.me().await.unwrap();
	assert_eq!(me.status, ClientStatus::Pending);
	let refused = client
		.records(&RecordsRequest::default())
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
}

#[tokio::test]
async fn owner_actions_need_the_passphrase_and_wrong_guesses_lock_out() {
	let h = start().await;
	let wrong = OwnerSession::open(h.endpoint.clone(), "guessing guessing")
		.await
		.unwrap_err();
	assert_eq!(wrong.code(), Some("unauthenticated"));
	let locked = OwnerSession::open(h.endpoint.clone(), PASS)
		.await
		.unwrap_err();
	assert_eq!(locked.code(), Some("locked"));
}

#[tokio::test]
async fn summary_grants_get_aggregates_but_never_text() {
	let h = start().await;
	seed(&h.state);
	let agent = h
		.paired(
			"Coach",
			ClientKind::Agent,
			approve(&["health:summary", "apps:summary"]),
		)
		.await;

	let records = agent.records(&RecordsRequest::default()).await.unwrap();
	assert_eq!(records.records, [] as [quant_api::ServedRecord; 0]);
	let screen = agent
		.records(&RecordsRequest {
			streams: vec!["screen".into(), "apps".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(screen.records, [] as [quant_api::ServedRecord; 0]);
	assert_eq!(screen.withheld, vec!["screen", "apps"]);
	assert_eq!(
		agent
			.search(&RecordsRequest {
				search: Some("budget".into()),
				..Default::default()
			})
			.await
			.unwrap()
			.records,
		[] as [quant_api::ServedRecord; 0]
	);

	let hr = agent
		.series(&SeriesRequest {
			stream: "health.heart_rate".into(),
			from: None,
			to: None,
			bucket_seconds: Some(3600),
			raw: true,
			max_rows: None,
		})
		.await
		.unwrap();
	assert_eq!(hr.batches, [] as [quant_core::SeriesBatch; 0]);
	assert!(hr.note.is_some());
	let total: u64 = hr.buckets.iter().map(|b| b.channels["bpm"].count).sum();
	assert_eq!(total, 30);

	let by_app = agent
		.durations(&DurationsRequest {
			stream: "apps.focus".into(),
			from: None,
			to: None,
			group_by: Some("app".into()),
			bucket_seconds: None,
		})
		.await
		.unwrap();
	let numbers = by_app
		.items
		.iter()
		.find(|i| i.group.as_deref() == Some("Numbers"))
		.unwrap();
	assert!((numbers.seconds - 2400.0).abs() < 1.0);

	let by_title = agent
		.durations(&DurationsRequest {
			stream: "apps.focus".into(),
			from: None,
			to: None,
			group_by: Some("title".into()),
			bucket_seconds: None,
		})
		.await
		.unwrap_err();
	assert_eq!(by_title.code(), Some("forbidden"));

	let overview = agent.overview(&OverviewRequest::default()).await.unwrap();
	let names: Vec<_> = overview.streams.iter().map(|s| s.stream.as_str()).collect();
	assert_eq!(names, vec!["apps.focus", "health.heart_rate"]);
}

#[tokio::test]
async fn read_grants_get_redacted_text_and_raw_grants_get_media() {
	let h = start().await;
	seed(&h.state);
	let reader = h
		.paired("Reader", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let hits = reader
		.search(&RecordsRequest {
			search: Some("budget".into()),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(hits.records.len(), 1);
	let hit = &hits.records[0];
	assert!(hit.text.as_ref().unwrap().contains("[redacted:api_key]"));
	assert!(!hit.text.as_ref().unwrap().contains("sk-ant"));
	assert!(
		hit.snippet.as_ref().unwrap().contains("[budget]")
			|| hit.snippet.as_ref().unwrap().contains("[Budget]")
	);
	assert!(hit.media.is_none());

	let raw = h
		.paired("Raw", ClientKind::Agent, approve(&["screen:raw"]))
		.await;
	let records = raw
		.records(&RecordsRequest {
			streams: vec!["screen".into()],
			..Default::default()
		})
		.await
		.unwrap();
	let media = records.records[0].media.clone().unwrap();
	let (_, bytes) = raw.media(&media.hash).await.unwrap();
	assert_eq!(&bytes[..], b"fake jpeg");

	// The reader cannot fetch it, and cannot tell it exists.
	let denied = reader.media(&media.hash).await.unwrap_err();
	assert_eq!(denied.code(), Some("not_found"));
}

#[tokio::test]
async fn lookback_hides_older_data() {
	let h = start().await;
	seed(&h.state);
	let mut request = approve(&["apps:read"]);
	request.lookback_hours = Some(0);
	let agent = h.paired("Short memory", ClientKind::Agent, request).await;
	let records = agent
		.records(&RecordsRequest {
			streams: vec!["apps".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(records.records, [] as [quant_api::ServedRecord; 0]);
}

#[tokio::test]
async fn rate_limits_cap_requests_and_records() {
	let h = start().await;
	seed(&h.state);
	let mut request = approve(&["*:read"]);
	request.limits = Some(Limits {
		requests_per_minute: 3,
		records_per_hour: 2,
		bytes_per_day: 1 << 20,
	});
	let agent = h.paired("Greedy", ClientKind::Agent, request).await;

	let first = agent.records(&RecordsRequest::default()).await.unwrap();
	assert_eq!(first.records.len(), 2);
	assert!(first.truncated);
	let second = agent.records(&RecordsRequest::default()).await.unwrap_err();
	assert_eq!(second.code(), Some("rate_limited"));
	let _ = agent.me().await;
	let third = agent.streams().await.unwrap_err();
	assert_eq!(third.code(), Some("rate_limited"));
}

#[tokio::test]
async fn devices_write_only_where_granted() {
	let h = start().await;
	let phone = h
		.paired(
			"Pixel",
			ClientKind::Device,
			approve(&["health:write", "motion:write"]),
		)
		.await;
	let device = DeviceId::new();
	let batch = SeriesBatch {
		stream: "health.heart_rate".parse().unwrap(),
		device,
		start: Timestamp::now() - SignedDuration::from_mins(1),
		channels: vec!["bpm".into()],
		offsets_ms: vec![0, 1000],
		values: vec![70.0, 71.0],
	};
	let stored = phone
		.ingest_series(&IngestSeries {
			batches: vec![batch.clone()],
		})
		.await
		.unwrap();
	assert_eq!(stored.stored, 1);
	let again = phone
		.ingest_series(&IngestSeries {
			batches: vec![batch],
		})
		.await
		.unwrap();
	assert_eq!(again.duplicate, 1);

	let posture = Record::new("motion.posture".parse().unwrap(), device).with("posture", "lying");
	let screen = Record::new("screen.frame".parse().unwrap(), device).with_text("sneaky");
	let mixed = IngestRecords {
		records: vec![posture.clone(), screen],
		device_name: None,
		platform: Some("android".into()),
	};
	assert_eq!(
		phone.ingest_records(&mixed).await.unwrap_err().code(),
		Some("forbidden")
	);
	let ok = IngestRecords {
		records: vec![posture],
		device_name: None,
		platform: Some("android".into()),
	};
	assert_eq!(phone.ingest_records(&ok).await.unwrap().stored, 1);

	// Writing does not grant reading.
	assert_eq!(
		phone
			.records(&RecordsRequest::default())
			.await
			.unwrap()
			.records
			.len(),
		0
	);
}

#[tokio::test]
async fn revoking_cuts_access_and_everything_is_audited() {
	let h = start().await;
	seed(&h.state);
	let agent = h
		.paired(
			"Temp",
			ClientKind::Agent,
			approve(&["*:read", "audit:read"]),
		)
		.await;
	agent.records(&RecordsRequest::default()).await.unwrap();
	let own = agent
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	assert!(own.entries.iter().any(|e| e.action == "records"));
	assert!(own.entries.iter().all(|e| e.action != "grant.create"));

	let owner = h.owner().await;
	let code = owner.clients().await.unwrap()[0].pairing_code.clone();
	owner.revoke(&code).await.unwrap();
	assert_eq!(
		agent
			.records(&RecordsRequest::default())
			.await
			.unwrap_err()
			.code(),
		Some("unauthenticated")
	);

	let audit = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	assert_eq!(audit.chain_ok, Some(true));
	let actions: Vec<_> = audit.entries.iter().map(|e| e.action.as_str()).collect();
	for expected in ["owner.unlock", "grant.create", "records", "grant.revoke"] {
		assert!(
			actions.contains(&expected),
			"{expected} missing from {actions:?}"
		);
	}
}

#[tokio::test]
async fn owner_can_purge() {
	let h = start().await;
	seed(&h.state);
	let owner = h.owner().await;
	let now = Timestamp::now();
	let purged = owner
		.purge(&PurgeRequest {
			streams: vec!["screen".into()],
			from: now - SignedDuration::from_hours(2),
			to: now,
		})
		.await
		.unwrap();
	assert_eq!(purged.records, 1);
	assert_eq!(purged.media, 1);
	let reader = h
		.paired("After", ClientKind::Agent, approve(&["*:read"]))
		.await;
	let left = reader
		.records(&RecordsRequest {
			streams: vec!["screen".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(left.records, [] as [quant_api::ServedRecord; 0]);
}

#[tokio::test]
async fn devices_connect_over_tls_only_to_the_pinned_certificate() {
	let h = start().await;
	let identity = quant_daemon::tls::TlsIdentity::load_or_create(&h.state.vault.store).unwrap();
	let again = quant_daemon::tls::TlsIdentity::load_or_create(&h.state.vault.store).unwrap();
	assert_eq!(identity.fingerprint(), again.fingerprint());

	let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let address = probe.local_addr().unwrap();
	drop(probe);
	let serving = Arc::clone(&h.state);
	tokio::spawn(async move {
		quant_daemon::serve_tls(serving, address, &identity, std::future::pending())
			.await
			.unwrap();
	});
	tokio::time::sleep(std::time::Duration::from_millis(100)).await;

	let fingerprint = quant_client::parse_fingerprint(&again.fingerprint()).unwrap();
	let pinned = Endpoint::Tls {
		address: address.to_string(),
		fingerprint,
	};
	let hello = Client::new(pinned.clone(), Identity::generate("phone"))
		.hello()
		.await
		.unwrap();
	assert_eq!(
		hello.tls_fingerprint.as_deref(),
		Some(again.fingerprint().as_str())
	);

	let mut phone = Client::new(pinned, Identity::generate("Pixel"));
	let pairing = phone
		.pair(ClientKind::Device, &["health:write".into()])
		.await
		.unwrap();
	assert_eq!(pairing.status, ClientStatus::Pending);

	let wrong = Endpoint::Tls {
		address: address.to_string(),
		fingerprint: [7; 32],
	};
	assert!(
		Client::new(wrong, Identity::generate("mitm"))
			.hello()
			.await
			.is_err()
	);
	assert!(Endpoint::parse(&format!("tls://{address}#{}", again.fingerprint())).is_ok());
	assert!(Endpoint::parse(&format!("tls://{address}")).is_err());
}
