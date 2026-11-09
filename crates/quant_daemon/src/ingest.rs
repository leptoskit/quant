//! Write routes: devices and agents sending data in.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::Timestamp;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_api::Ingested;
use quant_core::MediaRef;
use quant_core::StreamId;

use crate::authn::Caller;
use crate::authn::Signed;
use crate::authn::SignedBytes;
use crate::error::ApiResult;
use crate::error::Error;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

/// Most records or batches in one request.
pub const MAX_BATCH: usize = 5_000;

/// Refuse the whole request if any stream is outside the grant: a device
/// cannot smuggle health data in alongside screen data it may write.
fn authorize(state: &State, caller: &Caller, streams: &[&StreamId], action: &str) -> ApiResult<()> {
	let now = Timestamp::now();
	for stream in streams {
		if let Err(denied) = caller.grant.authorize_write(stream, now) {
			state.vault.store.audit(AuditEvent::new(
				caller.actor(),
				action,
				stream.as_str(),
				Outcome::Denied { reason: denied },
			))?;
			return Err(Error::Forbidden(denied));
		}
	}
	Ok(())
}

fn targets(streams: &[&StreamId]) -> String {
	let mut names: Vec<&str> = streams.iter().map(|s| s.as_str()).collect();
	names.sort_unstable();
	names.dedup();
	names.join(",")
}

/// `POST /v1/ingest/records`
pub async fn records(
	AxumState(state): Shared,
	request: Signed<IngestRecords>,
) -> ApiResult<Json<Ingested>> {
	let caller = &request.caller;
	let mut body = request.body;
	if body.records.len() > MAX_BATCH {
		return Err(Error::Invalid(format!(
			"at most {MAX_BATCH} records per request"
		)));
	}
	for record in &mut body.records {
		if record.stream.as_str() == "audio.ambient" {
			let duration = record.end.map(|end| end.duration_since(record.start));
			if !duration.is_some_and(|duration| {
				duration > jiff::SignedDuration::ZERO
					&& duration <= jiff::SignedDuration::from_hours(1)
			}) || !record
				.media
				.as_ref()
				.is_some_and(|media| media.mime == "audio/mp4" && media.size > 0)
			{
				return Err(Error::Invalid(
					"ambient audio needs an AAC m4a and a duration of at most one hour".into(),
				));
			}
			// Only the daemon may mark transcription complete. A retried insert
			// is ignored and leaves any completed work alone.
			record
				.data
				.insert("transcript_status".into(), "pending".into());
			record.text = None;
		}
	}
	let streams: Vec<&StreamId> = body.records.iter().map(|r| &r.stream).collect();
	authorize(&state, caller, &streams, "ingest.records")?;
	// Claims are not records: nothing may ingest into the stream that
	// serves them.
	if body
		.records
		.iter()
		.any(|record| record.stream.as_str() == crate::memory::PROFILE)
	{
		return Err(Error::Forbidden(quant_access::Denied::OutOfScope));
	}
	for record in &body.records {
		if let Some(media) = &record.media
			&& !state.vault.store.has_media(&media.hash)?
		{
			return Err(Error::Invalid(format!(
				"record {} references media that was not uploaded",
				record.id
			)));
		}
	}
	if let Some(device) = body.records.first().map(|r| r.device) {
		let name = body.device_name.as_deref().unwrap_or(&caller.client.name);
		state.vault.store.touch_device(
			device,
			name,
			body.platform.as_deref().unwrap_or("unknown"),
		)?;
	}
	let report = state.vault.store.insert_records(&body.records)?;
	state.vault.store.audit(AuditEvent::new(
		caller.actor(),
		"ingest.records",
		targets(&streams),
		Outcome::Allowed {
			records: report.stored as u64,
			bytes: 0,
		},
	))?;
	Ok(Json(Ingested {
		stored: report.stored,
		duplicate: report.duplicate,
	}))
}

/// `POST /v1/ingest/series`
pub async fn series(
	AxumState(state): Shared,
	request: Signed<IngestSeries>,
) -> ApiResult<Json<Ingested>> {
	let caller = &request.caller;
	let body = request.body;
	if body.batches.len() > MAX_BATCH {
		return Err(Error::Invalid(format!(
			"at most {MAX_BATCH} batches per request"
		)));
	}
	let streams: Vec<&StreamId> = body.batches.iter().map(|b| &b.stream).collect();
	authorize(&state, caller, &streams, "ingest.series")?;
	for batch in &body.batches {
		batch
			.validate()
			.map_err(|e| Error::Invalid(e.to_string()))?;
	}
	let mut answer = Ingested::default();
	let mut rows = 0;
	for batch in &body.batches {
		if state.vault.store.insert_series(batch)? {
			answer.stored += 1;
			rows += batch.len() as u64;
		} else {
			answer.duplicate += 1;
		}
	}
	state.vault.store.audit(AuditEvent::new(
		caller.actor(),
		"ingest.series",
		targets(&streams),
		Outcome::Allowed {
			records: rows,
			bytes: 0,
		},
	))?;
	Ok(Json(answer))
}

/// `POST /v1/ingest/media?stream=<stream>&mime=<type>`
pub async fn media(AxumState(state): Shared, request: SignedBytes) -> ApiResult<Json<MediaRef>> {
	let caller = &request.caller;
	let mut stream = None;
	let mut mime = None;
	for pair in request.query.as_deref().unwrap_or_default().split('&') {
		match pair.split_once('=') {
			Some(("stream", value)) => stream = Some(value.to_owned()),
			Some(("mime", value)) => mime = Some(value.replace("%2F", "/").replace("%2f", "/")),
			_ => {}
		}
	}
	let stream: StreamId = stream
		.ok_or_else(|| Error::Invalid("media needs ?stream=".into()))?
		.parse()
		.map_err(|_| Error::Invalid("unknown stream".into()))?;
	let mime = mime.unwrap_or_else(|| "application/octet-stream".into());
	if !mime
		.bytes()
		.all(|b| b.is_ascii_alphanumeric() || b"/+.-".contains(&b))
	{
		return Err(Error::Invalid("bad media type".into()));
	}
	authorize(&state, caller, &[&stream], "ingest.media")?;
	let retention = state
		.spec(&stream)
		.and_then(|spec| spec.media_retention_hours);
	let media = state.vault.put_media(&request.body, &mime, retention)?;
	state.vault.store.audit(AuditEvent::new(
		caller.actor(),
		"ingest.media",
		stream.as_str(),
		Outcome::Allowed {
			records: 1,
			bytes: media.size,
		},
	))?;
	Ok(Json(media))
}
