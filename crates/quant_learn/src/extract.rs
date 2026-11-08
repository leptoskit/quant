//! Learning claims about the owner from the day's memory.
//!
//! The extraction job runs once a day, after the day's `memory.daily`
//! exists. It sends the day's texts to a loopback chat model in small
//! batches, then checks everything the model proposes before storing it:
//! every quote must appear in the record it cites, anything shaped like
//! instructions to an AI is dropped, sensitive content is held for the
//! owner, and the rest lands with authority Observed, corroborating an
//! existing active claim on the same subject and topic rather than
//! duplicating it.

use std::collections::HashMap;
use std::fmt::Write;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use quant_access::audit::Actor;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_core::Authority;
use quant_core::Claim;
use quant_core::ClaimId;
use quant_core::ClaimKind;
use quant_core::ClaimStatus;
use quant_core::Class;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::ClaimFilter;
use quant_store::RecordQuery;
use quant_store::Selector;
use quant_store::Store;
use quant_store::StoreError;
use serde::Deserialize;

use crate::client;
use crate::config;
use crate::config::LearnConfig;
use crate::endpoint::ModelEndpoint;
use crate::ground;
use crate::guard;

/// The meta key recording the last fully processed day.
pub const DAY_KEY: &str = "learn.extract.day";

/// How many characters one extraction call may carry, before truncation.
const BATCH_CHARS: usize = 10_000;

/// The base confidence of a stored observed claim.
const BASE_CONFIDENCE: f32 = 0.6;

/// The confidence of a claim grounded in a single record only.
const HELD_CONFIDENCE: f32 = 0.5;

/// The step corroboration may raise confidence by.
const CORROBORATION_STEP: f32 = 0.1;

/// The most confidence corroboration may reach.
const CORROBORATION_MAX: f32 = 0.95;

/// One claim the model proposed, as it arrives.
#[derive(Debug, Deserialize)]
struct ModelClaim {
	#[serde(default)]
	kind: Option<String>,
	#[serde(default)]
	subject: Option<String>,
	#[serde(default)]
	topic: Option<String>,
	statement: String,
	#[serde(default)]
	confidence: Option<f32>,
	#[serde(default)]
	classes: Vec<String>,
	#[serde(default)]
	evidence: Vec<ModelEvidence>,
}

/// One evidence record a proposed claim cites.
#[derive(Debug, Deserialize)]
struct ModelEvidence {
	record: String,
	quote: String,
}

/// The model's answer, as it promised it.
#[derive(Debug, Deserialize)]
struct ModelResponse {
	#[serde(default)]
	claims: Vec<ModelClaim>,
}

/// What storing one proposed claim did.
enum StoreOutcome {
	/// A new claim was stored.
	Stored { rowid: i64, statement: String },
	/// Evidence was added to an existing active claim.
	Corroborated,
	/// The claim failed grounding, shape, or instruction checks.
	Dropped,
}

/// What an extraction run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtractReport {
	/// Model calls made.
	pub calls: usize,
	/// Calls that answered and stored something.
	pub ok: usize,
	/// Calls that answered with nothing usable.
	pub empty: usize,
	/// Calls that failed to reach the endpoint.
	pub unavailable: usize,
	/// Claims stored, including corroborations.
	pub stored: usize,
	/// Claims dropped by grounding, instruction, or shape rules.
	pub dropped: usize,
	/// Whether the day's watermark advanced, so the day is not tried again.
	pub day_done: bool,
}

/// Extract claims from the day's memory once: the day's `memory.daily` and
/// `memory.episode`s and the last day of `memory.note`s and saved
/// transcripts, in small batches. Only `ok` and `empty` calls advance the
/// day's watermark, so an outage never looks like "nothing was learned".
/// One audit entry per run, counts only.
pub async fn extract_daily(
	store: &Store,
	config: &LearnConfig,
	now: Timestamp,
	tz: &TimeZone,
) -> Result<ExtractReport, StoreError> {
	let mut report = ExtractReport::default();
	let Some(extract) = &config.extract else {
		return Ok(report);
	};
	let endpoint = ModelEndpoint::parse(&extract.endpoint).map_err(StoreError::Invalid)?;
	let today = now.to_zoned(tz.clone()).date();
	let today_string = today.to_string();
	let calls_key = format!("learn.extract.calls.{today_string}");
	let used: u32 = store
		.meta(&calls_key)?
		.and_then(|value| value.parse().ok())
		.unwrap_or(0);
	let mut calls_left = extract.calls_per_day.saturating_sub(used);
	if calls_left == 0 {
		return Ok(report);
	}

	let records = source_records(store, now, tz)?;
	for batch in split_batches(records) {
		if report.stored >= extract.claims_per_run || calls_left == 0 {
			break;
		}
		let prompt = build_prompt(&batch);
		report.calls += 1;
		let Ok(content) = client::chat(&endpoint, &extract.model, &prompt).await else {
			report.unavailable += 1;
			break;
		};
		calls_left -= 1;
		store.set_meta(
			&calls_key,
			&(used + u32::try_from(report.calls).unwrap_or(u32::MAX)).to_string(),
		)?;
		let before = report.stored;
		if let Ok(response) = parse_response(&content) {
			for claim in response.claims {
				if report.stored >= extract.claims_per_run {
					break;
				}
				match store_claim(store, &batch, claim, tz)? {
					StoreOutcome::Stored { rowid, statement } => {
						report.stored += 1;
						embed_claim(store, config, rowid, &statement).await?;
					}
					StoreOutcome::Corroborated => report.stored += 1,
					StoreOutcome::Dropped => report.dropped += 1,
				}
			}
		}
		if report.stored > before {
			report.ok += 1;
		} else {
			report.empty += 1;
		}
	}

	// The day is done when the endpoint never failed; hitting the budgets
	// is a deliberate stop, not an outage.
	report.day_done = report.unavailable == 0;
	if report.day_done {
		store.set_meta(DAY_KEY, &today_string)?;
	}
	store.audit(
		AuditEvent::new(
			Actor::Owner,
			"learn.extract",
			"",
			Outcome::Allowed {
				records: report.stored as u64,
				bytes: 0,
			},
		)
		.detail(serde_json::json!({
			"calls": report.calls,
			"ok": report.ok,
			"empty": report.empty,
			"unavailable": report.unavailable,
			"stored": report.stored,
			"dropped": report.dropped,
			"day_done": report.day_done,
		})),
	)?;
	Ok(report)
}

/// The records one extraction day reads: the day's daily summary and
/// episodes, and the last day of notes and saved transcripts.
fn source_records(store: &Store, now: Timestamp, tz: &TimeZone) -> Result<Vec<Record>, StoreError> {
	let day = day_range(now.to_zoned(tz.clone()).date(), tz)?;
	let streams = [
		"memory.daily",
		"memory.episode",
		"memory.note",
		"audio.transcript",
	]
	.map(|name| StreamId::new(name).expect("built-in stream"));
	let range = TimeRange::new(now - SignedDuration::from_hours(24), day.end)
		.map_err(|error| StoreError::Invalid(error.to_string()))?;
	let mut records = store.records(
		&RecordQuery::new(
			Selector {
				streams: streams.to_vec(),
				..Selector::default()
			},
			range,
		)
		.oldest_first(),
	)?;
	records.retain(|record| {
		record
			.text
			.as_deref()
			.is_some_and(|text| !text.trim().is_empty())
	});
	Ok(records)
}

/// The span of `date` in `tz`.
fn day_range(date: Date, tz: &TimeZone) -> Result<TimeRange, StoreError> {
	let start = date
		.to_datetime(jiff::civil::Time::midnight())
		.to_zoned(tz.clone())
		.map_err(|error| StoreError::Invalid(error.to_string()))?;
	let end = start
		.tomorrow()
		.map_err(|error| StoreError::Invalid(error.to_string()))?;
	TimeRange::new(start.timestamp(), end.timestamp())
		.map_err(|error| StoreError::Invalid(error.to_string()))
}

/// Group the records into batches that each fit the prompt budget.
fn split_batches(records: Vec<Record>) -> Vec<Vec<Record>> {
	let mut batches = Vec::new();
	let mut current: Vec<Record> = Vec::new();
	let mut chars = 0usize;
	for record in records {
		let text = record.text.as_deref().unwrap_or_default();
		let cost = config::truncate(text).chars().count() + record.id.to_string().len() + 40;
		if !current.is_empty() && chars + cost > BATCH_CHARS {
			batches.push(std::mem::take(&mut current));
			chars = 0;
		}
		chars += cost;
		current.push(record);
	}
	if !current.is_empty() {
		batches.push(current);
	}
	batches
}

/// The prompt for one batch: the records, and the exact JSON shape asked
/// back.
fn build_prompt(batch: &[Record]) -> String {
	let mut prompt = String::from(
		"Read the records from the owner's own Quant vault below and state what can be observed \
		 about the person who produced them. Answer with strict JSON only and no commentary: \
		 {\"claims\":[{\"kind\":\"preference|value|routine|fact|relationship|goal|commitment\",\"\
		 subject\":\"me\",\"topic\":\"an optional dotted key\",\"statement\":\"one \
		 natural-language \
		 sentence\",\"confidence\":0.6,\"classes\":[\"health\"],\"evidence\":[{\"record\":\"the \
		 record id\",\"quote\":\"words copied exactly from that record's text\"}]}]}. Every quote \
		 must appear word for word in the cited record's text, and every evidence id must be one \
		 of the records below. Say nothing about credentials, passwords, access, grants, or money \
		 movement.\n\nRecords:\n",
	);
	for record in batch {
		let text = config::truncate(record.text.as_deref().unwrap_or_default());
		let _ = writeln!(
			prompt,
			"id {} stream {} at {}: {text}",
			record.id, record.stream, record.start
		);
	}
	prompt
}

/// The model's answer, reduced to the JSON it promised: code fences are
/// stripped and the outermost `{` `}` pair is parsed.
fn parse_response(content: &str) -> Result<ModelResponse, String> {
	let cleaned = content
		.trim()
		.trim_start_matches("```json")
		.trim_start_matches("```")
		.trim_end_matches("```")
		.trim();
	let start = cleaned.find('{').ok_or("no json object in the answer")?;
	let end = cleaned.rfind('}').ok_or("no json object in the answer")?;
	serde_json::from_str(&cleaned[start..=end]).map_err(|error| error.to_string())
}

/// The classes of `stream`'s spec, or of its nearest registered ancestor.
fn stream_classes(specs: &[quant_core::StreamSpec], stream: &StreamId) -> Vec<Class> {
	let mut name = stream.as_str();
	loop {
		if let Some(spec) = specs.iter().find(|spec| spec.id.as_str() == name) {
			return spec.classes.clone();
		}
		let Some((parent, _)) = name.rsplit_once('.') else {
			return Vec::new();
		};
		name = parent;
	}
}

/// Check one proposed claim against its evidence and the rules, and store
/// it: grounded, instruction-free, sensitive content held, the rest
/// Observed — Active only when grounded in at least two distinct records
/// from different days or different streams.
fn store_claim(
	store: &Store,
	batch: &[Record],
	mut model: ModelClaim,
	tz: &TimeZone,
) -> Result<StoreOutcome, StoreError> {
	// Shape: a known kind, a real statement, a well-formed topic, known
	// classes, and a sane confidence. Anything off is dropped.
	let Some(kind) = model.kind.as_deref().and_then(ClaimKind::parse) else {
		return Ok(StoreOutcome::Dropped);
	};
	let subject = model.subject.unwrap_or_else(|| "me".to_owned());
	if subject.trim().is_empty() {
		return Ok(StoreOutcome::Dropped);
	}
	let statement = model.statement.trim().to_owned();
	if statement.is_empty() || statement.chars().count() > 500 {
		return Ok(StoreOutcome::Dropped);
	}
	let raw_topic = model.topic.take();
	let topic = raw_topic.as_deref().and_then(|topic| {
		(!topic.is_empty()
			&& topic.chars().count() <= 64
			&& topic.bytes().all(|b| {
				b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
			}))
		.then(|| topic.to_owned())
	});
	if raw_topic.is_some() && topic.is_none() {
		return Ok(StoreOutcome::Dropped);
	}
	let mut classes = Vec::with_capacity(model.classes.len());
	for name in &model.classes {
		let Some(class) = Class::parse(name) else {
			return Ok(StoreOutcome::Dropped);
		};
		if !classes.contains(&class) {
			classes.push(class);
		}
	}
	let confidence = model.confidence.unwrap_or(BASE_CONFIDENCE).clamp(0.0, 1.0);

	// Grounding: every evidence id must be a record in this batch, and
	// every quote must appear in that record's text, whole words, after
	// normalisation. One ungrounded quote drops the whole claim.
	if model.evidence.is_empty() {
		return Ok(StoreOutcome::Dropped);
	}
	let texts: HashMap<RecordId, &str> = batch
		.iter()
		.map(|record| (record.id, record.text.as_deref().unwrap_or_default()))
		.collect();
	let mut evidence: Vec<RecordId> = Vec::new();
	for piece in &model.evidence {
		let Ok(id) = piece.record.parse::<RecordId>() else {
			return Ok(StoreOutcome::Dropped);
		};
		let Some(text) = texts.get(&id) else {
			return Ok(StoreOutcome::Dropped);
		};
		if !ground::quote_grounded(text, &piece.quote) {
			return Ok(StoreOutcome::Dropped);
		}
		if !evidence.contains(&id) {
			evidence.push(id);
		}
	}

	// A claim carries at least the classes of the records it rests on, so a
	// model that forgets to tag a heart-rate fact as health cannot hand it
	// to a grant that denies health.
	let mut specs = quant_core::builtin_streams();
	specs.extend(store.custom_streams()?);
	for record in batch.iter().filter(|record| evidence.contains(&record.id)) {
		for class in stream_classes(&specs, &record.stream) {
			if !classes.contains(&class) {
				classes.push(class);
			}
		}
	}

	// No instructions: a captured screen or transcript must not plant
	// orders through a claim.
	if guard::looks_like_instruction(&statement)
		|| model
			.evidence
			.iter()
			.any(|piece| guard::looks_like_instruction(&piece.quote))
	{
		return Ok(StoreOutcome::Dropped);
	}

	// Sensitive content is always held for the owner, relationships to
	// named people included.
	let sensitive =
		guard::is_sensitive(topic.as_deref(), &statement) || kind == ClaimKind::Relationship;

	// Active only when grounded in at least two distinct records from
	// different days or different streams; otherwise held.
	let supporting: Vec<&Record> = batch
		.iter()
		.filter(|record| evidence.contains(&record.id))
		.collect();
	let mut days: Vec<Date> = supporting
		.iter()
		.map(|record| record.start.to_zoned(tz.clone()).date())
		.collect();
	days.sort_unstable();
	days.dedup();
	let mut streams: Vec<&StreamId> = supporting.iter().map(|record| &record.stream).collect();
	streams.sort_unstable();
	streams.dedup();
	let active = !sensitive && supporting.len() >= 2 && (days.len() >= 2 || streams.len() >= 2);

	// Corroboration: an active claim with the same subject and topic gains
	// the new evidence and a bounded confidence step, instead of a
	// duplicate. Owner claims are never touched.
	if active && let Some(topic) = &topic {
		let existing = store.claims(
			&ClaimFilter::new()
				.status(ClaimStatus::Active)
				.subject(&subject)
				.topic(topic),
		)?;
		if existing
			.iter()
			.any(|claim| matches!(claim.authority, Authority::Owner | Authority::OwnerAuthored))
		{
			return store_new(
				store,
				ObservedClaim {
					status: ClaimStatus::Held,
					subject,
					kind,
					statement,
					topic: Some(topic.clone()),
					confidence: confidence.min(HELD_CONFIDENCE),
					classes,
					evidence,
				},
			);
		}
		if let Some(target) = existing
			.into_iter()
			.find(|claim| matches!(claim.authority, Authority::Observed | Authority::Inferred))
		{
			store.add_claim_evidence(target.id, &evidence)?;
			let confidence = (target.confidence + CORROBORATION_STEP).min(CORROBORATION_MAX);
			store.set_claim_confidence(target.id, confidence)?;
			return Ok(StoreOutcome::Corroborated);
		}
	}

	store_new(
		store,
		ObservedClaim {
			status: if active {
				ClaimStatus::Active
			} else {
				ClaimStatus::Held
			},
			subject,
			kind,
			statement,
			topic,
			confidence,
			classes,
			evidence,
		},
	)
}

/// One fully checked claim, ready to store with authority Observed.
struct ObservedClaim {
	status: ClaimStatus,
	subject: String,
	kind: ClaimKind,
	statement: String,
	topic: Option<String>,
	confidence: f32,
	classes: Vec<Class>,
	evidence: Vec<RecordId>,
}

/// Insert a fully checked claim with authority Observed.
fn store_new(store: &Store, observed: ObservedClaim) -> Result<StoreOutcome, StoreError> {
	let ObservedClaim {
		status,
		subject,
		kind,
		statement,
		topic,
		confidence,
		classes,
		evidence,
	} = observed;
	let now = Timestamp::now();
	let claim = Claim {
		id: ClaimId::new(),
		subject,
		kind,
		statement: statement.clone(),
		topic,
		authority: Authority::Observed,
		confidence,
		classes,
		status,
		valid_from: None,
		valid_to: None,
		evidence,
		proposed_by: None,
		created_at: now,
		updated_at: now,
		superseded_by: None,
	};
	claim
		.validate()
		.map_err(|error| StoreError::Invalid(error.to_string()))?;
	let rowid = store.insert_claim(&claim)?;
	Ok(StoreOutcome::Stored { rowid, statement })
}

/// Embed a stored claim's statement for claim similarity, when an
/// embedding endpoint is configured. A failure here leaves the claim
/// stored but unembedded; it is simply missing from similarity results.
async fn embed_claim(
	store: &Store,
	config: &LearnConfig,
	rowid: i64,
	statement: &str,
) -> Result<(), StoreError> {
	let Some(embed) = &config.embed else {
		return Ok(());
	};
	let Some(space) = config.space() else {
		return Ok(());
	};
	let endpoint = ModelEndpoint::parse(&embed.endpoint).map_err(StoreError::Invalid)?;
	let Ok(mut vectors) = client::embed(&endpoint, &embed.model, &[statement]).await else {
		return Ok(());
	};
	let Some(mut vector) = vectors.pop() else {
		return Ok(());
	};
	if vector.len() != embed.dims {
		return Ok(());
	}
	quant_store::l2_normalize(&mut vector);
	store.set_claim_embedding(&space, rowid, &quant_store::vector_to_bytes(&vector))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn claims_inherit_the_classes_of_their_streams() {
		let specs = quant_core::builtin_streams();
		let heart: StreamId = "health.heart_rate".parse().unwrap();
		assert!(stream_classes(&specs, &heart).contains(&Class::Health));
		let daily: StreamId = "memory.daily".parse().unwrap();
		assert!(stream_classes(&specs, &daily).contains(&Class::Health));
		let note: StreamId = "memory.note".parse().unwrap();
		assert!(stream_classes(&specs, &note).is_empty());
	}

	#[test]
	fn responses_parse_with_or_without_fences() {
		let raw =
			"Here you go:\n```json\n{\"claims\":[{\"statement\":\"ok\",\"evidence\":[]}]}\n```";
		let response = parse_response(raw).unwrap();
		assert_eq!(response.claims.len(), 1);
		assert!(parse_response("no json here").is_err());
	}

	#[test]
	fn batches_fit_the_budget_and_keep_order() {
		let record = |text: &str| {
			Record::new("memory.note".parse().unwrap(), quant_core::DeviceId::new()).with_text(text)
		};
		let small = record("short");
		let batches = split_batches(vec![small.clone(), small]);
		assert_eq!(batches.len(), 1);
		let big_text = "x".repeat(BATCH_CHARS + 100);
		let big = record(&big_text);
		let batches = split_batches(vec![big.clone(), record("tail")]);
		assert_eq!(batches.len(), 2);
		assert_eq!(batches[1][0].text.as_deref().unwrap(), "tail");
	}

	#[test]
	fn prompts_carry_the_records_and_the_shape() {
		let record = Record::new("memory.note".parse().unwrap(), quant_core::DeviceId::new())
			.with_text("espresso at 14:02");
		let prompt = build_prompt(std::slice::from_ref(&record));
		assert!(prompt.contains("espresso at 14:02"), "{prompt}");
		assert!(prompt.contains(&record.id.to_string()), "{prompt}");
		assert!(prompt.contains("\"evidence\""));
	}
}
