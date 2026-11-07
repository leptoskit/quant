//! AAC chunks through Apple's encoder: the writer the rolling buffer fills,
//! the reader that measures and joins them when a moment is saved.
//!
//! Everything here is software encoding and decoding — no audio hardware —
//! so it works on any Mac and is what the benchmark exercises.

/// An encoded chunk and what it holds.
#[derive(Clone, Debug)]
pub struct AacFile {
	/// The `.m4a` bytes.
	pub bytes: Vec<u8>,
	/// Frames of audio the encoder was given.
	pub frames: u64,
	/// The file's sample rate.
	pub sample_rate: u32,
}

impl AacFile {
	/// The chunk's duration in milliseconds.
	pub fn duration_ms(&self) -> u64 {
		self.frames * 1000 / u64::from(self.sample_rate.max(1))
	}
}

/// One chunk to join into a saved clip, and how much of it to use.
#[derive(Clone, Debug)]
pub struct Piece {
	/// The chunk's `.m4a` bytes.
	pub bytes: Vec<u8>,
	/// Frames to skip at the start, trimming to the saved range.
	pub skip_frames: u64,
	/// Frames to take after the skip; `u64::MAX` takes the rest.
	pub take_frames: u64,
}

#[cfg(target_os = "macos")]
mod apple {
	use std::slice;

	use objc2::AnyThread;
	use objc2::rc::Retained;
	use objc2::runtime::AnyObject;
	use objc2_avf_audio::AVAudioCommonFormat;
	use objc2_avf_audio::AVAudioFile;
	use objc2_avf_audio::AVAudioFormat;
	use objc2_avf_audio::AVAudioPCMBuffer;
	use objc2_avf_audio::AVEncoderBitRateKey;
	use objc2_avf_audio::AVFormatIDKey;
	use objc2_avf_audio::AVNumberOfChannelsKey;
	use objc2_avf_audio::AVSampleRateKey;
	use objc2_core_audio_types::kAudioFormatMPEG4AAC;
	use objc2_foundation::NSDictionary;
	use objc2_foundation::NSError;
	use objc2_foundation::NSNumber;
	use objc2_foundation::NSString;
	use objc2_foundation::NSURL;

	/// Frames per write handed to `AVAudioFile`.
	pub(super) const BLOCK: u32 = 8192;

	pub(super) fn settings(
		sample_rate: u32,
		bitrate: u32,
	) -> Retained<NSDictionary<NSString, AnyObject>> {
		// SAFETY: reading AVFoundation constants linked into the process.
		let keys = unsafe {
			[
				AVFormatIDKey.expect("AVFormatIDKey"),
				AVSampleRateKey.expect("AVSampleRateKey"),
				AVNumberOfChannelsKey.expect("AVNumberOfChannelsKey"),
				AVEncoderBitRateKey.expect("AVEncoderBitRateKey"),
			]
		};
		let values: [Retained<AnyObject>; 4] = [
			NSNumber::new_u32(kAudioFormatMPEG4AAC).into(),
			NSNumber::new_f64(f64::from(sample_rate)).into(),
			NSNumber::new_u32(1).into(),
			NSNumber::new_u64(u64::from(bitrate)).into(),
		];
		let values: Vec<&AnyObject> = values.iter().map(|value| &**value).collect();
		NSDictionary::from_slices(&keys, &values)
	}

	pub(super) fn buffer(format: &AVAudioFormat) -> Option<Retained<AVAudioPCMBuffer>> {
		// SAFETY: a fresh allocation with a valid PCM format and a small,
		// non-zero capacity.
		unsafe {
			AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
				AVAudioPCMBuffer::alloc(),
				format,
				BLOCK,
			)
		}
	}

	pub(super) fn describe(error: &Retained<NSError>) -> String {
		// SAFETY: plain property read.
		error.localizedDescription().to_string()
	}

	/// The mono float channel of a PCM buffer, as a mutable slice of its
	/// capacity. Valid up to the frame length set on the buffer.
	///
	/// `AVAudioPCMBuffer` hands out a mutable channel pointer even through a
	/// shared reference; each buffer is only ever touched by the one thread
	/// that owns it, so the aliasing the type system cannot see does not
	/// happen in practice.
	#[allow(clippy::mut_from_ref)]
	pub(super) fn channel(buffer: &AVAudioPCMBuffer) -> &mut [f32] {
		// SAFETY: `floatChannelData` points at one pointer per channel; the
		// buffer is mono float32, and the returned slice covers the capacity
		// the buffer was allocated with.
		let channels = unsafe { *buffer.floatChannelData() };
		// SAFETY: the channel pointer stays valid for the buffer's lifetime
		// and covers `frameCapacity` samples.
		unsafe { slice::from_raw_parts_mut(channels.as_ptr(), buffer.frameCapacity() as usize) }
	}

	pub(super) fn url(path: &std::path::Path) -> Retained<NSURL> {
		NSURL::fileURLWithPath(&NSString::from_str(&path.display().to_string()))
	}

	pub(super) fn open_writer(
		path: &std::path::Path,
		sample_rate: u32,
		bitrate: u32,
	) -> Result<(Retained<AVAudioFile>, Retained<AVAudioPCMBuffer>), String> {
		let settings = settings(sample_rate, bitrate);
		// SAFETY: a fresh allocation; the URL is writable and the settings
		// describe the mono AAC format we want. The client format is
		// deinterleaved float32, matching the buffer we write through.
		let file = unsafe {
			AVAudioFile::initForWriting_settings_commonFormat_interleaved_error(
				AVAudioFile::alloc(),
				&url(path),
				&settings,
				AVAudioCommonFormat::PCMFormatFloat32,
				false,
			)
		}
		.map_err(|error| describe(&error))?;
		// SAFETY: plain property read on an open file.
		let format = unsafe { file.processingFormat() };
		let buffer = buffer(&format).ok_or("could not allocate an encode buffer")?;
		Ok((file, buffer))
	}

	pub(super) fn write(
		file: &AVAudioFile,
		buffer: &AVAudioPCMBuffer,
		mono: &[f32],
	) -> Result<(), String> {
		for block in mono.chunks(BLOCK as usize) {
			let channel = channel(buffer);
			channel[..block.len()].copy_from_slice(block);
			// SAFETY: the frame length never exceeds the capacity, and the
			// buffer's format matches the file's processing format.
			#[allow(clippy::cast_possible_truncation)]
			let frames = block.len() as u32;
			// SAFETY: the frame length never exceeds the capacity, and the
			// buffer's format matches the file's processing format.
			unsafe { buffer.setFrameLength(frames) };
			// SAFETY: the buffer matches the file's processing format.
			unsafe { file.writeFromBuffer_error(buffer) }.map_err(|error| describe(&error))?;
		}
		Ok(())
	}

	pub(super) struct Reader {
		file: Retained<AVAudioFile>,
		buffer: Retained<AVAudioPCMBuffer>,
		/// Frames already read out of the file.
		read: u64,
	}

	pub(super) fn open_reader(path: &std::path::Path) -> Result<Reader, String> {
		// SAFETY: a fresh allocation opening an existing file for reading.
		let file = unsafe { AVAudioFile::initForReading_error(AVAudioFile::alloc(), &url(path)) }
			.map_err(|error| describe(&error))?;
		// SAFETY: plain property reads on an open file.
		let (format, length) = unsafe { (file.processingFormat(), file.length()) };
		let channels = unsafe { format.channelCount() } as usize;
		if channels == 0 {
			return Err("audio file has no channels".to_owned());
		}
		let buffer = buffer(&format).ok_or("could not allocate a decode buffer")?;
		Ok(Reader {
			file,
			buffer,
			read: u64::try_from(length).unwrap_or(u64::MAX),
		})
	}

	impl Reader {
		/// The file's sample rate.
		pub(super) fn sample_rate(&self) -> u32 {
			// SAFETY: plain property read on the processing format.
			#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
			let rate = unsafe { self.file.processingFormat().sampleRate() } as u32;
			rate.max(1)
		}

		/// Frames left to read.
		pub(super) fn remaining(&self) -> u64 {
			self.read
		}

		/// Move the read position forward by `frames`.
		pub(super) fn skip(&mut self, frames: u64) {
			let target = self.position() + i64::try_from(frames).unwrap_or(i64::MAX);
			// SAFETY: plain property write; positions past the end read as
			// empty, which `remaining` then reports.
			unsafe { self.file.setFramePosition(target) };
			self.read = self.read.saturating_sub(frames);
		}

		fn position(&self) -> i64 {
			// SAFETY: plain property read.
			unsafe { self.file.framePosition() }
		}

		/// Read up to `max` frames of mono float audio, downmixing if the
		/// file has more than one channel.
		pub(super) fn read(&mut self, max: u64) -> Result<Vec<f32>, String> {
			let mut out = Vec::new();
			while (out.len() as u64) < max && self.remaining() > 0 {
				#[allow(clippy::cast_possible_truncation)]
				let want = ((max - out.len() as u64) as u32).min(BLOCK);
				// SAFETY: reading sequentially into a buffer whose format
				// matches the file's processing format.
				unsafe {
					self.file
						.readIntoBuffer_frameCount_error(self.buffer.as_ref(), want)
				}
				.map_err(|error| describe(&error))?;
				// SAFETY: plain property read after the read.
				let got = unsafe { self.buffer.frameLength() } as usize;
				if got == 0 {
					break;
				}
				self.read = self.read.saturating_sub(got as u64);
				// SAFETY: plain property read on the processing format.
				let channels = unsafe { self.file.processingFormat().channelCount() } as usize;
				let channel = channel(self.buffer.as_ref());
				if channels <= 1 {
					out.extend_from_slice(&channel[..got]);
				} else {
					let scale = 1.0 / channels as f32;
					for frame in 0..got {
						let sum: f32 = (0..channels).map(|c| channel[c * got + frame]).sum();
						out.push(sum * scale);
					}
				}
			}
			Ok(out)
		}
	}
}

/// An AAC chunk open for writing. Created and finished on the encoder
/// thread; nothing crosses threads but the finished bytes.
#[cfg(target_os = "macos")]
pub struct AacWriter {
	file: objc2::rc::Retained<objc2_avf_audio::AVAudioFile>,
	buffer: objc2::rc::Retained<objc2_avf_audio::AVAudioPCMBuffer>,
	temp: tempfile::NamedTempFile,
	frames: u64,
	sample_rate: u32,
}

#[cfg(target_os = "macos")]
impl AacWriter {
	/// A writer for mono AAC at `sample_rate`, `bitrate` bits per second.
	pub fn new(sample_rate: u32, bitrate: u32) -> Result<Self, String> {
		let temp = tempfile::Builder::new()
			.prefix("quant-rewind-")
			.suffix(".m4a")
			.tempfile()
			.map_err(|error| format!("could not create a chunk file: {error}"))?;
		let (file, buffer) = apple::open_writer(temp.path(), sample_rate, bitrate)?;
		Ok(Self {
			file,
			buffer,
			temp,
			frames: 0,
			sample_rate,
		})
	}

	/// Append mono float samples.
	pub fn write(&mut self, mono: &[f32]) -> Result<(), String> {
		if mono.is_empty() {
			return Ok(());
		}
		apple::write(self.file.as_ref(), self.buffer.as_ref(), mono)?;
		self.frames += mono.len() as u64;
		Ok(())
	}

	/// Close the file and take its bytes.
	pub fn finish(self) -> Result<AacFile, String> {
		// SAFETY: closing an open file flushes its header.
		unsafe { self.file.close() };
		let bytes = std::fs::read(self.temp.path())
			.map_err(|error| format!("could not read the chunk back: {error}"))?;
		Ok(AacFile {
			bytes,
			frames: self.frames,
			sample_rate: self.sample_rate,
		})
	}
}

/// A writer that cannot run on this platform.
#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct AacWriter;

#[cfg(not(target_os = "macos"))]
impl AacWriter {
	/// Always fails: Apple's encoder is only available on macOS.
	pub fn new(_: u32, _: u32) -> Result<Self, String> {
		Err("AAC encoding needs macOS".to_owned())
	}

	/// Always fails.
	pub fn write(&mut self, _: &[f32]) -> Result<(), String> {
		Err("AAC encoding needs macOS".to_owned())
	}

	/// Always fails.
	pub fn finish(self) -> Result<AacFile, String> {
		Err("AAC encoding needs macOS".to_owned())
	}
}

/// Decode a chunk, skipping `skip_frames` and taking at most `take_frames`,
/// as mono float samples at the file's own rate.
pub fn decode(bytes: &[u8], skip_frames: u64, take_frames: u64) -> Result<(Vec<f32>, u32), String> {
	#[cfg(target_os = "macos")]
	{
		let temp = tempfile::Builder::new()
			.prefix("quant-rewind-read-")
			.suffix(".m4a")
			.tempfile()
			.map_err(|error| format!("could not stage the chunk: {error}"))?;
		std::fs::write(temp.path(), bytes)
			.map_err(|error| format!("could not stage the chunk: {error}"))?;
		let mut reader = apple::open_reader(temp.path())?;
		let rate = reader.sample_rate();
		reader.skip(skip_frames);
		let samples = reader.read(take_frames)?;
		Ok((samples, rate))
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = (bytes, skip_frames, take_frames);
		Err("AAC decoding needs macOS".to_owned())
	}
}

/// A chunk's duration in milliseconds, read from the file itself.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn duration_ms(bytes: &[u8]) -> Result<u64, String> {
	let (samples, rate) = decode(bytes, 0, u64::MAX)?;
	Ok(samples.len() as u64 * 1000 / u64::from(rate.max(1)))
}

/// Re-encode chunks into one joined file, trimming each edge as asked.
///
/// Saving a moment is rare, so one decode-and-re-encode pass is fine; the
/// rolling buffer itself is never re-encoded. Chunks are decoded one at a
/// time, so memory stays at about one chunk of PCM.
pub fn join(pieces: &[Piece], sample_rate: u32, bitrate: u32) -> Result<AacFile, String> {
	if pieces.is_empty() {
		return Err("nothing to join".to_owned());
	}
	let mut writer = AacWriter::new(sample_rate, bitrate)?;
	for piece in pieces {
		let (mut samples, rate) = decode(&piece.bytes, piece.skip_frames, piece.take_frames)?;
		if rate != sample_rate && !samples.is_empty() {
			// A chunk encoded before the sample rate changed.
			let mut resampler = crate::resample::Resampler::with_rates(rate, sample_rate);
			let mut converted =
				Vec::with_capacity(samples.len() * sample_rate as usize / rate as usize);
			resampler.process_f32(&samples, &mut converted);
			samples = converted;
		}
		writer.write(&samples)?;
	}
	writer.finish()
}

#[cfg(test)]
mod tests {
	#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
	use super::*;

	/// A second of speech-ish audio: a tone that fades in and out.
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
	fn tone(seconds: f64, rate: u32) -> Vec<f32> {
		let frames = (seconds * f64::from(rate)) as usize;
		(0..frames)
			.map(|i| {
				let t = i as f64 / f64::from(rate);
				((t * 220.0 * std::f64::consts::TAU).sin() * 0.4) as f32
			})
			.collect()
	}

	#[cfg(target_os = "macos")]
	fn chunk(seconds: f64, rate: u32) -> AacFile {
		let mut writer = AacWriter::new(rate, 24_000).unwrap();
		writer.write(&tone(seconds, rate)).unwrap();
		writer.finish().unwrap()
	}

	#[test]
	#[cfg(target_os = "macos")]
	fn a_chunk_round_trips_within_50_ms() {
		let file = chunk(1.0, 16_000);
		assert!(file.bytes.len() > 100, "chunk looks empty");
		let read_back = duration_ms(&file.bytes).unwrap();
		assert!(
			(i64::try_from(read_back).unwrap_or(0) - 1000).abs() <= 50,
			"round trip was {read_back} ms"
		);
	}

	#[test]
	#[cfg(target_os = "macos")]
	fn three_chunks_join_to_three_seconds() {
		let chunks: Vec<AacFile> = (0..3).map(|_| chunk(1.0, 16_000)).collect();
		let pieces: Vec<Piece> = chunks
			.iter()
			.map(|chunk| {
				Piece {
					bytes: chunk.bytes.clone(),
					skip_frames: 0,
					take_frames: u64::MAX,
				}
			})
			.collect();
		let joined = join(&pieces, 16_000, 24_000).unwrap();
		assert_eq!(joined.sample_rate, 16_000);
		let duration = duration_ms(&joined.bytes).unwrap();
		assert!(
			(i64::try_from(duration).unwrap_or(0) - 3000).abs() <= 150,
			"joined clip was {duration} ms"
		);
	}

	#[test]
	#[cfg(target_os = "macos")]
	fn edges_are_trimmed_to_the_requested_frames() {
		let chunk = chunk(2.0, 16_000);
		let joined = join(
			&[Piece {
				bytes: chunk.bytes.clone(),
				skip_frames: 16_000 / 2,
				take_frames: 16_000,
			}],
			16_000,
			24_000,
		)
		.unwrap();
		let duration = duration_ms(&joined.bytes).unwrap();
		assert!(
			(i64::try_from(duration).unwrap_or(0) - 1000).abs() <= 100,
			"trimmed clip was {duration} ms"
		);
	}
}
