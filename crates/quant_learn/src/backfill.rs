//! Filling the embeddings table: at most 200 un-embedded texts per run,
//! oldest first. An unavailable endpoint stops the run; the next run
//! resumes where this one left off, because nothing marks the remaining
//! texts as done.

use quant_store::Store;
use quant_store::StoreError;

use crate::client;
use crate::config;
use crate::config::LearnConfig;
use crate::endpoint::ModelEndpoint;

/// How many texts one backfill run embeds at most.
pub const BACKFILL_LIMIT: usize = 200;

/// How many texts one embedding call carries.
const BATCH: usize = 16;

/// What a backfill run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackfillReport {
	/// Texts embedded.
	pub embedded: usize,
	/// Whether the endpoint could not be reached and the run stopped early.
	/// The remaining texts are tried again next run.
	pub unavailable: bool,
}

/// Embed at most [`BACKFILL_LIMIT`] un-embedded texts for `space`, oldest
/// first. The endpoint is reachable only on the machine's loopback; when it
/// is not, the run stops and reports it.
pub async fn backfill(
	store: &Store,
	config: &LearnConfig,
	space: &str,
) -> Result<BackfillReport, StoreError> {
	let mut report = BackfillReport::default();
	let Some(embed) = &config.embed else {
		return Ok(report);
	};
	let endpoint = ModelEndpoint::parse(&embed.endpoint).map_err(StoreError::Invalid)?;
	let selector = config
		.streams
		.as_deref()
		.map_or_else(config::default_selector, |streams| {
			config::learn_selector(streams).expect("settings are validated when stored")
		});
	let pending = store.pending_embeddings(space, &selector, BACKFILL_LIMIT)?;
	for chunk in pending.chunks(BATCH) {
		let texts: Vec<&str> = chunk
			.iter()
			.map(|(_, text)| config::truncate(text))
			.collect();
		let Ok(vectors) = client::embed(&endpoint, &embed.model, &texts).await else {
			report.unavailable = true;
			return Ok(report);
		};
		if vectors.len() != texts.len() {
			report.unavailable = true;
			return Ok(report);
		}
		for ((rowid, _), mut vector) in chunk.iter().zip(vectors) {
			if vector.len() != embed.dims {
				report.unavailable = true;
				return Ok(report);
			}
			quant_store::l2_normalize(&mut vector);
			store.set_embedding(space, *rowid, &quant_store::vector_to_bytes(&vector))?;
			report.embedded += 1;
		}
	}
	Ok(report)
}
