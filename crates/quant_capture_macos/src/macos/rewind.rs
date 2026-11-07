//! The `rewind` source: a rolling buffer of the last day of audio.
//!
//! It records the microphone and whatever the machine is playing, encodes
//! each as mono AAC with Apple's encoder on a low-priority thread, and cuts
//! an `audio.buffer` chunk every minute. Nothing is transcribed while it
//! runs; a moment is saved (and only then transcribed) after the fact. It
//! keeps recording while the screen is locked or the user is idle, and stops
//! only when the owner pauses capture.
//!
//! Unlike every other source it shares the microphone through [`super::mic`],
//! so the `audio` source and `rewind` never open the device twice.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use jiff::Timestamp;
use objc2::AnyThread;
use objc2::DefinedClass;
use objc2::define_class;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::rc::autoreleasepool;
use objc2::runtime::NSObject;
use objc2::runtime::ProtocolObject;
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::NSError;
use objc2_foundation::NSObjectProtocol;
use objc2_screen_capture_kit::SCContentFilter;
use objc2_screen_capture_kit::SCStream;
use objc2_screen_capture_kit::SCStreamConfiguration;
use objc2_screen_capture_kit::SCStreamOutput;
use objc2_screen_capture_kit::SCStreamOutputType;
use quant_core::Record;

use super::Context;
use super::mic::MicListener;
use super::mic::MicMessage;
use super::screen::shareable_content;
use super::system;
use crate::aac::AacFile;
use crate::aac::AacWriter;
use crate::config::AudioConfig;
use crate::config::RewindConfig;
use crate::control::Status;
use crate::resample::Resampler;
use crate::resample::downmix;
use crate::rewind::Chunking;
use crate::rewind::Command;
use crate::rewind::Finished;
use crate::rewind::Track;
use crate::rewind::peak_dbfs;
use crate::rewind::release_control;
use crate::rewind::take_control;
use crate::stream;
use crate::vad::Vad;
use crate::vad::VadSettings;
use crate::wav::duration_ms;

/// How long a single wait on any channel lasts before the loop re-checks
/// rotation and stop. The sources themselves block for several of these.
const POLL: Duration = Duration::from_millis(50);

/// How long to give the encoder when a chunk must be closed right now.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// The rate system audio is asked for; `SCStream` resamples to it.
const SYSTEM_RATE: u32 = 48_000;

/// What the machine is playing, as mono float samples.
struct SystemMessage {
	/// Mono samples at the delivered rate.
	samples: Arc<Vec<f32>>,
	/// The delivered sample rate.
	rate: u32,
}

enum ToEncoder {
	Write { track: Track, samples: Vec<f32> },
	Close { track: Track },
	Stop,
}

enum FromEncoder {
	Chunk { track: Track, file: AacFile },
	Empty { track: Track },
	Failed { track: Track, error: String },
	Stopped,
}

/// Run the rolling buffer until stopped.
pub(crate) fn run(cx: &Context, config: &RewindConfig) {
	let mut status = cx.status("rewind");
	if let Err(reason) = listen(cx, config, &mut status) {
		tracing::warn!(%reason, "rolling audio buffer unavailable");
		status.stopped(&reason);
		// Stay parked so the reported reason is the last word until stopped.
		cx.stop.wait(Duration::MAX);
		return;
	}
	status.stopped("stopped");
}

/// The inputs the buffer is recording from.
struct Listening {
	/// The shared microphone, if buffering it.
	mic: Option<MicListener>,
	/// System audio, if buffering it.
	system: Option<SystemAudio>,
	/// The microphone's name, for records.
	mic_name: String,
}

fn listen(cx: &Context, config: &RewindConfig, status: &mut Status) -> Result<(), String> {
	let listening = open_inputs(config, status)?;
	let (to_encoder, inbox) = mpsc::channel::<ToEncoder>();
	let (outbox, from_encoder) = mpsc::channel::<FromEncoder>();
	let encoder = std::thread::Builder::new()
		.name("quant-rewind-encode".to_owned())
		.spawn({
			let rates = (config.sample_rate, config.bitrate);
			move || {
				super::system::lower_priority();
				encoder_loop(rates, &inbox, &outbox);
			}
		})
		.map_err(|error| format!("could not start the encoder thread: {error}"))?;

	let (control_tx, control_rx) = mpsc::channel::<Command>();
	let mut state = State::new(cx, config, to_encoder.clone(), &listening);
	if !take_control(control_tx) {
		// Another source claimed the slot; stop before recording anything.
		let _ = to_encoder.send(ToEncoder::Stop);
		while let Ok(message) = from_encoder.recv() {
			if matches!(message, FromEncoder::Stopped) {
				break;
			}
		}
		let _ = encoder.join();
		return Err("another rolling buffer is already running".to_owned());
	}

	let mut listening = listening;
	while let Turn::Continue = turn(&mut state, &mut listening, &from_encoder, &control_rx) {}

	// Flush what is buffered, then stop the encoder.
	let _ = close_chunks(&mut state, &from_encoder);
	let _ = to_encoder.send(ToEncoder::Stop);
	while let Ok(message) = from_encoder.recv_timeout(CLOSE_WAIT) {
		if matches!(message, FromEncoder::Stopped) {
			break;
		}
		if let FromEncoder::Chunk { track, file } = message {
			state.store(track, &file);
		}
	}
	let _ = encoder.join();
	release_control();
	drop(listening);
	Ok(())
}

/// One pass over the inputs. Anything that arrives is encoded; chunks past
/// their minute are closed; the daemon may ask for an early close.
fn turn(
	state: &mut State<'_>,
	listening: &mut Listening,
	from_encoder: &mpsc::Receiver<FromEncoder>,
	control: &mpsc::Receiver<Command>,
) -> Turn {
	// The microphone, if any.
	match listening.mic.as_ref() {
		Some(mic) => {
			if let Some(message) = mic.recv(POLL) {
				match message {
					MicMessage::Samples { interleaved, .. } => {
						let (channels, rate) = (mic.info.channels, mic.info.rate);
						state.feed(Track::Mic, &interleaved, channels, rate);
					}
					MicMessage::Error(error) => {
						tracing::warn!(%error, "microphone stream error");
					}
				}
			}
		}
		None => std::thread::sleep(POLL),
	}
	// System audio, if any.
	match listening.system.as_ref() {
		Some(system) => {
			if let Ok(message) = system.receiver.recv_timeout(POLL) {
				state.feed(Track::System, &message.samples, 1, message.rate);
			}
		}
		None => std::thread::sleep(POLL),
	}
	// Answers from the encoder thread.
	while let Ok(message) = from_encoder.try_recv() {
		match message {
			FromEncoder::Chunk { track, file } => state.store(track, &file),
			FromEncoder::Empty { track } => {
				state.pending.remove(&track);
			}
			FromEncoder::Failed { track, error } => {
				tracing::warn!(track = track.as_str(), %error, "audio chunk encoding failed");
				state.pending.remove(&track);
			}
			FromEncoder::Stopped => {}
		}
	}
	// The daemon asking for an early close, so a save can reach "just now".
	while let Ok(command) = control.try_recv() {
		match command {
			Command::CloseAll { ack } => {
				close_chunks(state, from_encoder);
				let _ = ack.send(());
			}
		}
	}
	if state.cx.stop.is_set() {
		return Turn::Stopped;
	}
	// Close chunks whose minute is up. Empty ones just roll over.
	let now = Timestamp::now();
	let due: Vec<Track> = state
		.chunking
		.iter()
		.filter(|(_, chunking)| chunking.due(now))
		.map(|(track, _)| *track)
		.collect();
	for track in due {
		state.rotate(track, now);
	}
	Turn::Continue
}

enum Turn {
	Continue,
	Stopped,
}

/// Close every chunk right now, waiting for the encoder to hand the bytes
/// back so the caller can rely on them being stored. Returns whether every
/// chunk came back.
fn close_chunks(state: &mut State<'_>, from_encoder: &mpsc::Receiver<FromEncoder>) -> bool {
	let now = Timestamp::now();
	let tracks: Vec<Track> = state.chunking.keys().copied().collect();
	let mut waiting = Vec::new();
	for track in tracks {
		if state.rotate(track, now) {
			waiting.push(track);
		}
	}
	if waiting.is_empty() {
		return true;
	}
	let deadline = std::time::Instant::now() + CLOSE_WAIT;
	while !waiting.is_empty() {
		let left = deadline.saturating_duration_since(std::time::Instant::now());
		if left.is_zero() {
			break;
		}
		match from_encoder.recv_timeout(left) {
			Ok(FromEncoder::Chunk { track, file }) => {
				state.store(track, &file);
				waiting.retain(|waiting| *waiting != track);
			}
			Ok(FromEncoder::Empty { track } | FromEncoder::Failed { track, .. }) => {
				state.pending.remove(&track);
				waiting.retain(|waiting| *waiting != track);
			}
			Ok(FromEncoder::Stopped) | Err(_) => break,
		}
	}
	waiting.is_empty()
}

/// Everything the main loop tracks.
struct State<'a> {
	cx: &'a Context,
	config: &'a RewindConfig,
	to_encoder: mpsc::Sender<ToEncoder>,
	chunking: HashMap<Track, Chunking>,
	vad: HashMap<Track, Vad>,
	/// Stats of chunks the encoder is still closing.
	pending: HashMap<Track, Finished>,
	/// Device name by track, for records.
	devices: HashMap<Track, String>,
	/// One resampler per track.
	resamplers: HashMap<Track, Resampler>,
	/// Scratch buffers, reused every callback.
	mono: Vec<f32>,
	converted: Vec<i16>,
}

impl State<'_> {
	fn new<'a>(
		cx: &'a Context,
		config: &'a RewindConfig,
		to_encoder: mpsc::Sender<ToEncoder>,
		listening: &Listening,
	) -> State<'a> {
		let now = Timestamp::now();
		let settings = VadSettings::from_config(&AudioConfig::default());
		let mut chunking = HashMap::new();
		let mut vad = HashMap::new();
		let mut devices = HashMap::new();
		if listening.mic.is_some() {
			chunking.insert(
				Track::Mic,
				Chunking::new(now, config.chunk_secs, config.sample_rate),
			);
			vad.insert(Track::Mic, Vad::new(settings.clone()));
			devices.insert(Track::Mic, listening.mic_name.clone());
		}
		if listening.system.is_some() {
			chunking.insert(
				Track::System,
				Chunking::new(now, config.chunk_secs, config.sample_rate),
			);
			vad.insert(Track::System, Vad::new(settings));
			devices.insert(Track::System, "system audio".to_owned());
		}
		State {
			cx,
			config,
			to_encoder,
			chunking,
			vad,
			pending: HashMap::new(),
			devices,
			resamplers: HashMap::new(),
			mono: Vec::new(),
			converted: Vec::new(),
		}
	}

	/// Downmix, resample, measure, and hand the encoder its float copy.
	fn feed(&mut self, track: Track, interleaved: &[f32], channels: usize, rate: u32) {
		self.mono.clear();
		self.mono.extend(downmix(interleaved, channels));
		self.converted.clear();
		let output = self.config.sample_rate;
		let resampler = self
			.resamplers
			.entry(track)
			.or_insert_with(|| Resampler::with_rates(rate, output));
		// Devices can change their rate under us; rebuild rather than
		// resample at the wrong ratio.
		if resampler.input_rate() != rate || resampler.output_rate() != output {
			*resampler = Resampler::with_rates(rate, output);
		}
		resampler.process(&self.mono, &mut self.converted);
		// The detector works on 16-bit samples and yields finished segments;
		// their length is this chunk's speech estimate.
		let mut speech_ms = 0;
		if let Some(vad) = self.vad.get_mut(&track) {
			for segment in vad.push(&self.converted) {
				speech_ms += duration_ms(segment.len(), output);
			}
		}
		let peak = peak_dbfs(&self.converted);
		let samples = self.converted.len();
		if let Some(chunking) = self.chunking.get_mut(&track) {
			chunking.note(samples, speech_ms, peak);
		}
		// The encoder wants floats; this is the only per-sample copy.
		let pcm: Vec<f32> = self
			.converted
			.iter()
			.map(|&sample| f32::from(sample) / f32::from(i16::MAX))
			.collect();
		let _ = self.to_encoder.send(ToEncoder::Write {
			track,
			samples: pcm,
		});
	}

	/// Close `track`'s chunk, opening the next at `now`. Returns whether the
	/// encoder has a chunk to finish.
	fn rotate(&mut self, track: Track, now: Timestamp) -> bool {
		let Some(chunking) = self.chunking.get_mut(&track) else {
			return false;
		};
		let finished = chunking.rotate(now);
		if !chunking_has_audio(&finished) {
			return false;
		}
		self.pending.insert(track, finished);
		let _ = self.to_encoder.send(ToEncoder::Close { track });
		true
	}

	/// Store one finished chunk as an `audio.buffer` record.
	fn store(&mut self, track: Track, file: &AacFile) {
		let Some(finished) = self.pending.remove(&track) else {
			return;
		};
		let device = self
			.devices
			.get(&track)
			.cloned()
			.unwrap_or_else(|| "unknown".to_owned());
		#[allow(clippy::cast_possible_truncation)]
		let peak = (finished.peak_dbfs * 10.0).round() / 10.0;
		let mut record = Record::new(stream("audio.buffer"), self.cx.device)
			.starting(finished.started)
			.ending(finished.ended)
			.with("source", track.as_str())
			.with("device", device)
			.with("speech_ms", finished.speech_ms)
			.with("duration_ms", file.duration_ms())
			.with("bytes", file.bytes.len())
			.with("peak_dbfs", peak);
		match self
			.cx
			.sink
			.media(&stream("audio.buffer"), &file.bytes, "audio/mp4")
		{
			Ok(media) => {
				record = record.with_media(media);
				self.cx.emit(record);
			}
			Err(error) => tracing::warn!(%error, "sink refused an audio chunk"),
		}
	}
}

/// Whether a finished chunk actually held audio. A chunk that saw no samples
/// (a paused input, a quiet minute on a track that is closing anyway) has
/// nothing to encode and nothing to store.
fn chunking_has_audio(finished: &Finished) -> bool {
	finished.ended > finished.started
}

/// The encoder thread: fills chunk files, closing and reopening on command.
/// All of Apple's encoder work happens here, at utility priority.
fn encoder_loop(
	(sample_rate, bitrate): (u32, u32),
	inbox: &mpsc::Receiver<ToEncoder>,
	outbox: &mpsc::Sender<FromEncoder>,
) {
	let mut writers: HashMap<Track, AacWriter> = HashMap::new();
	loop {
		let message = match inbox.recv_timeout(CLOSE_WAIT) {
			Ok(message) => message,
			Err(mpsc::RecvTimeoutError::Timeout) => continue,
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		};
		match message {
			ToEncoder::Write { track, samples } => {
				let writer = match writers.entry(track) {
					std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
					std::collections::hash_map::Entry::Vacant(entry) => {
						match AacWriter::new(sample_rate, bitrate) {
							Ok(writer) => entry.insert(writer),
							Err(error) => {
								let _ = outbox.send(FromEncoder::Failed { track, error });
								continue;
							}
						}
					}
				};
				if let Err(error) = writer.write(&samples) {
					let _ = outbox.send(FromEncoder::Failed { track, error });
					writers.remove(&track);
				}
			}
			ToEncoder::Close { track } => {
				if let Some(writer) = writers.remove(&track) {
					match writer.finish() {
						Ok(file) => {
							let _ = outbox.send(FromEncoder::Chunk { track, file });
						}
						Err(error) => {
							let _ = outbox.send(FromEncoder::Failed { track, error });
						}
					}
				} else {
					let _ = outbox.send(FromEncoder::Empty { track });
				}
			}
			ToEncoder::Stop => {
				for (track, writer) in writers.drain() {
					if let Ok(file) = writer.finish() {
						let _ = outbox.send(FromEncoder::Chunk { track, file });
					}
				}
				let _ = outbox.send(FromEncoder::Stopped);
				break;
			}
		}
	}
}

/// System audio capture through `SCStream`.
struct SystemAudio {
	stream: Retained<SCStream>,
	/// Kept beside the stream; the `SCStream` retains the sink itself once
	/// it is added as an output.
	_sink: Retained<SampleBufferSink>,
	receiver: mpsc::Receiver<SystemMessage>,
}

impl Drop for SystemAudio {
	fn drop(&mut self) {
		let (done, finished) = mpsc::sync_channel(1);
		let handler = block2::RcBlock::new(move |error: *mut NSError| {
			if let Some(error) = unsafe { error.as_ref() } {
				tracing::warn!(%error, "stopping system audio failed");
			}
			let _ = done.send(());
		});
		// SAFETY: the stream is valid and the handler is retained until it
		// runs.
		unsafe {
			self.stream.stopCaptureWithCompletionHandler(Some(&handler));
		}
		let _ = finished.recv_timeout(Duration::from_secs(5));
	}
}

impl SystemAudio {
	/// Capture what the machine is playing, excluding this process's own
	/// audio, through a display-wide capture stream.
	fn open() -> Result<Self, String> {
		let content = shareable_content()?;
		// SAFETY: plain property reads on a valid object.
		let displays = unsafe { content.displays() };
		let display = displays
			.firstObject()
			.ok_or("no display to capture audio from")?;
		let empty = objc2_foundation::NSArray::from_slice(&[]);
		// SAFETY: the display and window list come from the same, current
		// shareable content.
		let filter = unsafe {
			SCContentFilter::initWithDisplay_excludingWindows(
				SCContentFilter::alloc(),
				&display,
				&empty,
			)
		};
		let configuration = unsafe { SCStreamConfiguration::new() };
		// SAFETY: plain property writes on a fresh configuration. The tiny
		// frame size keeps the video side of the stream (unused) cheap.
		unsafe {
			configuration.setWidth(2);
			configuration.setHeight(2);
			configuration.setCapturesAudio(true);
			configuration.setExcludesCurrentProcessAudio(true);
			#[allow(clippy::cast_possible_wrap)]
			configuration.setSampleRate(SYSTEM_RATE as isize);
			configuration.setChannelCount(1);
		}
		let (sender, receiver) = mpsc::channel();
		let sink = SampleBufferSink::new(sender);
		// SAFETY: the filter and configuration are valid; no delegate is
		// needed because failures surface through the completion handler.
		let stream = unsafe {
			SCStream::initWithFilter_configuration_delegate(
				SCStream::alloc(),
				&filter,
				&configuration,
				None,
			)
		};
		let output = ProtocolObject::from_retained(sink.clone());
		// SAFETY: the sink implements SCStreamOutput; passing no queue lets
		// ScreenCaptureKit deliver on one of its own.
		unsafe {
			stream
				.addStreamOutput_type_sampleHandlerQueue_error(
					&output,
					SCStreamOutputType::Audio,
					None,
				)
				.map_err(|error| {
					format!("could not listen to system audio: {}", describe(&error))
				})?;
		}
		let (done, finished) = mpsc::sync_channel(1);
		let handler = block2::RcBlock::new(move |error: *mut NSError| {
			// SAFETY: the pointer is valid or null for the block's duration.
			let result =
				unsafe { error.as_ref() }.map_or_else(|| Ok(()), |error| Err(describe(error)));
			let _ = done.send(result);
		});
		// SAFETY: the stream is fully configured and the handler is retained
		// until it runs.
		unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
		match finished.recv_timeout(Duration::from_secs(10)) {
			Ok(Ok(())) => {
				Ok(Self {
					stream,
					_sink: sink,
					receiver,
				})
			}
			Ok(Err(error)) => Err(format!("could not start system audio: {error}")),
			Err(_) => Err("timed out starting system audio".to_owned()),
		}
	}
}

fn describe(error: &NSError) -> String {
	error.localizedDescription().to_string()
}

struct Ivars {
	sender: Mutex<mpsc::Sender<SystemMessage>>,
}

define_class!(
	#[unsafe(super(NSObject))]
	#[ivars = Ivars]
	struct SampleBufferSink;

	unsafe impl NSObjectProtocol for SampleBufferSink {}

	unsafe impl SCStreamOutput for SampleBufferSink {
		#[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
		#[allow(non_snake_case)]
		unsafe fn stream_didOutputSampleBuffer_ofType(
			&self,
			_stream: &SCStream,
			sample_buffer: &CMSampleBuffer,
			r#type: SCStreamOutputType,
		) {
			if r#type != SCStreamOutputType::Audio {
				return;
			}
			autoreleasepool(|_| {
				if let Some((mono, rate)) = read_pcm(sample_buffer) {
					let _ = self.ivars().sender.lock().unwrap().send(SystemMessage {
						samples: Arc::new(mono),
						rate,
					});
				}
			});
		}
	}
);

impl SampleBufferSink {
	fn new(sender: mpsc::Sender<SystemMessage>) -> Retained<Self> {
		let this = Self::alloc().set_ivars(Ivars {
			sender: Mutex::new(sender),
		});
		// SAFETY: calling NSObject's initializer on a fresh allocation.
		unsafe { msg_send![super(this), init] }
	}
}

/// Pull mono float PCM out of a captured audio sample buffer, or
/// `None` when the format is not linear float PCM (logged once per run).
fn read_pcm(buffer: &CMSampleBuffer) -> Option<(Vec<f32>, u32)> {
	use objc2_core_audio_types::AudioBufferList;
	use objc2_core_audio_types::kAudioFormatFlagIsFloat;
	use objc2_core_audio_types::kAudioFormatFlagIsNonInterleaved;
	use objc2_core_audio_types::kAudioFormatLinearPCM;
	use objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription;

	// SAFETY: plain getters on a valid sample buffer.
	let (description, samples) = unsafe { (buffer.format_description()?, buffer.num_samples()) };
	if samples == 0 {
		return None;
	}
	// SAFETY: the format description is the buffer's own, and audio format
	// descriptions carry an ASBD.
	let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description).as_ref()? };
	if asbd.mFormatID != kAudioFormatLinearPCM || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 {
		static WARNED: std::sync::Once = std::sync::Once::new();
		WARNED.call_once(|| {
			tracing::warn!("system audio arrived in a format the rolling buffer cannot read");
		});
		return None;
	}
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let rate = if asbd.mSampleRate.is_finite() && asbd.mSampleRate > 0.0 {
		asbd.mSampleRate as u32
	} else {
		SYSTEM_RATE
	};
	let channels = asbd.mChannelsPerFrame.max(1) as usize;
	let interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved == 0;

	// The buffer list lives inside the sample buffer; the retained block
	// buffer keeps it readable while we copy out of it.
	let mut needed = 0_usize;
	// SAFETY: a null output pointer with a size of zero only asks for the
	// needed size.
	let status = unsafe {
		buffer.audio_buffer_list_with_retained_block_buffer(
			std::ptr::from_mut(&mut needed),
			std::ptr::null_mut(),
			0,
			None,
			None,
			0,
			std::ptr::null_mut(),
		)
	};
	if status != 0 || needed == 0 {
		return None;
	}
	// The list starts 8-byte aligned inside the sample buffer, so a
	// `u64`-aligned Vec keeps it that way.
	let mut storage = vec![0_u64; needed.div_ceil(8)];
	let mut block: *mut objc2_core_media::CMBlockBuffer = std::ptr::null_mut();
	// SAFETY: `storage` is `needed` bytes large, as just measured, and the
	// out-pointer for the block buffer is valid.
	let status = unsafe {
		buffer.audio_buffer_list_with_retained_block_buffer(
			std::ptr::null_mut(),
			storage.as_mut_ptr().cast::<AudioBufferList>(),
			needed,
			None,
			None,
			0,
			std::ptr::from_mut(&mut block),
		)
	};
	if status != 0 {
		return None;
	}
	// SAFETY: the call above wrote a valid AudioBufferList of `needed` bytes.
	let list: &AudioBufferList = unsafe { &*storage.as_ptr().cast() };
	// The retained block buffer keeps the buffer list's memory alive; hold
	// it until the copy below is done. The call above retained it for us.
	// SAFETY: `block` is a retained CMBlockBuffer or null.
	let _retained = std::ptr::NonNull::new(block)
		.map(|block| unsafe { objc2_core_foundation::CFRetained::from_raw(block) });
	let mono = unsafe { mix_to_mono(list, interleaved, channels) };

	(!mono.is_empty()).then_some((mono, rate))
}

/// Downmix an `AudioBufferList` of float PCM to mono.
///
/// Interleaved buffers hold every channel in one buffer; deinterleaved
/// buffers hold one channel each. Channels are averaged so nothing is
/// dropped.
///
/// # Safety
///
/// `list` must point to a valid `AudioBufferList` whose buffers hold packed
/// float32 samples, as read from a sample buffer.
unsafe fn mix_to_mono(
	list: &objc2_core_audio_types::AudioBufferList,
	interleaved: bool,
	channels: usize,
) -> Vec<f32> {
	let count = list.mNumberBuffers.min(64) as usize;
	if count == 0 || channels == 0 {
		return Vec::new();
	}
	let buffers = list.mBuffers.as_ptr();
	if interleaved {
		// SAFETY: buffer 0 exists and holds `channels` interleaved floats.
		let buffer = unsafe { &*buffers };
		if buffer.mData.is_null() {
			return Vec::new();
		}
		// SAFETY: the buffer's data is packed float32 samples, and its byte
		// size covers them.
		let samples = unsafe {
			std::slice::from_raw_parts(
				buffer.mData.cast::<f32>(),
				buffer.mDataByteSize as usize / 4,
			)
		};
		return downmix(samples, channels);
	}
	// SAFETY: each of the first `count` buffers is one channel, of equal
	// length.
	let lanes: Vec<&[f32]> = (0..count)
		.map(|index| {
			let buffer = unsafe { &*buffers.add(index) };
			if buffer.mData.is_null() {
				return &[][..];
			}
			unsafe {
				std::slice::from_raw_parts(
					buffer.mData.cast::<f32>(),
					buffer.mDataByteSize as usize / 4,
				)
			}
		})
		.collect();
	let len = lanes.iter().map(|lane| lane.len()).min().unwrap_or(0);
	let used = lanes.iter().filter(|lane| !lane.is_empty()).count().max(1);
	let mut mono = Vec::with_capacity(len);
	for frame in 0..len {
		let sum: f32 = lanes
			.iter()
			.filter(|lane| !lane.is_empty())
			.map(|lane| lane[frame])
			.sum();
		mono.push(sum / used as f32);
	}
	mono
}

/// Open the microphone and system audio, reporting what could not be opened.
fn open_inputs(config: &RewindConfig, status: &mut Status) -> Result<Listening, String> {
	let mut mic = None;
	let mut mic_name = String::new();
	let mut mic_error = None;
	if config.mic {
		if super::audio::microphone_denied() {
			mic_error = Some("microphone permission not granted".to_owned());
		} else {
			match super::mic::MicHub::shared().listen() {
				Ok(listener) => {
					mic_name.clone_from(&listener.info.name);
					mic = Some(listener);
				}
				Err(error) => mic_error = Some(error),
			}
		}
	}
	let mut system = None;
	let mut system_error = None;
	if config.system_audio {
		match SystemAudio::open() {
			Ok(capture) => system = Some(capture),
			Err(_) if !system::screen_capture_allowed() => {
				system_error = Some("screen recording permission not granted".to_owned());
			}
			Err(error) => system_error = Some(error),
		}
	}
	if mic.is_none() && system.is_none() {
		let reasons = [mic_error, system_error]
			.into_iter()
			.flatten()
			.collect::<Vec<_>>()
			.join("; ");
		return Err(reasons);
	}
	let listening = Listening {
		mic,
		system,
		mic_name,
	};
	match (&mic_error, &system_error) {
		(None, None) => status.running(),
		(Some(mic), None) => status.set(true, Some(&format!("mic unavailable: {mic}"))),
		(None, Some(system)) => {
			status.set(true, Some(&format!("system audio unavailable: {system}")));
		}
		(Some(mic), Some(system)) => {
			status.set(
				true,
				Some(&format!(
					"mic unavailable: {mic}; system audio unavailable: {system}"
				)),
			);
		}
	}
	Ok(listening)
}
