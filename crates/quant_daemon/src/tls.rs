//! TLS for the network listener.
//!
//! Phones and sensor bridges reach the daemon over the local network. Every
//! request is already signed, so TLS here is for confidentiality: the daemon
//! makes a self-signed certificate once, keeps it in the vault, and devices
//! pin its SHA-256 fingerprint when they pair (it travels in the pairing
//! link), so no certificate authority is involved and none can be abused.

use std::net::SocketAddr;
use std::sync::Arc;

use quant_store::Store;
use quant_store::StoreError;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;
use sha2::Digest;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// The daemon's certificate and key.
pub struct TlsIdentity {
	cert: Vec<u8>,
	key: Vec<u8>,
}

/// Hex SHA-256 of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> String {
	hex::encode(sha2::Sha256::digest(cert_der))
}

impl TlsIdentity {
	/// Load the vault's certificate, making one the first time.
	pub fn load_or_create(store: &Store) -> Result<Self, StoreError> {
		if let (Some(cert), Some(key)) = (store.meta("tls.cert")?, store.meta("tls.key")?) {
			let cert = hex::decode(cert).map_err(|_| StoreError::Corrupt("tls cert"))?;
			let key = hex::decode(key).map_err(|_| StoreError::Corrupt("tls key"))?;
			return Ok(Self { cert, key });
		}
		let generated = rcgen::generate_simple_self_signed(vec!["quantd.local".to_owned()])
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
		let identity = Self {
			cert: generated.cert.der().to_vec(),
			key: generated.signing_key.serialize_der(),
		};
		store.set_meta("tls.key", &hex::encode(&identity.key))?;
		store.set_meta("tls.cert", &hex::encode(&identity.cert))?;
		Ok(identity)
	}

	/// The fingerprint devices pin.
	pub fn fingerprint(&self) -> String {
		fingerprint(&self.cert)
	}

	/// The TLS acceptor, also used for relay connections, which arrive as
	/// ordinary byte streams inside a WebSocket.
	pub fn acceptor(&self) -> Result<TlsAcceptor, rustls::Error> {
		let provider = Arc::new(rustls::crypto::ring::default_provider());
		let config = rustls::ServerConfig::builder_with_provider(provider)
			.with_safe_default_protocol_versions()?
			.with_no_client_auth()
			.with_single_cert(
				vec![CertificateDer::from(self.cert.clone())],
				PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone())),
			)?;
		Ok(TlsAcceptor::from(Arc::new(config)))
	}
}

/// A TCP listener that completes a TLS handshake before handing a
/// connection to the server. Failed handshakes are dropped.
pub struct TlsListener {
	tcp: TcpListener,
	acceptor: TlsAcceptor,
}

impl TlsListener {
	/// Bind `address` with `identity`.
	pub async fn bind(address: SocketAddr, identity: &TlsIdentity) -> std::io::Result<Self> {
		let acceptor = identity.acceptor().map_err(std::io::Error::other)?;
		Ok(Self {
			tcp: TcpListener::bind(address).await?,
			acceptor,
		})
	}
}

impl axum::serve::Listener for TlsListener {
	type Addr = SocketAddr;
	type Io = TlsStream<TcpStream>;

	async fn accept(&mut self) -> (Self::Io, Self::Addr) {
		loop {
			let (stream, address) = match self.tcp.accept().await {
				Ok(accepted) => accepted,
				Err(error) => {
					tracing::warn!(%error, "accept failed");
					tokio::time::sleep(std::time::Duration::from_millis(100)).await;
					continue;
				}
			};
			let handshake = tokio::time::timeout(
				std::time::Duration::from_secs(10),
				self.acceptor.accept(stream),
			);
			match handshake.await {
				Ok(Ok(tls)) => return (tls, address),
				Ok(Err(error)) => tracing::debug!(%error, %address, "tls handshake failed"),
				Err(_) => tracing::debug!(%address, "tls handshake timed out"),
			}
		}
	}

	fn local_addr(&self) -> std::io::Result<Self::Addr> {
		self.tcp.local_addr()
	}
}
