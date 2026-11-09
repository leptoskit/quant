//! Talk to a Quant daemon.
//!
//! A [`Client`] holds an [`Identity`]: a client id and a private signing key
//! that never leaves this machine. Every request is signed with it. Pair a
//! new identity with [`Client::pair`], have the owner approve it, then query
//! within the grant they chose. Owner actions go through an [`OwnerSession`]
//! opened with the passphrase, or with an enrolled owner device whose
//! hardware key signs the unlock challenge.

mod identity;
mod pinned;
pub use pinned::parse_fingerprint;
pub mod when;

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
use quant_api::ClaimActionRequest;
use quant_api::ClaimCorrectRequest;
use quant_api::ClaimListRequest;
use quant_api::ClientKind;
use quant_api::ClientView;
use quant_api::Context;
use quant_api::ContextRequest;
use quant_api::DenyRequest;
use quant_api::DeviceUnlockRequest;
use quant_api::Durations;
use quant_api::DurationsRequest;
use quant_api::EnrolDeviceRequest;
use quant_api::Hello;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_api::Ingested;
use quant_api::Me;
use quant_api::Overview;
use quant_api::OverviewRequest;
use quant_api::OwnerChallenge;
use quant_api::OwnerClaimRequest;
use quant_api::OwnerDeviceView;
use quant_api::PairRequest;
use quant_api::PairResponse;
use quant_api::ProfileClaims;
use quant_api::ProposeRequest;
use quant_api::Proposed;
use quant_api::PurgeRequest;
use quant_api::Purged;
use quant_api::Records;
use quant_api::RecordsRequest;
use quant_api::RenameClip;
use quant_api::RevokeDeviceRequest;
use quant_api::RevokeRequest;
use quant_api::SaveAudio;
use quant_api::SavedClip;
use quant_api::Series;
use quant_api::SeriesRequest;
use quant_api::Status;
use quant_api::StreamAccess;
use quant_api::UnlockRequest;
use quant_api::routes;
use quant_core::Claim;
use quant_core::ClientId;
use quant_core::MediaRef;
use quant_core::OwnerDeviceId;
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
	/// Through a relay: TLS to the daemon inside a WebSocket, with the same
	/// pinned fingerprint. When `address` is set it is tried first, with a
	/// short connect timeout, so clients on the LAN never pay the relay's
	/// latency.
	Relay {
		/// The relay, `wss://host[:port]`.
		url: String,
		/// The daemon's rendezvous id.
		rid: String,
		/// The pinned certificate fingerprint.
		fingerprint: [u8; 32],
		/// A direct `host:port` to try first.
		address: Option<String>,
	},
}

/// How long to wait for the direct address before falling back to the relay.
const DIRECT_CONNECT: std::time::Duration = std::time::Duration::from_secs(2);

impl Endpoint {
	/// Parse `unix:/path`, `tcp://host:port`, `tls://host:port#<hex
	/// fingerprint>`, `relay://host[:port]/<rid>#<hex fingerprint>`, a
	/// `quant://link?…` pairing link, a bare path, or a bare `host:port`.
	pub fn parse(s: &str) -> Result<Self, String> {
		if let Some(rest) = s.strip_prefix("quant://link") {
			return parse_link(s, rest.starts_with('?') || rest.is_empty());
		}
		if let Some(rest) = s.strip_prefix("relay://") {
			let (base, fingerprint) = rest
				.split_once('#')
				.ok_or("a relay:// address needs #<certificate fingerprint>")?;
			let fingerprint =
				parse_fingerprint(fingerprint).ok_or("bad certificate fingerprint")?;
			let (authority, rid) = base
				.rsplit_once('/')
				.ok_or("a relay:// address needs /<rendezvous id>")?;
			let url = quant_relay::RelayUrl::parse(&format!("wss://{authority}"))
				.map_err(|error| error.to_string())?;
			return Ok(Endpoint::Relay {
				url: url.to_string(),
				rid: rid.to_owned(),
				fingerprint,
				address: None,
			});
		}
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

/// A `quant://link?…` pairing link, as `quant link` prints it.
fn parse_link(text: &str, shaped: bool) -> Result<Endpoint, String> {
	if !shaped {
		return Err("not a quant:// link".into());
	}
	let link = quant_relay::link::Link::parse(text).map_err(|error| error.to_string())?;
	let fingerprint = parse_fingerprint(
		link.fp
			.as_deref()
			.ok_or("the link has no fp=<fingerprint> parameter")?,
	)
	.ok_or("bad certificate fingerprint")?;
	if link.has_relay() {
		return Ok(Endpoint::Relay {
			url: link.relay.expect("has_relay checked"),
			rid: link.rid.expect("has_relay checked"),
			fingerprint,
			address: link.addr,
		});
	}
	let address = link
		.addr
		.ok_or("the link has no addr=<host:port> parameter")?;
	Ok(Endpoint::Tls {
		address,
		fingerprint,
	})
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

// Aborting a timed-out upload must also release the detached HTTP driver's
// request buffers and socket, rather than leaving it alive across retries.
struct ConnectionTask(tokio::task::JoinHandle<Result<(), hyper::Error>>);

impl Drop for ConnectionTask {
	fn drop(&mut self) {
		self.0.abort();
	}
}

async fn send_typed(
	endpoint: &Endpoint,
	request: Request<Full<Bytes>>,
) -> Result<(StatusCode, Option<String>, Bytes)> {
	let shown = match endpoint {
		Endpoint::Unix(path) => path.display().to_string(),
		Endpoint::Tcp(address) | Endpoint::Tls { address, .. } => address.clone(),
		Endpoint::Relay { url, .. } => url.clone(),
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
			let _connection = ConnectionTask(tokio::spawn(connection));
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
		Endpoint::Relay {
			url,
			rid,
			fingerprint,
			address,
		} => {
			// The direct address first, when the link carried one and this
			// network can reach it, then the relay.
			if let Some(address) = address
				&& let Ok(tcp) =
					tokio::time::timeout(DIRECT_CONNECT, tokio::net::TcpStream::connect(address))
						.await
			{
				let tcp = tcp.map_err(unreachable)?;
				let connector = pinned::connector(*fingerprint)
					.map_err(|e| ClientError::Http(e.to_string()))?;
				let name =
					rustls::pki_types::ServerName::try_from("quantd.local").expect("valid name");
				if let Ok(tls) = connector.connect(name, tcp).await {
					return exchange!(tls);
				}
			}
			let url =
				quant_relay::RelayUrl::parse(url).map_err(|e| ClientError::Http(e.to_string()))?;
			let socket = quant_relay::dial::dial(&url.connect(rid))
				.await
				.map_err(unreachable)?;
			let stream = quant_relay::dial::WsStream::new(socket);
			let connector =
				pinned::connector(*fingerprint).map_err(|e| ClientError::Http(e.to_string()))?;
			let name = rustls::pki_types::ServerName::try_from("quantd.local").expect("valid name");
			let tls = connector.connect(name, stream).await.map_err(unreachable)?;
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

	/// The owner's profile claims, shaped by this grant's `memory.profile`
	/// level.
	pub async fn profile(&self) -> Result<ProfileClaims> {
		self.signed::<(), _>(Method::GET, routes::MEMORY_PROFILE, None)
			.await
	}

	/// Propose claims about the owner (needs `memory.profile:write`).
	pub async fn propose(&self, request: &ProposeRequest) -> Result<Proposed> {
		self.signed(Method::POST, routes::MEMORY_PROPOSALS, Some(request))
			.await
	}

	/// A budgeted context packet assembled for one task.
	pub async fn context(&self, request: &ContextRequest) -> Result<Context> {
		self.signed(Method::POST, routes::CONTEXT, Some(request))
			.await
	}

	/// Open an owner session with the passphrase.
	pub async fn unlock(&self, passphrase: &str) -> Result<OwnerSession> {
		OwnerSession::open(self.endpoint.clone(), passphrase).await
	}

	/// A challenge for an owner device's key to sign, for
	/// [`OwnerSession::open_with_device`]. Unauthenticated: a challenge is
	/// 32 random bytes that mean nothing until a valid signature answers it.
	pub async fn owner_challenge(&self) -> Result<OwnerChallenge> {
		let request = builder(Method::GET, routes::OWNER_CHALLENGE)
			.body(Full::new(Bytes::new()))
			.map_err(|e| ClientError::Http(e.to_string()))?;
		let (status, body) = send(&self.endpoint, request).await?;
		decode(status, &body)
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

	/// Open a session at `endpoint` with an enrolled owner device: fetch the
	/// vault's id and a challenge, have `signer` sign the unlock message with
	/// the device's hardware key (ECDSA P-256/SHA-256, DER or raw `r‖s`), and
	/// post the signature. The signer runs where the private key lives; it
	/// never leaves it.
	pub async fn open_with_device(
		endpoint: Endpoint,
		device: OwnerDeviceId,
		signer: impl FnOnce(&[u8]) -> Vec<u8>,
	) -> Result<Self> {
		let hello: Hello = {
			let request = builder(Method::GET, routes::HELLO)
				.body(Full::new(Bytes::new()))
				.map_err(|e| ClientError::Http(e.to_string()))?;
			let (status, body) = send(&endpoint, request).await?;
			decode(status, &body)?
		};
		let challenge: OwnerChallenge = {
			let request = builder(Method::GET, routes::OWNER_CHALLENGE)
				.body(Full::new(Bytes::new()))
				.map_err(|e| ClientError::Http(e.to_string()))?;
			let (status, body) = send(&endpoint, request).await?;
			decode(status, &body)?
		};
		let message = quant_api::owner_unlock_message(&hello.vault_id, &challenge.challenge);
		let signature = signer(&message);
		Self::open_with_device_signature(
			endpoint,
			device,
			challenge.challenge,
			URL_SAFE_NO_PAD.encode(signature),
		)
		.await
	}

	/// Open one with a signature already computed over
	/// [`quant_api::owner_unlock_message`] for this vault and challenge.
	pub async fn open_with_device_signature(
		endpoint: Endpoint,
		device: OwnerDeviceId,
		challenge: String,
		signature: String,
	) -> Result<Self> {
		let body = serde_json::to_vec(&DeviceUnlockRequest {
			device,
			challenge,
			signature,
		})?;
		let request = builder(Method::POST, routes::OWNER_UNLOCK_DEVICE)
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

	/// The relay settings: the relay in use, if any, and this daemon's
	/// rendezvous id on it.
	pub async fn relay(&self) -> Result<quant_api::RelayState> {
		self.call::<(), _>(Method::GET, routes::OWNER_RELAY, None)
			.await
	}

	/// Set a relay (with an optional invite), or clear it with
	/// `url: None`.
	pub async fn set_relay(
		&self,
		request: &quant_api::RelaySetRequest,
	) -> Result<quant_api::RelayState> {
		self.call(Method::POST, routes::OWNER_RELAY, Some(request))
			.await
	}

	/// The on-device learning settings, as JSON.
	pub async fn learn_config(&self) -> Result<serde_json::Value> {
		self.call::<(), _>(Method::GET, routes::OWNER_LEARN, None)
			.await
	}

	/// Replace the on-device learning settings. Endpoints must be literal
	/// loopback addresses; anything else is refused.
	pub async fn set_learn_config(&self, config: &serde_json::Value) -> Result<serde_json::Value> {
		self.call(Method::POST, routes::OWNER_LEARN, Some(config))
			.await
	}

	/// Turn on-device learning off.
	pub async fn learn_off(&self) -> Result<()> {
		let path = format!("{}/off", routes::OWNER_LEARN);
		self.call::<(), _>(Method::POST, &path, None).await
	}

	/// Save a moment from the rolling audio buffer.
	pub async fn save_audio(&self, request: &SaveAudio) -> Result<SavedClip> {
		self.call(Method::POST, routes::OWNER_AUDIO_SAVE, Some(request))
			.await
	}

	/// State a claim about yourself.
	pub async fn tell(&self, request: &OwnerClaimRequest) -> Result<Claim> {
		self.call(Method::POST, routes::OWNER_CLAIMS, Some(request))
			.await
	}

	/// List claims, optionally filtered.
	pub async fn claims(&self, request: &ClaimListRequest) -> Result<Vec<Claim>> {
		self.call(Method::POST, routes::OWNER_CLAIMS_LIST, Some(request))
			.await
	}

	/// Approve a held proposal; it becomes active, still inferred.
	pub async fn claim_approve(&self, id: quant_core::ClaimId) -> Result<Claim> {
		self.call(
			Method::POST,
			routes::OWNER_CLAIM_APPROVE,
			Some(&ClaimActionRequest { id }),
		)
		.await
	}

	/// Refuse a held proposal.
	pub async fn claim_reject(&self, id: quant_core::ClaimId) -> Result<Claim> {
		self.call(
			Method::POST,
			routes::OWNER_CLAIM_REJECT,
			Some(&ClaimActionRequest { id }),
		)
		.await
	}

	/// Withdraw an active or held claim.
	pub async fn claim_retract(&self, id: quant_core::ClaimId) -> Result<Claim> {
		self.call(
			Method::POST,
			routes::OWNER_CLAIM_RETRACT,
			Some(&ClaimActionRequest { id }),
		)
		.await
	}

	/// Correct a claim with a new owner claim that supersedes it.
	pub async fn claim_correct(&self, request: &ClaimCorrectRequest) -> Result<Claim> {
		self.call(Method::POST, routes::OWNER_CLAIM_CORRECT, Some(request))
			.await
	}

	/// Every enrolled owner device.
	pub async fn owner_devices(&self) -> Result<Vec<OwnerDeviceView>> {
		self.call::<(), _>(Method::GET, routes::OWNER_DEVICES, None)
			.await
	}

	/// Enrol an owner device with the passphrase session this was opened
	/// with. A session opened with a device cannot enrol others.
	pub async fn owner_device_add(&self, request: &EnrolDeviceRequest) -> Result<OwnerDeviceView> {
		self.call(Method::POST, routes::OWNER_DEVICES, Some(request))
			.await
	}

	/// Revoke an owner device; its key stops working at once.
	pub async fn owner_device_revoke(&self, id: OwnerDeviceId) -> Result<OwnerDeviceView> {
		self.call(
			Method::POST,
			routes::OWNER_DEVICE_REVOKE,
			Some(&RevokeDeviceRequest { id }),
		)
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
