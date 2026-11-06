//! The macOS sources.

mod apps;
mod audio;
mod image;
mod input;
mod screen;
mod system;
#[cfg(feature = "whisper")]
mod whisper;

use std::panic;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;

use quant_core::DeviceId;
use quant_core::Record;
use quant_core::Sink;

use crate::config::CaptureConfig;
use crate::control::Status;
use crate::control::Stop;
use crate::privacy::Exclusions;

/// What every source thread needs.
#[derive(Clone)]
pub(crate) struct Context {
	pub(crate) sink: Arc<dyn Sink>,
	pub(crate) device: DeviceId,
	pub(crate) stop: Stop,
}

impl Context {
	/// Hand a record to the sink, logging (not failing) if it refuses.
	pub(crate) fn emit(&self, record: Record) {
		let stream = record.stream.clone();
		if let Err(error) = self.sink.record(record) {
			tracing::warn!(%stream, %error, "sink refused a record");
		}
	}

	pub(crate) fn status(&self, name: &'static str) -> Status {
		Status::new(Arc::clone(&self.sink), name)
	}
}

/// Start a thread for each enabled source.
pub(crate) fn spawn(
	config: CaptureConfig,
	sink: Arc<dyn Sink>,
	device: DeviceId,
	stop: &Stop,
) -> Vec<JoinHandle<()>> {
	let context = Context {
		sink,
		device,
		stop: stop.clone(),
	};
	let exclusions = Exclusions::new(
		&config.screen.excluded_bundle_ids,
		&config.screen.excluded_title_patterns,
	);
	let mut threads = Vec::new();

	let mut start = |name: &'static str, enabled: bool, run: Box<dyn FnOnce(&Context) + Send>| {
		if !enabled {
			context
				.sink
				.status(name, false, Some("disabled".to_owned()));
			return;
		}
		let thread_context = context.clone();
		let spawned = thread::Builder::new()
			.name(format!("quant-{name}"))
			.spawn(move || {
				system::lower_priority();
				let context = thread_context;
				let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(&context)));
				if let Err(panic) = outcome {
					let message = panic
						.downcast_ref::<&str>()
						.map(ToString::to_string)
						.or_else(|| panic.downcast_ref::<String>().cloned())
						.unwrap_or_else(|| "unknown panic".to_owned());
					tracing::error!(source = name, %message, "capture source crashed");
					context
						.sink
						.status(name, false, Some(format!("crashed: {message}")));
				}
			});
		match spawned {
			Ok(handle) => threads.push(handle),
			Err(error) => {
				context.sink.status(
					name,
					false,
					Some(format!("could not start thread: {error}")),
				);
			}
		}
	};

	let CaptureConfig {
		screen,
		apps,
		input,
		audio,
	} = config;
	let apps_exclusions = exclusions.clone();
	start(
		"apps",
		apps.enabled,
		Box::new(move |cx| apps::run(cx, &apps, &apps_exclusions)),
	);
	start(
		"input",
		input.enabled,
		Box::new(move |cx| input::run(cx, &input)),
	);
	start(
		"screen",
		screen.enabled,
		Box::new(move |cx| screen::run(cx, &screen, &exclusions)),
	);
	start(
		"audio",
		audio.enabled,
		Box::new(move |cx| audio::run(cx, &audio)),
	);
	threads
}
