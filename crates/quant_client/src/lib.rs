//! Talk to a Quant daemon.
//!
//! A [`Client`] holds an [`Identity`]: a client id and a private signing key
//! that never leaves this machine. Every request is signed with it. Pair a
//! new identity with [`Client::pair`], have the owner approve it, then query
//! within the grant they chose. Owner actions go through an [`OwnerSession`]
//! opened with the passphrase.

mod identity;
mod pinned;
pub use pinned::parse_fingerprint;
pub mod when;

use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::Method;
use hyper::Request;
use hyper::StatusCode;
use hyper_util::rt::TokioIo;
pub use identity::*;
use jiff::Timestamp;
use quant_access::Grant;
use quant_api::ApiError;
use quant_api::ApproveRequest;
use quant_api::Audit;
use quant_api::AuditRequest;
use quant_api::CaptureState;
use quant_api::ClientKind;
use quant_api::ClientView;
use quant_api::DenyRequest;
use quant_api::Durations;
use quant_api::DurationsRequest;
use quant_api::Hello;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_api::Ingested;
use quant_api::Me;
use quant_api::Overview;
use quant_api::OverviewRequest;
use quant_api::PairRequest;
use quant_api::PairResponse;
use quant_api::PurgeRequest;
use quant_api::Purged;
use quant_api::Records;
use quant_api::RecordsRequest;
use quant_api::RenameClip;
use quant_api::RevokeRequest;
use quant_api::SaveAudio;
use quant_api::SavedClip;
use quant_api::Series;
use quant_api::SeriesRequest;
use quant_api::Status;
use quant_api::StreamAccess;
use quant_api::UnlockRequest;
use quant_api::routes;
use quant_core::ClientId;
use quant_core::MediaRef;
use quant_core::StreamId;
use quant_core::StreamSpec;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Where the daemon listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
	/// A Unix socket path.
	Unix(PathBuf),
	/// `host:port` over plain TCP (loopback only; requests are signed but
	/// not encrypted).
	Tcp(String),
	/// `host:port` over TLS, trusting only the certificate with this
	/// SHA-256 fingerprint.
	Tls {
		/// `host:port`.
		address: String,
		/// The pinned certificate fingerprint.
		fingerprint: [u8; 32],
	},
}

impl Endpoint {
	/// Parse `unix:/path`, `tcp://host:port`, `tls://host:port#<hex
	/// fingerprint>`, a bare path, or a bare `host:port`.
	pub fn parse(s: &str) -> Result<Self, String> {
		if let Some(rest) = s.strip_prefix("tls://") {
			let (address, fingerprint) = rest
				.split_once('#')
				.ok_or("a tls:// address needs #<certificate fingerprint>")?;
			let fingerprint =
				parse_fingerprint(fingerprint).ok_or("bad certificate fingerprint")?;
			return Ok(Endpoint::Tls {
				address: address.to_owned(),
				fingerprint,
			});
		}
		if let Some(address) = s.strip_prefix("tcp://") {
			return Ok(Endpoint::Tcp(address.to_owned()));
		}
		if let Some(path) = s.strip_prefix("unix:") {
			return Ok(Endpoint::Unix(path.into()));
		}
		if s.contains('/') {
			return Ok(Endpoint::Unix(s.into()));
		}
		Ok(Endpoint::Tcp(s.to_owned()))
	}
}

/// A failed call.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
	/// The daemon answered with an error.
	#[error("{} ({status}): {}", .error.code, .error.message)]
	Api {
		/// HTTP status.
		status: u16,
		/// The error body.
		error: ApiError,
	},
	/// The daemon could not be reached.
	#[error("cannot reach quantd at {endpoint}: {source}")]
	Unreachable {
		/// Where we tried.
		endpoint: String,
		/// Why.
		source: std::io::Error,
	},
	/// HTTP failed.
	#[error("http: {0}")]
	Http(String),
	/// The answer did not decode.
	#[error("decode: {0}")]
	Decode(#[from] serde_json::Error),
	/// This client has not paired yet.
	#[error("this identity has not paired with a vault; run `quant pair` first")]
	NotPaired,
}

impl ClientError {
	/// The API error code, if the daemon answered with one.
	pub fn code(&self) -> Option<&str> {
		match self {
			ClientError::Api { error, .. } => Some(&error.code),
			_ => None,
		}
	}
}

type Result<T, E = ClientError> = std::result::Result<T, E>;

async fn send(endpoint: &Endpoint, request: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes)> {
	send_typed(endpoint, request)
		.await
		.map(|(status, _, body)| (status, body))
}

async fn send_typed(
	endpoint: &Endpoint,
	request: Request<Full<Bytes>>,
) -> Result<(StatusCode, Option<String>, Bytes)> {
	let shown = match endpoint {
		Endpoint::Unix(path) => path.display().to_string(),
		Endpoint::Tcp(address) | Endpoint::Tls { address, .. } => address.clone(),
	};
	let unreachable = |source| {
		ClientError::Unreachable {
			endpoint: shown.clone(),
			source,
		}
	};
	macro_rules! exchange {
		($stream:expr) => {{
			let (mut sender, connection) =
				hyper::client::conn::http1::handshake(TokioIo::new($stream))
					.await
					.map_err(|e| ClientError::Http(e.to_string()))?;
			tokio::spawn(connection);
			let response = sender
				.send_request(request)
				.await
				.map_err(|e| ClientError::Http(e.to_string()))?;
			let status = response.status();
			let content_type = response
				.headers()
				.get("content-type")
				.and_then(|v| v.to_str().ok())
				.map(ToOwned::to_owned);
			let body = response
				.into_body()
				.collect()
				.await
				.map_err(|e| ClientError::Http(e.to_string()))?;
			Ok((status, content_type, body.to_bytes()))
		}};
	}
	match endpoint {
		#[cfg(unix)]
		Endpoint::Unix(path) => {
			exchange!(
				tokio::net::UnixStream::connect(path)
					.await
					.map_err(unreachable)?
			)
		}
		#[cfg(not(unix))]
		Endpoint::Unix(_) => {
			Err(unreachable(std::io::Error::other(
				"unix sockets are unavailable",
			)))
		}
		Endpoint::Tcp(address) => {
			exchange!(
				tokio::net::TcpStream::connect(address)
					.await
					.map_err(unreachable)?
			)
		}
		Endpoint::Tls {
			address,
			fingerprint,
		} => {
			let tcp = tokio::net::TcpStream::connect(address)
				.await
				.map_err(unreachable)?;
			let connector =
				pinned::connector(*fingerprint).map_err(|e| ClientError::Http(e.to_string()))?;
			let name = rustls::pki_types::ServerName::try_from("quantd.local").expect("valid name");
			let tls = connector.connect(name, tcp).await.map_err(unreachable)?;
			exchange!(tls)
		}
	}
}

fn decode<T: DeserializeOwned>(status: StatusCode, body: &[u8]) -> Result<T> {
	if status.is_success() {
		return Ok(serde_json::from_slice(body)?);
	}
	let error = serde_json::from_slice(body).unwrap_or_else(|_| {
		ApiError {
			code: "http".into(),
			message: String::from_utf8_lossy(body).into_owned(),
		}
	});
	Err(ClientError::Api {
		status: status.as_u16(),
		error,
	})
}

fn builder(method: Method, path: &str) -> hyper::http::request::Builder {
	Request::builder()
		.method(method)
		.uri(path)
		.header("host", "quantd")
}

/// A signing client.
#[derive(Clone, Debug)]
pub struct Client {
	endpoint: Endpoint,
	identity: Identity,
}

impl Client {
	/// A client for `endpoint` using `identity`.
	pub fn new(endpoint: Endpoint, identity: Identity) -> Self {
		Self { endpoint, identity }
	}

	/// The identity in use.
	pub fn identity(&self) -> &Identity {
		&self.identity
	}

	/// The endpoint in use.
	pub fn endpoint(&self) -> &Endpoint {
		&self.endpoint
	}

	async fn signed_raw(
		&self,
		method: Method,
		path: &str,
		body: Vec<u8>,
		content_type: &str,
	) -> Result<(StatusCode, Bytes)> {
		self.signed_typed(method, path, body, content_type)
			.await
			.map(|(status, _, body)| (status, body))
	}

	async fn signed_typed(
		&self,
		method: Method,
		path: &str,
		body: Vec<u8>,
		content_type: &str,
	) -> Result<(StatusCode, Option<String>, Bytes)> {
		let client = self.identity.client.ok_or(ClientError::NotPaired)?;
		let headers =
			self.identity
				.key()
				.sign(client, method.as_str(), path, &body, Timestamp::now());
		let mut request = builder(method, path).header("content-type", content_type);
		for (name, value) in headers.pairs() {
			request = request.header(name, value);
		}
		let request = request
			.body(Full::new(Bytes::from(body)))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		send_typed(&self.endpoint, request).await
	}

	async fn signed<B: Serialize, T: DeserializeOwned>(
		&self,
		method: Method,
		path: &str,
		body: Option<&B>,
	) -> Result<T> {
		let bytes = match body {
			Some(body) => serde_json::to_vec(body)?,
			None => Vec::new(),
		};
		let (status, answer) = self
			.signed_raw(method, path, bytes, "application/json")
			.await?;
		decode(status, &answer)
	}

	/// Who the vault is.
	pub async fn hello(&self) -> Result<Hello> {
		let request = builder(Method::GET, routes::HELLO)
			.body(Full::new(Bytes::new()))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		let (status, body) = send(&self.endpoint, request).await?;
		decode(status, &body)
	}

	/// Ask to pair this identity. On success the identity gains a client id;
	/// save it, then have the owner approve the returned pairing code.
	pub async fn pair(&mut self, kind: ClientKind, scopes: &[String]) -> Result<PairResponse> {
		let body = PairRequest {
			name: self.identity.name.clone(),
			kind,
			public_key: self.identity.key().public(),
			scopes: scopes.to_vec(),
		};
		let request = builder(Method::POST, routes::PAIR)
			.header("content-type", "application/json")
			.body(Full::new(Bytes::from(serde_json::to_vec(&body)?)))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		let (status, answer) = send(&self.endpoint, request).await?;
		let response: PairResponse = decode(status, &answer)?;
		self.identity.client = Some(response.client);
		Ok(response)
	}

	/// This client's status and grant.
	pub async fn me(&self) -> Result<Me> {
		self.signed::<(), _>(Method::GET, routes::ME, None).await
	}

	/// Streams this client may see.
	pub async fn streams(&self) -> Result<Vec<StreamAccess>> {
		self.signed::<(), _>(Method::GET, routes::STREAMS, None)
			.await
	}

	/// What each visible stream holds over a range.
	pub async fn overview(&self, request: &OverviewRequest) -> Result<Overview> {
		self.signed(Method::POST, routes::OVERVIEW, Some(request))
			.await
	}

	/// Records.
	pub async fn records(&self, request: &RecordsRequest) -> Result<Records> {
		self.signed(Method::POST, routes::RECORDS, Some(request))
			.await
	}

	/// Full-text search.
	pub async fn search(&self, request: &RecordsRequest) -> Result<Records> {
		self.signed(Method::POST, routes::SEARCH, Some(request))
			.await
	}

	/// A numeric stream, summarised or raw.
	pub async fn series(&self, request: &SeriesRequest) -> Result<Series> {
		self.signed(Method::POST, routes::SERIES, Some(request))
			.await
	}

	/// Time spent per group in an event stream.
	pub async fn durations(&self, request: &DurationsRequest) -> Result<Durations> {
		self.signed(Method::POST, routes::DURATIONS, Some(request))
			.await
	}

	/// Media bytes and type.
	pub async fn media(&self, hash: &str) -> Result<(String, Bytes)> {
		let path = format!("{}/{hash}", routes::MEDIA);
		let (status, mime, body) = self
			.signed_typed(Method::GET, &path, Vec::new(), "application/json")
			.await?;
		if !status.is_success() {
			return Err(decode::<()>(status, &body).expect_err("error statuses decode to errors"));
		}
		Ok((
			mime.unwrap_or_else(|| "application/octet-stream".into()),
			body,
		))
	}

	/// Store records.
	pub async fn ingest_records(&self, request: &IngestRecords) -> Result<Ingested> {
		self.signed(Method::POST, routes::INGEST_RECORDS, Some(request))
			.await
	}

	/// Store series.
	pub async fn ingest_series(&self, request: &IngestSeries) -> Result<Ingested> {
		self.signed(Method::POST, routes::INGEST_SERIES, Some(request))
			.await
	}

	/// Upload media for a record on `stream`.
	pub async fn ingest_media(
		&self,
		stream: &StreamId,
		mime: &str,
		bytes: Vec<u8>,
	) -> Result<MediaRef> {
		let path = format!(
			"{}?stream={}&mime={}",
			routes::INGEST_MEDIA,
			stream,
			mime.replace('/', "%2F")
		);
		let (status, body) = self.signed_raw(Method::POST, &path, bytes, mime).await?;
		decode(status, &body)
	}

	/// This client's own audit entries (needs `audit:read`).
	pub async fn audit(&self, request: &AuditRequest) -> Result<Audit> {
		self.signed(Method::POST, routes::AUDIT, Some(request))
			.await
	}

	/// Which capture sources run, and whether capture is paused. Any
	/// approved client may ask; it reveals nothing captured.
	pub async fn capture_status(&self) -> Result<CaptureState> {
		self.signed::<(), _>(Method::GET, routes::CAPTURE_STATUS, None)
			.await
	}

	/// Keep a stretch of the rolling audio buffer as a clip (needs
	/// `audio:write`).
	pub async fn save_audio(&self, request: &SaveAudio) -> Result<SavedClip> {
		self.signed(Method::POST, routes::AUDIO_SAVE, Some(request))
			.await
	}

	/// Give a saved clip a new title (needs `audio:write`).
	pub async fn rename_clip(&self, request: &RenameClip) -> Result<()> {
		self.signed(Method::POST, routes::AUDIO_RENAME, Some(request))
			.await
	}

	/// Open an owner session with the passphrase.
	pub async fn unlock(&self, passphrase: &str) -> Result<OwnerSession> {
		OwnerSession::open(self.endpoint.clone(), passphrase).await
	}
}

/// An open owner session.
#[derive(Clone, Debug)]
pub struct OwnerSession {
	endpoint: Endpoint,
	token: String,
	/// When it lapses.
	pub expires_at: Timestamp,
}

impl OwnerSession {
	/// Open a session at `endpoint` with the passphrase.
	pub async fn open(endpoint: Endpoint, passphrase: &str) -> Result<Self> {
		let body = serde_json::to_vec(&UnlockRequest {
			passphrase: passphrase.to_owned(),
		})?;
		let request = builder(Method::POST, routes::OWNER_UNLOCK)
			.header("content-type", "application/json")
			.body(Full::new(Bytes::from(body)))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		let (status, answer) = send(&endpoint, request).await?;
		let session: quant_api::OwnerSession = decode(status, &answer)?;
		Ok(Self {
			endpoint,
			token: session.token,
			expires_at: session.expires_at,
		})
	}

	async fn call<B: Serialize, T: DeserializeOwned>(
		&self,
		method: Method,
		path: &str,
		body: Option<&B>,
	) -> Result<T> {
		let bytes = match body {
			Some(body) => serde_json::to_vec(body)?,
			None => Vec::new(),
		};
		let request = builder(method, path)
			.header("content-type", "application/json")
			.header(routes::OWNER_HEADER, &self.token)
			.body(Full::new(Bytes::from(bytes)))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		let (status, answer) = send(&self.endpoint, request).await?;
		decode(status, &answer)
	}

	/// Every client.
	pub async fn clients(&self) -> Result<Vec<ClientView>> {
		self.call::<(), _>(Method::GET, routes::OWNER_CLIENTS, None)
			.await
	}

	/// Every grant.
	pub async fn grants(&self) -> Result<Vec<Grant>> {
		self.call::<(), _>(Method::GET, routes::OWNER_GRANTS, None)
			.await
	}

	/// Approve a pairing.
	pub async fn approve(&self, request: &ApproveRequest) -> Result<ClientView> {
		self.call(Method::POST, routes::OWNER_APPROVE, Some(request))
			.await
	}

	/// Refuse a pairing.
	pub async fn deny(&self, client: &str) -> Result<()> {
		self.call(
			Method::POST,
			routes::OWNER_DENY,
			Some(&DenyRequest {
				client: client.to_owned(),
			}),
		)
		.await
	}

	/// Revoke a grant (by grant id, client id, or pairing code).
	pub async fn revoke(&self, id: &str) -> Result<Grant> {
		self.call(
			Method::POST,
			routes::OWNER_REVOKE,
			Some(&RevokeRequest { id: id.to_owned() }),
		)
		.await
	}

	/// Permanently delete data.
	pub async fn purge(&self, request: &PurgeRequest) -> Result<Purged> {
		self.call(Method::POST, routes::OWNER_PURGE, Some(request))
			.await
	}

	/// The audit log, with a chain check.
	pub async fn audit(&self, request: &AuditRequest) -> Result<Audit> {
		self.call(Method::POST, routes::OWNER_AUDIT, Some(request))
			.await
	}

	/// Daemon and capture status.
	pub async fn status(&self) -> Result<Status> {
		self.call::<(), _>(Method::GET, routes::OWNER_STATUS, None)
			.await
	}

	/// Register a custom stream.
	pub async fn register_stream(&self, spec: &StreamSpec) -> Result<StreamSpec> {
		self.call(Method::POST, routes::OWNER_STREAMS, Some(spec))
			.await
	}

	/// The derivation settings, as JSON.
	pub async fn derive_config(&self) -> Result<serde_json::Value> {
		self.call::<(), _>(Method::GET, routes::OWNER_DERIVE, None)
			.await
	}

	/// Replace the derivation settings.
	pub async fn set_derive_config(&self, config: &serde_json::Value) -> Result<serde_json::Value> {
		self.call(Method::POST, routes::OWNER_DERIVE, Some(config))
			.await
	}

	/// What this machine captures, as JSON.
	pub async fn capture_config(&self) -> Result<serde_json::Value> {
		self.call::<(), _>(Method::GET, routes::OWNER_CAPTURE, None)
			.await
	}

	/// Replace what this machine captures; running sources restart.
	pub async fn set_capture_config(
		&self,
		config: &serde_json::Value,
	) -> Result<serde_json::Value> {
		self.call(Method::POST, routes::OWNER_CAPTURE, Some(config))
			.await
	}

	/// Pause capture, optionally for a number of minutes, or resume it.
	pub async fn pause_capture(&self, request: &quant_api::PauseRequest) -> Result<Status> {
		self.call(Method::POST, routes::OWNER_CAPTURE_PAUSE, Some(request))
			.await
	}

	/// Save a moment from the rolling audio buffer.
	pub async fn save_audio(&self, request: &SaveAudio) -> Result<SavedClip> {
		self.call(Method::POST, routes::OWNER_AUDIO_SAVE, Some(request))
			.await
	}

	/// End the session.
	pub async fn lock(self) -> Result<()> {
		self.call::<(), _>(Method::POST, routes::OWNER_LOCK, None)
			.await
	}
}

/// The default client id type, re-exported for callers.
pub type Id = ClientId;
