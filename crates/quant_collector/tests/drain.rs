//! A phone spool draining into a live daemon.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_collector::Spool;
use quant_collector::Uploader;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::TimeRange;
use quant_crypto::KdfParams;
use quant_crypto::VaultKey;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Vault;
use quant_store::VaultPaths;

const PASS: &str = "correct horse battery staple";

#[tokio::test]
async fn a_spool_drains_into_the_vault_and_empties() {
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
	let endpoint = Endpoint::Unix(socket);

	let mut phone = Client::new(endpoint.clone(), Identity::generate("Pixel"));
	let pairing = phone.pair(ClientKind::Device, &[]).await.unwrap();
	OwnerSession::open(endpoint, PASS)
		.await
		.unwrap()
		.approve(&ApproveRequest {
			client: pairing.pairing_code,
			scopes: Some(vec!["health:write".into(), "audio:write".into()]),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();

	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	let device = DeviceId::new();
	let now = Timestamp::now();
	for minute in 0..10 {
		spool
			.series(SeriesBatch {
				stream: "health.heart_rate".parse().unwrap(),
				device,
				start: now - SignedDuration::from_mins(60 - minute),
				channels: vec!["bpm".into()],
				offsets_ms: (0..60).map(|s| s * 1000).collect(),
				values: vec![64.0; 60],
			})
			.unwrap();
	}
	let clip = spool
		.media(
			&"audio.transcript".parse().unwrap(),
			b"RIFF fake wav",
			"audio/wav",
		)
		.unwrap();
	spool
		.record(
			Record::new("audio.transcript".parse().unwrap(), device)
				.starting(now - SignedDuration::from_mins(5))
				.with_text("let's ship the quant demo friday")
				.with_media(clip.clone()),
		)
		.unwrap();
	spool
		.record(
			Record::new("health.sleep".parse().unwrap(), device)
				.starting(now - SignedDuration::from_hours(8))
				.ending(now - SignedDuration::from_hours(1))
				.with("stage", "light"),
		)
		.unwrap();
	assert_eq!(spool.pending().unwrap().batches, 10);

	let mut uploader = Uploader::new(&spool, &phone);
	uploader.series_batch = 3;
	uploader.record_batch = 1;
	let delivered = uploader.drain().await.unwrap();
	assert_eq!(
		(delivered.batches, delivered.records, delivered.media),
		(10, 2, 1)
	);
	assert!(!delivered.throttled);
	assert_eq!(
		spool.pending().unwrap(),
		quant_collector::Pending::default()
	);

	let range = TimeRange::new(now - SignedDuration::from_hours(9), now).unwrap();
	let rows: usize = state
		.vault
		.store
		.series(&"health.heart_rate".parse().unwrap(), range, 10_000)
		.unwrap()
		.iter()
		.map(SeriesBatch::len)
		.sum();
	assert_eq!(rows, 600);
	let transcripts = state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream("audio.transcript".parse().unwrap()),
			range,
		))
		.unwrap();
	assert_eq!(
		transcripts[0].text.as_deref(),
		Some("let's ship the quant demo friday")
	);
	assert_eq!(state.vault.get_media(&clip.hash).unwrap(), b"RIFF fake wav");

	// Continuous phone audio follows the same signed spool path and queues
	// recognition on the daemon without needing a read scope on the phone.
	let ambient = quant_collector::AmbientAudio {
		start: now - SignedDuration::from_mins(5),
		duration_ms: 300_000,
		active_ms: 1200,
		peak_dbfs: -18.0,
	};
	let chunk_id = spool
		.ambient_audio(device, ambient, b"AAC phone audio")
		.unwrap()
		.unwrap();
	let delivered = Uploader::new(&spool, &phone).drain().await.unwrap();
	assert_eq!((delivered.records, delivered.media), (1, 1));
	let queued = state
		.vault
		.store
		.pending_ambient_audio(now)
		.unwrap()
		.unwrap();
	assert_eq!(queued.id, chunk_id);
	assert_eq!(queued.start, ambient.start);
	state
		.vault
		.store
		.finish_ambient_audio(chunk_id, "phone transcript", now)
		.unwrap();
	// A recorder's recovery after upload must not enqueue recognition twice.
	spool
		.ambient_audio(device, ambient, b"AAC phone audio")
		.unwrap();
	Uploader::new(&spool, &phone).drain().await.unwrap();
	assert!(
		state
			.vault
			.store
			.pending_ambient_audio(now)
			.unwrap()
			.is_none()
	);

	// Something the grant does not cover stays in the spool.
	spool
		.record(Record::new("location.visit".parse().unwrap(), device).with("place", "home"))
		.unwrap();
	let refused = Uploader::new(&spool, &phone).drain().await.unwrap_err();
	assert!(refused.to_string().contains("forbidden"), "{refused}");
	assert_eq!(spool.pending().unwrap().records, 1);
}
