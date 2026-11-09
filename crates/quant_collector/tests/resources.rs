//! Bounded catch-up work, preserving captures across budget and network stops.

use std::sync::Arc;
use std::time::Duration;

use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_collector::Spool;
use quant_collector::UploadBudget;
use quant_collector::Uploader;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::Sink;
use quant_crypto::KdfParams;
use quant_crypto::VaultKey;
use quant_daemon::State;
use quant_store::Vault;
use quant_store::VaultPaths;
use tokio::io::AsyncReadExt;

async fn phone() -> (tempfile::TempDir, Arc<State>, Client) {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("vault"));
	let state = Arc::new(
		State::new(
			Vault::create(
				paths.clone(),
				"correct horse battery staple",
				KdfParams::INSECURE_FAST,
			)
			.unwrap(),
		)
		.unwrap(),
	);
	let serving = Arc::clone(&state);
	let socket = paths.socket();
	let serving_socket = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &serving_socket, std::future::pending())
			.await
			.unwrap();
	});
	while !socket.exists() {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	let endpoint = Endpoint::Unix(socket);
	let mut phone = Client::new(endpoint.clone(), Identity::generate("phone"));
	let pairing = phone.pair(ClientKind::Device, &[]).await.unwrap();
	OwnerSession::open(endpoint, "correct horse battery staple")
		.await
		.unwrap()
		.approve(&ApproveRequest {
			client: pairing.pairing_code,
			scopes: Some(vec!["audio:write".into(), "apps:write".into()]),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();
	(dir, state, phone)
}

#[tokio::test]
async fn catch_up_yields_and_resumes_without_losing_records() {
	let (dir, state, phone) = phone().await;
	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	for _ in 0..7 {
		spool
			.record(Record::new("apps.focus".parse().unwrap(), DeviceId::new()))
			.unwrap();
	}
	let mut uploader = Uploader::new(&spool, &phone);
	uploader.record_batch = usize::MAX;
	// Pausing must do no network or database work and leave the spool intact.
	let paused = uploader
		.drain_with_budget(UploadBudget {
			max_duration: Duration::ZERO,
			..Default::default()
		})
		.await
		.unwrap();
	assert!(paused.yielded);
	assert_eq!(paused.records, 0);
	let budget = UploadBudget {
		max_records: 3,
		max_batches: 0,
		..Default::default()
	};
	let first = uploader.drain_with_budget(budget).await.unwrap();
	assert!(first.yielded);
	assert_eq!(first.records, 3);
	assert_eq!(spool.pending().unwrap().records, 4);
	assert_eq!(uploader.drain_with_budget(budget).await.unwrap().records, 3);
	assert_eq!(uploader.drain_with_budget(budget).await.unwrap().records, 1);
	assert_eq!(spool.pending().unwrap().records, 0);
	assert_eq!(state.vault.store.backlog_counts().unwrap().0, 7);
}

#[tokio::test]
async fn media_budget_commits_progress_instead_of_replaying_the_first_files() {
	let (dir, state, phone) = phone().await;
	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	for content in [b"one".as_slice(), b"two", b"longer-than-the-budget"] {
		let stream = "audio.clip".parse().unwrap();
		let mut media = spool.media(&stream, content, "audio/mp4").unwrap();
		if content == b"two" {
			media.size = 0;
		} // Old metadata must not bypass the byte budget.
		spool
			.record(Record::new(stream, DeviceId::new()).with_media(media))
			.unwrap();
	}
	let mut uploader = Uploader::new(&spool, &phone);
	uploader.record_batch = usize::MAX;
	let budget = UploadBudget {
		max_media_bytes: 3,
		..Default::default()
	};
	for remaining in [2, 1, 0] {
		let report = uploader.drain_with_budget(budget).await.unwrap();
		assert_eq!(report.records, 1);
		assert_eq!(spool.pending().unwrap().records, remaining);
	}
	assert_eq!(state.vault.store.backlog_counts().unwrap().0, 3);
}

#[tokio::test]
async fn deadline_cancels_the_http_driver_and_keeps_unacknowledged_data() {
	let (dir, _, phone) = phone().await;
	let spool = Spool::open(&dir.path().join("spool"), &VaultKey::generate()).unwrap();
	spool
		.record(Record::new("apps.focus".parse().unwrap(), DeviceId::new()))
		.unwrap();
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let (seen, received) = tokio::sync::oneshot::channel();
	let server = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		let mut request = [0u8; 4096];
		assert!(socket.read(&mut request).await.unwrap() > 0);
		seen.send(()).unwrap();
		// Deliberately never acknowledge; cancellation must close the socket.
		socket.read_to_end(&mut Vec::new()).await
	});
	let stalled = Client::new(Endpoint::Tcp(address.to_string()), phone.identity().clone());
	let report = Uploader::new(&spool, &stalled)
		.drain_with_budget(UploadBudget {
			max_duration: Duration::from_millis(500),
			..Default::default()
		})
		.await
		.unwrap();
	assert!(report.yielded);
	assert_eq!(report.records, 0);
	tokio::time::timeout(Duration::from_secs(2), received)
		.await
		.unwrap()
		.unwrap();
	let closed = tokio::time::timeout(Duration::from_secs(2), server)
		.await
		.expect("HTTP driver must release the socket")
		.unwrap();
	if let Err(error) = closed {
		assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
	}
	assert_eq!(spool.pending().unwrap().records, 1);
	assert_eq!(
		Uploader::new(&spool, &phone).drain().await.unwrap().records,
		1
	);
}
