//! Imports the owner's existing history from other tools into a Quant vault.
//!
//! Quant's own capture starts the moment the daemon does, but the owner may
//! have years of history in Screenpipe (a local SQLite database) or
//! `ActivityWatch` (a local REST server, or an exported JSON file). The
//! importers read those sources and write through the ordinary signed ingest
//! path as a paired client with write scopes for the families they touch;
//! they never open the vault database directly.
//!
//! Every record id is derived deterministically from the source, the source
//! event's id, and the event time, so re-running an import never duplicates
//! anything: the vault stores each id once. A per-source watermark, kept in
//! the client's config directory, lets `--follow` pull only new data.
//!
//! Mappings:
//!
//! - Screenpipe frames → `apps.focus` spans (consecutive frames with the same
//!   app and window; a gap of more than twice the median frame interval, capped
//!   at 60 s, ends a span) and `screen.frame` records for the OCR text of each
//!   frame, deduplicated against the previous frame's text. Audio
//!   transcriptions → `audio.transcript`.
//! - `ActivityWatch` `aw-watcher-window` events → `apps.focus`,
//!   `aw-watcher-afk` events → `device.state` idle and active spans, and
//!   `aw-watcher-web` events → `apps.focus` spans carrying the page URL. Events
//!   from other hosts keep their hostname as the device label.

mod activitywatch;
mod screenpipe;
mod watermark;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub use activitywatch::ActivityWatchOptions;
use jiff::Timestamp;
use quant_api::IngestRecords;
use quant_api::Ingested;
use quant_client::Client;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::RecordId;
pub use screenpipe::ScreenpipeOptions;
use tokio::sync::mpsc;

/// How often `--follow` polls by default.
pub const DEFAULT_EVERY: Duration = Duration::from_mins(5);

/// Records per ingest request.
const BATCH: usize = 500;

/// Something an import could not do.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
	/// The filesystem refused.
	#[error("{0}")]
	Io(#[from] std::io::Error),
	/// The source database refused.
	#[error("{0}")]
	Sql(#[from] rusqlite::Error),
	/// The source did not decode.
	#[error("{0}")]
	Json(#[from] serde_json::Error),
	/// A time did not parse.
	#[error("{0}")]
	Time(#[from] jiff::Error),
	/// The daemon refused or was unreachable.
	#[error("{0}")]
	Remote(#[from] quant_client::ClientError),
	/// Anything else, in plain words.
	#[error("{0}")]
	Other(String),
}

impl ImportError {
	/// An error from anything displayable.
	pub fn message(error: impl std::fmt::Display) -> Self {
		Self::Other(error.to_string())
	}
}

/// What one import pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportReport {
	/// Records seen per stream, including ones already present. A dry run
	/// reports only these.
	pub counts: BTreeMap<String, usize>,
	/// Records newly stored in the vault.
	pub stored: usize,
	/// Records the vault already had.
	pub duplicate: usize,
	/// Earliest event seen, when any.
	pub from: Option<Timestamp>,
	/// Latest event seen, when any.
	pub to: Option<Timestamp>,
}

impl ImportReport {
	fn note(&mut self, record: &Record) {
		*self
			.counts
			.entry(record.stream.as_str().to_owned())
			.or_default() += 1;
		let end = record.end_or_start();
		self.from = Some(match self.from {
			Some(from) => from.min(record.start),
			None => record.start,
		});
		self.to = Some(match self.to {
			Some(to) => to.max(end),
			None => end,
		});
	}

	/// Fold a later pass into this one.
	pub fn add(&mut self, later: ImportReport) {
		for (stream, count) in later.counts {
			*self.counts.entry(stream).or_default() += count;
		}
		self.stored += later.stored;
		self.duplicate += later.duplicate;
		if let Some(from) = later.from {
			self.from = Some(self.from.map_or(from, |own| own.min(from)));
		}
		if let Some(to) = later.to {
			self.to = Some(self.to.map_or(to, |own| own.max(to)));
		}
	}
}

/// A batch of records from one source device, on its way to the vault.
pub(crate) struct Item {
	/// The device's label, sent with the ingest request and remembered with
	/// the device each record already names.
	pub(crate) label: String,
	/// At most [`BATCH`] records, ordered as the source produced them.
	pub(crate) records: Vec<Record>,
}

/// Watches the running report after every batch.
type Progress = Box<dyn FnMut(&ImportReport) + Send>;

/// An import run against one vault. Each importer keeps its watermark in
/// `state_dir`, so re-running pulls only what is new.
pub struct Importer {
	client: Client,
	state_dir: PathBuf,
	vault_id: String,
	/// Called after every batch the vault accepts, with the running report.
	progress: Option<Progress>,
}

impl Importer {
	/// An importer for the vault `client` is paired with. Asks the daemon
	/// which vault it is, so watermarks never leak between vaults.
	pub async fn new(client: Client, state_dir: impl Into<PathBuf>) -> Result<Self, ImportError> {
		let vault_id = client.hello().await?.vault_id;
		Ok(Self {
			client,
			state_dir: state_dir.into(),
			vault_id,
			progress: None,
		})
	}

	/// Watch the running report after every batch. A dry run still reports.
	pub fn on_progress(&mut self, callback: impl FnMut(&ImportReport) + Send + 'static) {
		self.progress = Some(Box::new(callback));
	}

	fn report_progress(&mut self, report: &ImportReport) {
		if let Some(progress) = &mut self.progress {
			progress(report);
		}
	}

	/// Import Screenpipe history, once or following the database.
	pub async fn screenpipe(
		&mut self,
		options: ScreenpipeOptions,
	) -> Result<ImportReport, ImportError> {
		if !options.follow {
			return self.screenpipe_once(&options).await;
		}
		let mut total = ImportReport::default();
		loop {
			total.add(self.screenpipe_once(&options).await?);
			tokio::select! {
				() = tokio::time::sleep(options.every) => {}
				_ = tokio::signal::ctrl_c() => break,
			}
		}
		Ok(total)
	}

	/// Import `ActivityWatch` history, once or following the server.
	pub async fn activitywatch(
		&mut self,
		options: ActivityWatchOptions,
	) -> Result<ImportReport, ImportError> {
		if !options.follow {
			return self.activitywatch_once(&options).await;
		}
		let mut total = ImportReport::default();
		loop {
			total.add(self.activitywatch_once(&options).await?);
			tokio::select! {
				() = tokio::time::sleep(options.every) => {}
				_ = tokio::signal::ctrl_c() => break,
			}
		}
		Ok(total)
	}

	/// Count everything the source sends, and store it through the signed
	/// ingest path unless this is a dry run. Batches are grouped by device,
	/// since each ingest request names one device.
	async fn consume(
		&mut self,
		mut receiver: mpsc::UnboundedReceiver<Item>,
		dry_run: bool,
	) -> Result<ImportReport, ImportError> {
		let mut report = ImportReport::default();
		while let Some(Item { label, records }) = receiver.recv().await {
			for record in &records {
				report.note(record);
			}
			if !dry_run && !records.is_empty() {
				let Ingested { stored, duplicate } = self
					.client
					.ingest_records(&IngestRecords {
						records,
						device_name: Some(label),
						platform: Some(std::env::consts::OS.into()),
					})
					.await?;
				report.stored += stored;
				report.duplicate += duplicate;
			}
			self.report_progress(&report);
		}
		Ok(report)
	}
}

/// Where a source's watermark lives: `<state_dir>/import/<kind>-<hash>.json`.
pub(crate) fn state_file(state_dir: &Path, kind: &str, location: &str, vault_id: &str) -> PathBuf {
	let mut hasher = blake3::Hasher::new();
	hasher.update(kind.as_bytes());
	hasher.update(&[0]);
	hasher.update(location.as_bytes());
	hasher.update(&[0]);
	hasher.update(vault_id.as_bytes());
	let hash = hasher.finalize();
	state_dir.join("import").join(format!(
		"{kind}-{}.json",
		hex::encode(&hash.as_bytes()[..8])
	))
}

/// A deterministic record id: a `UUIDv7` whose timestamp bits come from the
/// event time and whose remaining bits come from a hash of the source, the
/// source event's id, and the stream, so importing the same event twice
/// stores it once.
pub fn derived_id(kind: &str, source_id: &str, at: Timestamp, stream: &str) -> RecordId {
	let mut hasher = blake3::Hasher::new();
	hasher.update(kind.as_bytes());
	hasher.update(&[0]);
	hasher.update(source_id.as_bytes());
	hasher.update(&[0]);
	hasher.update(stream.as_bytes());
	let hash = hasher.finalize();
	let millis = u64::try_from(at.as_millisecond())
		.unwrap_or(0)
		.min((1u64 << 48) - 1);
	let mut bytes = [0u8; 16];
	bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
	// Version 7, then 12 bits of hash, then the RFC 4122 variant and 62
	// more bits of hash.
	bytes[6] = 0x70 | (hash.as_bytes()[0] & 0x0F);
	bytes[7] = hash.as_bytes()[1];
	bytes[8] = 0x80 | (hash.as_bytes()[2] >> 2);
	bytes[9..].copy_from_slice(&hash.as_bytes()[3..10]);
	RecordId(uuid::Uuid::from_bytes(bytes))
}

/// The device imported events from one host are attributed to. Deterministic,
/// so re-imports keep records attached to the same device.
pub fn device_for(kind: &str, label: &str) -> DeviceId {
	let epoch = Timestamp::from_second(0).expect("the epoch is a valid timestamp");
	DeviceId(derived_id(kind, &format!("device:{label}"), epoch, "device").0)
}

/// This machine's short hostname, the way the owner would recognise it.
pub fn hostname() -> String {
	std::process::Command::new("hostname")
		.arg("-s")
		.output()
		.ok()
		.and_then(|output| String::from_utf8(output.stdout).ok())
		.map(|name| name.trim_matches(['.', '\n']).to_owned())
		.filter(|name| !name.is_empty())
		.unwrap_or_else(|| "this machine".into())
}

/// The exclusion rules importers share with native capture: password manager
/// apps by name, private browser windows by title.
pub(crate) fn exclusions() -> quant_capture_macos::privacy::Exclusions {
	quant_capture_macos::privacy::Exclusions::new(
		&[],
		&quant_capture_macos::default_excluded_title_patterns(),
	)
	.with_app_names(&quant_capture_macos::default_excluded_app_names())
}

/// Clean a title, URL, or captured text the way served text is cleaned:
/// invisible characters out, markup defanged.
pub(crate) fn clean(text: &str) -> String {
	quant_access::redact::sanitize(text).into_owned()
}

/// RFC 3339, an ISO date-time with a space, a date-time, or a date.
pub(crate) fn parse_text_ts(text: &str) -> Option<Timestamp> {
	if let Ok(at) = text.trim().parse::<Timestamp>() {
		return Some(at);
	}
	if let Ok(zoned) = text.trim().parse::<jiff::Zoned>() {
		return Some(zoned.timestamp());
	}
	let swapped = text.trim().replace(' ', "T");
	if let Ok(at) = swapped.parse::<Timestamp>() {
		return Some(at);
	}
	if let Ok(datetime) = swapped.parse::<jiff::civil::DateTime>() {
		return datetime
			.to_zoned(jiff::tz::TimeZone::UTC)
			.ok()
			.map(|z| z.timestamp());
	}
	swapped
		.parse::<jiff::civil::Date>()
		.ok()
		.map(|date| date.to_datetime(jiff::civil::Time::midnight()))
		.and_then(|datetime| datetime.to_zoned(jiff::tz::TimeZone::UTC).ok())
		.map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ids_are_uuidv7_ordered_by_event_time() {
		let early = derived_id(
			"screenpipe",
			"frame:1",
			Timestamp::from_second(1_000).unwrap(),
			"apps.focus",
		);
		let late = derived_id(
			"screenpipe",
			"frame:1",
			Timestamp::from_second(2_000).unwrap(),
			"apps.focus",
		);
		assert!(early.0 < late.0, "UUIDv7 sorts by event time");
		let shown = late.0.to_string();
		assert_eq!(&shown[14..15], "7", "version 7: {shown}");
		assert!(
			matches!(&shown[19..20], "8" | "9" | "a" | "b"),
			"RFC 4122 variant: {shown}"
		);
	}

	#[test]
	fn ids_are_deterministic_and_distinct_per_stream() {
		let at = Timestamp::from_second(1_700_000_000).unwrap();
		let a = derived_id("screenpipe", "frame:7", at, "apps.focus");
		let b = derived_id("screenpipe", "frame:7", at, "screen.frame");
		assert_eq!(a, derived_id("screenpipe", "frame:7", at, "apps.focus"));
		assert_ne!(a, b);
		assert_ne!(a, derived_id("activitywatch", "frame:7", at, "apps.focus"));
	}

	#[test]
	fn devices_follow_their_labels() {
		let here = device_for("screenpipe", "mac-mini");
		let there = device_for("screenpipe", "macbook");
		assert_eq!(here, device_for("screenpipe", "mac-mini"));
		assert_ne!(here, there);
	}
}
