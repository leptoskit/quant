//! The phone side of Quant.
//!
//! Capture code on a phone (health data, motion, location, microphone)
//! writes into a [`Spool`], which implements [`Sink`] by storing everything
//! in a small encrypted database on the device. An [`Uploader`] drains the
//! spool to the desktop daemon over pinned TLS whenever it can, deleting
//! each item once the vault has it, so the phone keeps only what has not
//! been delivered yet.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

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
use serde::Deserialize;
use serde::Serialize;

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
		Ok(Pending {
			records: self.store.oldest_records(usize::MAX)?.len(),
			batches: self.store.oldest_series(usize::MAX)?.len(),
		})
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
		self.store
			.insert_records(&[record])
			.map(|_| ())
			.map_err(SinkError::from_display)
	}

	fn series(&self, batch: SeriesBatch) -> Result<(), SinkError> {
		self.store
			.insert_series(&batch)
			.map(|_| ())
			.map_err(SinkError::from_display)
	}

	fn media(&self, _stream: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError> {
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

/// Delivers a spool to the vault.
pub struct Uploader<'a> {
	spool: &'a Spool,
	client: &'a Client,
	/// Records per request.
	pub record_batch: usize,
	/// Series batches per request.
	pub series_batch: usize,
}

impl<'a> Uploader<'a> {
	/// An uploader for `spool` through `client`, which must hold a device
	/// grant with write scopes for what the phone captures.
	pub fn new(spool: &'a Spool, client: &'a Client) -> Self {
		Self {
			spool,
			client,
			record_batch: 200,
			series_batch: 500,
		}
	}

	/// Deliver everything waiting. Items are deleted locally only after the
	/// vault confirms them, so a dropped connection loses nothing, and
	/// resending is harmless because the vault deduplicates.
	pub async fn drain(&self) -> Result<Delivered, DrainError> {
		let mut delivered = Delivered::default();
		let throttled = |error: &ClientError| error.code() == Some("rate_limited");
		loop {
			let batches = self.spool.store.oldest_series(self.series_batch)?;
			if batches.is_empty() {
				break;
			}
			let (rows, batches): (Vec<i64>, Vec<SeriesBatch>) = batches.into_iter().unzip();
			match self.client.ingest_series(&IngestSeries { batches }).await {
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
			let records = self.spool.store.oldest_records(self.record_batch)?;
			if records.is_empty() {
				break;
			}
			let (rows, records): (Vec<i64>, Vec<Record>) = records.into_iter().unzip();
			let mut sent_media = Vec::new();
			for record in &records {
				let Some(media) = &record.media else { continue };
				let bytes = match self.spool.media.get(&media.hash) {
					Ok(bytes) => bytes,
					// Already delivered with an earlier record.
					Err(StoreError::NotFound) => continue,
					Err(error) => return Err(error.into()),
				};
				match self
					.client
					.ingest_media(&record.stream, &media.mime, bytes)
					.await
				{
					Ok(_) => sent_media.push(media.hash.clone()),
					Err(error) if throttled(&error) => {
						delivered.throttled = true;
						return Ok(delivered);
					}
					Err(error) => return Err(DrainError::Remote(error)),
				}
			}
			match self
				.client
				.ingest_records(&IngestRecords {
					records,
					device_name: None,
					platform: Some(std::env::consts::OS.into()),
				})
				.await
			{
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
