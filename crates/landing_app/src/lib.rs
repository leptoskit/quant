//! The Quant landing page: the screens the SSR server and the browser share.
//!
//! Routes live in `routes/` (the file router writes `routes.rs`), the drawn
//! interface in `components/`, and the page's state in one Leptos store.

pub mod components;
// The file router emits compatibility conversions redundant with this SDK.
#[allow(missing_docs, clippy::useless_conversion)]
pub mod routes;
pub mod store;
pub mod theme;

pub use landing_shared::Health;
use leptoskit::prelude::Element;
use leptoskit_router::Routable;

/// The URL path the server is rendering, handed to [`App`] as context so each
/// address gets its own page in the first HTML, before any script runs.
#[derive(Clone, Debug)]
pub struct InitialPath(pub String);

/// Whether `path` is a page of this site, as opposed to the not-found page.
#[must_use]
pub fn is_page(path: &str) -> bool {
	matches!(routes::Route::parse(path), Ok(route) if !matches!(route, routes::Route::NotFound(_)))
}

/// Renders the portable application tree for any host.
pub fn screen(route: routes::Route) -> Element {
	reactive_graph::owner::provide_context(store::Store::default());
	routes::Navigation::new(route).render()
}

/// The router a fresh render starts from: the browser's address, or on the
/// server the path the request asked for.
#[cfg(feature = "web")]
fn start() -> routes::Navigation {
	#[cfg(target_arch = "wasm32")]
	{
		routes::Navigation::from_browser()
			.unwrap_or_else(|_| routes::Navigation::new(routes::Route::Home))
	}
	#[cfg(not(target_arch = "wasm32"))]
	{
		let path = leptos::prelude::use_context::<InitialPath>()
			.map_or_else(|| "/".to_owned(), |path| path.0);
		routes::Navigation::new(routes::Route::parse(&path).unwrap_or(routes::Route::Home))
	}
}

/// The browser and server adapter for the same application tree.
///
/// Palette changes rebuild the hand-drawn ink with its new colours. The
/// same store holds the menu, FAQ, and memory demo state across redraws.
#[cfg(feature = "web")]
#[leptos::component]
pub fn App() -> impl leptos::prelude::IntoView {
	use leptos::prelude::*;

	let navigation = start();
	let store = store::Store::default();
	provide_context(store.clone());
	let appearance = store.appearance();

	#[cfg(target_arch = "wasm32")]
	{
		let applied = store.clone();
		Effect::new(move |previous: Option<()>| {
			if previous.is_none() {
				applied.restore_appearance();
			}
			applied.apply_to_document();
		});
	}

	leptoskit::web::screen(move || {
		appearance.get();
		navigation.render()
	})
}

/// The complete HTML document sent by the SSR server.
#[cfg(feature = "web")]
pub fn shell(options: leptos::config::LeptosOptions) -> impl leptos::prelude::IntoView {
	use leptos::prelude::*;

	leptos::view! {
		<!DOCTYPE html>
		<html lang="en">
			<head>
				<meta charset="utf-8" />
				<meta
					name="viewport"
					content="width=device-width, initial-scale=1, viewport-fit=cover"
				/>
				<meta name="color-scheme" content="light dark" />
				<meta name="theme-color" content="#fbf6ea" media="(prefers-color-scheme: light)" />
				<meta name="theme-color" content="#1b1a17" media="(prefers-color-scheme: dark)" />
				<meta
					name="description"
					content="Quant quietly records the shape of your days into an encrypted vault only you can open. Your AI agents ask; you decide what they see."
				/>
				<link rel="icon" href="/assets/favicon.svg" type="image/svg+xml" />
				<link
					rel="preload"
					href="/assets/fonts/SkribbleNotoSketch-Regular.woff2"
					r#as="font"
					r#type="font/woff2"
					crossorigin="anonymous"
				/>
				<title>"Quant: quantify your life, and decide who reads it"</title>
				<HydrationScripts options />
				<style>{include_str!("../../../assets/landing/app.css")}</style>
			</head>
			<body>
				<App />
			</body>
		</html>
	}
}

/// Hydrates server markup when the document's hydration script calls it.
#[cfg(all(target_arch = "wasm32", feature = "hydrate"))]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
	#[cfg(feature = "devtools")]
	leptoskit::devtools::install(env!("CARGO_PKG_NAME"));
	leptos::mount::hydrate_body(App);
	if let Some(root) = leptos::prelude::document().document_element() {
		let _ = root.set_attribute("data-hydrated", "true");
	}
}

/// Mounts the CSR application when the browser loads its Wasm module.
#[cfg(all(target_arch = "wasm32", feature = "web", not(feature = "hydrate")))]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start_csr() {
	#[cfg(feature = "devtools")]
	leptoskit::devtools::install(env!("CARGO_PKG_NAME"));
	leptos::mount::mount_to_body(App);
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
	use leptos::prelude::*;

	use super::*;

	#[test]
	fn routes_render_the_requested_page() {
		assert!(is_page("/"));
		assert!(is_page("/privacy"));
		assert!(!is_page("/not-a-real-page"));
		let owner = Owner::new();
		owner.with(|| {
			provide_context(InitialPath("/privacy".into()));
			let privacy = leptos::view! { <App /> }.to_html();
			assert!(privacy.contains("What Quant records"));
			assert!(privacy.contains("Where it lives"));
			assert!(!privacy.contains("id=\"hero-title\""));
		});
	}
}
