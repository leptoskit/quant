//! The wire messages between a daemon, a client, and a relay.
//!
//! Control frames are JSON text; the bytes a relay splices are binary frames
//! it never looks into. A daemon proves it owns its rendezvous id by signing
//! the relay's challenge over a fixed domain string that names the relay host
//! (and, for accepts, the connection), so a proof for one relay or one
//! connection is worthless on another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signature;
use quant_access::auth::PublicKey;
use serde::Deserialize;
use serde::Serialize;

use crate::key::RelayKey;
use crate::key::signed_bytes;
use crate::key::verify;

/// The domain string a listen proof starts with.
pub const LISTEN_DOMAIN: &str = "quant relay listen v1";
/// The domain string an accept proof starts with.
pub const ACCEPT_DOMAIN: &str = "quant relay accept v1";

/// The relay's challenge: 32 random bytes, base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
	/// The bytes to sign.
	pub challenge: String,
}

/// The daemon's answer to a listen challenge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenAnswer {
	/// The rendezvous key's public half.
	pub public_key: String,
	/// The signature over [`LISTEN_DOMAIN`], the relay host, and the
	/// challenge.
	pub signature: String,
	/// An invite code, when the relay asks daemons to present one.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub invite: Option<String>,
}

/// The relay's answer to an accept challenge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptAnswer {
	/// The rendezvous key's public half.
	pub public_key: String,
	/// The signature over [`ACCEPT_DOMAIN`], the relay host, the connection
	/// id, and the challenge.
	pub signature: String,
}

/// The relay telling a daemon a client is waiting: `{"open": <conn>}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenNotice {
	/// The connection id to accept.
	pub open: String,
}

/// The bytes a listen proof covers.
pub fn listen_message(host: &str, challenge: &[u8]) -> Vec<u8> {
	signed_bytes(LISTEN_DOMAIN, &[host.as_bytes(), challenge])
}

/// The bytes an accept proof covers.
pub fn accept_message(host: &str, conn: &str, challenge: &[u8]) -> Vec<u8> {
	signed_bytes(
		ACCEPT_DOMAIN,
		&[host.as_bytes(), conn.as_bytes(), challenge],
	)
}

impl RelayKey {
	/// Answer a listen `challenge` from `host`.
	pub fn sign_listen(&self, host: &str, challenge: &[u8]) -> ListenAnswer {
		let signature = self.sign(&listen_message(host, challenge));
		ListenAnswer {
			public_key: self.public().to_string(),
			signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
			invite: None,
		}
	}

	/// Answer an accept `challenge` from `host` for `conn`.
	pub fn sign_accept(&self, host: &str, conn: &str, challenge: &[u8]) -> AcceptAnswer {
		let signature = self.sign(&accept_message(host, conn, challenge));
		AcceptAnswer {
			public_key: self.public().to_string(),
			signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
		}
	}
}

/// Decode a challenge's base64.
pub fn challenge_bytes(text: &str) -> Option<Vec<u8>> {
	URL_SAFE_NO_PAD.decode(text).ok()
}

/// Check a listen answer: the signature must cover this host and challenge,
/// made by the key in the answer. Returns the key it verified.
pub fn verify_listen(host: &str, challenge: &[u8], answer: &ListenAnswer) -> Option<PublicKey> {
	let key = answer.public_key.parse().ok()?;
	let signature = URL_SAFE_NO_PAD.decode(&answer.signature).ok()?;
	verify(&key, &listen_message(host, challenge), &signature).then_some(key)
}

/// Check an accept answer: the signature must cover this host, connection,
/// and challenge, made by `key`.
pub fn verify_accept(
	host: &str,
	conn: &str,
	challenge: &[u8],
	key: &PublicKey,
	answer: &AcceptAnswer,
) -> bool {
	let Some(signature) = URL_SAFE_NO_PAD
		.decode(&answer.signature)
		.ok()
		.and_then(|bytes| Signature::from_slice(&bytes).ok())
	else {
		return false;
	};
	verify(
		key,
		&accept_message(host, conn, challenge),
		&signature.to_bytes(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn challenge() -> Vec<u8> {
		vec![7; 32]
	}

	fn encoded(bytes: &[u8]) -> String {
		URL_SAFE_NO_PAD.encode(bytes)
	}

	#[test]
	fn listen_answers_verify_against_the_same_host_and_challenge() {
		let key = RelayKey::generate();
		let answer = key.sign_listen("relay.example", &challenge());
		let public = key.public();
		assert_eq!(
			verify_listen("relay.example", &challenge(), &answer),
			Some(public)
		);
	}

	#[test]
	fn listen_answers_fail_for_another_host_or_challenge() {
		let key = RelayKey::generate();
		let answer = key.sign_listen("relay.example", &challenge());
		let other = vec![8; 32];
		assert_eq!(verify_listen("evil.example", &challenge(), &answer), None);
		assert_eq!(verify_listen("relay.example", &other, &answer), None);
		assert_eq!(
			verify_listen(
				"relay.example",
				&challenge(),
				&ListenAnswer {
					public_key: answer.public_key.clone(),
					signature: encoded(&[0; 64]),
					invite: None,
				}
			),
			None
		);
	}

	#[test]
	fn accept_answers_cover_the_connection() {
		let key = RelayKey::generate();
		let public = key.public();
		let answer = key.sign_accept("relay.example", "conn1", &challenge());
		assert!(verify_accept(
			"relay.example",
			"conn1",
			&challenge(),
			&public,
			&answer
		));
		assert!(!verify_accept(
			"relay.example",
			"conn2",
			&challenge(),
			&public,
			&answer
		));
		assert!(!verify_accept(
			"evil.example",
			"conn1",
			&challenge(),
			&public,
			&answer
		));

		let other = RelayKey::generate();
		assert!(!verify_accept(
			"relay.example",
			"conn1",
			&challenge(),
			&other.public(),
			&answer
		));
	}

	#[test]
	fn messages_round_trip_as_json() {
		let notice = OpenNotice {
			open: "conn1".into(),
		};
		assert_eq!(
			serde_json::to_string(&notice).unwrap(),
			r#"{"open":"conn1"}"#
		);
		let back: OpenNotice = serde_json::from_str(r#"{"open":"conn1"}"#).unwrap();
		assert_eq!(back, notice);

		let answer = ListenAnswer {
			public_key: "key".into(),
			signature: "sig".into(),
			invite: Some("invite".into()),
		};
		let text = serde_json::to_string(&answer).unwrap();
		assert!(text.contains(r#""invite":"invite""#));
		let back: ListenAnswer = serde_json::from_str(&text).unwrap();
		assert_eq!(back, answer);
		let without_invite: ListenAnswer =
			serde_json::from_str(r#"{"public_key":"k","signature":"s"}"#).unwrap();
		assert_eq!(without_invite.invite, None);
	}

	#[test]
	fn a_flip_challenge_round_trips_through_base64() {
		let bytes = challenge();
		let encoded = URL_SAFE_NO_PAD.encode(&bytes);
		assert_eq!(challenge_bytes(&encoded).unwrap(), bytes);
		assert!(challenge_bytes("!not base64!").is_none());
	}
}
