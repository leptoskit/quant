//! The pairing link: how a phone learns the address, the pinned
//! fingerprint, and the relay parameters.
//!
//! `quant link` prints `quant://link?addr=…&fp=…&vault=…`, and adds
//! `relay=…&rid=…` when the daemon reaches a relay. Clients parse the whole
//! thing here so the query syntax has one definition.

use crate::key::valid_rid;

/// The port `quant daemon --tcp` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7717;

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
	/// A link to a daemon at `addr` (`host:port`), pinned to `fingerprint`
	/// and serving `vault`. The relay parameters are written only when both
	/// are present.
	pub fn new(
		addr: impl Into<String>,
		fingerprint: impl Into<String>,
		vault: impl Into<String>,
		relay: Option<String>,
		rid: Option<String>,
	) -> Self {
		Self {
			addr: Some(addr.into()),
			fp: Some(fingerprint.into()),
			vault: Some(vault.into()),
			relay,
			rid,
		}
	}

	/// A link to a daemon at `host` and `port`, bracketing IPv6 literals so
	/// `addr=` always parses as a valid `host:port`.
	pub fn from_host_port(
		host: &str,
		port: u16,
		fingerprint: impl Into<String>,
		vault: impl Into<String>,
		relay: Option<String>,
		rid: Option<String>,
	) -> Self {
		Self::new(format_host_port(host, port), fingerprint, vault, relay, rid)
	}

	/// A relay-only link with no direct LAN address, when the daemon has a
	/// live relay rendezvous id.
	pub fn relay_only(
		fingerprint: impl Into<String>,
		vault: impl Into<String>,
		relay: impl Into<String>,
		rid: impl Into<String>,
	) -> Self {
		Self {
			addr: None,
			fp: Some(fingerprint.into()),
			vault: Some(vault.into()),
			relay: Some(relay.into()),
			rid: Some(rid.into()),
		}
	}

	/// The `quant://link?…` URL this link names. `quant link` prints it and
	/// the desktop window shows it, so both come from this one writer.
	pub fn to_url(&self) -> String {
		let mut query: Vec<String> = Vec::with_capacity(5);
		if let Some(addr) = &self.addr {
			query.push(format!("addr={addr}"));
		}
		if let Some(fingerprint) = &self.fp {
			query.push(format!("fp={fingerprint}"));
		}
		if let Some(vault) = &self.vault {
			query.push(format!("vault={vault}"));
		}
		if let (Some(relay), Some(rid)) = (&self.relay, &self.rid) {
			// The scheme's colon and slashes stay out of the query syntax.
			let encoded = relay.replace(':', "%3A").replace('/', "%2F");
			query.push(format!("relay={encoded}"));
			query.push(format!("rid={rid}"));
		}
		if query.is_empty() {
			"quant://link".to_owned()
		} else {
			format!("quant://link?{}", query.join("&"))
		}
	}

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

/// Format `host` and `port` as `host:port`, bracketing IPv6 literals without
/// double-bracketing an already-bracketed host.
pub fn format_host_port(host: &str, port: u16) -> String {
	let bare = host
		.strip_prefix('[')
		.and_then(|inner| inner.strip_suffix(']'))
		.unwrap_or(host);
	if bare.contains(':') {
		format!("[{bare}]:{port}")
	} else {
		format!("{bare}:{port}")
	}
}

/// Split `host:port` or `[ipv6]:port`, rejecting bare IPv6 literals without a
/// port and empty hosts.
pub fn split_host_port(address: &str) -> Option<(String, u16)> {
	if let Some(rest) = address.strip_prefix('[') {
		let (host, port) = rest.split_once("]:")?;
		let port = port.parse().ok()?;
		return (!host.is_empty()).then(|| (host.to_owned(), port));
	}
	let (host, port) = address.rsplit_once(':')?;
	if host.is_empty() || host.contains(':') {
		return None;
	}
	let port = port.parse().ok()?;
	Some((host.to_owned(), port))
}

/// Whether another device on the network could use `host` (neither loopback
/// nor an unspecified wildcard address).
pub fn is_reachable_host(host: &str) -> bool {
	let bare = host
		.strip_prefix('[')
		.and_then(|inner| inner.strip_suffix(']'))
		.unwrap_or(host);
	if bare.is_empty() {
		return false;
	}
	match bare.parse::<std::net::IpAddr>() {
		Ok(ip) => !ip.is_loopback() && !ip.is_unspecified(),
		Err(_) => !bare.eq_ignore_ascii_case("localhost"),
	}
}

/// The IPv4 address this machine uses to reach the network, with no packet
/// sent.
///
/// A UDP socket bound to `0.0.0.0:0` and connected to a documentation
/// address asks the kernel which source address it would use. `None` means
/// the machine has no non-loopback IPv4 route right now.
pub fn lan_address() -> Option<String> {
	let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
	socket.connect("192.0.2.1:9").ok()?;
	let ip = socket.local_addr().ok()?.ip();
	(!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// The IPv6 address this machine uses to reach the network, with no packet
/// sent.
pub fn lan_address_v6() -> Option<String> {
	let socket = std::net::UdpSocket::bind("[::]:0").ok()?;
	socket.connect("[2001:db8::1]:9").ok()?;
	let ip = socket.local_addr().ok()?.ip();
	(!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_built_link_round_trips() {
		let link = Link::from_host_port("192.168.1.4", 7717, "abc123", "v1", None, None);
		let url = link.to_url();
		assert_eq!(url, "quant://link?addr=192.168.1.4:7717&fp=abc123&vault=v1");
		assert_eq!(Link::parse(&url).unwrap(), link);
	}

	#[test]
	fn ipv6_hosts_are_bracketed_once_and_round_trip() {
		assert_eq!(format_host_port("2001:db8::1", 9000), "[2001:db8::1]:9000");
		assert_eq!(
			format_host_port("[2001:db8::1]", 9000),
			"[2001:db8::1]:9000"
		);
		assert_eq!(format_host_port("192.168.1.4", 7717), "192.168.1.4:7717");

		let link = Link::from_host_port("2001:db8::1", 9000, "abc123", "v1", None, None);
		let url = link.to_url();
		assert_eq!(
			url,
			"quant://link?addr=[2001:db8::1]:9000&fp=abc123&vault=v1"
		);
		assert_eq!(Link::parse(&url).unwrap(), link);
	}

	#[test]
	fn split_host_port_handles_ipv4_ipv6_and_rejects_ambiguous_inputs() {
		assert_eq!(
			split_host_port("192.168.1.4:9000"),
			Some(("192.168.1.4".into(), 9000))
		);
		assert_eq!(
			split_host_port("[fe80::1]:7717"),
			Some(("fe80::1".into(), 7717))
		);
		assert_eq!(split_host_port("2001:db8::1"), None);
		assert_eq!(split_host_port("no-port"), None);
		assert_eq!(split_host_port(":7717"), None);
		assert_eq!(split_host_port("[]:7717"), None);
	}

	#[test]
	fn reachability_rejects_loopback_and_unspecified_hosts() {
		assert!(!is_reachable_host("127.0.0.1"));
		assert!(!is_reachable_host("::1"));
		assert!(!is_reachable_host("[::1]"));
		assert!(!is_reachable_host("0.0.0.0"));
		assert!(!is_reachable_host("::"));
		assert!(!is_reachable_host("localhost"));
		assert!(!is_reachable_host("LOCALHOST"));
		assert!(!is_reachable_host(""));
		assert!(is_reachable_host("192.168.1.4"));
		assert!(is_reachable_host("2001:db8::1"));
		assert!(is_reachable_host("studio.local"));
	}

	#[test]
	fn a_relay_only_link_round_trips() {
		let rid = "abcdefghijklmnopqrstuvwxyz";
		let link = Link::relay_only("abc123", "v1", "wss://relay.example", rid);
		let url = link.to_url();
		assert_eq!(
			url,
			"quant://link?fp=abc123&vault=v1&relay=wss%3A%2F%2Frelay.example&\
			 rid=abcdefghijklmnopqrstuvwxyz"
		);
		assert_eq!(Link::parse(&url).unwrap(), link);
		assert!(link.has_relay());
	}

	#[test]
	fn a_built_relay_link_encodes_the_relay_url() {
		let rid = "abcdefghijklmnopqrstuvwxyz";
		let link = Link::new(
			"192.168.1.4:7717",
			"abc123",
			"v1",
			Some("wss://relay.example".into()),
			Some(rid.into()),
		);
		let url = link.to_url();
		assert_eq!(
			url,
			"quant://link?addr=192.168.1.4:7717&fp=abc123&vault=v1&relay=wss%3A%2F%2Frelay.\
			 example&rid=abcdefghijklmnopqrstuvwxyz"
		);
		let parsed = Link::parse(&url).unwrap();
		assert_eq!(parsed.relay.as_deref(), Some("wss://relay.example"));
		assert!(parsed.has_relay());
	}

	#[test]
	fn a_relay_without_a_rid_is_not_written() {
		let link = Link::new(
			"host:1",
			"abc",
			"v1",
			Some("wss://relay.example".into()),
			None,
		);
		assert_eq!(link.to_url(), "quant://link?addr=host:1&fp=abc&vault=v1");
	}

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
