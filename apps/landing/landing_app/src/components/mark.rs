//! The Riemann mark and lockup, drawn in rough ink.
//!
//! The mark is a disc that is smooth on its left half and sampled into bars
//! on its right, like a Riemann sum: life is continuous, and Quant measures
//! it in discrete quanta. The bars double as an audio waveform. The geometry
//! is the one in `assets/brand/mark.svg`, redrawn by the rough engine so it
//! sits in the page's hand-drawn language.

use leptoskit::prelude::*;
use skribble_leptoskit::FillStyle;
use skribble_leptoskit::Shape;
use skribble_leptoskit::Sketch;

use crate::theme;

/// The mark's own coordinate system, matching `assets/brand/mark.svg`.
const VIEW: f64 = 512.0;

/// The half-disc's radius in that system.
const RADIUS: f64 = 192.0;

/// The disc's centre in that system.
const CENTRE: f64 = 256.0;

/// The three bars, as (top, height, left edge): the disc's right half
/// sampled into capsules of falling height.
const BARS: [(f64, f64, f64); 3] = [
	(68.9, 374.2, 278.0),
	(96.6, 318.8, 342.0),
	(168.7, 174.6, 406.0),
];

/// A bar's width in that system; its ends round into capsules.
const BAR_WIDTH: f64 = 42.0;

/// The Riemann mark at `size` logical pixels square: a violet half-disc and
/// three coral bars, outlined in ink by the rough engine.
///
/// The drawing is decorative; the lockup beside it names the site.
#[component]
pub fn Mark(
	/// The mark's width and height, in logical pixels.
	size: f32,
	/// Chooses the wobble. The same seed always draws the same mark.
	seed: i64,
) -> Element {
	let palette = theme::ink();
	let scale = f64::from(size) / VIEW;

	// The smooth left half: points around the disc from its bottom, through
	// its left, to its top, closed by the diameter.
	let half = (0..=18)
		.map(|step| {
			let angle = std::f64::consts::PI * f64::from(90 + step * 10) / 180.0;
			(
				(CENTRE + RADIUS * angle.cos()) * scale,
				(CENTRE + RADIUS * angle.sin()) * scale,
			)
		})
		.collect::<Vec<_>>();
	// The sampled right half: three rounded bars of falling height.
	let bars = BARS
		.iter()
		.map(|&(top, height, left)| {
			Shape::rounded_rectangle(
				left * scale,
				top * scale,
				BAR_WIDTH * scale,
				height * scale,
				(BAR_WIDTH / 2.0) * scale,
			)
		})
		.collect::<Vec<_>>();
	view! {
		<View class="relative">
			<Sketch
				width=size
				height=size
				seed=seed
				shapes=vec![Shape::polygon(half)]
				fill_style=FillStyle::Solid
				fill=palette.violet
				stroke=palette.skribble.ink()
				theme=palette.skribble
			/>
			<Sketch
				class="absolute top-0 left-0"
				width=size
				height=size
				seed=seed + 7
				shapes=bars
				fill_style=FillStyle::Solid
				fill=palette.coral
				stroke=palette.skribble.ink()
				theme=palette.skribble
			/>
		</View>
	}
}

/// The lockup: the mark and the wordmark, side by side.
#[component]
pub fn Lockup(
	/// The mark's width and height, in logical pixels.
	size: f32,
	/// Chooses the wobble.
	seed: i64,
	/// The id of the lockup; its wordmark's is this followed by `-word`.
	#[prop(into)]
	id: String,
) -> Element {
	view! {
		<View id=id.clone() class="flex-row items-center gap-2">
			<Mark size=size seed=seed />
			<Text id=format!("{id}-word")>"quant"</Text>
		</View>
	}
}
