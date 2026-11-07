//! The `audio` source: speech segments from the default microphone.
//!
//! The device itself is shared through [`super::mic`], so this source and
//! the rolling buffer never open the microphone twice. This thread downmixes,
//! resamples to 16 kHz, runs voice activity detection, and turns each
//! finished segment into a WAV clip and an `audio.transcript` record,
//! transcribed when built with `whisper`.

use std::time::Duration;

use jiff::SignedDuration;
use jiff::Timestamp;
use objc2_av_foundation::AVAuthorizationStatus;
use objc2_av_foundation::AVCaptureDevice;
use objc2_av_foundation::AVMediaTypeAudio;
use quant_core::Record;

use super::Context;
use super::mic::MicHub;
use super::mic::MicMessage;
use crate::config::AudioConfig;
use crate::resample::Resampler;
use crate::resample::TARGET_RATE;
use crate::resample::downmix;
use crate::stream;
use crate::vad::Vad;
use crate::vad::VadSettings;
use crate::wav::duration_ms;
use crate::wav::encode_wav;

/// How often the thread checks for a stop while no audio arrives.
const POLL: Duration = Duration::from_millis(200);

/// Listen until stopped.
pub(crate) fn run(cx: &Context, config: &AudioConfig) {
	let mut status = cx.status("audio");
	if let Err(reason) = listen(cx, config, &mut status) {
		tracing::warn!(%reason, "microphone capture unavailable");
		status.stopped(&reason);
		// Stay parked so the reported reason is the last word until stopped.
		cx.stop.wait(Duration::MAX);
		return;
	}
	status.stopped("stopped");
}

/// Whether macOS says the Microphone permission was refused outright.
pub(crate) fn microphone_denied() -> bool {
	matches!(
		microphone_permission(),
		Some(AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted)
	)
}

fn listen(
	cx: &Context,
	config: &AudioConfig,
	status: &mut crate::control::Status,
) -> Result<(), String> {
	if microphone_denied() {
		return Err("microphone permission not granted".to_owned());
	}
	let (mut transcriber, note) = Transcriber::load(config);

	let mic = MicHub::shared().listen()?;
	let (channels, rate) = (mic.info.channels, mic.info.rate);
	status.set(true, note.as_deref());

	let mut resampler = Resampler::new(rate);
	let mut vad = Vad::new(VadSettings::from_config(config));
	let mut converted = Vec::new();
	let mut last_heard = Timestamp::now();
	loop {
		match mic.recv(POLL) {
			Some(MicMessage::Samples { at, interleaved }) => {
				converted.clear();
				resampler.process(&downmix(&interleaved, channels), &mut converted);
				for segment in vad.push(&converted) {
					emit(cx, config, transcriber.as_mut(), &segment, at);
				}
				last_heard = at;
			}
			Some(MicMessage::Error(error)) => {
				tracing::warn!(%error, "microphone stream error");
				status.set(true, Some(&format!("microphone error: {error}")));
			}
			None => {}
		}
		if cx.stop.is_set() {
			break;
		}
	}
	drop(mic);
	if let Some(segment) = vad.flush() {
		emit(cx, config, transcriber.as_mut(), &segment, last_heard);
	}
	Ok(())
}

/// The Microphone privacy permission, if `AVFoundation` can say.
fn microphone_permission() -> Option<AVAuthorizationStatus> {
	// SAFETY: reading an AVFoundation constant.
	let audio = unsafe { AVMediaTypeAudio }?;
	// SAFETY: a class method taking a valid media type.
	Some(unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) })
}

/// Store one speech segment that ended at about `ended`.
fn emit(
	cx: &Context,
	config: &AudioConfig,
	speech_to_text: Option<&mut Transcriber>,
	samples: &[i16],
	ended: Timestamp,
) {
	let millis = duration_ms(samples.len(), TARGET_RATE);
	let started = ended
		.checked_sub(SignedDuration::from_millis(
			i64::try_from(millis).unwrap_or(i64::MAX),
		))
		.unwrap_or(ended);
	let transcript = speech_to_text.map(|model| model.transcribe(samples, config));
	let transcribed = matches!(transcript, Some(Ok(_)));
	let mut record = Record::new(stream("audio.transcript"), cx.device)
		.starting(started)
		.ending(ended)
		.with("source", "microphone")
		.with("duration_ms", millis)
		.with("transcribed", transcribed);
	match transcript {
		Some(Ok(text)) if !text.trim().is_empty() => record = record.with_text(text.trim()),
		Some(Err(error)) => tracing::warn!(%error, "transcription failed"),
		_ => {}
	}
	if config.store_audio {
		let wav = encode_wav(samples, TARGET_RATE);
		match cx
			.sink
			.media(&stream("audio.transcript"), &wav, "audio/wav")
		{
			Ok(media) => record = record.with_media(media),
			Err(error) => tracing::warn!(%error, "sink refused a speech clip"),
		}
	}
	cx.emit(record);
}

#[cfg(feature = "whisper")]
use super::whisper::Transcriber;

/// Stand-in when built without the `whisper` feature: never loads.
#[cfg(not(feature = "whisper"))]
pub(crate) enum Transcriber {}

#[cfg(not(feature = "whisper"))]
impl Transcriber {
	/// Always `None`, with a note saying why.
	pub(crate) fn load(_: &AudioConfig) -> (Option<Self>, Option<String>) {
		(
			None,
			Some("transcription unavailable: built without the `whisper` feature".to_owned()),
		)
	}

	/// Unreachable: no value of this type exists.
	pub(crate) fn transcribe(&mut self, _: &[i16], _: &AudioConfig) -> Result<String, String> {
		match *self {}
	}
}
