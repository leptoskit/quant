//! "How it works": capture, vault, and agents, as three numbered steps.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::Shape;
use skribble_leptoskit::Sketch;
use skribble_leptoskit::WiredCircle;

use crate::components::Section;
use crate::theme;

/// One step of the flow: its number, name, and what happens.
struct Step {
	/// The step's number, drawn inside its circle.
	number: &'static str,
	/// The step's name.
	title: &'static str,
	/// What happens in this step.
	body: &'static str,
}

/// The three steps, in the order the data flows.
const STEPS: [Step; 3] = [
	Step {
		number: "1",
		title: "Capture",
		body: "A quiet daemon on your Mac records what is on screen, which apps you use, and what \
		       you hear and say. It pauses when you are idle, skips frames that did not change, \
		       and never records password managers or private browsing windows. Phones and any \
		       sensor that can print numbers feed in through write-only grants.",
	},
	Step {
		number: "2",
		title: "Vault",
		body: "Everything lands in one encrypted vault on your own machine: a SQLCipher database \
		       whose key is wrapped with Argon2id from your passphrase, with media sealed under \
		       XChaCha20-Poly1305. A copy of the key also goes in your keychain, so the daemon \
		       can start at login.",
	},
	Step {
		number: "3",
		title: "Agents",
		body: "AI agents reach the vault through an MCP server or the CLI, never the files. Each \
		       works under a grant you approved: scoped, bounded by a lookback window and rate \
		       limits, redacted, and written to a tamper-evident audit log.",
	},
];

/// The how-it-works section: three steps in drawn circles, joined by
/// hand-drawn arrows that follow the data.
#[component]
pub fn HowItWorks() -> Element {
	view! {
		<Section
			id="how-it-works"
			number="01"
			kicker="the shape of it"
			heading="A day goes in. Only what you allow comes out."
			seed=41
		>
			<View id="steps" role=Role::List>
				{step(0, 31)} <StepArrow index=1 seed=34 /> {step(1, 32)}
				<StepArrow index=2 seed=35 /> {step(2, 33)}
			</View>
		</Section>
	}
}

/// One step: its number in a drawn circle, its name, and what happens.
fn step(index: usize, seed: i64) -> Element {
	let ink = theme::ink();
	let one = &STEPS[index];

	view! {
		<View id=format!("step-{}", index + 1) role=Role::ListItem>
			<View id=format!("step-{}-mark", index + 1)>
				<WiredCircle seed=seed theme=ink.skribble>
					<Text>{one.number}</Text>
				</WiredCircle>
			</View>
			<Heading aria_level=3>{one.title}</Heading>
			<View role=Role::Paragraph>
				<Text>{one.body}</Text>
			</View>
		</View>
	}
}

/// The arrow between two steps: across on wide screens, down on narrow ones.
#[component]
fn StepArrow(index: usize, seed: i64) -> Element {
	let ink = theme::ink();

	view! {
		<View id=format!("step-arrow-{index}") aria_hidden=true>
			<View id=format!("step-arrow-{index}-across")>
				<Sketch
					width=88.0
					height=40.0
					seed=seed
					theme=ink.skribble
					stroke=ink.violet
					shapes=vec![
						Shape::curve([(6.0, 26.0), (30.0, 12.0), (56.0, 28.0), (80.0, 18.0)])
							.with_arrowhead(),
					]
				/>
			</View>
			<View id=format!("step-arrow-{index}-down")>
				<Sketch
					width=40.0
					height=64.0
					seed=seed + 50
					theme=ink.skribble
					stroke=ink.violet
					shapes=vec![
						Shape::curve([(18.0, 6.0), (28.0, 22.0), (12.0, 40.0), (20.0, 56.0)])
							.with_arrowhead(),
					]
				/>
			</View>
		</View>
	}
}
