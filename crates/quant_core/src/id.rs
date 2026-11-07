use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

macro_rules! uuid_id {
	($(#[$meta:meta])* $name:ident, $prefix:literal) => {
		$(#[$meta])*
		#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
		#[serde(transparent)]
		pub struct $name(pub Uuid);

		impl $name {
			/// A new, time-ordered identifier.
			pub fn new() -> Self {
				Self(Uuid::now_v7())
			}

			/// The prefix used when this id is shown to people.
			pub const PREFIX: &'static str = $prefix;
		}

		impl Default for $name {
			fn default() -> Self {
				Self::new()
			}
		}

		impl fmt::Display for $name {
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				write!(f, "{}_{}", $prefix, self.0.simple())
			}
		}

		impl FromStr for $name {
			type Err = uuid::Error;

			fn from_str(s: &str) -> Result<Self, Self::Err> {
				let raw = s.strip_prefix(concat!($prefix, "_")).unwrap_or(s);
				Uuid::parse_str(raw).map(Self)
			}
		}
	};
}

uuid_id!(
	/// Identifies one record.
	RecordId,
	"rec"
);
uuid_id!(
	/// Identifies a capturing device: a laptop, a phone, a watch bridge.
	DeviceId,
	"dev"
);
uuid_id!(
	/// Identifies a client (an agent, the CLI, a device) that talks to the
	/// vault.
	ClientId,
	"cli"
);
uuid_id!(
	/// Identifies an access grant.
	GrantId,
	"grt"
);
uuid_id!(
	/// Identifies one claim about the owner.
	ClaimId,
	"clm"
);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ids_round_trip_with_and_without_prefix() {
		let id = RecordId::new();
		let shown = id.to_string();
		assert!(shown.starts_with("rec_"));
		assert_eq!(shown.parse::<RecordId>().unwrap(), id);
		assert_eq!(id.0.to_string().parse::<RecordId>().unwrap(), id);
	}

	#[test]
	fn ids_are_time_ordered() {
		let a = RecordId::new();
		let b = RecordId::new();
		assert!(a < b);
	}
}
