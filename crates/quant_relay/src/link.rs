//! The pairing link: how a phone learns the address, the pinned
//! fingerprint, and the relay parameters.
//!
//! `quant link` prints `quant://link?addr=…&fp=…&vault=…`, and adds
//! `relay=…&rid=…` when the daemon reaches a relay. Clients parse the whole
//! thing here so the query syntax has one definition.

use crate::key::valid_rid;

/// What a pairing link says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
	/// The daemon's direct TLS address, `host:port`.
	pub addr: Option<String>,
	/// The pinned certificate fingerprint, hex.
	pub fp: Option<String>,
	/// The vault id.
	pub vault: Option<String>,
	/// The relay address, `wss://host[:port]`.
	pub relay: Option<String>,
	/// The daemon's rendezvous id on that relay.
	pub rid: Option<String>,
}

/// Errors from parsing a link.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkError {
	/// Not a `quant://link` URL.
	#[error("not a quant:// link")]
	NotALink,
}

impl Link {
	/// Parse a `quant://link?…` URL. Unknown parameters are ignored, so new
	/// ones can be added without breaking old clients.
	pub fn parse(text: &str) -> Result<Self, LinkError> {
		let rest = text
			.strip_prefix("quant://link")
			.ok_or(LinkError::NotALink)?;
		let query = rest.strip_prefix('?').unwrap_or_default();
		let mut link = Link::default();
		for pair in query.split('&').filter(|pair| !pair.is_empty()) {
			let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
			let value = percent_decode(value);
			match name {
				"addr" => link.addr = Some(value),
				"fp" => link.fp = Some(value),
				"vault" => link.vault = Some(value),
				"relay" => link.relay = Some(value),
				// A rid that is not shaped like one is ignored, like any
				// other unknown parameter.
				"rid" => link.rid = valid_rid(&value).then_some(value),
				_ => {}
			}
		}
		Ok(link)
	}

	/// Whether this link can reach the daemon through a relay.
	pub fn has_relay(&self) -> bool {
		self.relay.is_some() && self.rid.is_some()
	}
}

/// Decode the few escapes a link's values use. A trailing `%` or a bad
/// escape passes through unchanged rather than failing the whole link.
fn percent_decode(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' && i + 2 < bytes.len() {
			let hi = (bytes[i + 1] as char).to_digit(16);
			let lo = (bytes[i + 2] as char).to_digit(16);
			if let (Some(hi), Some(lo)) = (hi, lo) {
				out.push(u8::try_from(hi * 16 + lo).expect("two hex digits are a byte"));
				i += 3;
				continue;
			}
		}
		out.push(bytes[i]);
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_every_parameter() {
		let rid = "abcdefghijklmnopqrstuvwxyz"; // 26 chars, base32 alphabet
		let link = Link::parse(&format!(
			"quant://link?addr=192.168.1.4:7717&fp=abc123&vault=v1&relay=wss%3A%2F%2Frelay.\
			 example&rid={rid}"
		))
		.unwrap();
		assert_eq!(link.addr.as_deref(), Some("192.168.1.4:7717"));
		assert_eq!(link.fp.as_deref(), Some("abc123"));
		assert_eq!(link.vault.as_deref(), Some("v1"));
		assert_eq!(link.relay.as_deref(), Some("wss://relay.example"));
		assert_eq!(link.rid.as_deref(), Some(rid));
		assert!(link.has_relay());
	}

	#[test]
	fn links_without_a_relay_still_parse() {
		let link = Link::parse("quant://link?addr=192.168.1.4:7717&fp=abc123&vault=v1").unwrap();
		assert!(link.relay.is_none() && link.rid.is_none());
		assert!(!link.has_relay());
	}

	#[test]
	fn junk_is_ignored_rather_than_fatal() {
		let link = Link::parse("quant://link?who=knows&rid=not-a-rid&addr=host:1").unwrap();
		assert_eq!(link.addr.as_deref(), Some("host:1"));
		assert_eq!(link.rid, None);
		assert!(!link.has_relay());
		assert!(Link::parse("quant://pair?addr=h").is_err());
	}
}
