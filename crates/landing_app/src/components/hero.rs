//! The hero: the headline, the calls to action, and a hand-drawn day page.

use std::time::Duration;

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::FillStyle;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::Shape;
use skribble_leptoskit::Sketch;
use skribble_leptoskit::WiredHighlight;

use crate::components::QUICK_START;
use crate::components::REPOSITORY;
use crate::theme;

/// Rises into place as the page loads. Only the position moves: the copy is
/// readable from the first paint, so the largest text never waits on motion.
fn rise(delay: u64) -> Motion {
	Motion::enter(Pose::new().y(18.0))
		.spring(Spring::GENTLE)
		.delay(Duration::from_millis(delay))
}

/// The hero: what Quant is, in one breath, with the day page beside it.
#[component]
pub fn Hero() -> Element {
	let ink = theme::ink();

	view! {
		<View id="hero" role=Role::Region aria_label="Quant">
			<View id="hero-head">
				<Text id="hero-eyebrow" motion=rise(0)>"a field notebook for your life"</Text>
				<Heading aria_level=1 id="hero-title" motion=rise(60)>
					<Text id="hero-line-1">"Your life, kept in "</Text>
					<WiredHighlight kind=HighlightKind::Marker seed=9 theme=ink.skribble>
						<Text id="hero-line-2">"your own hands."</Text>
					</WiredHighlight>
				</Heading>
			</View>
			<View id="hero-body">
				<View id="hero-lede" role=Role::Paragraph motion=rise(140)>
					<Text>
						"Quant quietly records the shape of your days into an encrypted vault only you can open. Your AI agents ask; you decide what they see."
					</Text>
				</View>
				<View id="hero-actions" motion=rise(220)>
					<Pressable id="hero-primary" accessibility_label="Get Quant" href=QUICK_START>
						<Text>"Get Quant"</Text>
					</Pressable>
					<Pressable
						id="hero-secondary"
						accessibility_label="Read the source"
						href=REPOSITORY
					>
						<Text>"Read the source"</Text>
						<Text aria_hidden=true>" ↗"</Text>
					</Pressable>
				</View>
				<View id="hero-trust" role=Role::List aria_label="Built around your privacy">
					<Text role=Role::ListItem>"Local-first"</Text>
					<Text role=Role::ListItem>"Encrypted vault"</Text>
					<Text role=Role::ListItem>"Open source"</Text>
				</View>
				<Text id="hero-note" motion=rise(300)>
					"Open source, free, and honestly early: the vault, access control, CLI, MCP server and macOS capture work today."
				</Text>
			</View>
			<DayPage />
		</View>
	}
}

/// One annotated note on the day page: an arrow in the second pen, and what
/// it points at, in the notebook's hand.
fn note(index: usize, seed: i64, text: &'static str) -> Element {
	let ink = theme::ink();

	view! {
		<View id=format!("day-note-{index}")>
			<View aria_hidden=true>
				<Sketch
					width=44.0
					height=30.0
					seed=seed
					theme=ink.skribble
					stroke=ink.violet
					shapes=vec![
						Shape::curve([(4.0, 24.0), (16.0, 8.0), (30.0, 12.0), (40.0, 6.0)])
							.with_arrowhead(),
					]
				/>
			</View>
			<Text>{text}</Text>
		</View>
	}
}

/// The day page: a page from the notebook, drawn rather than photographed:
/// a heart-rate line, a night of sleep, and a few annotated notes. Every
/// number on it is an illustration, not a claim.
#[component]
fn DayPage() -> Element {
	let ink = theme::ink();

	view! {
		<View
			id="daypage"
			role=Role::Figure
			aria_label="An illustrated page from the notebook for a single day: a heart-rate line, a night's sleep, and notes about coffee and a call."
		>
			<View id="daypage-tape" aria_hidden=true />
			<View id="daypage-head">
				<Text id="daypage-day">"Tuesday"</Text>
				<Text id="daypage-sub">"the quantified day"</Text>
			</View>
			<View id="daypage-heart" aria_hidden=true>
				<Text id="daypage-heart-label">"heart rate"</Text>
				<Sketch
					width=300.0
					height=64.0
					seed=21
					theme=ink.skribble
					stroke=ink.coral
					shapes=vec![Shape::curve([
						(6.0, 40.0),
						(30.0, 36.0),
						(52.0, 42.0),
						(70.0, 20.0),
						(88.0, 52.0),
						(106.0, 32.0),
						(130.0, 38.0),
						(152.0, 14.0),
						(172.0, 50.0),
						(196.0, 34.0),
						(222.0, 40.0),
						(246.0, 24.0),
						(270.0, 38.0),
						(294.0, 34.0),
					])]
				/>
			</View>
			<View id="daypage-sleep" aria_hidden=true>
				<Text id="daypage-sleep-label">"sleep"</Text>
				<Sketch
					width=300.0
					height=30.0
					seed=22
					theme=ink.skribble
					fill=ink.wash
					fill_style=FillStyle::Solid
					shapes=vec![
						Shape::rounded_rectangle(4.0, 5.0, 292.0, 20.0, 10.0),
						Shape::line((104.0, 5.0), (104.0, 25.0)),
						Shape::line((196.0, 5.0), (196.0, 25.0)),
					]
				/>
			</View>
			<View id="daypage-notes">
				{note(1, 24, "espresso, 13:40")} {note(2, 25, "call with Ada, 10 min saved")}
				{note(3, 26, "focused best, 9 to 12")}
			</View>
			<Text id="daypage-foot" aria_hidden=true>"illustration: drawn, not measured"</Text>
		</View>
	}
}
