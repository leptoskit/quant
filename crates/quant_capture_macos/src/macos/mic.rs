//! One microphone stream shared by every listener.
//!
//! The `audio` source and the `rewind` source both want the default input
//! device; opening it twice would double the background cost and can fail
//! outright on machines that only allow one client. The hub opens one cpal
//! stream for the process and fans samples out to whoever is listening,
//! closing the device again once the last listener goes away.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
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
use jiff::Timestamp;

/// How often the hub thread checks for the last listener leaving.
const POLL: Duration = Duration::from_millis(200);

/// What the device callback produced.
pub(crate) enum MicMessage {
	/// Interleaved samples and when the callback got them. Shared, so
	/// fanning out to several listeners copies no audio.
	Samples {
		/// Arrival time.
		at: Timestamp,
		/// Interleaved samples, all channels.
		interleaved: Arc<Vec<f32>>,
	},
	/// The stream reported an error.
	Error(String),
}

/// The device the hub opened.
#[derive(Clone, Debug)]
pub(crate) struct MicInfo {
	/// The device's name, as `data.device` on records.
	pub(crate) name: String,
	/// Channels the device delivers.
	pub(crate) channels: usize,
	/// The device's sample rate.
	pub(crate) rate: u32,
}

/// A subscription to the shared microphone. Dropping it unsubscribes; the
/// device closes once the last listener does.
pub(crate) struct MicListener {
	/// The device behind the samples.
	pub(crate) info: MicInfo,
	receiver: mpsc::Receiver<MicMessage>,
	hub: &'static MicHub,
	live: Arc<AtomicUsize>,
}

impl MicListener {
	/// The next message, waiting up to `timeout`.
	pub(crate) fn recv(&self, timeout: Duration) -> Option<MicMessage> {
		self.receiver.recv_timeout(timeout).ok()
	}
}

impl Drop for MicListener {
	fn drop(&mut self) {
		self.live.fetch_sub(1, Ordering::Release);
		self.hub.maybe_close();
	}
}

/// The process-wide microphone hub.
pub(crate) struct MicHub {
	inner: std::sync::Mutex<HubInner>,
	/// How many listeners are alive, shared with the reader thread.
	live: Arc<AtomicUsize>,
}

#[derive(Default)]
struct HubInner {
	listeners: Vec<mpsc::Sender<MicMessage>>,
	info: Option<MicInfo>,
	reader: Option<JoinHandle<()>>,
	stream: Option<Stream>,
}

impl MicHub {
	/// The shared hub.
	pub(crate) fn shared() -> &'static Self {
		static HUB: OnceLock<MicHub> = OnceLock::new();
		HUB.get_or_init(|| {
			MicHub {
				inner: std::sync::Mutex::new(HubInner::default()),
				live: Arc::new(AtomicUsize::new(0)),
			}
		})
	}

	/// Subscribe to the default microphone, opening it if this is the first
	/// listener.
	pub(crate) fn listen(&'static self) -> Result<MicListener, String> {
		let mut inner = self.lock();
		if inner.info.is_none() {
			// Count ourselves before the device opens, so the reader thread
			// cannot see a hub with no listeners while it is starting.
			self.live.fetch_add(1, Ordering::Release);
			let opened = match open() {
				Ok(opened) => opened,
				Err(error) => {
					self.live.fetch_sub(1, Ordering::Release);
					return Err(error);
				}
			};
			let reader = thread::Builder::new()
				.name("quant-mic".to_owned())
				.spawn({
					let receiver = opened.receiver;
					let (hub, live) = (std::ptr::from_ref(self), Arc::clone(&self.live));
					// The hub lives in a process-wide `OnceLock`, so the
					// pointer outlives the thread.
					// SAFETY: `MicHub::shared` is a `&'static` reference.
					let fanout = Fanout {
						hub: unsafe { &*hub },
						live,
					};
					move || {
						super::system::lower_priority();
						while !fanout.quiet() {
							match receiver.recv_timeout(POLL) {
								Ok(message) => fanout.send(&message),
								Err(mpsc::RecvTimeoutError::Timeout) => {}
								Err(mpsc::RecvTimeoutError::Disconnected) => break,
							}
						}
					}
				})
				.map_err(|error| {
					self.live.fetch_sub(1, Ordering::Release);
					format!("could not start the mic thread: {error}")
				})?;
			inner.info = Some(opened.info);
			inner.reader = Some(reader);
			// Dropping the stream stops the device, so it is only dropped
			// once the reader has noticed nobody is listening.
			inner.stream = Some(opened.stream);
		} else {
			self.live.fetch_add(1, Ordering::Release);
		}
		let (sender, receiver) = mpsc::channel();
		inner.listeners.push(sender);
		Ok(MicListener {
			info: inner.info.clone().expect("just opened"),
			receiver,
			hub: self,
			live: Arc::clone(&self.live),
		})
	}

	/// Close the device once the last listener has gone. The reader thread
	/// exits on its own at the next poll; its handle is dropped here rather
	/// than joined, because joining under this lock would deadlock with the
	/// reader's own fan-out.
	fn maybe_close(&self) {
		if self.live.load(Ordering::Acquire) > 0 {
			return;
		}
		let mut inner = self.lock();
		inner.listeners.clear();
		if inner.info.is_some() {
			inner.reader = None;
			inner.stream = None;
			inner.info = None;
		}
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
		self.inner
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// Fan messages out to the current listeners, and tell when the last one has
/// gone so the reader can stop.
struct Fanout {
	hub: &'static MicHub,
	live: Arc<AtomicUsize>,
}

impl Fanout {
	fn send(&self, message: &MicMessage) {
		let inner = self.hub.lock();
		for listener in &inner.listeners {
			let _ = listener.send(shared_message(message));
		}
	}

	/// Whether every listener has been dropped. The reader checks this
	/// whenever the device is quiet.
	fn quiet(&self) -> bool {
		self.live.load(Ordering::Acquire) == 0
	}
}

/// Clone a message for one listener, sharing the samples.
fn shared_message(message: &MicMessage) -> MicMessage {
	match message {
		MicMessage::Samples { at, interleaved } => {
			MicMessage::Samples {
				at: *at,
				interleaved: Arc::clone(interleaved),
			}
		}
		MicMessage::Error(error) => MicMessage::Error(error.clone()),
	}
}

/// Open the default input device once, whatever format it speaks.
fn open() -> Result<Opened, String> {
	let host = cpal::default_host();
	let device = host.default_input_device().ok_or("no microphone found")?;
	let name = device
		.to_string()
		.split(" (")
		.next()
		.unwrap_or("microphone")
		.to_owned();
	let supported = device
		.default_input_config()
		.map_err(|error| format!("microphone unavailable: {error}"))?;
	let channels = usize::from(supported.channels()).max(1);
	let rate = supported.sample_rate();
	let (sender, receiver) = mpsc::channel();
	let stream = open_stream(
		&device,
		supported.sample_format(),
		supported.config(),
		sender.clone(),
	)
	.map_err(|error| format!("could not open microphone: {error}"))?;
	stream
		.play()
		.map_err(|error| format!("could not start microphone: {error}"))?;
	Ok(Opened {
		info: MicInfo {
			name,
			channels,
			rate,
		},
		receiver,
		stream,
	})
}

struct Opened {
	info: MicInfo,
	receiver: mpsc::Receiver<MicMessage>,
	stream: Stream,
}

/// Build an input stream that forwards `f32` samples, whatever the device's
/// native format.
fn open_stream(
	device: &cpal::Device,
	format: SampleFormat,
	config: StreamConfig,
	sender: mpsc::Sender<MicMessage>,
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
	sender: mpsc::Sender<MicMessage>,
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
				let _ = sender.send(MicMessage::Samples {
					at: Timestamp::now(),
					interleaved: Arc::new(samples),
				});
			},
			move |error| {
				let _ = errors.send(MicMessage::Error(error.to_string()));
			},
			None,
		)
		.map_err(|error| error.to_string())
}
