//! Keeps the keyboard where it was when a control is drawn again.

/// Puts focus back on the element with `id` once the page has settled.
///
/// A control whose accessible state changes (a menu button, a question) is
/// drawn again under its new state, and the browser drops focus with the old
/// node. Calling this from the press handler returns it to the new one, so a
/// keyboard visitor stays where they were.
#[cfg(all(target_arch = "wasm32", feature = "web"))]
pub fn restore_focus(id: String) {
	use leptos::prelude::document;
	use leptos::prelude::request_animation_frame;
	use wasm_bindgen::JsCast;

	let _ = request_animation_frame(move || {
		// A visitor may already have moved to another control. Restore focus
		// only when the old node lost it, never steal their new focus.
		if document().active_element().is_some_and(|active| {
			active.tag_name() != "BODY" && active.tag_name() != "HTML" && active.id() != id
		}) {
			return;
		}
		if let Some(element) = document().get_element_by_id(&id)
			&& let Ok(element) = element.dyn_into::<leptos::web_sys::HtmlElement>()
		{
			let _ = element.focus();
		}
	});
}

/// On the server there is no focus to restore.
#[cfg(not(all(target_arch = "wasm32", feature = "web")))]
pub fn restore_focus(_id: String) {}

/// Keeps a controlled field's focus and caret through a region redraw.
#[cfg(all(target_arch = "wasm32", feature = "web"))]
pub fn with_input_focus(container: &'static str, update: impl FnOnce()) {
	use leptos::prelude::document;
	use leptos::prelude::queue_microtask;
	use wasm_bindgen::JsCast;

	let field = move || {
		document()
			.get_element_by_id(container)
			.and_then(|parent| parent.query_selector("input").ok().flatten())
			.and_then(|element| element.dyn_into::<leptos::web_sys::HtmlInputElement>().ok())
	};
	let selection = field()
		.and_then(|input| {
			let active = document().active_element()?;
			if !input.is_same_node(Some(&active)) {
				return None;
			}
			Some((input, active))
		})
		.map(|(input, active)| {
			(
				input.selection_start().ok().flatten(),
				input.selection_end().ok().flatten(),
				active,
			)
		});
	update();
	if let Some((start, end, previous)) = selection {
		queue_microtask(move || {
			let Some(input) = field() else { return };
			if document().active_element().is_some_and(|active| {
				active.tag_name() != "BODY"
					&& active.tag_name() != "HTML"
					&& active != previous
					&& !input.is_same_node(Some(&active))
			}) {
				return;
			}
			let _ = input.focus();
			if let (Some(start), Some(end)) = (start, end) {
				let _ = input.set_selection_range(start, end);
			}
		});
	}
}

/// Native/SSR construction has no DOM focus to preserve.
#[cfg(not(all(target_arch = "wasm32", feature = "web")))]
pub fn with_input_focus(_container: &'static str, update: impl FnOnce()) {
	update();
}
