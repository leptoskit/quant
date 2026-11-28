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
	}
}

#[cfg(test)]
mod tests {
	use quant_access::Scopes;
	use quant_core::ClientId;

	use super::*;

	fn grant(scopes: &[&str]) -> Grant {
		Grant::new(
			ClientId::new(),
			"phone stats",
			Scopes::parse(scopes).unwrap(),
		)
	}

	#[test]
	fn stats_needs_a_read_scope_but_accounts_for_the_owner_over_granting() {
		assert!(!allows(&grant(&[]), CompanionCapability::StatsSummary));
		assert!(!allows(
			&grant(&["health:write", "apps:write"]),
			CompanionCapability::StatsSummary
		));
		assert!(!allows(
			&grant(&["audit:read"]),
			CompanionCapability::StatsSummary
		));
		assert!(allows(
			&grant(&["health:summary"]),
			CompanionCapability::StatsSummary
		));
		assert!(allows(
			&grant(&["apps:read"]),
			CompanionCapability::StatsSummary
		));
	}
}
