//! The `input` source: key, click and scroll counts per interval.
//!
//! Reads the window server's cumulative per-event-type counters. There is no
//! event tap and no access to which keys were pressed.

use std::time::Duration;

use jiff::Timestamp;
use objc2_core_graphics::CGEventSource;
use objc2_core_graphics::CGEventSourceStateID;
use objc2_core_graphics::CGEventType;
use quant_core::SeriesBatch;

use super::Context;
use crate::config::InputConfig;
use crate::counters::Counts;
use crate::stream;

/// Count input until stopped, emitting one row per interval with activity.
pub(crate) fn run(cx: &Context, config: &InputConfig) {
	let mut status = cx.status("input");
	let interval = Duration::from_secs(config.interval_secs.max(1));
	let mut previous = read();
	if previous.is_zero() {
		status.set(
			true,
			Some("input counters read zero; counts may be unavailable"),
		);
	} else {
		status.running();
	}
	let mut start = Timestamp::now();
	loop {
		let stopped = cx.stop.wait(interval);
		let current = read();
		let delta = current.since(previous);
		if !delta.is_zero() {
			emit(cx, start, delta.values());
		}
		previous = current;
		start = Timestamp::now();
		if stopped {
			break;
		}
	}
	status.stopped("stopped");
}

fn emit(cx: &Context, start: Timestamp, values: [f64; 3]) {
	let batch = SeriesBatch {
		stream: stream("input.activity"),
		device: cx.device,
		start,
		channels: vec!["keys".into(), "clicks".into(), "scroll".into()],
		offsets_ms: vec![0],
		values: values.to_vec(),
	};
	if let Err(error) = cx.sink.series(batch) {
		tracing::warn!(%error, "sink refused input activity");
	}
}

/// The current cumulative counters.
fn read() -> Counts {
	let count = |kind| {
		CGEventSource::counter_for_event_type(CGEventSourceStateID::CombinedSessionState, kind)
	};
	Counts {
		keys: count(CGEventType::KeyDown),
		clicks: count(CGEventType::LeftMouseDown)
			.wrapping_add(count(CGEventType::RightMouseDown))
			.wrapping_add(count(CGEventType::OtherMouseDown)),
		scroll: count(CGEventType::ScrollWheel),
	}
}
