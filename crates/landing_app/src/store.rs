//! The landing page's state, in a Leptos store.
//!
//! [`Store::current`] hands any screen a cheap clone of the same handle.
//! Each field has its own signal. The app also reads the appearance so the
//! hand-drawn ink is rebuilt when the palette changes.

#[cfg(all(target_arch = "wasm32", feature = "web"))]
use leptoskit::prelude::Get;
use leptoskit::prelude::Set;
use leptoskit::prelude::Update;
use leptoskit::prelude::With;
use leptoskit::store::ArcField;
use leptoskit::store::ArcStore;

/// Which palette the page wears.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
	/// Follow the system preference: warm paper or chalk on charcoal, live.
	#[default]
	System,
	/// Warm paper and dark ink, whatever the system says.
	Light,
	/// Chalk on charcoal, whatever the system says.
	Dark,
}

/// How many answers the FAQ section holds; [`State::faq_open`] tracks one
/// flag per answer.
pub const FAQ_COUNT: usize = 5;

/// Everything the screens show beyond their static copy.
#[derive(Clone, Debug, PartialEq, reactive_stores::Store)]
pub struct State {
	/// The palette the page wears.
	pub appearance: Appearance,
	/// Whether the small-screen navigation menu is unfolded.
	pub nav_open: bool,
	/// Which FAQ answers are unfolded, one flag per answer.
	pub faq_open: Vec<bool>,
	/// The memory section's correction, once the visitor writes one: the
	/// held claim it replaces is struck through and this correction appears
	/// above it, ranked as the owner's own word.
	pub claim_correction: Option<String>,
	/// What the memory section's correction field holds while it is typed.
	pub claim_draft: String,
	/// Whether the memory section's held claim has been approved as it
	/// stands.
	pub claim_approved: bool,
}

impl Default for State {
	fn default() -> Self {
		Self {
			appearance: Appearance::default(),
			nav_open: false,
			faq_open: vec![false; FAQ_COUNT],
			claim_correction: None,
			claim_draft: String::new(),
			claim_approved: false,
		}
	}
}

/// A handle on the state. It is cheap to clone, and every clone is the same
/// state.
#[derive(Clone, Default)]
pub struct Store {
	state: ArcStore<State>,
}

impl Store {
	/// The app's store.
	#[must_use]
	pub fn current() -> Self {
		reactive_graph::owner::use_context::<Self>()
			.expect("the landing App provides a store for this render")
	}

	/// The palette the page wears.
	pub fn appearance(&self) -> ArcField<Appearance> {
		self.state.clone().appearance().into()
	}

	/// Whether the small-screen navigation menu is unfolded.
	pub fn nav_open(&self) -> ArcField<bool> {
		self.state.clone().nav_open().into()
	}

	/// Whether the FAQ answer at `index` is unfolded.
	#[must_use]
	pub fn faq(&self, index: usize) -> bool {
		self.state
			.clone()
			.faq_open()
			.with(|open| open.get(index).copied().unwrap_or(false))
	}

	/// Moves to the next palette: system, light, dark, system.
	pub fn cycle_appearance(&self) {
		self.appearance().update(|appearance| {
			*appearance = match *appearance {
				Appearance::System => Appearance::Light,
				Appearance::Light => Appearance::Dark,
				Appearance::Dark => Appearance::System,
			};
		});
	}

	/// Folds or unfolds the small-screen navigation menu.
	pub fn toggle_nav(&self) {
		self.nav_open().update(|open| *open = !*open);
	}

	/// Folds or unfolds the FAQ answer at `index`.
	pub fn toggle_faq(&self, index: usize) {
		self.state.clone().faq_open().update(|open| {
			if let Some(flag) = open.get_mut(index) {
				*flag = !*flag;
			}
		});
	}

	/// The correction field's text so far.
	#[must_use]
	pub fn claim_draft(&self) -> String {
		self.state.clone().claim_draft().with(Clone::clone)
	}

	/// The visitor's saved correction to the held claim, if there is one.
	#[must_use]
	pub fn claim_correction(&self) -> Option<String> {
		self.state.clone().claim_correction().with(Clone::clone)
	}

	/// Whether the held claim has been approved as it stands.
	#[must_use]
	pub fn claim_approved(&self) -> bool {
		self.state
			.clone()
			.claim_approved()
			.with(|approved| *approved)
	}

	/// Records what the correction field holds as it is typed.
	pub fn set_claim_draft(&self, text: String) {
		self.state.clone().claim_draft().set(text);
	}

	/// Saves the typed correction, striking the held claim it replaces.
	pub fn correct_claim(&self) {
		let draft = self.claim_draft();
		if draft.trim().is_empty() {
			return;
		}
		self.state.clone().claim_correction().set(Some(draft));
	}

	/// Approves the held claim as it stands.
	pub fn approve_claim(&self) {
		self.state.clone().claim_approved().set(true);
	}
}

/// The key the chosen palette is remembered under in the browser.
#[cfg(all(target_arch = "wasm32", feature = "web"))]
const STORAGE_KEY: &str = "quant-landing-appearance";

#[cfg(all(target_arch = "wasm32", feature = "web"))]
impl Store {
	/// Reads the palette the visitor chose last time, once, after the page
	/// has hydrated so the first paint always matches the server's.
	pub fn restore_appearance(&self) {
		use leptos::prelude::window;

		let saved = window()
			.local_storage()
			.ok()
			.flatten()
			.and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten());
		match saved.as_deref() {
			Some("light") => self.appearance().set(Appearance::Light),
			Some("dark") => self.appearance().set(Appearance::Dark),
			_ => {}
		}
	}

	/// Writes the palette onto the document, where the page's stylesheet
	/// reads it, and remembers it. Reads the appearance, so an effect that
	/// calls this runs again whenever the palette changes.
	pub fn apply_to_document(&self) {
		use leptos::prelude::document;
		use leptos::prelude::window;

		let appearance = self.appearance().get();
		let value = match appearance {
			Appearance::System => None,
			Appearance::Light => Some("light"),
			Appearance::Dark => Some("dark"),
		};
		if let Some(root) = document().document_element() {
			match value {
				Some(value) => {
					let _ = root.set_attribute("data-theme", value);
				}
				None => {
					let _ = root.remove_attribute("data-theme");
				}
			}
		}
		if let Some(storage) = window().local_storage().ok().flatten() {
			let _ = match value {
				Some(value) => storage.set_item(STORAGE_KEY, value),
				None => storage.remove_item(STORAGE_KEY),
			};
		}
	}
}

#[cfg(test)]
mod tests {
	use reactive_graph::owner::Owner;

	use super::*;

	#[test]
	fn server_render_owners_do_not_share_visitor_state() {
		let first = Owner::new();
		let second = Owner::new();
		first.with(|| {
			reactive_graph::owner::provide_context(Store::default());
			Store::current().set_claim_draft("only the first visitor".into());
			Store::current().correct_claim();
			Store::current().toggle_faq(0);
		});
		second.with(|| {
			reactive_graph::owner::provide_context(Store::default());
			assert_eq!(Store::current().claim_draft(), "");
			assert!(Store::current().claim_correction().is_none());
			assert!(!Store::current().faq(0));
		});
		first.with(|| {
			assert_eq!(
				Store::current().claim_correction().as_deref(),
				Some("only the first visitor")
			);
		});
	}
}
