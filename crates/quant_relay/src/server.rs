//! The relay server: three WebSocket routes over any [`RelayStore`].
//!
//! This is the relay that `apps/relay` serves, with storage left open: a
//! [`RelayStore`] holds registrations, invites, and usage counts, and
//! [`MemoryStore`] is an in-memory one for tests. Nothing here looks at a
//! client's bytes, logs an address, or stores one; what the relay knows is
//! the rendezvous id, the public key, when it was last seen, and how many
//! bytes flowed.
//!
//! The routes:
//!
//! - `GET /v1/listen` — the daemon's control socket. The relay challenges; the
//!   daemon answers with its rendezvous key and a signature. A verified answer
//!   registers the socket for the derived rendezvous id, replacing an older
//!   verified listener; an unverified answer registers nothing.
//! - `GET /v1/connect/{rid}` — a client. The relay makes a connection id, asks
//!   the daemon to accept it, and waits up to the accept timeout.
//! - `GET /v1/accept/{conn}` — the daemon's data socket, authenticated the same
//!   way, with the connection id folded into the signed message.
//!
//! Once both are there the relay copies binary frames each way until either
//! side closes, the idle timeout passes, or the day's byte budget runs out.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use axum::Router;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::CloseFrame;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::close_code;
use axum::http::HeaderMap;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::SinkExt;
use futures_util::StreamExt;
use jiff::Timestamp;
use quant_access::auth::PublicKey;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::Limits;
use crate::key;
use crate::limits::day;
use crate::proto;
pub use crate::store::RelayStore;
pub use crate::store::StoreError;

/// The connect path's one answer, so a probing client learns nothing about
/// which rendezvous ids exist or which daemons answered.
pub const NO_DAEMON: &str = "daemon unreachable";
/// Over a per-id connection or pending-connect limit.
pub const BUSY: &str = "too many connections";
/// The day's byte budget for this rendezvous id is spent.
pub const DAILY: &str = "daily byte limit reached";
/// A spliced connection carried no traffic for the idle timeout.
pub const IDLE: &str = "idle";
/// The listen or accept handshake did not verify.
pub const HANDSHAKE: &str = "relay handshake failed";

/// An in-memory store, for tests and for embedding the relay in a process.
#[derive(Clone)]
pub struct MemoryStore {
	inner: Arc<Mutex<MemoryInner>>,
	invite_only: bool,
}

#[derive(Default)]
struct MemoryInner {
	registrations: HashMap<String, (String, Option<String>)>,
	invites: HashMap<String, bool>,
	usage: HashMap<(String, String), u64>,
	last_seen: HashMap<String, Timestamp>,
}

impl MemoryStore {
	/// A store that registers any verified daemon.
	pub fn open() -> Self {
		Self {
			inner: Arc::new(Mutex::new(MemoryInner::default())),
			invite_only: false,
		}
	}

	/// A store that registers a `rid` only after it presents one of the
	/// invites added with [`MemoryStore::add_invite`].
	pub fn invite_only() -> Self {
		Self {
			inner: Arc::new(Mutex::new(MemoryInner::default())),
			invite_only: true,
		}
	}

	/// Accept this invite code, once.
	pub async fn add_invite(&self, code: &str) {
		self.inner
			.lock()
			.await
			.invites
			.insert(code.to_owned(), false);
	}
}

impl RelayStore for MemoryStore {
	async fn register(
		&self,
		rid: &str,
		public_key: &str,
		invite: Option<&str>,
	) -> Result<(), StoreError> {
		let mut inner = self.inner.lock().await;
		if let Some((existing, _)) = inner.registrations.get(rid) {
			return if existing == public_key {
				Ok(())
			} else {
				Err(StoreError::KeyMismatch)
			};
		}
		let invite = if self.invite_only {
			let code = invite.ok_or(StoreError::InviteRequired)?;
			let used = inner.invites.get_mut(code).ok_or(StoreError::InviteUsed)?;
			if *used {
				return Err(StoreError::InviteUsed);
			}
			*used = true;
			Some(code.to_owned())
		} else {
			invite.map(ToOwned::to_owned)
		};
		inner
			.registrations
			.insert(rid.to_owned(), (public_key.to_owned(), invite));
		Ok(())
	}

	async fn touch(&self, rid: &str) {
		self.inner
			.lock()
			.await
			.last_seen
			.insert(rid.to_owned(), Timestamp::now());
	}

	async fn add_usage(&self, rid: &str, day: &str, bytes: u64) {
		*self
			.inner
			.lock()
			.await
			.usage
			.entry((rid.to_owned(), day.to_owned()))
			.or_insert(0) += bytes;
	}

	async fn usage(&self, rid: &str, day: &str) -> Result<u64, StoreError> {
		Ok(self
			.inner
			.lock()
			.await
			.usage
			.get(&(rid.to_owned(), day.to_owned()))
			.copied()
			.unwrap_or(0))
	}
}

/// A hook the splice offers to tests: every frame a client sends may be
/// rewritten before the relay forwards it. The relay itself never sets one;
/// a production [`RelayOptions`] leaves it `None`.
pub type FrameHook = Arc<dyn Fn(&mut [u8]) + Send + Sync>;

/// How to serve the relay.
#[derive(Clone)]
pub struct RelayOptions<S> {
	/// The store registrations and usage live in.
	pub store: S,
	/// The limits to enforce.
	pub limits: Limits,
	/// A test hook on client frames; `None` in production.
	pub frame_hook: Option<FrameHook>,
}

/// The relay's live state over a store.
pub struct Relay<S: RelayStore> {
	options: RelayOptions<S>,
	listeners: Mutex<HashMap<String, Listener>>,
	pending: Mutex<HashMap<String, PendingConnect>>,
	connections: Mutex<HashMap<String, usize>>,
	usage_delta: Mutex<HashMap<(String, String), u64>>,
}

/// A daemon's verified control socket.
struct Listener {
	key: PublicKey,
	generation: u64,
	opens: tokio::sync::mpsc::Sender<String>,
	replaced: Arc<Notify>,
}

/// A client waiting for its daemon.
struct PendingConnect {
	rid: String,
	accepted: oneshot::Sender<WebSocket>,
}

/// The relay routes over `options`, ready to serve or merge.
pub fn router<S: RelayStore>(options: RelayOptions<S>) -> Router {
	Router::new()
		.route("/v1/listen", get(listen))
		.route("/v1/connect/{rid}", get(connect))
		.route("/v1/accept/{conn}", get(accept))
		.with_state(Arc::new(Relay::new(options)))
}

/// Serve an in-memory relay on a local port, for tests. Returns the address
/// it listens on and the store it keeps, to seed invites and read usage.
pub async fn spawn_memory(limits: Limits) -> std::io::Result<(SocketAddr, MemoryStore)> {
	let store = MemoryStore::open();
	let app = router(RelayOptions {
		store: store.clone(),
		limits,
		frame_hook: None,
	});
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
	let address = listener.local_addr()?;
	tokio::spawn(async move {
		axum::serve(listener, app).await.expect("the relay serves");
	});
	Ok((address, store))
}

impl<S: RelayStore> Relay<S> {
	fn new(options: RelayOptions<S>) -> Self {
		Self {
			options,
			listeners: Mutex::new(HashMap::new()),
			pending: Mutex::new(HashMap::new()),
			connections: Mutex::new(HashMap::new()),
			usage_delta: Mutex::new(HashMap::new()),
		}
	}

	/// Bytes counted for `rid` today that the store does not have yet.
	async fn unflushed(&self, rid: &str, today: &str) -> u64 {
		self.usage_delta
			.lock()
			.await
			.get(&(rid.to_owned(), today.to_owned()))
			.copied()
			.unwrap_or(0)
	}

	/// Count `bytes` against `rid`'s day: at once in memory, in the store
	/// when the connection ends.
	async fn count(&self, rid: &str, today: &str, bytes: u64) {
		if bytes == 0 {
			return;
		}
		*self
			.usage_delta
			.lock()
			.await
			.entry((rid.to_owned(), today.to_owned()))
			.or_insert(0) += bytes;
	}

	/// Move the day's counted bytes into the store.
	async fn flush(&self, rid: &str, today: &str) {
		let bytes = self
			.usage_delta
			.lock()
			.await
			.remove(&(rid.to_owned(), today.to_owned()))
			.unwrap_or(0);
		if bytes > 0 {
			self.options.store.add_usage(rid, today, bytes).await;
		}
	}
}

/// The host the client dialed, as the handshake signatures see it. Missing
/// means a broken proxy; verification then fails, which is the safe side.
fn authority(headers: &HeaderMap) -> String {
	headers
		.get(axum::http::header::HOST)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_owned()
}

/// `GET /v1/listen`: the daemon's control socket.
async fn listen<S: RelayStore>(
	State(relay): State<Arc<Relay<S>>>,
	headers: HeaderMap,
	ws: WebSocketUpgrade,
) -> axum::response::Response {
	let host = authority(&headers);
	ws.max_message_size(relay.options.limits.max_frame)
		.max_frame_size(relay.options.limits.max_frame)
		.on_upgrade(move |socket| control_socket(relay, host, socket))
}

async fn control_socket<S: RelayStore>(relay: Arc<Relay<S>>, host: String, socket: WebSocket) {
	let (mut sink, mut stream) = socket.split();
	let challenge = fresh_challenge();
	if send_json(
		&mut sink,
		&proto::Challenge {
			challenge: URL_SAFE_NO_PAD.encode(challenge),
		},
	)
	.await
	.is_err()
	{
		return;
	}
	let Ok(Some(answer)) = recv_json::<proto::ListenAnswer>(&mut stream).await else {
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	};
	// Only a signature by the rendezvous key itself registers anything.
	let Some(key) = proto::verify_listen(&host, &challenge, &answer) else {
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	};
	let rid = key::rid(key.as_bytes());
	if let Err(error) = relay
		.options
		.store
		.register(&rid, &answer.public_key, answer.invite.as_deref())
		.await
	{
		tracing::warn!(rid = %rid, %error, "relay refused a registration");
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	}
	relay.options.store.touch(&rid).await;

	// A newer verified listener replaces an older one; the older one leaves
	// without unregistering its replacement.
	let (opens, mut opens_rx) = tokio::sync::mpsc::channel::<String>(8);
	let replaced = Arc::new(Notify::new());
	let generation = {
		let mut listeners = relay.listeners.lock().await;
		let generation = listeners.get(&rid).map_or(0, |old| old.generation + 1);
		if let Some(old) = listeners.get(&rid) {
			old.replaced.notify_waiters();
		}
		listeners.insert(
			rid.clone(),
			Listener {
				key,
				generation,
				opens,
				replaced: Arc::clone(&replaced),
			},
		);
		generation
	};
	tracing::info!(rid = %rid, generation, "relay listener registered");

	let mut last = Instant::now();
	loop {
		let idle = tokio::time::sleep_until(tokio::time::Instant::from_std(
			last + Duration::from_secs(30),
		));
		tokio::select! {
			notice = opens_rx.recv() => match notice {
				Some(open) => {
					last = Instant::now();
					if send_json(&mut sink, &proto::OpenNotice { open }).await.is_err() {
						break;
					}
				}
				None => break,
			},
			message = stream.next() => match message {
				Some(Ok(Message::Ping(_) | Message::Pong(_))) => last = Instant::now(),
				_ => break,
			},
			() = idle => {
				last = Instant::now();
				if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
					break;
				}
			},
			() = replaced.notified() => break,
		}
	}
	// Unregister only if no newer listener took the rid over.
	let mut listeners = relay.listeners.lock().await;
	if listeners
		.get(&rid)
		.is_some_and(|current| current.generation == generation)
	{
		listeners.remove(&rid);
	}
}

/// `GET /v1/connect/{rid}`: a client, waiting to be joined to its daemon.
async fn connect<S: RelayStore>(
	State(relay): State<Arc<Relay<S>>>,
	Path(rid): Path<String>,
	ws: WebSocketUpgrade,
) -> axum::response::Response {
	ws.max_message_size(relay.options.limits.max_frame)
		.max_frame_size(relay.options.limits.max_frame)
		.on_upgrade(move |socket| connect_socket(relay, rid, socket))
}

async fn connect_socket<S: RelayStore>(relay: Arc<Relay<S>>, rid: String, socket: WebSocket) {
	if !key::valid_rid(&rid) {
		let (mut sink, _stream) = socket.split();
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	}
	// This client connection counts against the rid's from the moment it
	// arrives, waiting or spliced.
	{
		let mut connections = relay.connections.lock().await;
		let live = connections.entry(rid.clone()).or_insert(0);
		if *live >= relay.options.limits.connections {
			let (mut sink, _stream) = socket.split();
			close_sink(&mut sink, BUSY).await;
			return;
		}
		*live += 1;
	}
	connect_wait(Arc::clone(&relay), &rid, socket).await;
	*relay.connections.lock().await.entry(rid).or_insert(1) -= 1;
}

async fn connect_wait<S: RelayStore>(relay: Arc<Relay<S>>, rid: &str, socket: WebSocket) {
	let (mut sink, stream) = socket.split();
	let Some(control) = relay
		.listeners
		.lock()
		.await
		.get(rid)
		.map(|listener| (listener.opens.clone(), listener.key))
	else {
		close_sink(&mut sink, NO_DAEMON).await;
		return;
	};
	let today = day(Timestamp::now());
	let used = relay
		.options
		.store
		.usage(rid, &today)
		.await
		.unwrap_or_default()
		+ relay.unflushed(rid, &today).await;
	if used >= relay.options.limits.bytes_per_day {
		close_sink(&mut sink, DAILY).await;
		return;
	}
	{
		let pending = relay.pending.lock().await;
		let waiting = pending.values().filter(|p| p.rid == rid).count();
		if waiting >= relay.options.limits.pending_connects {
			close_sink(&mut sink, BUSY).await;
			return;
		}
	}
	let conn = Uuid::now_v7().to_string();
	let (accepted, mut accepted_rx) = oneshot::channel::<WebSocket>();
	relay.pending.lock().await.insert(
		conn.clone(),
		PendingConnect {
			rid: rid.to_owned(),
			accepted,
		},
	);
	if control.0.send(conn.clone()).await.is_err() {
		relay.pending.lock().await.remove(&conn);
		close_sink(&mut sink, NO_DAEMON).await;
		return;
	}
	let Ok(Ok(daemon_socket)) =
		tokio::time::timeout(relay.options.limits.accept_timeout, &mut accepted_rx).await
	else {
		relay.pending.lock().await.remove(&conn);
		close_sink(&mut sink, NO_DAEMON).await;
		return;
	};
	relay.pending.lock().await.remove(&conn);
	relay.options.store.touch(rid).await;
	splice(
		Arc::clone(&relay),
		rid.to_owned(),
		relay.options.limits.bytes_per_day - used,
		sink,
		stream,
		daemon_socket,
	)
	.await;
}

/// `GET /v1/accept/{conn}`: the daemon's side of a spliced connection.
async fn accept<S: RelayStore>(
	State(relay): State<Arc<Relay<S>>>,
	headers: HeaderMap,
	Path(conn): Path<String>,
	ws: WebSocketUpgrade,
) -> axum::response::Response {
	let host = authority(&headers);
	ws.max_message_size(relay.options.limits.max_frame)
		.max_frame_size(relay.options.limits.max_frame)
		.on_upgrade(move |socket| accept_socket(relay, host, conn, socket))
}

async fn accept_socket<S: RelayStore>(
	relay: Arc<Relay<S>>,
	host: String,
	conn: String,
	socket: WebSocket,
) {
	let (mut sink, mut stream) = socket.split();
	let challenge = fresh_challenge();
	if send_json(
		&mut sink,
		&proto::Challenge {
			challenge: URL_SAFE_NO_PAD.encode(challenge),
		},
	)
	.await
	.is_err()
	{
		return;
	}
	let Ok(Some(answer)) = recv_json::<proto::AcceptAnswer>(&mut stream).await else {
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	};
	// Whether or not the answer verifies, this connection id is spent.
	let pending = relay.pending.lock().await.remove(&conn);
	let Some(pending) = pending else {
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	};
	let expected = relay
		.listeners
		.lock()
		.await
		.get(&pending.rid)
		.map(|listener| listener.key);
	let verified =
		expected.is_some_and(|key| proto::verify_accept(&host, &conn, &challenge, &key, &answer));
	if !verified {
		close_sink(&mut sink, HANDSHAKE).await;
		return;
	}
	// The halves came from one socket, so reunite always succeeds; the
	// arm only satisfies the type.
	let socket = match sink.reunite(stream) {
		Ok(socket) => socket,
		Err(error) => {
			let _ = error;
			return;
		}
	};
	let _ = pending.accepted.send(socket);
}

/// Copy binary frames between a client and its daemon until either side
/// closes, the idle timeout passes, or the day's remaining budget runs out.
async fn splice<S: RelayStore>(
	relay: Arc<Relay<S>>,
	rid: String,
	budget: u64,
	mut to_client: futures_util::stream::SplitSink<WebSocket, Message>,
	mut from_client: futures_util::stream::SplitStream<WebSocket>,
	daemon: WebSocket,
) {
	let (mut to_daemon, mut from_daemon) = daemon.split();
	let today = day(Timestamp::now());
	let mut last = Instant::now();
	let mut moved = 0_u64;
	let mut reason = "";
	loop {
		let idle = tokio::time::sleep_until(tokio::time::Instant::from_std(
			last + relay.options.limits.idle_timeout,
		));
		tokio::select! {
			frame = from_client.next() => {
				let Some(frame) = frame else { break };
				match frame {
					Ok(Message::Binary(bytes)) => {
						last = Instant::now();
						let frame = if let Some(hook) = relay.options.frame_hook.as_ref() {
							let mut bytes = bytes.to_vec();
							hook(&mut bytes);
							bytes.into()
						} else {
							bytes
						};
						moved += u64::try_from(frame.len()).unwrap_or(u64::MAX);
						if moved > budget {
							reason = DAILY;
							break;
						}
						if to_daemon.send(Message::Binary(frame)).await.is_err() {
							break;
						}
					}
					Ok(Message::Ping(_) | Message::Pong(_)) => last = Instant::now(),
					_ => break,
				}
			}
			frame = from_daemon.next() => {
				let Some(frame) = frame else { break };
				match frame {
					Ok(Message::Binary(bytes)) => {
						last = Instant::now();
						moved += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
						if moved > budget {
							reason = DAILY;
							break;
						}
						if to_client.send(Message::Binary(bytes)).await.is_err() {
							break;
						}
					}
					Ok(Message::Ping(_) | Message::Pong(_)) => last = Instant::now(),
					_ => break,
				}
			}
			() = idle => {
				reason = IDLE;
				break;
			}
		}
	}
	relay.count(&rid, &today, moved).await;
	if !reason.is_empty() {
		let _ = to_client.send(close(reason)).await;
		let _ = to_daemon.send(close(reason)).await;
	}
	relay.flush(&rid, &today).await;
}

fn fresh_challenge() -> [u8; 32] {
	let mut challenge = [0u8; 32];
	getrandom::fill(&mut challenge).expect("the OS random source is available");
	challenge
}

fn close(reason: &str) -> Message {
	Message::Close(Some(CloseFrame {
		code: close_code::NORMAL,
		reason: reason.to_owned().into(),
	}))
}

async fn close_sink(sink: &mut futures_util::stream::SplitSink<WebSocket, Message>, reason: &str) {
	let _ = sink.send(close(reason)).await;
}

async fn send_json<T: Serialize>(
	sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
	value: &T,
) -> Result<(), ()> {
	let text = serde_json::to_string(value).map_err(|_| ())?;
	sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

async fn recv_json<T: DeserializeOwned>(
	stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<Option<T>, ()> {
	loop {
		match stream.next().await {
			Some(Ok(Message::Text(text))) => {
				return serde_json::from_str(&text).map(Some).map_err(|_| ());
			}
			Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
			Some(Ok(Message::Close(_))) | None => return Ok(None),
			Some(Ok(Message::Binary(_)) | Err(_)) => return Err(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn memory_stores_track_registrations_invites_and_usage() {
		let store = MemoryStore::invite_only();
		store.add_invite("welcome").await;
		let key = key::RelayKey::generate();
		let rid = key.rid();
		assert!(matches!(
			store.register(&rid, &key.public().to_string(), None).await,
			Err(StoreError::InviteRequired)
		));
		assert!(matches!(
			store
				.register(&rid, &key.public().to_string(), Some("wrong"))
				.await,
			Err(StoreError::InviteUsed)
		));
		// The invite opens the door once…
		store
			.register(&rid, &key.public().to_string(), Some("welcome"))
			.await
			.unwrap();
		// …and a registered daemon may re-listen without a fresh invite.
		store
			.register(&rid, &key.public().to_string(), None)
			.await
			.unwrap();
		// The invite is spent: a different daemon cannot use it.
		let other = key::RelayKey::generate();
		assert!(matches!(
			store
				.register(&other.rid(), &other.public().to_string(), Some("welcome"))
				.await,
			Err(StoreError::InviteUsed)
		));
		assert!(matches!(
			store
				.register(&rid, &other.public().to_string(), None)
				.await,
			Err(StoreError::KeyMismatch)
		));
	}
}
