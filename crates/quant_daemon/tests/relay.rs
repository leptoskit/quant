//! The relay end to end: an in-process relay, a daemon on a temporary
//! vault with a listener on it, and a paired client. TLS runs from the
//! client to the daemon inside the spliced stream, so a relay that alters a
//! byte breaks it, a relay that lies cannot impersonate, and the relay's
//! limits are all that stand between one vault and the rest.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::ClientError;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_crypto::KdfParams;
use quant_daemon::State;
use quant_daemon::relay;
use quant_daemon::tls::TlsIdentity;
use quant_relay::Limits;
use quant_relay::RelayUrl;
use quant_relay::dial;
use quant_relay::key::RelayKey;
use quant_relay::proto;
use quant_relay::server::BUSY;
use quant_relay::server::IDLE;
use quant_relay::server::NO_DAEMON;
use quant_store::Vault;
use quant_store::VaultPaths;
use tokio_tungstenite::tungstenite::Message;

const PASS: &str = "correct horse battery staple";

/// A relay on a local port, with the daemon's backoff and the relay's
/// timeouts kept short so failures arrive quickly.
fn fast_limits() -> Limits {
	Limits {
		accept_timeout: Duration::from_millis(300),
		idle_timeout: Duration::from_millis(400),
		..Limits::default()
	}
}

struct Harness {
	_dir: tempfile::TempDir,
	state: Arc<State>,
	url: RelayUrl,
	fingerprint: String,
	rid: String,
	socket: std::path::PathBuf,
	_stop: tokio::sync::oneshot::Sender<()>,
	_socket_stop: tokio::sync::oneshot::Sender<()>,
}

/// A vault whose daemon listens on a relay at `url`.
async fn daemon_on(url: &RelayUrl) -> Harness {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("v"));
	let vault = Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap();
	let identity = TlsIdentity::load_or_create(&vault.store).unwrap();
	let state = Arc::new(State::new(vault).unwrap());
	state
		.vault
		.store
		.set_meta("relay.url", &url.to_string())
		.unwrap();
	let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
	let serving = Arc::clone(&state);
	tokio::spawn(async move {
		relay::listen(serving, async {
			let _ = stopped.await;
		})
		.await;
	});
	// The daemon's private socket, for pairing over a path the test relay
	// does not carry.
	let (socket_stop, socket_stopped) = tokio::sync::oneshot::channel::<()>();
	let socket = paths.socket();
	let serving = Arc::clone(&state);
	let socket_path = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &socket_path, async {
			let _ = socket_stopped.await;
		})
		.await
		.unwrap();
	});
	for _ in 0..250 {
		if socket.exists() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	Harness {
		_dir: dir,
		rid: state.relay_key.rid(),
		fingerprint: identity.fingerprint(),
		state,
		url: url.clone(),
		socket,
		_stop: stop,
		_socket_stop: socket_stop,
	}
}

impl Harness {
	/// A client endpoint through the relay, with the daemon's pinned
	/// certificate.
	fn endpoint(&self) -> Endpoint {
		Endpoint::Relay {
			url: self.url.to_string(),
			rid: self.rid.clone(),
			fingerprint: hex::decode(&self.fingerprint).unwrap().try_into().unwrap(),
			address: None,
		}
	}

	/// Wait until the daemon's listener has registered, so dials that expect
	/// a daemon do not race the relay handshake.
	async fn wait_listening(&self) {
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		loop {
			match Client::new(self.endpoint(), Identity::generate("probe"))
				.hello()
				.await
			{
				Ok(_) => return,
				Err(error) if std::time::Instant::now() < deadline => {
					tokio::time::sleep(Duration::from_millis(20)).await;
					let _ = error;
				}
				Err(error) => panic!("the daemon never registered on the relay: {error}"),
			}
		}
	}

	/// Pair and approve over the daemon's private socket, returning the
	/// client ready to talk through the relay.
	async fn paired(&self) -> Client {
		let unix = Endpoint::Unix(self.socket.clone());
		let mut client = Client::new(unix.clone(), Identity::generate("phone"));
		let pairing = client.pair(ClientKind::Device, &[]).await.unwrap();
		OwnerSession::open(unix, PASS)
			.await
			.unwrap()
			.approve(&ApproveRequest {
				client: pairing.pairing_code,
				..approve_body()
			})
			.await
			.unwrap();
		let identity = client.identity().clone();
		Client::new(self.endpoint(), identity)
	}
}

fn approve_body() -> ApproveRequest {
	ApproveRequest {
		client: String::new(),
		scopes: Some(vec!["apps:read".into()]),
		label: None,
		lookback_hours: None,
		expires_in_hours: None,
		limits: None,
		unredacted: false,
		deny_classes: None,
	}
}

#[tokio::test]
async fn a_signed_request_round_trips_through_the_relay() {
	let (address, _store) = quant_relay::server::spawn_memory(Limits::default())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	let client = harness.paired().await;

	let me = client.me().await.unwrap();
	let grant = me.grant.expect("approved while pairing");
	assert!(grant.scopes.to_strings().contains(&"apps:read".to_owned()));
	let hello = Client::new(harness.endpoint(), Identity::generate("link"))
		.hello()
		.await
		.unwrap();
	assert_eq!(hello.relay.as_deref(), Some(url.to_string().as_str()));
	assert_eq!(hello.relay_rid.as_deref(), Some(harness.rid.as_str()));
}

#[tokio::test]
async fn a_relay_that_flips_a_byte_breaks_tls() {
	let store = quant_relay::server::MemoryStore::open();
	let app = quant_relay::server::router(quant_relay::server::RelayOptions {
		store,
		limits: fast_limits(),
		frame_hook: Some(Arc::new(|frame: &mut [u8]| {
			frame[0] ^= 1;
		})),
	});
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move {
		axum::serve(listener, app).await.unwrap();
	});
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	let client = harness.paired().await;

	let error = client.me().await.expect_err("altered TLS cannot succeed");
	assert!(
		!matches!(error, ClientError::Api { .. }),
		"a tampering relay must fail the handshake, not reach the API: {error}"
	);
}

#[tokio::test]
async fn a_different_key_cannot_take_over_the_rid() {
	let (address, _store) = quant_relay::server::spawn_memory(Limits::default())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	let client = harness.paired().await;

	// A second daemon, with its own key, listens too. The relay derives the
	// rendezvous id from the key it verifies, so this registers a *different*
	// rid and leaves the first daemon's registration alone.
	let impostor = RelayKey::generate();
	let mut socket = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut socket, &url.authority, &impostor, None)
		.await
		.unwrap();
	assert_ne!(impostor.rid(), harness.rid);

	// An unverified listener claiming nothing at all changes nothing either.
	let mut bogus = dial::dial(&url.listen()).await.unwrap();
	let challenge: proto::Challenge = dial::recv_text(&mut bogus).await.unwrap().unwrap();
	let answer = proto::ListenAnswer {
		public_key: impostor.public().to_string(),
		signature: "not-a-signature".into(),
		invite: None,
	};
	dial::send_text(&mut bogus, &answer).await.unwrap();
	let closed = bogus.next().await;
	assert!(
		matches!(closed, Some(Ok(Message::Close(_))) | None),
		"an unverified listener must be dropped, got {closed:?}"
	);
	let _ = challenge;

	// The real daemon still answers.
	assert!(client.me().await.is_ok());
}

#[tokio::test]
async fn a_connect_with_no_listener_fails_cleanly() {
	let (address, _store) = quant_relay::server::spawn_memory(fast_limits())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let endpoint = Endpoint::Relay {
		url: url.to_string(),
		rid: RelayKey::generate().rid(),
		fingerprint: [1; 32],
		address: None,
	};
	let error = Client::new(endpoint, Identity::generate("phone"))
		.hello()
		.await
		.expect_err("nobody is listening");
	assert!(matches!(
		error,
		ClientError::Unreachable { .. } | ClientError::Http(_)
	));
}

#[tokio::test]
async fn the_accept_timeout_closes_the_client() {
	let (address, _store) = quant_relay::server::spawn_memory(fast_limits())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();

	// A daemon that registers but never accepts: it holds the control
	// socket and ignores open notices.
	let lazy = RelayKey::generate();
	let mut control = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut control, &url.authority, &lazy, None)
		.await
		.unwrap();
	let rid = lazy.rid();

	let mut client = dial::dial(&url.connect(&rid)).await.unwrap();
	let started = std::time::Instant::now();
	let closed = loop {
		match client.next().await {
			Some(Ok(Message::Close(Some(frame)))) => break frame,
			Some(Ok(_)) => {}
			other => panic!("expected a close, got {other:?}"),
		}
	};
	assert!(started.elapsed() < Duration::from_secs(5));
	assert!(
		closed.reason.as_str().contains(NO_DAEMON),
		"the close should reveal nothing beyond 'no daemon': {}",
		closed.reason
	);
}

#[tokio::test]
async fn per_rid_connection_limits_are_enforced() {
	let limits = Limits {
		connections: 2,
		idle_timeout: Duration::from_secs(30),
		..fast_limits()
	};
	let (address, _store) = quant_relay::server::spawn_memory(limits).await.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	harness.wait_listening().await;

	// Two spliced connections held open, then a third that must be refused.
	let mut held = Vec::new();
	for _ in 0..2 {
		let socket = dial::dial(&url.connect(&harness.rid)).await.unwrap();
		held.push(socket);
	}
	tokio::time::sleep(Duration::from_millis(200)).await;
	let mut third = dial::dial(&url.connect(&harness.rid)).await.unwrap();
	let reason = loop {
		match third.next().await {
			Some(Ok(Message::Close(Some(frame)))) => break frame.reason.as_str().to_owned(),
			Some(Ok(_)) => {}
			other => panic!("expected the third connection to close, got {other:?}"),
		}
	};
	assert!(reason.contains(BUSY), "expected a busy close, got {reason}");
	drop(held);
}

#[tokio::test]
async fn an_idle_spliced_connection_is_closed() {
	let (address, _store) = quant_relay::server::spawn_memory(fast_limits())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	harness.wait_listening().await;

	let mut idle = dial::dial(&url.connect(&harness.rid)).await.unwrap();
	let reason = loop {
		match idle.next().await {
			Some(Ok(Message::Close(Some(frame)))) => break frame.reason.as_str().to_owned(),
			Some(Ok(_)) => {}
			other => panic!("expected an idle close, got {other:?}"),
		}
	};
	assert!(
		reason.contains(IDLE),
		"expected an idle close, got {reason}"
	);
}

#[tokio::test]
async fn the_owner_can_set_and_clear_the_relay() {
	let (address, _store) = quant_relay::server::spawn_memory(Limits::default())
		.await
		.unwrap();
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	let harness = daemon_on(&url).await;
	harness.wait_listening().await;
	let owner = OwnerSession::open(harness.endpoint(), PASS).await.unwrap();

	let state = owner
		.set_relay(&quant_api::RelaySetRequest {
			url: Some("wss://relay.example.com".into()),
			invite: None,
		})
		.await
		.unwrap();
	assert_eq!(state.url.as_deref(), Some("wss://relay.example.com"));
	assert_eq!(state.rid, harness.rid);
	let config = relay::config(&harness.state.vault.store).unwrap().unwrap();
	assert_eq!(config.url.authority, "relay.example.com");

	let state = owner
		.set_relay(&quant_api::RelaySetRequest {
			url: None,
			invite: None,
		})
		.await
		.unwrap();
	assert_eq!(state.url, None);
	assert!(relay::config(&harness.state.vault.store).unwrap().is_none());
}
