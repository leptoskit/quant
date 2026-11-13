//! Unmatched URLs.

use leptoskit::native::Role;
use leptoskit::prelude::*;

use crate::components::Para;
use crate::routes::Navigation;
use crate::routes::Route;

/// Anything the router does not match.
#[component]
pub fn Page(navigation: &Navigation) -> Element {
	view! {
		<View id="lost" role=Role::Region aria_label="Page not found">
			<Text id="lost-note" aria_hidden=true>"a page torn out of the notebook"</Text>
			<Heading aria_level=1 id="lost-title">"Nothing written here."</Heading>
			<Para text="That address is not a page on this site. The way back is short." />
			{navigation
				.link(Route::Home, "Back to the start")
				.id("lost-home")
				.child(view! { <Text>"Back to the start"</Text> })}
		</View>
	}
}
