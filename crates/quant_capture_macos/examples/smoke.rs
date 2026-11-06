//! Run capture for a while and print a line for everything it produces.
//!
//! ```sh
//! cargo run -p quant_capture_macos --example smoke            # 20 seconds
//! QUANT_SMOKE_SECS=60 QUANT_SMOKE_AUDIO=1 cargo run -p quant_capture_macos --example smoke
//! ```
//!
//! `QUANT_SMOKE_IDLE_SECS` raises the idle threshold, for machines nobody is
//! touching (capture pauses while the user is idle).
//!
//! Screen frames are taken every 5 seconds here (10 by default) so a short
//! run shows a few. The example never shows the Screen Recording prompt; it
//! reports the missing permission instead.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use quant_capture_macos::AppsConfig;
use quant_capture_macos::AudioConfig;
use quant_capture_macos::CaptureConfig;
use quant_capture_macos::ScreenConfig;
use quant_core::DeviceId;
use quant_core::MediaRef;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::SinkError;
use quant_core::StreamId;

/// Prints summaries instead of storing anything.
#[derive(Default)]
struct PrintSink {
	records: AtomicUsize,
	media_bytes: AtomicUsize,
}

impl Sink for PrintSink {
	fn record(&self, record: Record) -> Result<(), SinkError> {
		self.records.fetch_add(1, Ordering::Relaxed);
		let span = match record.end {
			Some(end) => format!("{} → {end}", record.start),
			None => record.start.to_string(),
		};
		let text = record.text.as_deref().map(|text| {
			let flat = text.replace('\n', " ⏎ ");
			let short: String = flat.chars().take(120).collect();
			format!(" text({} chars)={short:?}", text.chars().count())
		});
		let media = record
			.media
			.as_ref()
			.map(|m| format!(" media={} {} bytes", m.mime, m.size));
		println!(
			"[record] {} {span} {}{}{}",
			record.stream,
			serde_json::Value::Object(record.data),
			text.unwrap_or_default(),
			media.unwrap_or_default()
		);
		Ok(())
	}

	fn series(&self, batch: SeriesBatch) -> Result<(), SinkError> {
		batch.validate().map_err(SinkError::from_display)?;
		for (at, row) in batch.rows() {
			println!(
				"[series] {} {at} {:?} = {row:?}",
				batch.stream, batch.channels
			);
		}
		Ok(())
	}

	fn media(&self, stream: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError> {
		self.media_bytes.fetch_add(bytes.len(), Ordering::Relaxed);
		let media = MediaRef::of(bytes, mime);
		println!(
			"[media]  {stream} {mime} {} bytes {}",
			bytes.len(),
			&media.hash[..12]
		);
		Ok(media)
	}

	fn status(&self, source: &str, running: bool, detail: Option<String>) {
		let state = if running { "running" } else { "stopped" };
		println!(
			"[status] {source}: {state}{}",
			detail.map(|d| format!(" ({d})")).unwrap_or_default()
		);
	}
}

fn main() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| "quant_capture_macos=warn".into()),
		)
		.with_writer(std::io::stderr)
		.init();
	let env = |name: &str| {
		std::env::var(name)
			.ok()
			.and_then(|value| value.parse().ok())
	};
	let seconds = env("QUANT_SMOKE_SECS").unwrap_or(20);
	let idle_secs = env("QUANT_SMOKE_IDLE_SECS").unwrap_or(120);
	let config = CaptureConfig {
		screen: ScreenConfig {
			interval_secs: 5,
			idle_pause_secs: idle_secs,
			request_permission: false,
			..ScreenConfig::default()
		},
		apps: AppsConfig {
			idle_secs,
			..AppsConfig::default()
		},
		audio: AudioConfig {
			enabled: std::env::var_os("QUANT_SMOKE_AUDIO").is_some(),
			..AudioConfig::default()
		},
		..CaptureConfig::default()
	};
	let sink = Arc::new(PrintSink::default());
	println!("capturing for {seconds}s…");
	let handle = quant_capture_macos::spawn(config, sink.clone(), DeviceId::new());
	std::thread::sleep(Duration::from_secs(seconds));
	let stopping = std::time::Instant::now();
	handle.stop();
	println!(
		"stopped in {:?}: {} records, {} media bytes",
		stopping.elapsed(),
		sink.records.load(Ordering::Relaxed),
		sink.media_bytes.load(Ordering::Relaxed)
	);
}
