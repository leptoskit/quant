//! "Reach it from anywhere, safely": the blind relay, drawn as a diagram.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::Shape;
use skribble_leptoskit::Sketch;
use skribble_leptoskit::WiredBox;

use crate::components::Para;
use crate::components::Section;
use crate::theme;

/// What the relay learns and what it never does, from `docs/hosting.md`.
const KNOWS: [&str; 3] = [
	"a rendezvous id, which is a hash of a key your vault derives",
	"the IP addresses that connect, and when",
	"how many bytes flow",
];

/// What stays out of its reach.
const NEVER: [&str; 4] = [
	"a request or a response",
	"a key",
	"a pairing code",
	"a way to alter traffic undetected: a changed TLS record fails",
];

/// The relay section: a phone, the relay, and your Mac, with the lock end to
/// end and the relay seeing only noise.
#[component]
pub fn Relay() -> Element {
	view! {
		<Section
			id="relay"
			number="04"
			kicker="away from home"
			heading="Reach it from anywhere, safely."
			seed=91
		>
			<View id="relay-intro">
				<Para text="Your vault stays on your Mac. Off your home network, a phone, a sensor bridge or an MCP server on another computer reaches it through a relay: your Mac keeps one outbound connection open, the relay joins the two ends and copies bytes. TLS runs end to end inside that pipe, from the client to your Mac, with the certificate pinned from the pairing link." />
			</View>
			<Diagram />
			<View id="relay-facts">
				<View id="relay-knows" role=Role::Group aria_label="What the relay knows">
					<Heading aria_level=3>"What the relay sees"</Heading>
					<View role=Role::List>
						{KNOWS
							.iter()
							.map(|line| {
								view! {
									<View role=Role::ListItem>
										<Text>{*line}</Text>
									</View>
								}
							})
							.collect::<Vec<_>>()}
					</View>
				</View>
				<View id="relay-never" role=Role::Group aria_label="What the relay never sees">
					<Heading aria_level=3>"What it never has"</Heading>
					<View role=Role::List>
						{NEVER
							.iter()
							.map(|line| {
								view! {
									<View role=Role::ListItem>
										<Text>{*line}</Text>
									</View>
								}
							})
							.collect::<Vec<_>>()}
					</View>
				</View>
			</View>
			<View id="relay-outro">
				<Para text="A relay can drop or delay traffic. It cannot read the contents or change them without detection, and a fake daemon fails the pinned fingerprint. Because it never holds a key, anyone can run one. A hosted vault in an attested enclave is designed in the docs, but it is not offered." />
			</View>
		</Section>
	}
}

/// The diagram: three drawn nodes along a wire, with the end-to-end lock
/// spanning all of them.
#[component]
fn Diagram() -> Element {
	let ink = theme::ink();

	view! {
		<View
			id="relay-diagram"
			role=Role::Figure
			aria_label="A phone connects to a relay, and the relay connects to your Mac. An encrypted channel runs from the phone to the Mac, straight through the relay, which sees only noise."
		>
			<View id="relay-nodes">
				<View id="node-phone">
					<View aria_hidden=true>
						<Sketch
							width=64.0
							height=96.0
							seed=92
							theme=ink.skribble
							shapes=vec![
								Shape::rounded_rectangle(6.0, 4.0, 52.0, 88.0, 10.0),
								Shape::line((24.0, 14.0), (40.0, 14.0)),
								Shape::circle(32.0, 80.0, 6.0),
							]
						/>
					</View>
					<Text>"your phone"</Text>
				</View>
				<Wire index=1 seed=93 />
				<View id="node-relay">
					<WiredBox seed=94 theme=ink.skribble>
						<View aria_hidden=true>
							<Sketch
								width=96.0
								height=34.0
								seed=95
								theme=ink.skribble
								stroke=ink.coral
								shapes=vec![Shape::curve([
									(4.0, 18.0),
									(12.0, 6.0),
									(20.0, 28.0),
									(28.0, 8.0),
									(36.0, 26.0),
									(44.0, 10.0),
									(52.0, 28.0),
									(60.0, 6.0),
									(68.0, 24.0),
									(76.0, 10.0),
									(84.0, 22.0),
									(92.0, 16.0),
								])]
							/>
						</View>
						<Text>"the relay"</Text>
					</WiredBox>
					<Text id="node-relay-note">"sees only noise"</Text>
				</View>
				<Wire index=2 seed=96 />
				<View id="node-mac">
					<View aria_hidden=true>
						<Sketch
							width=104.0
							height=84.0
							seed=97
							theme=ink.skribble
							shapes=vec![
								Shape::rounded_rectangle(10.0, 6.0, 84.0, 54.0, 6.0),
								Shape::line((2.0, 70.0), (102.0, 70.0)),
								Shape::line((2.0, 70.0), (12.0, 62.0)),
								Shape::line((102.0, 70.0), (92.0, 62.0)),
							]
						/>
					</View>
					<Text>"your Mac, with the vault"</Text>
				</View>
			</View>
			<View id="relay-tunnel" aria_hidden=true>
				<View id="relay-tunnel-line" />
				<View id="relay-tunnel-lock">
					<Sketch
						width=40.0
						height=46.0
						seed=98
						theme=ink.skribble
						stroke=ink.violet
						fill=ink.violet
						fill_style=skribble_leptoskit::FillStyle::Solid
						shapes=vec![Shape::rounded_rectangle(6.0, 20.0, 28.0, 22.0, 4.0)]
					/>
					<Sketch
						class="absolute left-0 top-0"
						width=40.0
						height=46.0
						seed=99
						theme=ink.skribble
						stroke=ink.violet
						shapes=vec![Shape::curve([
							(12.0, 22.0),
							(11.0, 12.0),
							(20.0, 5.0),
							(29.0, 12.0),
							(28.0, 22.0),
						])]
					/>
				</View>
				<Text id="relay-tunnel-label">"end-to-end TLS, from the phone to your Mac"</Text>
			</View>
		</View>
	}
}

/// A two-headed arrow between nodes: across on wide screens, down on narrow
/// ones.
#[component]
fn Wire(index: usize, seed: i64) -> Element {
	let ink = theme::ink();

	view! {
		<View id=format!("wire-{index}") aria_hidden=true>
			<View id=format!("wire-{index}-across")>
				<Sketch
					width=84.0
					height=36.0
					seed=seed
					theme=ink.skribble
					shapes=vec![
						Shape::curve([(6.0, 10.0), (30.0, 6.0), (54.0, 12.0), (78.0, 9.0)])
							.with_arrowhead(),
						Shape::curve([(78.0, 28.0), (54.0, 31.0), (30.0, 25.0), (6.0, 28.0)])
							.with_arrowhead(),
					]
				/>
			</View>
			<View id=format!("wire-{index}-down")>
				<Sketch
					width=40.0
					height=60.0
					seed=seed + 40
					theme=ink.skribble
					shapes=vec![
						Shape::curve([(10.0, 6.0), (13.0, 20.0), (8.0, 36.0), (11.0, 54.0)])
							.with_arrowhead(),
						Shape::curve([(29.0, 54.0), (32.0, 38.0), (27.0, 22.0), (30.0, 6.0)])
							.with_arrowhead(),
					]
				/>
			</View>
		</View>
	}
}
