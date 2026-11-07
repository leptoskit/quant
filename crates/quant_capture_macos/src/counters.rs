//! Turning the system's cumulative event counters into per-interval counts.

/// More events than this in one interval means the counter was reset rather
/// than that the user typed for days without pause.
const MAX_PLAUSIBLE_DELTA: u32 = 1 << 24;

/// Events counted between two readings of a cumulative `u32` counter.
///
/// The counter wraps at `u32::MAX`, which wrapping subtraction handles. A
/// reading far below the previous one is a reset (a new login session, say),
/// so everything counted since the reset is returned.
pub(crate) fn counter_delta(previous: u32, current: u32) -> u32 {
	let delta = current.wrapping_sub(previous);
	if delta > MAX_PLAUSIBLE_DELTA {
		current
	} else {
		delta
	}
}

/// Cumulative counters for keys, clicks and scroll events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
	/// Key presses.
	pub(crate) keys: u32,
	/// Mouse button presses of any button.
	pub(crate) clicks: u32,
	/// Scroll wheel and trackpad scroll events.
	pub(crate) scroll: u32,
}

impl Counts {
	/// Events since `previous`, per channel.
	pub(crate) fn since(self, previous: Counts) -> Counts {
		Counts {
			keys: counter_delta(previous.keys, self.keys),
			clicks: counter_delta(previous.clicks, self.clicks),
			scroll: counter_delta(previous.scroll, self.scroll),
		}
	}

	/// Whether every channel is zero.
	pub(crate) fn is_zero(self) -> bool {
		self == Counts::default()
	}

	/// The row as series values, in `keys, clicks, scroll` order.
	pub(crate) fn values(self) -> [f64; 3] {
		[
			f64::from(self.keys),
			f64::from(self.clicks),
			f64::from(self.scroll),
		]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn plain_deltas() {
		assert_eq!(counter_delta(100, 142), 42);
		assert_eq!(counter_delta(7, 7), 0);
	}

	#[test]
	fn wraparound_at_u32_max() {
		assert_eq!(counter_delta(u32::MAX - 4, 10), 15);
		assert_eq!(counter_delta(u32::MAX, 0), 1);
	}

	#[test]
	fn a_reset_counts_from_zero() {
		assert_eq!(counter_delta(5_000_000, 12), 12);
		assert_eq!(counter_delta(1, 0), 0);
	}

	#[test]
	fn counts_subtract_per_channel() {
		let before = Counts {
			keys: 10,
			clicks: u32::MAX,
			scroll: 3,
		};
		let after = Counts {
			keys: 25,
			clicks: 1,
			scroll: 3,
		};
		let delta = after.since(before);
		assert_eq!(
			delta,
			Counts {
				keys: 15,
				clicks: 2,
				scroll: 0
			}
		);
		assert_eq!(delta.values(), [15.0, 2.0, 0.0]);
		assert!(!delta.is_zero());
		assert!(after.since(after).is_zero());
	}
}
