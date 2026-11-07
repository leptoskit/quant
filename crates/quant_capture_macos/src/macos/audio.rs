//! The `audio` source: speech segments from the default microphone.
//!
//! The device callback only converts samples to `f32` and hands them over;
//! this thread downmixes, resamples to 16 kHz, runs voice activity detection,
//! and turns each finished segment into a WAV clip and an
//! `audio.transcript` record, transcribed when built with `whisper`.

use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use cpal::FromSample;
use cpal::Sample;
use cpal::SampleFormat;
use cpal::SizedSample;
use cpal::Stream;
use cpal::StreamConfig;
use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use cpal::traits::StreamTrait;
use jiff::SignedDuration;
use jiff::Timestamp;
use objc2_av_foundation::AVAuthorizationStatus;
use objc2_av_foundation::AVCaptureDevice;
use objc2_av_foundation::AVMediaTypeAudio;
use quant_core::Record;

use super::Context;
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

enum Message {
	/// Interleaved samples and when the callback received them.
	Samples(Timestamp, Vec<f32>),
	/// The stream reported an error.
	Error(String),
}

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

fn listen(
	cx: &Context,
	config: &AudioConfig,
	status: &mut crate::control::Status,
) -> Result<(), String> {
	if let Some(AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted) =
		microphone_permission()
	{
		return Err("microphone permission not granted".to_owned());
	}
	let (mut transcriber, note) = Transcriber::load(config);

	let host = cpal::default_host();
	let device = host.default_input_device().ok_or("no microphone found")?;
	let supported = device
		.default_input_config()
		.map_err(|error| format!("microphone unavailable: {error}"))?;
	let channels = usize::from(supported.channels()).max(1);
	let rate = supported.sample_rate();
	let (sender, receiver) = mpsc::channel();
	let stream = open(
		&device,
		supported.sample_format(),
		supported.config(),
		sender,
	)
	.map_err(|error| format!("could not open microphone: {error}"))?;
	stream
		.play()
		.map_err(|error| format!("could not start microphone: {error}"))?;
	status.set(true, note.as_deref());

	let mut resampler = Resampler::new(rate);
	let mut vad = Vad::new(VadSettings::from_config(config));
	let mut converted = Vec::new();
	let mut last_heard = Timestamp::now();
	loop {
		match receiver.recv_timeout(POLL) {
			Ok(Message::Samples(at, interleaved)) => {
				converted.clear();
				resampler.process(&downmix(&interleaved, channels), &mut converted);
				for segment in vad.push(&converted) {
					emit(cx, config, transcriber.as_mut(), &segment, at);
				}
				last_heard = at;
			}
			Ok(Message::Error(error)) => {
				tracing::warn!(%error, "microphone stream error");
				status.set(true, Some(&format!("microphone error: {error}")));
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => break,
		}
		if cx.stop.is_set() {
			break;
		}
	}
	drop(stream);
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

/// Build an input stream that forwards `f32` samples, whatever the device's
/// native format.
fn open(
	device: &cpal::Device,
	format: SampleFormat,
	config: StreamConfig,
	sender: mpsc::Sender<Message>,
) -> Result<Stream, String> {
	match format {
		SampleFormat::F32 => typed::<f32>(device, config, sender),
		SampleFormat::F64 => typed::<f64>(device, config, sender),
		SampleFormat::I16 => typed::<i16>(device, config, sender),
		SampleFormat::I32 => typed::<i32>(device, config, sender),
		SampleFormat::I8 => typed::<i8>(device, config, sender),
		SampleFormat::U8 => typed::<u8>(device, config, sender),
		SampleFormat::U16 => typed::<u16>(device, config, sender),
		SampleFormat::U32 => typed::<u32>(device, config, sender),
		other => Err(format!("unsupported sample format {other}")),
	}
}

fn typed<T>(
	device: &cpal::Device,
	config: StreamConfig,
	sender: mpsc::Sender<Message>,
) -> Result<Stream, String>
where
	T: SizedSample + Send + 'static,
	f32: FromSample<T>,
{
	let errors = sender.clone();
	device
		.build_input_stream::<T, _, _>(
			config,
			move |data: &[T], _| {
				let samples = data.iter().map(|&s| f32::from_sample(s)).collect();
				let _ = sender.send(Message::Samples(Timestamp::now(), samples));
			},
			move |error| {
				let _ = errors.send(Message::Error(error.to_string()));
			},
			None,
		)
		.map_err(|error| error.to_string())
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
