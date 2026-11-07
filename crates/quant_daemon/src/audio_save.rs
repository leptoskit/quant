//! Saving a moment from the rolling audio buffer.
//!
//! `/v1/audio/save` (an approved client with `audio:write`) and
//! `/v1/owner/audio/save` (an owner session) both land here. The daemon
//! closes the chunks still being recorded when the range reaches into the
//! present, joins every overlapping buffer chunk into one AAC file, and
//! stores it as an `audio.clip` record whose media never expires. The audio
//! itself is never returned — which is exactly why write scope is enough —
//! and the clip is transcribed once, in the background.

use std::sync::Arc;

use axum::Json;
use axum::extract::State as AxumState;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_api::SaveAudio;
use quant_api::SavedClip;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::RecordQuery;
use quant_store::Selector;
use serde_json::json;

use crate::State;
use crate::authn::Signed;
use crate::error::ApiResult;
use crate::error::Error;

type Shared = AxumState<Arc<State>>;

/// The buffer chunks are about a minute long, so a range ending within one
/// of now may reach into the chunk still being recorded.
const CHUNK_SECS: i64 = 120;

/// How long to give the source to close the current chunks.
const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

/// How many chunks one clip may join (a day at one minute each, plus room).
const MAX_CHUNKS: usize = 1500;

fn buffer() -> StreamId {
	StreamId::new("audio.buffer").expect("built-in stream")
}

fn clip() -> StreamId {
	StreamId::new("audio.clip").expect("built-in stream")
}

/// `POST /v1/audio/save`: save a moment, as an approved client.
pub async fn save(
	AxumState(state): Shared,
	request: Signed<SaveAudio>,
) -> ApiResult<Json<SavedClip>> {
	let caller = request.caller;
	let body = request.body;
	if let Err(denied) = caller.grant.authorize_write(&buffer(), Timestamp::now()) {
		let _ = state.vault.store.audit(AuditEvent::new(
			caller.actor(),
			"audio.save",
			buffer().as_str(),
			Outcome::Denied { reason: denied },
		));
		return Err(Error::Forbidden(denied));
	}
	// Saving joins chunks and stores a new file; hold it against the grant's
	// byte budget so a runaway client cannot fill the disk.
	let allowance =
		state
			.limiter()
			.byte_allowance(caller.grant.id, &caller.grant.limits, Timestamp::now());
	if allowance < 1024 * 1024 {
		return Err(Error::Forbidden(quant_access::Denied::RateLimited));
	}
	let saving = Arc::clone(&state);
	let saved = match tokio::task::spawn_blocking(move || save_blocking(&saving, &body))
		.await
		.map_err(|e| Error::Internal(e.to_string()))?
	{
		Ok(saved) => saved,
		Err(error) => {
			let _ = state.vault.store.audit(
				AuditEvent::new(
					caller.actor(),
					"audio.save",
					buffer().as_str(),
					Outcome::Allowed {
						records: 0,
						bytes: 0,
					},
				)
				.detail(json!({ "error": format!("{error:?}") })),
			);
			return Err(error);
		}
	};
	state.limiter().charge(
		caller.grant.id,
		&caller.grant.limits,
		saved.chunks,
		saved.bytes,
		Timestamp::now(),
	);
	state.vault.store.audit(
		AuditEvent::new(
			caller.actor(),
			"audio.save",
			buffer().as_str(),
			Outcome::Allowed {
				records: saved.chunks as u64,
				bytes: saved.bytes,
			},
		)
		.detail(json!({ "from": saved.from, "to": saved.to, "clip": saved.id })),
	)?;
	Ok(Json(saved))
}

/// `POST /v1/owner/audio/save`: save a moment, as the owner.
pub async fn owner_save(
	AxumState(state): Shared,
	_owner: crate::authn::Owner,
	Json(body): Json<SaveAudio>,
) -> ApiResult<Json<SavedClip>> {
	let saving = Arc::clone(&state);
	let saved = tokio::task::spawn_blocking(move || save_blocking(&saving, &body))
		.await
		.map_err(|e| Error::Internal(e.to_string()))??;
	state.vault.store.audit(
		AuditEvent::new(
			Actor::Owner,
			"audio.save",
			buffer().as_str(),
			Outcome::Allowed {
				records: saved.chunks as u64,
				bytes: saved.bytes,
			},
		)
		.detail(json!({ "from": saved.from, "to": saved.to, "clip": saved.id })),
	)?;
	Ok(Json(saved))
}

fn save_blocking(state: &Arc<State>, body: &SaveAudio) -> ApiResult<SavedClip> {
	let now = Timestamp::now();
	if body.to <= body.from {
		return Err(Error::Invalid("the range ends before it starts".into()));
	}
	if body.to.duration_since(body.from) > SignedDuration::from_hours(24) {
		return Err(Error::Invalid("save at most 24 hours at once".into()));
	}
	if body.to > now + SignedDuration::from_mins(5) {
		return Err(Error::Invalid("the range ends in the future".into()));
	}
	// A range reaching into the present may still be partly inside the chunk
	// being recorded; closing it now includes "just now" in the clip.
	if body.to > now - SignedDuration::from_secs(CHUNK_SECS) {
		let _ = quant_capture_macos::rewind::close_current_chunks(CLOSE_WAIT);
	}

	let range = TimeRange::new(body.from, body.to).map_err(|e| Error::Invalid(e.to_string()))?;
	let query = RecordQuery::new(Selector::stream(buffer()), range)
		.oldest_first()
		.limit(MAX_CHUNKS);
	let chunks: Vec<Record> = state
		.vault
		.store
		.records(&query)?
		.into_iter()
		.filter(|record| record.stream == buffer() && record.media.is_some())
		.collect();
	if chunks.is_empty() {
		return Err(Error::NotFound);
	}

	let rewind = crate::capture::config(state)
		.map(|config| config.rewind)
		.unwrap_or_default();
	let rate = u64::from(rewind.sample_rate.max(8000));
	// Frames of a chunk the asked-for range overlaps with.
	let overlap = |chunk: &Record| -> (u64, u64) {
		let end = chunk.end.unwrap_or(chunk.start);
		let millis = |from: Timestamp, to: Timestamp| {
			i64::try_from(to.duration_since(from).as_micros()).unwrap_or(i64::MAX)
		};
		let frames = |span_ms: i64| span_ms.unsigned_abs() * rate / 1_000_000;
		let skip = frames(millis(chunk.start, body.from.max(chunk.start)));
		let total = frames(millis(chunk.start, end));
		let tail = frames(millis(body.to.min(end), end));
		(skip, total.saturating_sub(skip).saturating_sub(tail))
	};
	let mut pieces = Vec::with_capacity(chunks.len());
	for chunk in &chunks {
		let (skip_frames, take_frames) = overlap(chunk);
		let media = chunk.media.as_ref().expect("filtered on media");
		pieces.push(quant_capture_macos::Piece {
			bytes: state.vault.get_media(&media.hash)?,
			skip_frames,
			take_frames,
		});
	}

	let joined = quant_capture_macos::join(&pieces, rewind.sample_rate, rewind.bitrate)
		.map_err(Error::Invalid)?;
	let duration_ms = joined.duration_ms();
	let media = state.vault.put_media(&joined.bytes, "audio/mp4", None)?;
	// A clip must outlive the buffer chunks it came from, even in the
	// unlikely case its bytes hash to one of them.
	state.vault.store.set_media_expiry(&media.hash, None)?;

	let mut sources: Vec<String> = chunks
		.iter()
		.filter_map(|chunk| {
			chunk
				.data
				.get("source")
				.and_then(|value| value.as_str())
				.map(str::to_owned)
		})
		.collect();
	sources.sort();
	sources.dedup();
	let chunk_ids: Vec<RecordId> = chunks.iter().map(|chunk| chunk.id).collect();
	let clip_from = chunks
		.first()
		.map_or(body.from, |chunk| chunk.start.max(body.from));
	let clip_to = clip_from
		.checked_add(SignedDuration::from_millis(
			i64::try_from(duration_ms).unwrap_or(i64::MAX),
		))
		.unwrap_or(clip_from);
	let device = crate::capture::device(state).unwrap_or_default();
	let mut record = Record::new(clip(), device)
		.starting(clip_from)
		.ending(clip_to)
		.with("sources", json!(sources))
		.with("chunk_ids", json!(chunk_ids))
		.with("duration_ms", duration_ms)
		.with("transcript_status", "pending")
		.with_media(media.clone());
	if let Some(title) = body
		.title
		.as_deref()
		.filter(|title| !title.trim().is_empty())
	{
		record = record.with("title", title.trim()).with_text(title.trim());
	}
	let id = record.id;
	state
		.vault
		.store
		.insert_records(std::slice::from_ref(&record))?;
	transcribe_later(Arc::clone(state), id);

	Ok(SavedClip {
		id,
		from: clip_from,
		to: clip_to,
		chunks: chunk_ids.len(),
		bytes: media.size,
	})
}

/// Transcribe a fresh clip once, at low priority, and write the text back
/// into the record. Whatever happens lands in `transcript_status` rather
/// than the log alone, so a silent clip explains itself.
fn transcribe_later(state: Arc<State>, id: RecordId) {
	std::thread::Builder::new()
		.name("quant-transcribe".to_owned())
		.spawn(move || {
			quant_capture_macos::lower_priority();
			let settings = crate::capture::config(&state)
				.map(|config| {
					quant_capture_macos::Transcribe {
						whisper_model: config.audio.whisper_model.clone(),
						language: config.audio.language.clone(),
					}
				})
				.map_err(|error| tracing::warn!(%error, "could not read the capture settings"));
			let Ok(settings) = settings else { return };
			let Some(mut record) = state.vault.store.record(id).unwrap_or(None) else {
				return;
			};
			let Some(media) = record.media.clone() else {
				return;
			};
			let Ok(bytes) = state.vault.get_media(&media.hash) else {
				return;
			};
			let outcome = quant_capture_macos::transcribe(&bytes, &settings);
			match outcome {
				Ok(text) if !text.trim().is_empty() => {
					// Search finds the clip by its title or by what was said.
					let title = record.data.get("title").and_then(|v| v.as_str());
					record.text = Some(match title {
						Some(title) => format!("{title}\n\n{}", text.trim()),
						None => text.trim().to_owned(),
					});
					record
						.data
						.insert("transcript_status".into(), json!("done"));
				}
				Ok(_) => {
					record
						.data
						.insert("transcript_status".into(), json!("no speech found"));
				}
				Err(reason) => {
					tracing::warn!(%reason, "clip transcription failed");
					record.data.insert(
						"transcript_status".into(),
						json!(format!("unavailable: {reason}")),
					);
				}
			}
			if let Err(error) = state.vault.store.replace_records(&[record]) {
				tracing::warn!(%error, "could not store the clip transcript");
			}
		})
		.map_err(|error| tracing::warn!(%error, "could not start transcription"))
		.ok();
}
