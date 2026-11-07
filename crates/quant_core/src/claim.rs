//! What Quant believes about its owner, and who said so.
//!
//! A [`Claim`] is one statement about a person (usually `me`): a preference,
//! a value, a routine, a fact, a relationship, a goal, or a commitment. Each
//! claim carries an [`Authority`] that says where it came from — the owner's
//! corrections rank above what the owner wrote, which ranks above what Quant
//! observed, which ranks above what an agent inferred — and evidence in the
//! form of record ids, so every claim can be traced back to the life it came
//! from.

use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;

use crate::ClaimId;
use crate::ClientId;
use crate::CoreError;
use crate::RecordId;

/// What sort of statement a claim makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
	/// What the person likes or wants, such as "prefers espresso after
	/// lunch".
	Preference,
	/// What the person holds important, such as "privacy over convenience".
	Value,
	/// Something the person does repeatedly, such as "writes best before
	/// noon".
	Routine,
	/// Something that is true about the person, such as "allergic to
	/// peanuts".
	Fact,
	/// How the person relates to someone, such as "Ada is their partner".
	Relationship,
	/// Something the person is working towards.
	Goal,
	/// Something the person promised, such as "will review the budget by
	/// Friday".
	Commitment,
}

impl ClaimKind {
	/// The kind's name as stored and served.
	pub fn as_str(self) -> &'static str {
		match self {
			ClaimKind::Preference => "preference",
			ClaimKind::Value => "value",
			ClaimKind::Routine => "routine",
			ClaimKind::Fact => "fact",
			ClaimKind::Relationship => "relationship",
			ClaimKind::Goal => "goal",
			ClaimKind::Commitment => "commitment",
		}
	}

	/// A kind by name, if known.
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"preference" => Some(ClaimKind::Preference),
			"value" => Some(ClaimKind::Value),
			"routine" => Some(ClaimKind::Routine),
			"fact" => Some(ClaimKind::Fact),
			"relationship" => Some(ClaimKind::Relationship),
			"goal" => Some(ClaimKind::Goal),
			"commitment" => Some(ClaimKind::Commitment),
			_ => None,
		}
	}
}

/// Where a claim came from. Ordered: the owner's corrections outrank what the
/// owner wrote, which outranks what Quant observed, which outranks what an
/// agent inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
	/// The owner stated it directly, or corrected an earlier claim.
	Owner,
	/// The owner wrote it somewhere (a note, a message).
	OwnerAuthored,
	/// Quant derived it from what it observed.
	Observed,
	/// An agent proposed it, and the owner approved it.
	Inferred,
}

impl Authority {
	/// The authority's name as stored and served.
	pub fn as_str(self) -> &'static str {
		match self {
			Authority::Owner => "owner",
			Authority::OwnerAuthored => "owner_authored",
			Authority::Observed => "observed",
			Authority::Inferred => "inferred",
		}
	}

	/// An authority by name, if known.
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"owner" => Some(Authority::Owner),
			"owner_authored" => Some(Authority::OwnerAuthored),
			"observed" => Some(Authority::Observed),
			"inferred" => Some(Authority::Inferred),
			_ => None,
		}
	}

	/// How much the authority counts: Owner ranks highest, Inferred lowest.
	fn rank(self) -> u8 {
		match self {
			Authority::Owner => 3,
			Authority::OwnerAuthored => 2,
			Authority::Observed => 1,
			Authority::Inferred => 0,
		}
	}
}

impl Ord for Authority {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.rank().cmp(&other.rank())
	}
}

impl PartialOrd for Authority {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

/// A sensitivity class. Streams carry classes, claims carry classes, and a
/// grant may deny classes outright: any stream or claim with a denied class
/// is treated as if it did not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
	/// Health and body data.
	Health,
	/// Intimate detail the owner would share only deliberately.
	Intimate,
	/// Where the owner has been.
	Location,
	/// Other people's words or voices.
	Others,
	/// Money: accounts, purchases, balances.
	Financial,
}

impl Class {
	/// Every class, in a stable order.
	pub const ALL: [Class; 5] = [
		Class::Health,
		Class::Intimate,
		Class::Location,
		Class::Others,
		Class::Financial,
	];

	/// The class's name as stored and served.
	pub fn as_str(self) -> &'static str {
		match self {
			Class::Health => "health",
			Class::Intimate => "intimate",
			Class::Location => "location",
			Class::Others => "others",
			Class::Financial => "financial",
		}
	}

	/// A class by name, if known.
	pub fn parse(s: &str) -> Option<Self> {
		Class::ALL.into_iter().find(|class| class.as_str() == s)
	}
}

impl std::fmt::Display for Class {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

impl std::str::FromStr for Class {
	type Err = CoreError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Class::parse(s).ok_or_else(|| CoreError::UnknownClass(s.to_owned()))
	}
}

/// Where a claim stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
	/// Served to agents, within their grant.
	Active,
	/// Proposed by an agent, waiting for the owner. Served to no one but the
	/// owner.
	Held,
	/// The owner refused it.
	Rejected,
	/// The owner withdrew it.
	Retracted,
	/// Replaced by a newer claim (see `superseded_by`).
	Superseded,
}

impl ClaimStatus {
	/// The status's name as stored and served.
	pub fn as_str(self) -> &'static str {
		match self {
			ClaimStatus::Active => "active",
			ClaimStatus::Held => "held",
			ClaimStatus::Rejected => "rejected",
			ClaimStatus::Retracted => "retracted",
			ClaimStatus::Superseded => "superseded",
		}
	}

	/// A status by name, if known.
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"active" => Some(ClaimStatus::Active),
			"held" => Some(ClaimStatus::Held),
			"rejected" => Some(ClaimStatus::Rejected),
			"retracted" => Some(ClaimStatus::Retracted),
			"superseded" => Some(ClaimStatus::Superseded),
			_ => None,
		}
	}
}

/// The most characters a claim statement may hold.
pub const MAX_STATEMENT: usize = 500;

/// One belief Quant holds about a person, with where it came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
	/// Unique, time-ordered id.
	pub id: ClaimId,
	/// Who it is about: `me` by default, otherwise an opaque key such as
	/// `person:sarah`.
	pub subject: String,
	/// What sort of statement.
	pub kind: ClaimKind,
	/// The statement itself, in natural language.
	pub statement: String,
	/// A short dotted key such as `coffee.timing`, so later claims about the
	/// same thing supersede earlier ones.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topic: Option<String>,
	/// Where the claim came from.
	pub authority: Authority,
	/// How sure Quant is, from 0 to 1.
	pub confidence: f32,
	/// Sensitivity classes, so a grant that denies a class never sees it.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub classes: Vec<Class>,
	/// Where the claim stands.
	pub status: ClaimStatus,
	/// From when it holds, if bounded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub valid_from: Option<Timestamp>,
	/// Until when it holds, if bounded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub valid_to: Option<Timestamp>,
	/// The records it is grounded in.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub evidence: Vec<RecordId>,
	/// The client that proposed it, for claims agents proposed.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub proposed_by: Option<ClientId>,
	/// When it was created.
	pub created_at: Timestamp,
	/// When it last changed.
	pub updated_at: Timestamp,
	/// The claim that replaced it, when it was superseded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub superseded_by: Option<ClaimId>,
}

impl Claim {
	/// A new claim with `statement`, `kind`, and `authority`, live now.
	pub fn new(statement: impl Into<String>, kind: ClaimKind, authority: Authority) -> Self {
		let now = Timestamp::now();
		Self {
			id: ClaimId::new(),
			subject: "me".to_owned(),
			kind,
			statement: statement.into(),
			topic: None,
			authority,
			confidence: 1.0,
			classes: Vec::new(),
			status: ClaimStatus::Active,
			valid_from: None,
			valid_to: None,
			evidence: Vec::new(),
			proposed_by: None,
			created_at: now,
			updated_at: now,
			superseded_by: None,
		}
	}

	/// Check the values a stored claim must satisfy.
	pub fn validate(&self) -> Result<(), CoreError> {
		if self.statement.is_empty() || self.statement.chars().count() > MAX_STATEMENT {
			return Err(CoreError::BadClaim(format!(
				"a claim statement must be 1 to {MAX_STATEMENT} characters"
			)));
		}
		if self.subject.is_empty() {
			return Err(CoreError::BadClaim("a claim needs a subject".to_owned()));
		}
		if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
			return Err(CoreError::BadClaim(format!(
				"a claim's confidence must be between 0 and 1, got {}",
				self.confidence
			)));
		}
		if let Some(topic) = &self.topic
			&& (topic.is_empty()
				|| topic.chars().count() > 64
				|| !topic.bytes().all(|b| {
					b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
				})) {
			return Err(CoreError::BadClaim(format!(
				"a claim topic must be a short dotted key such as `coffee.timing`, got `{topic}`"
			)));
		}
		if let (Some(from), Some(to)) = (self.valid_from, self.valid_to)
			&& to < from
		{
			return Err(CoreError::BadClaim(
				"a claim's valid range ends before it starts".to_owned(),
			));
		}
		if matches!(self.authority, Authority::Observed | Authority::Inferred)
			&& self.evidence.is_empty()
		{
			return Err(CoreError::BadClaim(format!(
				"a claim with authority `{}` needs evidence records",
				self.authority.as_str()
			)));
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn enums_round_trip_through_their_names() {
		for kind in [
			ClaimKind::Preference,
			ClaimKind::Value,
			ClaimKind::Routine,
			ClaimKind::Fact,
			ClaimKind::Relationship,
			ClaimKind::Goal,
			ClaimKind::Commitment,
		] {
			assert_eq!(ClaimKind::parse(kind.as_str()), Some(kind));
			assert_eq!(
				serde_json::to_string(&kind).unwrap(),
				format!("\"{}\"", kind.as_str())
			);
		}
		for authority in [
			Authority::Owner,
			Authority::OwnerAuthored,
			Authority::Observed,
			Authority::Inferred,
		] {
			assert_eq!(Authority::parse(authority.as_str()), Some(authority));
		}
		for status in [
			ClaimStatus::Active,
			ClaimStatus::Held,
			ClaimStatus::Rejected,
			ClaimStatus::Retracted,
			ClaimStatus::Superseded,
		] {
			assert_eq!(ClaimStatus::parse(status.as_str()), Some(status));
		}
	}

	#[test]
	fn authority_orders_from_owner_to_inferred() {
		assert!(Authority::Owner > Authority::OwnerAuthored);
		assert!(Authority::OwnerAuthored > Authority::Observed);
		assert!(Authority::Observed > Authority::Inferred);
	}

	#[test]
	fn classes_parse_from_their_names() {
		for class in Class::ALL {
			assert_eq!(class.as_str().parse::<Class>().unwrap(), class);
		}
		assert!("bogus".parse::<Class>().is_err());
	}

	#[test]
	fn claims_round_trip_through_json() {
		let claim = Claim::new(
			"Prefers espresso after lunch",
			ClaimKind::Preference,
			Authority::Owner,
		);
		let json = serde_json::to_string(&claim).unwrap();
		let back: Claim = serde_json::from_str(&json).unwrap();
		assert_eq!(back, claim);
	}

	#[test]
	fn empty_fields_are_omitted() {
		let claim = Claim::new("Runs on Tuesdays", ClaimKind::Routine, Authority::Owner);
		let json = serde_json::to_value(&claim).unwrap();
		let object = json.as_object().unwrap();
		for absent in [
			"topic",
			"classes",
			"valid_from",
			"valid_to",
			"evidence",
			"proposed_by",
			"superseded_by",
		] {
			assert!(!object.contains_key(absent), "{absent} should be omitted");
		}
	}

	#[test]
	fn validates_statements_confidence_and_topics() {
		let base = Claim::new("Fine", ClaimKind::Fact, Authority::Owner);
		assert!(base.validate().is_ok());
		assert!(
			Claim {
				statement: String::new(),
				..base.clone()
			}
			.validate()
			.is_err()
		);
		assert!(
			Claim {
				statement: "x".repeat(MAX_STATEMENT + 1),
				..base.clone()
			}
			.validate()
			.is_err()
		);
		assert!(
			Claim {
				confidence: 1.5,
				..base.clone()
			}
			.validate()
			.is_err()
		);
		assert!(
			Claim {
				confidence: f32::NAN,
				..base.clone()
			}
			.validate()
			.is_err()
		);
		assert!(
			Claim {
				topic: Some("Bad Topic".into()),
				..base.clone()
			}
			.validate()
			.is_err()
		);
		assert!(
			Claim {
				topic: Some("coffee.timing".into()),
				..base.clone()
			}
			.validate()
			.is_ok()
		);
		assert!(
			Claim {
				authority: Authority::Inferred,
				..base
			}
			.validate()
			.is_err()
		);
	}
}
