//! Converting device audio to 16 kHz mono 16-bit, the format speech
//! recognisers expect.

/// The rate every speech segment is stored and transcribed at.
pub(crate) const TARGET_RATE: u32 = 16_000;

/// Average interleaved frames of `channels` samples down to mono.
pub(crate) fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
	if channels <= 1 {
		return interleaved.to_vec();
	}
	#[allow(clippy::cast_precision_loss)]
	let scale = 1.0 / channels as f32;
	interleaved
		.chunks_exact(channels)
		.map(|frame| frame.iter().sum::<f32>() * scale)
		.collect()
}

/// Streaming sample-rate converter to [`TARGET_RATE`].
///
/// Downsampling averages every input sample that falls inside an output
/// sample's period, which doubles as a crude anti-aliasing filter and is
/// plenty for speech. Upsampling interpolates linearly.
#[derive(Debug)]
pub(crate) struct Resampler {
	/// Input samples per output sample.
	step: f64,
	/// Downsampling: how far into the current output period we are.
	phase: f64,
	sum: f64,
	count: u32,
	/// Upsampling: the previous input sample and the fractional read position.
	previous: Option<f32>,
	position: f64,
}

impl Resampler {
	/// A converter from `input_rate` Hz.
	pub(crate) fn new(input_rate: u32) -> Self {
		Self {
			step: f64::from(input_rate.max(1)) / f64::from(TARGET_RATE),
			phase: 0.0,
			sum: 0.0,
			count: 0,
			previous: None,
			position: 0.0,
		}
	}

	/// Convert the next run of mono samples, appending 16-bit output.
	pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
		if (self.step - 1.0).abs() < f64::EPSILON {
			out.extend(input.iter().map(|&s| to_i16(s)));
		} else if self.step > 1.0 {
			for &sample in input {
				self.sum += f64::from(sample);
				self.count += 1;
				self.phase += 1.0;
				if self.phase >= self.step {
					self.phase -= self.step;
					#[allow(clippy::cast_possible_truncation)]
					out.push(to_i16((self.sum / f64::from(self.count)) as f32));
					self.sum = 0.0;
					self.count = 0;
				}
			}
		} else {
			for &sample in input {
				let previous = self.previous.unwrap_or(sample);
				// Emit every output position that falls between the previous
				// input sample (at 0) and this one (at 1).
				while self.position < 1.0 {
					#[allow(clippy::cast_possible_truncation)]
					let t = self.position as f32;
					out.push(to_i16(previous + (sample - previous) * t));
					self.position += self.step;
				}
				self.position -= 1.0;
				self.previous = Some(sample);
			}
		}
	}
}

/// Clamp a float sample to 16 bits.
pub(crate) fn to_i16(sample: f32) -> i16 {
	#[allow(clippy::cast_possible_truncation)]
	let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
	value
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn downmix_averages_channels() {
		assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
		assert_eq!(downmix(&[0.25, -0.25], 1), vec![0.25, -0.25]);
	}

	#[test]
	fn converts_to_clamped_16_bit() {
		assert_eq!(to_i16(0.0), 0);
		assert_eq!(to_i16(1.0), i16::MAX);
		assert_eq!(to_i16(2.0), i16::MAX);
		assert_eq!(to_i16(-1.0), -i16::MAX);
	}

	fn convert(rate: u32, seconds: usize) -> Vec<i16> {
		let mut resampler = Resampler::new(rate);
		let mut out = Vec::new();
		let input = vec![0.5f32; rate as usize * seconds];
		// Feed in uneven chunks, as a device would.
		for chunk in input.chunks(437) {
			resampler.process(chunk, &mut out);
		}
		out
	}

	#[test]
	fn common_rates_land_on_16k() {
		for rate in [8_000, 16_000, 44_100, 48_000, 96_000] {
			let out = convert(rate, 2);
			let expected = 2 * TARGET_RATE as usize;
			assert!(
				out.len().abs_diff(expected) <= 2,
				"{rate} Hz gave {} samples",
				out.len()
			);
			assert!(
				out.iter().all(|&s| s == to_i16(0.5)),
				"{rate} Hz changed the level"
			);
		}
	}

	#[test]
	fn downsampling_averages_away_alternating_noise() {
		let mut resampler = Resampler::new(48_000);
		let input: Vec<f32> = (0..4800)
			.map(|i| if i % 2 == 0 { 0.9 } else { -0.9 })
			.collect();
		let mut out = Vec::new();
		resampler.process(&input, &mut out);
		let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap();
		assert!(peak < 12_000, "aliasing peak {peak}");
	}
}
