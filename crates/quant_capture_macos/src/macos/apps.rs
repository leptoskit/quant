//! The `apps` source: focus spans and device state transitions.

use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use objc2::rc::autoreleasepool;
use quant_core::Record;

use super::Context;
use super::system;
use crate::config::AppsConfig;
use crate::focus::Focus;
use crate::focus::FocusTracker;
use crate::focus::PresenceTracker;
use crate::focus::Span;
use crate::privacy::Exclusions;
use crate::stream;

/// Poll focus and presence until stopped.
pub(crate) fn run(cx: &Context, config: &AppsConfig, exclusions: &Exclusions) {
	let mut status = cx.status("apps");
	let poll = Duration::from_millis(config.poll_ms.max(100));
	#[allow(clippy::cast_precision_loss)]
	let idle_after = config.idle_secs.max(1) as f64;
	// A poll arriving this much later than due means the machine slept.
	let max_gap = SignedDuration::try_from(poll * 10)
		.unwrap_or_default()
		.max(SignedDuration::from_secs(30));
	let mut focus = FocusTracker::default();
	let mut presence = PresenceTracker::new(max_gap);
	let mut titles_readable = None;

	loop {
		autoreleasepool(|_| {
			let readable = system::screen_capture_allowed();
			if titles_readable != Some(readable) {
				titles_readable = Some(readable);
				if readable {
					status.running();
				} else {
					status.set(
						true,
						Some("window titles unavailable: screen recording permission not granted"),
					);
				}
			}
			tick(cx, idle_after, exclusions, &mut focus, &mut presence);
		});
		if cx.stop.wait(poll) {
			break;
		}
	}
	if let Some(span) = focus.close(Timestamp::now()) {
		emit_span(cx, span);
	}
	status.stopped("stopped");
}

fn tick(
	cx: &Context,
	idle_after: f64,
	exclusions: &Exclusions,
	focus: &mut FocusTracker,
	presence: &mut PresenceTracker,
) {
	let now = Timestamp::now();
	let (state, idle_for) = system::presence(idle_after);
	tracing::trace!(?state, idle_for, "presence");
	let (transitions, slept_at) = presence.observe(state, now);
	if let Some(at) = slept_at
		&& let Some(span) = focus.close(at)
	{
		emit_span(cx, span);
	}
	for transition in transitions {
		cx.emit(
			Record::new(stream("device.state"), cx.device)
				.starting(transition.at)
				.with("state", transition.state),
		);
	}
	if state.active() {
		let current = system::frontmost().map(|current| redact(current, exclusions));
		if let Some(span) = focus.observe(current, now) {
			emit_span(cx, span);
		}
	} else {
		// An idle span really ended at the last input, not when we noticed.
		let ended = if state.locked || state.display_asleep {
			now
		} else {
			now.checked_sub(SignedDuration::from_secs_f64(idle_for))
				.unwrap_or(now)
		};
		if let Some(span) = focus.close(ended) {
			emit_span(cx, span);
		}
	}
}

/// Drop the window title of excluded apps and private windows; the app name
/// alone is kept so time spent there still counts.
fn redact(mut focus: Focus, exclusions: &Exclusions) -> Focus {
	if exclusions.excludes(focus.bundle_id.as_deref(), focus.title.as_deref()) {
		focus.title = None;
	}
	focus
}

fn emit_span(cx: &Context, span: Span) {
	let mut record = Record::new(stream("apps.focus"), cx.device)
		.starting(span.start)
		.ending(span.end)
		.with("app", span.focus.app);
	if let Some(bundle_id) = span.focus.bundle_id {
		record = record.with("bundle_id", bundle_id);
	}
	if let Some(title) = span.focus.title {
		record = record.with("title", title);
	}
	cx.emit(record);
}
