//! `/privacy`: the security model in plain language, taken from the docs.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use leptoskit_router::Route;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::WiredDivider;
use skribble_leptoskit::WiredHighlight;

use crate::components::Para;
use crate::theme;

/// Names the route `Privacy`.
#[derive(Route, Clone, Debug, PartialEq)]
pub struct Privacy;

/// The architecture notes this page summarises.
const ARCHITECTURE: &str = "https://github.com/leptoskit/quant/blob/main/docs/architecture.md";

/// The topics of the page: a heading and its paragraphs, each paragraph a
/// plain statement of what the docs say.
const TOPICS: [(&str, &[&str]); 7] = [
	(
		"What Quant records",
		&[
			"On your Mac: the text on your screens, read on the device from screenshots; which \
			 app and window has focus; and counts of keys, clicks and scrolls per minute, never \
			 what you typed. The rolling audio buffer holds the last day when microphone access \
			 is allowed. You can pause recording; live speech transcription is a separate opt-in \
			 source.",
			"Heart rate, sleep, steps and other sensors arrive through devices you pair. Password \
			 managers and detected private browsing windows are excluded from screenshots. Review \
			 your exclusions before relying on them.",
		],
	),
	(
		"Where it lives",
		&[
			"In one encrypted vault on your own machine. The records are a SQLCipher database; \
			 the key is 256 random bits, stored only wrapped under your passphrase with Argon2id, \
			 and in your keychain so the daemon can start at login. Screenshots and audio are \
			 sealed with XChaCha20-Poly1305. Screenshots are kept for 7 days and audio for 24 \
			 hours unless you save them.",
			"There is no automatic cloud sync. Authorized clients receive only the data their \
			 grants permit; through a relay, that data remains encrypted end to end.",
		],
	),
	(
		"Who can read it",
		&[
			"Nobody, by default. An agent that connects sees one tool, which tells it how to ask \
			 you for access. You approve what it gets: scopes such as health:summary (averages, \
			 never a raw sample), a lookback window, limits on requests, records and bytes, and \
			 secrets redacted from anything it reads.",
			"A grant can also deny whole sensitivity classes: health, intimate, location, others \
			 and financial. A denied class is treated exactly like data that is out of scope. \
			 Approving, purging, changing capture and reading the full audit log need your \
			 passphrase or Touch ID. An ordinary client grant does not authorize those owner \
			 actions.",
		],
	),
	(
		"How you can check",
		&[
			"Every request, refusal, owner action and pairing is written to a hash-chained audit \
			 log: who acted, the shape of the request, and how much was served, never the data \
			 itself. Each entry includes the previous entry's hash; quant audit checks those \
			 links for inconsistencies.",
		],
	),
	(
		"Away from home",
		&[
			"A relay lets a phone or an agent on another computer reach your vault. TLS runs end \
			 to end through it, with your Mac's certificate pinned, so the relay sees only a \
			 rendezvous id, IP addresses, times and byte counts. It cannot read the contents or \
			 change them without detection. Anyone can run one, because it never holds a key.",
			"A hosted vault in an attested enclave is designed in the docs. It is not offered, \
			 and nothing here depends on it.",
		],
	),
	(
		"What this website does",
		&[
			"It sets no cookies, runs no analytics and loads nothing from other sites: even the \
			 lettering is served from here. It remembers one thing in your browser, the colour \
			 theme you pick, and nothing else.",
		],
	),
	(
		"Where it stops",
		&[
			"Quant is early software with a single owner by design. It protects your vault from \
			 agents, strangers and a relay. It cannot protect it from someone who has your \
			 account or an unlocked vault process, or someone who has your passphrase. Touch ID \
			 proves you are holding your Mac, not who you are. Do not take this page's word for \
			 any of it: the code and the notes are public.",
		],
	),
];

/// The privacy page.
#[component]
pub fn Page(navigation: &crate::routes::Navigation) -> Element {
	let ink = theme::ink();

	view! {
		<View id="privacy" role=Role::Region aria_label="Privacy">
			<View id="privacy-head">
				<Text id="privacy-note" aria_hidden=true>
					"the security model, written plainly"
				</Text>
				<Heading aria_level=1 id="privacy-title">
					<Text>"Privacy, in "</Text>
					<WiredHighlight kind=HighlightKind::Marker seed=7 theme=ink.skribble>
						<Text>"plain language."</Text>
					</WiredHighlight>
				</Heading>
				<View id="privacy-lede" role=Role::Paragraph>
					<Text>
						"Quant exists so that the most sensitive data you have stays yours. This page says how, and where that stops. Everything on it comes from the project's own notes."
					</Text>
				</View>
			</View>
			<View id="privacy-topics">
				{TOPICS
					.iter()
					.enumerate()
					.map(|(index, (heading, paragraphs))| {
						view! {
							<View id=format!("topic-{index}") role=Role::Region aria_label=*heading>
								<View aria_hidden=true>
									<WiredDivider
										seed=170 + i64::try_from(index).expect("small fixed list")
										theme=ink.skribble
									/>
								</View>
								<Heading aria_level=2>{*heading}</Heading>
								<View id=format!("topic-{index}-body")>
									{paragraphs
										.iter()
										.map(|text| view! { <Para text=*text /> })
										.collect::<Vec<_>>()}
								</View>
							</View>
						}
					})
					.collect::<Vec<_>>()}
			</View>
			<View id="privacy-foot">
				<Pressable
					id="privacy-source"
					accessibility_label="Read the architecture notes"
					href=ARCHITECTURE
				>
					<Text>"Read the architecture notes ↗"</Text>
				</Pressable>
				{navigation
					.link(crate::routes::Route::Home, "Back to the start")
					.id("privacy-home")
					.child(view! { <Text>"Back to the start"</Text> })}
			</View>
		</View>
	}
}
