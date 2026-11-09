//! The phone side of Quant.
//!
//! Capture code on a phone (health data, motion, location, microphone)
//! writes into a [`Spool`], which implements [`Sink`] by storing everything
//! in a small encrypted database on the device. An [`Uploader`] drains the
//! spool to the desktop daemon over pinned TLS whenever it can, deleting
//! each item once the vault has it, so the phone keeps only what has not
//! been delivered yet.

mod ambient;
mod resources;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

pub use ambient::AmbientAudio;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_client::Client;
use quant_client::ClientError;
use quant_core::MediaRef;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::SinkError;
use quant_core::StreamId;
use quant_crypto::VaultKey;
use quant_store::MediaStore;
use quant_store::Store;
use quant_store::StoreError;
pub use resources::MAX_MEDIA_BYTES;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;

/// Undelivered captures, encrypted at rest.
pub struct Spool {
	store: Store,
	media: MediaStore,
	status: Mutex<BTreeMap<String, SourceStatus>>,
}

/// How one capture source on the device is doing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStatus {
	/// Whether it is running.
	pub running: bool,
	/// Why not, or a recent problem.
	pub detail: Option<String>,
}

/// How much is waiting to be delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
	/// Records.
	pub records: usize,
	/// Series batches.
	pub batches: usize,
}

impl Spool {
	/// Open (or create) a spool in `dir`, encrypted with `key`. On a phone
	/// the key lives in the platform keystore.
	pub fn open(dir: &Path, key: &VaultKey) -> Result<Self, StoreError> {
		std::fs::create_dir_all(dir.join("media"))?;
		Ok(Self {
			store: Store::open(&dir.join("spool.db"), key)?,
			media: MediaStore::new(dir.join("media"), key),
			status: Mutex::new(BTreeMap::new()),
		})
	}

	/// What is waiting.
	pub fn pending(&self) -> Result<Pending, StoreError> {
		let (records, batches) = self.store.backlog_counts()?;
		Ok(Pending { records, batches })
	}

	/// How each capture source is doing.
	pub fn sources(&self) -> BTreeMap<String, SourceStatus> {
		self.status
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.clone()
	}
}

impl Sink for Spool {
	fn record(&self, record: Record) -> Result<(), SinkError> {
		crate::resources::check_record(&record)?;
		self.store
			.insert_records(&[record])
			.map(|_| ())
			.map_err(SinkError::from_display)
	}

	fn series(&self, batch: SeriesBatch) -> Result<(), SinkError> {
		crate::resources::check_series(&batch)?;
		self.store
			.insert_series(&batch)
			.map(|_| ())
			.map_err(SinkError::from_display)
	}

	fn media(&self, _stream: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError> {
		if bytes.len() > MAX_MEDIA_BYTES {
			return Err(SinkError::from_display(
				"phone media exceeds 16 MiB; use shorter segments",
			));
		}
		self.media.put(bytes, mime).map_err(SinkError::from_display)
	}

	fn status(&self, source: &str, running: bool, detail: Option<String>) {
		self.status
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.insert(source.to_owned(), SourceStatus { running, detail });
	}
}

/// What a drain delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
	/// Records delivered.
	pub records: usize,
	/// Series batches delivered.
	pub batches: usize,
	/// Media files delivered.
	pub media: usize,
	/// Stopped early because the grant's rate limit was reached; try again
	/// later.
	pub throttled: bool,
	/// Yielded to the next scheduled pass after reaching a local work budget.
	#[serde(default)]
	pub yielded: bool,
	/// Media bytes successfully sent, including uploads awaiting a record
	/// acknowledgement.
	#[serde(default)]
	pub media_bytes: u64,
}

/// Why a drain stopped.
#[derive(Debug)]
pub enum DrainError {
	/// The local spool failed.
	Spool(StoreError),
	/// The daemon refused or could not be reached. Nothing undelivered was
	/// deleted.
	Remote(ClientError),
}

impl std::fmt::Display for DrainError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			DrainError::Spool(error) => write!(f, "spool: {error}"),
			DrainError::Remote(error) => write!(f, "{error}"),
		}
	}
}

impl std::error::Error for DrainError {}

impl From<StoreError> for DrainError {
	fn from(error: StoreError) -> Self {
		DrainError::Spool(error)
	}
}

/// Work allowed in one upload burst, reduced by the caller when the phone
/// is hot or in low-power mode. Zero budgets pause the affected work.
#[derive(Clone, Copy, Debug)]
pub struct UploadBudget {
	/// Maximum records acknowledged in this pass.
	pub max_records: usize,
	/// Maximum sensor batches acknowledged in this pass.
	pub max_batches: usize,
	/// Soft media-byte budget; one larger item may make progress on its own.
	pub max_media_bytes: u64,
	/// Deadline including network waits. In-flight work is safely retryable.
	pub max_duration: Duration,
}

impl Default for UploadBudget {
	fn default() -> Self {
		Self {
			max_records: 128,
			max_batches: 32,
			max_media_bytes: 4 * 1024 * 1024,
			max_duration: Duration::from_secs(10),
		}
	}
}

/// Delivers a spool to the vault.
pub struct Uploader<'a> {
	spool: &'a Spool,
	client: &'a Client,
	/// Records per request, clamped to 1–32 to bound allocations.
	pub record_batch: usize,
	/// Series batches per request, clamped to 1–8 to bound allocations.
	pub series_batch: usize,
}

impl<'a> Uploader<'a> {
	/// An uploader for `spool` through `client`, which must hold a device
	/// grant with write scopes for what the phone captures.
	pub fn new(spool: &'a Spool, client: &'a Client) -> Self {
		Self {
			spool,
			client,
			record_batch: 32,
			series_batch: 8,
		}
	}

	/// Upload one bounded burst using the default budget. The caller schedules
	/// the next pass later when `yielded` is true; it must not busy-loop.
	/// Unacknowledged data stays encrypted in the spool, including on timeout.
	pub async fn drain(&self) -> Result<Delivered, DrainError> {
		self.drain_with_budget(UploadBudget::default()).await
	}

	/// Upload a burst within the supplied battery/thermal-aware work budget.
	/// Values above the default ceilings are clamped; callers can reduce them.
	pub async fn drain_with_budget(&self, budget: UploadBudget) -> Result<Delivered, DrainError> {
		let ceiling = UploadBudget::default();
		let budget = UploadBudget {
			max_records: budget.max_records.min(ceiling.max_records),
			max_batches: budget.max_batches.min(ceiling.max_batches),
			max_media_bytes: budget.max_media_bytes.min(ceiling.max_media_bytes),
			max_duration: budget.max_duration.min(ceiling.max_duration),
		};
		let deadline = Instant::now() + budget.max_duration;

		let mut delivered = Delivered::default();
		let throttled = |error: &ClientError| error.code() == Some("rate_limited");
		loop {
			if Instant::now() >= deadline || delivered.batches >= budget.max_batches {
				delivered.yielded = true;
				break;
			}
			let batches = self.spool.store.oldest_series_bounded(
				self.series_batch
					.clamp(1, 8)
					.min(budget.max_batches - delivered.batches),
				resources::MAX_SERIES_BYTES,
			)?;
			if batches.is_empty() {
				break;
			}
			let (rows, batches): (Vec<i64>, Vec<SeriesBatch>) = batches.into_iter().unzip();
			let result = tokio::time::timeout_at(
				deadline,
				self.client.ingest_series(&IngestSeries { batches }),
			)
			.await;
			let Ok(result) = result else {
				delivered.yielded = true;
				return Ok(delivered);
			};
			match result {
				Ok(_) => {
					self.spool.store.delete_series_rows(&rows)?;
					delivered.batches += rows.len();
				}
				Err(error) if throttled(&error) => {
					delivered.throttled = true;
					return Ok(delivered);
				}
				Err(error) => return Err(DrainError::Remote(error)),
			}
		}
		loop {
			if Instant::now() >= deadline || delivered.records >= budget.max_records {
				delivered.yielded = true;
				break;
			}
			let records = self.spool.store.oldest_records_bounded(
				self.record_batch
					.clamp(1, 32)
					.min(budget.max_records - delivered.records),
				resources::MAX_RECORD_BYTES,
			)?;
			if records.is_empty() {
				break;
			}
			// Commit at most one media item per batch. Otherwise a byte-budget
			// boundary could replay an unfinished multi-file batch forever.
			let second_media = records
				.iter()
				.enumerate()
				.filter(|(_, (_, r))| r.media.is_some())
				.nth(1)
				.map(|(index, _)| index);
			let take = second_media.unwrap_or(records.len());
			let (rows, records): (Vec<i64>, Vec<Record>) = records.into_iter().take(take).unzip();
			let mut sent_media = Vec::new();
			for record in &records {
				let Some(media) = &record.media else { continue };
				if budget.max_media_bytes == 0
					|| (delivered.media_bytes > 0
						&& media.size
							> budget.max_media_bytes.saturating_sub(delivered.media_bytes))
				{
					delivered.yielded = true;
					return Ok(delivered);
				}
				let bytes = match self.spool.media.get_bounded(&media.hash, MAX_MEDIA_BYTES) {
					Ok(bytes) => bytes,
					// Already delivered with an earlier record.
					Err(StoreError::NotFound) => continue,
					Err(error) => return Err(error.into()),
				};
				let size = bytes.len() as u64;
				// Stored references can be old or inconsistent. Charge the
				// actual bytes, rather than trusting their
				// declared media size.
				if delivered.media_bytes > 0
					&& size > budget.max_media_bytes.saturating_sub(delivered.media_bytes)
				{
					delivered.yielded = true;
					return Ok(delivered);
				}
				let result = tokio::time::timeout_at(
					deadline,
					self.client.ingest_media(&record.stream, &media.mime, bytes),
				)
				.await;
				let Ok(result) = result else {
					delivered.yielded = true;
					return Ok(delivered);
				};
				match result {
					Ok(_) => {
						sent_media.push(media.hash.clone());
						delivered.media_bytes += size;
					}
					Err(error) if throttled(&error) => {
						delivered.throttled = true;
						return Ok(delivered);
					}
					Err(error) => return Err(DrainError::Remote(error)),
				}
			}
			let result = tokio::time::timeout_at(
				deadline,
				self.client.ingest_records(&IngestRecords {
					records,
					device_name: None,
					platform: Some(std::env::consts::OS.into()),
				}),
			)
			.await;
			let Ok(result) = result else {
				delivered.yielded = true;
				return Ok(delivered);
			};
			match result {
				Ok(_) => {
					self.spool.store.delete_record_rows(&rows)?;
					delivered.records += rows.len();
					for hash in sent_media {
						if self.spool.store.media_owner(&hash)?.is_none() {
							self.spool.media.remove(&hash)?;
							delivered.media += 1;
						}
					}
				}
				Err(error) if throttled(&error) => {
					delivered.throttled = true;
					return Ok(delivered);
				}
				Err(error) => return Err(DrainError::Remote(error)),
			}
		}
		Ok(delivered)
	}
}
