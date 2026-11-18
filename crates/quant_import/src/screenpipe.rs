//! Reads a Screenpipe SQLite database and turns it into Quant records.
//!
//! Screenpipe's schema changes between versions, so tables and columns are
//! detected at runtime rather than assumed: the `frames` table is found by
//! name, its optional `browser_url` and `focused` columns are used only when
//! they exist, and its `timestamp` column is read as unix seconds, unix
//! milliseconds, or RFC 3339 text, whichever it holds. The database is opened
//! read-only (`?mode=ro`), and immutable when Screenpipe holds it locked.
//!
//! Frames with the same app and window become one `apps.focus` span; a gap
//! of more than twice the median frame interval — sampled from the first
//! few thousand frames, capped at 60 s — ends the span. Each frame's OCR text
//! becomes a `screen.frame` record, skipped when it repeats the previous
//! frame's text, and audio transcriptions become `audio.transcript`
//! records. Password managers and private browser windows are excluded the
//! same way native capture excludes them.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::Record;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::BATCH;
use crate::ImportError;
use crate::ImportReport;
use crate::Importer;
use crate::Item;
use crate::clean;
use crate::derived_id;
use crate::device_for;
use crate::exclusions;
use crate::hostname;
use crate::parse_text_ts;
use crate::state_file;
use crate::watermark::SourceState;

/// How many frame gaps the median interval is sampled from.
const MEDIAN_SAMPLE: usize = 4_096;

/// Options for one Screenpipe import.
#[derive(Clone, Debug)]
pub struct ScreenpipeOptions {
	/// The Screenpipe database. Defaults to `~/.screenpipe/db.sqlite`.
	pub db: Option<PathBuf>,
	/// Import from this time (inclusive), ignoring the watermark.
	pub since: Option<Timestamp>,
	/// Import up to this time (inclusive).
	pub until: Option<Timestamp>,
	/// Keep polling for new data instead of importing once.
	pub follow: bool,
	/// How often `--follow` polls.
	pub every: Duration,
	/// Count and print what would be imported, without writing anything.
	pub dry_run: bool,
}

impl ScreenpipeOptions {
	/// Options with only the given window and mode set.
	pub fn new(since: Option<Timestamp>, until: Option<Timestamp>, follow: bool) -> Self {
		Self {
			db: None,
			since,
			until,
			follow,
			every: crate::DEFAULT_EVERY,
			dry_run: false,
		}
	}
}

/// The Screenpipe database's default location.
pub fn default_db() -> PathBuf {
	std::env::var_os("HOME").map_or_else(
		|| PathBuf::from(".screenpipe").join("db.sqlite"),
		|home| PathBuf::from(home).join(".screenpipe").join("db.sqlite"),
	)
}

/// A focus span still being extended by `--follow`, carried between passes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpenSpan {
	/// The app that has focus.
	app: String,
	/// The window that has focus.
	window: String,
	/// The id of the frame that opened the span, which the record id is
	/// derived from.
	first_id: i64,
	/// When the span began.
	start: Timestamp,
	/// The newest frame seen in the span.
	last: Timestamp,
	/// The most recent URL seen in the span.
	url: Option<String>,
}

/// What one pass saw, for the watermark file.
pub(crate) struct ScreenpipePass {
	/// The newest frame timestamp.
	frames_mark: Option<Timestamp>,
	/// The newest audio transcription timestamp.
	audio_mark: Option<Timestamp>,
	/// The span still open at the end of the pass.
	open: Option<OpenSpan>,
	/// The last OCR text seen, with its app and window.
	last_text: Option<(String, String, String)>,
}

/// Where a query starts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StartBound {
	/// At this time, inclusive.
	At(Timestamp),
	/// After this time, exclusive (the watermark).
	After(Timestamp),
	/// From the beginning.
	Unbounded,
}

/// How a timestamp column stores times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TsKind {
	/// Unix seconds.
	Seconds,
	/// Unix milliseconds.
	Millis,
	/// RFC 3339 text.
	Text,
}

impl Importer {
	/// One Screenpipe pass: read what is new, ingest it, and advance the
	/// watermark unless this is a dry run.
	pub(crate) async fn screenpipe_once(
		&mut self,
		options: &ScreenpipeOptions,
	) -> Result<ImportReport, ImportError> {
		let db = options.db.clone().unwrap_or_else(default_db);
		if !db.exists() {
			return Err(ImportError::Other(format!(
				"no Screenpipe database at {}",
				db.display()
			)));
		}
		let location = db.canonicalize().map_or_else(
			|_| format!("file:{}", db.display()),
			|path| format!("file:{}", path.display()),
		);
		let file = state_file(&self.state_dir, "screenpipe", &location, &self.vault_id);
		let state = SourceState::load(&file);

		// An explicit --since reprocesses from there, so it resumes no span.
		// A plain re-run continues from the watermark, picking up a span a
		// stopped --follow left open.
		let resume = if options.since.is_some() {
			(None, None)
		} else {
			let open = state
				.open
				.as_ref()
				.and_then(|value| serde_json::from_value::<OpenSpan>(value.clone()).ok());
			(open, state.last_text.clone())
		};
		let frames_start = match options.since {
			Some(since) => StartBound::At(since),
			None => {
				state
					.mark("frames")
					.map_or(StartBound::Unbounded, StartBound::After)
			}
		};
		let audio_start = match options.since {
			Some(since) => StartBound::At(since),
			None => {
				state
					.mark("audio")
					.map_or(StartBound::Unbounded, StartBound::After)
			}
		};

		let (sender, receiver) = mpsc::unbounded_channel();
		let close_tail = !options.follow;
		let until = options.until;
		let reading = tokio::task::spawn_blocking(move || {
			produce(
				&db,
				until,
				frames_start,
				audio_start,
				resume,
				close_tail,
				&sender,
			)
		});
		let report = self.consume(receiver, options.dry_run).await?;
		let pass = reading
			.await
			.map_err(|error| ImportError::Other(error.to_string()))??;

		if !options.dry_run {
			let mut next = state;
			if let Some(mark) = pass.frames_mark {
				next.advance("frames", mark);
			}
			if let Some(mark) = pass.audio_mark {
				next.advance("audio", mark);
			}
			next.open = pass
				.open
				.map(|span| serde_json::to_value(&span))
				.transpose()?;
			next.last_text = pass.last_text;
			next.follow = options.follow;
			next.save(&file)?;
		}
		Ok(report)
	}
}

/// Open the database read-only; immutable when Screenpipe holds it locked.
fn open_readonly(path: &Path) -> Result<Connection, ImportError> {
	if let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
		Ok(conn)
	} else {
		let escaped = path
			.display()
			.to_string()
			.replace('%', "%25")
			.replace('?', "%3f")
			.replace('#', "%23");
		Connection::open_with_flags(
			format!("file:{escaped}?immutable=1"),
			OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
		)
		.map_err(|error| {
			ImportError::Other(format!(
				"could not open {} read-only: {error}",
				path.display()
			))
		})
	}
}

/// The first of `candidates` the database has, matched ignoring case.
fn find_table(conn: &Connection, candidates: &[&str]) -> Option<String> {
	let names: Vec<String> = conn
		.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
		.ok()?
		.query_map([], |row| row.get::<_, String>(0))
		.ok()?
		.filter_map(Result::ok)
		.collect();
	candidates.iter().find_map(|candidate| {
		names
			.iter()
			.find(|name| name.eq_ignore_ascii_case(candidate))
			.cloned()
	})
}

/// A table's columns: `(name, declared type)`, lowercased.
fn table_info(conn: &Connection, table: &str) -> Vec<(String, String)> {
	let quoted = table.replace('"', "\"\"");
	let sql = format!("PRAGMA table_info(\"{quoted}\")");
	let Ok(mut statement) = conn.prepare(&sql) else {
		return Vec::new();
	};
	let rows = statement.query_map([], |row| {
		Ok((
			row.get::<_, String>(1)?.to_lowercase(),
			row.get::<_, String>(2)?.to_lowercase(),
		))
	});
	rows.map(|rows| rows.filter_map(Result::ok).collect())
		.unwrap_or_default()
}

/// How the `timestamp` column of `table` stores times, judged from its
/// declared type and one sampled value.
fn detect_ts_kind(conn: &Connection, table: &str, column: &str) -> TsKind {
	let declared = table_info(conn, table)
		.iter()
		.find(|(name, _)| name == column)
		.map(|(_, kind)| kind.clone())
		.unwrap_or_default();
	let quoted = table.replace('"', "\"\"");
	let sql = format!("SELECT \"{column}\" FROM \"{quoted}\" LIMIT 1");
	let sample: Option<SqlValue> = conn
		.query_row(sql.as_str(), [], |row| row.get(0))
		.ok()
		.flatten();
	let numeric = |value: f64| {
		if value.abs() > 1_000_000_000_000.0 {
			TsKind::Millis
		} else {
			TsKind::Seconds
		}
	};
	match sample {
		Some(SqlValue::Integer(n)) => numeric(n as f64),
		Some(SqlValue::Real(f)) => numeric(f),
		Some(SqlValue::Text(s)) => {
			match s.trim().parse::<i64>() {
				Ok(n) => numeric(n as f64),
				Err(_) => TsKind::Text,
			}
		}
		_ if declared.contains("int") => TsKind::Seconds,
		_ => TsKind::Text,
	}
}

/// The SQL value a timestamp becomes for a comparison in `kind`.
fn bound_value(kind: TsKind, at: Timestamp) -> SqlValue {
	match kind {
		TsKind::Seconds => SqlValue::Integer(at.as_second()),
		TsKind::Millis => SqlValue::Integer(at.as_millisecond()),
		TsKind::Text => SqlValue::Text(at.to_string()),
	}
}

/// Read a timestamp column's value as a Quant time.
fn parse_ts(value: SqlValue) -> Option<Timestamp> {
	// Screenpipe stores seconds, milliseconds, or text; a real column is
	// read with its fraction rounded to the nearest millisecond.
	#[allow(clippy::cast_possible_truncation)]
	let real_millis = |f: f64| -> i64 { (f * 1000.0).round() as i64 };
	let numeric = |millis: i64| {
		if millis > 1_000_000_000_000 {
			Timestamp::from_millisecond(millis).ok()
		} else {
			Timestamp::from_second(millis).ok()
		}
	};
	match value {
		SqlValue::Integer(n) => numeric(n),
		SqlValue::Real(f) => {
			if f.abs() > 1_000_000_000_000.0 {
				#[allow(clippy::cast_possible_truncation)]
				let whole = f as i64;
				numeric(whole)
			} else {
				numeric(real_millis(f))
			}
		}
		SqlValue::Text(text) => {
			text.trim()
				.parse::<i64>()
				.ok()
				.and_then(numeric)
				.or_else(|| parse_text_ts(&text))
		}
		_ => None,
	}
}

/// `timestamp <op> ?` clauses for a start and end bound.
fn where_clause(
	column: &str,
	start: StartBound,
	until: Option<Timestamp>,
	kind: TsKind,
	params: &mut Vec<SqlValue>,
) -> String {
	let mut parts: Vec<String> = Vec::new();
	match start {
		StartBound::At(from) => {
			parts.push(format!("\"{column}\" >= ?"));
			params.push(bound_value(kind, from));
		}
		StartBound::After(mark) => {
			parts.push(format!("\"{column}\" > ?"));
			params.push(bound_value(kind, mark));
		}
		StartBound::Unbounded => {}
	}
	if let Some(until) = until {
		parts.push(format!("\"{column}\" <= ?"));
		params.push(bound_value(kind, until));
	}
	if parts.is_empty() {
		String::new()
	} else {
		format!(" WHERE {}", parts.join(" AND "))
	}
}

/// Seconds between two times, as a float.
fn secs(from: Timestamp, to: Timestamp) -> f64 {
	(to.as_millisecond() - from.as_millisecond()) as f64 / 1000.0
}

/// One frame row.
struct Frame {
	id: i64,
	at: Timestamp,
	app: String,
	window: String,
	url: Option<String>,
}

/// Records gathered for one device until a batch is full.
struct Batch<'a> {
	sender: &'a mpsc::UnboundedSender<Item>,
	label: String,
	records: Vec<Record>,
}

impl Batch<'_> {
	fn push(&mut self, record: Record) -> Result<(), ImportError> {
		self.records.push(record);
		if self.records.len() >= BATCH {
			self.flush()?;
		}
		Ok(())
	}

	fn flush(&mut self) -> Result<(), ImportError> {
		if self.records.is_empty() {
			return Ok(());
		}
		// Unbounded sends never block, so the reading thread can keep
		// streaming while the vault accepts what came before.
		self.sender
			.send(Item {
				label: self.label.clone(),
				records: std::mem::take(&mut self.records),
			})
			.map_err(|_| ImportError::Other("the import was cancelled".into()))
	}
}

/// The record a closed focus span becomes. `end` is capped at the frame that
/// ended the span, so consecutive spans never overlap.
fn span_record(span: &OpenSpan, end: Timestamp, device: DeviceId) -> Option<Record> {
	let end = end.max(span.start);
	if end <= span.start {
		return None;
	}
	let excluded = exclusions();
	let private =
		excluded.excludes_name(Some(&span.app)) || excluded.excludes_title(Some(&span.window));
	let mut record = Record {
		id: derived_id(
			"screenpipe",
			&format!("frame-span:{}", span.first_id),
			span.start,
			"apps.focus",
		),
		stream: "apps.focus".parse().expect("a built-in stream id"),
		device,
		start: span.start,
		end: Some(end),
		text: None,
		data: serde_json::Map::new(),
		media: None,
	};
	record.data.insert("app".into(), clean(&span.app).into());
	record.data.insert("source".into(), "screenpipe".into());
	if !private {
		if !span.window.is_empty() {
			record
				.data
				.insert("title".into(), clean(&span.window).into());
		}
		if let Some(url) = span.url.as_deref().filter(|url| !url.is_empty()) {
			record.data.insert("url".into(), clean(url).into());
		}
	}
	Some(record)
}

/// The OCR text of one frame, as a `screen.frame` record. Frames whose app
/// or window is excluded are skipped entirely, the way native capture skips
/// the frame itself.
fn ocr_record(frame: &Frame, text: &str, device: DeviceId) -> Option<Record> {
	if text.trim().is_empty() {
		return None;
	}
	let excluded = exclusions();
	if excluded.excludes_name(Some(&frame.app)) || excluded.excludes_title(Some(&frame.window)) {
		return None;
	}
	let mut record = Record {
		id: derived_id(
			"screenpipe",
			&format!("frame-text:{}", frame.id),
			frame.at,
			"screen.frame",
		),
		stream: "screen.frame".parse().expect("a built-in stream id"),
		device,
		start: frame.at,
		end: None,
		text: Some(clean(text)),
		data: serde_json::Map::new(),
		media: None,
	};
	record.data.insert("source".into(), "screenpipe".into());
	if !frame.app.is_empty() {
		record.data.insert("app".into(), clean(&frame.app).into());
	}
	if !frame.window.is_empty() {
		record
			.data
			.insert("title".into(), clean(&frame.window).into());
	}
	if let Some(url) = frame.url.as_deref().filter(|url| !url.is_empty()) {
		record.data.insert("url".into(), clean(url).into());
	}
	Some(record)
}

/// One audio transcription row, as an `audio.transcript` record.
fn audio_record(
	id: i64,
	at: Timestamp,
	transcription: &str,
	device_name: Option<&str>,
	input: Option<bool>,
	speaker: Option<i64>,
	device: DeviceId,
) -> Option<Record> {
	if transcription.trim().is_empty() {
		return None;
	}
	let mut record = Record {
		id: derived_id("screenpipe", &format!("audio:{id}"), at, "audio.transcript"),
		stream: "audio.transcript".parse().expect("a built-in stream id"),
		device,
		start: at,
		end: None,
		text: Some(clean(transcription)),
		data: serde_json::Map::new(),
		media: None,
	};
	record.data.insert("source".into(), "screenpipe".into());
	if let Some(name) = device_name.filter(|name| !name.is_empty()) {
		record.data.insert("device".into(), clean(name).into());
	}
	if let Some(input) = input {
		record.data.insert(
			"direction".into(),
			if input { "input" } else { "output" }.into(),
		);
	}
	if let Some(speaker) = speaker {
		record.data.insert("speaker".into(), speaker.into());
	}
	Some(record)
}

/// Read one pass of Screenpipe history and send the records it maps to.
///
/// Frames stream out of the database row by row; only one span, one OCR
/// chunk, and one batch of records are held at a time. Returns the marks
/// and span state the caller should keep.
#[allow(clippy::too_many_arguments)]
pub(crate) fn produce(
	db: &Path,
	until: Option<Timestamp>,
	frames_start: StartBound,
	audio_start: StartBound,
	resume: (Option<OpenSpan>, Option<(String, String, String)>),
	close_tail: bool,
	sender: &mpsc::UnboundedSender<Item>,
) -> Result<ScreenpipePass, ImportError> {
	let conn = open_readonly(db)?;
	let frames = find_table(&conn, &["frames"])
		.ok_or_else(|| ImportError::Other(format!("{} has no frames table", db.display())))?;
	let ocr = find_table(&conn, &["ocr_text", "ocr_texts"]);
	let audio = find_table(
		&conn,
		&[
			"audio_transcriptions",
			"audio_transcription",
			"audio_transcripts",
		],
	);

	let frames_info = table_info(&conn, &frames);
	let frames_has = |name: &str| frames_info.iter().any(|(column, _)| column == name);
	let ts_column = "timestamp";
	let app_column = "app_name";
	let window_column = "window_name";
	for column in [ts_column, app_column, window_column] {
		if !frames_has(column) {
			return Err(ImportError::Other(format!(
				"the frames table has no {column} column"
			)));
		}
	}
	let url_column = frames_has("browser_url");
	let focused_column = frames_has("focused");
	let frames_ts = detect_ts_kind(&conn, &frames, ts_column);

	let label = hostname();
	let device = device_for("screenpipe", &label);
	let mut batch = Batch {
		sender,
		label,
		records: Vec::new(),
	};

	// The median frame interval, sampled from the start of the window: the
	// span gap threshold and the span tail both follow the source's rhythm.
	let mut params = Vec::new();
	let frames_where = where_clause(ts_column, frames_start, until, frames_ts, &mut params);
	let sample_sql = format!(
		"SELECT \"{ts_column}\" FROM \"{frames}\"{frames_where} ORDER BY \"{ts_column}\", \"id\" \
		 LIMIT {}",
		MEDIAN_SAMPLE + 1
	);
	let mut gaps: Vec<f64> = Vec::new();
	{
		let mut statement = conn.prepare(&sample_sql)?;
		let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
		let mut previous: Option<Timestamp> = None;
		while let Some(row) = rows.next()? {
			let Some(at) = parse_ts(row.get::<_, SqlValue>(0)?) else {
				continue;
			};
			if let Some(previous) = previous {
				let gap = secs(previous, at);
				if gap > 0.0 {
					gaps.push(gap);
				}
			}
			previous = Some(at);
			if gaps.len() >= MEDIAN_SAMPLE {
				break;
			}
		}
	}
	gaps.sort_unstable_by(f64::total_cmp);
	let median = if gaps.is_empty() {
		1.0
	} else if gaps.len() % 2 == 1 {
		gaps[gaps.len() / 2]
	} else {
		f64::midpoint(gaps[gaps.len() / 2 - 1], gaps[gaps.len() / 2])
	};
	// A gap of more than twice the median interval ends a span, capped at a
	// minute; the floor keeps sparse clocks from shredding every frame into
	// its own span.
	let threshold = (2.0 * median).clamp(2.0, 60.0);
	let tail = span_tail(median);

	// Frames, in order, merged into spans.
	let mut params = Vec::new();
	let frames_where = where_clause(ts_column, frames_start, until, frames_ts, &mut params);
	let mut columns = ["id", ts_column, app_column, window_column]
		.iter()
		.map(|column| format!("\"{column}\""))
		.collect::<Vec<_>>();
	if url_column {
		columns.push("\"browser_url\"".into());
	}
	if focused_column {
		columns.push("\"focused\"".into());
	}
	let frames_sql = format!(
		"SELECT {} FROM \"{frames}\"{frames_where} ORDER BY \"{ts_column}\", \"id\"",
		columns.join(", ")
	);
	let (mut open, mut last_text) = resume;
	let mut frames_mark: Option<Timestamp> = None;
	{
		let mut statement = conn.prepare(&frames_sql)?;
		let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
		let mut ocr_chunk: Vec<Frame> = Vec::new();
		while let Some(row) = rows.next()? {
			let id: i64 = row.get(0)?;
			let Some(at) = parse_ts(row.get::<_, SqlValue>(1)?) else {
				continue;
			};
			let app: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
			let window: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
			let mut url = if url_column {
				row.get::<_, Option<String>>(4).unwrap_or_default()
			} else {
				None
			};
			if url.as_deref().is_some_and(str::is_empty) {
				url = None;
			}
			if focused_column {
				let focused = match row.get::<_, SqlValue>(if url_column { 5 } else { 4 })? {
					SqlValue::Integer(n) => n != 0,
					SqlValue::Real(f) => f != 0.0,
					SqlValue::Text(text) => matches!(text.trim(), "1" | "t" | "true"),
					SqlValue::Null | SqlValue::Blob(_) => true,
				};
				if !focused {
					continue;
				}
			}
			frames_mark = Some(frames_mark.map_or(at, |mark| mark.max(at)));

			// The span machine: same app and window extends the span; a gap
			// beyond the threshold or a change closes it.
			if !app.is_empty() {
				let extends = open.as_ref().is_some_and(|span| {
					span.app == app && span.window == window && secs(span.last, at) <= threshold
				});
				if extends {
					let span = open.as_mut().expect("checked above");
					span.last = at;
					if url.is_some() {
						span.url.clone_from(&url);
					}
				} else {
					if let Some(span) = open.take() {
						let end = at.min(span.last.checked_add(tail).unwrap_or(span.last));
						if let Some(record) = span_record(&span, end, device) {
							batch.push(record)?;
						}
					}
					open = Some(OpenSpan {
						app: app.clone(),
						window: window.clone(),
						first_id: id,
						start: at,
						last: at,
						url: url.clone(),
					});
				}
			}

			// OCR text, buffered a chunk at a time and joined per frame.
			if let Some(ocr) = ocr.as_deref() {
				ocr_chunk.push(Frame {
					id,
					at,
					app,
					window,
					url,
				});
				if ocr_chunk.len() >= BATCH {
					emit_ocr(
						&conn,
						ocr,
						&mut ocr_chunk,
						&mut last_text,
						&mut batch,
						device,
					)?;
				}
			}
		}
		if let Some(ocr) = ocr.as_deref() {
			emit_ocr(
				&conn,
				ocr,
				&mut ocr_chunk,
				&mut last_text,
				&mut batch,
				device,
			)?;
		}
	}

	if close_tail && let Some(span) = open.take() {
		let end = span.last.checked_add(tail).unwrap_or(span.last);
		if let Some(record) = span_record(&span, end, device) {
			batch.push(record)?;
		}
	}
	batch.flush()?;

	// Audio transcriptions, streamed like frames.
	let mut audio_mark: Option<Timestamp> = None;
	if let Some(audio_table) = audio.as_deref() {
		let audio_info = table_info(&conn, audio_table);
		let audio_has = |name: &str| audio_info.iter().any(|(column, _)| column == name);
		let text_column = if audio_has("transcription") {
			Some("transcription")
		} else if audio_has("text") {
			Some("text")
		} else {
			None
		};
		if let Some(text_column) = text_column {
			let audio_ts = detect_ts_kind(&conn, audio_table, ts_column);
			let mut params = Vec::new();
			let audio_where = where_clause(ts_column, audio_start, until, audio_ts, &mut params);
			let mut columns: Vec<&str> = Vec::new();
			if audio_has("id") {
				columns.push("id");
			}
			columns.push(ts_column);
			columns.push(text_column);
			if audio_has("device") {
				columns.push("device");
			}
			if audio_has("is_input_device") {
				columns.push("is_input_device");
			}
			if audio_has("speaker_id") {
				columns.push("speaker_id");
			}
			let audio_sql = format!(
				"SELECT {} FROM \"{audio_table}\"{audio_where} ORDER BY \"{ts_column}\"",
				columns
					.iter()
					.map(|column| format!("\"{column}\""))
					.collect::<Vec<_>>()
					.join(", ")
			);
			let mut statement = conn.prepare(&audio_sql)?;
			let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
			while let Some(row) = rows.next()? {
				let mut column = 0;
				let id = if audio_has("id") {
					let id = row.get::<_, Option<i64>>(0)?.unwrap_or_default();
					column += 1;
					id
				} else {
					0
				};
				let Some(at) = parse_ts(row.get::<_, SqlValue>(column)?) else {
					continue;
				};
				column += 1;
				let transcription: String =
					row.get::<_, Option<String>>(column)?.unwrap_or_default();
				let device_name = if audio_has("device") {
					column += 1;
					row.get::<_, Option<String>>(column).unwrap_or_default()
				} else {
					None
				};
				let input = if audio_has("is_input_device") {
					column += 1;
					match row.get::<_, SqlValue>(column)? {
						SqlValue::Integer(n) => Some(n != 0),
						SqlValue::Real(f) => Some(f != 0.0),
						SqlValue::Text(text) => Some(matches!(text.trim(), "1" | "t" | "true")),
						_ => None,
					}
				} else {
					None
				};
				let speaker = if audio_has("speaker_id") {
					column += 1;
					row.get::<_, Option<i64>>(column).unwrap_or_default()
				} else {
					None
				};
				if let Some(record) = audio_record(
					id,
					at,
					&transcription,
					device_name.as_deref(),
					input,
					speaker,
					device,
				) {
					batch.push(record)?;
				}
				audio_mark = Some(audio_mark.map_or(at, |mark| mark.max(at)));
			}
		}
	}
	batch.flush()?;

	Ok(ScreenpipePass {
		frames_mark,
		audio_mark,
		open,
		last_text,
	})
}

/// How long a closed span runs past its last frame.
fn span_tail(median: f64) -> jiff::SignedDuration {
	jiff::SignedDuration::from_secs_f64(median)
}

/// Look up the OCR text of a chunk of frames and emit one record per frame,
/// skipping frames whose text repeats the previous frame's.
fn emit_ocr(
	conn: &Connection,
	ocr: &str,
	chunk: &mut Vec<Frame>,
	last_text: &mut Option<(String, String, String)>,
	batch: &mut Batch<'_>,
	device: DeviceId,
) -> Result<(), ImportError> {
	if chunk.is_empty() {
		return Ok(());
	}
	let ids = chunk.iter().map(|frame| SqlValue::Integer(frame.id));
	let placeholders = vec!["?"; chunk.len()].join(", ");
	let sql = format!(
		"SELECT \"frame_id\", \"text\" FROM \"{ocr}\" WHERE \"frame_id\" IN ({placeholders})"
	);
	let mut texts: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
	{
		let mut statement = conn.prepare(&sql)?;
		let mut rows = statement.query(rusqlite::params_from_iter(ids))?;
		while let Some(row) = rows.next()? {
			let frame_id: i64 = row.get::<_, Option<i64>>(0)?.unwrap_or_default();
			let text: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
			texts.entry(frame_id).or_default().push(text);
		}
	}
	for frame in chunk.iter() {
		let text = texts
			.get(&frame.id)
			.map(|parts| parts.join("\n"))
			.unwrap_or_default();
		let repeats = last_text.as_ref().is_some_and(|(app, window, previous)| {
			*app == frame.app && *window == frame.window && *previous == text
		});
		if !repeats && let Some(record) = ocr_record(frame, &text, device) {
			batch.push(record)?;
		}
		*last_text = Some((frame.app.clone(), frame.window.clone(), text));
	}
	chunk.clear();
	Ok(())
}
