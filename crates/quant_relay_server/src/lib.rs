//! The Quant relay server: a leptoskit backend that joins clients to
//! `quant daemon` while seeing only TLS bytes.
//!
//! The WebSocket routes and the limits around them come from
//! `quant_relay::server`, shared with the daemon's tests; this crate adds
//! the deployment parts: serving with `leptoskit_axum`, storage in
//! `leptoskit_db` through [`relay_db`], and the limits read from the
//! environment.

use std::net::SocketAddr;

use leptoskit_axum::GracefulShutdown;
use leptoskit_axum::Readiness;
use leptoskit_axum::axum::Router;
use leptoskit_axum::serve_listener_with;
use quant_relay::Limits;
use quant_relay::server::RelayOptions;
use relay_db::RelayDb;

/// Read a number from `name`, falling back to `fallback`.
fn env_parse<T: std::str::FromStr>(name: &str, fallback: T) -> T {
	std::env::var(name)
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or(fallback)
}

/// The limits this relay enforces, from `RELAY_*` environment variables:
/// `RELAY_MAX_CONNECTIONS`, `RELAY_BYTES_PER_DAY`, `RELAY_MAX_FRAME`,
/// `RELAY_IDLE_SECS`, `RELAY_ACCEPT_SECS`, `RELAY_PENDING_CONNECTS`.
pub fn limits_from_env() -> Limits {
	Limits {
		connections: env_parse("RELAY_MAX_CONNECTIONS", Limits::default().connections),
		bytes_per_day: env_parse("RELAY_BYTES_PER_DAY", Limits::default().bytes_per_day),
		max_frame: env_parse("RELAY_MAX_FRAME", Limits::default().max_frame),
		idle_timeout: std::time::Duration::from_secs(env_parse("RELAY_IDLE_SECS", 300)),
		accept_timeout: std::time::Duration::from_secs(env_parse("RELAY_ACCEPT_SECS", 10)),
		pending_connects: env_parse("RELAY_PENDING_CONNECTS", Limits::default().pending_connects),
	}
}

/// The relay's routes over `store`, with the health checks a deploy needs.
pub fn router(store: RelayDb, limits: Limits, readiness: &Readiness) -> Router {
	quant_relay::server::router(RelayOptions {
		store,
		limits,
		frame_hook: None,
	})
	.merge(readiness.routes())
}

/// Where to listen: `LEPTOS_SITE_ADDR`, which `leptoskit serve` and the
/// generated containers set, and 127.0.0.1:3000 otherwise.
pub fn address() -> SocketAddr {
	std::env::var("LEPTOS_SITE_ADDR")
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or_else(|| "127.0.0.1:3000".parse().expect("a valid address"))
}

/// Serve on `listener` until `SIGTERM` or Ctrl-C, draining connections
/// first. Migrations are the caller's: they gate readiness.
pub async fn serve(
	listener: tokio::net::TcpListener,
	store: RelayDb,
	limits: Limits,
	readiness: Readiness,
) -> Result<(), leptoskit_axum::ServeError> {
	let app = router(store, limits, &readiness);
	let shutdown = GracefulShutdown::new().readiness(&readiness);
	serve_listener_with(listener, app, shutdown).await
}
