//! Times as people (and agents) write them.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::Zoned;
use jiff::civil::Time;
use jiff::tz::TimeZone;

/// Parse a time:
///
/// - an RFC 3339 timestamp: `2026-11-06T09:30:00Z`
/// - a civil date, read in the local time zone: `2026-11-06`
/// - `now`, `today`, `yesterday` (local midnight)
/// - an offset from now: `-90m`, `-2h`, `-3d`, `-1w` (`s`, `m`, `h`, `d`, `w`)
pub fn parse(input: &str, now: Timestamp) -> Result<Timestamp, String> {
	parse_in(input, now, &TimeZone::system())
}

fn parse_in(input: &str, now: Timestamp, tz: &TimeZone) -> Result<Timestamp, String> {
	let input = input.trim();
	let midnight = |days_ago: i64| -> Result<Timestamp, String> {
		let today = now.to_zoned(tz.clone()).date();
		let day = today
			.checked_sub(jiff::Span::new().days(days_ago))
			.map_err(|e| e.to_string())?;
		day.to_datetime(Time::midnight())
			.to_zoned(tz.clone())
			.map(|z| z.timestamp())
			.map_err(|e| e.to_string())
	};
	match input {
		"now" => return Ok(now),
		"today" => return midnight(0),
		"yesterday" => return midnight(1),
		_ => {}
	}
	if let Some(offset) = input.strip_prefix('-') {
		let split = offset
			.find(|c: char| !c.is_ascii_digit())
			.unwrap_or(offset.len());
		let (number, unit) = offset.split_at(split);
		let n: i64 = number
			.parse()
			.map_err(|_| format!("bad offset `{input}`"))?;
		let seconds = match unit {
			"s" => 1,
			"m" | "min" => 60,
			"h" => 3600,
			"d" => 86_400,
			"w" => 604_800,
			_ => return Err(format!("bad unit in `{input}`; use s, m, h, d, or w")),
		};
		return now
			.checked_sub(SignedDuration::from_secs(n.saturating_mul(seconds)))
			.map_err(|e| e.to_string());
	}
	if let Ok(timestamp) = input.parse::<Timestamp>() {
		return Ok(timestamp);
	}
	if let Ok(zoned) = input.parse::<Zoned>() {
		return Ok(zoned.timestamp());
	}
	if let Ok(datetime) = input.parse::<jiff::civil::DateTime>() {
		return datetime
			.to_zoned(tz.clone())
			.map(|z| z.timestamp())
			.map_err(|e| e.to_string());
	}
	if let Ok(date) = input.parse::<jiff::civil::Date>() {
		return date
			.to_datetime(Time::midnight())
			.to_zoned(tz.clone())
			.map(|z| z.timestamp())
			.map_err(|e| e.to_string());
	}
	Err(format!(
		"cannot read `{input}` as a time; try `-2h`, `yesterday`, `2026-11-06`, or an RFC 3339 \
		 timestamp"
	))
}

/// Parse an optional time.
pub fn parse_opt(input: Option<&str>, now: Timestamp) -> Result<Option<Timestamp>, String> {
	input.map(|s| parse(s, now)).transpose()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_every_form() {
		let tz = TimeZone::UTC;
		let now: Timestamp = "2026-11-06T15:30:00Z".parse().unwrap();
		let at = |s: &str| parse_in(s, now, &tz).unwrap().to_string();
		assert_eq!(at("now"), "2026-11-06T15:30:00Z");
		assert_eq!(at("-90m"), "2026-11-06T14:00:00Z");
		assert_eq!(at("-2h"), "2026-11-06T13:30:00Z");
		assert_eq!(at("-1d"), "2026-11-05T15:30:00Z");
		assert_eq!(at("today"), "2026-11-06T00:00:00Z");
		assert_eq!(at("yesterday"), "2026-11-05T00:00:00Z");
		assert_eq!(at("2026-11-01"), "2026-11-01T00:00:00Z");
		assert_eq!(at("2026-11-01T08:00"), "2026-11-01T08:00:00Z");
		assert_eq!(at("2026-11-01T08:00:00+01:00"), "2026-11-01T07:00:00Z");
		assert!(parse_in("-3y", now, &tz).is_err());
		assert!(parse_in("whenever", now, &tz).is_err());
	}
}
