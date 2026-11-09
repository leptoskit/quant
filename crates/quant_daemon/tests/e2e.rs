//! The daemon end to end: a real vault, a real socket, signed requests.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::SignedDuration;
use jiff::Timestamp;
use p256::ecdsa::SigningKey;
use p256::ecdsa::signature::Signer as _;
use quant_access::Limits;
use quant_access::OwnerDevicePolicy;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::ClientStatus;
use quant_api::DurationsRequest;
use quant_api::EnrolDeviceRequest;
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
use quant_core::Class;
use quant_core::DeviceId;
use quant_core::OwnerDeviceId;
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
		deny_classes: None,
	}
}

/// A software P-256 key standing in for the hardware key of an owner device.
fn device_key() -> SigningKey {
	let mut seed = [0u8; 32];
	getrandom::fill(&mut seed).unwrap();
	SigningKey::from_bytes((&seed).into()).unwrap()
}

/// The key's uncompressed SEC1 public half, as enrolment wants it.
fn device_public_key(key: &SigningKey) -> String {
	URL_SAFE_NO_PAD.encode(key.verifying_key().to_sec1_bytes())
}

/// Sign with a device key, naming the signature type once.
fn device_sign(key: &SigningKey, message: &[u8]) -> p256::ecdsa::Signature {
	key.sign(message)
}

/// Enrol a device's key with a passphrase session.
async fn enrol_device(h: &Harness, key: &SigningKey, label: &str) -> quant_api::OwnerDeviceView {
	h.owner()
		.await
		.owner_device_add(&EnrolDeviceRequest {
			label: label.to_owned(),
			public_key: device_public_key(key),
			policy: OwnerDevicePolicy::Biometry,
		})
		.await
		.unwrap()
}

/// Open an owner session with an enrolled device, DER-signing the challenge.
async fn unlock_device(
	h: &Harness,
	device: OwnerDeviceId,
	key: &SigningKey,
) -> Result<OwnerSession, quant_client::ClientError> {
	OwnerSession::open_with_device(h.endpoint.clone(), device, |message| {
		device_sign(key, message).to_der().as_bytes().to_vec()
	})
	.await
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

async fn tell(
	owner: &OwnerSession,
	statement: &str,
	topic: Option<&str>,
	classes: &[Class],
) -> quant_core::Claim {
	owner
		.tell(&quant_api::OwnerClaimRequest {
			statement: statement.to_owned(),
			kind: Some(quant_core::ClaimKind::Preference),
			topic: topic.map(str::to_owned),
			subject: Some("me".into()),
			classes: classes.to_vec(),
		})
		.await
		.unwrap()
}

async fn propose(
	client: &Client,
	statement: &str,
	evidence: Vec<quant_core::RecordId>,
	topic: Option<&str>,
) -> Result<quant_api::Proposed, quant_client::ClientError> {
	client
		.propose(&quant_api::ProposeRequest {
			claims: vec![quant_api::ClaimProposal {
				statement: statement.to_owned(),
				kind: Some(quant_core::ClaimKind::Routine),
				topic: topic.map(str::to_owned),
				subject: None,
				classes: Vec::new(),
				confidence: Some(0.7),
				evidence,
			}],
		})
		.await
}

#[tokio::test]
async fn summary_profiles_serve_only_the_owners_own_words() {
	let h = start().await;
	seed(&h.state);
	let owner = h.owner().await;
	let coffee = tell(
		&owner,
		"Prefers espresso after lunch",
		Some("coffee.timing"),
		&[],
	)
	.await;

	let writer = h
		.paired(
			"Scout",
			ClientKind::Agent,
			approve(&["memory.profile:read", "memory.profile:write", "screen:read"]),
		)
		.await;
	let seen = writer
		.records(&RecordsRequest {
			streams: vec!["screen".into()],
			..Default::default()
		})
		.await
		.unwrap();
	let evidence = vec![seen.records[0].id];
	let proposed = propose(&writer, "Writes best before noon", evidence.clone(), None)
		.await
		.unwrap();
	let held = proposed.claims[0];

	// The summary agent sees only what the owner stated: no evidence, no
	// proposer, and the held proposal is invisible.
	let summary = h
		.paired(
			"Assistant",
			ClientKind::Agent,
			approve(&["memory.profile:summary"]),
		)
		.await;
	let profile = summary.profile().await.unwrap();
	assert_eq!(profile.level, quant_access::Level::Summary);
	assert_eq!(profile.claims.len(), 1);
	assert_eq!(profile.claims[0].id, coffee.id);
	assert!(profile.claims[0].evidence.is_empty());

	// A reader sees the owner's claim too, but still not the held proposal.
	let reader = h
		.paired(
			"Reader",
			ClientKind::Agent,
			approve(&["memory.profile:read"]),
		)
		.await;
	assert_eq!(reader.profile().await.unwrap().claims.len(), 1);

	// The owner sees both, and approves the proposal.
	let all = owner
		.claims(&quant_api::ClaimListRequest::default())
		.await
		.unwrap();
	assert_eq!(all.len(), 2);
	assert!(
		all.iter()
			.any(|c| c.status == quant_core::ClaimStatus::Held)
	);
	let approved = owner.claim_approve(held).await.unwrap();
	assert_eq!(approved.status, quant_core::ClaimStatus::Active);
	assert_eq!(approved.authority, quant_core::Authority::Inferred);
	assert_eq!(
		approved.proposed_by,
		Some(writer.identity().client.unwrap())
	);

	// Now the reader sees it, with confidence; the evidence ids are dropped
	// for this grant, because it cannot read the screen records they point
	// at. The writer, which can, sees the ids. The summary agent still sees
	// only the owner's words.
	let full = reader.profile().await.unwrap();
	assert_eq!(full.level, quant_access::Level::Read);
	assert_eq!(full.claims.len(), 2);
	let inferred = full.claims.iter().find(|c| c.id == held).unwrap();
	assert_eq!(inferred.confidence, 0.7);
	assert!(inferred.evidence.is_empty());
	let writers_view = writer.profile().await.unwrap();
	let inferred = writers_view.claims.iter().find(|c| c.id == held).unwrap();
	assert_eq!(inferred.evidence, evidence);
	assert_eq!(summary.profile().await.unwrap().claims.len(), 1);

	// Both reads were audited with their shape.
	let audit = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	let actions: Vec<_> = audit.entries.iter().map(|e| e.action.as_str()).collect();
	assert!(actions.contains(&"claims.read"));
	assert!(actions.contains(&"claims.propose"));
	assert!(actions.contains(&"claims.tell"));
	assert!(actions.contains(&"claims.approve"));
}

#[tokio::test]
async fn proposals_with_unreadable_evidence_fail_as_not_found() {
	let h = start().await;
	let device = DeviceId::new();
	let record = Record::new("screen.frame".parse().unwrap(), device).with_text("hidden");
	h.state
		.vault
		.store
		.insert_records(std::slice::from_ref(&record))
		.unwrap();

	// A write-only agent may propose, but it cannot read the evidence it
	// cites; the whole request fails as not found, never saying which id.
	let writer = h
		.paired(
			"Blind",
			ClientKind::Agent,
			approve(&["memory.profile:write"]),
		)
		.await;
	let refused = propose(&writer, "A guess", vec![record.id], None)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("not_found"));

	// A missing record id fails the same way.
	let missing = propose(
		&writer,
		"Another guess",
		vec![quant_core::RecordId::new()],
		None,
	)
	.await
	.unwrap_err();
	assert_eq!(missing.code(), Some("not_found"));

	// Nothing was stored either way.
	assert!(
		h.owner()
			.await
			.claims(&quant_api::ClaimListRequest::default())
			.await
			.unwrap()
			.is_empty()
	);
}

#[tokio::test]
async fn held_proposals_are_invisible_until_the_owner_approves() {
	let h = start().await;
	seed(&h.state);
	let agent = h
		.paired(
			"Agent",
			ClientKind::Agent,
			approve(&["memory.profile:read", "memory.profile:write", "*:read"]),
		)
		.await;
	let seen = agent.records(&RecordsRequest::default()).await.unwrap();
	let proposed = propose(&agent, "Ships on Fridays", vec![seen.records[0].id], None)
		.await
		.unwrap();
	let held = proposed.claims[0];

	// No client but the owner can see a held proposal, whatever its level.
	let profile = agent.profile().await.unwrap();
	assert!(profile.claims.iter().all(|c| c.id != held));
	let search = agent
		.search(&RecordsRequest {
			search: Some("Ships on Fridays".into()),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(search.records, [] as [quant_api::ServedRecord; 0]);

	let owner = h.owner().await;
	let listed = owner
		.claims(&quant_api::ClaimListRequest {
			status: Some(quant_core::ClaimStatus::Held),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(listed.len(), 1);
	assert_eq!(listed[0].id, held);

	// Rejecting keeps it hidden.
	owner.claim_reject(held).await.unwrap();
	assert!(
		agent
			.profile()
			.await
			.unwrap()
			.claims
			.iter()
			.all(|c| c.id != held)
	);
	let listed = owner
		.claims(&quant_api::ClaimListRequest {
			status: Some(quant_core::ClaimStatus::Rejected),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(listed.len(), 1);

	// A second proposal, approved this time, appears.
	let proposed = propose(&agent, "Ships on Fridays", vec![seen.records[0].id], None)
		.await
		.unwrap();
	owner.claim_approve(proposed.claims[0]).await.unwrap();
	let profile = agent.profile().await.unwrap();
	assert_eq!(profile.claims.len(), 1);
	assert_eq!(profile.claims[0].id, proposed.claims[0]);
}

#[tokio::test]
async fn deny_classes_hide_health_records_and_health_claims() {
	let h = start().await;
	seed(&h.state);
	let owner = h.owner().await;
	tell(&owner, "Prefers espresso after lunch", None, &[]).await;
	tell(
		&owner,
		"Takes blood pressure medicine each morning",
		None,
		&[Class::Health],
	)
	.await;

	let mut request = approve(&["*:read", "memory.profile:read"]);
	request.deny_classes = Some(vec![Class::Health]);
	let agent = h.paired("Cautious", ClientKind::Agent, request).await;

	// Records: the health stream is out of scope everywhere.
	let records = agent.records(&RecordsRequest::default()).await.unwrap();
	assert!(
		records
			.records
			.iter()
			.all(|r| r.stream.as_str() != "health.heart_rate")
	);
	let withheld = agent
		.records(&RecordsRequest {
			streams: vec!["health.heart_rate".into(), "screen".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(withheld.withheld, vec!["health.heart_rate"]);
	assert!(!withheld.records.is_empty());

	// Derived summaries quote heart rate and sleep, so they are withheld too.
	h.state
		.record(
			Record::new("memory.daily".parse().unwrap(), DeviceId::new())
				.starting(Timestamp::now() - SignedDuration::from_hours(2))
				.with_text("Resting heart rate 52 bpm; slept 7 h 10 min."),
		)
		.unwrap();
	let daily = agent
		.records(&RecordsRequest {
			streams: vec!["memory.daily".into()],
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(daily.withheld, vec!["memory.daily"]);
	assert!(daily.records.is_empty());

	// Series and durations treat it exactly like an out-of-scope stream.
	assert_eq!(
		agent
			.series(&SeriesRequest {
				stream: "health.heart_rate".into(),
				from: None,
				to: None,
				bucket_seconds: None,
				raw: false,
				max_rows: None,
			})
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);
	assert_eq!(
		agent
			.durations(&DurationsRequest {
				stream: "health.sleep".into(),
				from: None,
				to: None,
				group_by: None,
				bucket_seconds: None,
			})
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);

	// The stream listing never mentions health streams.
	let streams = agent.streams().await.unwrap();
	assert!(
		streams
			.iter()
			.all(|s| s.spec.id.as_str() != "health.heart_rate")
	);

	// Claims: only the non-health claim is served.
	let profile = agent.profile().await.unwrap();
	assert_eq!(profile.claims.len(), 1);
	assert_eq!(profile.claims[0].statement, "Prefers espresso after lunch");
}

#[tokio::test]
async fn context_packets_respect_budget_and_scopes() {
	let h = start().await;
	let device = DeviceId::new();
	let now = Timestamp::now();
	h.state
		.vault
		.store
		.insert_records(&[
			Record::new("memory.daily".parse().unwrap(), device)
				.starting(now - SignedDuration::from_hours(20))
				.with_text("Yesterday: mostly writing, an espresso at 14:02."),
			Record::new("memory.episode".parse().unwrap(), device)
				.starting(now - SignedDuration::from_hours(2))
				.ending(now - SignedDuration::from_hours(1))
				.with_text("Hour 22: writing in Zed, calm and focused."),
			Record::new("screen.frame".parse().unwrap(), device)
				.starting(now - SignedDuration::from_mins(30))
				.with_text("Reviewing the espresso order for tomorrow."),
		])
		.unwrap();
	let owner = h.owner().await;
	for statement in [
		"Prefers espresso after lunch, and a second one is fine when the afternoon is long",
		"Values focus over meetings, and reschedules a meeting rather than break a writing block",
		"Runs on Tuesday mornings along the river path, rain or shine, for about an hour",
		"Wants to read more fiction than news this year, and keeps a reading list in the vault",
		"Keeps a paper notebook for ideas that matter, and types them into notes on Fridays",
		"Prefers quiet workspaces over open plan offices, and books rooms for long calls",
		"Tracks spending in a spreadsheet every Sunday evening before the week starts",
		"Learns languages best by listening to podcasts at 1.5x on the morning walk",
	] {
		tell(&owner, statement, None, &[]).await;
	}

	// A grant that can read the profile, memory, and records gets every
	// section within a generous budget.
	let full = h
		.paired(
			"Full",
			ClientKind::Agent,
			approve(&["memory.profile:read", "memory:read", "screen:read"]),
		)
		.await;
	let packet = full
		.context(&quant_api::ContextRequest {
			task: "espresso".into(),
			budget_tokens: Some(1500),
		})
		.await
		.unwrap();
	assert!(packet.profile.included && !packet.profile.withheld && !packet.profile.cut);
	// The claim matching the task's terms comes first.
	assert_eq!(
		packet.profile.items[0].statement,
		"Prefers espresso after lunch, and a second one is fine when the afternoon is long"
	);
	assert!(packet.memory.included && !packet.memory.withheld);
	assert!(packet.records.included && !packet.records.withheld);
	assert!(packet.used_tokens <= packet.budget_tokens);

	// A tight budget stops before later sections are complete and says so.
	let tight = full
		.context(&quant_api::ContextRequest {
			task: "espresso".into(),
			budget_tokens: Some(200),
		})
		.await
		.unwrap();
	assert!(tight.used_tokens <= tight.budget_tokens);
	assert!(tight.profile.cut || tight.memory.cut || tight.records.cut);

	// A grant that can read none of the sections gets only withheld flags.
	let blind = h
		.paired("Blind", ClientKind::Agent, approve(&["health:summary"]))
		.await;
	let packet = blind
		.context(&quant_api::ContextRequest {
			task: "anything".into(),
			budget_tokens: None,
		})
		.await
		.unwrap();
	assert!(packet.profile.withheld && !packet.profile.included);
	assert!(packet.memory.withheld && !packet.memory.included);
	assert!(packet.records.withheld && !packet.records.included);
}

#[tokio::test]
async fn tell_with_a_topic_supersedes_earlier_claims_on_it() {
	let h = start().await;
	seed(&h.state);
	let owner = h.owner().await;
	tell(
		&owner,
		"Drinks espresso after lunch",
		Some("coffee.timing"),
		&[],
	)
	.await;

	// An agent proposes the opposite; the owner approves it.
	let agent = h
		.paired(
			"Agent",
			ClientKind::Agent,
			approve(&["memory.profile:read", "memory.profile:write", "*:read"]),
		)
		.await;
	let seen = agent.records(&RecordsRequest::default()).await.unwrap();
	let proposed = propose(
		&agent,
		"Drinks decaf after lunch",
		vec![seen.records[0].id],
		Some("coffee.timing"),
	)
	.await
	.unwrap();
	let held = proposed.claims[0];
	owner.claim_approve(held).await.unwrap();

	// The owner states it again with the same topic: everything earlier on
	// it, the owner's own first statement included, is superseded.
	let correction = tell(
		&owner,
		"Espresso, but never after 16:00",
		Some("coffee.timing"),
		&[],
	)
	.await;
	let claims = owner
		.claims(&quant_api::ClaimListRequest {
			topic: Some("coffee.timing".into()),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(claims.len(), 3);
	let old = claims.iter().find(|c| c.id == held).unwrap();
	assert_eq!(old.status, quant_core::ClaimStatus::Superseded);
	assert_eq!(old.superseded_by, Some(correction.id));
	for claim in &claims {
		let expected = if claim.id == correction.id {
			quant_core::ClaimStatus::Active
		} else {
			quant_core::ClaimStatus::Superseded
		};
		assert_eq!(claim.status, expected, "{}", claim.statement);
	}

	// Serving skips superseded claims entirely.
	let profile = agent.profile().await.unwrap();
	assert!(profile.claims.iter().all(|c| c.id != held));
}

#[tokio::test]
async fn owner_devices_unlock_sessions_that_cannot_enrol_others() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	assert_eq!(device.label, "MacBook Touch ID");
	assert_eq!(device.fingerprint.len(), 16);
	assert!(device.revoked_at.is_none());
	assert!(device.last_used.is_none());

	// The device opens the same kind of session the passphrase does, marked
	// as opened via device, and it works for everything but enrolment.
	let session = unlock_device(&h, device.id, &key).await.unwrap();
	assert_eq!(
		session.status().await.unwrap().session,
		quant_api::SessionSource::Device
	);
	let listed = session.owner_devices().await.unwrap();
	assert_eq!(
		listed.iter().map(|d| d.id).collect::<Vec<_>>(),
		vec![device.id]
	);
	assert!(
		session
			.owner_device_add(&EnrolDeviceRequest {
				label: "Another".into(),
				public_key: device_public_key(&device_key()),
				policy: OwnerDevicePolicy::Biometry,
			})
			.await
			.unwrap_err()
			.to_string()
			.contains("passphrase"),
		"enrolment through a device session must ask for the passphrase"
	);

	// The passphrase's own session says how it was opened, and the device's
	// key use was noted.
	let passphrase = h.owner().await.status().await.unwrap();
	assert_eq!(passphrase.session, quant_api::SessionSource::Passphrase);
	let stored = h
		.owner()
		.await
		.owner_devices()
		.await
		.unwrap()
		.into_iter()
		.find(|d| d.id == device.id)
		.unwrap();
	assert!(stored.last_used.is_some());
	assert_eq!(stored.fingerprint, device.fingerprint);

	// Enrolment and the unlock were audited.
	let owner = h.owner().await;
	let audit = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	assert!(audit.entries.iter().any(|e| {
		e.action == "owner.device.add"
			&& e.target == device.id.to_string()
			&& e.detail["label"] == "MacBook Touch ID"
			&& e.detail["fingerprint"] == device.fingerprint.as_str()
	}));
	assert!(
		audit
			.entries
			.iter()
			.any(|e| e.action == "owner.device.unlock" && e.target == device.id.to_string())
	);
}

#[tokio::test]
async fn a_wrong_device_signature_fails_and_shares_the_passphrase_backoff() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	let wrong = device_key();
	let refused = unlock_device(&h, device.id, &wrong).await.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
	assert!(refused.to_string().contains("owner device unlock failed"));

	// The failure counter is shared with the passphrase, so alternating the
	// two gains nothing.
	let locked = OwnerSession::open(h.endpoint.clone(), PASS)
		.await
		.unwrap_err();
	assert_eq!(locked.code(), Some("locked"));

	// The refusal was audited, without saying which part failed.
	let tail = h.state.vault.store.audit_tail(5).unwrap();
	let denial = tail
		.iter()
		.find(|e| e.action == "owner.device.unlock")
		.expect("the failed unlock was audited");
	assert!(matches!(
		denial.outcome,
		quant_access::audit::Outcome::Denied { .. }
	));
	assert_eq!(denial.target, "");
	assert_eq!(denial.detail, serde_json::Value::Null);
}

#[tokio::test]
async fn a_replayed_challenge_fails() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	let client = Client::new(h.endpoint.clone(), Identity::generate("device"));
	let challenge = client.owner_challenge().await.unwrap();
	let hello = client.hello().await.unwrap();
	let message = quant_api::owner_unlock_message(&hello.vault_id, &challenge.challenge);
	// Raw r‖s works as well as DER.
	let signature = URL_SAFE_NO_PAD.encode(device_sign(&key, &message).to_bytes());
	let opened = OwnerSession::open_with_device_signature(
		h.endpoint.clone(),
		device.id,
		challenge.challenge.clone(),
		signature.clone(),
	)
	.await
	.unwrap();
	assert!(opened.status().await.is_ok());

	let replay = OwnerSession::open_with_device_signature(
		h.endpoint.clone(),
		device.id,
		challenge.challenge,
		signature,
	)
	.await
	.unwrap_err();
	assert_eq!(replay.code(), Some("unauthenticated"));
	assert!(replay.to_string().contains("owner device unlock failed"));
}

#[tokio::test]
async fn an_expired_challenge_fails() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	// A challenge minted 61 seconds ago is already past its 60 seconds.
	let past = Timestamp::now() - SignedDuration::from_secs(61);
	let (challenge, _) = h.state.owner().issue_challenge(past);
	let text = URL_SAFE_NO_PAD.encode(challenge);
	let message = quant_api::owner_unlock_message(&h.state.vault.manifest.vault_id, &text);
	let signature = URL_SAFE_NO_PAD.encode(device_sign(&key, &message).to_der().as_bytes());
	let refused =
		OwnerSession::open_with_device_signature(h.endpoint.clone(), device.id, text, signature)
			.await
			.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
	assert!(refused.to_string().contains("owner device unlock failed"));
}

#[tokio::test]
async fn a_challenge_answered_for_another_vault_fails() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	let client = Client::new(h.endpoint.clone(), Identity::generate("device"));
	let challenge = client.owner_challenge().await.unwrap();
	let message = quant_api::owner_unlock_message("some-other-vault", &challenge.challenge);
	let signature = URL_SAFE_NO_PAD.encode(device_sign(&key, &message).to_der().as_bytes());
	let refused = OwnerSession::open_with_device_signature(
		h.endpoint.clone(),
		device.id,
		challenge.challenge,
		signature,
	)
	.await
	.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
	assert!(refused.to_string().contains("owner device unlock failed"));
}

#[tokio::test]
async fn an_unknown_device_fails_exactly_like_a_bad_signature() {
	let h = start().await;
	let key = device_key();
	enrol_device(&h, &key, "MacBook Touch ID").await;
	// An unknown device id answers exactly like the wrong signature the
	// next test checks: same code, same message, and the backoff it trips.
	let refused = unlock_device(&h, OwnerDeviceId::new(), &key)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
	assert!(refused.to_string().contains("owner device unlock failed"));
	let locked = OwnerSession::open(h.endpoint.clone(), PASS)
		.await
		.unwrap_err();
	assert_eq!(locked.code(), Some("locked"));
}

#[tokio::test]
async fn a_revoked_device_fails_like_a_bad_signature() {
	let h = start().await;
	let key = device_key();
	let device = enrol_device(&h, &key, "MacBook Touch ID").await;
	let session = unlock_device(&h, device.id, &key).await.unwrap();
	session.lock().await.unwrap();

	let owner = h.owner().await;
	let revoked = owner.owner_device_revoke(device.id).await.unwrap();
	assert!(revoked.revoked_at.is_some());
	let refused = unlock_device(&h, device.id, &key).await.unwrap_err();
	assert_eq!(refused.code(), Some("unauthenticated"));
	assert!(refused.to_string().contains("owner device unlock failed"));

	let audit = owner
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	assert!(
		audit
			.entries
			.iter()
			.any(|e| e.action == "owner.device.revoke")
	);
	assert!(audit.entries.iter().any(|e| {
		e.action == "owner.device.unlock"
			&& matches!(e.outcome, quant_access::audit::Outcome::Denied { .. })
	}));
}

#[tokio::test]
async fn at_most_eight_owner_devices_are_active_at_once() {
	let h = start().await;
	let owner = h.owner().await;
	let mut ids = Vec::new();
	for index in 0..8 {
		let device = enrol_device(&h, &device_key(), &format!("Key {}", index + 1)).await;
		ids.push(device.id);
	}
	let ninth = owner
		.owner_device_add(&EnrolDeviceRequest {
			label: "Ninth".into(),
			public_key: device_public_key(&device_key()),
			policy: OwnerDevicePolicy::Biometry,
		})
		.await
		.unwrap_err();
	assert_eq!(ninth.code(), Some("invalid"));

	// Revoking one leaves room for another.
	owner.owner_device_revoke(ids[0]).await.unwrap();
	let added = owner
		.owner_device_add(&EnrolDeviceRequest {
			label: "Ninth".into(),
			public_key: device_public_key(&device_key()),
			policy: OwnerDevicePolicy::BiometryOrPasscode,
		})
		.await
		.unwrap();
	let listed = owner.owner_devices().await.unwrap();
	assert_eq!(listed.len(), 9);
	assert_eq!(listed.iter().filter(|d| d.revoked_at.is_none()).count(), 8);
	assert!(
		listed
			.iter()
			.any(|d| d.id == added.id && d.policy == OwnerDevicePolicy::BiometryOrPasscode)
	);
}

#[tokio::test]
async fn uploaded_ambient_audio_is_queued_once_and_transcripts_keep_class_permissions() {
	let h = start().await;
	let phone = h
		.paired("phone", ClientKind::Device, approve(&["audio:write"]))
		.await;
	let stream = "audio.ambient".parse().unwrap();
	let media = phone
		.ingest_media(&stream, "audio/mp4", b"AAC test fixture".to_vec())
		.await
		.unwrap();
	let now = Timestamp::now();
	let record = Record::new(stream, DeviceId::new())
		.starting(now - SignedDuration::from_mins(5))
		.ending(now)
		.with("transcript_status", "done")
		.with_text("untrusted supplied transcript")
		.with_media(media);
	let request = IngestRecords {
		records: vec![record.clone()],
		device_name: None,
		platform: Some("android".into()),
	};
	phone.ingest_records(&request).await.unwrap();
	let stored = h.state.vault.store.record(record.id).unwrap().unwrap();
	assert_eq!(stored.data["transcript_status"], "pending");
	assert!(stored.text.is_none());
	assert_eq!(
		h.state
			.vault
			.store
			.pending_ambient_audio(now)
			.unwrap()
			.unwrap()
			.id,
		record.id
	);
	h.state
		.vault
		.store
		.finish_ambient_audio(record.id, "an entire phone conversation", now)
		.unwrap();
	phone.ingest_records(&request).await.unwrap();
	assert!(
		h.state
			.vault
			.store
			.pending_ambient_audio(now)
			.unwrap()
			.is_none()
	);
	let mut invalid = request.clone();
	invalid.records[0].end = None;
	assert_eq!(
		phone.ingest_records(&invalid).await.unwrap_err().code(),
		Some("invalid")
	);
	invalid.records[0].end = Some(record.start + SignedDuration::from_hours(2));
	assert_eq!(
		phone.ingest_records(&invalid).await.unwrap_err().code(),
		Some("invalid")
	);

	let mut denied = approve(&["audio:read"]);
	denied.deny_classes = Some(vec![Class::Others]);
	let reader = h.paired("denied", ClientKind::Agent, denied).await;
	let result = reader
		.records(&RecordsRequest {
			streams: vec!["audio.transcript".into()],
			from: Some(now - SignedDuration::from_hours(1)),
			to: Some(now + SignedDuration::from_secs(1)),
			..RecordsRequest::default()
		})
		.await
		.unwrap();
	assert!(result.records.is_empty());
	assert_eq!(result.withheld, vec!["audio.transcript"]);
}
