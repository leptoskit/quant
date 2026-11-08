//! On-device learning inside the daemon: the owner's settings, and the
//! schedule that runs the embedding backfill and the daily extraction job
//! after derivation, so the day's `memory.daily` exists before anything
//! reads it.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_learn::LearnConfig;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::StoreError;

use crate::authn::Owner;
use crate::error::ApiResult;
use crate::error::Error;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// The vault meta key the learning settings live under.
const CONFIG: &str = "learn.config";

/// The stored learning settings, or nothing configured at all.
pub fn config(state: &State) -> Result<LearnConfig, StoreError> {
	Ok(state
		.vault
		.store
		.meta(CONFIG)?
		.and_then(|json| serde_json::from_str(&json).ok())
		.unwrap_or_default())
}

/// The current embedding space, when an embedding model is configured.
pub fn space(state: &State) -> Result<Option<String>, StoreError> {
	Ok(config(state)?.space())
}

/// `GET /v1/owner/learn`
pub async fn learn_config(AxumState(state): Shared, _owner: Owner) -> ApiResult<Json<LearnConfig>> {
	Ok(Json(config(&state)?))
}

/// `POST /v1/owner/learn`
pub async fn set_learn_config(
	AxumState(state): Shared,
	_owner: Owner,
	Json(config): Json<LearnConfig>,
) -> ApiResult<Json<LearnConfig>> {
	config.validate().map_err(Error::Invalid)?;
	let json = serde_json::to_string(&config).map_err(|e| Error::Invalid(e.to_string()))?;
	state.vault.store.set_meta(CONFIG, &json)?;
	state.vault.store.audit(
		quant_access::audit::AuditEvent::new(
			quant_access::audit::Actor::Owner,
			"learn.configure",
			"",
			quant_access::audit::Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		)
		.detail(serde_json::to_value(&config).unwrap_or_default()),
	)?;
	Ok(Json(config))
}

/// `POST /v1/owner/learn/off`
pub async fn learn_off(AxumState(state): Shared, _owner: Owner) -> ApiResult<Json<()>> {
	state.vault.store.remove_meta(CONFIG)?;
	state
		.vault
		.store
		.audit(quant_access::audit::AuditEvent::new(
			quant_access::audit::Actor::Owner,
			"learn.off",
			"",
			quant_access::audit::Outcome::Allowed {
				records: 0,
				bytes: 0,
			},
		))?;
	Ok(Json(()))
}

/// One round of the learning jobs, after derivation: backfill embeddings,
/// and run the daily extraction once the day's `memory.daily` exists and
/// the day has not been processed yet.
pub async fn tick(state: &Arc<State>) -> Result<(), StoreError> {
	let config = config(state)?;
	let tz = TimeZone::system();
	let now = Timestamp::now();

	if let Some(space) = config.space() {
		let report = quant_learn::backfill::backfill(&state.vault.store, &config, &space).await?;
		if report.embedded > 0 {
			tracing::debug!(embedded = report.embedded, "backfilled embeddings");
		}
		if report.unavailable {
			tracing::warn!("the embedding endpoint is unavailable; backfill will retry");
		}
	}

	if config.extract.is_some() {
		let today = now.to_zoned(tz.clone()).date().to_string();
		if state
			.vault
			.store
			.meta(quant_learn::extract::DAY_KEY)?
			.as_deref()
			!= Some(today.as_str())
			&& todays_daily_exists(&state.vault.store, now, &tz)?
		{
			let report =
				quant_learn::extract::extract_daily(&state.vault.store, &config, now, &tz).await?;
			tracing::debug!(?report, "extracted claims");
		}
	}
	Ok(())
}

/// Whether today's `memory.daily` record exists yet.
fn todays_daily_exists(
	store: &quant_store::Store,
	now: Timestamp,
	tz: &TimeZone,
) -> Result<bool, StoreError> {
	let date = now.to_zoned(tz.clone()).date();
	let start = date
		.to_datetime(jiff::civil::Time::midnight())
		.to_zoned(tz.clone())
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	let end = start
		.tomorrow()
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	let range = TimeRange::new(start.timestamp(), end.timestamp())
		.map_err(|e| StoreError::Invalid(e.to_string()))?;
	let daily: StreamId = "memory.daily".parse().expect("built-in stream");
	Ok(!store
		.records(&RecordQuery::new(Selector::stream(daily), range).limit(1))?
		.is_empty())
}

/// Embed one query text for a search, normalised for comparison. Returns
/// the vector, or an error when the endpoint cannot be reached.
pub async fn embed_query(state: &State, terms: &str) -> Result<Vec<f32>, Error> {
	let config = config(state)?;
	let Some(embed) = config.embed else {
		return Err(Error::Invalid("no embedding endpoint is configured".into()));
	};
	let endpoint =
		quant_learn::endpoint::ModelEndpoint::parse(&embed.endpoint).map_err(Error::Invalid)?;
	let mut vectors = quant_learn::client::embed(&endpoint, &embed.model, &[terms])
		.await
		.map_err(|error| Error::Internal(error.0))?;
	let mut vector = vectors.pop().unwrap_or_default();
	if vector.len() != embed.dims {
		return Err(Error::Internal(
			"the embedding endpoint returned the wrong dimensions".into(),
		));
	}
	quant_store::l2_normalize(&mut vector);
	Ok(vector)
}
