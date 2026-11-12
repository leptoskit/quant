//! How the app words what the daemon reports: the status line, times,
//! durations, and the result of saving a moment.

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_api::CaptureState;
use quant_api::Paused;
use quant_api::SavedClip;
use quant_model::Link;
use quant_model::Settings;

/// The status line: "Recording · 6 sources", "Paused until 15:30", or what
/// stands in the way.
pub fn headline(link: &Link, capture: Option<&CaptureState>, tz: &TimeZone) -> String {
	match link {
		Link::Starting => "Connecting to Quant…".to_owned(),
		Link::Unreachable { .. } => "Quant isn't running".to_owned(),
		Link::Pending { code } => format!("Waiting for approval · {code}"),
		Link::Revoked => "Access revoked".to_owned(),
		Link::Ready => {
			capture.map_or_else(|| "Connecting to Quant…".to_owned(), |c| recording(c, tz))
		}
	}
}

fn recording(capture: &CaptureState, tz: &TimeZone) -> String {
	match capture.paused {
		Some(Paused::UntilResumed) => return "Paused".to_owned(),
		Some(Paused::Until(until)) => return format!("Paused until {}", clock(until, tz)),
		None => {}
	}
	let total = capture.sources.len();
	let running = capture.sources.iter().filter(|s| s.running).count();
	match (running, total) {
		(_, 0) => "Not recording".to_owned(),
		(0, _) => "Not recording · every source stopped".to_owned(),
		(1, 1) => "Recording · 1 source".to_owned(),
		(running, total) if running == total => format!("Recording · {total} sources"),
		(running, total) => format!("Recording · {running} of {total} sources"),
	}
}

/// The warning entry for a missing permission.
pub fn permission_warning(pane: Settings) -> String {
	format!("{} is off · Open Settings…", pane.name())
}

/// A time of day, "15:30".
pub fn clock(at: Timestamp, tz: &TimeZone) -> String {
	at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
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

/// A length of time: "45 s", "15 min", "1 h 5 min".
pub fn duration(length: SignedDuration) -> String {
	let seconds = length.as_secs().max(0);
	if seconds < 60 {
		return format!("{seconds} s");
	}
	let minutes = (seconds + 30) / 60;
	match (minutes / 60, minutes % 60) {
		(0, minutes) => format!("{minutes} min"),
		(hours, 0) => format!("{hours} h"),
		(hours, minutes) => format!("{hours} h {minutes} min"),
	}
}

/// "1 day", "1 week", "30 days", or "No limit".
pub fn lookback(lookback: Option<SignedDuration>) -> String {
	let Some(lookback) = lookback else {
		return "No limit".to_owned();
	};
	let hours = lookback.as_hours();
	match hours {
		h if h % (24 * 7) == 0 && h > 0 && h / (24 * 7) < 5 => plural(h / (24 * 7), "week"),
		h if h % 24 == 0 && h > 0 => plural(h / 24, "day"),
		h => plural(h, "hour"),
	}
}

fn plural(n: i64, unit: &str) -> String {
	if n == 1 {
		format!("1 {unit}")
	} else {
		format!("{n} {unit}s")
	}
}

/// The rolling buffer window for "Save the last `minutes` minutes".
pub fn save_window(minutes: u32, now: Timestamp) -> (Timestamp, Timestamp) {
	(now - SignedDuration::from_mins(i64::from(minutes)), now)
}

/// The menu entry for saving.
pub fn save_label(minutes: u32) -> String {
	format!("Save the last {minutes} minutes")
}

/// What saving a moment came to.
pub fn saved(minutes: u32, clip: &SavedClip) -> (String, bool) {
	if clip.chunks == 0 || clip.bytes == 0 {
		return ("Nothing recorded in that window".to_owned(), true);
	}
	let kept = clip.to.duration_since(clip.from);
	let asked = SignedDuration::from_mins(i64::from(minutes));
	if kept + SignedDuration::from_secs(30) < asked {
		(
			format!("Saved {} (only that much was recorded)", duration(kept)),
			false,
		)
	} else {
		(format!("Saved the last {minutes} minutes"), false)
	}
}

/// A default title for a moment saved now: "Moment at 14:03".
pub fn moment_title(now: Timestamp, tz: &TimeZone) -> String {
	format!("Moment at {}", clock(now, tz))
}

/// The first line of a transcript, cut to about `limit` characters.
pub fn snippet(text: &str, limit: usize) -> Option<String> {
	let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
	if text.is_empty() {
		return None;
	}
	if text.chars().count() <= limit {
		return Some(text);
	}
	let cut: String = text.chars().take(limit).collect();
	let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
	Some(format!("{cut}…"))
}

#[cfg(test)]
mod tests {
	use quant_api::Permission;
	use quant_api::SourceState;

	use super::*;

	fn utc() -> TimeZone {
		TimeZone::UTC
	}

	fn at(s: &str) -> Timestamp {
		s.parse().unwrap()
	}

	fn source(name: &str, running: bool, needs: Option<Permission>) -> SourceState {
		SourceState {
			source: name.into(),
			running,
			needs,
		}
	}

	fn capture(sources: Vec<SourceState>, paused: Option<Paused>) -> CaptureState {
		CaptureState {
			version: "0.1.0".into(),
			sources,
			paused,
		}
	}

	#[test]
	fn counts_running_sources() {
		let all: Vec<_> = ["apps", "input", "screen", "audio", "motion", "health"]
			.into_iter()
			.map(|name| source(name, true, None))
			.collect();
		let line = headline(&Link::Ready, Some(&capture(all, None)), &utc());
		assert_eq!(line, "Recording · 6 sources");

		let some = vec![
			source("apps", true, None),
			source("screen", false, Some(Permission::ScreenRecording)),
			source("audio", false, None),
		];
		let line = headline(&Link::Ready, Some(&capture(some, None)), &utc());
		assert_eq!(line, "Recording · 1 of 3 sources");

		let one = vec![source("apps", true, None)];
		let line = headline(&Link::Ready, Some(&capture(one, None)), &utc());
		assert_eq!(line, "Recording · 1 source");

		let none = vec![source("apps", false, None)];
		let line = headline(&Link::Ready, Some(&capture(none, None)), &utc());
		assert_eq!(line, "Not recording · every source stopped");
	}

	#[test]
	fn says_until_when_capture_is_paused() {
		let paused = capture(
			vec![source("apps", false, None)],
			Some(Paused::Until(at("2026-11-07T15:30:00Z"))),
		);
		assert_eq!(
			headline(&Link::Ready, Some(&paused), &utc()),
			"Paused until 15:30"
		);
		let forever = capture(vec![], Some(Paused::UntilResumed));
		assert_eq!(headline(&Link::Ready, Some(&forever), &utc()), "Paused");
	}

	#[test]
	fn says_what_stands_in_the_way() {
		assert_eq!(
			headline(
				&Link::Unreachable {
					reason: "no socket".into()
				},
				None,
				&utc()
			),
			"Quant isn't running"
		);
		assert_eq!(
			headline(
				&Link::Pending {
					code: "K7QD-M2XA".into()
				},
				None,
				&utc()
			),
			"Waiting for approval · K7QD-M2XA"
		);
		assert_eq!(headline(&Link::Revoked, None, &utc()), "Access revoked");
	}

	#[test]
	fn warns_with_the_permission_name() {
		assert_eq!(
			permission_warning(Settings::Microphone),
			"Microphone is off · Open Settings…"
		);
		assert_eq!(
			permission_warning(Settings::ScreenRecording),
			"Screen Recording is off · Open Settings…"
		);
	}

	#[test]
	fn save_windows_end_now() {
		let now = at("2026-11-07T15:30:00Z");
		for minutes in [5, 15, 30, 60] {
			let (from, to) = save_window(minutes, now);
			assert_eq!(to, now);
			assert_eq!(
				to.duration_since(from),
				SignedDuration::from_mins(i64::from(minutes))
			);
		}
		assert_eq!(save_label(15), "Save the last 15 minutes");
	}

	#[test]
	fn saving_nothing_says_so() {
		let now = at("2026-11-07T15:30:00Z");
		let (from, to) = save_window(15, now);
		let clip = |from, chunks, bytes| {
			SavedClip {
				id: quant_core::RecordId::new(),
				from,
				to,
				chunks,
				bytes,
			}
		};
		assert_eq!(
			saved(15, &clip(from, 0, 0)),
			("Nothing recorded in that window".to_owned(), true)
		);
		assert_eq!(
			saved(15, &clip(from, 15, 1_000)),
			("Saved the last 15 minutes".to_owned(), false)
		);
		let late = now - SignedDuration::from_mins(4);
		assert_eq!(
			saved(15, &clip(late, 4, 1_000)),
			(
				"Saved 4 min (only that much was recorded)".to_owned(),
				false
			)
		);
	}

	#[test]
	fn words_times_and_lengths() {
		let now = at("2026-11-07T15:30:00Z");
		assert_eq!(when(at("2026-11-07T14:03:00Z"), now, &utc()), "Today 14:03");
		assert_eq!(
			when(at("2026-11-06T09:12:00Z"), now, &utc()),
			"Yesterday 09:12"
		);
		assert_eq!(
			when(at("2026-11-03T18:40:00Z"), now, &utc()),
			"Tue 3 Nov 18:40"
		);
		assert_eq!(duration(SignedDuration::from_secs(45)), "45 s");
		assert_eq!(duration(SignedDuration::from_mins(15)), "15 min");
		assert_eq!(duration(SignedDuration::from_mins(60)), "1 h");
		assert_eq!(duration(SignedDuration::from_mins(65)), "1 h 5 min");
		assert_eq!(lookback(None), "No limit");
		assert_eq!(lookback(Some(SignedDuration::from_hours(24))), "1 day");
		assert_eq!(lookback(Some(SignedDuration::from_hours(168))), "1 week");
		assert_eq!(lookback(Some(SignedDuration::from_hours(720))), "30 days");
		assert_eq!(lookback(Some(SignedDuration::from_hours(5))), "5 hours");
		assert_eq!(moment_title(now, &utc()), "Moment at 15:30");
	}

	#[test]
	fn snippets_cut_at_a_word() {
		assert_eq!(snippet("  \n ", 10), None);
		assert_eq!(
			snippet("we agreed\non Tuesday", 40).unwrap(),
			"we agreed on Tuesday"
		);
		assert_eq!(
			snippet("we agreed on the quarterly budget", 20).unwrap(),
			"we agreed on the…"
		);
	}
}
