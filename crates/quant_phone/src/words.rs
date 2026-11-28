//! How the phone words what it knows: the status line, times, and the
//! command that approves it.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_client::Endpoint;
use quant_core::Family;
use quant_model::phone::Link;
use quant_model::phone::Storage;

/// The status line.
pub fn headline(link: &Link, storage: &Storage, collecting: bool, waiting: u64) -> String {
	if let Storage::Failed { .. } = storage {
		return "This phone's storage can't be opened".to_owned();
	}
	let queue = match waiting {
		0 => String::new(),
		1 => " · 1 reading waiting".to_owned(),
		n => format!(" · {n} readings waiting"),
	};
	match link {
		Link::Unpaired => "Not connected to your vault".to_owned(),
		Link::Connecting => "Connecting to your vault…".to_owned(),
		Link::Pending { code } => format!("Waiting for approval · {code}"),
		Link::Revoked => "Your vault no longer accepts this phone".to_owned(),
		_ if !collecting => format!("Not collecting{queue}"),
		Link::Unreachable { .. } => format!("Collecting · vault out of reach{queue}"),
		Link::Ready => format!("Collecting{queue}"),
	}
}

/// The command the owner runs on their computer to approve a device with
/// explicit `scopes`. A wildcard is quoted so zsh does not try to expand it.
pub fn approve_for(code: &str, scopes: &[&str]) -> String {
	let mut command = format!("quant approve {code}");
	for scope in scopes {
		command.push_str(" --scope ");
		if scope.contains('*') {
			command.push('\'');
			command.push_str(scope);
			command.push('\'');
		} else {
			command.push_str(scope);
		}
	}
	command
}

/// Where the vault is, as the owner would recognise it.
pub fn vault(endpoint: &Endpoint) -> String {
	match endpoint {
		Endpoint::Relay {
			address: Some(address),
			url,
			..
		} => format!("{address}, or through {}", host(url)),
		Endpoint::Relay { url, .. } => format!("through {}", host(url)),
		Endpoint::Tls { address, .. } | Endpoint::Tcp(address) => address.clone(),
		Endpoint::Unix(path) => path.display().to_string(),
	}
}

fn host(url: &str) -> &str {
	url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// A time of day, "15:30".
pub fn clock(at: Timestamp, tz: &TimeZone) -> String {
	at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
}

/// A stream's short name: `health.resting_heart_rate` reads "Resting heart
/// rate", and common short forms keep their capitals.
pub fn stream_label(id: &str) -> String {
	let leaf = id.rsplit('.').next().unwrap_or(id);
	leaf.split('_')
		.enumerate()
		.map(|(index, word)| {
			match word {
				"hrv" => "HRV".to_owned(),
				"sdnn" => "SDNN".to_owned(),
				"rmssd" => "RMSSD".to_owned(),
				"spo2" => "SpO2".to_owned(),
				"ecg" => "ECG".to_owned(),
				"bpm" => "BPM".to_owned(),
				_ if index == 0 => {
					let mut chars = word.chars();
					match chars.next() {
						Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
						None => String::new(),
					}
				}
				_ => word.to_owned(),
			}
		})
		.collect::<Vec<_>>()
		.join(" ")
}

/// A family's name on the screen.
pub fn family_label(family: Family) -> &'static str {
	match family {
		Family::Screen => "Screen",
		Family::Audio => "Audio",
		Family::Apps => "Apps",
		Family::Input => "Input",
		Family::Health => "Health",
		Family::Motion => "Motion",
		Family::Location => "Location",
		Family::Device => "Device",
		Family::Comms => "Comms",
		Family::Sensor => "Sensors",
		Family::Memory => "Memory",
	}
}

/// One line of prose, collapsed to a bounded length.
pub fn shorten(text: &str, max: usize) -> String {
	let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
	if text.chars().count() <= max {
		return text;
	}
	let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
	short.push('…');
	short
}

/// A span of seconds as "45s", "12m", "2h 5m", or "3h".
pub fn duration(seconds: f64) -> String {
	if !seconds.is_finite() || seconds <= 0.0 {
		return "0m".to_owned();
	}
	#[allow(
		clippy::cast_possible_truncation,
		clippy::cast_precision_loss,
		clippy::cast_sign_loss
	)]
	let total = seconds.round().min(u64::MAX as f64) as u64;
	let hours = total / 3600;
	let minutes = (total % 3600) / 60;
	if hours > 0 && minutes > 0 {
		format!("{hours}h {minutes}m")
	} else if hours > 0 {
		format!("{hours}h")
	} else if minutes > 0 {
		format!("{minutes}m")
	} else {
		format!("{total}s")
	}
}

/// A time relative to `now`: "Today 14:03", "Yesterday 09:12", or
/// "Mon 3 Nov 18:40".
pub fn when(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
	let at = at.to_zoned(tz.clone());
	let today = now.to_zoned(tz.clone()).date();
	let day = at.date();
	let time = at.strftime("%H:%M");
	if day == today {
		format!("Today {time}")
	} else if today.yesterday().ok() == Some(day) {
		format!("Yesterday {time}")
	} else {
		at.strftime("%a %-d %b %H:%M").to_string()
	}
}

#[cfg(test)]
mod tests {
	use quant_model::phone::PHONE_SCOPES;
	use quant_model::phone::WATCH_SCOPES;

	use super::*;

	#[test]
	fn the_status_line_says_where_the_phone_stands() {
		let open = Storage::Open;
		assert_eq!(
			headline(&Link::Unpaired, &open, true, 0),
			"Not connected to your vault"
		);
		assert_eq!(
			headline(
				&Link::Pending {
					code: "K7QD-M2XA".into()
				},
				&open,
				true,
				4
			),
			"Waiting for approval · K7QD-M2XA"
		);
		assert_eq!(headline(&Link::Ready, &open, true, 0), "Collecting");
		assert_eq!(
			headline(&Link::Ready, &open, true, 1),
			"Collecting · 1 reading waiting"
		);
		assert_eq!(
			headline(
				&Link::Unreachable {
					reason: "timed out".into()
				},
				&open,
				true,
				12
			),
			"Collecting · vault out of reach · 12 readings waiting"
		);
		assert_eq!(
			headline(&Link::Ready, &open, false, 3),
			"Not collecting · 3 readings waiting"
		);
		assert_eq!(
			headline(
				&Link::Ready,
				&Storage::Failed { reason: "x".into() },
				true,
				0
			),
			"This phone's storage can't be opened"
		);
	}

	#[test]
	fn the_approval_command_names_every_scope_the_phone_and_watch_ask_for() {
		assert_eq!(
			approve_for("K7QD-M2XA", &PHONE_SCOPES),
			"quant approve K7QD-M2XA --scope location:write --scope apps:write --scope \
			 device:write --scope health:write --scope motion:write --scope sensor:write"
		);
		assert_eq!(
			approve_for("K7QD-M2XA", &WATCH_SCOPES),
			"quant approve K7QD-M2XA --scope health:write --scope motion:write --scope \
			 location:write --scope device:write --scope sensor:write"
		);
		assert_eq!(
			approve_for("K7QD-M2XA", &["*:summary"]),
			"quant approve K7QD-M2XA --scope '*:summary'"
		);
	}

	#[test]
	fn the_vault_is_named_by_where_it_is() {
		let fingerprint = [0; 32];
		assert_eq!(
			vault(&Endpoint::Tls {
				address: "192.168.1.4:7717".into(),
				fingerprint
			}),
			"192.168.1.4:7717"
		);
		assert_eq!(
			vault(&Endpoint::Relay {
				url: "wss://relay.example".into(),
				rid: "r".into(),
				fingerprint,
				address: Some("192.168.1.4:7717".into()),
			}),
			"192.168.1.4:7717, or through relay.example"
		);
	}

	#[test]
	fn times_read_relative_to_today() {
		let tz = TimeZone::UTC;
		let now: Timestamp = "2026-11-18T18:00:00Z".parse().unwrap();
		assert_eq!(
			when("2026-11-18T14:03:00Z".parse().unwrap(), now, &tz),
			"Today 14:03"
		);
		assert_eq!(
			when("2026-11-17T09:12:00Z".parse().unwrap(), now, &tz),
			"Yesterday 09:12"
		);
		assert_eq!(clock(now, &tz), "18:00");
	}

	#[test]
	fn streams_and_families_read_as_screen_labels() {
		assert_eq!(stream_label("health.heart_rate"), "Heart rate");
		assert_eq!(stream_label("health.hrv_sdnn"), "HRV SDNN");
		assert_eq!(stream_label("health.spo2"), "SpO2");
		assert_eq!(
			stream_label("sensor.ble.running_cadence"),
			"Running cadence"
		);
		assert_eq!(family_label(quant_core::Family::Health), "Health");
		assert_eq!(family_label(quant_core::Family::Sensor), "Sensors");
	}

	#[test]
	fn prose_and_durations_stay_bounded_and_readable() {
		assert_eq!(shorten("  one   two  ", 20), "one two");
		assert_eq!(shorten("abcdefgh", 5), "abcd…");
		assert_eq!(duration(0.0), "0m");
		assert_eq!(duration(f64::NAN), "0m");
		assert_eq!(duration(45.0), "45s");
		assert_eq!(duration(12.0 * 60.0), "12m");
		assert_eq!(duration(2.0 * 3600.0 + 5.0 * 60.0), "2h 5m");
		assert_eq!(duration(3.0 * 3600.0), "3h");
	}
}
