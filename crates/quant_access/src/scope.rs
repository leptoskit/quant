use std::fmt;
use std::str::FromStr;

use quant_core::Family;
use quant_core::StreamId;
use serde::Deserialize;
use serde::Serialize;

use crate::AccessError;

/// How much of a stream a grant may see. Each level includes the ones below
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
	/// Aggregates only: averages, totals, durations, counts. No individual
	/// records and no text.
	Summary,
	/// Individual records and their (redacted) text, but no media.
	Read,
	/// Everything, including screenshots and audio.
	Raw,
}

impl Level {
	fn as_str(self) -> &'static str {
		match self {
			Level::Summary => "summary",
			Level::Read => "read",
			Level::Raw => "raw",
		}
	}
}

/// What a scope applies to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
	/// Every stream.
	All,
	/// Every stream in one family.
	Family(Family),
	/// One stream, or every stream beneath it (`sensor.vest` covers
	/// `sensor.vest.ecg`).
	Stream(StreamId),
}

impl Target {
	/// Whether this target covers `stream`.
	pub fn covers(&self, stream: &StreamId) -> bool {
		match self {
			Target::All => true,
			Target::Family(family) => stream.family() == *family,
			Target::Stream(prefix) => {
				let (prefix, name) = (prefix.as_str(), stream.as_str());
				name == prefix
					|| (name.starts_with(prefix)
						&& name.as_bytes().get(prefix.len()) == Some(&b'.'))
			}
		}
	}

	/// Whether this target covers every stream of `family`.
	pub fn covers_family(&self, family: Family) -> bool {
		match self {
			Target::All => true,
			Target::Family(f) => *f == family,
			Target::Stream(_) => false,
		}
	}
}

impl fmt::Display for Target {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Target::All => f.write_str("*"),
			Target::Family(family) => family.fmt(f),
			Target::Stream(stream) => stream.fmt(f),
		}
	}
}

impl FromStr for Target {
	type Err = AccessError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if s == "*" {
			return Ok(Target::All);
		}
		if s.contains('.') {
			return StreamId::new(s)
				.map(Target::Stream)
				.map_err(|_| AccessError::InvalidScope(s.to_owned()));
		}
		s.parse()
			.map(Target::Family)
			.map_err(|_| AccessError::InvalidScope(s.to_owned()))
	}
}

/// One permission, written the way people grant them:
///
/// | Scope                 | Allows                                                 |
/// | --------------------- | ------------------------------------------------------ |
/// | `health:summary`      | Aggregates of every health stream                      |
/// | `screen:read`         | Screen records and their text, no screenshots          |
/// | `audio:raw`           | Transcripts and the audio itself                       |
/// | `sensor.vest:read`    | Records from every `sensor.vest.*` stream              |
/// | `*:summary`           | Aggregates of everything                               |
/// | `motion:write`        | Sending motion data (for capturing devices)            |
/// | `memory:write`        | Writing notes into memory (for agents)                 |
/// | `audit:read`          | Reading the audit log                                  |
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Scope {
	/// See data at a level.
	Read {
		/// What may be read.
		target: Target,
		/// How much.
		level: Level,
	},
	/// Send data into the vault.
	Write {
		/// What may be written.
		target: Target,
	},
	/// Read the audit log.
	AuditRead,
}

impl Scope {
	/// A read scope.
	pub fn read(target: Target, level: Level) -> Self {
		Scope::Read { target, level }
	}

	/// A write scope.
	pub fn write(target: Target) -> Self {
		Scope::Write { target }
	}
}

impl fmt::Display for Scope {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Scope::Read { target, level } => write!(f, "{target}:{}", level.as_str()),
			Scope::Write { target } => write!(f, "{target}:write"),
			Scope::AuditRead => f.write_str("audit:read"),
		}
	}
}

impl FromStr for Scope {
	type Err = AccessError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let invalid = || AccessError::InvalidScope(s.to_owned());
		if s == "audit:read" {
			return Ok(Scope::AuditRead);
		}
		let (target, permission) = s.rsplit_once(':').ok_or_else(invalid)?;
		let target: Target = target.parse().map_err(|_| invalid())?;
		match permission {
			"summary" => Ok(Scope::read(target, Level::Summary)),
			"read" => Ok(Scope::read(target, Level::Read)),
			"raw" => Ok(Scope::read(target, Level::Raw)),
			"write" => Ok(Scope::write(target)),
			_ => Err(invalid()),
		}
	}
}

impl TryFrom<String> for Scope {
	type Error = AccessError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		value.parse()
	}
}

impl From<Scope> for String {
	fn from(value: Scope) -> Self {
		value.to_string()
	}
}

/// A set of scopes, answering what they allow together.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scopes(pub Vec<Scope>);

impl Scopes {
	/// Parse a list such as `["health:summary", "screen:read"]`.
	pub fn parse<I, S>(items: I) -> Result<Self, AccessError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		items
			.into_iter()
			.map(|s| s.as_ref().parse())
			.collect::<Result<_, _>>()
			.map(Scopes)
	}

	/// The highest level these scopes allow on `stream`, if any.
	pub fn read_level(&self, stream: &StreamId) -> Option<Level> {
		self.0
			.iter()
			.filter_map(|scope| {
				match scope {
					Scope::Read { target, level } if target.covers(stream) => Some(*level),
					_ => None,
				}
			})
			.max()
	}

	/// The level these scopes allow across every stream of `family`, if any.
	/// Scopes on single streams do not count here.
	pub fn family_level(&self, family: Family) -> Option<Level> {
		self.0
			.iter()
			.filter_map(|scope| {
				match scope {
					Scope::Read { target, level } if target.covers_family(family) => Some(*level),
					_ => None,
				}
			})
			.max()
	}

	/// Whether these scopes allow any reading of `family` at all, including
	/// through a single-stream scope.
	pub fn touches_family(&self, family: Family) -> bool {
		self.0.iter().any(|scope| {
			match scope {
				Scope::Read {
					target: Target::Stream(stream),
					..
				} => stream.family() == family,
				Scope::Read { target, .. } => target.covers_family(family),
				_ => false,
			}
		})
	}

	/// Whether these scopes allow writing to `stream`.
	pub fn can_write(&self, stream: &StreamId) -> bool {
		self.0
			.iter()
			.any(|scope| matches!(scope, Scope::Write { target } if target.covers(stream)))
	}

	/// Whether these scopes allow reading the audit log.
	pub fn can_read_audit(&self) -> bool {
		self.0.contains(&Scope::AuditRead)
	}

	/// The scopes as strings.
	pub fn to_strings(&self) -> Vec<String> {
		self.0.iter().map(ToString::to_string).collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stream(s: &str) -> StreamId {
		s.parse().unwrap()
	}

	#[test]
	fn scopes_round_trip_through_strings() {
		for s in [
			"health:summary",
			"screen:read",
			"audio:raw",
			"sensor.vest:read",
			"*:summary",
			"motion:write",
			"audit:read",
		] {
			let scope: Scope = s.parse().unwrap();
			assert_eq!(scope.to_string(), s);
		}
	}

	#[test]
	fn rejects_malformed_scopes() {
		for s in [
			"health",
			"health:admin",
			"bogus:read",
			"Health:read",
			":read",
			"audit:write",
		] {
			assert!(s.parse::<Scope>().is_err(), "{s} should be rejected");
		}
	}

	#[test]
	fn stream_targets_cover_children_but_not_siblings() {
		let target: Target = "sensor.vest".parse().unwrap();
		assert!(target.covers(&stream("sensor.vest")));
		assert!(target.covers(&stream("sensor.vest.ecg")));
		assert!(!target.covers(&stream("sensor.vestibular")));
		assert!(!target.covers(&stream("sensor.leg.imu")));
	}

	#[test]
	fn highest_level_wins() {
		let scopes = Scopes::parse(["*:summary", "health:read", "health.heart_rate:raw"]).unwrap();
		assert_eq!(
			scopes.read_level(&stream("health.heart_rate")),
			Some(Level::Raw)
		);
		assert_eq!(scopes.read_level(&stream("health.hrv")), Some(Level::Read));
		assert_eq!(
			scopes.read_level(&stream("screen.frame")),
			Some(Level::Summary)
		);
		assert_eq!(scopes.family_level(Family::Health), Some(Level::Read));
	}

	#[test]
	fn nothing_is_allowed_by_default() {
		let scopes = Scopes::default();
		assert_eq!(scopes.read_level(&stream("health.heart_rate")), None);
		assert!(!scopes.can_write(&stream("health.heart_rate")));
		assert!(!scopes.can_read_audit());
	}

	#[test]
	fn write_does_not_imply_read() {
		let scopes = Scopes::parse(["health:write"]).unwrap();
		assert!(scopes.can_write(&stream("health.steps")));
		assert_eq!(scopes.read_level(&stream("health.steps")), None);
	}

	#[test]
	fn single_stream_scopes_touch_their_family() {
		let scopes = Scopes::parse(["health.steps:summary"]).unwrap();
		assert!(scopes.touches_family(Family::Health));
		assert_eq!(scopes.family_level(Family::Health), None);
		assert!(!scopes.touches_family(Family::Screen));
	}
}
