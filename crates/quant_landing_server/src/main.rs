//! Web SSR entrypoint. Application UI and backend handlers live in their
//! crates.
//!
//! Everything the server is built from comes through `leptoskit::server`:
//! `axum`, `tokio`, and `leptos_axum` are re-exported there, so this package
//! depends on `leptoskit` alone.

use landing_app::App;
use landing_app::InitialPath;
use landing_app::shell;
use leptoskit::leptos::prelude::provide_context;
use leptoskit::server::GracefulShutdown;
use leptoskit::server::Readiness;
use leptoskit::server::axum::body::Body;
use leptoskit::server::axum::http::Request;
use leptoskit::server::axum::http::StatusCode;
use leptoskit::server::leptos_axum::LeptosRoutes;
use leptoskit::server::leptos_axum::generate_route_list;
use leptoskit::server::leptos_axum::render_app_to_stream_with_context;
use leptoskit::server::tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	// `leptoskit rpc spec` and `leptoskit rpc generate` run this binary with
	// `LEPTOSKIT_RPC_SPEC_OUT` set to read the published API: this writes it
	// there and exits, before anything below reads configuration or binds a
	// port. Otherwise it checks that no two functions share a route.
	leptoskit::rpc::spec::export_if_requested();

	let mut options = leptoskit::leptos::config::get_configuration(None)?.leptos_options;
	options.output_name = "landing_app".into();

	if std::env::var_os("LEPTOS_SITE_ROOT").is_none() {
		options.site_root = "dist/landing".into();
	}

	let address = options.site_addr;
	let package_dir = format!("{}/{}", options.site_root, options.site_pkg_dir);
	let assets_dir = format!("{}/assets", options.site_root);
	// `/readyz` answers 200 once every check passes, so a load balancer only
	// sends traffic to an instance that can serve it. Add a check with
	// `readiness.check("name")` for work that has to finish first.
	let readiness = Readiness::new();
	let routes = generate_route_list(App);
	let app_options = options.clone();
	let page_options = options.clone();
	// The built files are served with their precompressed brotli and gzip
	// copies when the browser accepts them.
	let app = landing_backend::router()
		.nest_service(
			"/pkg",
			leptoskit::server::static_files(package_dir.as_ref()),
		)
		.nest_service(
			"/assets",
			leptoskit::server::static_files(assets_dir.as_ref()),
		)
		.leptos_routes(&options, routes, move || shell(app_options.clone()))
		// Every other address, rendered for its own route so `/privacy` and the
		// not-found page arrive complete, the not-found one with a 404.
		.fallback(move |request: Request<Body>| {
			let options = page_options.clone();
			async move {
				let path = request.uri().path().to_owned();
				let missing = !landing_app::is_page(&path);
				let initial = path.clone();
				let mut response = render_app_to_stream_with_context(
					move || provide_context(InitialPath(initial.clone())),
					move || shell(options.clone()),
				)(request)
				.await;
				if missing {
					*response.status_mut() = StatusCode::NOT_FOUND;
				}
				response
			}
		})
		.with_state(options)
		.merge(readiness.routes());
	let listener = tokio::net::TcpListener::bind(address).await?;

	println!("listening on http://{address}");
	// Serves until Ctrl-C or the SIGTERM a container runtime sends. Then
	// `/readyz` fails, and after a few seconds for the load balancer to
	// notice, the server stops accepting and finishes the requests in flight.
	let shutdown = GracefulShutdown::new().readiness(&readiness);
	leptoskit::server::serve_listener_with(listener, app, shutdown).await?;

	Ok(())
}
