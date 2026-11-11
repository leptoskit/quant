//! The root layout: the navigation and the footer around every screen.

use leptoskit::native::Role;
use leptoskit::prelude::*;

use crate::components::Footer;
use crate::components::Nav;
use crate::routes::Navigation;

/// Wraps the matched route in the page's frame.
#[component]
pub fn Layout(navigation: &Navigation, children: Children) -> Element {
	view! {
		<View id="site">
			<Nav navigation=navigation />
			<View id="main" role=Role::Main>{children}</View>
			<Footer navigation=navigation />
		</View>
	}
}
