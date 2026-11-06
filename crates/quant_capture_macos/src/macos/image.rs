//! Image work on captured frames: the change-detection thumbnail, Vision
//! OCR, and JPEG encoding.

use std::ffi::c_void;

use objc2::AnyThread;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFDictionary;
use objc2_core_foundation::CFMutableData;
use objc2_core_foundation::CFNumber;
use objc2_core_foundation::CFString;
use objc2_core_foundation::CGPoint;
use objc2_core_foundation::CGRect;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::CGBitmapContextCreate;
use objc2_core_graphics::CGColorSpace;
use objc2_core_graphics::CGContext;
use objc2_core_graphics::CGImage;
use objc2_core_graphics::CGImageAlphaInfo;
use objc2_core_graphics::CGInterpolationQuality;
use objc2_foundation::NSArray;
use objc2_foundation::NSDictionary;
use objc2_image_io::CGImageDestination;
use objc2_image_io::kCGImageDestinationLossyCompressionQuality;
use objc2_vision::VNImageOption;
use objc2_vision::VNImageRequestHandler;
use objc2_vision::VNRecognizeTextRequest;
use objc2_vision::VNRequest;
use objc2_vision::VNRequestTextRecognitionLevel;

use crate::config::OcrLevel;
use crate::dhash::DHash;
use crate::dhash::THUMB_HEIGHT;
use crate::dhash::THUMB_WIDTH;
use crate::dhash::box_downscale;

/// Oversampling of the thumbnail before box-averaging it down, so the hash
/// sees the average of each cell rather than whichever pixel CoreGraphics
/// sampled.
const OVERSAMPLE: usize = 4;

/// The frame's difference hash.
pub(crate) fn dhash(image: &CGImage) -> Option<DHash> {
	let (width, height) = (THUMB_WIDTH * OVERSAMPLE, THUMB_HEIGHT * OVERSAMPLE);
	let pixels = grayscale(image, width, height)?;
	Some(DHash::of_thumbnail(&box_downscale(
		&pixels,
		width,
		height,
		THUMB_WIDTH,
		THUMB_HEIGHT,
	)))
}

/// Draw `image` into a `width` × `height` 8-bit grayscale buffer.
pub(crate) fn grayscale(image: &CGImage, width: usize, height: usize) -> Option<Vec<u8>> {
	let mut pixels = vec![0u8; width * height];
	let space = CGColorSpace::new_device_gray()?;
	// SAFETY: `pixels` holds exactly `width * height` bytes, matching an
	// 8-bit, one-channel, unpadded layout, and outlives the context.
	let context = unsafe {
		CGBitmapContextCreate(
			pixels.as_mut_ptr().cast::<c_void>(),
			width,
			height,
			8,
			width,
			Some(&space),
			CGImageAlphaInfo::None.0,
		)
	}?;
	CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::Medium);
	#[allow(clippy::cast_precision_loss)]
	let rect = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
	CGContext::draw_image(Some(&context), rect, Some(image));
	drop(context);
	Some(pixels)
}

/// Read the text in `image`, top to bottom, one line per observation. If the
/// accurate recogniser fails (its model can fail to load, for example when
/// the disk is full) the fast one is tried instead.
pub(crate) fn recognize_text(image: &CGImage, level: OcrLevel) -> Result<String, String> {
	match recognize_with(image, level) {
		Err(error) if level == OcrLevel::Accurate => {
			tracing::warn!(%error, "accurate OCR failed; falling back to fast");
			recognize_with(image, OcrLevel::Fast)
		}
		result => result,
	}
}

fn recognize_with(image: &CGImage, level: OcrLevel) -> Result<String, String> {
	let request = VNRecognizeTextRequest::new();
	match level {
		OcrLevel::Fast => {
			request.setRecognitionLevel(VNRequestTextRecognitionLevel::Fast);
			request.setUsesLanguageCorrection(false);
		}
		OcrLevel::Accurate => {
			request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
			request.setUsesLanguageCorrection(true);
		}
	}
	let options = NSDictionary::<VNImageOption, AnyObject>::new();
	// SAFETY: the options dictionary is empty, so its generic types hold.
	let handler = unsafe {
		VNImageRequestHandler::initWithCGImage_options(
			VNImageRequestHandler::alloc(),
			image,
			&options,
		)
	};
	let as_request: &VNRequest = &request;
	handler
		.performRequests_error(&NSArray::from_slice(&[as_request]))
		.map_err(|error| error.localizedDescription().to_string())?;
	let lines: Vec<String> = request
		.results()
		.map(|observations| {
			observations
				.iter()
				.filter_map(|observation| {
					observation
						.topCandidates(1)
						.firstObject()
						.map(|text| text.string().to_string())
				})
				.filter(|line| !line.trim().is_empty())
				.collect()
		})
		.unwrap_or_default();
	Ok(lines.join("\n"))
}

/// Encode `image` as a JPEG of `quality` (0.0 to 1.0).
pub(crate) fn encode_jpeg(image: &CGImage, quality: f32) -> Option<Vec<u8>> {
	let data = CFMutableData::new(None, 0)?;
	let jpeg = CFString::from_static_str("public.jpeg");
	// SAFETY: `data` is a fresh mutable data object and the type identifier
	// is a valid UTI string.
	let destination = unsafe { CGImageDestination::with_data(&data, &jpeg, 1, None) }?;
	let quality = CFNumber::new_f64(f64::from(quality.clamp(0.0, 1.0)));
	// SAFETY: the key is ImageIO's constant.
	let key: &CFString = unsafe { kCGImageDestinationLossyCompressionQuality };
	let properties = CFDictionary::<CFString, CFNumber>::from_slices(&[key], &[&quality]);
	// SAFETY: the destination expects one image; properties map ImageIO keys
	// to CF values.
	let written = unsafe {
		destination.add_image(image, Some(properties.as_opaque()));
		destination.finalize()
	};
	written.then(|| data.to_vec())
}

/// Draw `text` large and black on white, for OCR tests.
#[cfg(test)]
pub(crate) fn render_text(
	text: &str,
	width: usize,
	height: usize,
) -> objc2_core_foundation::CFRetained<CGImage> {
	use objc2_app_kit::NSFont;
	use objc2_app_kit::NSFontAttributeName;
	use objc2_app_kit::NSGraphicsContext;
	use objc2_app_kit::NSStringDrawing;
	use objc2_core_graphics::CGBitmapContextCreateImage;
	use objc2_foundation::NSPoint;
	use objc2_foundation::NSString;

	let space = CGColorSpace::new_device_rgb().unwrap();
	// SAFETY: a null data pointer asks CoreGraphics to allocate the buffer.
	let context = unsafe {
		CGBitmapContextCreate(
			std::ptr::null_mut(),
			width,
			height,
			8,
			0,
			Some(&space),
			CGImageAlphaInfo::PremultipliedLast.0,
		)
	}
	.unwrap();
	#[allow(clippy::cast_precision_loss)]
	let rect = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
	CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
	CGContext::fill_rect(Some(&context), rect);
	let graphics = NSGraphicsContext::graphicsContextWithCGContext_flipped(&context, false);
	NSGraphicsContext::saveGraphicsState_class();
	NSGraphicsContext::setCurrentContext(Some(&graphics));
	let font = NSFont::systemFontOfSize(48.0);
	// SAFETY: the attribute key is AppKit's constant and maps to a font.
	let attributes = unsafe {
		NSDictionary::<NSString, AnyObject>::from_slices(
			&[NSFontAttributeName],
			&[&*font as &AnyObject],
		)
	};
	// SAFETY: drawing happens on this thread into the current context.
	unsafe {
		NSString::from_str(text)
			.drawAtPoint_withAttributes(NSPoint::new(40.0, 40.0), Some(&attributes));
	}
	NSGraphicsContext::restoreGraphicsState_class();
	CGBitmapContextCreateImage(Some(&context)).unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ocr_reads_rendered_text_and_jpeg_encodes() {
		let image = render_text("Quant reads the screen", 900, 160);
		let text = recognize_text(&image, OcrLevel::Accurate).unwrap();
		assert!(text.contains("Quant reads the screen"), "OCR read {text:?}");
		let fast = recognize_text(&image, OcrLevel::Fast).unwrap();
		assert!(
			fast.to_lowercase().contains("quant"),
			"fast OCR read {fast:?}"
		);

		let jpeg = encode_jpeg(&image, 0.6).unwrap();
		assert_eq!(
			&jpeg[..3],
			&[0xff, 0xd8, 0xff],
			"JPEG start-of-image marker"
		);
		assert!(jpeg.len() > 1000);
	}

	#[test]
	fn hash_tracks_content_not_brightness() {
		let a = dhash(&render_text("first frame", 900, 160)).unwrap();
		let again = dhash(&render_text("first frame", 900, 160)).unwrap();
		let b = dhash(&render_text("something else entirely here", 900, 160)).unwrap();
		assert_eq!(a.distance(&again), 0);
		assert!(a.distance(&b) > 4, "distance {}", a.distance(&b));
	}
}
