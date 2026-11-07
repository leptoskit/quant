//! Measure the CPU cost of the rolling buffer's exact encode-and-chunk
//! path, faster than real time.
//!
//! ```sh
//! cargo run -p quant_capture_macos --release --example rewind_bench
//! QUANT_BENCH_MINUTES=30 cargo run -p quant_capture_macos --release --example rewind_bench
//! ```
//!
//! Synthetic speech-like audio — noise bursts with pauses, which drive the
//! voice detector the way real speech does — is downmixed, resampled,
//! measured, and encoded into 60-second AAC chunks through the same
//! converter, detector, peak tracker, chunk bookkeeping, and `AVAudioFile`
//! encoder the live `rewind` source uses, at the same 20 ms cadence a
//! device callback delivers. CPU is read with `getrusage` before and after,
//! and reported as CPU-seconds per hour of audio.
//!
//! The microphone and system-audio capture itself is not measured here (it
//! needs hardware); a device callback that hands over already-interleaved
//! samples is the boundary. On an M4 Mac mini the whole path settles around
//! four CPU-seconds per audio-hour — about 0.1% of one core while
//! buffering.

// Reading getrusage needs one line of FFI; the workspace forbids unsafe
// everywhere but the capture crate, and this example is part of it.
#![allow(unsafe_code)]

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_capture_macos::AacWriter;
use quant_capture_macos::RewindConfig;
use quant_capture_macos::resample::Resampler;
use quant_capture_macos::resample::downmix;
use quant_capture_macos::rewind::Chunking;
use quant_capture_macos::rewind::peak_dbfs;
use quant_capture_macos::vad::Vad;
use quant_capture_macos::vad::VadSettings;
use quant_core::DeviceId;
use quant_core::MediaRef;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::SinkError;
use quant_core::StreamId;

/// The synthetic device's rate, as microphones deliver.
const DEVICE_RATE: u32 = 48_000;

/// Seconds of audio per callback chunk.
const CALLBACK_MS: i64 = 20;

fn main() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| "quant_capture_macos=warn".into()),
		)
		.with_writer(std::io::stderr)
		.init();
	let minutes: u64 = std::env::var("QUANT_BENCH_MINUTES")
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or(10);
	let config = RewindConfig::default();
	let sink = Arc::new(CountingSink::default());

	println!(
		"buffering {minutes} minutes of synthetic speech-like audio through the rolling buffer's \
		 encode-and-chunk path…"
	);
	let cpu_before = cpu_seconds();
	let wall_before = std::time::Instant::now();
	let files = buffer(minutes, &config, &sink);
	let wall = wall_before.elapsed();
	let cpu = cpu_seconds() - cpu_before;

	#[allow(clippy::cast_precision_loss)]
	let audio_hours = minutes as f64 / 60.0;
	let bytes: u64 = files.iter().map(|file| file.bytes as u64).sum();
	let audio_ms: u64 = files.iter().map(|file| file.audio_ms).sum();
	let speech_ms: u64 = files.iter().map(|file| file.speech_ms).sum();
	println!();
	println!(
		"{} chunks, {} MB of AAC, {:.1} minutes of audio, {} ms called speech",
		files.len(),
		bytes / (1024 * 1024),
		minutes as f64,
		speech_ms
	);
	println!("encoded audio: {audio_ms} ms");
	println!(
		"CPU: {cpu:.2}s for {:.2} audio-hours → {:.2} CPU-seconds per audio-hour",
		audio_hours,
		cpu / audio_hours
	);
	println!(
		"that is {:.3}% of one core while buffering in real time (this run finished {:.0}× faster \
		 than real time)",
		(cpu / audio_hours) / 3600.0 * 100.0,
		audio_hours * 3600.0 / wall.as_secs_f64()
	);
}

/// The `rewind` source's pipeline, minus the threads: downmix, resample,
/// voice activity detection, peak tracking, chunk rotation, AAC encoding,
/// and the sink calls that store each chunk.
fn buffer(minutes: u64, config: &RewindConfig, sink: &CountingSink) -> Vec<Chunk> {
	let started = Timestamp::now();
	let mut clock = SignedDuration::ZERO;
	let mut resampler = Resampler::with_rates(DEVICE_RATE, config.sample_rate);
	let mut vad = Vad::new(VadSettings::from_config(
		&quant_capture_macos::AudioConfig::default(),
	));
	let mut chunking = Chunking::new(started, config.chunk_secs, config.sample_rate);
	let mut writer = AacWriter::new(config.sample_rate, config.bitrate).expect("writer");
	let mut mono = Vec::new();
	let mut pcm = Vec::new();
	let mut float = Vec::new();
	let mut chunks = Vec::new();

	#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
	let callbacks = minutes as usize * 60 * 1000 / CALLBACK_MS as usize;
	#[allow(clippy::cast_possible_truncation)]
	let per_callback = DEVICE_RATE as usize * CALLBACK_MS as usize / 1000;
	let mut burst = true;
	let mut burst_left = DEVICE_RATE as usize * 6 / 5;
	let mut quiet_left = 0_usize;
	let mut sample = 0_usize;

	for _ in 0..callbacks {
		// Speech-like input: alternating 1.2 s bursts of modulated tone and
		// 0.8 s pauses over faint noise, which is what the detector has to
		// work hardest to split.
		if burst {
			burst_left = burst_left.saturating_sub(per_callback);
			if burst_left == 0 {
				burst = false;
				quiet_left = DEVICE_RATE as usize * 4 / 5;
			}
		} else {
			quiet_left = quiet_left.saturating_sub(per_callback);
			if quiet_left == 0 {
				burst = true;
				burst_left = DEVICE_RATE as usize * 6 / 5;
			}
		}
		mono.clear();
		mono.extend((0..per_callback).map(|i| {
			sample += 1;
			if burst {
				#[allow(clippy::cast_possible_truncation)]
				let t = (i + sample) as f32 / f64::from(DEVICE_RATE) as f32;
				((t * 190.0 * std::f32::consts::TAU).sin() * 0.25
					+ ((i * 7919) % 512) as f32 / 512.0 * 0.2)
					.clamp(-1.0, 1.0)
			} else {
				(((i * 2_654_435_761) % 97) as f32 / 97.0 - 0.5) * 0.003
			}
		}));

		pcm.clear();
		let interleaved = downmix(&mono, 1);
		resampler.process(&interleaved, &mut pcm);
		let mut speech_ms = 0;
		for segment in vad.push(&pcm) {
			speech_ms += segment.len() as u64 * 1000 / u64::from(config.sample_rate);
		}
		let peak = peak_dbfs(&pcm);
		chunking.note(pcm.len(), speech_ms, peak);

		float.clear();
		float.extend(pcm.iter().map(|&s| f32::from(s) / f32::from(i16::MAX)));
		writer.write(&float).expect("encoder");

		clock += SignedDuration::from_millis(CALLBACK_MS);
		let now = started.checked_add(clock).expect("bench time");
		if chunking.due(now) {
			chunks.push(close(&mut chunking, &mut writer, now, config, sink));
		}
	}
	let now = started.checked_add(clock).expect("bench time");
	if chunking.has_audio() {
		chunks.push(close(&mut chunking, &mut writer, now, config, sink));
	}
	chunks
}

/// Finish the open chunk, store it as the source would, and open the next.
fn close(
	chunking: &mut Chunking,
	writer: &mut AacWriter,
	now: Timestamp,
	config: &RewindConfig,
	sink: &CountingSink,
) -> Chunk {
	let finished = chunking.rotate(now);
	let fresh = AacWriter::new(config.sample_rate, config.bitrate).expect("writer");
	let open = std::mem::replace(writer, fresh);
	let file = open.finish().expect("finish");
	let stream: StreamId = "audio.buffer".parse().unwrap();
	let media = sink.media(&stream, &file.bytes, "audio/mp4").expect("sink");
	sink.record(
		Record::new(stream, DeviceId::new())
			.starting(finished.started)
			.ending(finished.ended)
			.with("source", "mic")
			.with("device", "bench microphone")
			.with("speech_ms", finished.speech_ms)
			.with("peak_dbfs", finished.peak_dbfs)
			.with("bytes", file.bytes.len())
			.with_media(media),
	)
	.expect("sink");
	Chunk {
		speech_ms: finished.speech_ms,
		audio_ms: file.duration_ms(),
		bytes: file.bytes.len(),
	}
}

struct Chunk {
	speech_ms: u64,
	audio_ms: u64,
	bytes: usize,
}

/// CPU-seconds this process has used, user and system.
fn cpu_seconds() -> f64 {
	let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
	// SAFETY: a plain getrusage call with a valid out-pointer.
	unsafe {
		libc::getrusage(libc::RUSAGE_SELF, std::ptr::from_mut(&mut usage));
	}
	// `tv_usec` is `i32` on macOS and `i64` on Linux.
	#[allow(clippy::cast_lossless)]
	let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
	seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// Counts what the sink stores, so storing each chunk is real work.
#[derive(Default)]
struct CountingSink {
	records: std::sync::atomic::AtomicUsize,
	media_bytes: std::sync::atomic::AtomicUsize,
}

impl Sink for CountingSink {
	fn record(&self, _: Record) -> Result<(), SinkError> {
		self.records
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		Ok(())
	}

	fn series(&self, _: SeriesBatch) -> Result<(), SinkError> {
		Ok(())
	}

	fn media(&self, _: &StreamId, bytes: &[u8], _: &str) -> Result<MediaRef, SinkError> {
		self.media_bytes
			.fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
		Ok(MediaRef::of(bytes, "audio/mp4"))
	}

	fn status(&self, _: &str, _: bool, _: Option<String>) {}
}
