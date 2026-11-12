//! The relay app over the real database: invite gating, limits, and byte
//! accounting, end to end over sockets.

use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use leptoskit_axum::Readiness;
use leptoskit_db::Db;
use quant_relay::Limits;
use quant_relay::RelayUrl;
use quant_relay::dial;
use quant_relay::key::RelayKey;
use quant_relay::limits::day;
use quant_relay::proto;
use quant_relay::server::BUSY;
use quant_relay::server::HANDSHAKE;
use quant_relay::server::RelayStore as _;
use relay_db::RelayDb;
use tokio_tungstenite::tungstenite::Message;

/// A relay app on a local port, over a fresh in-memory database.
async fn serve(limits: Limits) -> (RelayUrl, RelayDb) {
	let db = Db::connect("sqlite::memory:").await.unwrap();
	for name in db.migrate(relay_db::MIGRATIONS).await.unwrap() {
		println!("applied migration {name}");
	}
	let store = RelayDb::new(db);
	let app = relay::router(store.clone(), limits, &Readiness::new());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move {
		axum::serve(listener, app).await.unwrap();
	});
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	(url, store)
}

fn limits() -> Limits {
	Limits {
		accept_timeout: Duration::from_secs(2),
		idle_timeout: Duration::from_secs(30),
		..Limits::default()
	}
}

/// Listen once as `key`, offering `invite`. Returns the close reason when
/// the relay refused the registration, or `None` when it accepted and the
/// socket stayed open.
async fn try_listen(url: &RelayUrl, key: &RelayKey, invite: Option<&str>) -> Option<String> {
	let mut socket = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut socket, &url.authority, key, invite)
		.await
		.unwrap();
	tokio::time::timeout(Duration::from_millis(250), async {
		loop {
			match socket.next().await {
				Some(Ok(Message::Close(Some(frame)))) => {
					return Some(frame.reason.as_str().to_owned());
				}
				Some(Ok(_)) => {}
				_ => return None,
			}
		}
	})
	.await
	.ok()
	.flatten()
}

/// Dial a client connection, retrying while the relay is still finishing
/// the daemon's registration.
async fn dial_client(url: &RelayUrl, rid: &str) -> dial::Dial {
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		let mut socket = dial::dial(&url.connect(rid)).await.unwrap();
		let settled = tokio::time::timeout(Duration::from_millis(100), socket.next()).await;
		match settled {
			// Still open after a beat: the daemon is reachable.
			Err(_) => return socket,
			Ok(Some(Ok(Message::Close(_))) | None) if std::time::Instant::now() < deadline => {
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
			other => panic!("the relay closed a waiting client: {other:?}"),
		}
	}
}

/// The accept-and-echo loop once the control socket is registered.
async fn echo_from(url: &RelayUrl, key: &Arc<RelayKey>, mut control: dial::Dial) {
	while let Some(notice) = dial::recv_text::<quant_relay::proto::OpenNotice>(&mut control)
		.await
		.unwrap()
	{
		let url = url.clone();
		let key = Arc::clone(key);
		let conn = notice.open;
		tokio::spawn(async move {
			let mut socket = dial::dial(&url.accept(&conn)).await.unwrap();
			dial::accept_handshake(&mut socket, &url.authority, &conn, &key)
				.await
				.unwrap();
			let (mut sink, mut stream) = socket.split();
			while let Some(Ok(Message::Binary(bytes))) = stream.next().await {
				if sink.send(Message::Binary(bytes)).await.is_err() {
					break;
				}
			}
		});
	}
}

#[tokio::test]
async fn invites_gate_registration_once() {
	let (url, store) = serve(limits()).await;

	// No invites: any daemon that proves its key registers.
	let open_relay = RelayKey::generate();
	assert_eq!(try_listen(&url, &open_relay, None).await, None);

	// One invite turns gating on for the whole relay.
	let code = "welcome-to-relay";
	relay_db::NewInvite {
		code: code.into(),
		used_by: None,
		used_at: None,
	}
	.insert(store.db())
	.await
	.unwrap();

	let first = RelayKey::generate();
	assert_eq!(
		try_listen(&url, &first, Some("wrong-code"))
			.await
			.as_deref(),
		Some(HANDSHAKE),
		"a wrong invite is refused"
	);
	assert_eq!(
		try_listen(&url, &first, None).await.as_deref(),
		Some(HANDSHAKE),
		"a missing invite is refused"
	);
	assert_eq!(try_listen(&url, &first, Some(code)).await, None);

	// The invite is spent, and re-listening needs no new one.
	let second = RelayKey::generate();
	assert_eq!(
		try_listen(&url, &second, Some(code)).await.as_deref(),
		Some(HANDSHAKE),
		"an invite works once"
	);
	assert_eq!(try_listen(&url, &first, None).await, None);

	let spent = relay_db::Invite::query()
		.filter(|invite| invite.code.eq(code))
		.first(store.db())
		.await
		.unwrap()
		.unwrap();
	assert_eq!(spent.used_by.as_deref(), Some(first.rid().as_str()));
}

#[tokio::test]
async fn a_spent_invite_cannot_take_over_a_registered_rid() {
	let (url, store) = serve(limits()).await;
	let code = "single-use";
	relay_db::NewInvite {
		code: code.into(),
		used_by: None,
		used_at: None,
	}
	.insert(store.db())
	.await
	.unwrap();

	let key = RelayKey::generate();
	assert_eq!(try_listen(&url, &key, Some(code)).await, None);

	// A different key claiming the same rendezvous id is refused by the
	// store, even with the (spent) invite.
	let mut socket = dial::dial(&url.listen()).await.unwrap();
	let challenge: proto::Challenge = dial::recv_text(&mut socket).await.unwrap().unwrap();
	let bytes = proto::challenge_bytes(&challenge.challenge).unwrap();
	let impostor = RelayKey::generate();
	let mut answer = impostor.sign_listen(&url.authority, &bytes);
	answer.public_key = key.public().to_string();
	answer.invite = Some(code.into());
	dial::send_text(&mut socket, &answer).await.unwrap();
	let closed = tokio::time::timeout(Duration::from_millis(250), async {
		loop {
			match socket.next().await {
				Some(Ok(Message::Close(Some(frame)))) => {
					return Some(frame.reason.as_str().to_owned());
				}
				Some(Ok(_)) => {}
				_ => return None,
			}
		}
	})
	.await
	.ok()
	.flatten();
	assert_eq!(closed.as_deref(), Some(HANDSHAKE));
}

#[tokio::test]
async fn bytes_flowing_through_the_relay_are_counted() {
	let (url, store) = serve(limits()).await;
	let key = Arc::new(RelayKey::generate());
	let rid = key.rid();
	let mut control = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut control, &url.authority, &key, None)
		.await
		.unwrap();
	let daemon_url = url.clone();
	let daemon_key = Arc::clone(&key);
	tokio::spawn(async move {
		echo_from(&daemon_url, &daemon_key, control).await;
	});

	let mut client = dial_client(&url, &rid).await;
	let payload = vec![7_u8; 1024];
	client
		.send(Message::Binary(payload.clone().into()))
		.await
		.unwrap();
	let echoed = tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			match client.next().await {
				Some(Ok(Message::Binary(bytes))) => break bytes,
				Some(Ok(_)) => {}
				other => panic!("expected an echo, got {other:?}"),
			}
		}
	})
	.await
	.unwrap();
	assert_eq!(echoed.to_vec(), payload);
	client.close(None).await.unwrap();

	// The day's counter holds both directions once the connection ends.
	let day = day(jiff::Timestamp::now());
	let mut counted = 0;
	for _ in 0..50 {
		counted = store.usage(&rid, &day).await.unwrap();
		if counted >= 2048 {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(
		counted >= 2048,
		"1024 bytes each way should be counted, saw {counted}"
	);
}

#[tokio::test]
async fn the_connection_limit_holds_per_rid() {
	let limits = Limits {
		connections: 1,
		..limits()
	};
	let (url, _store) = serve(limits).await;
	let key = Arc::new(RelayKey::generate());
	let rid = key.rid();
	let mut control = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut control, &url.authority, &key, None)
		.await
		.unwrap();
	let daemon_url = url.clone();
	let daemon_key = Arc::clone(&key);
	tokio::spawn(async move {
		echo_from(&daemon_url, &daemon_key, control).await;
	});

	let mut first = dial_client(&url, &rid).await;
	let payload = vec![1_u8; 16];
	first.send(Message::Binary(payload.into())).await.unwrap();
	match first.next().await {
		Some(Ok(Message::Binary(_))) => {}
		other => panic!("expected an echo, got {other:?}"),
	}

	let mut second = dial::dial(&url.connect(&rid)).await.unwrap();
	let reason = loop {
		match second.next().await {
			Some(Ok(Message::Close(Some(frame)))) => break frame.reason.as_str().to_owned(),
			Some(Ok(_)) => {}
			other => panic!("expected a busy close, got {other:?}"),
		}
	};
	assert!(reason.contains(BUSY), "expected a busy close, got {reason}");
}
