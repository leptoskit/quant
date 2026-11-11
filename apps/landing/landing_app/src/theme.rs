//! The page's palette: skribble's warm paper and chalk, with the Riemann
//! mark's violet and coral as a second and third pen.
//!
//! The page's own surfaces take their colours from CSS variables in
//! `assets/app.css`, which follow the system preference or the `data-theme`
//! the store writes on the document. The hand-drawn ink is painted by
//! skribble at build time, so it takes the palette from here: [`current`]
//! resolves the appearance in the store, and `System` keeps skribble's own
//! live `prefers-color-scheme` switching while `Light` and `Dark` pin both of
//! the theme's appearances to one palette.

use leptoskit::native::Color;
use leptoskit::prelude::Get;
use skribble_leptoskit::Palette;
use skribble_leptoskit::Theme;
use skribble_leptoskit::Tone;

use crate::store::Appearance;
use crate::store::Store;

/// What the drawn ink is painted with, in one appearance.
pub struct Ink {
	/// The theme the skribble components draw with.
	pub skribble: Theme,
	/// The Riemann mark's violet: the page's "second pen".
	pub violet: Tone,
	/// The Riemann mark's coral, for what is denied or corrected.
	pub coral: Tone,
	/// A translucent violet, for a wash of colour inside a drawing.
	pub wash: Tone,
}

/// Violet, as the mark has it on paper and on charcoal.
const VIOLET: Tone = Tone {
	light: Color::rgba(0x5b, 0x4c, 0xf5, 0xff),
	dark: Color::rgba(0x8f, 0x85, 0xff, 0xff),
};

/// Coral, as the mark has it on paper and on charcoal.
const CORAL: Tone = Tone {
	light: Color::rgba(0xe0, 0x4f, 0x2c, 0xff),
	dark: Color::rgba(0xff, 0x8a, 0x6b, 0xff),
};

/// Violet at a third of its strength, for fills.
const WASH: Tone = Tone {
	light: Color::rgba(0x5b, 0x4c, 0xf5, 0x4d),
	dark: Color::rgba(0x8f, 0x85, 0xff, 0x59),
};

/// The pen: a little finer and steadier than skribble's default, so the ink
/// reads as confident rather than loud.
const PEN: Theme = Theme {
	stroke_width: 1.7,
	roughness: 0.9,
	..Theme::SKRIBBLE
};

/// The system's own preference, followed live by every drawing.
static SYSTEM: Ink = Ink {
	skribble: PEN,
	violet: VIOLET,
	coral: CORAL,
	wash: WASH,
};

/// Warm paper, forced.
static LIGHT: Ink = Ink {
	skribble: Theme {
		dark: Palette::LIGHT,
		..PEN
	},
	violet: Tone::fixed(VIOLET.light),
	coral: Tone::fixed(CORAL.light),
	wash: Tone::fixed(WASH.light),
};

/// Chalk on charcoal, forced.
static DARK: Ink = Ink {
	skribble: Theme {
		light: Palette::DARK,
		..PEN
	},
	violet: Tone::fixed(VIOLET.dark),
	coral: Tone::fixed(CORAL.dark),
	wash: Tone::fixed(WASH.dark),
};

/// The ink the store's appearance picks.
#[must_use]
pub fn for_appearance(appearance: Appearance) -> &'static Ink {
	match appearance {
		Appearance::System => &SYSTEM,
		Appearance::Light => &LIGHT,
		Appearance::Dark => &DARK,
	}
}

/// The ink the page is drawn with right now.
#[must_use]
pub fn ink() -> &'static Ink {
	for_appearance(Store::current().appearance().get())
}

/// The label the palette toggle shows.
#[must_use]
pub fn label() -> &'static str {
	match Store::current().appearance().get() {
		Appearance::System => "Auto",
		Appearance::Light => "Paper",
		Appearance::Dark => "Chalk",
	}
}

/// The name the palette toggle carries for assistive technology.
#[must_use]
pub fn name() -> &'static str {
	match Store::current().appearance().get() {
		Appearance::System => "Colour theme: following your system. Press to use paper.",
		Appearance::Light => "Colour theme: paper. Press to use chalk.",
		Appearance::Dark => "Colour theme: chalk. Press to follow your system.",
	}
}
