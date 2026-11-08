//! Owner devices: hardware keys that open owner sessions by signing a
//! challenge after a biometric check, so the owner can act without typing
//! the passphrase. Enrolment still needs the passphrase — a device proves
//! the pocket, the passphrase proves the person.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use p256::ecdsa::Signature;
use p256::ecdsa::VerifyingKey;
use p256::ecdsa::signature::Verifier as _;
use quant_access::Denied;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_api::DeviceUnlockRequest;
use quant_api::EnrolDeviceRequest;
use quant_api::OwnerChallenge;
use quant_api::OwnerDeviceView;
use quant_api::OwnerSession;
use quant_api::RevokeDeviceRequest;
use quant_api::SessionSource;
use quant_api::owner_unlock_message;
use quant_store::OwnerDevice;

use crate::authn::Owner;
use crate::error::ApiResult;
use crate::error::Error;
use crate::owner::owner_audit;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// A short, comparable hash of an owner device's public key, shown in lists
/// and audit entries so two enrolled devices can be told apart at a glance.
fn fingerprint(public_key: &[u8]) -> String {
	hex::encode(&blake3::hash(public_key).as_bytes()[..8])
}

/// Decode and validate a device's public key: unpadded URL-safe base64, an
/// uncompressed SEC1 point (65 bytes, `04 ‖ X ‖ Y`) on the P-256 curve.
fn parse_public_key(text: &str) -> Result<Vec<u8>, Error> {
	let bytes = URL_SAFE_NO_PAD
		.decode(text)
		.map_err(|_| Error::Invalid("the public key is not base64".into()))?;
	if bytes.len() != 65 || bytes[0] != 0x04 {
		return Err(Error::Invalid(
			"the public key must be an uncompressed SEC1 P-256 point (65 bytes)".into(),
		));
	}
	VerifyingKey::from_sec1_bytes(&bytes)
		.map_err(|_| Error::Invalid("the public key is not a point on the P-256 curve".into()))?;
	Ok(bytes)
}

/// Verify a device's signature over `message`, accepting DER or raw `r‖s`,
/// both base64. Either `S` or `n−S` verifies, which is safe because every
/// challenge is consumed by the first attempt that presents it.
fn verify(public_key: &[u8], message: &[u8], signature: &str) -> bool {
	let Ok(key) = VerifyingKey::from_sec1_bytes(public_key) else {
		return false;
	};
	URL_SAFE_NO_PAD
		.decode(signature)
		.ok()
		.and_then(|bytes| {
			Signature::from_der(&bytes)
				.or_else(|_| Signature::from_slice(&bytes))
				.ok()
		})
		.is_some_and(|signature| key.verify(message, &signature).is_ok())
}

/// `GET /v1/owner/challenge`. Unauthenticated: the challenge is 32 random
/// bytes with no meaning until a valid signature answers it.
pub async fn challenge(AxumState(state): Shared) -> ApiResult<Json<OwnerChallenge>> {
	let (challenge, expires_at) = state.owner().issue_challenge(Timestamp::now());
	Ok(Json(OwnerChallenge {
		challenge: URL_SAFE_NO_PAD.encode(challenge),
		expires_at,
	}))
}

/// `POST /v1/owner/unlock/device`. Every failure — an unknown or revoked
/// device, a spent, unknown, or expired challenge, a bad signature, even a
/// challenge answered for another vault — answers exactly the same, so a
/// caller learns nothing about which check failed. The challenge is consumed
/// whatever happens, and failures share the passphrase unlock's backoff.
pub async fn unlock_device(
	AxumState(state): Shared,
	Json(body): Json<DeviceUnlockRequest>,
) -> ApiResult<Json<OwnerSession>> {
	let now = Timestamp::now();
	if let Some(wait) = state.owner().wait(now) {
		return Err(Error::Locked(wait));
	}
	let challenge_ok = URL_SAFE_NO_PAD
		.decode(&body.challenge)
		.ok()
		.and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
		.is_some_and(|challenge| state.owner().take_challenge(&challenge, now));
	let message = owner_unlock_message(&state.vault.manifest.vault_id, &body.challenge);
	let device = state
		.vault
		.store
		.owner_device(body.device)?
		.filter(|device| device.revoked_at.is_none())
		.filter(|device| verify(&device.public_key, &message, &body.signature));
	let device = device.filter(|_| challenge_ok);
	let Some(device) = device else {
		state.owner().fail(Timestamp::now());
		state.vault.store.audit(AuditEvent::new(
			Actor::Unknown {
				claimed: Some("owner".into()),
			},
			"owner.device.unlock",
			"",
			Outcome::Denied {
				reason: Denied::OutOfScope,
			},
		))?;
		return Err(Error::Unauthenticated("owner device unlock failed".into()));
	};
	state.vault.store.touch_owner_device(device.id)?;
	let (token, expires_at) = state.owner().open(SessionSource::Device, Timestamp::now());
	state.vault.store.audit(
		AuditEvent::new(
			Actor::Owner,
			"owner.device.unlock",
			device.id.to_string(),
			Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.detail(serde_json::json!({ "label": device.label })),
	)?;
	Ok(Json(OwnerSession { token, expires_at }))
}

fn view(device: &OwnerDevice) -> OwnerDeviceView {
	OwnerDeviceView {
		id: device.id,
		label: device.label.clone(),
		fingerprint: fingerprint(&device.public_key),
		policy: device.policy,
		created_at: device.created_at,
		last_used: device.last_used,
		revoked_at: device.revoked_at,
	}
}

/// `GET /v1/owner/devices`. Any owner session.
pub async fn devices(
	AxumState(state): Shared,
	_owner: Owner,
) -> ApiResult<Json<Vec<OwnerDeviceView>>> {
	Ok(Json(
		state
			.vault
			.store
			.owner_devices()?
			.iter()
			.map(view)
			.collect(),
	))
}

/// `POST /v1/owner/devices`. Enrolment needs a session opened with the
/// passphrase: a device session must never be able to gather more devices
/// under the owner's name.
pub async fn enrol(
	AxumState(state): Shared,
	owner: Owner,
	Json(body): Json<EnrolDeviceRequest>,
) -> ApiResult<Json<OwnerDeviceView>> {
	if owner.via != SessionSource::Passphrase {
		return Err(Error::Forbidden(Denied::PassphraseSession));
	}
	let label: String = body.label.chars().filter(|c| !c.is_control()).collect();
	if !(1..=64).contains(&label.chars().count()) {
		return Err(Error::Invalid(
			"a device label is 1 to 64 characters".into(),
		));
	}
	let public_key = parse_public_key(&body.public_key)?;
	let device = state
		.vault
		.store
		.add_owner_device(&label, &public_key, body.policy)?;
	owner_audit(
		&state,
		"owner.device.add",
		device.id.to_string(),
		serde_json::json!({
			"label": device.label,
			"fingerprint": fingerprint(&public_key),
		}),
	)?;
	Ok(Json(view(&device)))
}

/// `POST /v1/owner/devices/revoke`. Any owner session. A revoked device's
/// next unlock fails exactly like a wrong signature.
pub async fn revoke(
	AxumState(state): Shared,
	_owner: Owner,
	Json(body): Json<RevokeDeviceRequest>,
) -> ApiResult<Json<OwnerDeviceView>> {
	let device = state.vault.store.revoke_owner_device(body.id)?;
	owner_audit(
		&state,
		"owner.device.revoke",
		device.id.to_string(),
		serde_json::Value::Null,
	)?;
	Ok(Json(view(&device)))
}

#[cfg(test)]
mod tests {
	use p256::ecdsa::SigningKey;
	use p256::ecdsa::signature::Signer as _;

	use super::*;

	fn signing_key() -> SigningKey {
		let mut seed = [0u8; 32];
		getrandom::fill(&mut seed).expect("the OS random source is available");
		SigningKey::from_bytes((&seed).into()).expect("a random seed gives a valid key")
	}

	fn public_key(key: &SigningKey) -> Vec<u8> {
		key.verifying_key().to_sec1_bytes().to_vec()
	}

	fn encode(bytes: &[u8]) -> String {
		URL_SAFE_NO_PAD.encode(bytes)
	}

	#[test]
	fn signatures_verify_in_both_encodings_and_either_s() {
		let key = signing_key();
		let message = b"quant owner unlock v1\nvault\nchallenge";
		let signature: Signature = key.sign(message);

		let key_bytes = public_key(&key);
		assert!(verify(
			&key_bytes,
			message,
			&encode(signature.to_der().as_bytes())
		));
		assert!(verify(&key_bytes, message, &encode(&signature.to_bytes())));

		// Either S or n−S verifies, consistently.
		let flipped = Signature::from_scalars(signature.r(), -signature.s())
			.expect("negating a nonzero scalar stays nonzero");
		assert_ne!(flipped.to_bytes(), signature.to_bytes());
		assert!(verify(
			&key_bytes,
			message,
			&encode(flipped.to_der().as_bytes())
		));

		// Wrong key, wrong message, and junk all fail.
		let other = signing_key();
		assert!(!verify(
			&public_key(&other),
			message,
			&encode(&signature.to_bytes())
		));
		assert!(!verify(
			&key_bytes,
			b"quant owner unlock v1\nother\nchallenge",
			&encode(&signature.to_bytes())
		));
		assert!(!verify(&key_bytes, message, "not base64!"));
		assert!(!verify(&key_bytes, message, &encode(b"too short")));
	}

	#[test]
	fn public_keys_must_be_uncompressed_sec1_points() {
		let key = signing_key();
		let good = encode(&public_key(&key));
		assert!(parse_public_key(&good).is_ok());
		// A compressed point, a wrong length, and junk are all refused.
		let compressed = key.verifying_key().to_encoded_point(true);
		assert!(parse_public_key(&encode(compressed.as_bytes())).is_err());
		assert!(parse_public_key(&encode(&[0x04; 33])).is_err());
		assert!(parse_public_key("!!!!").is_err());
		// A point-shaped but off-curve key is refused too.
		let mut off_curve = public_key(&key);
		let last = off_curve.len() - 1;
		off_curve[last] ^= 0xff;
		assert!(parse_public_key(&encode(&off_curve)).is_err());
	}

	#[test]
	fn fingerprints_are_stable_and_distinct() {
		let a = signing_key();
		let b = signing_key();
		let fa = fingerprint(&public_key(&a));
		assert_eq!(fa, fingerprint(&public_key(&a)));
		assert_ne!(fa, fingerprint(&public_key(&b)));
		assert_eq!(fa.len(), 16);
	}
}
