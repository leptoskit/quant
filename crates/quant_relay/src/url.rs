//! Where a relay lives, and the three URLs the protocol uses.
//!
//! A relay address is `wss://host[:port]` (`https://` is accepted and means
//! the same). The host as dialed — including any port — is part of every
//! handshake signature, so a proof for one relay cannot be replayed at
//! another that happens to share a host name.

use std::fmt;

/// Errors from parsing a relay address.
#[derive(Debug, thiserror::Error)]
pub enum UrlError {
	/// Not a URL at all.
	#[error("a relay address must be wss://host[:port]")]
	NotARelay,
	/// A URL, but not one this protocol speaks.
	#[error("a relay address must be wss:// or https://, not {0}")]
	WrongScheme(String),
	/// No host in it.
	#[error("a relay address needs a host")]
	NoHost,
}

/// A relay address, normalized to `wss://`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RelayUrl {
	scheme: String,
	/// The host and optional port, as dialed and as signed.
	pub authority: String,
}

impl RelayUrl {
	/// Parse `wss://host:port` or `https://host:port`, normalizing `http(s)`
	/// to `ws(s)`.
	pub fn parse(text: &str) -> Result<Self, UrlError> {
		let trimmed = text.trim().trim_end_matches('/');
		let (scheme, rest) = trimmed.split_once("://").ok_or(UrlError::NotARelay)?;
		let ws_scheme = match scheme {
			"wss" | "https" => "wss",
			"ws" | "http" => "ws",
			other => return Err(UrlError::WrongScheme(other.to_owned())),
		};
		let (host, port) = split_host_port(rest)?;
		let authority = match port {
			Some(port) => format!("{host}:{port}"),
			None => host.clone(),
		};
		Ok(Self {
			scheme: ws_scheme.to_owned(),
			authority,
		})
	}

	/// The scheme (`ws` or `wss`).
	pub fn scheme(&self) -> &str {
		&self.scheme
	}

	/// The listen URL the daemon keeps open.
	pub fn listen(&self) -> String {
		format!("{}://{}/v1/listen", self.scheme, self.authority)
	}

	/// The URL a client connects on for `rid`.
	pub fn connect(&self, rid: &str) -> String {
		format!("{}://{}/v1/connect/{rid}", self.scheme, self.authority)
	}

	/// The URL the daemon accepts `conn` on.
	pub fn accept(&self, conn: &str) -> String {
		format!("{}://{}/v1/accept/{conn}", self.scheme, self.authority)
	}
}

fn split_host_port(rest: &str) -> Result<(String, Option<u16>), UrlError> {
	if let Some((host, port)) = rest.rsplit_once(':')
		&& (!host.contains(']') || host.starts_with('['))
		&& let Ok(port) = port.parse::<u16>()
	{
		let host = host.trim_start_matches('[').trim_end_matches(']');
		if host.is_empty() {
			return Err(UrlError::NoHost);
		}
		return Ok((host.to_owned(), Some(port)));
	}
	if rest.is_empty() {
		return Err(UrlError::NoHost);
	}
	Ok((rest.to_owned(), None))
}

impl fmt::Display for RelayUrl {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}://{}", self.scheme, self.authority)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn addresses_normalize_and_keep_their_port() {
		for (text, display) in [
			("wss://relay.example", "wss://relay.example"),
			("wss://relay.example:8443/", "wss://relay.example:8443"),
			("https://relay.example", "wss://relay.example"),
			("http://127.0.0.1:8080", "ws://127.0.0.1:8080"),
		] {
			let url = RelayUrl::parse(text).unwrap();
			assert_eq!(url.to_string(), display, "{text}");
			assert_eq!(url.authority, display.split_once("://").unwrap().1);
		}
		assert!(RelayUrl::parse("relay.example").is_err());
		assert!(RelayUrl::parse("ftp://relay.example").is_err());
		assert!(RelayUrl::parse("wss://").is_err());
	}

	#[test]
	fn the_three_urls_are_shaped_as_the_protocol_says() {
		let url = RelayUrl::parse("wss://relay.example:8443").unwrap();
		assert_eq!(url.listen(), "wss://relay.example:8443/v1/listen");
		assert_eq!(
			url.connect("abc"),
			"wss://relay.example:8443/v1/connect/abc"
		);
		assert_eq!(
			url.accept("conn1"),
			"wss://relay.example:8443/v1/accept/conn1"
		);
	}
}
