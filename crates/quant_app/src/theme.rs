//! The phone's one palette, and the mark it draws.
//!
//! The phone is always night: every color here is picked against `#0E0C24`,
//! so the system controls (such as the collecting switch) draw dark too. The
//! desktop window keeps its own light and dark styling in [`crate::ui`].

use leptoskit::native::Color;
use leptoskit::native::Paint;
use leptoskit::native::PathData;
use leptoskit::native::Stroke;
use leptoskit::native::StrokeScaling;
use leptoskit::native::VectorPath;
use leptoskit::native::ViewBox;
use leptoskit::prelude::Element;
use leptoskit::prelude::Vector;
use leptoskit::prelude::View;

/// The full-screen container: night fills the window.
pub const SCREEN: &str = "flex-1 min-h-screen bg-[#0E0C24]";
/// The scrollable content's padding, inset clear of the safe areas.
pub const CONTENT: &str = "gap-5 px-5 pt-safe-offset-4 pb-safe-offset-10";
/// A bordered card on the night background.
pub const CARD: &str = "gap-4 p-5 rounded-3xl border border-[#2A2560] bg-[#17143A]";
/// The raised card that holds the current headline.
pub const HERO: &str = "gap-4 p-6 rounded-3xl border border-[#3A3380] bg-[#1E1A4A]";
/// The welcome screen's one great promise.
pub const DISPLAY: &str = "text-4xl font-bold leading-tight text-[#F4F1FF]";
/// A stage or card title.
pub const TITLE: &str = "text-2xl font-bold leading-snug text-[#F4F1FF]";
/// A card's heading.
pub const HEADING: &str = "text-lg font-semibold text-[#F4F1FF]";
/// The name of a row.
pub const LABEL: &str = "text-base font-semibold text-[#F4F1FF]";
/// The small kicker above a title.
pub const EYEBROW: &str = "text-xs font-semibold text-[#9D90FF]";
/// A paragraph of explanation.
pub const BODY: &str = "text-base leading-relaxed text-[#E4E0FA]";
/// Secondary explanation, one step quieter than [`BODY`].
pub const MUTED: &str = "text-sm leading-relaxed text-[#A9A3D6]";
/// The smallest label: a hint or a count.
pub const CAPTION: &str = "text-xs leading-normal text-[#8A83C4]";
/// Why something failed.
pub const PROBLEM: &str = "text-sm leading-relaxed text-[#FF8FA3]";
/// A large count, such as what is waiting to be delivered.
pub const STAT: &str = "text-4xl font-bold text-[#FF7A59]";
/// The one main action on a screen.
pub const PRIMARY: &str =
	"px-5 py-4 rounded-2xl bg-[#6B5BF5] text-white text-base font-semibold active:bg-[#5A4AE6]";
/// A second action beside a main one.
pub const SECONDARY: &str =
	"px-5 py-4 rounded-2xl bg-[#221E4F] text-[#E4E0FA] text-base font-medium active:bg-[#2C2763]";
/// A third action: a text-weight button that leaves the layout alone.
pub const QUIET: &str =
	"px-4 py-3 rounded-xl text-[#A9A3D6] text-sm font-medium active:bg-[#221E4F]";
/// A text field where the owner pastes a pairing link.
pub const INPUT: &str =
	"px-4 py-4 rounded-2xl border border-[#3A3380] bg-[#0B0A1D] text-base text-[#F4F1FF]";
/// A command to run on the computer, or another block of machine text.
pub const CODE: &str =
	"p-4 rounded-2xl border border-[#2A2560] bg-[#0B0A1D] text-sm leading-relaxed text-[#CFC8FF]";
/// A row beneath the first in a card.
pub const ROW: &str = "gap-2 pt-4 border-t border-[#2A2560]";
/// The tile behind a row's icon.
pub const TILE: &str = "w-10 h-10 rounded-xl items-center justify-center bg-[#251F5C]";
/// The tile behind a granted row's icon.
pub const TILE_OK: &str = "w-10 h-10 rounded-xl items-center justify-center bg-[#123A30]";
/// The tile behind a row that needs attention.
pub const TILE_WARN: &str = "w-10 h-10 rounded-xl items-center justify-center bg-[#3D2E14]";
/// The tile behind a row that has stopped.
pub const TILE_BAD: &str = "w-10 h-10 rounded-xl items-center justify-center bg-[#3D1626]";
/// A row's icon.
pub const ICON: &str = "w-5 h-5 text-[#9D90FF]";
/// A granted row's icon.
pub const ICON_OK: &str = "w-5 h-5 text-[#5BE0B3]";
/// An icon that needs attention.
pub const ICON_WARN: &str = "w-5 h-5 text-[#FFC46B]";
/// An icon that has stopped.
pub const ICON_BAD: &str = "w-5 h-5 text-[#FF8FA3]";

/// The mark's violet.
pub const VIOLET: Color = Color::rgba(0x9D, 0x90, 0xFF, 0xFF);
/// The tail that leaves the circle.
pub const CORAL: Color = Color::rgba(0xFF, 0x7A, 0x59, 0xFF);
/// The wordmark's ink.
pub const INK: Color = Color::rgba(0xF4, 0xF1, 0xFF, 0xFF);
/// The splash's one breath of ring, translucent violet.
pub const HALO: Color = Color::rgba(0x9D, 0x90, 0xFF, 0x66);

/// The Q's continuous left side.
const MARK_ARC: &str = "M301.55 426A176 176 0 1 1 256 80";
/// The coral tail that leaves the circle.
const MARK_TAIL: &str = "M330.95 330.95L429.95 429.95";
/// The five dots, as (centre x, centre y, radius), shrinking clockwise.
const MARK_DOTS: [(f32, f32, f32); 5] = [
	(321.93, 92.82, 24.0),
	(378.26, 129.4, 20.88),
	(416.78, 184.41, 17.76),
	(431.89, 249.86, 14.64),
	(421.39, 316.2, 11.52),
];
/// The mark's stroke width, in view box units.
const MARK_STROKE: f32 = 48.0;

/// The mark's view box.
fn mark_box() -> ViewBox {
	ViewBox::new(0.0, 0.0, 512.0, 512.0).expect("a positive size")
}

/// A mark stroke: in the drawing's own units, so it scales with the mark.
fn mark_stroke(color: Color) -> Stroke {
	Stroke::new(color, MARK_STROKE)
		.expect("a positive width")
		.round()
		.scaling(StrokeScaling::Drawing)
}

/// A path stroked as given.
fn stroked(data: PathData, stroke: Stroke) -> VectorPath {
	VectorPath::new(data).stroke(stroke)
}

/// The Q's continuous left side, as its own full-size layer.
pub fn mark_arc() -> Element {
	Vector(mark_box()).class("absolute inset-0").path(stroked(
		PathData::parse(MARK_ARC).expect("a valid arc"),
		mark_stroke(VIOLET),
	))
}

/// One of the five dots, as its own full-size layer, so it rotates and
/// scales about the mark's centre with every other layer.
pub fn mark_dot(index: usize) -> Element {
	let (x, y, radius) = MARK_DOTS[index];
	Vector(mark_box()).class("absolute inset-0").path(
		VectorPath::new(PathData::circle(x, y, radius).expect("a positive radius")).fill(VIOLET),
	)
}

/// The coral tail that leaves the circle.
pub fn mark_tail() -> Element {
	Vector(mark_box()).class("absolute inset-0").path(stroked(
		PathData::parse(MARK_TAIL).expect("a valid tail"),
		mark_stroke(CORAL),
	))
}

/// The soft ring the splash breathes out from the mark.
pub fn mark_halo() -> Element {
	Vector(mark_box())
		.class("absolute inset-0 opacity-0")
		.path(stroked(
			PathData::circle(256.0, 256.0, 176.0).expect("a positive radius"),
			Stroke::new(HALO, 6.0)
				.expect("a positive width")
				.scaling(StrokeScaling::Drawing),
		))
}

/// The static mark: the arc, the five dots and the tail in one square. The
/// caller gives it a size, such as `"w-8 h-8"`.
pub fn logo(size: &'static str) -> Element {
	View()
		.class(format!("relative {size}"))
		.accessibility_label("Quant")
		.child(mark_arc())
		.children((0..MARK_DOTS.len()).map(mark_dot))
		.child(mark_tail())
}

/// One letter of the wordmark.
#[derive(Clone, Copy)]
enum Letter {
	/// A round letter, drawn as a circle.
	Circle(f32, f32, f32),
	/// A line the monoline letters trace.
	Line(&'static str),
}

/// The wordmark's letters, in drawing order.
const WORDMARK: [Letter; 7] = [
	Letter::Circle(50.0, 50.0, 39.0),
	Letter::Line("M89 11V150"),
	Letter::Line("M139 11V50A39 39 0 0 0 217 50V11M217 11V89"),
	Letter::Circle(306.0, 50.0, 39.0),
	Letter::Line("M345 11V89"),
	Letter::Line("M395 11V89M395 50A39 39 0 0 1 473 50V89"),
	Letter::Line("M536 -36V62A27 27 0 0 0 563 89H574M512 11H570"),
];

/// The wordmark's view box, with room for the letters' strokes.
fn wordmark_box() -> ViewBox {
	ViewBox::new(-12.0, -48.0, 598.0, 210.0).expect("a positive size")
}

/// The wordmark's stroke: in the drawing's own units, so it scales.
fn wordmark_stroke() -> Stroke {
	Stroke::new(INK, 22.0)
		.expect("a positive width")
		.round()
		.scaling(StrokeScaling::Drawing)
}

/// One wordmark letter as a painted path.
fn wordmark_path(letter: Letter) -> VectorPath {
	let data = match letter {
		Letter::Circle(x, y, radius) => PathData::circle(x, y, radius).expect("a positive radius"),
		Letter::Line(path) => PathData::parse(path).expect("a valid wordmark path"),
	};
	stroked(data, wordmark_stroke())
}

/// The wordmark alone. Its ratio is 598:210, so `"w-[150px] h-[53px]"` is
/// the splash size and `"w-[68px] h-[24px]"` sits in the top bar.
pub fn wordmark(size: &'static str) -> Element {
	WORDMARK
		.into_iter()
		.fold(Vector(wordmark_box()).class(size), |drawing, letter| {
			drawing.path(wordmark_path(letter))
		})
		.accessibility_label("quant")
}

/// A line icon, drawn in a 24 x 24 box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
	/// A heartbeat trace.
	Pulse,
	/// A padlock.
	Lock,
	/// A key.
	Key,
	/// A shield with a tick.
	Shield,
	/// A map pin.
	Pin,
	/// A grid of apps.
	Apps,
	/// A battery.
	Battery,
	/// A heart.
	Heart,
	/// A watch.
	Watch,
	/// The Bluetooth rune.
	Bluetooth,
	/// A motion trace.
	Motion,
	/// Two arrows chasing each other.
	Sync,
	/// A bell.
	Bell,
	/// A tick.
	Check,
	/// A left chevron.
	Back,
}

impl Icon {
	/// The icon's paths, in a 24 x 24 box.
	fn path(self) -> &'static str {
		match self {
			Icon::Pulse => "M3 12h4l3-8 4 16 3-8h4",
			Icon::Lock => {
				"M6 11h12a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1v-8a1 1 0 0 1 1-1zM8 11V8a4 \
				 4 0 0 1 8 0v3"
			}
			Icon::Key => "M12 15a4 4 0 1 1-8 0a4 4 0 0 1 8 0zM10.8 12.2L20 3M16 7l3 3M14 9l2 2",
			Icon::Shield => "M12 3l7 3v5c0 4.5-3 8.5-7 10c-4-1.5-7-5.5-7-10V6zM9 12l2 2l4-4",
			Icon::Pin => {
				"M12 21s-7-6.2-7-11a7 7 0 0 1 14 0c0 4.8-7 11-7 11zM12 12.5a2.5 2.5 0 1 0 0-5a2.5 \
				 2.5 0 0 0 0 5z"
			}
			Icon::Apps => {
				"M5 4h4a1 1 0 0 1 1 1v4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM15 4h4a1 1 \
				 0 0 1 1 1v4a1 1 0 0 1-1 1h-4a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM5 14h4a1 1 0 0 1 1 \
				 1v4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-4a1 1 0 0 1 1-1zM15 14h4a1 1 0 0 1 1 1v4a1 1 0 \
				 0 1-1 1h-4a1 1 0 0 1-1-1v-4a1 1 0 0 1 1-1z"
			}
			Icon::Battery => {
				"M4 7h13a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V8a1 1 0 0 1 1-1zM21 \
				 10.5v3M6.5 10v4M9.5 10v4"
			}
			Icon::Heart => "M12 20s-7-4.4-7-10a4 4 0 0 1 7-2.6A4 4 0 0 1 19 10c0 5.6-7 10-7 10z",
			Icon::Watch => {
				"M9 7h6a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2V9a2 2 0 0 1 2-2zM9.5 \
				 7l.5-4h4l.5 4M9.5 17l.5 4h4l.5-4"
			}
			Icon::Bluetooth => "M7 7l10 10-5 4V3l5 4L7 17",
			Icon::Motion => "M4 17l5-5 4 3 7-8M15 7h5v5",
			Icon::Sync => "M20 8a8 8 0 0 0-14.5-2M4 4v4h4M4 16a8 8 0 0 0 14.5 2M20 20v-4h-4",
			Icon::Bell => "M6 16v-5a6 6 0 0 1 12 0v5l2 2H4zM10 20.5a2 2 0 0 0 4 0",
			Icon::Check => "M5 12.5l4.5 4.5L19 7.5",
			Icon::Back => "M15 5l-7 7 7 7",
		}
	}
}

/// One decorative line icon. The text beside it names the row, so the icon
/// is hidden from assistive technology; its color is the `text-[...]` in
/// `class`, such as [`ICON`].
pub fn icon(icon: Icon, class: &'static str) -> Element {
	Vector(ViewBox::new(0.0, 0.0, 24.0, 24.0).expect("a positive size"))
		.class(class)
		.path(
			VectorPath::new(PathData::parse(icon.path()).expect("a valid icon path")).stroke(
				Stroke::new(Paint::CurrentColor, 1.75)
					.expect("a positive width")
					.round(),
			),
		)
		.aria_hidden(true)
}

/// A row's icon in its tile.
pub fn tile(icon: Icon) -> Element {
	View().class(TILE).child(self::icon(icon, ICON))
}

/// A granted row's icon in its green tile.
pub fn tile_ok(icon: Icon) -> Element {
	View().class(TILE_OK).child(self::icon(icon, ICON_OK))
}

/// A row's icon in the tile that asks for attention.
pub fn tile_warn(icon: Icon) -> Element {
	View().class(TILE_WARN).child(self::icon(icon, ICON_WARN))
}

/// A stopped row's icon in its red tile.
pub fn tile_bad(icon: Icon) -> Element {
	View().class(TILE_BAD).child(self::icon(icon, ICON_BAD))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_drawing_parses() {
		for icon in [
			Icon::Pulse,
			Icon::Lock,
			Icon::Key,
			Icon::Shield,
			Icon::Pin,
			Icon::Apps,
			Icon::Battery,
			Icon::Heart,
			Icon::Watch,
			Icon::Bluetooth,
			Icon::Motion,
			Icon::Sync,
			Icon::Bell,
			Icon::Check,
			Icon::Back,
		] {
			assert!(PathData::parse(icon.path()).is_ok(), "{icon:?}");
		}

		assert!(PathData::parse(MARK_ARC).is_ok());
		assert!(PathData::parse(MARK_TAIL).is_ok());

		for letter in WORDMARK {
			match letter {
				Letter::Circle(x, y, radius) => assert!(PathData::circle(x, y, radius).is_ok()),
				Letter::Line(path) => assert!(PathData::parse(path).is_ok(), "{path}"),
			}
		}
	}
}
