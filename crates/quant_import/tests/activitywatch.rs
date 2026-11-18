//! `ActivityWatch` imports against a live daemon: an in-process REST mock of
//! aw-server, an exported JSON file, and the mappings they produce.

use std::sync::Arc;

use axum::Json;
use axum::routing::get;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::Record;
use quant_core::TimeRange;
use quant_crypto::KdfParams;
use quant_daemon::State;
use quant_import::ActivityWatchOptions;
use quant_import::Importer;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Vault;
use quant_store::VaultPaths;
use serde_json::Value;
use serde_json::json;

const PASS: &str = "correct horse battery staple";

fn at(second: i64) -> Timestamp {
	"2026-11-17T10:00:00Z".parse::<Timestamp>().unwrap() + SignedDuration::from_secs(second)
}

#[allow(clippy::needless_pass_by_value)] // json! consumes the value.
fn event(timestamp: &str, duration: f64, data: Value) -> Value {
	json!({ "timestamp": timestamp, "duration": duration, "data": data })
}

/// The buckets an aw-server for two hosts would show, and their events.
fn buckets() -> Vec<(&'static str, &'static str, &'static str, Vec<Value>)> {
	vec![
		(
			"aw-watcher-window_mini",
			"currentwindow",
			"mini",
			vec![
				event(
					"2026-11-17T10:00:00Z",
					10.0,
					json!({ "app": "Zed", "title": "main.rs" }),
				),
				event(
					"2026-11-17T10:00:10Z",
					5.0,
					json!({ "app": "Zed", "title": "main.rs" }),
				),
				event(
					"2026-11-17T10:00:15Z",
					5.0,
					json!({ "app": "Firefox", "title": "Private Browsing" }),
				),
			],
		),
		(
			"aw-watcher-afk_mini",
			"afkstatus",
			"mini",
			vec![
				event(
					"2026-11-17T10:00:00Z",
					300.0,
					json!({ "status": "not-afk" }),
				),
				event("2026-11-17T10:05:00Z", 600.0, json!({ "status": "afk" })),
			],
		),
		(
			"aw-watcher-web-firefox_mini",
			"web.tab.current",
			"mini",
			vec![
				event(
					"2026-11-17T10:00:15Z",
					5.0,
					json!({ "url": "https://example.com/news", "title": "News - Example" }),
				),
				event(
					"2026-11-17T10:00:20Z",
					7.0,
					json!({ "url": "https://example.com/news", "title": "News - Example" }),
				),
			],
		),
		(
			"aw-watcher-window_studio",
			"currentwindow",
			"studio",
			vec![event(
				"2026-11-17T10:00:00Z",
				60.0,
				json!({ "app": "Terminal", "title": "zsh" }),
			)],
		),
	]
}

/// Serve the aw-server REST API in-process and return its URL.
async fn serve_rest() -> String {
	let known = buckets();
	let app = axum::Router::new()
		.route(
			"/api/0/buckets",
			get(move || {
				let known = known.clone();
				async move {
					Json(json!({
						"buckets": known.iter().map(|(id, kind, host, _)| json!({
							"id": id,
							"type": kind,
							"hostname": host,
						})).collect::<Vec<_>>(),
					}))
				}
			}),
		)
		.route(
			"/api/0/buckets/{id}/events",
			get(|axum::extract::Path(id): axum::extract::Path<String>| {
				async move {
					let events = buckets()
						.into_iter()
						.find(|(bucket, ..)| *bucket == id)
						.map(|(_, _, _, events)| events)
						.unwrap_or_default();
					Json(Value::Array(events))
				}
			}),
		);
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move {
		let _ = axum::serve(listener, app).await;
	});
	url
}

/// An `ActivityWatch` export file holding the same history.
fn export_file(dir: &std::path::Path) -> std::path::PathBuf {
	let path = dir.join("aw-export.json");
	let mut map = serde_json::Map::new();
	for (id, kind, host, events) in buckets() {
		map.insert(
			id.to_owned(),
			json!({ "type": kind, "hostname": host, "events": events }),
		);
	}
	std::fs::write(&path, json!({ "buckets": Value::Object(map) }).to_string()).unwrap();
	path
}

/// A running daemon with a paired, approved importer.
struct Harness {
	state: Arc<State>,
	importer: Importer,
}

async fn start(dir: &std::path::Path) -> Harness {
	let paths = VaultPaths::new(dir.join("v"));
	let state = Arc::new(
		State::new(Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap()).unwrap(),
	);
	let socket = paths.socket();
	let serving = Arc::clone(&state);
	let path = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &path, std::future::pending())
			.await
			.unwrap();
	});
	while !socket.exists() {
		tokio::time::sleep(std::time::Duration::from_millis(5)).await;
	}
	let endpoint = Endpoint::Unix(socket);

	let mut client = Client::new(endpoint.clone(), Identity::generate("importer"));
	let pairing = client.pair(ClientKind::Device, &[]).await.unwrap();
	OwnerSession::open(endpoint, PASS)
		.await
		.unwrap()
		.approve(&ApproveRequest {
			client: pairing.pairing_code,
			scopes: Some(vec!["apps:write".into(), "device:write".into()]),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();
	let importer = Importer::new(client, &dir.join("state")).await.unwrap();
	Harness { state, importer }
}

fn records(harness: &Harness, stream: &str) -> Vec<Record> {
	harness
		.state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream(stream.parse().unwrap()),
			TimeRange::new(at(-60), at(600)).unwrap(),
		))
		.unwrap()
}

fn rest_options(url: &str) -> ActivityWatchOptions {
	ActivityWatchOptions {
		url: Some(url.to_owned()),
		file: None,
		since: None,
		until: None,
		follow: false,
		every: quant_import::DEFAULT_EVERY,
		dry_run: false,
	}
}

/// The one span an app produced.
fn by_app<'a>(spans: &'a [Record], name: &str) -> &'a Record {
	spans
		.iter()
		.find(|span| span.data["app"].as_str() == Some(name))
		.expect("the span exists")
}

/// The one device-state span a state produced.
fn by_state<'a>(states: &'a [Record], state: &str) -> &'a Record {
	states
		.iter()
		.find(|record| record.data["state"].as_str() == Some(state))
		.expect("the state span exists")
}

fn file_options(path: &std::path::Path) -> ActivityWatchOptions {
	ActivityWatchOptions {
		url: None,
		file: Some(path.to_owned()),
		..rest_options("http://localhost:5600")
	}
}

#[tokio::test]
async fn rest_import_maps_windows_afk_and_web_pages() {
	let dir = tempfile::tempdir().unwrap();
	let url = serve_rest().await;
	let mut harness = start(dir.path()).await;
	let report = harness
		.importer
		.activitywatch(rest_options(&url))
		.await
		.unwrap();

	// The window watcher's Zed events merge into one span; its Firefox event
	// is skipped because the web watcher covers that browser, so the page
	// span (with the URL) replaces it. The other host keeps its own device.
	let spans = records(&harness, "apps.focus");
	assert_eq!(spans.len(), 3, "{spans:?}");
	let zed = by_app(&spans, "Zed");
	assert_eq!(zed.data["title"].as_str(), Some("main.rs"));
	assert_eq!((zed.start, zed.end), (at(0), Some(at(15))));

	let page = by_app(&spans, "Firefox");
	assert_eq!(page.data["url"].as_str(), Some("https://example.com/news"));
	assert_eq!((page.start, page.end), (at(15), Some(at(27))));

	// Events from another host keep their hostname as the device label, so
	// they land on a different device than this Mac's.
	let terminal = by_app(&spans, "Terminal");
	assert_eq!(terminal.data["title"].as_str(), Some("zsh"));
	assert_ne!(terminal.device, zed.device);
	assert_eq!((terminal.start, terminal.end), (at(0), Some(at(60))));

	for span in &spans {
		assert_eq!(span.data["source"].as_str(), Some("activitywatch"));
	}

	// The afk watcher becomes idle and active device state spans.
	let states = records(&harness, "device.state");
	assert_eq!(states.len(), 2, "{states:?}");
	let active = by_state(&states, "active");
	assert_eq!((active.start, active.end), (at(0), Some(at(300))));
	let idle = by_state(&states, "idle");
	assert_eq!((idle.start, idle.end), (at(300), Some(at(900))));
	for state in &states {
		assert_eq!(state.data["source"].as_str(), Some("activitywatch"));
	}

	assert_eq!(report.stored, 5);
}

#[tokio::test]
async fn an_exported_file_imports_offline() {
	let dir = tempfile::tempdir().unwrap();
	let file = export_file(dir.path());
	let mut harness = start(dir.path()).await;
	let report = harness
		.importer
		.activitywatch(file_options(&file))
		.await
		.unwrap();

	// The same history as the REST server: two focus spans on the window
	// watcher (the Firefox one is covered by the web watcher), one page
	// span, one span from the other host, and the two afk spans.
	let spans = records(&harness, "apps.focus");
	assert_eq!(spans.len(), 3, "{spans:?}");
	assert!(
		spans
			.iter()
			.any(|span| span.data["url"].as_str() == Some("https://example.com/news"))
	);
	let states = records(&harness, "device.state");
	assert_eq!(states.len(), 2, "{states:?}");
	assert_eq!(report.stored, 5);
}

#[tokio::test]
async fn re_imports_add_nothing_and_watermarks_only_pull_new_data() {
	let dir = tempfile::tempdir().unwrap();
	let url = serve_rest().await;
	let mut harness = start(dir.path()).await;
	let first = harness
		.importer
		.activitywatch(rest_options(&url))
		.await
		.unwrap();
	assert_eq!(first.stored, 5);

	// The per-bucket watermarks mean a second run pulls nothing.
	let second = harness
		.importer
		.activitywatch(rest_options(&url))
		.await
		.unwrap();
	assert_eq!(second.stored, 0);
	assert_eq!(second.duplicate, 0);
	assert!(second.counts.is_empty());

	// An explicit --since reprocesses everything, deriving the same ids, so
	// all five records come back as duplicates.
	let mut replay = rest_options(&url);
	replay.since = Some(at(-60));
	let third = harness.importer.activitywatch(replay).await.unwrap();
	assert_eq!(third.stored, 0, "a replay must not add records");
	assert_eq!(third.duplicate, 5);

	// A bounded window pulls only the events inside it, and the span it
	// re-derives carries the id the first import already stored.
	let mut window = rest_options(&url);
	window.since = Some(at(15));
	window.until = Some(at(20));
	let fourth = harness.importer.activitywatch(window).await.unwrap();
	let mut expected = std::collections::BTreeMap::new();
	expected.insert("apps.focus".to_owned(), 1);
	assert_eq!(fourth.counts, expected);
	assert_eq!(fourth.stored, 0);
	assert_eq!(fourth.duplicate, 1);
	assert_eq!(
		records(&harness, "apps.focus").len(),
		3,
		"the window import must not duplicate spans"
	);
}

#[tokio::test]
async fn dry_runs_write_nothing() {
	let dir = tempfile::tempdir().unwrap();
	let file = export_file(dir.path());
	let mut harness = start(dir.path()).await;
	let mut dry = file_options(&file);
	dry.dry_run = true;
	let report = harness.importer.activitywatch(dry).await.unwrap();

	assert_eq!(report.stored, 0);
	assert_eq!(report.duplicate, 0);
	assert_eq!(report.counts.values().sum::<usize>(), 5);
	for stream in ["apps.focus", "device.state"] {
		assert!(records(&harness, stream).is_empty(), "{stream} was written");
	}

	// No watermark was kept, so the real run imports everything.
	let real = harness
		.importer
		.activitywatch(file_options(&file))
		.await
		.unwrap();
	assert_eq!(real.stored, 5);
}

#[tokio::test]
async fn a_private_window_from_the_web_watcher_is_skipped() {
	let dir = tempfile::tempdir().unwrap();
	let mut map = serde_json::Map::new();
	map.insert(
		"aw-watcher-web-firefox_mini".into(),
		json!({
			"type": "web.tab.current",
			"hostname": "mini",
			"events": [
				event(
					"2026-11-17T10:00:00Z",
					30.0,
					json!({ "url": "https://example.com/a", "title": "Normal page" }),
				),
				event(
					"2026-11-17T10:00:30Z",
					30.0,
					json!({ "url": "https://example.com/b", "title": "Mozilla Firefox — Private Browsing" }),
				),
			],
		}),
	);
	let file = dir.path().join("private.json");
	std::fs::write(&file, json!({ "buckets": Value::Object(map) }).to_string()).unwrap();
	let mut harness = start(dir.path()).await;
	harness
		.importer
		.activitywatch(file_options(&file))
		.await
		.unwrap();

	let spans = records(&harness, "apps.focus");
	assert_eq!(spans.len(), 1, "{spans:?}");
	assert_eq!(spans[0].data["url"].as_str(), Some("https://example.com/a"));
}
