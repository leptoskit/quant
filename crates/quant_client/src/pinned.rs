//! Accept exactly one certificate: the daemon's, by SHA-256 fingerprint.

use std::sync::Arc;

use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use sha2::Digest;

#[derive(Debug)]
struct Pinned {
	fingerprint: [u8; 32],
	provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		_intermediates: &[CertificateDer<'_>],
		_server_name: &ServerName<'_>,
		_ocsp: &[u8],
		_now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		let seen: [u8; 32] = sha2::Sha256::digest(end_entity.as_ref()).into();
		if seen == self.fingerprint {
			Ok(ServerCertVerified::assertion())
		} else {
			Err(rustls::Error::General(
				"the daemon's certificate does not match the paired fingerprint".into(),
			))
		}
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls12_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls13_signature(
			message,
			cert,
			dss,
			&self.provider.signature_verification_algorithms,
		)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.provider
			.signature_verification_algorithms
			.supported_schemes()
	}
}

/// A TLS connector that trusts only the certificate with `fingerprint`.
pub(crate) fn connector(
	fingerprint: [u8; 32],
) -> Result<tokio_rustls::TlsConnector, rustls::Error> {
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
		.with_safe_default_protocol_versions()?
		.dangerous()
		.with_custom_certificate_verifier(Arc::new(Pinned {
			fingerprint,
			provider,
		}))
		.with_no_client_auth();
	Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// Parse a hex fingerprint.
pub fn parse_fingerprint(hex: &str) -> Option<[u8; 32]> {
	let cleaned: String = hex.chars().filter(|c| *c != ':').collect();
	hex::decode(cleaned).ok()?.try_into().ok()
}
