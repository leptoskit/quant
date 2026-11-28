//! The opt-in companion identity.
//!
//! The collector has its own signing key and a write-only grant. Statistics
//! (and, later, messaging) use a second identity with its own key and its
//! own grant, kept beside the collector's. Each capability asks for its own
//! scopes, so approving stats never grants messaging and the collector never
//! gains read access.

use quant_access::Grant;
use quant_access::Scope;
use quant_client::Client;
use quant_model::phone::CompanionCapability;

/// A companion client opened for exactly one capability.
///
/// Follow-on features take one of these rather than the raw signing client,
/// so each one carries its own permission and the grant it was opened under.
/// Messaging, for example, adds its own [`CompanionCapability`] variant and
/// scopes, then asks the worker for a handle with that capability; the
/// request, approval, revocation, and clearing rules stay shared.
#[derive(Clone, Debug)]
pub struct Companion {
	capability: CompanionCapability,
	client: Client,
}

impl Companion {
	/// A handle for `capability` using `client`.
	pub fn new(capability: CompanionCapability, client: Client) -> Self {
		Self { capability, client }
	}

	/// The permission this handle was opened for.
	pub fn capability(&self) -> CompanionCapability {
		self.capability
	}

	/// The signed client. Feature code uses it only for its capability.
	pub fn client(&self) -> &Client {
		&self.client
	}
}

/// Whether `grant` can serve `capability` at all. The vault still enforces
/// every scope on every request; this only decides whether the companion is
/// ready or restricted before a query is attempted.
pub fn allows(grant: &Grant, capability: CompanionCapability) -> bool {
	match capability {
		CompanionCapability::StatsSummary => {
			grant
				.scopes
				.0
				.iter()
				.any(|scope| matches!(scope, Scope::Read { .. }))
		}
		CompanionCapability::AskAgents => grant.scopes.can_companion(),
	}
}

/// Normalize a list of capabilities into canonical order without duplicates.
pub fn normalize(capabilities: &[CompanionCapability]) -> Vec<CompanionCapability> {
	let mut out: Vec<CompanionCapability> = capabilities.to_vec();
	out.sort();
	out.dedup();
	out
}

/// Build the explicit scope list to request or re-approve for `capabilities`.
///
/// When updating an already active grant that retains `StatsSummary`, any
/// read scopes the owner previously narrowed (such as `apps:summary`) are
/// preserved instead of silently widening back to `*:summary`.
pub fn scopes_for(capabilities: &[CompanionCapability], existing: Option<&Grant>) -> Vec<String> {
	let capabilities = normalize(capabilities);
	let mut scopes = Vec::new();
	if capabilities.contains(&CompanionCapability::StatsSummary) {
		let existing_reads: Vec<String> = existing
			.into_iter()
			.flat_map(|grant| grant.scopes.0.iter())
			.filter(|scope| matches!(scope, Scope::Read { .. }))
			.map(ToString::to_string)
			.collect();
		if existing_reads.is_empty() {
			for scope in CompanionCapability::StatsSummary.scopes() {
				scopes.push((*scope).to_owned());
			}
		} else {
			scopes.extend(existing_reads);
		}
	}
	if capabilities.contains(&CompanionCapability::AskAgents) {
		for scope in CompanionCapability::AskAgents.scopes() {
			scopes.push((*scope).to_owned());
		}
	}
	scopes
}

#[cfg(test)]
mod tests {
	use quant_access::Scopes;
	use quant_core::ClientId;

	use super::*;

	fn grant(scopes: &[&str]) -> Grant {
		Grant::new(
			ClientId::new(),
			"phone companion",
			Scopes::parse(scopes).unwrap(),
		)
	}

	#[test]
	fn stats_needs_a_read_scope_and_ask_needs_companion_write() {
		assert!(!allows(&grant(&[]), CompanionCapability::StatsSummary));
		assert!(!allows(&grant(&[]), CompanionCapability::AskAgents));
		assert!(!allows(
			&grant(&["health:write", "apps:write"]),
			CompanionCapability::StatsSummary
		));
		assert!(!allows(
			&grant(&["health:write", "apps:write"]),
			CompanionCapability::AskAgents
		));
		assert!(!allows(
			&grant(&["audit:read"]),
			CompanionCapability::StatsSummary
		));
		assert!(!allows(
			&grant(&["*:raw", "audit:read"]),
			CompanionCapability::AskAgents
		));
		assert!(allows(
			&grant(&["health:summary"]),
			CompanionCapability::StatsSummary
		));
		assert!(!allows(
			&grant(&["health:summary"]),
			CompanionCapability::AskAgents
		));
		assert!(allows(
			&grant(&["apps:read"]),
			CompanionCapability::StatsSummary
		));
		assert!(allows(
			&grant(&["companion:write"]),
			CompanionCapability::AskAgents
		));
		assert!(!allows(
			&grant(&["companion:write"]),
			CompanionCapability::StatsSummary
		));
		let both = grant(&["*:summary", "companion:write"]);
		assert!(allows(&both, CompanionCapability::StatsSummary));
		assert!(allows(&both, CompanionCapability::AskAgents));
	}

	#[test]
	fn scopes_for_preserves_narrowed_read_scopes_when_adding_ask() {
		let narrowed = grant(&["apps:summary"]);
		assert_eq!(
			scopes_for(
				&[
					CompanionCapability::AskAgents,
					CompanionCapability::StatsSummary
				],
				Some(&narrowed)
			),
			vec!["apps:summary".to_owned(), "companion:write".to_owned()]
		);
		assert_eq!(
			scopes_for(&[CompanionCapability::AskAgents], Some(&narrowed)),
			vec!["companion:write".to_owned()]
		);
		assert_eq!(
			scopes_for(&[CompanionCapability::StatsSummary], None),
			vec!["*:summary".to_owned()]
		);
	}
}
