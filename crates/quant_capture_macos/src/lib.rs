//! First-party desktop capture for macOS.
//!
//! [`spawn`] starts one thread per enabled source and hands everything it
//! captures to a [`Sink`]:
//!
//! - **screen**: a frame every few seconds while someone is at the machine,
//!   skipped when it looks like the last one, read with Vision OCR and stored
//!   as `screen.frame` records with an optional JPEG.
//! - **apps**: `apps.focus` spans of the frontmost app and window, plus
//!   `device.state` lock, display, idle and sleep transitions.
//! - **input**: `input.activity` key, click and scroll counts. Counts only,
//!   read from system counters; no event taps, never keystroke content.
//! - **audio** (off by default): speech segments from the microphone as
//!   `audio.transcript` records with a WAV clip and, with the `whisper`
//!   feature, a transcript.
//! - **rewind** (on by default): a rolling buffer of the last day of microphone
//!   and system audio as `audio.buffer` chunks, so a moment can be saved after
//!   it happened. It never transcribes while recording; see [`rewind`].
//!
//! Every source reports how it is doing through [`Sink::status`] under the
//! names `screen`, `apps`, `input`, `audio` and `rewind`, including when
//! macOS refuses a permission. On other platforms [`spawn`] reports every
//! source unavailable and starts nothing.

// Capture has to call Apple's Objective-C and C APIs.
#![allow(unsafe_code)]

mod aac;
mod config;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod control;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod counters;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod dhash;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod focus;
#[cfg(target_os = "macos")]
mod macos;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod privacy;
pub mod resample;
pub mod rewind;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod transcribe;
pub mod vad;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod wav;

use std::sync::Arc;
use std::thread::JoinHandle;

pub use aac::AacFile;
pub use aac::AacWriter;
pub use aac::Piece;
pub use aac::join;
pub use config::*;
use control::Stop;
use quant_core::DeviceId;
use quant_core::Sink;
use quant_core::StreamId;
pub use rewind::Track;
pub use transcribe::Transcribe;
pub use transcribe::transcribe;

/// The names sources report their status under, in start order.
pub const SOURCES: [&str; 5] = ["apps", "input", "screen", "audio", "rewind"];

/// Start capturing. Each enabled source runs on its own thread until the
/// returned handle is stopped or dropped.
pub fn spawn(config: CaptureConfig, sink: Arc<dyn Sink>, device: DeviceId) -> CaptureHandle {
	let stop = Stop::default();
	#[cfg(target_os = "macos")]
	let threads = macos::spawn(config, sink, device, &stop);
	#[cfg(not(target_os = "macos"))]
	let threads = {
		let _ = (config, device);
		for source in SOURCES {
			sink.status(
				source,
				false,
				Some("capture is only available on macOS".to_owned()),
			);
		}
		drop(sink);
		Vec::new()
	};
	CaptureHandle { stop, threads }
}

/// Running capture threads. Dropping the handle stops them too.
pub struct CaptureHandle {
	stop: Stop,
	threads: Vec<JoinHandle<()>>,
}

impl CaptureHandle {
	/// Stop every source and wait for its thread to finish. Sources flush
	/// what they hold (the open focus span, a partial input interval, speech
	/// in progress) before exiting.
	pub fn stop(mut self) {
		self.shutdown();
	}

	/// How many source threads are still running.
	pub fn running(&self) -> usize {
		self.threads
			.iter()
			.filter(|thread| !thread.is_finished())
			.count()
	}

	fn shutdown(&mut self) {
		self.stop.trigger();
		for thread in self.threads.drain(..) {
			if thread.join().is_err() {
				tracing::error!("a capture thread panicked while stopping");
			}
		}
	}
}

impl Drop for CaptureHandle {
	fn drop(&mut self) {
		self.shutdown();
	}
}

impl std::fmt::Debug for CaptureHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("CaptureHandle")
			.field("running", &self.running())
			.finish()
	}
}

/// Run the calling thread at utility quality of service, as the capture
/// sources do, so background work never competes with the user's.
#[cfg(target_os = "macos")]
pub fn lower_priority() {
	macos::system::lower_priority();
}

/// Run the calling thread at utility quality of service. No-op where the
/// platform has no such knob.
#[cfg(not(target_os = "macos"))]
pub fn lower_priority() {}

/// A built-in stream id.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn stream(name: &str) -> StreamId {
	StreamId::new(name).expect("built-in stream ids are valid")
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use quant_core::MediaRef;
	use quant_core::Record;
	use quant_core::SeriesBatch;
	use quant_core::SinkError;
	use quant_core::builtin_streams;

	use super::*;

	#[derive(Default)]
	struct Statuses(Mutex<Vec<(String, bool, Option<String>)>>);

	impl Sink for Statuses {
		fn record(&self, _: Record) -> Result<(), SinkError> {
			Ok(())
		}

		fn series(&self, _: SeriesBatch) -> Result<(), SinkError> {
			Ok(())
		}

		fn media(&self, _: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError> {
			Ok(MediaRef::of(bytes, mime))
		}

		fn status(&self, source: &str, running: bool, detail: Option<String>) {
			self.0
				.lock()
				.unwrap()
				.push((source.to_owned(), running, detail));
		}
	}

	#[test]
	fn streams_used_are_builtin() {
		let builtin: Vec<_> = builtin_streams().into_iter().map(|s| s.id).collect();
		for name in [
			"screen.frame",
			"apps.focus",
			"input.activity",
			"audio.transcript",
			"audio.buffer",
			"audio.clip",
			"device.state",
		] {
			assert!(
				builtin.contains(&stream(name)),
				"{name} is not a built-in stream"
			);
		}
	}

	#[test]
	fn disabled_sources_report_and_stop_promptly() {
		let sink = Arc::new(Statuses::default());
		let config = CaptureConfig {
			screen: ScreenConfig {
				enabled: false,
				..ScreenConfig::default()
			},
			apps: AppsConfig {
				enabled: false,
				..AppsConfig::default()
			},
			input: InputConfig {
				enabled: false,
				..InputConfig::default()
			},
			audio: AudioConfig::default(),
			rewind: RewindConfig {
				enabled: false,
				..RewindConfig::default()
			},
		};
		let handle = spawn(config, sink.clone(), DeviceId::new());
		let began = std::time::Instant::now();
		handle.stop();
		assert!(began.elapsed() < std::time::Duration::from_secs(2));
		let statuses = sink.0.lock().unwrap();
		for source in SOURCES {
			assert!(
				statuses
					.iter()
					.any(|(name, running, _)| name == source && !running),
				"{source} did not report"
			);
		}
	}
}
