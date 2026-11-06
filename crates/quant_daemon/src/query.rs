//! Read routes: what a client may see, filtered by its grant.

use std::sync::Arc;

use axum::Json;
use axum::extract::Path;
use axum::extract::State as AxumState;
use axum::http::header;
use axum::response::IntoResponse;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Grant;
use quant_access::Level;
use quant_access::Scope;
use quant_access::Target;
use quant_access::audit::AuditEvent;
use quant_access::audit::Outcome;
use quant_access::redact;
use quant_api::Bucket;
use quant_api::ChannelSummary;
use quant_api::DurationItem;
use quant_api::Durations;
use quant_api::DurationsRequest;
use quant_api::Me;
use quant_api::Overview;
use quant_api::OverviewItem;
use quant_api::OverviewRequest;
use quant_api::Records;
use quant_api::RecordsRequest;
use quant_api::Series;
use quant_api::SeriesRequest;
use quant_api::ServedRecord;
use quant_api::StreamAccess;
use quant_core::Record;
use quant_core::StreamId;
use quant_core::TimeRange;
use quant_store::RecordQuery;
use quant_store::Selector;
use serde::Serialize;

use crate::authn::Caller;
use crate::authn::Signed;
use crate::authn::SignedAny;
use crate::error::ApiResult;
use crate::error::Error;
use crate::state::State;

type Shared = AxumState<Arc<State>>;

const DEFAULT_SPAN: SignedDuration = SignedDuration::from_hours(24);

/// Resolve `from`/`to` defaults and clip to the grant's lookback.
fn window(grant: &Grant, from: Option<Timestamp>, to: Option<Timestamp>) -> ApiResult<TimeRange> {
	let now = Timestamp::now();
	let to = to.unwrap_or(now).min(now + SignedDuration::from_mins(5));
	let from = from.unwrap_or_else(|| to.checked_sub(DEFAULT_SPAN).unwrap_or(Timestamp::MIN));
	let range = TimeRange::new(from, to).map_err(|e| Error::Invalid(e.to_string()))?;
	Ok(range.clamp_start(grant.floor(now)))
}

/// The selector of streams a grant may read at `min` level or above,
/// narrowed to what was asked for. Requested items the grant can see only
/// below `min` come back in the second list.
pub(crate) fn resolve(
	grant: &Grant,
	requested: &[String],
	min: Level,
) -> ApiResult<(Selector, Vec<String>)> {
	let readable: Vec<Target> = grant
		.scopes
		.0
		.iter()
		.filter_map(|scope| {
			match scope {
				Scope::Read { target, level } if *level >= min => Some(target.clone()),
				_ => None,
			}
		})
		.collect();
	let add = |selector: &mut Selector, target: &Target| {
		match target {
			Target::All => selector.all = true,
			Target::Family(family) => {
				if !selector.families.contains(family) {
					selector.families.push(*family);
				}
			}
			Target::Stream(stream) => {
				if !selector.streams.contains(stream) {
					selector.streams.push(stream.clone());
				}
			}
		}
	};

	let mut selector = Selector::default();
	let mut withheld = Vec::new();
	let wanted: Vec<Target> = if requested.is_empty() {
		vec![Target::All]
	} else {
		requested
			.iter()
			.map(|item| {
				item.parse::<Target>()
					.map_err(|_| Error::Invalid(format!("unknown stream or family `{item}`")))
			})
			.collect::<ApiResult<_>>()?
	};

	for want in &wanted {
		let mut matched = false;
		for have in &readable {
			// The narrower of the two targets is what may be read.
			let narrower = match (want, have) {
				(_, Target::All) => Some(want.clone()),
				(Target::All, _) => Some(have.clone()),
				(Target::Family(w), Target::Family(h)) => (w == h).then(|| want.clone()),
				(Target::Family(w), Target::Stream(h)) => (h.family() == *w).then(|| have.clone()),
				(Target::Stream(w), Target::Family(_)) => have.covers(w).then(|| want.clone()),
				(Target::Stream(w), Target::Stream(h)) => {
					if have.covers(w) {
						Some(want.clone())
					} else if want.covers(h) {
						Some(have.clone())
					} else {
						None
					}
				}
			};
			if let Some(target) = narrower {
				add(&mut selector, &target);
				matched = true;
			}
		}
		if !matched && !matches!(want, Target::All) {
			withheld.push(want.to_string());
		}
	}
	Ok((selector, withheld))
}

fn clean_value(value: &mut serde_json::Value, redact: bool) {
	match value {
		serde_json::Value::String(text) => *text = redact::prepare(text, redact),
		serde_json::Value::Array(items) => items.iter_mut().for_each(|v| clean_value(v, redact)),
		serde_json::Value::Object(map) => map.values_mut().for_each(|v| clean_value(v, redact)),
		_ => {}
	}
}

fn serve(record: Record, snippet: Option<String>, grant: &Grant) -> ServedRecord {
	let redact = !grant.unredacted;
	let raw = grant.scopes.read_level(&record.stream) == Some(Level::Raw);
	let mut data = serde_json::Value::Object(record.data);
	clean_value(&mut data, redact);
	ServedRecord {
		id: record.id,
		stream: record.stream,
		device: record.device,
		start: record.start,
		end: record.end,
		text: record.text.map(|t| redact::prepare(&t, redact)),
		snippet: snippet.map(|s| redact::prepare(&s, redact)),
		data: match data {
			serde_json::Value::Object(map) => map,
			_ => serde_json::Map::new(),
		},
		media: if raw { record.media } else { None },
	}
}

/// Charge the response to the grant, audit it, and send it.
fn respond<T: Serialize>(
	state: &State,
	caller: &Caller,
	action: &str,
	target: String,
	detail: serde_json::Value,
	records: usize,
	body: &T,
) -> ApiResult<Json<serde_json::Value>> {
	let value = serde_json::to_value(body).map_err(|e| Error::Internal(e.to_string()))?;
	let bytes = serde_json::to_vec(&value).map_or(0, |b| b.len() as u64);
	state.limiter().charge(
		caller.grant.id,
		&caller.grant.limits,
		records,
		bytes,
		Timestamp::now(),
	);
	state.vault.store.audit(
		AuditEvent::new(
			caller.actor(),
			action,
			target,
			Outcome::Allowed {
				records: records as u64,
				bytes,
			},
		)
		.detail(detail),
	)?;
	Ok(Json(value))
}

fn deny(state: &State, caller: &Caller, action: &str, target: &str, error: Error) -> Error {
	if let Error::Forbidden(reason) = &error {
		let _ = state.vault.store.audit(AuditEvent::new(
			caller.actor(),
			action,
			target,
			Outcome::Denied { reason: *reason },
		));
	}
	error
}

fn status(status: quant_store::ClientStatus) -> quant_api::ClientStatus {
	match status {
		quant_store::ClientStatus::Pending => quant_api::ClientStatus::Pending,
		quant_store::ClientStatus::Active => quant_api::ClientStatus::Active,
		quant_store::ClientStatus::Revoked => quant_api::ClientStatus::Revoked,
	}
}

/// `GET /v1/me`
pub async fn me(AxumState(state): Shared, request: SignedAny) -> ApiResult<Json<Me>> {
	let client = request.client;
	let grant = state.vault.store.active_grant(client.id)?;
	Ok(Json(Me {
		client: client.id,
		name: client.name,
		status: status(client.status),
		grant,
	}))
}

/// `GET /v1/streams`
pub async fn streams(
	AxumState(state): Shared,
	request: Signed<()>,
) -> ApiResult<Json<Vec<StreamAccess>>> {
	let grant = &request.caller.grant;
	let visible = state
		.specs()
		.into_iter()
		.filter_map(|spec| {
			grant
				.scopes
				.read_level(&spec.id)
				.map(|level| StreamAccess { spec, level })
		})
		.collect();
	Ok(Json(visible))
}

/// `POST /v1/query/overview`
pub async fn overview(
	AxumState(state): Shared,
	request: Signed<Option<OverviewRequest>>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body.unwrap_or_default();
	let range = window(&caller.grant, body.from, body.to)?;
	let (selector, _) = resolve(&caller.grant, &[], Level::Summary)?;
	let streams = state.vault.store.overview(&selector, range)?;
	let items: Vec<OverviewItem> = streams
		.into_iter()
		.map(|s| {
			OverviewItem {
				stream: s.stream,
				count: s.count,
				first: s.first,
				last: s.last,
			}
		})
		.collect();
	let detail = serde_json::json!({ "from": range.start, "to": range.end });
	let answer = Overview {
		from: range.start,
		to: range.end,
		streams: items,
	};
	respond(&state, caller, "overview", "*".into(), detail, 0, &answer)
}

fn records_or_search(
	state: &State,
	request: Signed<RecordsRequest>,
	search: bool,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body;
	let action = if search { "search" } else { "records" };
	let target = if body.streams.is_empty() {
		"*".to_owned()
	} else {
		body.streams.join(",")
	};
	let terms = body
		.search
		.as_deref()
		.map(str::trim)
		.filter(|t| !t.is_empty());
	if search && terms.is_none() {
		return Err(Error::Invalid("search needs terms".into()));
	}
	let range = window(&caller.grant, body.from, body.to)?;
	let (selector, withheld) = resolve(&caller.grant, &body.streams, Level::Read)?;

	let allowance =
		state
			.limiter()
			.record_allowance(caller.grant.id, &caller.grant.limits, Timestamp::now());
	if allowance == 0 {
		return Err(deny(
			state,
			caller,
			action,
			&target,
			Error::Forbidden(quant_access::Denied::RateLimited),
		));
	}
	let wanted = body.limit.unwrap_or(50).clamp(1, 500);
	let limit = wanted.min(allowance);
	let mut query = RecordQuery::new(selector, range).limit(limit + 1);
	if body.oldest_first {
		query = query.oldest_first();
	}
	if let Some(terms) = terms {
		query = query.search(terms);
	}
	let mut hits = if terms.is_some() {
		state.vault.store.search(&query)?
	} else {
		state
			.vault
			.store
			.records(&query)?
			.into_iter()
			.map(|record| {
				quant_store::SearchHit {
					record,
					snippet: String::new(),
					rank: 0.0,
				}
			})
			.collect()
	};
	let truncated = hits.len() > limit;
	hits.truncate(limit);
	let records: Vec<ServedRecord> = hits
		.into_iter()
		.map(|hit| {
			let snippet = (!hit.snippet.is_empty()).then_some(hit.snippet);
			serve(hit.record, snippet, &caller.grant)
		})
		.collect();
	let count = records.len();
	let detail = serde_json::json!({
		"from": range.start,
		"to": range.end,
		"limit": wanted,
		"search": terms,
		"withheld": withheld,
	});
	let answer = Records {
		from: range.start,
		to: range.end,
		records,
		truncated,
		withheld,
	};
	respond(state, caller, action, target, detail, count, &answer)
}

/// `POST /v1/query/records`
pub async fn records(
	AxumState(state): Shared,
	request: Signed<RecordsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	records_or_search(&state, request, false)
}

/// `POST /v1/query/search`
pub async fn search(
	AxumState(state): Shared,
	request: Signed<RecordsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	records_or_search(&state, request, true)
}

fn parse_stream(name: &str) -> ApiResult<StreamId> {
	name.parse()
		.map_err(|_| Error::Invalid(format!("unknown stream `{name}`")))
}

/// `POST /v1/query/series`
pub async fn series(
	AxumState(state): Shared,
	request: Signed<SeriesRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body;
	let stream = parse_stream(&body.stream)?;
	let range = window(&caller.grant, body.from, body.to)?;
	let wanted = if body.raw {
		Level::Read
	} else {
		Level::Summary
	};
	let permit = caller
		.grant
		.authorize_read(&stream, wanted, range, Timestamp::now())
		.map_err(|denied| deny(&state, caller, "series", stream.as_str(), denied.into()))?;
	let range = permit.range;
	let detail = serde_json::json!({ "from": range.start, "to": range.end, "raw": body.raw });

	if body.raw && permit.level >= Level::Read {
		let allowance = state.limiter().record_allowance(
			caller.grant.id,
			&caller.grant.limits,
			Timestamp::now(),
		);
		let limit = body
			.max_rows
			.unwrap_or(2000)
			.clamp(1, 20_000)
			.min(allowance);
		let batches = state.vault.store.series(&stream, range, limit + 1)?;
		let rows: usize = batches.iter().map(quant_core::SeriesBatch::len).sum();
		let mut batches = batches;
		let truncated = rows > limit;
		if truncated && let Some(last) = batches.last_mut() {
			let width = last.channels.len();
			last.offsets_ms.pop();
			last.values
				.truncate(last.values.len().saturating_sub(width));
			if last.is_empty() {
				batches.pop();
			}
		}
		let served = rows.min(limit);
		let answer = Series {
			stream: stream.clone(),
			from: range.start,
			to: range.end,
			bucket_seconds: None,
			buckets: Vec::new(),
			batches,
			truncated,
			note: None,
		};
		return respond(
			&state,
			caller,
			"series.raw",
			stream.to_string(),
			detail,
			served,
			&answer,
		);
	}

	let span = u64::try_from(range.duration().as_secs()).unwrap_or(0);
	let bucket_seconds = body.bucket_seconds.unwrap_or(span / 48).max(60);
	let bucket = SignedDuration::from_secs(i64::try_from(bucket_seconds).unwrap_or(i64::MAX));
	let buckets: Vec<Bucket> = state
		.vault
		.store
		.series_summary(&stream, range, bucket)?
		.into_iter()
		.map(|b| {
			Bucket {
				start: b.start,
				channels: b
					.channels
					.into_iter()
					.filter_map(|(name, stats)| {
						Some((
							name,
							ChannelSummary {
								count: stats.count,
								min: stats.min,
								max: stats.max,
								mean: stats.mean()?,
							},
						))
					})
					.collect(),
			}
		})
		.collect();
	let note = (body.raw && permit.level < Level::Read)
		.then(|| "this grant allows summaries of this stream, not raw samples".to_owned());
	let count = buckets.len();
	let answer = Series {
		stream: stream.clone(),
		from: range.start,
		to: range.end,
		bucket_seconds: Some(bucket_seconds),
		buckets,
		batches: Vec::new(),
		truncated: false,
		note,
	};
	respond(
		&state,
		caller,
		"series.summary",
		stream.to_string(),
		detail,
		count,
		&answer,
	)
}

/// `POST /v1/query/durations`
pub async fn durations(
	AxumState(state): Shared,
	request: Signed<DurationsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
	let caller = &request.caller;
	let body = request.body;
	let stream = parse_stream(&body.stream)?;
	let range = window(&caller.grant, body.from, body.to)?;
	let permit = caller
		.grant
		.authorize_read(&stream, Level::Read, range, Timestamp::now())
		.map_err(|denied| deny(&state, caller, "durations", stream.as_str(), denied.into()))?;
	if let Some(key) = &body.group_by
		&& permit.level < Level::Read
	{
		let allowed = state
			.spec(&stream)
			.is_some_and(|spec| spec.summary_keys.contains(key));
		if !allowed {
			return Err(deny(
				&state,
				caller,
				"durations",
				stream.as_str(),
				Error::Forbidden(quant_access::Denied::OutOfScope),
			));
		}
	}
	let range = permit.range;
	let span = range.duration().max(SignedDuration::from_secs(1));
	let bucket = body.bucket_seconds.map_or(span, |s| {
		SignedDuration::from_secs(i64::try_from(s.max(60)).unwrap_or(i64::MAX))
	});
	let items: Vec<DurationItem> = state
		.vault
		.store
		.durations(&stream, range, body.group_by.as_deref(), bucket)?
		.into_iter()
		.map(|d| {
			DurationItem {
				start: d.start,
				group: d.group.map(|g| redact::prepare(&g, permit.redact)),
				count: d.count,
				seconds: d.seconds,
			}
		})
		.collect();
	let detail =
		serde_json::json!({ "from": range.start, "to": range.end, "group_by": body.group_by });
	let count = items.len();
	let answer = Durations {
		stream: stream.clone(),
		from: range.start,
		to: range.end,
		items,
	};
	respond(
		&state,
		caller,
		"durations",
		stream.to_string(),
		detail,
		count,
		&answer,
	)
}

/// `GET /v1/media/{hash}`. Anything the caller may not see answers exactly
/// like media that does not exist.
pub async fn media(
	AxumState(state): Shared,
	Path(hash): Path<String>,
	request: Signed<()>,
) -> ApiResult<impl IntoResponse> {
	let caller = &request.caller;
	let now = Timestamp::now();
	let owner = state.vault.store.media_owner(&hash)?;
	let Some((stream, at)) = owner else {
		return Err(Error::NotFound);
	};
	let allowed = caller.grant.check_live(now).is_ok()
		&& caller.grant.scopes.read_level(&stream) == Some(Level::Raw)
		&& at >= caller.grant.floor(now);
	if !allowed {
		let _ = state.vault.store.audit(AuditEvent::new(
			caller.actor(),
			"media",
			stream.as_str(),
			Outcome::Denied {
				reason: quant_access::Denied::OutOfScope,
			},
		));
		return Err(Error::NotFound);
	}
	let budget = state
		.limiter()
		.byte_allowance(caller.grant.id, &caller.grant.limits, now);
	let bytes = state.vault.get_media(&hash)?;
	if bytes.len() as u64 > budget {
		return Err(deny(
			&state,
			caller,
			"media",
			stream.as_str(),
			Error::Forbidden(quant_access::Denied::RateLimited),
		));
	}
	let mime = state
		.vault
		.store
		.media_mime(&hash)?
		.unwrap_or_else(|| "application/octet-stream".into());
	state.limiter().charge(
		caller.grant.id,
		&caller.grant.limits,
		1,
		bytes.len() as u64,
		now,
	);
	state.vault.store.audit(
		AuditEvent::new(
			caller.actor(),
			"media",
			stream.as_str(),
			Outcome::Allowed {
				records: 1,
				bytes: bytes.len() as u64,
			},
		)
		.detail(serde_json::json!({ "hash": hash })),
	)?;
	Ok(([(header::CONTENT_TYPE, mime)], bytes))
}

#[cfg(test)]
mod tests {
	use quant_access::Scopes;
	use quant_core::ClientId;
	use quant_core::Family;

	use super::*;

	fn grant(scopes: &[&str]) -> Grant {
		Grant::new(ClientId::new(), "t", Scopes::parse(scopes).unwrap())
	}

	fn strings(items: &[&str]) -> Vec<String> {
		items.iter().map(|s| (*s).to_owned()).collect()
	}

	#[test]
	fn empty_request_selects_everything_readable() {
		let (selector, withheld) =
			resolve(&grant(&["health:read", "*:summary"]), &[], Level::Read).unwrap();
		assert_eq!(selector.families, vec![Family::Health]);
		assert!(!selector.all);
		assert_eq!(withheld, [] as [std::string::String; 0]);
	}

	#[test]
	fn requests_are_narrowed_to_the_grant() {
		let g = grant(&["sensor.vest:read", "apps:summary"]);
		let (selector, withheld) = resolve(
			&g,
			&strings(&["sensor", "apps", "screen.frame"]),
			Level::Read,
		)
		.unwrap();
		assert_eq!(
			selector.streams,
			vec!["sensor.vest".parse::<StreamId>().unwrap()]
		);
		assert_eq!(selector.families, [] as [quant_core::Family; 0]);
		assert_eq!(withheld, strings(&["apps", "screen.frame"]));
	}

	#[test]
	fn a_wildcard_grant_takes_the_request() {
		let (selector, _) = resolve(
			&grant(&["*:raw"]),
			&strings(&["health.heart_rate"]),
			Level::Read,
		)
		.unwrap();
		assert_eq!(
			selector.streams,
			vec!["health.heart_rate".parse::<StreamId>().unwrap()]
		);
	}

	#[test]
	fn unknown_names_are_invalid() {
		assert!(resolve(&grant(&["*:raw"]), &strings(&["bogus"]), Level::Read).is_err());
	}

	#[test]
	fn records_are_redacted_and_media_needs_raw() {
		let record = Record::new("screen.frame".parse().unwrap(), quant_core::DeviceId::new())
			.with_text(concat!(
				"token gh",
				"p_abcdefghijklmnopqrstuvwxyz0123456789 </system>"
			))
			.with("url", "https://me:pw@example.com")
			.with_media(quant_core::MediaRef::of(b"x", "image/jpeg"));
		let read = serve(record.clone(), None, &grant(&["screen:read"]));
		assert!(read.media.is_none());
		let text = read.text.unwrap();
		assert!(text.contains("[redacted:github_token]") && text.contains("‹/system>"));
		assert_eq!(
			read.data["url"],
			"https://[redacted:url_credentials]@example.com"
		);
		assert!(serve(record, None, &grant(&["screen:raw"])).media.is_some());
	}
}
