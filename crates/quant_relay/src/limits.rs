//! What a relay allows one rendezvous id, and one connection.
//!
//! The defaults keep one vault from starving the rest. A public relay can
//! tighten or loosen them through environment variables; `apps/relay`
//! documents them.

use std::time::Duration;

/// The limits a relay enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
	/// Concurrent spliced connections per rendezvous id.
	pub connections: usize,
	/// Bytes per rendezvous id per UTC day, both directions counted.
	pub bytes_per_day: u64,
	/// Largest binary frame the relay will carry.
	pub max_frame: usize,
	/// How long a spliced connection may carry no traffic before the relay
	/// closes it.
	pub idle_timeout: Duration,
	/// How long a client's connect waits for the daemon to accept.
	pub accept_timeout: Duration,
	/// Connects waiting for their daemon per rendezvous id.
	pub pending_connects: usize,
}

impl Default for Limits {
	fn default() -> Self {
		Self {
			connections: 16,
			bytes_per_day: 2 * 1024 * 1024 * 1024,
			max_frame: 1024 * 1024,
			idle_timeout: Duration::from_mins(5),
			accept_timeout: Duration::from_secs(10),
			pending_connects: 64,
		}
	}
}

/// The UTC day a timestamp falls in, as `YYYY-MM-DD`: the key usage is
/// counted under.
pub fn day(timestamp: jiff::Timestamp) -> String {
	timestamp
		.to_zoned(jiff::tz::TimeZone::UTC)
		.date()
		.to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_defaults_are_the_documented_ones() {
		let limits = Limits::default();
		assert_eq!(limits.connections, 16);
		assert_eq!(limits.bytes_per_day, 2 * 1024 * 1024 * 1024);
		assert_eq!(limits.max_frame, 1024 * 1024);
		assert_eq!(limits.idle_timeout, Duration::from_mins(5));
		assert_eq!(limits.accept_timeout, Duration::from_secs(10));
		assert_eq!(limits.pending_connects, 64);
	}

	#[test]
	fn days_are_utc_dates() {
		let day = day(jiff::Timestamp::from_second(1_762_489_600).unwrap());
		assert_eq!(day, "2025-11-07");
	}
}
