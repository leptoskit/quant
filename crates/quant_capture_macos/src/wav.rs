//! Minimal WAV encoding for speech clips.

/// Encode mono 16-bit PCM samples at `sample_rate` as a WAV file.
pub(crate) fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
	const CHANNELS: u16 = 1;
	const BITS: u16 = 16;
	let block_align = CHANNELS * BITS / 8;
	let data_len = u32::try_from(samples.len() * usize::from(block_align)).unwrap_or(u32::MAX);
	let mut wav = Vec::with_capacity(44 + samples.len() * 2);
	wav.extend_from_slice(b"RIFF");
	wav.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
	wav.extend_from_slice(b"WAVE");
	wav.extend_from_slice(b"fmt ");
	wav.extend_from_slice(&16u32.to_le_bytes());
	wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
	wav.extend_from_slice(&CHANNELS.to_le_bytes());
	wav.extend_from_slice(&sample_rate.to_le_bytes());
	wav.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
	wav.extend_from_slice(&block_align.to_le_bytes());
	wav.extend_from_slice(&BITS.to_le_bytes());
	wav.extend_from_slice(b"data");
	wav.extend_from_slice(&data_len.to_le_bytes());
	for sample in samples {
		wav.extend_from_slice(&sample.to_le_bytes());
	}
	wav
}

/// Duration of `samples` mono samples at `sample_rate`, in milliseconds.
pub(crate) fn duration_ms(samples: usize, sample_rate: u32) -> u64 {
	samples as u64 * 1000 / u64::from(sample_rate.max(1))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn u32_at(bytes: &[u8], at: usize) -> u32 {
		u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
	}

	fn u16_at(bytes: &[u8], at: usize) -> u16 {
		u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
	}

	#[test]
	fn header_describes_16k_mono_pcm() {
		let wav = encode_wav(&[0, 1, -1, i16::MAX, i16::MIN], 16_000);
		assert_eq!(wav.len(), 44 + 10);
		assert_eq!(&wav[0..4], b"RIFF");
		assert_eq!(u32_at(&wav, 4), 36 + 10);
		assert_eq!(&wav[8..16], b"WAVEfmt ");
		assert_eq!(u32_at(&wav, 16), 16);
		assert_eq!(u16_at(&wav, 20), 1, "PCM");
		assert_eq!(u16_at(&wav, 22), 1, "mono");
		assert_eq!(u32_at(&wav, 24), 16_000, "sample rate");
		assert_eq!(u32_at(&wav, 28), 32_000, "byte rate");
		assert_eq!(u16_at(&wav, 32), 2, "block align");
		assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
		assert_eq!(&wav[36..40], b"data");
		assert_eq!(u32_at(&wav, 40), 10);
		assert_eq!(&wav[44..48], &[0, 0, 1, 0]);
		assert_eq!(&wav[50..54], &[0xff, 0x7f, 0x00, 0x80]);
	}

	#[test]
	fn durations() {
		assert_eq!(duration_ms(16_000, 16_000), 1000);
		assert_eq!(duration_ms(8, 16_000), 0);
		assert_eq!(duration_ms(24_000, 16_000), 1500);
	}
}
