//! Thread plumbing shared by every source: a stop signal threads can sleep
//! on, and a status reporter that only speaks when something changes.

use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use quant_core::Sink;

/// Tells source threads to finish. Threads sleep on it rather than with
/// `thread::sleep`, so stopping wakes them at once.
#[derive(Clone, Default)]
pub(crate) struct Stop(Arc<(Mutex<bool>, Condvar)>);

impl Stop {
	/// Ask every thread holding this signal to stop.
	pub(crate) fn trigger(&self) {
		let (flag, condvar) = &*self.0;
		*flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
		condvar.notify_all();
	}

	/// Whether a stop was requested.
	pub(crate) fn is_set(&self) -> bool {
		*self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Sleep for `timeout` or until stopped. Returns whether a stop was
	/// requested.
	pub(crate) fn wait(&self, timeout: Duration) -> bool {
		let (flag, condvar) = &*self.0;
		let deadline = Instant::now().checked_add(timeout);
		let mut stopped = flag.lock().unwrap_or_else(PoisonError::into_inner);
		while !*stopped {
			let Some(deadline) = deadline else {
				stopped = condvar
					.wait(stopped)
					.unwrap_or_else(PoisonError::into_inner);
				continue;
			};
			let left = deadline.saturating_duration_since(Instant::now());
			if left.is_zero() {
				break;
			}
			stopped = condvar
				.wait_timeout(stopped, left)
				.unwrap_or_else(PoisonError::into_inner)
				.0;
		}
		*stopped
	}
}

/// Reports one source's state to the sink, skipping repeats.
pub(crate) struct Status {
	sink: Arc<dyn Sink>,
	name: &'static str,
	last: Option<(bool, Option<String>)>,
}

impl Status {
	pub(crate) fn new(sink: Arc<dyn Sink>, name: &'static str) -> Self {
		Self {
			sink,
			name,
			last: None,
		}
	}

	/// Report `running` with an optional explanation, unless it is what was
	/// reported last.
	pub(crate) fn set(&mut self, running: bool, detail: Option<&str>) {
		let next = (running, detail.map(str::to_owned));
		if self.last.as_ref() == Some(&next) {
			return;
		}
		if let Some(detail) = &next.1 {
			tracing::info!(source = self.name, running, detail, "capture status");
		} else {
			tracing::info!(source = self.name, running, "capture status");
		}
		self.sink.status(self.name, running, next.1.clone());
		self.last = Some(next);
	}

	/// Report the source as running normally.
	pub(crate) fn running(&mut self) {
		self.set(true, None);
	}

	/// Report the source as stopped with a reason.
	pub(crate) fn stopped(&mut self, detail: &str) {
		self.set(false, Some(detail));
	}
}

#[cfg(test)]
mod tests {
	use std::thread;

	use super::*;

	#[test]
	fn stop_wakes_a_waiting_thread_promptly() {
		let stop = Stop::default();
		let sleeper = {
			let stop = stop.clone();
			thread::spawn(move || {
				let began = Instant::now();
				let stopped = stop.wait(Duration::from_secs(30));
				(stopped, began.elapsed())
			})
		};
		thread::sleep(Duration::from_millis(20));
		stop.trigger();
		let (stopped, waited) = sleeper.join().unwrap();
		assert!(stopped);
		assert!(waited < Duration::from_secs(5));
		assert!(stop.is_set());
	}

	#[test]
	fn wait_times_out_without_a_stop() {
		let stop = Stop::default();
		assert!(!stop.wait(Duration::from_millis(5)));
	}
}
