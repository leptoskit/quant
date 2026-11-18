//! What an agent pairing must say about itself.
//!
//! The owner's rule: an agent has to say exactly what agent it is before it
//! gets access. The identity is given once, at pairing, and is shown with
//! the pairing request; the model actually running a session may differ and
//! is restated per request through `x-quant-model`.

use serde::Deserialize;
use serde::Serialize;

/// How long each identity field may be, in characters.
pub const MAX_FIELD: usize = 120;

/// The invisible characters a name may not hide behind: zero-width joins,
/// bidi overrides, and byte-order marks.
fn invisible(c: char) -> bool {
	matches!(c,
		'\u{200B}'..='\u{200F}'
		| '\u{202A}'..='\u{202E}'
		| '\u{2060}'..='\u{2064}'
		| '\u{2066}'..='\u{2069}'
		| '\u{FEFF}'
	)
}

/// Who is asking for access: the product, its vendor, the model, the
/// machine it runs on, and — optionally — the person operating it.
///
/// Required when an `agent` client pairs. The fields are the agent's own
/// words about itself; the daemon checks they are present and sane, not
/// that they are true, so the owner still reads them before approving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIdentity {
	/// The product, such as "Claude Code".
	pub product: String,
	/// Who makes it, such as "Anthropic".
	pub vendor: String,
	/// The model it runs, such as "claude-opus-5-5".
	pub model: String,
	/// The product's version.
	pub version: String,
	/// The machine it runs on.
	pub host: String,
	/// Free text: who is operating it, when that matters.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub operator: Option<String>,
}

impl AgentIdentity {
	/// Clean a single field: no control characters, no invisible or bidi
	/// characters (which could disguise a name), no leading or trailing
	/// whitespace, at most [`MAX_FIELD`] characters.
	fn clean(field: &str) -> String {
		field
			.chars()
			.filter(|c| !c.is_control() && !invisible(*c))
			.collect::<String>()
			.trim()
			.chars()
			.take(MAX_FIELD)
			.collect()
	}

	/// The identity as it would be stored: every field cleaned, empty
	/// optional fields dropped.
	#[must_use]
	pub fn sanitised(&self) -> Self {
		Self {
			product: Self::clean(&self.product),
			vendor: Self::clean(&self.vendor),
			model: Self::clean(&self.model),
			version: Self::clean(&self.version),
			host: Self::clean(&self.host),
			operator: self
				.operator
				.as_deref()
				.map(Self::clean)
				.filter(|o| !o.is_empty()),
		}
	}

	/// Whether this identity says enough to pair: a product and a host.
	/// Everything else may be empty — an honest agent can lack a vendor
	/// string, but it cannot lack a name.
	#[must_use]
	pub fn is_complete(&self) -> bool {
		let clean = self.sanitised();
		!clean.product.is_empty() && !clean.host.is_empty()
	}

	/// One line for the owner, such as
	/// `Claude Code (Anthropic) claude-opus-5-5 on "mini"`.
	#[must_use]
	pub fn summary(&self) -> String {
		let clean = self.sanitised();
		let mut out = String::new();
		out.push_str(&clean.product);
		if !clean.vendor.is_empty() {
			out.push_str(" (");
			out.push_str(&clean.vendor);
			out.push(')');
		}
		for field in [&clean.model, &clean.version] {
			if !field.is_empty() {
				out.push(' ');
				out.push_str(field);
			}
		}
		if !clean.host.is_empty() {
			out.push_str(" on ");
			out.push_str(&clean.host);
		}
		if let Some(operator) = &clean.operator {
			out.push_str(", operated by ");
			out.push_str(operator);
		}
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn identity() -> AgentIdentity {
		AgentIdentity {
			product: "Claude Code".into(),
			vendor: "Anthropic".into(),
			model: "claude-opus-5-5".into(),
			version: "2.1.0".into(),
			host: "mini".into(),
			operator: Some("Ada".into()),
		}
	}

	#[test]
	fn summarises_for_the_owner() {
		assert_eq!(
			identity().summary(),
			"Claude Code (Anthropic) claude-opus-5-5 2.1.0 on mini, operated by Ada"
		);
	}

	#[test]
	fn needs_a_product_and_a_host() {
		assert!(identity().is_complete());
		for incomplete in [
			AgentIdentity {
				product: String::new(),
				..identity()
			},
			AgentIdentity {
				host: String::new(),
				..identity()
			},
			AgentIdentity {
				product: "  \u{200b}".into(),
				..identity()
			},
		] {
			assert!(!incomplete.is_complete());
		}
		// Vendor, model, and version may be empty; operator may be absent.
		assert!(
			AgentIdentity {
				vendor: String::new(),
				model: String::new(),
				version: String::new(),
				operator: None,
				..identity()
			}
			.is_complete()
		);
	}

	#[test]
	fn strips_control_characters_and_caps_length() {
		let noisy = AgentIdentity {
			product: " Evil\u{202E}\u{7}Agent ".into(),
			host: "h".repeat(MAX_FIELD + 10),
			..identity()
		};
		let clean = noisy.sanitised();
		assert_eq!(clean.product, "EvilAgent");
		assert_eq!(clean.host.chars().count(), MAX_FIELD);
	}
}
