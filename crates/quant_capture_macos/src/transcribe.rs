//! On-demand transcription of saved clips.
//!
//! The rolling buffer never transcribes as it records — that is the whole
//! point of Rewind. When a moment is saved, the daemon calls [`transcribe`]
//! once, on a low-priority thread: with whisper.cpp when Quant was built
//! with the `whisper` feature and a model is configured, otherwise with
//! Apple's on-device speech recognizer. When neither can run, the caller
//! says so on the clip instead of leaving silence unexplained.

use std::path::PathBuf;

/// Which audio a transcription is for, and what to transcribe it with.
#[derive(Clone, Debug, Default)]
pub struct Transcribe {
	/// Path to a whisper.cpp `ggml` model; used when Quant was built with
	/// the `whisper` feature.
	pub whisper_model: Option<PathBuf>,
	/// Spoken language hint, such as `en`; `None` auto-detects.
	pub language: Option<String>,
}

/// Transcribe one AAC `.m4a` clip. The result is trimmed text; the error is
/// a sentence fit for the clip's `transcript_status`.
pub fn transcribe(m4a: &[u8], settings: &Transcribe) -> Result<String, String> {
	#[cfg(all(target_os = "macos", feature = "whisper"))]
	if settings.whisper_model.is_some() {
		return whisper(m4a, settings);
	}
	#[cfg(not(all(target_os = "macos", feature = "whisper")))]
	let _ = settings;
	apple(m4a)
}

/// Decode a clip to the 16 kHz mono 16-bit samples recognisers want.
#[cfg(all(target_os = "macos", feature = "whisper"))]
fn pcm_16k(m4a: &[u8]) -> Result<Vec<i16>, String> {
	let (samples, rate) = crate::aac::decode(m4a, 0, u64::MAX)?;
	let mut resampler = crate::resample::Resampler::with_rates(rate, crate::resample::TARGET_RATE);
	let mut pcm = Vec::with_capacity(samples.len());
	resampler.process(&samples, &mut pcm);
	Ok(pcm)
}

/// whisper.cpp, the same path the `audio` source uses.
#[cfg(all(target_os = "macos", feature = "whisper"))]
fn whisper(m4a: &[u8], settings: &Transcribe) -> Result<String, String> {
	let config = crate::config::AudioConfig {
		whisper_model: settings.whisper_model.clone(),
		language: settings.language.clone(),
		..crate::config::AudioConfig::default()
	};
	let (Some(mut transcriber), _) = crate::macos::whisper::Transcriber::load(&config) else {
		return Err("the configured whisper model could not be loaded".to_owned());
	};
	transcriber.transcribe(&pcm_16k(m4a)?, &config)
}

/// whisper.cpp was compiled out.
#[cfg(not(all(target_os = "macos", feature = "whisper")))]
#[allow(dead_code)]
fn whisper(m4a: &[u8], settings: &Transcribe) -> Result<String, String> {
	let _ = (m4a, settings);
	Err("built without the `whisper` feature".to_owned())
}

/// Apple's on-device speech recognition, through the Speech framework.
#[cfg(target_os = "macos")]
fn apple(m4a: &[u8]) -> Result<String, String> {
	use std::time::Duration;

	use objc2::AnyThread;
	use objc2_foundation::NSString;
	use objc2_foundation::NSURL;
	use objc2_speech::SFSpeechRecognizer;
	use objc2_speech::SFSpeechRecognizerAuthorizationStatus;
	use objc2_speech::SFSpeechURLRecognitionRequest;

	const STARTUP: Duration = Duration::from_secs(20);
	/// On-device recognition runs faster than real time, but leave room for
	/// a long clip on a busy machine.
	#[allow(clippy::cast_sign_loss)]
	const PER_SECOND: Duration = Duration::from_millis(1500);

	// SAFETY: plain class methods with no arguments.
	let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
	if status != SFSpeechRecognizerAuthorizationStatus::Authorized {
		// A daemon cannot show the system prompt itself; the owner enables
		// Speech Recognition for it in System Settings.
		return Err("speech recognition is not allowed for this program".to_owned());
	}
	// SAFETY: a fresh allocation; the default locale's recognizer, which is
	// `nil` only when the language is unsupported.
	let recognizer = unsafe { SFSpeechRecognizer::init(SFSpeechRecognizer::alloc()) }
		.ok_or("no speech recognizer for this language")?;
	// SAFETY: plain property reads on a valid recognizer.
	if !unsafe { recognizer.isAvailable() } {
		return Err("the speech recognizer is not available right now".to_owned());
	}
	if !unsafe { recognizer.supportsOnDeviceRecognition() } {
		return Err("this Mac cannot recognize speech on device".to_owned());
	}

	let temp = tempfile::Builder::new()
		.prefix("quant-clip-")
		.suffix(".m4a")
		.tempfile()
		.map_err(|error| format!("could not stage the clip: {error}"))?;
	std::fs::write(temp.path(), m4a)
		.map_err(|error| format!("could not stage the clip: {error}"))?;
	let url = NSURL::fileURLWithPath(&NSString::from_str(&temp.path().display().to_string()));

	// SAFETY: a fresh allocation with a valid file URL.
	let request = unsafe {
		SFSpeechURLRecognitionRequest::initWithURL(SFSpeechURLRecognitionRequest::alloc(), &url)
	};
	unsafe {
		// SAFETY: plain property writes on a fresh request.
		request.setRequiresOnDeviceRecognition(true);
		request.setShouldReportPartialResults(false);
	}

	let (sender, receiver) = std::sync::mpsc::channel::<(String, bool, Option<String>)>();
	let handler = block2::RcBlock::new(
		move |result: *mut objc2_speech::SFSpeechRecognitionResult,
		      error: *mut objc2_foundation::NSError| {
			// SAFETY: the pointers are valid or null for the block's
			// duration.
			let (text, final_, error) = unsafe {
				match result.as_ref() {
					Some(result) => {
						(
							result.bestTranscription().formattedString().to_string(),
							result.isFinal(),
							None,
						)
					}
					None => {
						(
							String::new(),
							true,
							error
								.as_ref()
								.map(|error| error.localizedDescription().to_string()),
						)
					}
				}
			};
			let _ = sender.send((text, final_, error));
		},
	);
	// SAFETY: the request is valid and the handler is retained until the
	// task finishes with it.
	let _task = unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };

	// How long the clip is, and so how long recognition may take.
	let duration = crate::aac::duration_ms(m4a).unwrap_or(0);
	let budget =
		STARTUP + PER_SECOND.saturating_mul(u32::try_from(duration).unwrap_or(u32::MAX) / 1000);
	let deadline = std::time::Instant::now() + budget;
	loop {
		let left = deadline.saturating_duration_since(std::time::Instant::now());
		if left.is_zero() {
			return Err("speech recognition timed out".to_owned());
		}
		match receiver.recv_timeout(left) {
			// A partial result can contain only the opening words. Wait for
			// the final result so a five-minute segment is searchable in full.
			Ok((text, true, None)) => {
				return Ok(text.trim().to_owned());
			}
			Ok((_, _, Some(error))) => return Err(error),
			Ok((_, false, None)) => {}
			Err(_) => return Err("speech recognition timed out".to_owned()),
		}
	}
}

/// Apple's recognizer is only available on macOS.
#[cfg(not(target_os = "macos"))]
fn apple(m4a: &[u8]) -> Result<String, String> {
	let _ = m4a;
	Err("on-device speech recognition needs macOS".to_owned())
}
