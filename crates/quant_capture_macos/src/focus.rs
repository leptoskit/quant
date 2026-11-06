//! Focus spans and device presence, as pure state machines fed by polling.

use jiff::SignedDuration;
use jiff::Timestamp;

/// What had focus: an app and, when readable, its front window's title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Focus {
	/// The app's display name.
	pub(crate) app: String,
	/// The app's bundle id, when it has one.
	pub(crate) bundle_id: Option<String>,
	/// The focused window's title, when readable and not excluded.
	pub(crate) title: Option<String>,
}

impl Focus {
	/// A key that changes whenever the app or window does.
	pub(crate) fn key(&self) -> String {
		format!(
			"{}|{}|{}",
			self.bundle_id.as_deref().unwrap_or(&self.app),
			self.app,
			self.title.as_deref().unwrap_or_default()
		)
	}
}

/// A finished focus span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
	/// What had focus.
	pub(crate) focus: Focus,
	/// When it gained focus.
	pub(crate) start: Timestamp,
	/// When it lost focus.
	pub(crate) end: Timestamp,
}

/// Turns a stream of focus observations into spans.
#[derive(Debug, Default)]
pub(crate) struct FocusTracker {
	open: Option<(Focus, Timestamp)>,
}

impl FocusTracker {
	/// Note what has focus at `now`. Returns the span that just ended, if
	/// focus moved. `None` means nothing has focus.
	pub(crate) fn observe(&mut self, focus: Option<Focus>, now: Timestamp) -> Option<Span> {
		if let (Some((open, _)), Some(next)) = (&self.open, &focus)
			&& open == next
		{
			return None;
		}
		let ended = self.close(now);
		self.open = focus.map(|focus| (focus, now));
		ended
	}

	/// End the open span at `at` (never before it began).
	pub(crate) fn close(&mut self, at: Timestamp) -> Option<Span> {
		let (focus, start) = self.open.take()?;
		Some(Span {
			focus,
			start,
			end: at.max(start),
		})
	}

	/// What has focus now, if anything.
	#[cfg_attr(not(test), allow(dead_code))]
	pub(crate) fn current(&self) -> Option<&Focus> {
		self.open.as_ref().map(|(focus, _)| focus)
	}
}

/// One poll of whether anyone is at the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Presence {
	/// The screen is locked or the session is switched away.
	pub(crate) locked: bool,
	/// The main display is asleep.
	pub(crate) display_asleep: bool,
	/// No input for longer than the idle threshold.
	pub(crate) idle: bool,
}

impl Presence {
	/// Whether the user is at the machine and using it.
	pub(crate) fn active(self) -> bool {
		!self.locked && !self.display_asleep && !self.idle
	}
}

/// A device state transition worth a `device.state` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Transition {
	/// `locked`, `unlocked`, `display_off`, `display_on`, `idle`, `active`,
	/// `sleep`, or `wake`.
	pub(crate) state: &'static str,
	/// When it happened.
	pub(crate) at: Timestamp,
}

/// Turns presence polls into transitions, and notices when the machine slept
/// between polls (the poll gap is far longer than expected).
#[derive(Debug)]
pub(crate) struct PresenceTracker {
	last: Option<(Presence, Timestamp)>,
	max_gap: SignedDuration,
}

impl PresenceTracker {
	/// A tracker that treats a gap longer than `max_gap` between polls as a
	/// system sleep.
	pub(crate) fn new(max_gap: SignedDuration) -> Self {
		Self {
			last: None,
			max_gap,
		}
	}

	/// Record a poll. Returns the transitions since the previous poll, in
	/// order, and when the machine went to sleep if it did.
	pub(crate) fn observe(
		&mut self,
		now_state: Presence,
		now: Timestamp,
	) -> (Vec<Transition>, Option<Timestamp>) {
		let mut transitions = Vec::new();
		let mut slept_at = None;
		if let Some((before, seen)) = self.last {
			if now.duration_since(seen) > self.max_gap {
				slept_at = Some(seen);
				transitions.push(Transition {
					state: "sleep",
					at: seen,
				});
				transitions.push(Transition {
					state: "wake",
					at: now,
				});
			}
			let mut changed = |was: bool, is: bool, on: &'static str, off: &'static str| {
				if was != is {
					transitions.push(Transition {
						state: if is { on } else { off },
						at: now,
					});
				}
			};
			changed(before.locked, now_state.locked, "locked", "unlocked");
			changed(
				before.display_asleep,
				now_state.display_asleep,
				"display_off",
				"display_on",
			);
			changed(before.idle, now_state.idle, "idle", "active");
		}
		self.last = Some((now_state, now));
		(transitions, slept_at)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(secs: i64) -> Timestamp {
		Timestamp::from_second(1_700_000_000 + secs).unwrap()
	}

	fn focus(app: &str, title: &str) -> Option<Focus> {
		let bundle_id = Some(format!("com.example.{}", app.to_lowercase()));
		(!app.is_empty()).then(|| {
			Focus {
				app: app.to_owned(),
				bundle_id,
				title: Some(title.to_owned()),
			}
		})
	}

	#[test]
	fn same_focus_extends_the_open_span() {
		let mut tracker = FocusTracker::default();
		assert_eq!(tracker.observe(focus("Zed", "main.rs"), at(0)), None);
		assert_eq!(tracker.observe(focus("Zed", "main.rs"), at(1)), None);
		assert_eq!(tracker.observe(focus("Zed", "main.rs"), at(2)), None);
		assert_eq!(tracker.current().unwrap().app, "Zed");
	}

	#[test]
	fn app_or_title_change_ends_the_span() {
		let mut tracker = FocusTracker::default();
		tracker.observe(focus("Zed", "main.rs"), at(0));
		let span = tracker.observe(focus("Zed", "lib.rs"), at(5)).unwrap();
		assert_eq!((span.start, span.end), (at(0), at(5)));
		assert_eq!(span.focus.title.as_deref(), Some("main.rs"));

		let span = tracker.observe(focus("Safari", "News"), at(9)).unwrap();
		assert_eq!((span.start, span.end), (at(5), at(9)));
		assert_eq!(span.focus.title.as_deref(), Some("lib.rs"));
		assert_eq!(tracker.current().unwrap().app, "Safari");
	}

	#[test]
	fn losing_focus_ends_the_span_and_regaining_opens_one() {
		let mut tracker = FocusTracker::default();
		tracker.observe(focus("Zed", "main.rs"), at(0));
		let span = tracker.observe(None, at(3)).unwrap();
		assert_eq!(span.end, at(3));
		assert_eq!(tracker.observe(None, at(4)), None);
		assert_eq!(tracker.observe(focus("Zed", "main.rs"), at(6)), None);
		let span = tracker.close(at(8)).unwrap();
		assert_eq!((span.start, span.end), (at(6), at(8)));
		assert_eq!(tracker.close(at(9)), None);
	}

	#[test]
	fn closing_before_the_start_clamps() {
		let mut tracker = FocusTracker::default();
		tracker.observe(focus("Zed", "main.rs"), at(10));
		let span = tracker.close(at(4)).unwrap();
		assert_eq!(span.end, at(10));
	}

	#[test]
	fn presence_reports_transitions() {
		let mut tracker = PresenceTracker::new(SignedDuration::from_secs(10));
		let active = Presence::default();
		assert!(active.active());
		assert_eq!(tracker.observe(active, at(0)), (vec![], None));

		let locked = Presence {
			locked: true,
			..active
		};
		let (transitions, slept) = tracker.observe(locked, at(1));
		assert_eq!(
			transitions,
			vec![Transition {
				state: "locked",
				at: at(1)
			}]
		);
		assert_eq!(slept, None);
		assert!(!locked.active());

		let dark = Presence {
			display_asleep: true,
			..locked
		};
		let (transitions, _) = tracker.observe(dark, at(2));
		assert_eq!(
			transitions,
			vec![Transition {
				state: "display_off",
				at: at(2)
			}]
		);

		let (transitions, _) = tracker.observe(active, at(3));
		let states: Vec<_> = transitions.iter().map(|t| t.state).collect();
		assert_eq!(states, ["unlocked", "display_on"]);

		let idle = Presence {
			idle: true,
			..active
		};
		let (transitions, _) = tracker.observe(idle, at(4));
		assert_eq!(transitions[0].state, "idle");
		let (transitions, _) = tracker.observe(active, at(5));
		assert_eq!(transitions[0].state, "active");
	}

	#[test]
	fn a_long_poll_gap_is_a_sleep() {
		let mut tracker = PresenceTracker::new(SignedDuration::from_secs(10));
		tracker.observe(Presence::default(), at(0));
		let (transitions, slept) = tracker.observe(Presence::default(), at(600));
		assert_eq!(slept, Some(at(0)));
		assert_eq!(
			transitions,
			vec![
				Transition {
					state: "sleep",
					at: at(0)
				},
				Transition {
					state: "wake",
					at: at(600)
				}
			]
		);
	}

	#[test]
	fn focus_keys_distinguish_windows() {
		let a = focus("Zed", "main.rs").unwrap();
		let b = focus("Zed", "lib.rs").unwrap();
		assert_ne!(a.key(), b.key());
		assert_eq!(a.key(), a.clone().key());
	}
}
