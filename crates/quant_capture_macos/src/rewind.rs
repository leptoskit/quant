//! The bookkeeping behind the rolling buffer: when to cut a chunk, what to
//! say about the audio inside it, and how the daemon asks the source to
//! close the chunk it is still filling.
//!
//! Everything here is plain arithmetic over timestamps and samples so it can
//! be tested with a fake clock; the macOS side in `macos::rewind` feeds it
//! real audio.

use std::sync::OnceLock;
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;

use crate::wav::duration_ms;

/// Which input a chunk came from. Each becomes the `source` attribute of an
/// `audio.buffer` record.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Track {
	/// The default microphone.
	Mic,
	/// What the machine is playing, from `SCStream`.
	System,
}

impl Track {
	/// The `source` attribute value.
	pub fn as_str(self) -> &'static str {
		match self {
			Track::Mic => "mic",
			Track::System => "system",
		}
	}
}

/// The loudest a run of 16-bit samples gets, in dB relative to full scale.
/// Silence reads as the floor rather than minus infinity.
pub fn peak_dbfs(samples: &[i16]) -> f32 {
	let peak = samples
		.iter()
		.map(|&s| f32::from(s.unsigned_abs()))
		.fold(0.0_f32, f32::max);
	if peak <= 0.0 {
		return -120.0;
	}
	20.0 * (peak / f32::from(i16::MAX)).log10()
}

/// One track's open chunk: when it started and what the audio in it was like.
///
/// The chunk's end is derived from the sample count, not the wall clock, so
/// the record always matches the audio actually encoded.
#[derive(Debug)]
pub struct Chunking {
	/// Seconds of audio per chunk.
	chunk_secs: u64,
	/// When the current chunk opened.
	started: Timestamp,
	/// Samples written to the current chunk, at the track's sample rate.
	samples: u64,
	/// Sample rate the track encodes at.
	sample_rate: u32,
	/// Milliseconds of speech the detector found in this chunk.
	speech_ms: u64,
	/// The loudest the chunk got, in dBFS.
	peak_dbfs: f32,
}

/// What the source needs to know about a chunk it just closed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finished {
	/// When the chunk began.
	pub started: Timestamp,
	/// When it ended: exactly `started` plus the encoded audio.
	pub ended: Timestamp,
	/// Milliseconds of speech in the chunk.
	pub speech_ms: u64,
	/// The loudest the chunk got, in dBFS.
	pub peak_dbfs: f32,
}

impl Chunking {
	/// A chunker that cuts every `chunk_secs` of audio, opening its first
	/// chunk at `started`.
	pub fn new(started: Timestamp, chunk_secs: u64, sample_rate: u32) -> Self {
		Self {
			chunk_secs: chunk_secs.max(1),
			started,
			samples: 0,
			sample_rate: sample_rate.max(1),
			speech_ms: 0,
			peak_dbfs: -120.0,
		}
	}

	/// Note a run of `samples` samples of the given loudness. `speech_ms` is
	/// how much of it the voice detector called speech.
	pub fn note(&mut self, samples: usize, speech_ms: u64, peak_dbfs: f32) {
		self.samples += samples as u64;
		self.speech_ms += speech_ms;
		self.peak_dbfs = self.peak_dbfs.max(peak_dbfs);
	}

	/// Whether the current chunk holds anything.
	pub fn has_audio(&self) -> bool {
		self.samples > 0
	}

	/// Whether the current chunk is full at `now`. A chunk closes on the
	/// clock, not on the sample count, so gaps while nothing arrived do not
	/// stretch a chunk open indefinitely.
	pub fn due(&self, now: Timestamp) -> bool {
		now.duration_since(self.started)
			>= SignedDuration::from_secs(i64::try_from(self.chunk_secs).unwrap_or(i64::MAX))
	}

	/// How much audio the chunk holds, in milliseconds.
	pub fn buffered_ms(&self) -> u64 {
		duration_ms(
			usize::try_from(self.samples).unwrap_or(usize::MAX),
			self.sample_rate,
		)
	}

	/// Close the current chunk, opening the next at `now`, and describe what
	/// closed.
	pub fn rotate(&mut self, now: Timestamp) -> Finished {
		let ended = self
			.started
			.checked_add(SignedDuration::from_millis(
				i64::try_from(self.buffered_ms()).unwrap_or(i64::MAX),
			))
			.unwrap_or(now);
		let finished = Finished {
			started: self.started,
			ended,
			speech_ms: self.speech_ms,
			peak_dbfs: self.peak_dbfs,
		};
		self.started = now;
		self.samples = 0;
		self.speech_ms = 0;
		self.peak_dbfs = -120.0;
		finished
	}
}

/// What the daemon can ask a running `rewind` source to do.
#[derive(Debug)]
pub enum Command {
	/// Close the chunk each track is still filling, so a saved range can
	/// include "just now", then report back on `ack`.
	CloseAll {
		/// Signalled once the closed chunks are stored.
		ack: mpsc::SyncSender<()>,
	},
}

static CONTROL: OnceLock<std::sync::Mutex<Option<Sender<Command>>>> = OnceLock::new();

fn control() -> &'static std::sync::Mutex<Option<Sender<Command>>> {
	CONTROL.get_or_init(|| std::sync::Mutex::new(None))
}

/// Claim the control slot while the source runs. Only one source runs at a
/// time; a second registration is refused so the slot never points at a
/// stopped thread.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn take_control(sender: Sender<Command>) -> bool {
	let mut slot = control()
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner);
	if slot.is_some() {
		return false;
	}
	*slot = Some(sender);
	true
}

/// Release the control slot.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn release_control() {
	let mut slot = control()
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner);
	*slot = None;
}
/// Ask the running `rewind` source to close the chunks it is still filling,
/// waiting up to `timeout` for them to be stored. Returns whether a source
/// was running and answered in time. With no source running (paused, or
/// `--no-capture`), the answer is `false` and the caller makes do with the
/// chunks already stored.
pub fn close_current_chunks(timeout: Duration) -> bool {
	let request = {
		let slot = control()
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		match &*slot {
			Some(sender) => sender.clone(),
			None => return false,
		}
	};
	let (ack, done) = mpsc::sync_channel(1);
	if request.send(Command::CloseAll { ack }).is_err() {
		return false;
	}
	done.recv_timeout(timeout).is_ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(seconds: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_secs(seconds)
	}

	#[test]
	fn silence_pegs_the_floor_and_full_scale_is_zero_db() {
		assert!(peak_dbfs(&[0; 480]) < -100.0);
		assert!(peak_dbfs(&[i16::MAX; 480]).abs() < 0.01);
		// -6 dBFS is half of full scale.
		assert!((peak_dbfs(&[i16::MAX / 2; 480]) + 6.02).abs() < 0.01);
	}

	#[test]
	fn chunks_rotate_on_the_clock() {
		let chunking = Chunking::new(at(0), 60, 16_000);
		assert!(!chunking.due(at(59)));
		assert!(chunking.due(at(60)));
	}

	#[test]
	fn a_chunk_ends_at_its_encoded_audio_not_the_wall_clock() {
		let mut chunking = Chunking::new(at(0), 60, 16_000);
		// 45 s of audio, then the clock runs on and the chunk closes late.
		chunking.note(16_000 * 45, 5_000, -20.0);
		assert!(chunking.due(at(61)));
		let finished = chunking.rotate(at(61));
		assert_eq!(finished.started, at(0));
		assert_eq!(finished.ended, at(45));
		assert_eq!(finished.speech_ms, 5_000);
		assert!((finished.peak_dbfs + 20.0).abs() < 1e-6);
		// The next chunk starts now, not at the crooked end.
		assert!(!chunking.due(at(61)));
		assert!(chunking.due(at(121)));
	}

	#[test]
	fn an_empty_chunk_closes_immediately_at_its_start() {
		let mut chunking = Chunking::new(at(0), 60, 16_000);
		let finished = chunking.rotate(at(60));
		assert_eq!(finished.ended, at(0));
		assert_eq!(finished.speech_ms, 0);
	}

	#[test]
	fn stats_accumulate_until_the_chunk_closes() {
		let mut chunking = Chunking::new(at(0), 60, 16_000);
		chunking.note(16_000, 1_000, -30.0);
		chunking.note(16_000, 500, -12.0);
		let finished = chunking.rotate(at(60));
		assert_eq!(finished.speech_ms, 1_500);
		assert!((finished.peak_dbfs + 12.0).abs() < 1e-6);
	}
}
