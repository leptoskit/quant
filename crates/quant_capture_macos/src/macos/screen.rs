//! The `screen` source: periodic frames of the main display, read with OCR.
//!
//! Each interval it checks, in order of cost, that someone is at the
//! machine, that no excluded app or private window has focus, captures a
//! scaled-down frame with `ScreenCaptureKit` (excluded apps' windows are left
//! out of the capture itself), and compares a 256-bit difference hash with
//! the last stored frame. Only frames that changed are read with Vision and
//! encoded.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use jiff::Timestamp;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::rc::autoreleasepool;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_graphics::CGRequestScreenCaptureAccess;
use objc2_foundation::NSArray;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCContentFilter;
use objc2_screen_capture_kit::SCScreenshotManager;
use objc2_screen_capture_kit::SCShareableContent;
use objc2_screen_capture_kit::SCStreamConfiguration;
use objc2_screen_capture_kit::SCWindow;
use quant_core::Record;

use super::Context;
use super::image;
use super::system;
use crate::config::ScreenConfig;
use crate::control::Status;
use crate::dhash::Decision;
use crate::dhash::FrameGate;
use crate::focus::Focus;
use crate::privacy::Exclusions;
use crate::stream;

/// How long to wait for `ScreenCaptureKit` before giving up on a frame.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Capture frames until stopped.
pub(crate) fn run(cx: &Context, config: &ScreenConfig, exclusions: &Exclusions) {
	let mut screen = Screen {
		cx,
		config,
		exclusions,
		status: cx.status("screen"),
		gate: FrameGate::new(config.min_change),
		asked_permission: false,
	};
	// ScreenCaptureKit needs a window server connection, which this makes.
	let _ = CGMainDisplayID();
	let interval = Duration::from_secs(config.interval_secs.max(1));
	loop {
		autoreleasepool(|_| screen.tick());
		if cx.stop.wait(interval) {
			break;
		}
	}
	screen.status.stopped("stopped");
}

struct Screen<'a> {
	cx: &'a Context,
	config: &'a ScreenConfig,
	exclusions: &'a Exclusions,
	status: Status,
	gate: FrameGate,
	asked_permission: bool,
}

impl Screen<'_> {
	fn tick(&mut self) {
		if !system::screen_capture_allowed() {
			if self.config.request_permission && !self.asked_permission {
				self.asked_permission = true;
				// Shows the system prompt once; the grant usually takes effect
				// after the process restarts.
				let _ = CGRequestScreenCaptureAccess();
			}
			self.status
				.stopped("screen recording permission not granted");
			return;
		}
		#[allow(clippy::cast_precision_loss)]
		let (presence, _) = system::presence(self.config.idle_pause_secs.max(1) as f64);
		let paused = if presence.locked {
			Some("paused: screen locked")
		} else if presence.display_asleep {
			Some("paused: display asleep")
		} else if presence.idle {
			Some("paused: idle")
		} else {
			None
		};
		if let Some(reason) = paused {
			self.status.set(true, Some(reason));
			return;
		}
		let focus = system::frontmost();
		if self.excluded(focus.as_ref()) {
			self.status.set(
				true,
				Some("paused: excluded app or private window in focus"),
			);
			return;
		}
		let captured_at = Timestamp::now();
		let frame = match capture(self.config, self.exclusions) {
			Ok(frame) => frame,
			Err(error) => {
				tracing::warn!(%error, "screen capture failed");
				self.status
					.set(true, Some(&format!("capture failed: {error}")));
				return;
			}
		};
		self.status.running();
		// Focus may have moved to an excluded app while capturing.
		if self.excluded(system::frontmost().as_ref()) {
			return;
		}
		self.store(&frame, focus, captured_at);
	}

	fn excluded(&self, focus: Option<&Focus>) -> bool {
		focus.is_some_and(|focus| {
			self.exclusions
				.excludes(focus.bundle_id.as_deref(), focus.title.as_deref())
		})
	}

	fn store(&mut self, frame: &CGImage, focus: Option<Focus>, captured_at: Timestamp) {
		let Some(hash) = image::dhash(frame) else {
			tracing::warn!("could not hash a screen frame");
			return;
		};
		let key = focus.as_ref().map(Focus::key).unwrap_or_default();
		if let Decision::Skip { distance } = self.gate.decide(&hash, &key) {
			tracing::trace!(distance, "screen unchanged; frame skipped");
			return;
		}
		let text = match image::recognize_text(frame, self.config.ocr_level) {
			Ok(text) => text,
			Err(error) => {
				tracing::warn!(%error, "OCR failed");
				String::new()
			}
		};
		let mut record = Record::new(stream("screen.frame"), self.cx.device).starting(captured_at);
		if self.config.store_images {
			if let Some(jpeg) = image::encode_jpeg(frame, self.config.jpeg_quality) {
				match self
					.cx
					.sink
					.media(&stream("screen.frame"), &jpeg, "image/jpeg")
				{
					Ok(media) => record = record.with_media(media),
					Err(error) => tracing::warn!(%error, "sink refused a screen frame image"),
				}
			} else {
				tracing::warn!("could not encode a screen frame");
			}
		}
		if !text.is_empty() {
			record = record.with_text(text);
		}
		if let Some(focus) = focus {
			record = record.with("app", focus.app);
			if let Some(bundle_id) = focus.bundle_id {
				record = record.with("bundle_id", bundle_id);
			}
			if let Some(title) = focus.title {
				record = record.with("title", title);
			}
		}
		self.cx.emit(record);
		self.gate.stored(hash, &key);
	}
}

/// Lets a retained object cross from `ScreenCaptureKit`'s callback queue to the
/// capture thread.
struct Handoff<T>(T);

// SAFETY: the wrapped objects (shareable content and CGImages) are immutable
// snapshots that Apple documents as safe to use from any thread; the callback
// only moves them to the waiting thread.
unsafe impl<T> Send for Handoff<T> {}

/// Capture the main display, scaled to at most `max_width` pixels wide, with
/// excluded apps' and private windows left out.
fn capture(config: &ScreenConfig, exclusions: &Exclusions) -> Result<CFRetained<CGImage>, String> {
	let content = shareable_content()?;
	// SAFETY: plain property reads on a valid object.
	let displays = unsafe { content.displays() };
	let main = CGMainDisplayID();
	let display = displays
		.iter()
		// SAFETY: plain property read.
		.find(|display| unsafe { display.displayID() } == main)
		.or_else(|| displays.firstObject())
		.ok_or("no display to capture")?;

	// SAFETY: plain property reads on valid objects.
	let hidden: Vec<Retained<SCWindow>> = unsafe { content.windows() }
		.iter()
		.filter(|window| {
			// SAFETY: plain property reads.
			let (bundle_id, title) = unsafe {
				(
					window
						.owningApplication()
						.map(|app| app.bundleIdentifier().to_string()),
					window.title().map(|title| title.to_string()),
				)
			};
			exclusions.excludes(bundle_id.as_deref(), title.as_deref())
		})
		.collect();
	let hidden_refs: Vec<&SCWindow> = hidden.iter().map(|window| &**window).collect();
	let hidden = NSArray::from_slice(&hidden_refs);

	// SAFETY: the display and window list come from the same, current
	// shareable content.
	let filter = unsafe {
		SCContentFilter::initWithDisplay_excludingWindows(
			SCContentFilter::alloc(),
			&display,
			&hidden,
		)
	};
	// SAFETY: plain property reads on the filter (macOS 14+).
	let (rect, scale) = unsafe { (filter.contentRect(), f64::from(filter.pointPixelScale())) };
	let (width, height) = output_size(rect.size.width, rect.size.height, scale, config.max_width);

	let configuration = unsafe { SCStreamConfiguration::new() };
	// SAFETY: plain property writes on a fresh configuration.
	unsafe {
		configuration.setWidth(width);
		configuration.setHeight(height);
		configuration.setShowsCursor(false);
		configuration.setScalesToFit(true);
	}

	let (sender, receiver) = mpsc::sync_channel(1);
	let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
		let result = match NonNull::new(image) {
			// SAFETY: the handler is given a valid image it does not own;
			// retaining keeps it alive past the callback.
			Some(image) => Ok(Handoff(unsafe { CFRetained::retain(image) })),
			None => Err(describe(error)),
		};
		let _ = sender.send(result);
	});
	// SAFETY: the filter and configuration are valid; the handler is retained
	// by ScreenCaptureKit until it runs.
	unsafe {
		SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
			&filter,
			&configuration,
			Some(&handler),
		);
	}
	match receiver.recv_timeout(CAPTURE_TIMEOUT) {
		Ok(result) => result.map(|image| image.0),
		Err(_) => Err("timed out waiting for ScreenCaptureKit".to_owned()),
	}
}

/// The windows, displays and apps that can be captured right now.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
	let (sender, receiver) = mpsc::sync_channel(1);
	let handler = RcBlock::new(
		move |content: *mut SCShareableContent, error: *mut NSError| {
			// SAFETY: the handler is given a valid object or null; retaining
			// keeps it alive past the callback.
			let result = match unsafe { Retained::retain(content) } {
				Some(content) => Ok(Handoff(content)),
				None => Err(describe(error)),
			};
			let _ = sender.send(result);
		},
	);
	// SAFETY: the handler is retained by ScreenCaptureKit until it runs.
	unsafe {
		SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
			true, true, &handler,
		);
	}
	match receiver.recv_timeout(CAPTURE_TIMEOUT) {
		Ok(result) => result.map(|content| content.0),
		Err(_) => Err("timed out listing shareable content".to_owned()),
	}
}

fn describe(error: *mut NSError) -> String {
	// SAFETY: the error pointer is valid or null for the callback's duration.
	match unsafe { error.as_ref() } {
		Some(error) => error.localizedDescription().to_string(),
		None => "unknown ScreenCaptureKit error".to_owned(),
	}
}

/// Pixel size to capture a `width` × `height` point area at `scale`, capped to
/// `max_width` pixels wide with the aspect ratio kept.
fn output_size(width: f64, height: f64, scale: f64, max_width: u32) -> (usize, usize) {
	let native = (width * scale).max(1.0);
	let target = native.min(f64::from(max_width.max(64)));
	let ratio = if width > 0.0 { height / width } else { 1.0 };
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let size = (
		target.round() as usize,
		(target * ratio).round().max(1.0) as usize,
	);
	size
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn output_size_caps_width_and_keeps_aspect() {
		assert_eq!(output_size(1512.0, 982.0, 2.0, 1600), (1600, 1039));
		assert_eq!(output_size(1280.0, 800.0, 1.0, 1600), (1280, 800));
		assert_eq!(output_size(1920.0, 1080.0, 2.0, 1920), (1920, 1080));
	}
}
