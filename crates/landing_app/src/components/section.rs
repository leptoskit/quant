//! The page's layout helpers: a numbered section and its paragraphs.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::WiredDivider;

use crate::theme;

/// One section of the page: a named region with a margin note (its number
/// and kicker, in the notebook's hand), a heading, and its content. The
/// stylesheet gives every section the same rhythm, addressed through the
/// `-head` and `-body` ids this builds from `id`.
#[component]
pub fn Section(
	/// The anchor the navigation links to, such as `how-it-works`.
	#[prop(into)]
	id: String,
	/// The section's number in the notebook, such as `01`.
	#[prop(into)]
	number: String,
	/// The short margin note beside the heading.
	#[prop(into)]
	kicker: String,
	/// The section's heading, which also names the region.
	#[prop(into)]
	heading: String,
	/// Chooses the wobble of the rule above the section.
	#[prop(default = 1)]
	seed: i64,
	/// The section's content.
	children: Children,
) -> Element {
	let ink = theme::ink();

	view! {
		<View id=id.clone() role=Role::Region aria_label=heading.clone()>
			<View id=format!("{id}-rule") aria_hidden=true>
				<WiredDivider seed=seed theme=ink.skribble />
			</View>
			<View id=format!("{id}-head")>
				<View id=format!("{id}-note") aria_hidden=true>
					<Text id=format!("{id}-number")>{number}</Text>
					<Text id=format!("{id}-kicker")>{kicker}</Text>
				</View>
				<Heading aria_level=2 id=format!("{id}-title")>{heading}</Heading>
			</View>
			<View id=format!("{id}-body")>{children}</View>
		</View>
	}
}

/// A paragraph in the page's body copy: a real paragraph element, on a
/// measure near sixty-five characters and sized inside the 17–19px band by
/// the stylesheet.
#[component]
pub fn Para(
	/// The paragraph's text.
	#[prop(into)]
	text: String,
) -> Element {
	view! {
		<View role=Role::Paragraph>
			<Text>{text}</Text>
		</View>
	}
}
