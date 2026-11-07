//! Derived memory.
//!
//! Raw streams answer "what was the accelerometer doing at 14:02"; people
//! ask "was I lying down all afternoon?". This crate turns one into the
//! other, writing `motion.posture` spans, `motion.gait` bouts,
//! `memory.episode` hours, and
//! `memory.daily` summaries back into the vault. Derived records get stable
//! ids, so recomputing replaces them, and each names the streams it came
//! from.

pub mod daily;
pub mod episode;
pub mod gait;
pub mod posture;
pub mod weekly;
mod run;

use quant_core::RecordId;
pub use run::Config;
pub use run::Report;
pub use run::run;
use uuid::Uuid;

/// A stable record id for a derived record, from what it summarises.
pub fn stable_id(parts: &[&str]) -> RecordId {
	let mut hasher = blake3::Hasher::new_derive_key("quant derived record id v1");
	for part in parts {
		hasher.update(part.as_bytes());
		hasher.update(&[0]);
	}
	let bytes: [u8; 16] = hasher.finalize().as_bytes()[..16]
		.try_into()
		.expect("16 bytes");
	RecordId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// The nil device, used for records Quant derives itself.
pub const DERIVED_DEVICE: quant_core::DeviceId = quant_core::DeviceId(Uuid::nil());

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stable_ids_are_stable_and_distinct() {
		assert_eq!(stable_id(&["a", "b"]), stable_id(&["a", "b"]));
		assert_ne!(stable_id(&["a", "b"]), stable_id(&["ab"]));
	}
}
