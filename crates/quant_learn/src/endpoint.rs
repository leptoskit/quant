//! Model endpoints are literal loopback addresses, nothing else.

use std::net::IpAddr;

use serde::Deserialize;
use serde::Serialize;

/// A model endpoint: plain `http://` to a literal loopback address,
/// optionally with a port, such as `http://127.0.0.1:11434` or
/// `http://[::1]:8080`. Hostnames are refused, so a DNS change or a proxy
/// can never redirect vault text off the machine; non-loopback addresses
/// are refused the same way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEndpoint {
	/// The loopback address.
	pub addr: IpAddr,
	/// The port; 80 when none is given.
	pub port: u16,
}

impl ModelEndpoint {
	/// Parse `http://127.0.0.1:11434`, `http://[::1]:8080`, or a bare
	/// `127.0.0.1:11434`. Anything else — a hostname, a non-loopback
	/// address, a path or query, a port that is not a number — is refused.
	pub fn parse(s: &str) -> Result<Self, String> {
		let rest = s.strip_prefix("http://").unwrap_or(s);
		if rest.is_empty()
			|| rest
				.contains(['/', '?', '#', ' '])
			|| s.starts_with("https://")
		{
			return Err(format!(
				"the model endpoint `{s}` is not a plain http:// loopback address"
			));
		}
		let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
			let (host, tail) = inner
				.split_once(']')
				.ok_or_else(|| format!("bad endpoint `{s}`"))?;
			let port = tail.strip_prefix(':').filter(|p| !p.is_empty());
			(host, port)
		} else {
			match rest.rsplit_once(':') {
				Some((host, port)) if !host.contains(':') && !port.is_empty() => {
					(host, Some(port))
				}
				_ => (rest, None),
			}
		};
		if host.is_empty() {
			return Err(format!("bad endpoint `{s}`"));
		}
		let addr: IpAddr = host
			.parse()
			.map_err(|_| format!("the model endpoint `{s}` is not a literal IP address"))?;
		if !addr.is_loopback() {
			return Err(format!(
				"the model endpoint `{s}` is not a loopback address; vault text may only reach \
				 127.0.0.1 or ::1"
			));
		}
		let port = match port {
			Some(port) => port
				.parse::<u16>()
				.map_err(|_| format!("bad port in `{s}`"))?,
			None => 80,
		};
		Ok(Self { addr, port })
	}

	/// The `host:port` to connect to.
	pub fn address(&self) -> String {
		match self.addr {
			IpAddr::V6(ip) => format!("[{ip}]:{}", self.port),
			IpAddr::V4(ip) => format!("{ip}:{}", self.port),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_literal_loopback_endpoints() {
		let endpoint = ModelEndpoint::parse("http://127.0.0.1:11434").unwrap();
		assert_eq!(endpoint.address(), "127.0.0.1:11434");
		assert_eq!(ModelEndpoint::parse("127.0.0.1").unwrap().port, 80);
		assert_eq!(
			ModelEndpoint::parse("http://[::1]:8080").unwrap().address(),
			"[::1]:8080"
		);
		assert_eq!(
			ModelEndpoint::parse("::1").unwrap().addr,
			"::1".parse::<IpAddr>().unwrap()
		);
	}

	#[test]
	fn refuses_hostnames_non_loopback_and_paths() {
		for bad in [
			"http://localhost:11434",
			"http://ollama.local:11434",
			"http://192.168.1.10:11434",
			"http://10.0.0.1:8080",
			"https://127.0.0.1:11434",
			"http://127.0.0.1:11434/v1",
			"http://127.0.0.1:port",
			"http://",
			"",
		] {
			assert!(
				ModelEndpoint::parse(bad).is_err(),
				"`{bad}` should be refused"
			);
		}
	}
}
