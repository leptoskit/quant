//! The chrome around every page: the navigation bar and the footer.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::WiredHighlight;

use crate::components::Lockup;
use crate::components::restore_focus;
use crate::routes::Navigation;
use crate::routes::Route;
use crate::store::Store;
use crate::theme;

/// The repository: the source, the issues, and the releases.
pub const REPOSITORY: &str = "https://github.com/leptoskit/quant";

/// Where "Get Quant" points: the quick start, which installs the CLI and the
/// daemon.
pub const QUICK_START: &str = "https://github.com/leptoskit/quant#quick-start-macos";

/// The bar at the top of every page: the lockup, the section links, the
/// palette toggle, and GitHub. Under 860px the links fold into a menu.
#[component]
pub fn Nav(navigation: &Navigation) -> Element {
	let store = Store::current();
	let toggle_theme = store.clone();
	let ink = theme::ink();

	view! {
		<View id="nav" role=Role::Banner>
			<View id="nav-bar">
				{home_link(navigation, "nav-home")}
				<View id="nav-links" role=Role::Navigation aria_label="Primary">
					{section_link("How it works", "/#how-it-works")} {privacy_link(navigation)}
					{source_link("Open source")}
				</View>
				<View id="nav-tools">
					<Button
						id="theme-toggle"
						accessibility_label=theme::name()
						on:press=move || {
							toggle_theme.cycle_appearance();
							restore_focus("theme-toggle".to_owned());
						}
					>
						{theme::label()}
					</Button>
					<Pressable
						id="nav-github"
						accessibility_label="Quant on GitHub"
						href=REPOSITORY
					>
						<WiredHighlight kind=HighlightKind::Box seed=5 theme=ink.skribble>
							<Text>"GitHub"</Text>
						</WiredHighlight>
					</Pressable>
					{Show(
						{
							let store = store.clone();
							move || !store.nav_open().get()
						},
						{
							let store = store.clone();
							move || menu_button(&store, false)
						},
					)}
					{Show(
						{
							let store = store.clone();
							move || store.nav_open().get()
						},
						{
							let store = store.clone();
							move || menu_button(&store, true)
						},
					)}
				</View>
			</View>
			{Show(
				{
					let store = store.clone();
					move || store.nav_open().get()
				},
				{
					let navigation = navigation.clone();
					move || {
						view! {
							<View id="nav-menu" role=Role::Navigation aria_label="Sections">
								{section_link("How it works", "/#how-it-works")}
								{privacy_link(&navigation)} {source_link("Open source")}
								{source_link("GitHub")}
							</View>
						}
					}
				},
			)}
		</View>
	}
}

/// The menu button in one state. It is drawn again when the menu opens or
/// closes, because its `aria-expanded` is part of the element, and focus is
/// put back on it.
fn menu_button(store: &Store, open: bool) -> Element {
	let store = store.clone();

	view! {
		<Button
			id="menu-toggle"
			accessibility_label="Menu"
			aria_expanded=open
			on:press=move || {
				store.toggle_nav();
				restore_focus("menu-toggle".to_owned());
			}
		>
			{if open { "Close" } else { "Menu" }}
		</Button>
	}
}

/// The lockup, as a link back to the landing page.
fn home_link(navigation: &Navigation, id: &str) -> Element {
	navigation
		.link(Route::Home, "Quant home")
		.id(id)
		.child(view! { <Lockup size=34.0 seed=2 id=format!("{id}-lockup") /> })
}

/// An in-page link to a section.
fn section_link(label: &str, href: &str) -> Element {
	view! {
		<Pressable accessibility_label=label href=href>
			<Text>{label}</Text>
		</Pressable>
	}
}

/// The link to the privacy page, which stays in the app.
fn privacy_link(navigation: &Navigation) -> Element {
	navigation
		.link(Route::Privacy, "Privacy")
		.child(view! { <Text>"Privacy"</Text> })
}

/// A link to the repository.
fn source_link(label: &str) -> Element {
	view! {
		<Pressable accessibility_label=label href=REPOSITORY>
			<Text>{label}</Text>
		</Pressable>
	}
}

/// The footer: the lockup, the licence, and the same few links.
#[component]
pub fn Footer(navigation: &Navigation) -> Element {
	view! {
		<View id="footer" role=Role::ContentInfo>
			<View id="footer-inner">
				<View id="footer-brand">
					{home_link(navigation, "footer-home")}
					<Text id="footer-tagline">
						"Quantify your life, and decide exactly which AI may read it."
					</Text>
				</View>
				<View id="footer-links" role=Role::Navigation aria_label="Footer">
					{privacy_link(navigation)} {source_link("GitHub")}
					{section_link("How it works", "/#how-it-works")}
				</View>
				<Text id="footer-licence">
					"Released under the Unlicense: public domain, no strings. Early days: the vault, access control, daemon, CLI, MCP server and macOS capture are tested end to end."
				</Text>
			</View>
		</View>
	}
}
