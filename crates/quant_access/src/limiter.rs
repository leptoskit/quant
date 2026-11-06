use std::collections::HashMap;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::GrantId;

use crate::Denied;
use crate::Limits;

/// A bucket that refills continuously at `capacity` tokens per `period`.
#[derive(Clone, Copy, Debug)]
struct Bucket {
	tokens: f64,
	updated: Timestamp,
}

impl Bucket {
	fn full(capacity: f64, now: Timestamp) -> Self {
		Self {
			tokens: capacity,
			updated: now,
		}
	}

	fn refill(&mut self, capacity: f64, period: SignedDuration, now: Timestamp) {
		let elapsed = now.duration_since(self.updated).as_secs_f64().max(0.0);
		let rate = capacity / period.as_secs_f64();
		self.tokens = (self.tokens + elapsed * rate).min(capacity);
		self.updated = now;
	}
}

#[derive(Clone, Copy, Debug)]
struct Buckets {
	requests: Bucket,
	records: Bucket,
	bytes: Bucket,
}

/// Enforces each grant's [`Limits`].
///
/// A request first passes [`Limiter::admit`], which spends one request token.
/// Its handler then asks [`Limiter::record_allowance`] how many records it may
/// return and trims its answer to fit, and finally [`Limiter::charge`]s what
/// it sent. Answers are trimmed rather than refused so an agent near its
/// limit still gets something, with a note that more is held back.
#[derive(Debug, Default)]
pub struct Limiter {
	buckets: HashMap<GrantId, Buckets>,
}

const MINUTE: SignedDuration = SignedDuration::from_secs(60);
const HOUR: SignedDuration = SignedDuration::from_secs(3600);
const DAY: SignedDuration = SignedDuration::from_secs(86_400);

impl Limiter {
	/// An empty limiter; every grant starts with full buckets.
	pub fn new() -> Self {
		Self::default()
	}

	fn buckets(&mut self, grant: GrantId, limits: &Limits, now: Timestamp) -> &mut Buckets {
		let buckets = self.buckets.entry(grant).or_insert_with(|| {
			Buckets {
				requests: Bucket::full(f64::from(limits.requests_per_minute), now),
				records: Bucket::full(f64::from(limits.records_per_hour), now),
				bytes: Bucket::full(limits.bytes_per_day as f64, now),
			}
		});
		buckets
			.requests
			.refill(f64::from(limits.requests_per_minute), MINUTE, now);
		buckets
			.records
			.refill(f64::from(limits.records_per_hour), HOUR, now);
		buckets.bytes.refill(limits.bytes_per_day as f64, DAY, now);
		buckets
	}

	/// Spend one request, or refuse if the grant is out of requests, records,
	/// or bytes.
	pub fn admit(&mut self, grant: GrantId, limits: &Limits, now: Timestamp) -> Result<(), Denied> {
		let buckets = self.buckets(grant, limits, now);
		if buckets.requests.tokens < 1.0
			|| buckets.records.tokens < 1.0
			|| buckets.bytes.tokens < 1.0
		{
			return Err(Denied::RateLimited);
		}
		buckets.requests.tokens -= 1.0;
		Ok(())
	}

	/// How many records the grant may still receive right now.
	pub fn record_allowance(&mut self, grant: GrantId, limits: &Limits, now: Timestamp) -> usize {
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let allowance = self
			.buckets(grant, limits, now)
			.records
			.tokens
			.floor()
			.max(0.0) as usize;
		allowance
	}

	/// How many response bytes the grant may still receive right now.
	pub fn byte_allowance(&mut self, grant: GrantId, limits: &Limits, now: Timestamp) -> u64 {
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let allowance = self
			.buckets(grant, limits, now)
			.bytes
			.tokens
			.floor()
			.max(0.0) as u64;
		allowance
	}

	/// Spend records and bytes that were sent.
	pub fn charge(
		&mut self,
		grant: GrantId,
		limits: &Limits,
		records: usize,
		bytes: u64,
		now: Timestamp,
	) {
		let buckets = self.buckets(grant, limits, now);
		buckets.records.tokens = (buckets.records.tokens - records as f64).max(0.0);
		buckets.bytes.tokens = (buckets.bytes.tokens - bytes as f64).max(0.0);
	}

	/// Forget a grant's buckets, after it is revoked or its limits change.
	pub fn reset(&mut self, grant: GrantId) {
		self.buckets.remove(&grant);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn limits() -> Limits {
		Limits {
			requests_per_minute: 2,
			records_per_hour: 100,
			bytes_per_day: 1000,
		}
	}

	#[test]
	fn refuses_requests_past_the_per_minute_limit() {
		let mut limiter = Limiter::new();
		let grant = GrantId::new();
		let now = Timestamp::UNIX_EPOCH + SignedDuration::from_hours(1000);
		assert!(limiter.admit(grant, &limits(), now).is_ok());
		assert!(limiter.admit(grant, &limits(), now).is_ok());
		assert_eq!(
			limiter.admit(grant, &limits(), now),
			Err(Denied::RateLimited)
		);
		// Half a minute later, one request has refilled.
		let later = now + SignedDuration::from_secs(30);
		assert!(limiter.admit(grant, &limits(), later).is_ok());
		assert_eq!(
			limiter.admit(grant, &limits(), later),
			Err(Denied::RateLimited)
		);
	}

	#[test]
	fn record_and_byte_budgets_drain_and_refill() {
		let mut limiter = Limiter::new();
		let grant = GrantId::new();
		let now = Timestamp::UNIX_EPOCH + SignedDuration::from_hours(1000);
		assert_eq!(limiter.record_allowance(grant, &limits(), now), 100);
		limiter.charge(grant, &limits(), 100, 1000, now);
		assert_eq!(limiter.record_allowance(grant, &limits(), now), 0);
		assert_eq!(limiter.byte_allowance(grant, &limits(), now), 0);
		assert_eq!(
			limiter.admit(grant, &limits(), now),
			Err(Denied::RateLimited)
		);
		let later = now + SignedDuration::from_mins(30);
		assert_eq!(limiter.record_allowance(grant, &limits(), later), 50);
	}

	#[test]
	fn grants_have_separate_buckets() {
		let mut limiter = Limiter::new();
		let now = Timestamp::UNIX_EPOCH + SignedDuration::from_hours(1000);
		let (a, b) = (GrantId::new(), GrantId::new());
		limiter.charge(a, &limits(), 100, 0, now);
		assert_eq!(limiter.record_allowance(a, &limits(), now), 0);
		assert_eq!(limiter.record_allowance(b, &limits(), now), 100);
	}
}
