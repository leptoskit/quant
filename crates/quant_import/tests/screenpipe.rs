//! Screenpipe imports against a live daemon: a real vault, a real socket,
//! a fixture Screenpipe database in two schema variants.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

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
use quant_import::Importer;
use quant_import::ScreenpipeOptions;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Vault;
use quant_store::VaultPaths;
use rusqlite::Connection;

const PASS: &str = "correct horse battery staple";
/// Frames sit at second 1000 and on, comfortably inside every range below.
const BASE: i64 = 1_700_000_000;

/// Which Screenpipe schema to build.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
	/// Every column: `browser_url`, `focused`, OCR text, audio.
	Full,
	/// An older database: no `browser_url`, no `focused`, no audio table,
	/// and timestamps stored in milliseconds.
	Legacy,
}

fn at(second: i64) -> Timestamp {
	Timestamp::from_second(BASE + second).unwrap()
}

/// A small Screenpipe database.
fn build_db(dir: &Path, variant: Variant) -> PathBuf {
	let path = dir.join(match variant {
		Variant::Full => "full.sqlite",
		Variant::Legacy => "legacy.sqlite",
	});
	let conn = Connection::open(&path).unwrap();
	match variant {
		Variant::Full => {
			conn.execute_batch(
				"CREATE TABLE frames (
					id INTEGER PRIMARY KEY,
					timestamp INTEGER,
					app_name TEXT,
					window_name TEXT,
					browser_url TEXT,
					focused INTEGER
				);
				CREATE TABLE ocr_text (frame_id INTEGER, text TEXT, app_name TEXT, window_name TEXT);
				CREATE TABLE audio_transcriptions (
					id INTEGER PRIMARY KEY,
					transcription TEXT,
					timestamp INTEGER,
					device TEXT,
					is_input_device INTEGER,
					speaker_id INTEGER
				);",
			)
			.unwrap();
			let frames: [(i64, &str, &str, Option<&str>, i64); 8] = [
				(1000, "Zed", "main.rs", None, 1),
				(1002, "Zed", "main.rs", None, 1),
				(1004, "Zed", "main.rs", None, 1),
				(1010, "Zed", "main.rs", None, 1),
				(1012, "Zed", "lib.rs", None, 1),
				(1014, "Safari", "News", Some("https://example.com/news"), 1),
				(1016, "Safari", "News", Some("https://example.com/news"), 0),
				(1018, "1Password", "Vault", None, 1),
			];
			for (id, (offset, app, window, url, focused)) in frames.iter().enumerate() {
				let id = i64::try_from(id).unwrap() + 1;
				conn.execute(
					"INSERT INTO frames (id, timestamp, app_name, window_name, browser_url, \
					 focused) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
					rusqlite::params![id, BASE + offset, app, window, url, focused],
				)
				.unwrap();
			}
			let ocr: [(i64, &str); 4] = [
				(1, "let x = 1;"),
				(2, "let x = 1;"),
				(3, "let y = 2;"),
				(6, "headline text"),
			];
			for (frame, text) in ocr {
				conn.execute(
					"INSERT INTO ocr_text (frame_id, text, app_name, window_name) VALUES (?1, ?2, \
					 'Zed', 'main.rs')",
					rusqlite::params![frame, text],
				)
				.unwrap();
			}
			conn.execute(
				"INSERT INTO audio_transcriptions (transcription, timestamp, device, \
				 is_input_device, speaker_id) VALUES ('the quick brown fox', ?1, 'MacBook Pro \
				 Microphone', 1, 2)",
				[BASE + 2000],
			)
			.unwrap();
		}
		Variant::Legacy => {
			conn.execute_batch(
				"CREATE TABLE frames (
					id INTEGER PRIMARY KEY,
					timestamp INTEGER,
					app_name TEXT,
					window_name TEXT
				);
				CREATE TABLE ocr_text (frame_id INTEGER, text TEXT);",
			)
			.unwrap();
			// Millisecond timestamps, two seconds apart.
			let base_ms = (BASE + 4000) * 1000;
			for (id, ts) in [(1, base_ms), (2, base_ms + 2_000), (3, base_ms + 4_000)] {
				conn.execute(
					"INSERT INTO frames (id, timestamp, app_name, window_name) VALUES (?1, ?2, \
					 'Safari', 'News')",
					rusqlite::params![id, ts],
				)
				.unwrap();
			}
			for (frame, text) in [(1, "a page"), (2, "a page"), (3, "another page")] {
				conn.execute(
					"INSERT INTO ocr_text (frame_id, text) VALUES (?1, ?2)",
					rusqlite::params![frame, text],
				)
				.unwrap();
			}
		}
	}
	drop(conn);
	path
}

/// A running daemon with a paired, approved importer. The vault's `TempDir`
/// outlives this in the test that created it.
struct Harness {
	state: Arc<State>,
	importer: Importer,
	state_dir: PathBuf,
}

async fn start(dir: &tempfile::TempDir, scopes: &[&str]) -> Harness {
	let paths = VaultPaths::new(dir.path().join("v"));
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
	let pairing = client.pair(ClientKind::Device, &[], None).await.unwrap();
	OwnerSession::open(endpoint, PASS)
		.await
		.unwrap()
		.approve(&ApproveRequest {
			client: pairing.pairing_code,
			scopes: Some(
				scopes
					.iter()
					.map(|scope| format!("{scope}:write"))
					.collect(),
			),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();
	let state_dir = dir.path().join("state");
	let importer = Importer::new(client, &state_dir).await.unwrap();
	Harness {
		state,
		importer,
		state_dir,
	}
}

fn records(harness: &Harness, stream: &str) -> Vec<Record> {
	harness
		.state
		.vault
		.store
		.records(&RecordQuery::new(
			Selector::stream(stream.parse().unwrap()),
			TimeRange::new(
				at(0) - SignedDuration::from_hours(1),
				at(9_000) + SignedDuration::from_hours(1),
			)
			.unwrap(),
		))
		.unwrap()
}

fn options(db: &Path) -> ScreenpipeOptions {
	ScreenpipeOptions {
		db: Some(db.to_owned()),
		since: None,
		until: None,
		follow: false,
		every: quant_import::DEFAULT_EVERY,
		dry_run: false,
	}
}

/// The spans one app produced.
fn app<'a>(spans: &'a [Record], name: &str) -> Vec<&'a Record> {
	spans
		.iter()
		.filter(|span| span.data["app"].as_str() == Some(name))
		.collect()
}

#[tokio::test]
async fn full_schema_maps_spans_ocr_and_audio() {
	let dir = tempfile::tempdir().unwrap();
	let db = build_db(dir.path(), Variant::Full);
	let mut harness = start(&dir, &["apps", "screen", "audio"]).await;
	let report = harness.importer.screenpipe(options(&db)).await.unwrap();

	// Five spans: two for Zed's main.rs (a gap and a window change split
	// them), one for lib.rs, one for Safari, and the password manager, whose
	// title is dropped but whose app is kept.
	let spans = records(&harness, "apps.focus");
	assert_eq!(spans.len(), 5, "{spans:?}");
	// The 6-second gap between frames 3 and 4 exceeds twice the 2-second
	// median interval, so it ends the span at the last frame plus the tail.
	let mut zed = app(&spans, "Zed");
	zed.sort_by_key(|span| span.start.as_millisecond());
	assert_eq!(zed.len(), 3);
	assert_eq!((zed[0].start, zed[0].end), (at(1000), Some(at(1006))));
	assert_eq!((zed[1].start, zed[1].end), (at(1010), Some(at(1012))));
	assert_eq!((zed[2].start, zed[2].end), (at(1012), Some(at(1014))));

	let safari = app(&spans, "Safari");
	assert_eq!(safari.len(), 1);
	assert_eq!(safari[0].end, Some(at(1016)));
	assert_eq!(
		safari[0].data.get("url").and_then(|u| u.as_str()),
		Some("https://example.com/news")
	);

	// The password manager keeps its app and loses its title and URL.
	let manager = app(&spans, "1Password");
	assert_eq!(manager.len(), 1);
	assert_eq!(manager[0].data.get("title"), None);
	assert!(manager[0].data.get("url").is_none());

	for span in &spans {
		assert_eq!(span.data["source"].as_str(), Some("screenpipe"));
	}

	// Three screen frames: the duplicate OCR text is skipped, the unfocused
	// frame has none, and the password manager's frame is excluded.
	let mut frames = records(&harness, "screen.frame");
	frames.sort_by_key(|frame| frame.start.as_millisecond());
	assert_eq!(frames.len(), 3, "{frames:?}");
	let texts: Vec<&str> = frames.iter().filter_map(|f| f.text.as_deref()).collect();
	assert_eq!(texts, ["let x = 1;", "let y = 2;", "headline text"]);
	assert_eq!(
		frames[2].data.get("url").and_then(|u| u.as_str()),
		Some("https://example.com/news")
	);

	// One transcription, tagged with its source.
	let audio = records(&harness, "audio.transcript");
	assert_eq!(audio.len(), 1);
	assert_eq!(audio[0].text.as_deref(), Some("the quick brown fox"));
	assert_eq!(audio[0].data["source"].as_str(), Some("screenpipe"));
	assert_eq!(audio[0].data["direction"].as_str(), Some("input"));

	assert_eq!(report.stored, 5 + 3 + 1);
	assert_eq!(report.duplicate, 0);
}

#[tokio::test]
async fn legacy_schema_without_url_or_focus_still_imports() {
	let dir = tempfile::tempdir().unwrap();
	let db = build_db(dir.path(), Variant::Legacy);
	let mut harness = start(&dir, &["apps", "screen"]).await;
	harness.importer.screenpipe(options(&db)).await.unwrap();

	// Millisecond timestamps were detected; one span over all three frames.
	let spans = records(&harness, "apps.focus");
	assert_eq!(spans.len(), 1, "{spans:?}");
	assert_eq!(spans[0].start, at(4000));
	assert_eq!(spans[0].end, Some(at(4006)));

	// The repeated text is deduplicated; there is no url on the frames.
	let mut frames = records(&harness, "screen.frame");
	frames.sort_by_key(|frame| frame.start.as_millisecond());
	let texts: Vec<&str> = frames.iter().filter_map(|f| f.text.as_deref()).collect();
	assert_eq!(texts, ["a page", "another page"]);
	assert!(frames.iter().all(|f| f.data.get("url").is_none()));
}

#[tokio::test]
async fn re_imports_add_nothing_and_watermarks_only_pull_new_data() {
	let dir = tempfile::tempdir().unwrap();
	let db = build_db(dir.path(), Variant::Full);
	let mut harness = start(&dir, &["apps", "screen", "audio"]).await;
	let first = harness.importer.screenpipe(options(&db)).await.unwrap();
	assert_eq!(first.stored, 9);

	// The watermark means a second run finds nothing new at all.
	let second = harness.importer.screenpipe(options(&db)).await.unwrap();
	assert_eq!(second.stored, 0);
	assert_eq!(second.duplicate, 0);
	assert!(second.counts.is_empty());

	// Re-processing the whole window from an explicit --since derives the
	// same ids, so everything comes back as a duplicate.
	let mut replay = options(&db);
	replay.since = Some(at(900));
	let third = harness.importer.screenpipe(replay).await.unwrap();
	assert_eq!(third.stored, 0, "a replay must not add records");
	assert_eq!(third.duplicate, 9);

	// New frames after the watermark are the only thing a follow-up pulls.
	{
		let conn = Connection::open(&db).unwrap();
		conn.execute(
			"INSERT INTO frames (id, timestamp, app_name, window_name, focused) VALUES (9, ?1, \
			 'Terminal', 'zsh', 1)",
			[BASE + 1030],
		)
		.unwrap();
	}
	let fourth = harness.importer.screenpipe(options(&db)).await.unwrap();
	assert_eq!(fourth.stored, 1);
	let spans = records(&harness, "apps.focus");
	let terminal = spans
		.iter()
		.find(|span| span.data["app"].as_str() == Some("Terminal"))
		.expect("the new span arrived");
	assert_eq!(terminal.start, at(1030));
	assert_eq!(spans.len(), 6);
}

#[tokio::test]
async fn dry_runs_write_nothing_and_leave_no_watermark() {
	let dir = tempfile::tempdir().unwrap();
	let db = build_db(dir.path(), Variant::Full);
	let mut harness = start(&dir, &["apps", "screen", "audio"]).await;
	let mut dry = options(&db);
	dry.dry_run = true;
	let report = harness.importer.screenpipe(dry).await.unwrap();

	// Everything was counted, nothing was written…
	assert_eq!(report.stored, 0);
	assert_eq!(report.duplicate, 0);
	assert_eq!(report.counts.values().sum::<usize>(), 9);
	for stream in ["apps.focus", "screen.frame", "audio.transcript"] {
		assert!(records(&harness, stream).is_empty(), "{stream} was written");
	}
	// …and no watermark was kept, so the real run that follows imports it all.
	assert!(!harness.state_dir.join("import").exists());
	let real = harness.importer.screenpipe(options(&db)).await.unwrap();
	assert_eq!(real.stored, 9);
}

#[tokio::test]
async fn a_missing_database_names_the_path() {
	let dir = tempfile::tempdir().unwrap();
	let mut harness = start(&dir, &["apps", "screen", "audio"]).await;
	let mut bad = options(&dir.path().join("nope.sqlite"));
	bad.db = Some(dir.path().join("nope.sqlite"));
	let error = harness.importer.screenpipe(bad).await.unwrap_err();
	assert!(
		error.to_string().contains("no Screenpipe database"),
		"{error}"
	);
}
