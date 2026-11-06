//! A tamper-evident, hash-chained log of who touched the vault.

use jiff::Timestamp;
use quant_core::ClientId;
use quant_core::GrantId;
use serde::Deserialize;
use serde::Serialize;

use crate::Denied;

/// Who acted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
	/// The owner, after unlocking.
	Owner,
	/// A client acting under a grant.
	Client {
		/// The client.
		client: ClientId,
		/// The grant it used.
		grant: GrantId,
	},
	/// A client that could not be matched to a live grant.
	Unknown {
		/// What it claimed to be, if anything.
		claimed: Option<String>,
	},
	/// Quant itself: retention sweeps, derivation.
	System,
}

/// What came of an action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
	/// It went ahead.
	Allowed {
		/// Records or rows served or stored.
		records: u64,
		/// Bytes served or stored.
		bytes: u64,
	},
	/// It was refused.
	Denied {
		/// Why.
		reason: Denied,
	},
	/// It failed for a reason unrelated to permission.
	Failed,
}

/// One line of the audit log. Entries form a hash chain: each covers the hash
/// of the one before it, so removing or editing an entry breaks every hash
/// after it.
///
/// Entries record the shape of a request (which stream, what range, which
/// tool), never the data served.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
	/// Position in the chain, from 1.
	pub seq: u64,
	/// When it happened.
	pub at: Timestamp,
	/// Who acted.
	pub actor: Actor,
	/// What they did, such as `query`, `search`, `media`, `ingest`,
	/// `grant.create`, or `owner.unlock`.
	pub action: String,
	/// What they did it to, such as a stream id.
	pub target: String,
	/// What came of it.
	pub outcome: Outcome,
	/// The request's shape: ranges, limits, levels.
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub detail: serde_json::Value,
	/// Hex hash of the previous entry, or 64 zeros for the first.
	pub prev: String,
	/// Hex hash of this entry.
	pub hash: String,
}

/// The `prev` of the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// An entry before it is placed in the chain.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEvent {
	/// Who acted.
	pub actor: Actor,
	/// What they did.
	pub action: String,
	/// What they did it to.
	pub target: String,
	/// What came of it.
	pub outcome: Outcome,
	/// The request's shape.
	pub detail: serde_json::Value,
}

impl AuditEvent {
	/// An event with no detail.
	pub fn new(actor: Actor, action: &str, target: impl Into<String>, outcome: Outcome) -> Self {
		Self {
			actor,
			action: action.to_owned(),
			target: target.into(),
			outcome,
			detail: serde_json::Value::Null,
		}
	}

	/// Attach the request's shape.
	#[must_use]
	pub fn detail(mut self, detail: serde_json::Value) -> Self {
		self.detail = detail;
		self
	}

	/// Place the event after `prev` in the chain.
	pub fn seal(self, seq: u64, at: Timestamp, prev: &str) -> AuditEntry {
		let mut entry = AuditEntry {
			seq,
			at,
			actor: self.actor,
			action: self.action,
			target: self.target,
			outcome: self.outcome,
			detail: self.detail,
			prev: prev.to_owned(),
			hash: String::new(),
		};
		entry.hash = entry.compute_hash();
		entry
	}
}

impl AuditEntry {
	/// The hash this entry should carry.
	pub fn compute_hash(&self) -> String {
		let mut unsealed = self.clone();
		unsealed.hash = String::new();
		let bytes = serde_json::to_vec(&unsealed).expect("audit entries serialize");
		let mut hasher = blake3::Hasher::new_derive_key("quant audit chain v1");
		hasher.update(&bytes);
		hasher.finalize().to_hex().to_string()
	}
}

/// Where a chain stopped verifying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChainBreak {
	/// An entry's hash does not match its contents.
	#[error("entry {0} was altered")]
	Altered(u64),
	/// An entry does not point at the one before it.
	#[error("the chain is broken before entry {0}")]
	Unlinked(u64),
	/// Sequence numbers skip.
	#[error("entries are missing before {0}")]
	Gap(u64),
}

/// Verify a run of consecutive entries. `prev` is the hash before the first
/// one (`GENESIS` when starting from the beginning).
pub fn verify_chain<'a>(
	prev: &str,
	entries: impl IntoIterator<Item = &'a AuditEntry>,
) -> Result<(), ChainBreak> {
	let mut prev = prev.to_owned();
	let mut expected_seq = None;
	for entry in entries {
		if let Some(seq) = expected_seq
			&& entry.seq != seq
		{
			return Err(ChainBreak::Gap(entry.seq));
		}
		if entry.prev != prev {
			return Err(ChainBreak::Unlinked(entry.seq));
		}
		if entry.compute_hash() != entry.hash {
			return Err(ChainBreak::Altered(entry.seq));
		}
		prev.clone_from(&entry.hash);
		expected_seq = Some(entry.seq + 1);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn chain(n: u64) -> Vec<AuditEntry> {
		let mut prev = GENESIS.to_owned();
		(1..=n)
			.map(|seq| {
				let entry = AuditEvent::new(
					Actor::Owner,
					"query",
					"health.heart_rate",
					Outcome::Allowed {
						records: seq,
						bytes: 10,
					},
				)
				.seal(seq, Timestamp::UNIX_EPOCH, &prev);
				prev.clone_from(&entry.hash);
				entry
			})
			.collect()
	}

	#[test]
	fn an_untouched_chain_verifies() {
		assert_eq!(verify_chain(GENESIS, &chain(5)), Ok(()));
	}

	#[test]
	fn detects_edits_removals_and_reordering() {
		let mut edited = chain(5);
		edited[2].outcome = Outcome::Allowed {
			records: 0,
			bytes: 0,
		};
		assert_eq!(verify_chain(GENESIS, &edited), Err(ChainBreak::Altered(3)));

		let mut removed = chain(5);
		removed.remove(2);
		assert_eq!(verify_chain(GENESIS, &removed), Err(ChainBreak::Gap(4)));

		let mut rehashed = chain(5);
		rehashed[2].target = "screen.frame".into();
		rehashed[2].hash = rehashed[2].compute_hash();
		assert_eq!(
			verify_chain(GENESIS, &rehashed),
			Err(ChainBreak::Unlinked(4))
		);
	}
}
