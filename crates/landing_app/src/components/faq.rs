//! Principles and questions, with the answers folded open by the store.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::Shape;
use skribble_leptoskit::Sketch;
use skribble_leptoskit::WiredHighlight;

use crate::components::Section;
use crate::components::restore_focus;
use crate::store::Store;
use crate::theme;

/// The principles Quant holds itself to, each a short promise.
const PRINCIPLES: [(&str, &str); 4] = [
	(
		"Open source",
		"All of it is in one public repository, released into the public domain under the \
		 Unlicense.",
	),
	(
		"No server key",
		"Your vault key is wrapped under your passphrase, with a copy in your keychain. No server \
		 holds it.",
	),
	(
		"Yours to delete",
		"The vault is a directory on your machine, and you can read it all back. Purging deletes \
		 what you choose, and what was derived from it.",
	),
	(
		"Local first",
		"Capture, the vault, search and learning all run on your machine. The network is only \
		 what you turn on.",
	),
];

/// A question and its answer. The answer is exactly what the docs say.
const QUESTIONS: [(&str, &str); 5] = [
	(
		"Is Quant open source?",
		"Yes. The vault, the daemon, the CLI, the MCP server, the relay and this site are all in \
		 one repository, released under the Unlicense: public domain, no strings.",
	),
	(
		"Does any server ever hold my key?",
		"No. The vault key is 256 random bits, stored only wrapped: under your passphrase with \
		 Argon2id, and in your OS keychain so the daemon can start at login. The relay only joins \
		 two encrypted connections and never holds a key.",
	),
	(
		"Can I get my data out, and delete it?",
		"Nothing is held anywhere but your vault. You can read it back through the CLI (quant \
		 search, quant series, quant overview), and save audio as an .m4a with quant rewind \
		 export. quant purge deletes records by stream and time range, with their derived \
		 records, claims and embeddings, and secure_delete overwrites the pages. Purging needs \
		 your passphrase.",
	),
	(
		"What if an agent has a shell on my machine?",
		"It still cannot approve itself. Approving a grant, purging data, changing or pausing \
		 capture, and reading the full audit log need an owner session: your passphrase, or Touch \
		 ID on a device you enrolled.",
	),
	(
		"Does it work without the internet?",
		"Yes. Capture, the vault and search run on your Mac. The only network paths are the ones \
		 you switch on: a link for your phone on the local network, and an optional relay. \
		 Learning models are only ever reached on loopback addresses, so vault text cannot leave \
		 through a changed DNS name or a proxy.",
	),
];

/// The principles and FAQ section.
#[component]
pub fn Faq() -> Element {
	let ink = theme::ink();

	view! {
		<Section
			id="principles"
			number="06"
			kicker="the small print, kept short"
			heading="Principles, and the questions people ask."
			seed=131
		>
			<View id="principles-list" role=Role::List>
				{PRINCIPLES
					.iter()
					.enumerate()
					.map(|(index, (name, body))| {
						view! {
							<View id=format!("principle-{index}") role=Role::ListItem>
								<Heading aria_level=3>
									<WiredHighlight
										kind=HighlightKind::Underline
										seed=132 + i64::try_from(index).expect("small fixed list")
										theme=ink.skribble
										color=ink.violet
									>
										<Text>{*name}</Text>
									</WiredHighlight>
								</Heading>
								<View role=Role::Paragraph>
									<Text>{*body}</Text>
								</View>
							</View>
						}
					})
					.collect::<Vec<_>>()}
			</View>
			<View id="faq" role=Role::List aria_label="Questions">
				{QUESTIONS
					.iter()
					.enumerate()
					.map(|(index, (question, answer))| item(index, question, answer))
					.collect::<Vec<_>>()}
			</View>
		</Section>
	}
}

/// One question: a button that folds its answer open and shut. The button is
/// drawn again under each state, because its `aria-expanded` is part of the
/// element; focus is put back on it after the swap.
fn item(index: usize, question: &'static str, answer: &'static str) -> Element {
	let store = Store::current();
	let closed = store.clone();
	let opened = store.clone();
	let body = store;

	view! {
		<View id=format!("faq-{index}") role=Role::ListItem>
			{Show(
				move || !closed.faq(index),
				move || question_button(index, question, false),
			)}
			{Show(
				move || opened.faq(index),
				move || question_button(index, question, true),
			)}
			{Show(
				move || body.faq(index),
				move || {
					view! {
						<View
							id=format!("faq-{index}-answer")
							role=Role::Region
							aria_label=question
						>
							<Text>{answer}</Text>
						</View>
					}
				},
			)}
		</View>
	}
}

/// The button of one question, in one state, with its drawn plus or minus.
fn question_button(index: usize, question: &'static str, open: bool) -> Element {
	let store = Store::current();
	let ink = theme::ink();
	let id = format!("faq-{index}-button");
	let focus = id.clone();
	let shapes = if open {
		vec![Shape::line((5.0, 13.0), (21.0, 13.0))]
	} else {
		vec![
			Shape::line((13.0, 5.0), (13.0, 21.0)),
			Shape::line((5.0, 13.0), (21.0, 13.0)),
		]
	};

	view! {
		<Button
			id=id
			accessibility_label=question
			aria_expanded=open
			on:press=move || {
				store.toggle_faq(index);
				restore_focus(focus.clone());
			}
		>
			<Text id=format!("faq-{index}-question")>{question}</Text>
			<View id=format!("faq-{index}-marker") aria_hidden=true>
				<Sketch
					width=26.0
					height=26.0
					seed=140 + i64::try_from(index).expect("small fixed list")
					theme=ink.skribble
					shapes=shapes
				/>
			</View>
		</Button>
	}
}
