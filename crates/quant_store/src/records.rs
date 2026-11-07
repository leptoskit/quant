use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::MediaRef;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::StreamId;
use quant_core::TimeRange;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::params;
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::Result;
use crate::Selector;
use crate::Store;
use crate::StoreError;
use crate::from_micros;
use crate::micros;

/// What happened to a batch of records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertReport {
	/// Newly stored.
	pub stored: usize,
	/// Already present (same id), so skipped.
	pub duplicate: usize,
}

/// A query over records.
#[derive(Clone, Debug)]
pub struct RecordQuery {
	/// Which streams.
	pub selector: Selector,
	/// When.
	pub range: TimeRange,
	/// Full-text search terms, if any.
	pub search: Option<String>,
	/// Most records to return.
	pub limit: usize,
	/// Newest first (the default) or oldest first.
	pub newest_first: bool,
}

impl RecordQuery {
	/// Records from `selector` within `range`, newest first.
	pub fn new(selector: Selector, range: TimeRange) -> Self {
		Self {
			selector,
			range,
			search: None,
			limit: 100,
			newest_first: true,
		}
	}

	/// Only records whose text matches `terms`.
	#[must_use]
	pub fn search(mut self, terms: impl Into<String>) -> Self {
		self.search = Some(terms.into());
		self
	}

	/// At most `limit` records.
	#[must_use]
	pub fn limit(mut self, limit: usize) -> Self {
		self.limit = limit;
		self
	}

	/// Oldest first.
	#[must_use]
	pub fn oldest_first(mut self) -> Self {
		self.newest_first = false;
		self
	}
}

/// A record found by search, with the matching passage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
	/// The record.
	pub record: Record,
	/// The text around the match, with matches in `[` `]`.
	pub snippet: String,
	/// BM25 rank; lower is better.
	pub rank: f64,
}

/// How many records and series rows a stream holds in a range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamCount {
	/// The stream.
	pub stream: StreamId,
	/// Records, or series rows.
	pub count: u64,
	/// Earliest sample in the range.
	pub first: Timestamp,
	/// Latest sample in the range.
	pub last: Timestamp,
}

/// Time spent and records counted for one group in one bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DurationBucket {
	/// Start of the bucket.
	pub start: Timestamp,
	/// The grouping value, such as an app name, or `None` when ungrouped.
	pub group: Option<String>,
	/// Records counted.
	pub count: u64,
	/// Seconds covered, clipped to the queried range.
	pub seconds: f64,
}

/// Turn free text into an FTS5 query that cannot inject operators: each term
/// is quoted, a trailing `*` keeps prefix search, and every term must match.
pub fn fts_query(terms: &str) -> Option<String> {
	let quoted: Vec<String> = terms
		.split_whitespace()
		.filter_map(|term| {
			let (body, prefix) = match term.strip_suffix('*') {
				Some(body) => (body, true),
				None => (term, false),
			};
			let body: String = body.chars().filter(|c| !matches!(c, '"' | '\0')).collect();
			if body.trim_matches(|c: char| !c.is_alphanumeric()).is_empty() {
				return None;
			}
			Some(if prefix {
				format!("\"{body}\"*")
			} else {
				format!("\"{body}\"")
			})
		})
		.collect();
	(!quoted.is_empty()).then(|| quoted.join(" "))
}

const COLUMNS: &str = "r.id, r.stream, r.device, r.start_us, r.end_us, r.text, r.data, \
                       r.media_hash, r.media_mime, r.media_size";

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Record>> {
	let id: Vec<u8> = row.get(0)?;
	let stream: String = row.get(1)?;
	let device: Vec<u8> = row.get(2)?;
	let start: i64 = row.get(3)?;
	let end: Option<i64> = row.get(4)?;
	let text: Option<String> = row.get(5)?;
	let data: Option<String> = row.get(6)?;
	let media_hash: Option<String> = row.get(7)?;
	let media_mime: Option<String> = row.get(8)?;
	let media_size: Option<i64> = row.get(9)?;
	Ok((|| {
		Ok(Record {
			id: RecordId(Uuid::from_slice(&id).map_err(|_| StoreError::Corrupt("record id"))?),
			stream: StreamId::new(stream).map_err(|_| StoreError::Corrupt("stream"))?,
			device: DeviceId(Uuid::from_slice(&device).map_err(|_| StoreError::Corrupt("device"))?),
			start: from_micros(start)?,
			end: end.map(from_micros).transpose()?,
			text,
			data: match data {
				Some(json) => serde_json::from_str(&json)?,
				None => serde_json::Map::new(),
			},
			media: match (media_hash, media_mime, media_size) {
				(Some(hash), Some(mime), Some(size)) => {
					Some(MediaRef {
						hash,
						mime,
						size: u64::try_from(size).map_err(|_| StoreError::Corrupt("media size"))?,
					})
				}
				_ => None,
			},
		})
	})())
}

impl Store {
	/// Store records, skipping ids already present.
	pub fn insert_records(&self, records: &[Record]) -> Result<InsertReport> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let mut report = InsertReport::default();
		let observed = micros(Timestamp::now());
		{
			let mut insert = tx.prepare_cached(
				"INSERT OR IGNORE INTO records
				 (id, stream, family, device, start_us, end_us, observed_us, text, data, media_hash, \
				 media_mime, media_size)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
			)?;
			for record in records {
				if record.end.is_some_and(|end| end < record.start) {
					return Err(StoreError::Invalid(format!(
						"record {} ends before it starts",
						record.id
					)));
				}
				let data = (!record.data.is_empty())
					.then(|| serde_json::to_string(&record.data))
					.transpose()?;
				let changed = insert.execute(params![
					record.id.0.as_bytes().as_slice(),
					record.stream.as_str(),
					record.stream.family().as_str(),
					record.device.0.as_bytes().as_slice(),
					micros(record.start),
					record.end.map(micros),
					observed,
					record.text.as_deref().filter(|t| !t.is_empty()),
					data,
					record.media.as_ref().map(|m| m.hash.as_str()),
					record.media.as_ref().map(|m| m.mime.as_str()),
					record
						.media
						.as_ref()
						.map(|m| i64::try_from(m.size).unwrap_or(i64::MAX)),
				])?;
				if changed == 0 {
					report.duplicate += 1;
				} else {
					report.stored += 1;
				}
			}
		}
		tx.commit()?;
		Ok(report)
	}

	/// Store records, replacing any with the same id. Derived records use
	/// stable ids so recomputing them overwrites the old version.
	pub fn replace_records(&self, records: &[Record]) -> Result<InsertReport> {
		{
			let mut conn = self.conn();
			let tx = conn.transaction()?;
			{
				let mut delete = tx.prepare_cached("DELETE FROM records WHERE id = ?1")?;
				for record in records {
					delete.execute([record.id.0.as_bytes().as_slice()])?;
				}
			}
			tx.commit()?;
		}
		self.insert_records(records)
	}

	/// Delete records Quant derived itself (from the nil device) in `stream`
	/// starting at or after `from`, whose `derived_from` attribute is
	/// `source`. Used before recomputing a stretch.
	pub fn delete_derived(
		&self,
		stream: &StreamId,
		source: &str,
		from: Timestamp,
	) -> Result<usize> {
		Ok(self.conn().execute(
			"DELETE FROM records WHERE stream = ?1 AND device = ?2 AND start_us >= ?3
			 AND json_extract(data, '$.derived_from') = ?4",
			params![
				stream.as_str(),
				Uuid::nil().as_bytes().as_slice(),
				micros(from),
				source
			],
		)?)
	}

	/// The oldest `limit` records with their row numbers, for draining a
	/// spool in order.
	pub fn oldest_records(&self, limit: usize) -> Result<Vec<(i64, Record)>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&format!(
			"SELECT {COLUMNS}, r.rowid FROM records r ORDER BY r.rowid LIMIT ?1"
		))?;
		let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
			let record = record_from_row(row)?;
			let rowid: i64 = row.get(10)?;
			Ok(record.map(|record| (rowid, record)))
		})?;
		rows.map(|row| row?).collect()
	}

	/// Delete records by row number.
	pub fn delete_record_rows(&self, rows: &[i64]) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		{
			let mut delete = tx.prepare_cached("DELETE FROM records WHERE rowid = ?1")?;
			for row in rows {
				delete.execute([row])?;
			}
		}
		tx.commit()?;
		Ok(())
	}

	/// Read a value from the store's key/value state.
	pub fn meta(&self, key: &str) -> Result<Option<String>> {
		Ok(self
			.conn()
			.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
			.optional()?)
	}

	/// Write a value to the store's key/value state.
	pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
		self.conn().execute(
			"INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value \
			 = excluded.value",
			params![key, value],
		)?;
		Ok(())
	}

	/// Distinct devices that sent `stream` (or its children) in `range`.
	pub fn series_devices(
		&self,
		stream: &StreamId,
		range: TimeRange,
	) -> Result<Vec<(StreamId, DeviceId)>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(
			"SELECT DISTINCT stream, device FROM series
			 WHERE (stream = ?1 OR stream LIKE ?2) AND start_us < ?3 AND end_us >= ?4",
		)?;
		let rows = statement.query_map(
			params![
				stream.as_str(),
				format!("{}.%", stream.as_str()),
				micros(range.end),
				micros(range.start)
			],
			|r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
		)?;
		rows.map(|row| {
			let (stream, device) = row?;
			Ok((
				StreamId::new(stream).map_err(|_| StoreError::Corrupt("stream"))?,
				DeviceId(Uuid::from_slice(&device).map_err(|_| StoreError::Corrupt("device"))?),
			))
		})
		.collect()
	}

	/// One record by id.
	pub fn record(&self, id: RecordId) -> Result<Option<Record>> {
		let conn = self.conn();
		conn.query_row(
			&format!("SELECT {COLUMNS} FROM records r WHERE r.id = ?1"),
			[id.0.as_bytes().as_slice()],
			record_from_row,
		)
		.optional()?
		.transpose()
	}

	/// Records matching a query. Overlapping spans count: a focus span that
	/// started before the range but ended inside it is included.
	pub fn records(&self, query: &RecordQuery) -> Result<Vec<Record>> {
		Ok(self
			.query(query)?
			.into_iter()
			.map(|hit| hit.record)
			.collect())
	}

	/// Records whose text matches `query.search`, with snippets, best first.
	pub fn search(&self, query: &RecordQuery) -> Result<Vec<SearchHit>> {
		if query.search.as_deref().and_then(fts_query).is_none() {
			return Ok(Vec::new());
		}
		self.query(query)
	}

	fn query(&self, query: &RecordQuery) -> Result<Vec<SearchHit>> {
		if query.selector.is_empty() || query.limit == 0 {
			return Ok(Vec::new());
		}
		let mut params: Vec<Value> = Vec::new();
		let fts = query.search.as_deref().and_then(fts_query);
		let (from, extra, order, snippet) = if let Some(fts) = fts {
			params.push(fts.into());
			(
				"records_fts f JOIN records r ON r.rowid = f.rowid",
				"records_fts MATCH ? AND",
				"rank".to_owned(),
				"snippet(records_fts, 0, '[', ']', '…', 24), rank",
			)
		} else {
			let order = if query.newest_first {
				"r.start_us DESC"
			} else {
				"r.start_us ASC"
			};
			("records r", "", order.to_owned(), "NULL, 0.0")
		};
		params.push(micros(query.range.end).into());
		params.push(micros(query.range.start).into());
		params.push(micros(query.range.start).into());
		let selector = query.selector.sql("r.", &mut params);
		params.push(i64::try_from(query.limit).unwrap_or(i64::MAX).into());
		let sql = format!(
			"SELECT {COLUMNS}, {snippet} FROM {from}
			 WHERE {extra} r.start_us < ? AND (r.start_us >= ? OR coalesce(r.end_us, r.start_us) > ?)
			 AND {selector}
			 ORDER BY {order} LIMIT ?",
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(params_from_iter(params), |row| {
			let record = record_from_row(row)?;
			let snippet: Option<String> = row.get(10)?;
			let rank: f64 = row.get(11)?;
			Ok(record.map(|record| {
				SearchHit {
					record,
					snippet: snippet.unwrap_or_default(),
					rank,
				}
			}))
		})?;
		rows.map(|row| row?).collect()
	}

	/// How much each selected stream holds in `range`, records and series
	/// rows together.
	pub fn overview(&self, selector: &Selector, range: TimeRange) -> Result<Vec<StreamCount>> {
		if selector.is_empty() {
			return Ok(Vec::new());
		}
		let mut params: Vec<Value> = vec![micros(range.start).into(), micros(range.end).into()];
		let records_selector = selector.sql("", &mut params);
		params.push(micros(range.start).into());
		params.push(micros(range.end).into());
		let series_selector = selector.sql("", &mut params);
		let sql = format!(
			"SELECT stream, sum(n), min(first), max(last) FROM (
				SELECT stream, count(*) AS n, min(start_us) AS first, max(coalesce(end_us, start_us)) AS last
				FROM records WHERE start_us >= ? AND start_us < ? AND {records_selector} GROUP BY stream
				UNION ALL
				SELECT stream, sum(rows), min(start_us), max(end_us)
				FROM series WHERE start_us >= ? AND start_us < ? AND {series_selector} GROUP BY stream
			) GROUP BY stream ORDER BY stream"
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(params_from_iter(params), |row| {
			Ok((
				row.get::<_, String>(0)?,
				row.get::<_, i64>(1)?,
				row.get::<_, i64>(2)?,
				row.get::<_, i64>(3)?,
			))
		})?;
		rows.map(|row| {
			let (stream, count, first, last) = row?;
			Ok(StreamCount {
				stream: StreamId::new(stream).map_err(|_| StoreError::Corrupt("stream"))?,
				count: u64::try_from(count).unwrap_or(0),
				first: from_micros(first)?,
				last: from_micros(last)?,
			})
		})
		.collect()
	}

	/// Permanently delete records and series rows in `range` from the
	/// selected streams, and any media only they referenced. Returns how many
	/// records and series batches went.
	pub fn purge(&self, selector: &Selector, range: TimeRange) -> Result<(usize, usize)> {
		if selector.is_empty() {
			return Ok((0, 0));
		}
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let mut params: Vec<Value> = vec![micros(range.start).into(), micros(range.end).into()];
		let condition = selector.sql("", &mut params);
		let records = tx.execute(
			&format!("DELETE FROM records WHERE start_us >= ? AND start_us < ? AND {condition}"),
			params_from_iter(params.iter()),
		)?;
		let series = tx.execute(
			&format!("DELETE FROM series WHERE start_us >= ? AND start_us < ? AND {condition}"),
			params_from_iter(params.iter()),
		)?;
		tx.execute(
			"UPDATE media SET expires_us = 0
			 WHERE hash NOT IN (SELECT media_hash FROM records WHERE media_hash IS NOT NULL)",
			[],
		)?;
		tx.commit()?;
		Ok((records, series))
	}

	/// Sum the time event records in `stream` cover, grouped by the record
	/// attribute `group_by` and bucketed by start time. Instants count with
	/// zero duration.
	pub fn durations(
		&self,
		stream: &StreamId,
		range: TimeRange,
		group_by: Option<&str>,
		bucket: jiff::SignedDuration,
	) -> Result<Vec<DurationBucket>> {
		if let Some(key) = group_by
			&& (key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
		{
			return Err(StoreError::Invalid(format!("bad group key `{key}`")));
		}
		let width = i64::try_from(bucket.as_micros())
			.unwrap_or(i64::MAX)
			.max(1_000_000);
		let (start, end) = (micros(range.start), micros(range.end));
		let group = group_by.map_or_else(
			|| "NULL".to_owned(),
			|key| format!("json_extract(data, '$.{key}')"),
		);
		let sql = format!(
			"SELECT (max(start_us, ?1) - ?1) / ?3 AS slot, {group} AS grp, count(*),
				sum(max(0, min(coalesce(end_us, start_us), ?2) - max(start_us, ?1)))
			 FROM records
			 WHERE (stream = ?4 OR stream LIKE ?5) AND start_us < ?2 AND coalesce(end_us, start_us) >= ?1
			 GROUP BY slot, grp ORDER BY slot, 4 DESC"
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(
			params![
				start,
				end,
				width,
				stream.as_str(),
				format!("{}.%", stream.as_str())
			],
			|row| {
				let group: Option<rusqlite::types::Value> = row.get(1)?;
				Ok((
					row.get::<_, i64>(0)?,
					group,
					row.get::<_, i64>(2)?,
					row.get::<_, i64>(3)?,
				))
			},
		)?;
		rows.map(|row| {
			let (slot, group, count, covered) = row?;
			let group = group.and_then(|value| {
				match value {
					rusqlite::types::Value::Text(text) => Some(text),
					rusqlite::types::Value::Integer(n) => Some(n.to_string()),
					rusqlite::types::Value::Real(n) => Some(n.to_string()),
					_ => None,
				}
			});
			#[allow(clippy::cast_precision_loss)]
			let seconds = covered as f64 / 1_000_000.0;
			Ok(DurationBucket {
				start: from_micros(start + slot * width)?,
				group,
				count: u64::try_from(count).unwrap_or(0),
				seconds,
			})
		})
		.collect()
	}

	/// The stream and time of the earliest record referencing media `hash`.
	pub fn media_owner(&self, hash: &str) -> Result<Option<(StreamId, Timestamp)>> {
		let row: Option<(String, i64)> = self
			.conn()
			.query_row(
				"SELECT stream, start_us FROM records WHERE media_hash = ?1 ORDER BY start_us \
				 LIMIT 1",
				[hash],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.optional()?;
		row.map(|(stream, start)| {
			Ok((
				StreamId::new(stream).map_err(|_| StoreError::Corrupt("stream"))?,
				from_micros(start)?,
			))
		})
		.transpose()
	}

	/// Remember a custom stream's spec.
	pub fn register_stream(&self, spec: &quant_core::StreamSpec) -> Result<()> {
		self.conn().execute(
			"INSERT INTO streams (id, spec) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET spec = \
			 excluded.spec",
			params![spec.id.as_str(), serde_json::to_string(spec)?],
		)?;
		Ok(())
	}

	/// Custom stream specs.
	pub fn custom_streams(&self) -> Result<Vec<quant_core::StreamSpec>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached("SELECT spec FROM streams ORDER BY id")?;
		let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
		rows.map(|spec| Ok(serde_json::from_str(&spec?)?)).collect()
	}

	/// Record that `device` was heard from.
	pub fn touch_device(&self, device: DeviceId, name: &str, platform: &str) -> Result<()> {
		let now = micros(Timestamp::now());
		self.conn().execute(
			"INSERT INTO devices (id, name, platform, first_seen_us, last_seen_us) VALUES (?1, \
			 ?2, ?3, ?4, ?4)
			 ON CONFLICT (id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
			 last_seen_us = excluded.last_seen_us",
			params![device.0.as_bytes().as_slice(), name, platform, now],
		)?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;
	use quant_core::Family;

	use super::*;
	use crate::test_support::store;

	fn at(minutes: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_mins(minutes)
	}

	fn range(from: i64, to: i64) -> TimeRange {
		TimeRange::new(at(from), at(to)).unwrap()
	}

	fn frame(minute: i64, text: &str) -> Record {
		Record::new("screen.frame".parse().unwrap(), DeviceId::new())
			.starting(at(minute))
			.with_text(text)
			.with("app", "Zed")
	}

	#[test]
	fn stores_and_reads_back_records() {
		let store = store();
		let record = frame(1, "hello world").with_media(MediaRef::of(b"img", "image/jpeg"));
		let report = store.insert_records(std::slice::from_ref(&record)).unwrap();
		assert_eq!(
			report,
			InsertReport {
				stored: 1,
				duplicate: 0
			}
		);
		assert_eq!(store.record(record.id).unwrap(), Some(record.clone()));

		let again = store.insert_records(std::slice::from_ref(&record)).unwrap();
		assert_eq!(
			again,
			InsertReport {
				stored: 0,
				duplicate: 1
			}
		);
	}

	#[test]
	fn queries_respect_range_order_limit_and_selector() {
		let store = store();
		let records: Vec<_> = (0..10).map(|m| frame(m, &format!("frame {m}"))).collect();
		store.insert_records(&records).unwrap();
		let hr = Record::new("health.sleep".parse().unwrap(), DeviceId::new()).starting(at(5));
		store.insert_records(&[hr]).unwrap();

		let found = store
			.records(&RecordQuery::new(
				Selector::family(Family::Screen),
				range(2, 6),
			))
			.unwrap();
		let minutes: Vec<_> = found.iter().map(|r| r.text.clone().unwrap()).collect();
		assert_eq!(minutes, ["frame 5", "frame 4", "frame 3", "frame 2"]);

		let oldest = store
			.records(
				&RecordQuery::new(Selector::all(), range(0, 100))
					.oldest_first()
					.limit(2),
			)
			.unwrap();
		assert_eq!(oldest.len(), 2);
		assert_eq!(oldest[0].text.as_deref(), Some("frame 0"));

		let none = store
			.records(&RecordQuery::new(Selector::default(), range(0, 100)))
			.unwrap();
		assert!(none.is_empty());
	}

	#[test]
	fn spans_overlapping_the_range_start_are_included() {
		let store = store();
		let span = Record::new("apps.focus".parse().unwrap(), DeviceId::new())
			.starting(at(0))
			.ending(at(30));
		store.insert_records(std::slice::from_ref(&span)).unwrap();
		let found = store
			.records(&RecordQuery::new(Selector::all(), range(10, 20)))
			.unwrap();
		assert_eq!(found, vec![span]);
	}

	#[test]
	fn full_text_search_finds_and_snippets() {
		let store = store();
		store
			.insert_records(&[
				frame(1, "Reviewing the quarterly budget with Ada"),
				frame(2, "Lunch order: ramen"),
				frame(3, "Budgeting app — café receipts"),
			])
			.unwrap();
		let query = RecordQuery::new(Selector::all(), range(0, 10)).search("budget*");
		let hits = store.search(&query).unwrap();
		assert_eq!(hits.len(), 2);
		assert!(hits.iter().any(|h| h.snippet.contains("[budget]")));

		let accents = store
			.search(&RecordQuery::new(Selector::all(), range(0, 10)).search("cafe"))
			.unwrap();
		assert_eq!(accents.len(), 1);
	}

	#[test]
	fn search_terms_cannot_inject_fts_syntax() {
		assert_eq!(fts_query("a OR b"), Some("\"a\" \"OR\" \"b\"".into()));
		assert_eq!(fts_query("\"); DROP"), Some("\"DROP\"".into()));
		assert_eq!(fts_query("*** --"), None);
		let store = store();
		store.insert_records(&[frame(1, "NEAR(a b)")]).unwrap();
		let query = RecordQuery::new(Selector::all(), range(0, 10)).search("NEAR( \"x");
		assert!(store.search(&query).is_ok());
	}

	#[test]
	fn overview_and_purge() {
		let store = store();
		store
			.insert_records(&(0..5).map(|m| frame(m, "x")).collect::<Vec<_>>())
			.unwrap();
		let overview = store.overview(&Selector::all(), range(0, 10)).unwrap();
		assert_eq!(overview.len(), 1);
		assert_eq!(overview[0].count, 5);
		assert_eq!(overview[0].first, at(0));

		let (records, _) = store
			.purge(&Selector::family(Family::Screen), range(0, 3))
			.unwrap();
		assert_eq!(records, 3);
		let left = store
			.records(&RecordQuery::new(Selector::all(), range(0, 10)))
			.unwrap();
		assert_eq!(left.len(), 2);
		let hits = store
			.search(&RecordQuery::new(Selector::all(), range(0, 10)).search("x"))
			.unwrap();
		assert_eq!(hits.len(), 2);
	}

	#[test]
	fn sums_durations_by_group_clipped_to_the_range() {
		let store = store();
		let focus = |from: i64, to: i64, app: &str| {
			Record::new("apps.focus".parse().unwrap(), DeviceId::new())
				.starting(at(from))
				.ending(at(to))
				.with("app", app)
				.with("title", "secret plans")
		};
		store
			.insert_records(&[
				focus(0, 30, "Zed"),
				focus(30, 40, "Safari"),
				focus(40, 90, "Zed"),
			])
			.unwrap();
		let stream: StreamId = "apps.focus".parse().unwrap();
		let buckets = store
			.durations(
				&stream,
				range(10, 70),
				Some("app"),
				jiff::SignedDuration::from_hours(24),
			)
			.unwrap();
		let zed = buckets
			.iter()
			.find(|b| b.group.as_deref() == Some("Zed"))
			.unwrap();
		assert_eq!(zed.count, 2);
		assert!((zed.seconds - 50.0 * 60.0).abs() < 1e-6);
		let safari = buckets
			.iter()
			.find(|b| b.group.as_deref() == Some("Safari"))
			.unwrap();
		assert!((safari.seconds - 600.0).abs() < 1e-6);
		assert!(
			store
				.durations(
					&stream,
					range(0, 1),
					Some("x') OR 1"),
					jiff::SignedDuration::from_hours(1)
				)
				.is_err()
		);
	}

	#[test]
	fn rejects_inverted_spans() {
		let store = store();
		let bad = frame(5, "x").ending(at(1));
		assert!(matches!(
			store.insert_records(&[bad]),
			Err(StoreError::Invalid(_))
		));
	}
}
