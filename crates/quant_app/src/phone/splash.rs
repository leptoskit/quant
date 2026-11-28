//! The launch animation: the mark the cold-start screen shows, brought to
//! life.
//!
//! The overlay is drawn on the app's first frame, so the system splash and the
//! first frame show the same mark in the same place and the hand-off is
//! seamless. The mark is already fully drawn; then the dots sample round the
//! Q, the tail turns like a dial needle, the ring breathes out, and the
//! wordmark rises beneath it.

use leptoskit::native::Transform;
use leptoskit::native::animation::Keyframe;
use leptoskit::native::animation::Keyframes;
use leptoskit::native::animation::Presence;
use leptoskit::native::animation::Spring;
use leptoskit::prelude::Element;
use leptoskit::prelude::Text;
use leptoskit::prelude::View;

use crate::theme;

/// The full-screen splash, over the screen until
/// [`crate::phone::State::splash`] clears.
pub fn splash() -> Element {
	View()
		.class("absolute inset-0 items-center justify-center bg-[#0E0C24]")
		.test_id("splash")
		.exiting(Presence::fade().scale(1.04).duration(360.0))
		.child(
			View()
				.class("relative w-[100px] h-[100px]")
				.child(theme::mark_halo().keyframes(ring()))
				.child(theme::mark_arc())
				.children(
					[150.0, 240.0, 330.0, 420.0, 510.0]
						.into_iter()
						.enumerate()
						.map(|(index, delay)| theme::mark_dot(index).keyframes(dot(delay))),
				)
				.child(theme::mark_tail().keyframes(tail())),
		)
		.child(
			View()
				.class("absolute bottom-0 left-0 right-0 items-center gap-3 pb-safe-offset-12")
				.child(
					theme::wordmark("w-[150px] h-[53px]").entering(
						Presence::fade()
							.slide_by(0.0, 14.0)
							.spring(Spring::GENTLE)
							.delay(450.0),
					),
				)
				.child(
					Text("Your life, measured. Yours alone.")
						.class(theme::MUTED)
						.entering(
							Presence::fade()
								.slide_by(0.0, 14.0)
								.spring(Spring::GENTLE)
								.delay(600.0),
						),
				),
		)
}

/// Each dot pops outward in turn: a sampling sweep round the Q.
fn dot(delay: f32) -> Keyframes {
	Keyframes::new(
		420.0,
		[
			Keyframe::at(0.0).transform(Transform::scale(1.0)),
			Keyframe::at(0.45).transform(Transform::scale(1.18)),
			Keyframe::at(1.0).transform(Transform::scale(1.0)),
		],
	)
	.delay(delay)
}

/// The tail nudges like a dial needle after the sweep.
fn tail() -> Keyframes {
	Keyframes::new(
		620.0,
		[
			Keyframe::at(0.0).transform(Transform::rotate(0.0)),
			Keyframe::at(0.35).transform(Transform::rotate(-16.0)),
			Keyframe::at(0.7).transform(Transform::rotate(5.0)),
			Keyframe::at(1.0).transform(Transform::rotate(0.0)),
		],
	)
	.delay(650.0)
}

/// One soft ring breathes out from the mark.
fn ring() -> Keyframes {
	Keyframes::new(
		1100.0,
		[
			Keyframe::at(0.0)
				.opacity(0.6)
				.transform(Transform::scale(1.0)),
			Keyframe::at(1.0)
				.opacity(0.0)
				.transform(Transform::scale(1.55)),
		],
	)
	.delay(900.0)
}
