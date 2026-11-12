//! `/`: the landing page.

use leptoskit::prelude::*;
use leptoskit_router::Route;

use crate::components::Agents;
use crate::components::Closing;
use crate::components::Faq;
use crate::components::Hero;
use crate::components::HowItWorks;
use crate::components::Memory;
use crate::components::Platforms;
use crate::components::Relay;

/// Names the route `Home`.
#[derive(Route, Clone, Debug, PartialEq)]
pub struct Home;

/// The landing page: the hero, then each section in the notebook's order.
#[component]
pub fn Page() -> Element {
	view! {
		<View id="home">
			<Hero />
			<HowItWorks />
			<Agents />
			<Memory />
			<Relay />
			<Platforms />
			<Faq />
			<Closing />
		</View>
	}
}
