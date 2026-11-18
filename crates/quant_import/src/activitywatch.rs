//! Reads `ActivityWatch` history and turns it into Quant records.
//!
//! History comes either from the aw-server REST API (`GET /api/0/buckets`,
//! then each bucket's `/events`) or from an exported JSON file for offline
//! import. Buckets are classified by type: `aw-watcher-window` events
//! (`data.app`, `data.title`) become `apps.focus` spans, `aw-watcher-afk`
//! events become `device.state` idle and active spans, and `aw-watcher-web`
//! events become `apps.focus` spans carrying the page URL — the closest
//! existing stream, since native focus spans have no URL of their own.
//! Events from another host keep that hostname as their device label.
//!
//! When a host also has a web watcher, the window watcher's events for that
//! browser are skipped, so browser focus time is not counted twice.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::Method;
use hyper::Request;
use hyper_util::rt::TokioIo;
use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::Record;
use serde_json::Map;
use serde_json::Value;
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
use crate::parse_text_ts;
use crate::state_file;
use crate::watermark::SourceState;

/// Events per REST page.
const PAGE: usize = 1_000;

/// The aw-server REST API's default location.
pub const DEFAULT_URL: &str = "http://localhost:5600";

/// Options for one `ActivityWatch` import.
#[derive(Clone, Debug)]
pub struct ActivityWatchOptions {
	/// The aw-server REST API. Defaults to `http://localhost:5600`.
	pub url: Option<String>,
	/// An exported JSON file, imported offline instead of over REST.
	pub file: Option<PathBuf>,
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

impl ActivityWatchOptions {
	/// Options reading from `url` or, when `file` is set, from an export.
	pub fn new(url: Option<String>, file: Option<PathBuf>) -> Self {
		Self {
			url,
			file,
			since: None,
			until: None,
			follow: false,
			every: crate::DEFAULT_EVERY,
			dry_run: false,
		}
	}
}

/// Which watcher a bucket belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BucketKind {
	/// `aw-watcher-window`: the focused app and window.
	Window,
	/// `aw-watcher-afk`: whether anyone was at the machine.
	Afk,
	/// `aw-watcher-web`: the focused browser tab.
	Web,
}

/// One `ActivityWatch` bucket worth importing.
struct Bucket {
	id: String,
	kind: BucketKind,
	/// The host the watcher ran on.
	hostname: String,
	/// For web buckets, the browser the id names (`chrome`, `firefox`, …).
	browser: Option<String>,
}

/// One `ActivityWatch` event.
struct AwEvent {
	id: Option<i64>,
	at: Timestamp,
	/// Seconds, often fractional.
	duration: f64,
	data: Map<String, Value>,
}

impl AwEvent {
	/// When the event ended.
	fn end(&self) -> Timestamp {
		self.at
			.checked_add(jiff::SignedDuration::from_secs_f64(self.duration.max(0.0)))
			.unwrap_or(self.at)
	}
}

impl Importer {
	/// One `ActivityWatch` pass: read what is new, ingest it, and advance the
	/// watermark unless this is a dry run.
	pub(crate) async fn activitywatch_once(
		&mut self,
		options: &ActivityWatchOptions,
	) -> Result<ImportReport, ImportError> {
		let file = options.file.clone();
		let url = options
			.url
			.clone()
			.unwrap_or_else(|| DEFAULT_URL.to_owned());
		let location = match &file {
			Some(path) => {
				path.canonicalize().map_or_else(
					|_| format!("file:{}", path.display()),
					|path| format!("file:{}", path.display()),
				)
			}
			None => format!("url:{url}"),
		};
		let state_path = state_file(&self.state_dir, "activitywatch", &location, &self.vault_id);
		let state = SourceState::load(&state_path);

		let (sender, receiver) = mpsc::unbounded_channel();
		let marks = state.marks.clone();
		let since = options.since;
		let until = options.until;
		let source = match &file {
			Some(path) => Source::File(path.clone()),
			None => Source::Rest(url),
		};
		let reading = tokio::spawn(produce(source, since, until, marks, sender));
		let report = self.consume(receiver, options.dry_run).await?;
		let marks = reading
			.await
			.map_err(|error| ImportError::Other(error.to_string()))??;

		if !options.dry_run {
			let mut next = state;
			for (bucket, mark) in marks {
				next.advance(&bucket, mark);
			}
			next.save(&state_path)?;
		}
		Ok(report)
	}
}

/// Where `ActivityWatch` history comes from.
enum Source {
	/// The aw-server REST API.
	Rest(String),
	/// An exported JSON file.
	File(PathBuf),
}

/// Read every relevant bucket's new events and send the records they map
/// to. Returns the newest event time per bucket, for the watermark.
async fn produce(
	source: Source,
	since: Option<Timestamp>,
	until: Option<Timestamp>,
	marks: BTreeMap<String, Timestamp>,
	sender: mpsc::UnboundedSender<Item>,
) -> Result<BTreeMap<String, Timestamp>, ImportError> {
	let buckets: Vec<(Bucket, Vec<AwEvent>)> = match &source {
		Source::File(path) => buckets_from_file(path).await?,
		Source::Rest(base) => {
			let mut pairs = Vec::new();
			for bucket in buckets_from_rest(base).await? {
				// An explicit --since reprocesses from there, so it ignores
				// the watermark; without one, the watermark is the start.
				let mark = if since.is_some() {
					None
				} else {
					marks.get(&format!("bucket:{}", bucket.id)).copied()
				};
				let events = fetch_events(base, &bucket.id, since.or(mark), until).await?;
				pairs.push((bucket, events));
			}
			pairs
		}
	};
	// The window is applied to REST answers too, in case a server ignores a
	// bound, and to file exports, which carry everything.
	let windowed: Vec<(Bucket, Vec<AwEvent>)> = buckets
		.into_iter()
		.map(|(bucket, events)| {
			let mark = if since.is_some() {
				None
			} else {
				marks.get(&format!("bucket:{}", bucket.id)).copied()
			};
			let events = events
				.into_iter()
				.filter(|event| {
					since.is_none_or(|since| event.at >= since)
						&& mark.is_none_or(|mark| event.at > mark)
						&& until.is_none_or(|until| event.at <= until)
				})
				.collect();
			(bucket, events)
		})
		.collect();

	// A web watcher covers its browser's focus time in finer detail, so the
	// window watcher's events for that browser are skipped.
	let mut browsers: BTreeMap<String, HashSet<String>> = BTreeMap::new();
	for (bucket, _) in &windowed {
		if bucket.kind == BucketKind::Web
			&& let Some(browser) = bucket.browser.as_deref()
		{
			browsers
				.entry(bucket.hostname.clone())
				.or_default()
				.insert(browser.to_owned());
		}
	}

	let mut newest = BTreeMap::new();
	for (bucket, events) in &windowed {
		let records = match bucket.kind {
			BucketKind::Window => window_records(bucket, events, &browsers),
			BucketKind::Afk => afk_records(bucket, events),
			BucketKind::Web => web_records(bucket, events),
		};
		for chunk in records.chunks(BATCH) {
			sender
				.send(Item {
					label: bucket.hostname.clone(),
					records: chunk.to_vec(),
				})
				.map_err(|_| ImportError::Other("the import was cancelled".into()))?;
		}
		if let Some(last) = events.iter().map(|event| event.at).max() {
			newest.insert(format!("bucket:{}", bucket.id), last);
		}
	}
	Ok(newest)
}

/// A GET that answers JSON, over plain http to a local aw-server.
async fn get_json(url: &str) -> Result<Value, ImportError> {
	let uri: hyper::Uri = url
		.parse()
		.map_err(|error| ImportError::Other(format!("{url} is not a URL: {error}")))?;
	if uri.scheme_str() != Some("http") {
		return Err(ImportError::Other(
			"the ActivityWatch URL must be plain http on this machine".into(),
		));
	}
	let host = uri
		.host()
		.ok_or_else(|| ImportError::Other(format!("{url} names no host")))?;
	let port = uri.port_u16().unwrap_or(80);
	let tcp = tokio::net::TcpStream::connect((host, port))
		.await
		.map_err(|error| {
			ImportError::Other(format!(
				"cannot reach ActivityWatch at {host}:{port}: {error}"
			))
		})?;
	let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
		.await
		.map_err(|error| ImportError::Other(format!("http: {error}")))?;
	tokio::spawn(async move {
		let _ = connection.await;
	});
	let path = uri.path_and_query().map_or("/", |path| path.as_str());
	let request = Request::builder()
		.method(Method::GET)
		.uri(path)
		.header(
			"host",
			uri.authority().map(ToString::to_string).unwrap_or_default(),
		)
		.body(Full::new(bytes::Bytes::new()))
		.map_err(|error| ImportError::Other(format!("http: {error}")))?;
	let response = sender
		.send_request(request)
		.await
		.map_err(|error| ImportError::Other(format!("http: {error}")))?;
	let status = response.status();
	let body = response
		.into_body()
		.collect()
		.await
		.map_err(|error| ImportError::Other(format!("http: {error}")))?
		.to_bytes();
	if !status.is_success() {
		return Err(ImportError::Other(format!(
			"ActivityWatch answered {status}: {}",
			String::from_utf8_lossy(&body)
		)));
	}
	Ok(serde_json::from_slice(&body)?)
}

/// Percent-encode a path segment or query value.
fn escape(part: &str) -> String {
	let mut out = String::with_capacity(part.len());
	for byte in part.bytes() {
		if byte.is_ascii_alphanumeric() || b"-_.".contains(&byte) {
			out.push(byte as char);
		} else {
			let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
		}
	}
	out
}

/// Which buckets the source has, classified.
fn classify(buckets: &[Value]) -> Vec<Bucket> {
	let mut seen = Vec::new();
	for bucket in buckets {
		let Some(id) = bucket["id"].as_str() else {
			continue;
		};
		let watcher = bucket["type"].as_str().unwrap_or_default().to_owned();
		let kind = match watcher.as_str() {
			"currentwindow" => Some(BucketKind::Window),
			"afkstatus" => Some(BucketKind::Afk),
			"web.tab.current" => Some(BucketKind::Web),
			_ => {
				if id.starts_with("aw-watcher-window") {
					Some(BucketKind::Window)
				} else if id.starts_with("aw-watcher-afk") {
					Some(BucketKind::Afk)
				} else if id.starts_with("aw-watcher-web") {
					Some(BucketKind::Web)
				} else {
					None
				}
			}
		};
		let Some(kind) = kind else {
			continue;
		};
		let hostname = bucket["hostname"]
			.as_str()
			.or_else(|| bucket["host"].as_str())
			.map(ToOwned::to_owned)
			.or_else(|| {
				id.split_once('_')
					.map(|(_, host)| host.trim_end_matches('_').to_owned())
			})
			.unwrap_or_else(|| "unknown host".into());
		let browser = (kind == BucketKind::Web)
			.then(|| {
				// The id is `aw-watcher-web-<browser>_<hostname>`, so the
				// host's suffix is stripped to leave the browser alone.
				let suffix = format!("_{hostname}");
				id.strip_prefix("aw-watcher-web-")
					.or_else(|| id.rsplit('-').next())
					.unwrap_or_default()
					.trim_end_matches(&suffix)
					.to_owned()
			})
			.filter(|browser| !browser.is_empty());
		seen.push(Bucket {
			id: id.to_owned(),
			kind,
			hostname,
			browser,
		});
	}
	seen
}

/// Every bucket from an exported JSON file, with its events inline.
async fn buckets_from_file(path: &Path) -> Result<Vec<(Bucket, Vec<AwEvent>)>, ImportError> {
	let path = path.to_owned();
	let exported = tokio::task::spawn_blocking(move || std::fs::read(&path))
		.await
		.map_err(|error| ImportError::Other(error.to_string()))?
		.map_err(|error| ImportError::Other(format!("cannot read the export: {error}")))?;
	let value: Value = serde_json::from_slice(&exported)?;
	let raw = match value.get("buckets") {
		Some(Value::Array(list)) => list.clone(),
		Some(Value::Object(map)) => {
			map.into_iter()
				.map(|(id, info)| {
					let mut info = info.clone();
					if info.get("id").is_none() {
						info["id"] = Value::String(id.clone());
					}
					info
				})
				.collect::<Vec<_>>()
		}
		_ => {
			return Err(ImportError::Other(
				"the export has no buckets; is it an ActivityWatch export?".into(),
			));
		}
	};
	Ok(classify(&raw)
		.into_iter()
		.map(|bucket| {
			let events = raw
				.iter()
				.find(|info| info["id"].as_str() == Some(bucket.id.as_str()))
				.and_then(|info| info.get("events"))
				.and_then(Value::as_array)
				.map(|events| events.iter().filter_map(parse_event).collect())
				.unwrap_or_default();
			(bucket, events)
		})
		.collect())
}

/// Every bucket the REST server has.
async fn buckets_from_rest(base: &str) -> Result<Vec<Bucket>, ImportError> {
	let value = get_json(&format!("{base}/api/0/buckets")).await?;
	let raw = match value.get("buckets") {
		Some(Value::Array(list)) => list.clone(),
		Some(Value::Object(map)) => {
			map.into_iter()
				.map(|(id, info)| {
					let mut info = info.clone();
					if info.get("id").is_none() {
						info["id"] = Value::String(id.clone());
					}
					info
				})
				.collect::<Vec<_>>()
		}
		_ => return Err(ImportError::Other("the server listed no buckets".into())),
	};
	Ok(classify(&raw))
}

/// One event out of the export's or server's JSON.
fn parse_event(value: &Value) -> Option<AwEvent> {
	let at = parse_text_ts(value["timestamp"].as_str()?)?;
	let duration = match &value["duration"] {
		Value::Number(number) => number.as_f64().unwrap_or_default(),
		Value::String(text) => text.trim().parse().unwrap_or_default(),
		_ => 0.0,
	};
	Some(AwEvent {
		id: value["id"].as_i64(),
		at,
		duration,
		data: value["data"].as_object().cloned().unwrap_or_default(),
	})
}

/// A bucket's events from the REST server, paged so a long history never
/// loads at once. The window is `[start, until]`, exclusive of nothing.
async fn fetch_events(
	base: &str,
	bucket: &str,
	start: Option<Timestamp>,
	until: Option<Timestamp>,
) -> Result<Vec<AwEvent>, ImportError> {
	let mut events: Vec<AwEvent> = Vec::new();
	let mut cursor = start;
	let mut boundary_seen: Vec<i64> = Vec::new();
	let mut boundary: Option<Timestamp> = None;
	loop {
		let mut query = String::from("?limit=");
		query.push_str(&PAGE.to_string());
		if let Some(start) = cursor {
			query.push_str("&start=");
			query.push_str(&escape(&start.to_string()));
		}
		if let Some(until) = until {
			query.push_str("&end=");
			query.push_str(&escape(&until.to_string()));
		}
		let page = get_json(&format!(
			"{base}/api/0/buckets/{}/events{query}",
			escape(bucket)
		))
		.await?;
		let Value::Array(rows) = page else {
			return Err(ImportError::Other(
				"the server's events are not a list".into(),
			));
		};
		let mut rows: Vec<AwEvent> = rows.iter().filter_map(parse_event).collect();
		if rows.is_empty() {
			break;
		}
		rows.sort_by(|a, b| {
			a.at.cmp(&b.at)
				.then_with(|| a.id.unwrap_or_default().cmp(&b.id.unwrap_or_default()))
		});
		for event in &rows {
			let seen_here = event
				.id
				.is_some_and(|id| boundary == Some(event.at) && boundary_seen.contains(&id));
			if !seen_here {
				events.push(AwEvent {
					id: event.id,
					at: event.at,
					duration: event.duration,
					data: event.data.clone(),
				});
			}
		}
		let page_end = rows.last().expect("rows is not empty").at;
		boundary = Some(page_end);
		boundary_seen = rows
			.iter()
			.filter(|event| event.at == page_end)
			.filter_map(|event| event.id)
			.collect();
		if rows.len() < PAGE || cursor == Some(page_end) {
			break;
		}
		cursor = Some(page_end);
	}
	events.sort_by(|a, b| {
		a.at.cmp(&b.at)
			.then_with(|| a.id.unwrap_or_default().cmp(&b.id.unwrap_or_default()))
	});
	Ok(events)
}

/// The browser a window event's app belongs to, when a web watcher for that
/// host covers it.
fn covered_browser<'a>(
	app: &str,
	browsers: &'a BTreeMap<String, HashSet<String>>,
	hostname: &str,
) -> Option<&'a str> {
	browsers
		.get(hostname)?
		.iter()
		.find(|browser| app.to_lowercase().contains(browser.as_str()))
		.map(String::as_str)
}

/// A span still being merged out of consecutive events.
struct Run {
	source_id: String,
	start: Timestamp,
	end: Timestamp,
	app: String,
	title: String,
	url: Option<String>,
	state: &'static str,
}

/// Close one merged run into a record, when it has any duration.
fn close_run(run: &Run, stream: &str, device: DeviceId) -> Option<Record> {
	if run.end <= run.start {
		return None;
	}
	let mut record = Record {
		id: derived_id("activitywatch", &run.source_id, run.start, stream),
		stream: stream.parse().expect("a built-in stream id"),
		device,
		start: run.start,
		end: Some(run.end),
		text: None,
		data: serde_json::Map::new(),
		media: None,
	};
	let excluded = exclusions();
	let private =
		excluded.excludes_name(Some(&run.app)) || excluded.excludes_title(Some(&run.title));
	record.data.insert("source".into(), "activitywatch".into());
	if stream == "device.state" {
		record.data.insert("state".into(), run.state.into());
	} else {
		record.data.insert("app".into(), clean(&run.app).into());
		if !private {
			if !run.title.is_empty() {
				record.data.insert("title".into(), clean(&run.title).into());
			}
			if let Some(url) = run.url.as_deref().filter(|url| !url.is_empty()) {
				record.data.insert("url".into(), clean(url).into());
			}
		}
	}
	Some(record)
}

/// `aw-watcher-window` events: runs of the same app and window become one
/// `apps.focus` span.
fn window_records(
	bucket: &Bucket,
	events: &[AwEvent],
	browsers: &BTreeMap<String, HashSet<String>>,
) -> Vec<Record> {
	let device = device_for("activitywatch", &bucket.hostname);
	let mut records = Vec::new();
	let mut open: Option<Run> = None;
	for event in events {
		let app = event
			.data
			.get("app")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let title = event
			.data
			.get("title")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		// A web watcher on the same host covers this browser in finer
		// detail; counting the window too would count the time twice.
		if covered_browser(&app, browsers, &bucket.hostname).is_some() {
			continue;
		}
		let extends = open
			.as_ref()
			.is_some_and(|run| run.app == app && run.title == title && event.at <= run.end);
		if extends {
			let run = open.as_mut().expect("checked above");
			run.end = run.end.max(event.end());
		} else {
			if let Some(run) = open.take()
				&& let Some(record) = close_run(&run, "apps.focus", device)
			{
				records.push(record);
			}
			open = Some(Run {
				source_id: format!("window:{}:{}", bucket.id, event.source_key()),
				start: event.at,
				end: event.end(),
				app,
				title,
				url: None,
				state: "",
			});
		}
	}
	if let Some(run) = open.take()
		&& let Some(record) = close_run(&run, "apps.focus", device)
	{
		records.push(record);
	}
	records
}

/// `aw-watcher-afk` events: afk and not-afk runs become `device.state` idle
/// and active spans.
fn afk_records(bucket: &Bucket, events: &[AwEvent]) -> Vec<Record> {
	let device = device_for("activitywatch", &bucket.hostname);
	let mut records = Vec::new();
	let mut open: Option<Run> = None;
	for event in events {
		let Some(status) = event.data.get("status").and_then(Value::as_str) else {
			continue;
		};
		let Some(state) = (match status {
			"afk" => Some("idle"),
			"not-afk" => Some("active"),
			_ => None,
		}) else {
			continue;
		};
		let extends = open
			.as_ref()
			.is_some_and(|run| run.state == state && event.at <= run.end);
		if extends {
			let run = open.as_mut().expect("checked above");
			run.end = run.end.max(event.end());
		} else {
			if let Some(run) = open.take()
				&& let Some(record) = close_run(&run, "device.state", device)
			{
				records.push(record);
			}
			open = Some(Run {
				source_id: format!("afk:{}:{}", bucket.id, event.source_key()),
				start: event.at,
				end: event.end(),
				app: String::new(),
				title: String::new(),
				url: None,
				state,
			});
		}
	}
	if let Some(run) = open.take()
		&& let Some(record) = close_run(&run, "device.state", device)
	{
		records.push(record);
	}
	records
}

/// The browser name a web watcher's bucket id carries.
fn browser_name(token: &str) -> String {
	match token.to_lowercase().as_str() {
		"chrome" => "Google Chrome".into(),
		"chromium" => "Chromium".into(),
		"firefox" => "Firefox".into(),
		"edge" => "Microsoft Edge".into(),
		"safari" => "Safari".into(),
		"opera" => "Opera".into(),
		"brave" => "Brave".into(),
		"arc" => "Arc".into(),
		"vivaldi" => "Vivaldi".into(),
		other => {
			let mut name = String::new();
			for word in other.split([' ', '_']) {
				let mut characters = word.chars();
				if let Some(first) = characters.next() {
					name.push(first.to_uppercase().next().unwrap_or(first));
					name.push_str(characters.as_str());
					name.push(' ');
				}
			}
			name.trim_end().to_owned()
		}
	}
}

/// `aw-watcher-web` events: runs of the same page become one `apps.focus`
/// span carrying the URL. Private windows are skipped whole, since the URL
/// is at least as sensitive as the title.
fn web_records(bucket: &Bucket, events: &[AwEvent]) -> Vec<Record> {
	let device = device_for("activitywatch", &bucket.hostname);
	let app = browser_name(bucket.browser.as_deref().unwrap_or("browser"));
	let mut records = Vec::new();
	let mut open: Option<Run> = None;
	for event in events {
		let url = event
			.data
			.get("url")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let title = event
			.data
			.get("title")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		if exclusions().excludes_title(Some(&title)) {
			continue;
		}
		let extends = open.as_ref().is_some_and(|run| {
			run.url.as_deref() == Some(url.as_str()) && run.title == title && event.at <= run.end
		});
		if extends {
			let run = open.as_mut().expect("checked above");
			run.end = run.end.max(event.end());
		} else {
			if let Some(run) = open.take()
				&& let Some(record) = close_run(&run, "apps.focus", device)
			{
				records.push(record);
			}
			open = Some(Run {
				source_id: format!("web:{}:{}", bucket.id, event.source_key()),
				start: event.at,
				end: event.end(),
				app: app.clone(),
				title,
				url: Some(url),
				state: "",
			});
		}
	}
	if let Some(run) = open.take()
		&& let Some(record) = close_run(&run, "apps.focus", device)
	{
		records.push(record);
	}
	records
}

impl AwEvent {
	/// What makes this event unique when it has no id.
	fn source_key(&self) -> String {
		match self.id {
			Some(id) => id.to_string(),
			None => self.at.as_millisecond().to_string(),
		}
	}
}
