//! Energy-based voice activity detection over 16 kHz mono audio.
//!
//! Each 30 ms frame's energy is compared with a slowly tracked noise floor.
//! Speech must persist for an onset period before a segment opens, a short
//! pre-roll keeps the first syllable, and a hangover lets speakers pause
//! mid-sentence without splitting the segment.

use std::collections::VecDeque;
use std::mem;

use crate::config::AudioConfig;
use crate::resample::TARGET_RATE;

/// Samples per analysis frame (30 ms at 16 kHz).
pub(crate) const FRAME: usize = (TARGET_RATE as usize) * 30 / 1000;

/// Frame-count thresholds derived from an [`AudioConfig`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VadSettings {
	/// dB above the noise floor that counts as speech.
	pub(crate) margin_db: f32,
	/// Absolute dBFS below which nothing is speech.
	pub(crate) min_level_dbfs: f32,
	/// Consecutive loud frames needed to open a segment.
	pub(crate) onset_frames: usize,
	/// Quiet frames tolerated inside a segment before it closes.
	pub(crate) hangover_frames: usize,
	/// Frames kept from before the onset.
	pub(crate) preroll_frames: usize,
	/// Longest segment, in frames.
	pub(crate) max_frames: usize,
	/// Loud frames a segment needs to be kept.
	pub(crate) min_speech_frames: usize,
}

impl VadSettings {
	/// Settings from the user's millisecond-based config.
	pub(crate) fn from_config(config: &AudioConfig) -> Self {
		let frames = |ms: u32| (ms as usize).div_ceil(30);
		Self {
			margin_db: config.margin_db,
			min_level_dbfs: config.min_level_dbfs,
			onset_frames: frames(config.onset_ms).max(1),
			hangover_frames: frames(config.hangover_ms),
			preroll_frames: frames(config.preroll_ms),
			max_frames: frames(config.max_segment_secs.saturating_mul(1000)).max(1),
			min_speech_frames: frames(config.min_speech_ms),
		}
	}
}

#[derive(Debug)]
enum State {
	Quiet {
		onset: Vec<Vec<i16>>,
	},
	Speaking {
		segment: Vec<i16>,
		frames: usize,
		speech_frames: usize,
		silent_run: usize,
	},
}

/// Splits a continuous stream into speech segments.
#[derive(Debug)]
pub(crate) struct Vad {
	settings: VadSettings,
	pending: Vec<i16>,
	floor_db: Option<f32>,
	preroll: VecDeque<Vec<i16>>,
	state: State,
}

impl Vad {
	/// A detector with `settings`.
	pub(crate) fn new(settings: VadSettings) -> Self {
		Self {
			settings,
			pending: Vec::with_capacity(FRAME),
			floor_db: None,
			preroll: VecDeque::new(),
			state: State::Quiet { onset: Vec::new() },
		}
	}

	/// Feed samples; returns every segment that finished.
	pub(crate) fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
		let mut finished = Vec::new();
		let mut rest = samples;
		while !rest.is_empty() {
			let take = (FRAME - self.pending.len()).min(rest.len());
			self.pending.extend_from_slice(&rest[..take]);
			rest = &rest[take..];
			if self.pending.len() == FRAME {
				let frame = mem::replace(&mut self.pending, Vec::with_capacity(FRAME));
				if let Some(segment) = self.frame(frame) {
					finished.push(segment);
				}
			}
		}
		finished
	}

	/// End the stream, returning the open segment if it holds enough speech.
	pub(crate) fn flush(&mut self) -> Option<Vec<i16>> {
		let state = mem::replace(&mut self.state, State::Quiet { onset: Vec::new() });
		self.preroll.clear();
		match state {
			State::Speaking {
				mut segment,
				speech_frames,
				..
			} => {
				segment.append(&mut self.pending);
				self.keep(segment, speech_frames)
			}
			State::Quiet { .. } => None,
		}
	}

	fn frame(&mut self, frame: Vec<i16>) -> Option<Vec<i16>> {
		let energy = energy_dbfs(&frame);
		let floor = *self.floor_db.get_or_insert(energy);
		let loud =
			energy >= self.settings.min_level_dbfs && energy > floor + self.settings.margin_db;
		match &mut self.state {
			State::Quiet { onset } => {
				if loud {
					onset.push(frame);
					if onset.len() >= self.settings.onset_frames {
						let mut segment = Vec::new();
						let frames = self.preroll.len() + onset.len();
						let speech_frames = onset.len();
						for old in self.preroll.drain(..) {
							segment.extend_from_slice(&old);
						}
						for loud in onset.drain(..) {
							segment.extend_from_slice(&loud);
						}
						self.state = State::Speaking {
							segment,
							frames,
							speech_frames,
							silent_run: 0,
						};
					}
				} else {
					let onset = mem::take(onset);
					self.adapt_floor(energy, 0.05);
					for old in onset.into_iter().chain([frame]) {
						self.remember(old);
					}
				}
				None
			}
			State::Speaking {
				segment,
				frames,
				speech_frames,
				silent_run,
			} => {
				segment.extend_from_slice(&frame);
				*frames += 1;
				if loud {
					*speech_frames += 1;
					*silent_run = 0;
				} else {
					*silent_run += 1;
				}
				let ended = *silent_run > self.settings.hangover_frames;
				let full = *frames >= self.settings.max_frames;
				let speech = *speech_frames;
				let finished = if ended || full {
					let segment = mem::take(segment);
					if ended {
						self.state = State::Quiet { onset: Vec::new() };
					} else {
						self.state = State::Speaking {
							segment: Vec::new(),
							frames: 0,
							speech_frames: 0,
							silent_run: 0,
						};
					}
					self.keep(segment, speech)
				} else {
					None
				};
				// Track the floor slowly even in speech, so a fan switched on
				// mid-sentence does not count as speech forever.
				self.adapt_floor(energy, 0.002);
				finished
			}
		}
	}

	fn adapt_floor(&mut self, energy: f32, rate: f32) {
		let floor = self.floor_db.get_or_insert(energy);
		if energy < *floor {
			*floor = energy;
		} else {
			*floor += (energy - *floor) * rate;
		}
	}

	fn remember(&mut self, frame: Vec<i16>) {
		if self.settings.preroll_frames == 0 {
			return;
		}
		if self.preroll.len() == self.settings.preroll_frames {
			self.preroll.pop_front();
		}
		self.preroll.push_back(frame);
	}

	fn keep(&self, segment: Vec<i16>, speech_frames: usize) -> Option<Vec<i16>> {
		(speech_frames >= self.settings.min_speech_frames.max(1) && !segment.is_empty())
			.then_some(segment)
	}
}

/// Mean energy of a frame in dB relative to full scale.
pub(crate) fn energy_dbfs(frame: &[i16]) -> f32 {
	if frame.is_empty() {
		return -120.0;
	}
	let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
	#[allow(clippy::cast_precision_loss)]
	let mean = sum / frame.len() as f64 / (32768.0 * 32768.0);
	#[allow(clippy::cast_possible_truncation)]
	let db = (10.0 * (mean + 1e-12).log10()) as f32;
	db
}

#[cfg(test)]
mod tests {
	use super::*;

	fn settings() -> VadSettings {
		VadSettings::from_config(&AudioConfig::default())
	}

	/// Deterministic quiet noise around amplitude 30 (about -60 dBFS).
	fn noise(len: usize, seed: &mut u32) -> Vec<i16> {
		(0..len)
			.map(|_| {
				*seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
				i16::try_from((*seed >> 16) % 61).unwrap() - 30
			})
			.collect()
	}

	fn tone(len: usize) -> Vec<i16> {
		(0..len)
			.map(|i| {
				#[allow(clippy::cast_precision_loss)]
				let t = i as f32 / 16_000.0;
				#[allow(clippy::cast_possible_truncation)]
				let s = ((t * 300.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
				s
			})
			.collect()
	}

	fn ms(ms: usize) -> usize {
		ms * 16
	}

	fn run(vad: &mut Vad, audio: &[i16]) -> Vec<Vec<i16>> {
		// Uneven chunks, as audio callbacks deliver them.
		let mut out = Vec::new();
		for chunk in audio.chunks(1234) {
			out.extend(vad.push(chunk));
		}
		out.extend(vad.flush());
		out
	}

	#[test]
	fn settings_convert_milliseconds_to_frames() {
		let s = settings();
		assert_eq!(FRAME, 480);
		assert_eq!(s.onset_frames, 3);
		assert_eq!(s.hangover_frames, 24);
		assert_eq!(s.preroll_frames, 10);
		assert_eq!(s.max_frames, 1000);
	}

	#[test]
	fn silence_yields_nothing() {
		let mut seed = 1;
		let mut vad = Vad::new(settings());
		assert_eq!(run(&mut vad, &noise(ms(5000), &mut seed)).len(), 0);
	}

	#[test]
	fn one_utterance_is_one_segment_with_preroll_and_hangover() {
		let mut seed = 7;
		let mut audio = noise(ms(1000), &mut seed);
		audio.extend(tone(ms(2000)));
		audio.extend(noise(ms(1500), &mut seed));
		let mut vad = Vad::new(settings());
		let segments = run(&mut vad, &audio);
		assert_eq!(segments.len(), 1);
		let len_ms = segments[0].len() / 16;
		// 300 ms pre-roll + 2 s of speech + ~750 ms hangover, give or take a
		// frame.
		assert!((2950..=3150).contains(&len_ms), "segment was {len_ms} ms");
	}

	#[test]
	fn short_pauses_do_not_split_and_long_ones_do() {
		let mut seed = 3;
		let mut audio = noise(ms(500), &mut seed);
		audio.extend(tone(ms(800)));
		audio.extend(noise(ms(300), &mut seed));
		audio.extend(tone(ms(800)));
		audio.extend(noise(ms(2000), &mut seed));
		audio.extend(tone(ms(800)));
		audio.extend(noise(ms(2000), &mut seed));
		let mut vad = Vad::new(settings());
		assert_eq!(run(&mut vad, &audio).len(), 2);
	}

	#[test]
	fn clicks_shorter_than_the_onset_are_ignored() {
		let mut seed = 5;
		let mut audio = noise(ms(500), &mut seed);
		audio.extend(tone(ms(60)));
		audio.extend(noise(ms(1500), &mut seed));
		let mut vad = Vad::new(settings());
		assert_eq!(run(&mut vad, &audio).len(), 0);
	}

	#[test]
	fn long_speech_is_cut_at_the_maximum() {
		let mut seed = 9;
		let config = AudioConfig {
			max_segment_secs: 2,
			..AudioConfig::default()
		};
		let mut audio = noise(ms(500), &mut seed);
		audio.extend(tone(ms(5000)));
		audio.extend(noise(ms(2000), &mut seed));
		let mut vad = Vad::new(VadSettings::from_config(&config));
		let segments = run(&mut vad, &audio);
		assert_eq!(
			segments.len(),
			3,
			"{:?}",
			segments.iter().map(Vec::len).collect::<Vec<_>>()
		);
		let max = VadSettings::from_config(&config).max_frames * FRAME;
		for segment in &segments[..2] {
			assert_eq!(segment.len(), max);
		}
	}

	#[test]
	fn flush_emits_speech_cut_off_by_stopping() {
		let mut seed = 11;
		let mut audio = noise(ms(500), &mut seed);
		audio.extend(tone(ms(1000)));
		let mut vad = Vad::new(settings());
		let mut segments = Vec::new();
		for chunk in audio.chunks(1000) {
			segments.extend(vad.push(chunk));
		}
		assert_eq!(segments.len(), 0);
		assert!(vad.flush().is_some());
		assert!(vad.flush().is_none());
	}

	#[test]
	fn energy_of_silence_and_full_scale() {
		assert!(energy_dbfs(&[0; 480]) < -100.0);
		assert!(energy_dbfs(&[i16::MAX; 480]).abs() < 0.01);
	}
}
