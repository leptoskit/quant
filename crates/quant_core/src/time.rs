use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;

use crate::CoreError;

/// A half-open span of time, `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
	/// Inclusive start.
	pub start: Timestamp,
	/// Exclusive end.
	pub end: Timestamp,
}

impl TimeRange {
	/// A range, rejecting one that ends before it starts.
	pub fn new(start: Timestamp, end: Timestamp) -> Result<Self, CoreError> {
		if end < start {
			return Err(CoreError::InvertedRange);
		}
		Ok(Self { start, end })
	}

	/// The range covering the last `span` up to `now`.
	pub fn last(span: jiff::SignedDuration, now: Timestamp) -> Self {
		let start = now.checked_sub(span).unwrap_or(Timestamp::MIN);
		Self { start, end: now }
	}

	/// Whether `t` falls inside the range.
	pub fn contains(&self, t: Timestamp) -> bool {
		self.start <= t && t < self.end
	}

	/// This range clipped so it starts no earlier than `floor`.
	#[must_use]
	pub fn clamp_start(self, floor: Timestamp) -> Self {
		let start = self.start.max(floor);
		Self {
			start,
			end: self.end.max(start),
		}
	}

	/// The length of the range.
	pub fn duration(&self) -> jiff::SignedDuration {
		self.end.duration_since(self.start)
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;

	use super::*;

	#[test]
	fn rejects_inverted_ranges() {
		let now = Timestamp::now();
		let earlier = now - SignedDuration::from_secs(5);
		assert_eq!(TimeRange::new(now, earlier), Err(CoreError::InvertedRange));
	}

	#[test]
	fn clamping_never_inverts() {
		let now = Timestamp::now();
		let range = TimeRange::last(SignedDuration::from_hours(48), now);
		let floor = now + SignedDuration::from_secs(10);
		let clamped = range.clamp_start(floor);
		assert!(clamped.start <= clamped.end);
		assert_eq!(clamped.duration(), SignedDuration::ZERO);
	}
}
