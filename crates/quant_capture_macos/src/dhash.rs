//! Perceptual change detection for screen frames.
//!
//! A frame is shrunk to a small grayscale thumbnail and each pixel compared
//! with its right-hand neighbour; the comparison bits form a difference hash.
//! Frames whose hash is close to the last stored frame's are skipped, so a
//! static screen costs one tiny thumbnail every interval and no OCR.

/// Columns of comparisons in the hash; the thumbnail is one pixel wider.
pub(crate) const HASH_COLS: usize = 16;
/// Rows in the hash and the thumbnail.
pub(crate) const HASH_ROWS: usize = 16;
/// Thumbnail width the hash is computed from.
pub(crate) const THUMB_WIDTH: usize = HASH_COLS + 1;
/// Thumbnail height the hash is computed from.
pub(crate) const THUMB_HEIGHT: usize = HASH_ROWS;

const WORDS: usize = HASH_COLS * HASH_ROWS / 64;

/// A 256-bit difference hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DHash([u64; WORDS]);

impl DHash {
	/// Hash a `THUMB_WIDTH` × `THUMB_HEIGHT` row-major grayscale thumbnail.
	pub(crate) fn of_thumbnail(pixels: &[u8]) -> Self {
		assert_eq!(
			pixels.len(),
			THUMB_WIDTH * THUMB_HEIGHT,
			"thumbnail has the wrong size"
		);
		let mut words = [0u64; WORDS];
		let mut bit = 0;
		for row in pixels.as_chunks::<THUMB_WIDTH>().0 {
			for pair in row.windows(2) {
				if pair[0] < pair[1] {
					words[bit / 64] |= 1 << (bit % 64);
				}
				bit += 1;
			}
		}
		Self(words)
	}

	/// Number of differing bits.
	pub(crate) fn distance(&self, other: &Self) -> u32 {
		self.0
			.iter()
			.zip(other.0)
			.map(|(a, b)| (a ^ b).count_ones())
			.sum()
	}
}

/// Box-average a row-major grayscale image down to `out_width` × `out_height`.
/// Each output pixel is the mean of the source pixels its cell covers.
pub(crate) fn box_downscale(
	pixels: &[u8],
	width: usize,
	height: usize,
	out_width: usize,
	out_height: usize,
) -> Vec<u8> {
	assert_eq!(pixels.len(), width * height, "image has the wrong size");
	let mut out = Vec::with_capacity(out_width * out_height);
	for oy in 0..out_height {
		let (y0, y1) = cell(oy, out_height, height);
		for ox in 0..out_width {
			let (x0, x1) = cell(ox, out_width, width);
			let mut sum = 0u64;
			for y in y0..y1 {
				sum += pixels[y * width + x0..y * width + x1]
					.iter()
					.map(|&p| u64::from(p))
					.sum::<u64>();
			}
			let count = ((y1 - y0) * (x1 - x0)).max(1) as u64;
			out.push(u8::try_from(sum / count).unwrap_or(u8::MAX));
		}
	}
	out
}

/// The source span `[start, end)` covered by output cell `index`, never empty.
fn cell(index: usize, cells: usize, size: usize) -> (usize, usize) {
	let start = index * size / cells;
	let end = ((index + 1) * size / cells).max(start + 1).min(size.max(1));
	(start.min(end.saturating_sub(1)), end)
}

/// Whether a candidate frame should be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
	/// Store it: nothing to compare with, the focus moved, or it changed
	/// enough.
	Store,
	/// Skip it: it looks like the last stored frame.
	Skip {
		/// Bits that differed.
		distance: u32,
	},
}

/// Remembers the last stored frame and decides whether new ones differ.
#[derive(Debug)]
pub(crate) struct FrameGate {
	min_change: u32,
	last: Option<(DHash, String)>,
}

impl FrameGate {
	/// A gate that keeps frames differing in at least `min_change` bits.
	pub(crate) fn new(min_change: u32) -> Self {
		Self {
			min_change,
			last: None,
		}
	}

	/// Decide on a frame with `hash`, taken while `focus` (an app and window
	/// key) had focus. A focus change always stores.
	pub(crate) fn decide(&self, hash: &DHash, focus: &str) -> Decision {
		match &self.last {
			Some((last, last_focus)) if last_focus == focus => {
				let distance = hash.distance(last);
				if distance < self.min_change {
					Decision::Skip { distance }
				} else {
					Decision::Store
				}
			}
			_ => Decision::Store,
		}
	}

	/// Record that a frame was stored.
	pub(crate) fn stored(&mut self, hash: DHash, focus: &str) {
		self.last = Some((hash, focus.to_owned()));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn gradient(flip_rows: usize) -> Vec<u8> {
		let mut pixels = Vec::new();
		for y in 0..THUMB_HEIGHT {
			for x in 0..THUMB_WIDTH {
				let value = u8::try_from(x * 10).unwrap();
				pixels.push(if y < flip_rows { 255 - value } else { value });
			}
		}
		pixels
	}

	#[test]
	fn identical_thumbnails_hash_identically() {
		let a = DHash::of_thumbnail(&gradient(0));
		assert_eq!(a.distance(&DHash::of_thumbnail(&gradient(0))), 0);
		// A rising gradient sets every bit.
		assert_eq!(a.0, [u64::MAX; WORDS]);
	}

	#[test]
	fn distance_counts_changed_comparisons() {
		let a = DHash::of_thumbnail(&gradient(0));
		let b = DHash::of_thumbnail(&gradient(2));
		assert_eq!(a.distance(&b), 2 * u32::try_from(HASH_COLS).unwrap());
		assert_eq!(b.distance(&a), a.distance(&b));
	}

	#[test]
	fn uniform_brightness_shift_does_not_change_the_hash() {
		let base = gradient(0);
		let brighter: Vec<u8> = base.iter().map(|p| p.saturating_add(40)).collect();
		assert_eq!(
			DHash::of_thumbnail(&base).distance(&DHash::of_thumbnail(&brighter)),
			0
		);
	}

	#[test]
	fn box_downscale_averages_cells() {
		// 4x2 image: left half 0, right half 200.
		let pixels = [0, 0, 200, 200, 0, 0, 200, 200];
		assert_eq!(box_downscale(&pixels, 4, 2, 2, 1), vec![0, 200]);
		assert_eq!(box_downscale(&pixels, 4, 2, 1, 1), vec![100]);
		// Upscaling repeats source pixels instead of reading out of bounds.
		assert_eq!(box_downscale(&[7, 9], 2, 1, 4, 1), vec![7, 7, 9, 9]);
	}

	#[test]
	fn gate_stores_first_and_changed_frames_and_skips_similar_ones() {
		let mut gate = FrameGate::new(4);
		let a = DHash::of_thumbnail(&gradient(0));
		assert_eq!(gate.decide(&a, "Zed|main.rs"), Decision::Store);
		gate.stored(a, "Zed|main.rs");
		assert_eq!(
			gate.decide(&a, "Zed|main.rs"),
			Decision::Skip { distance: 0 }
		);

		// Three flipped bits stay under the threshold of four.
		let mut near = a;
		near.0[0] ^= 0b111;
		assert_eq!(
			gate.decide(&near, "Zed|main.rs"),
			Decision::Skip { distance: 3 }
		);
		near.0[1] ^= 1;
		assert_eq!(gate.decide(&near, "Zed|main.rs"), Decision::Store);

		// The same pixels under a different window still store.
		assert_eq!(gate.decide(&a, "Safari|news"), Decision::Store);
	}

	#[test]
	fn zero_threshold_keeps_every_frame() {
		let mut gate = FrameGate::new(0);
		let a = DHash::of_thumbnail(&gradient(0));
		gate.stored(a, "x");
		assert_eq!(gate.decide(&a, "x"), Decision::Store);
	}
}
