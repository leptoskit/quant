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

#[tokio::test]
async fn only_the_owner_configures_and_pauses_capture() {
	let h = start().await;
	let owner = h.owner().await;
	let mut config = owner.capture_config().await.unwrap();
	assert_eq!(config["audio"]["enabled"], false);
	config["audio"]["enabled"] = true.into();
	config["screen"]["interval_secs"] = 30.into();
	owner.set_capture_config(&config).await.unwrap();
	let stored = quant_daemon::capture::config(&h.state).unwrap();
	assert!(stored.audio.enabled);
	assert_eq!(stored.screen.interval_secs, 30);

	let paused = owner
		.pause_capture(&quant_api::PauseRequest {
			paused: true,
			minutes: Some(30),
		})
		.await
		.unwrap();
	assert!(matches!(paused.paused, Some(quant_api::Paused::Until(_))));
	assert!(
		paused
			.capture
			.iter()
			.all(|s| !s.running && s.detail.as_deref() == Some("paused"))
	);
	let resumed = owner
		.pause_capture(&quant_api::PauseRequest {
			paused: false,
			minutes: None,
		})
		.await
		.unwrap();
	assert_eq!(resumed.paused, None);

	let actions: Vec<_> = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap()
		.entries
		.into_iter()
		.map(|e| e.action)
		.collect();
	for action in ["capture.configure", "capture.pause", "capture.resume"] {
		assert!(actions.iter().any(|a| a == action), "{action} not audited");
	}
}

/// Encode one minute of speech-like audio as a real AAC chunk. Each `voice`
/// sounds different so no two chunks hash the same.
#[cfg(target_os = "macos")]
fn aac_chunk(seconds: u32, voice: u32) -> Vec<u8> {
	let rate = 16_000_u32;
	let mut writer = quant_capture_macos::AacWriter::new(rate, 24_000).unwrap();
	let mut samples = Vec::new();
	let frequency = 180.0 + (voice % 5) as f32 * 40.0;
	for i in 0..rate as usize * seconds as usize {
		let t = i as f32 / rate as f32;
		samples.push((t * frequency * std::f32::consts::TAU).sin() * 0.3);
	}
	writer.write(&samples).unwrap();
	writer.finish().unwrap().bytes
}

/// Seed the vault with three minutes of rolling-buffer chunks ending now.
#[cfg(target_os = "macos")]
fn seed_buffer(state: &State) -> Timestamp {
	let device = DeviceId::new();
	let now = Timestamp::now();
	let started = now - SignedDuration::from_mins(3);
	let mut at = started;
	for index in 0..3 {
		let ended = at + SignedDuration::from_mins(1);
		let bytes = aac_chunk(60, index);
		let media = state
			.media(&"audio.buffer".parse().unwrap(), &bytes, "audio/mp4")
			.unwrap();
		state
			.record(
				Record::new("audio.buffer".parse().unwrap(), device)
					.starting(at)
					.ending(ended)
					.with("source", if index % 2 == 0 { "mic" } else { "system" })
					.with("device", "test microphone")
					.with("speech_ms", 30_000)
					.with("peak_dbfs", -12.5)
					.with("bytes", bytes.len())
					.with_media(media),
			)
			.unwrap();
		at = ended;
	}
	started
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn saving_a_moment_needs_write_scope_and_never_returns_audio() {
	let h = start().await;
	let buffer_from = seed_buffer(&h.state);
	let now = Timestamp::now();

	// A read-only client can see the chunks exist, but not their audio…
	let reader = h
		.paired("Reader", ClientKind::Agent, approve(&["audio:read"]))
		.await;
	let listed = reader
		.records(&RecordsRequest {
			streams: vec!["audio.buffer".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(listed.records.len(), 3);
	assert!(listed.records.iter().all(|record| record.media.is_none()));

	// …and cannot save.
	let refused = reader
		.save_audio(&quant_api::SaveAudio {
			from: buffer_from,
			to: now,
			title: None,
		})
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("forbidden"));
	assert!(
		h.owner()
			.await
			.audit(&quant_api::AuditRequest::default())
			.await
			.unwrap()
			.entries
			.iter()
			.any(|entry| {
				entry.action == "audio.save"
					&& matches!(entry.outcome, quant_access::audit::Outcome::Denied { .. })
			}),
		"the refused save was not audited"
	);

	// A write client saves the range; the answer has ids and sizes only.
	let writer = h
		.paired("Writer", ClientKind::Agent, approve(&["audio:write"]))
		.await;
	let saved = writer
		.save_audio(&quant_api::SaveAudio {
			from: buffer_from + SignedDuration::from_mins(1),
			to: now,
			title: Some("Tuesday call".into()),
		})
		.await
		.unwrap();
	assert_eq!(saved.chunks, 2);
	assert!(saved.bytes > 1000);
	let body = serde_json::to_value(saved).unwrap();
	let keys: Vec<_> = body.as_object().unwrap().keys().collect();
	assert_eq!(keys, ["bytes", "chunks", "from", "id", "to"]);
	assert!(saved.to > saved.from);

	// The owner can save too.
	let owner_saved = h
		.owner()
		.await
		.save_audio(&quant_api::SaveAudio {
			from: buffer_from,
			to: now,
			title: None,
		})
		.await
		.unwrap();
	assert_eq!(owner_saved.chunks, 3);

	// The clip record carries the title and its sources; its media has no
	// expiry, so the sweep that clears the buffer never clears a clip.
	let raw = h
		.paired("Raw", ClientKind::Agent, approve(&["audio:raw"]))
		.await;
	let clips = raw
		.records(&RecordsRequest {
			streams: vec!["audio.clip".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(clips.records.len(), 2);
	let clip = clips
		.records
		.iter()
		.find(|record| record.id == saved.id)
		.unwrap();
	assert_eq!(clip.text.as_deref(), Some("Tuesday call"));
	assert_eq!(clip.data["title"], "Tuesday call");
	assert_eq!(clip.data["sources"], serde_json::json!(["mic", "system"]));
	let media = clip.media.clone().unwrap();
	let (mime, bytes) = raw.media(&media.hash).await.unwrap();
	assert_eq!(mime, "audio/mp4");
	assert!(!bytes.is_empty());

	// A reader cannot fetch the clip's audio either: it answers exactly
	// like media that does not exist.
	let denied = reader.media(&media.hash).await.unwrap_err();
	assert_eq!(denied.code(), Some("not_found"));

	// Two days later the sweep deletes the expired buffer chunks — and never
	// the saved clip.
	let later = Timestamp::now() + SignedDuration::from_hours(48);
	let swept = h.state.vault.sweep_media(later).unwrap();
	assert!(swept >= 3, "the buffer chunks were not swept");
	assert!(!swept_removed(&h.state, &media.hash));
	assert_eq!(h.state.vault.get_media(&media.hash).unwrap(), bytes);
	assert!(
		h.state
			.vault
			.store
			.media_mime(&media.hash)
			.unwrap()
			.is_some()
	);
}

/// Whether the sweep removed this hash.
#[cfg(target_os = "macos")]
fn swept_removed(state: &State, hash: &str) -> bool {
	matches!(
		state.vault.get_media(hash),
		Err(quant_store::StoreError::NotFound)
	)
}

#[tokio::test]
async fn any_approved_client_sees_capture_state_but_nothing_captured() {
	let h = start().await;
	h.state.status(
		"screen",
		false,
		Some("screen recording permission not granted".into()),
	);
	h.state.status(
		"audio",
		false,
		Some("microphone permission not granted".into()),
	);
	h.state.status("apps", true, None);
	seed(&h.state);

	let mut pending = Client::new(h.endpoint.clone(), Identity::generate("pending"));
	pending.pair(ClientKind::Device, &[]).await.unwrap();
	assert_eq!(
		pending.capture_status().await.unwrap_err().code(),
		Some("unauthenticated")
	);

	let desktop = h
		.paired("desktop", ClientKind::Device, approve(&["audio:write"]))
		.await;
	let state = desktop.capture_status().await.unwrap();
	assert_eq!(state.paused, None);
	let source = |name: &str| state.sources.iter().find(|s| s.source == name).unwrap();
	assert_eq!(
		source("screen").needs,
		Some(quant_api::Permission::ScreenRecording)
	);
	assert_eq!(
		source("audio").needs,
		Some(quant_api::Permission::Microphone)
	);
	assert!(source("apps").running);

	// The raw answer carries no counts, capture times, details, or text.
	let json = serde_json::to_string(&state).unwrap();
	for leak in ["captured", "last", "detail", "Budget", "Numbers"] {
		assert!(!json.contains(leak), "capture state leaks {leak}: {json}");
	}

	h.owner()
		.await
		.pause_capture(&quant_api::PauseRequest {
			paused: true,
			minutes: Some(30),
		})
		.await
		.unwrap();
	let paused = desktop.capture_status().await.unwrap();
	assert!(matches!(paused.paused, Some(quant_api::Paused::Until(_))));
	assert!(
		paused
			.sources
			.iter()
			.filter(|s| quant_capture_macos::SOURCES.contains(&s.source.as_str()))
			.all(|s| !s.running && s.needs.is_none())
	);
}

#[tokio::test]
async fn the_owner_can_read_the_newest_audit_entries() {
	let h = start().await;
	for name in ["one", "two", "three"] {
		h.paired(name, ClientKind::Agent, approve(&["health:summary"]))
			.await;
	}
	let owner = h.owner().await;
	let all = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	let newest = owner
		.audit(&quant_api::AuditRequest {
			limit: Some(2),
			newest: true,
			..Default::default()
		})
		.await
		.unwrap();
	let seqs: Vec<u64> = newest.entries.iter().map(|e| e.seq).collect();
	let last = all.chain_len.unwrap();
	assert_eq!(seqs, vec![last - 1, last]);
	assert_eq!(newest.chain_ok, Some(true));
}

#[tokio::test]
async fn clips_are_renamed_with_audio_write_and_keep_their_transcript() {
	let h = start().await;
	let device = DeviceId::new();
	let clip = Record::new("audio.clip".parse().unwrap(), device)
		.with("title", "Moment at 14:02")
		.with_text("Moment at 14:02\n\nwe agreed to ship on Friday");
	let note = Record::new("memory.note".parse().unwrap(), device).with_text("a note");
	h.state
		.vault
		.store
		.insert_records(&[clip.clone(), note.clone()])
		.unwrap();

	let reader = h
		.paired("Reader", ClientKind::Agent, approve(&["audio:read"]))
		.await;
	let refused = reader
		.rename_clip(&quant_api::RenameClip {
			id: clip.id,
			title: "Mine now".into(),
		})
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("forbidden"));

	let app = h
		.paired(
			"Desktop",
			ClientKind::Device,
			approve(&["audio:write", "memory:write"]),
		)
		.await;
	app.rename_clip(&quant_api::RenameClip {
		id: clip.id,
		title: "Release planning".into(),
	})
	.await
	.unwrap();
	let renamed = h.state.vault.store.record(clip.id).unwrap().unwrap();
	assert_eq!(renamed.data["title"], "Release planning");
	assert_eq!(
		renamed.text.as_deref(),
		Some("Release planning\n\nwe agreed to ship on Friday")
	);

	// Only clips can be renamed, whatever else the grant may write.
	let other = app
		.rename_clip(&quant_api::RenameClip {
			id: note.id,
			title: "Nope".into(),
		})
		.await
		.unwrap_err();
	assert_eq!(other.code(), Some("not_found"));
}
