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
use quant_relay::server::DAILY;
use quant_relay::server::HANDSHAKE;
use quant_relay::server::NO_DAEMON;
use quant_relay::server::RelayStore as _;
use relay_db::RelayDb;
use tokio_tungstenite::tungstenite::Message;

/// A relay app serving on a local port, over a database it keeps open.
struct Served {
	/// Where it listens.
	url: RelayUrl,
	/// The database it serves from.
	store: RelayDb,
	/// The task serving it, which [`Served::stop`] aborts.
	server: tokio::task::JoinHandle<()>,
}

impl Served {
	/// Stop the relay and drop its database handle, so a relay opened later
	/// over the same file sees only what was committed.
	async fn stop(self) {
		self.server.abort();
		let _ = self.server.await;
		drop(self.store);
	}
}

/// A relay app on a local port, over a fresh in-memory database.
async fn serve(limits: Limits) -> (RelayUrl, RelayDb) {
	let served = serve_at("sqlite::memory:", limits).await;
	(served.url, served.store)
}

/// A relay app on a local port, over `database` (a leptoskit database URL).
async fn serve_at(database: &str, limits: Limits) -> Served {
	let db = Db::connect(database).await.unwrap();
	for name in db.migrate(relay_db::MIGRATIONS).await.unwrap() {
		println!("applied migration {name}");
	}
	let store = RelayDb::new(db);
	let app = relay::router(store.clone(), limits, &Readiness::new());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		axum::serve(listener, app).await.unwrap();
	});
	let url = RelayUrl::parse(&format!("ws://{address}")).unwrap();
	Served { url, store, server }
}

/// A scratch directory of its own for one test, under the system temporary
/// directory. Dropping it removes the database and the write-ahead log and
/// shared-memory files `SQLite` keeps beside it, including when an assertion
/// fails and the test unwinds.
fn database_dir() -> tempfile::TempDir {
	tempfile::Builder::new()
		.prefix("quant-relay-")
		.tempdir()
		.expect("a scratch directory for the relay database")
}

/// The `sqlite://` URL of `name` inside `dir`.
fn database_url(dir: &tempfile::TempDir, name: &str) -> String {
	format!("sqlite://{}", dir.path().join(name).display())
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

/// Register `key` on `url` and echo every accepted client frame until the
/// returned task is aborted.
async fn start_daemon(url: &RelayUrl, key: &Arc<RelayKey>) -> tokio::task::JoinHandle<()> {
	let mut control = dial::dial(&url.listen()).await.unwrap();
	dial::listen_handshake(&mut control, &url.authority, key, None)
		.await
		.unwrap();
	let daemon_url = url.clone();
	let daemon_key = Arc::clone(key);
	tokio::spawn(async move {
		echo_from(&daemon_url, &daemon_key, control).await;
	})
}

/// Stop a daemon task and the control socket it holds.
async fn stop_daemon(daemon: tokio::task::JoinHandle<()>) {
	daemon.abort();
	let _ = daemon.await;
}

/// Send `payload` through `client` and return the daemon's echo.
async fn echo_round_trip(client: &mut dial::Dial, payload: &[u8]) -> Vec<u8> {
	client
		.send(Message::Binary(payload.to_vec().into()))
		.await
		.unwrap();
	tokio::time::timeout(Duration::from_secs(5), async {
		loop {
			match client.next().await {
				Some(Ok(Message::Binary(bytes))) => break bytes.to_vec(),
				Some(Ok(_)) => {}
				other => panic!("expected an echo, got {other:?}"),
			}
		}
	})
	.await
	.unwrap()
}

/// Dial a client and return the reason the relay closed it, retrying while
/// the daemon's registration is still settling.
async fn refused_connect(url: &RelayUrl, rid: &str) -> String {
	let deadline = std::time::Instant::now() + Duration::from_secs(5);
	loop {
		let mut socket = dial::dial(&url.connect(rid)).await.unwrap();
		match socket.next().await {
			Some(Ok(Message::Close(Some(frame))))
				if frame.reason.as_str() == NO_DAEMON && std::time::Instant::now() < deadline =>
			{
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
			Some(Ok(Message::Close(Some(frame)))) => return frame.reason.as_str().to_owned(),
			other => panic!("expected the relay to close the connect, got {other:?}"),
		}
	}
}

/// Wait for the relay to commit `rid`'s registration, so stopping it cannot
/// race the write.
async fn wait_for_registration(store: &RelayDb, rid: &str) {
	for _ in 0..50 {
		let registered = relay_db::Registration::query()
			.filter(|row| row.rid.eq(rid.to_owned()))
			.first(store.db())
			.await
			.unwrap()
			.is_some();
		if registered {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("the relay did not commit the registration for {rid}");
}

/// Wait for the stored total for `rid` on `today` to reach `at_least`, and
/// return it.
async fn wait_for_usage(store: &RelayDb, rid: &str, today: &str, at_least: u64) -> u64 {
	let mut counted = 0;
	for _ in 0..50 {
		counted = store.usage(rid, today).await.unwrap();
		if counted >= at_least {
			break;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(
		counted >= at_least,
		"expected at least {at_least} bytes, saw {counted}"
	);
	counted
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

#[tokio::test]
async fn a_registration_and_its_spent_invite_survive_a_restart() {
	let dir = database_dir();
	let url = database_url(&dir, "registration.sqlite3");
	let code = "restart-invite";
	let key = Arc::new(RelayKey::generate());
	let rid = key.rid();

	let before = serve_at(&url, limits()).await;
	relay_db::NewInvite {
		code: code.into(),
		used_by: None,
		used_at: None,
	}
	.insert(before.store.db())
	.await
	.unwrap();
	assert_eq!(try_listen(&before.url, &key, Some(code)).await, None);
	wait_for_registration(&before.store, &rid).await;
	before.stop().await;

	// A new relay over the same file knows the daemon and the invite it spent,
	// so the daemon re-registers with no invite and traffic reaches it.
	let after = serve_at(&url, limits()).await;
	let daemon = start_daemon(&after.url, &key).await;
	let mut client = dial_client(&after.url, &rid).await;
	let payload = vec![7_u8; 1024];
	assert_eq!(
		echo_round_trip(&mut client, &payload).await,
		payload,
		"a registered daemon re-listens without an invite and carries traffic"
	);
	client.close(None).await.unwrap();
	stop_daemon(daemon).await;

	let stranger = RelayKey::generate();
	assert_eq!(
		try_listen(&after.url, &stranger, Some(code))
			.await
			.as_deref(),
		Some(HANDSHAKE),
		"a spent invite stays spent across a restart"
	);
	let newcomer = RelayKey::generate();
	assert_eq!(
		try_listen(&after.url, &newcomer, None).await.as_deref(),
		Some(HANDSHAKE),
		"invite gating stays on across a restart"
	);
	after.stop().await;
}

#[tokio::test]
async fn usage_counters_survive_a_restart() {
	let dir = database_dir();
	let url = database_url(&dir, "usage.sqlite3");
	let key = Arc::new(RelayKey::generate());
	let rid = key.rid();
	let today = day(jiff::Timestamp::now());
	let payload = vec![7_u8; 1024];

	let before = serve_at(&url, limits()).await;
	let daemon = start_daemon(&before.url, &key).await;
	let mut client = dial_client(&before.url, &rid).await;
	assert_eq!(echo_round_trip(&mut client, &payload).await, payload);
	client.close(None).await.unwrap();
	let counted = wait_for_usage(&before.store, &rid, &today, 2048).await;
	stop_daemon(daemon).await;
	before.stop().await;

	// The total is still there, rather than starting from zero.
	let after = serve_at(&url, limits()).await;
	assert_eq!(
		after.store.usage(&rid, &today).await.unwrap(),
		counted,
		"the stored total is unchanged by a restart"
	);

	// Further traffic adds to the stored total.
	let daemon = start_daemon(&after.url, &key).await;
	let mut client = dial_client(&after.url, &rid).await;
	assert_eq!(echo_round_trip(&mut client, &payload).await, payload);
	client.close(None).await.unwrap();
	wait_for_usage(&after.store, &rid, &today, counted + 2048).await;
	stop_daemon(daemon).await;
	after.stop().await;
}

#[tokio::test]
async fn the_daily_byte_limit_holds_across_a_restart() {
	let limits = Limits {
		bytes_per_day: 1024,
		..limits()
	};
	let dir = database_dir();
	let url = database_url(&dir, "daily-limit.sqlite3");
	let key = Arc::new(RelayKey::generate());
	let rid = key.rid();
	let today = day(jiff::Timestamp::now());

	let before = serve_at(&url, limits).await;
	let daemon = start_daemon(&before.url, &key).await;
	let mut client = dial_client(&before.url, &rid).await;
	// One frame over the allowance closes the connection with the daily
	// reason, and its bytes are counted.
	client
		.send(Message::Binary(vec![9_u8; 2048].into()))
		.await
		.unwrap();
	let reason = loop {
		match client.next().await {
			Some(Ok(Message::Close(Some(frame)))) => break frame.reason.as_str().to_owned(),
			Some(Ok(_)) => {}
			other => panic!("expected a daily-limit close, got {other:?}"),
		}
	};
	assert!(
		reason.contains(DAILY),
		"expected the daily limit, got {reason}"
	);
	let counted = wait_for_usage(&before.store, &rid, &today, 1024).await;
	stop_daemon(daemon).await;
	before.stop().await;

	// A new relay over the same file still refuses the daemon's clients.
	let after = serve_at(&url, limits).await;
	let daemon = start_daemon(&after.url, &key).await;
	let reason = refused_connect(&after.url, &rid).await;
	assert!(
		reason.contains(DAILY),
		"the daily limit should survive a restart, got {reason}"
	);
	assert_eq!(
		after.store.usage(&rid, &today).await.unwrap(),
		counted,
		"the usage behind the limit is unchanged by a restart"
	);
	stop_daemon(daemon).await;
	after.stop().await;
}
