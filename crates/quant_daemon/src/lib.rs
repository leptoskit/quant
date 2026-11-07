//! `quantd`, the Quant daemon.
//!
//! It holds the unlocked vault, runs capture sources, and serves the API in
//! [`quant_api`] on a Unix socket only the owner's account can open. Clients
//! sign every request with their own key and are held to the grant the owner
//! approved; owner actions need the passphrase.

pub mod audio_save;
mod authn;
pub mod capture;
mod error;
mod ingest;
mod owner;
mod query;
mod state;
pub mod tls;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::post;
use jiff::Timestamp;
use quant_api::routes;
pub use state::State;

/// The API router over shared state.
pub fn router(state: Arc<State>) -> Router {
	Router::new()
		.route(routes::HELLO, get(owner::hello))
		.route(routes::PAIR, post(owner::pair))
		.route(routes::ME, get(query::me))
		.route(routes::STREAMS, get(query::streams))
		.route(routes::OVERVIEW, post(query::overview))
		.route(routes::RECORDS, post(query::records))
		.route(routes::SEARCH, post(query::search))
		.route(routes::SERIES, post(query::series))
		.route(routes::DURATIONS, post(query::durations))
		.route(&format!("{}/{{hash}}", routes::MEDIA), get(query::media))
		.route(routes::INGEST_RECORDS, post(ingest::records))
		.route(routes::INGEST_SERIES, post(ingest::series))
		.route(routes::INGEST_MEDIA, post(ingest::media))
		.route(routes::AUDIT, post(owner::client_audit))
		.route(routes::AUDIO_SAVE, post(audio_save::save))
		.route(routes::OWNER_AUDIO_SAVE, post(audio_save::owner_save))
		.route(routes::OWNER_UNLOCK, post(owner::unlock))
		.route(routes::OWNER_LOCK, post(owner::lock))
		.route(routes::OWNER_CLIENTS, get(owner::clients))
		.route(routes::OWNER_GRANTS, get(owner::grants))
		.route(routes::OWNER_APPROVE, post(owner::approve))
		.route(routes::OWNER_DENY, post(owner::deny))
		.route(routes::OWNER_REVOKE, post(owner::revoke))
		.route(routes::OWNER_PURGE, post(owner::purge))
		.route(routes::OWNER_AUDIT, post(owner::audit))
		.route(routes::OWNER_STATUS, get(owner::status))
		.route(routes::OWNER_STREAMS, post(owner::register_stream))
		.route(
			routes::OWNER_DERIVE,
			get(owner::derive_config).post(owner::set_derive_config),
		)
		.route(
			routes::OWNER_CAPTURE,
			get(owner::capture_config).post(owner::set_capture_config),
		)
		.route(routes::OWNER_CAPTURE_PAUSE, post(owner::pause_capture))
		.layer(DefaultBodyLimit::max(authn::MAX_BODY))
		.with_state(state)
}

/// Sweep expired media every so often.
pub fn spawn_maintenance(state: Arc<State>) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		let mut tick = tokio::time::interval(Duration::from_mins(15));
		loop {
			tick.tick().await;
			let sweeper = Arc::clone(&state);
			match tokio::task::spawn_blocking(move || sweeper.vault.sweep_media(Timestamp::now()))
				.await
			{
				Ok(Ok(0)) => {}
				Ok(Ok(n)) => tracing::info!(files = n, "swept expired media"),
				Ok(Err(error)) => tracing::warn!(%error, "media sweep failed"),
				Err(error) => tracing::warn!(%error, "media sweep panicked"),
			}
		}
	})
}

/// Keep derived memory (posture spans, daily summaries) up to date, every
/// ten minutes. The configuration lives in the vault under `derive.config`.
pub fn spawn_derivation(state: Arc<State>) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		let mut tick = tokio::time::interval(Duration::from_mins(10));
		loop {
			tick.tick().await;
			let deriving = Arc::clone(&state);
			let outcome = tokio::task::spawn_blocking(move || {
				let store = &deriving.vault.store;
				let config = store
					.meta("derive.config")?
					.and_then(|json| serde_json::from_str(&json).ok())
					.unwrap_or_default();
				quant_derive::run(
					store,
					&config,
					Timestamp::now(),
					&jiff::tz::TimeZone::system(),
				)
			})
			.await;
			match outcome {
				Ok(Ok(report)) => tracing::debug!(?report, "derived"),
				Ok(Err(error)) => tracing::warn!(%error, "derivation failed"),
				Err(error) => tracing::warn!(%error, "derivation panicked"),
			}
		}
	})
}

/// Serve on a Unix socket at `path`, readable and writable by the owner
/// only. Returns when `shutdown` resolves.
#[cfg(unix)]
pub async fn serve_socket(
	state: Arc<State>,
	path: &Path,
	shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
	use std::os::unix::fs::PermissionsExt;

	if path.exists() {
		// A live daemon answers; a stale socket from a crash does not.
		if tokio::net::UnixStream::connect(path).await.is_ok() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::AddrInUse,
				format!("another quantd is already serving {}", path.display()),
			));
		}
		std::fs::remove_file(path)?;
	}
	let listener = tokio::net::UnixListener::bind(path)?;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
	tracing::info!(socket = %path.display(), "serving");
	let result = axum::serve(listener, router(state))
		.with_graceful_shutdown(shutdown)
		.await;
	let _ = std::fs::remove_file(path);
	result
}

/// Serve over TLS on TCP, for phones and other devices on the local
/// network. Devices pin the certificate fingerprint from the pairing link.
pub async fn serve_tls(
	state: Arc<State>,
	address: std::net::SocketAddr,
	identity: &tls::TlsIdentity,
	shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
	let listener = tls::TlsListener::bind(address, identity).await?;
	*state
		.tls_fingerprint
		.write()
		.unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity.fingerprint());
	tracing::info!(%address, fingerprint = identity.fingerprint(), "serving tls");
	axum::serve(listener, router(state))
		.with_graceful_shutdown(shutdown)
		.await
}
