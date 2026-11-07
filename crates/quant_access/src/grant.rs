use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::Class;
use quant_core::ClientId;
use quant_core::GrantId;
use quant_core::StreamId;
use quant_core::TimeRange;
use serde::Deserialize;
use serde::Serialize;

use crate::Level;
use crate::Scopes;

/// How much a grant may pull out of the vault, so a leaked or runaway agent
/// cannot copy a life in one afternoon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
	/// Requests allowed per minute.
	pub requests_per_minute: u32,
	/// Records (or series rows) returned per hour.
	pub records_per_hour: u32,
	/// Response bytes per day.
	pub bytes_per_day: u64,
}

impl Default for Limits {
	fn default() -> Self {
		Self {
			requests_per_minute: 60,
			records_per_hour: 5_000,
			bytes_per_day: 50 * 1024 * 1024,
		}
	}
}

/// Permission for one client to use the vault.
///
/// A grant is inert until the owner approves it, and stays bounded by its
/// scopes, its lookback, its limits, and its expiry. Every change bumps
/// `epoch`, so a session holding an older copy knows to reload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
	/// The grant's id.
	pub id: GrantId,
	/// The client it belongs to.
	pub client: ClientId,
	/// A name the owner recognises, such as "Claude Code on mini".
	pub label: String,
	/// What it may do.
	pub scopes: Scopes,
	/// How far back it may see. `None` means no limit.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub lookback: Option<SignedDuration>,
	/// Rate limits.
	pub limits: Limits,
	/// Whether text it receives keeps secrets and personal data unredacted.
	#[serde(default)]
	pub unredacted: bool,
	/// Sensitivity classes this grant never sees: any stream or claim
	/// carrying one is treated exactly as if it were out of scope.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub deny_classes: Vec<Class>,
	/// When it was created.
	pub created_at: Timestamp,
	/// When it stops working.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub expires_at: Option<Timestamp>,
	/// When the owner revoked it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub revoked_at: Option<Timestamp>,
	/// Bumped on every change.
	pub epoch: u32,
}

/// Why a request was refused. Reasons are kept coarse on purpose: a caller
/// learns that it may not do something, not what exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Denied {
	/// The grant was revoked.
	#[error("grant revoked")]
	Revoked,
	/// The grant expired.
	#[error("grant expired")]
	Expired,
	/// No scope allows this.
	#[error("not permitted by this grant's scopes")]
	OutOfScope,
	/// A rate limit was hit; retry later.
	#[error("rate limit reached")]
	RateLimited,
}

/// What a read may return once authorised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPermit {
	/// The level of detail allowed.
	pub level: Level,
	/// The time range, clipped to the grant's lookback.
	pub range: TimeRange,
	/// Whether text must be redacted.
	pub redact: bool,
}

impl Grant {
	/// A new grant for `client`, live from now.
	pub fn new(client: ClientId, label: impl Into<String>, scopes: Scopes) -> Self {
		Self {
			id: GrantId::new(),
			client,
			label: label.into(),
			scopes,
			lookback: None,
			limits: Limits::default(),
			unredacted: false,
			deny_classes: Vec::new(),
			created_at: Timestamp::now(),
			expires_at: None,
			revoked_at: None,
			epoch: 1,
		}
	}

	/// Whether the grant is usable at `now`.
	pub fn check_live(&self, now: Timestamp) -> Result<(), Denied> {
		if self.revoked_at.is_some() {
			return Err(Denied::Revoked);
		}
		if self.expires_at.is_some_and(|expiry| now >= expiry) {
			return Err(Denied::Expired);
		}
		Ok(())
	}

	/// The earliest moment this grant may see at `now`.
	pub fn floor(&self, now: Timestamp) -> Timestamp {
		self.lookback.map_or(Timestamp::MIN, |span| {
			now.checked_sub(span).unwrap_or(Timestamp::MIN)
		})
	}

	/// Authorise reading `stream` over `range` at `wanted` detail. The permit
	/// may come back at a lower level than asked for: an agent with only
	/// `health:summary` asking for records gets summaries.
	pub fn authorize_read(
		&self,
		stream: &StreamId,
		wanted: Level,
		range: TimeRange,
		now: Timestamp,
	) -> Result<ReadPermit, Denied> {
		self.check_live(now)?;
		let allowed = self.scopes.read_level(stream).ok_or(Denied::OutOfScope)?;
		Ok(ReadPermit {
			level: wanted.min(allowed),
			range: range.clamp_start(self.floor(now)),
			redact: !self.unredacted,
		})
	}

	/// Authorise writing to `stream`.
	pub fn authorize_write(&self, stream: &StreamId, now: Timestamp) -> Result<(), Denied> {
		self.check_live(now)?;
		if self.scopes.can_write(stream) {
			Ok(())
		} else {
			Err(Denied::OutOfScope)
		}
	}

	/// Authorise reading the audit log.
	pub fn authorize_audit(&self, now: Timestamp) -> Result<(), Denied> {
		self.check_live(now)?;
		if self.scopes.can_read_audit() {
			Ok(())
		} else {
			Err(Denied::OutOfScope)
		}
	}

	/// Whether this grant denies any of `classes`.
	pub fn denies(&self, classes: &[Class]) -> bool {
		classes
			.iter()
			.any(|class| self.deny_classes.contains(class))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn grant(scopes: &[&str]) -> Grant {
		Grant::new(ClientId::new(), "test", Scopes::parse(scopes).unwrap())
	}

	fn hr() -> StreamId {
		"health.heart_rate".parse().unwrap()
	}

	#[test]
	fn downgrades_to_the_allowed_level() {
		let g = grant(&["health:summary"]);
		let now = Timestamp::now();
		let range = TimeRange::last(SignedDuration::from_hours(1), now);
		let permit = g.authorize_read(&hr(), Level::Raw, range, now).unwrap();
		assert_eq!(permit.level, Level::Summary);
		assert!(permit.redact);
	}

	#[test]
	fn refuses_streams_outside_scope() {
		let g = grant(&["health:summary"]);
		let now = Timestamp::now();
		let range = TimeRange::last(SignedDuration::from_hours(1), now);
		let screen: StreamId = "screen.frame".parse().unwrap();
		assert_eq!(
			g.authorize_read(&screen, Level::Read, range, now),
			Err(Denied::OutOfScope)
		);
	}

	#[test]
	fn clips_reads_to_the_lookback() {
		let mut g = grant(&["*:read"]);
		g.lookback = Some(SignedDuration::from_hours(24));
		let now = Timestamp::now();
		let range = TimeRange::last(SignedDuration::from_hours(24 * 30), now);
		let permit = g.authorize_read(&hr(), Level::Read, range, now).unwrap();
		assert_eq!(permit.range.duration(), SignedDuration::from_hours(24));
	}

	#[test]
	fn revoked_and_expired_grants_refuse_everything() {
		let now = Timestamp::now();
		let range = TimeRange::last(SignedDuration::from_hours(1), now);

		let mut revoked = grant(&["*:raw", "*:write", "audit:read"]);
		revoked.revoked_at = Some(now);
		assert_eq!(
			revoked.authorize_read(&hr(), Level::Read, range, now),
			Err(Denied::Revoked)
		);
		assert_eq!(revoked.authorize_write(&hr(), now), Err(Denied::Revoked));
		assert_eq!(revoked.authorize_audit(now), Err(Denied::Revoked));

		let mut expired = grant(&["*:raw"]);
		expired.expires_at = Some(now);
		assert_eq!(
			expired.authorize_read(&hr(), Level::Read, range, now),
			Err(Denied::Expired)
		);
	}

	#[test]
	fn deny_classes_default_empty_and_round_trip() {
		let g = grant(&["*:read"]);
		assert!(g.deny_classes.is_empty());
		assert!(!g.denies(&[Class::Health]));

		let mut g = grant(&["*:read"]);
		g.deny_classes = vec![Class::Health, Class::Others];
		assert!(g.denies(&[Class::Others]));
		assert!(g.denies(&[Class::Health, Class::Location]));
		assert!(!g.denies(&[Class::Location, Class::Financial]));

		// A grant written before deny_classes existed still loads.
		let json = serde_json::to_string(&g).unwrap();
		let mut without: serde_json::Value = serde_json::from_str(&json).unwrap();
		without.as_object_mut().unwrap().remove("deny_classes");
		let back: Grant = serde_json::from_value(without).unwrap();
		assert!(back.deny_classes.is_empty());
	}
}
