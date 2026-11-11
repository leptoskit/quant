//! The last word before the footer: how to start.

use leptoskit::native::Role;
use leptoskit::prelude::*;

use crate::components::QUICK_START;
use crate::components::REPOSITORY;

/// A closing call to action: install, choose a passphrase, and the notebook
/// starts being kept.
#[component]
pub fn Closing() -> Element {
	view! {
		<View id="closing" role=Role::Region aria_label="Get started">
			<Text id="closing-note" aria_hidden=true>"turn the page"</Text>
			<Heading aria_level=2 id="closing-title">"Start with your Mac."</Heading>
			<View id="closing-copy" role=Role::Paragraph>
				<Text>
					"Install the CLI, choose a passphrase, and the daemon starts keeping the notebook. Connect an agent when you are ready, and approve only what it needs."
				</Text>
			</View>
			<View id="closing-actions">
				<Pressable id="closing-primary" accessibility_label="Get Quant" href=QUICK_START>
					<Text>"Get Quant"</Text>
				</Pressable>
				<Pressable
					id="closing-secondary"
					accessibility_label="Read the source"
					href=REPOSITORY
				>
					<Text>"Read the source"</Text>
					<Text aria_hidden=true>" ↗"</Text>
				</Pressable>
			</View>
		</View>
	}
}
