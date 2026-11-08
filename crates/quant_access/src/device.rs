//! Owner devices: hardware keys that open owner sessions after a
//! biometric check.

use serde::Deserialize;
use serde::Serialize;

/// How an owner device's key is protected, as the device reported at
/// enrolment. Informational: the daemon records the hint and enforces
/// nothing, because the key itself lives in the device's secure hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDevicePolicy {
	/// The key signs only after a biometric check: Touch ID or a
	/// fingerprint.
	Biometry,
	/// The key signs after a biometric check or the device's passcode.
	BiometryOrPasscode,
}

impl OwnerDevicePolicy {
	/// The policy as it is stored and shown.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Biometry => "biometry",
			Self::BiometryOrPasscode => "biometry_or_passcode",
		}
	}

	/// Parse a policy from its stored text.
	pub fn parse(text: &str) -> Option<Self> {
		match text {
			"biometry" => Some(Self::Biometry),
			"biometry_or_passcode" => Some(Self::BiometryOrPasscode),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn policies_round_trip() {
		for policy in [
			OwnerDevicePolicy::Biometry,
			OwnerDevicePolicy::BiometryOrPasscode,
		] {
			assert_eq!(OwnerDevicePolicy::parse(policy.as_str()), Some(policy));
			let json = serde_json::to_string(&policy).unwrap();
			let back: OwnerDevicePolicy = serde_json::from_str(&json).unwrap();
			assert_eq!(back, policy);
		}
		assert_eq!(OwnerDevicePolicy::parse("passcode"), None);
	}
}
