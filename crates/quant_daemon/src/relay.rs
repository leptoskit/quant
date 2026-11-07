//! The relay listener: one outbound WebSocket that brings the daemon to its
//! clients wherever they are, without opening a port.
//!
//! The daemon derives an Ed25519 key from the vault key and proves ownership
//! of the rendezvous id by signing the relay's challenge, so nobody else can
//! take its place. When a client connects, the relay says so on the control
//! socket; the daemon opens a second WebSocket, authenticates it the same
//! way, and then runs the same TLS acceptor and the same router as
//! [`serve_tls`](crate::serve_tls) over it. Relay traffic is therefore never
//! a byte more trusted than LAN traffic: the same pinned certificate, the
//! same signed requests, the same grants.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State as AxumState;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use quant_api::RelaySetRequest;
use quant_api::RelayState;
use quant_relay::RelayKey;
use quant_relay::RelayUrl;
use quant_relay::dial;
use quant_relay::dial::WsStream;
use quant_relay::proto::OpenNotice;
use quant_store::Store;
use quant_store::StoreError;
use serde_json::json;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::State;
use crate::authn::Owner;
use crate::error::ApiResult;
use crate::error::Error;
use crate::owner::owner_audit;
use crate::router;
use crate::tls::TlsIdentity;

type Shared = AxumState<Arc<State>>;

/// How long to wait for an accepted relay stream to finish its TLS
/// handshake, like the TCP listener does.
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);

/// The relay the daemon listens on, from the vault's meta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayConfig {
	/// Where the relay lives.
	pub url: RelayUrl,
	/// An invite, for relays that gate first registration.
	pub invite: Option<String>,
}

/// Read the relay settings. `None` until the owner sets one.
pub fn config(store: &Store) -> Result<Option<RelayConfig>, StoreError> {
	let Some(url) = store.meta("relay.url")?.filter(|url| !url.is_empty()) else {
		return Ok(None);
	};
	let url = RelayUrl::parse(&url).map_err(|_| StoreError::Invalid("relay.url".into()))?;
	let invite = store
		.meta("relay.invite")?
		.filter(|invite| !invite.is_empty());
	Ok(Some(RelayConfig { url, invite }))
}

/// Store (or, with `None`, clear) the relay settings.
fn set_config(store: &Store, config: Option<&RelayConfig>) -> Result<(), StoreError> {
	if let Some(config) = config {
		store.set_meta("relay.url", &config.url.to_string())?;
		let invite = config.invite.as_deref().unwrap_or_default();
		store.set_meta("relay.invite", invite)?;
	} else {
		store.set_meta("relay.url", "")?;
		store.set_meta("relay.invite", "")?;
	}
	Ok(())
}

/// `GET /v1/owner/relay`
pub async fn owner_state(
	AxumState(state): Shared,
	_owner: Owner,
) -> ApiResult<axum::Json<RelayState>> {
	let config = config(&state.vault.store)?;
	Ok(axum::Json(RelayState {
		rid: state.relay_key.rid(),
		url: config.as_ref().map(|config| config.url.to_string()),
		invite: config.is_some_and(|config| config.invite.is_some()),
	}))
}

/// `POST /v1/owner/relay`
pub async fn owner_set(
	AxumState(state): Shared,
	owner: Owner,
	axum::Json(body): axum::Json<RelaySetRequest>,
) -> ApiResult<axum::Json<RelayState>> {
	let config = match body.url {
		None => None,
		Some(text) => {
			let url = RelayUrl::parse(&text)
				.map_err(|error| Error::Invalid(format!("{error}; try wss://relay.example.com")))?;
			Some(RelayConfig {
				url,
				invite: body.invite.filter(|invite| !invite.is_empty()),
			})
		}
	};
	set_config(&state.vault.store, config.as_ref())?;
	let action = if config.is_some() {
		"relay.set"
	} else {
		"relay.off"
	};
	let detail = match &config {
		Some(config) => json!({ "url": config.url.to_string(), "invite": config.invite.is_some() }),
		None => json!({}),
	};
	owner_audit(&state, action, "relay", detail)?;
	owner_state(AxumState(state), owner).await
}

/// Keep a listener on the relay named in the vault's meta until `shutdown`.
/// The settings are re-read whenever the connection drops, so `quant relay
/// set` and `quant relay off` take effect without restarting the daemon.
pub async fn listen(state: Arc<State>, shutdown: impl Future<Output = ()> + Send + 'static) {
	let mut shutdown = Box::pin(shutdown);
	let mut backoff = Duration::from_secs(1);
	loop {
		// The acceptor and router are the ones every connection shares.
		let config = match config(&state.vault.store) {
			Ok(Some(config)) => config,
			Ok(None) => return,
			Err(error) => {
				tracing::warn!(%error, "could not read the relay settings");
				return;
			}
		};
		let acceptor = TlsIdentity::load_or_create(&state.vault.store)
			.map_err(std::io::Error::other)
			.and_then(|identity| identity.acceptor().map_err(std::io::Error::other));
		let outcome = match acceptor {
			Ok(acceptor) => {
				tokio::select! {
					outcome = one_connection(Arc::clone(&state), &config, acceptor) => outcome,
					() = &mut shutdown => return,
				}
			}
			Err(error) => Err(std::io::Error::other(error)),
		};
		if outcome.is_err() {
			tracing::debug!(url = %config.url, ?outcome, "relay connection ended");
		}
		// Reconnect with exponential backoff and jitter, unless we are
		// stopping or the owner turned the relay off.
		let wait = backoff + jitter(backoff);
		backoff = (backoff * 2).min(Duration::from_secs(60));
		tokio::select! {
			() = &mut shutdown => return,
			() = tokio::time::sleep(wait) => {}
		}
	}
}

/// Up to half again the backoff, so reconnecting daemons do not line up.
fn jitter(backoff: Duration) -> Duration {
	Duration::from_millis(backoff.as_millis().rem_euclid(7).try_into().unwrap_or(0))
}

async fn one_connection(
	state: Arc<State>,
	config: &RelayConfig,
	acceptor: TlsAcceptor,
) -> std::io::Result<()> {
	let mut socket = dial::dial(&config.url.listen()).await?;
	dial::listen_handshake(
		&mut socket,
		&config.url.authority,
		&state.relay_key,
		config.invite.as_deref(),
	)
	.await?;
	tracing::info!(url = %config.url, rid = %state.relay_key.rid(), "relay listener up");
	let router = router(Arc::clone(&state));
	let mut ping = tokio::time::interval(Duration::from_secs(30));
	ping.reset();
	loop {
		tokio::select! {
			_ = ping.tick() => dial::ping(&mut socket).await?,
			notice = dial::recv_text::<OpenNotice>(&mut socket) => match notice {
				Ok(Some(notice)) => {
					let url = config.url.clone();
					let conn = notice.open;
					let router = router.clone();
					let acceptor = acceptor.clone();
					let key = RelayKey::clone(&state.relay_key);
					tokio::spawn(async move {
						accept_connection(url, conn, key, acceptor, router).await;
					});
				}
				_ => return Err(std::io::Error::new(
					std::io::ErrorKind::UnexpectedEof,
					"the relay closed the control socket",
				)),
			},
		}
	}
}

/// Open `/v1/accept/<conn>` for a waiting client and serve it exactly as the
/// TLS listener would.
async fn accept_connection(
	url: RelayUrl,
	conn: String,
	key: RelayKey,
	acceptor: TlsAcceptor,
	router: Router,
) {
	let mut socket = match dial::dial(&url.accept(&conn)).await {
		Ok(socket) => socket,
		Err(error) => {
			tracing::debug!(%error, %conn, "could not accept a relay connection");
			return;
		}
	};
	if let Err(error) = dial::accept_handshake(&mut socket, &url.authority, &conn, &key).await {
		tracing::debug!(%error, %conn, "relay accept handshake failed");
		return;
	}
	let stream = WsStream::new(socket);
	let tls = match tokio::time::timeout(TLS_HANDSHAKE, acceptor.accept(stream)).await {
		Ok(Ok(tls)) => tls,
		Ok(Err(error)) => {
			tracing::debug!(%error, %conn, "relay tls handshake failed");
			return;
		}
		Err(_) => {
			tracing::debug!(%conn, "relay tls handshake timed out");
			return;
		}
	};
	if let Err(error) = serve(tls, router).await {
		tracing::debug!(%error, %conn, "relay connection ended");
	}
}

/// HTTP over one already-authenticated stream, the same router as
/// [`serve_tls`](crate::serve_tls).
async fn serve<S>(stream: TlsStream<S>, router: Router) -> std::io::Result<()>
where
	S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
	let service = TowerToHyperService::new(router);
	Builder::new(TokioExecutor::new())
		.serve_connection_with_upgrades(TokioIo::new(stream), service)
		.await
		.map_err(std::io::Error::other)
}
