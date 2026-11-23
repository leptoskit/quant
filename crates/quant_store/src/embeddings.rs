//! Embedding vectors for records and claims, and the vector search built on
//! them.
//!
//! Vectors are stored per embedding `space`: a short hash of the embedding
//! model, its dimensions, and the chunking version, so switching models
//! re-embeds everything into a fresh space instead of mixing vectors. A
//! vector is a run of little-endian f32s, L2-normalised, and is never served
//! to clients: it is only ever compared on this machine.

use std::collections::HashMap;
use std::hash::Hash;

use quant_core::Claim;
use quant_core::Record;
use quant_core::RecordId;
use rusqlite::OptionalExtension;
use rusqlite::params_from_iter;
use rusqlite::types::Value;

use crate::ClaimFilter;
use crate::RecordQuery;
use crate::Result;
use crate::SearchHit;
use crate::Selector;
use crate::Store;
use crate::claims::COLUMNS as CLAIM_COLUMNS;
use crate::claims::attach_evidence;
use crate::claims::claim_from_row;
use crate::claims::filter_sql;
use crate::records::COLUMNS;
use crate::records::record_from_row;

/// Fuse two ranked lists of items with reciprocal rank fusion: an item's
/// fused score is the sum of `1 / (k + rank)` over the lists that rank it,
/// ranks counting from one, best first. The result is sorted by fused score
/// descending; ties keep the smaller item first so the order is
/// deterministic.
pub fn rrf_fuse<T: Ord + Hash + Clone>(a: &[T], b: &[T], k: u32) -> Vec<(T, f64)> {
	let mut scores: HashMap<T, f64> = HashMap::new();
	for (index, item) in a.iter().enumerate() {
		*scores.entry(item.clone()).or_default() += 1.0 / (f64::from(k) + index as f64 + 1.0);
	}
	for (index, item) in b.iter().enumerate() {
		*scores.entry(item.clone()).or_default() += 1.0 / (f64::from(k) + index as f64 + 1.0);
	}
	let mut fused: Vec<(T, f64)> = scores.into_iter().collect();
	fused.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then_with(|| a.cmp(b)));
	fused
}

/// A short hash naming one embedding space: the model, its dimensions, and
/// the chunking version that produced the texts. Two vaults share a space
/// only when they embed the same texts the same way; changing any of the
/// three starts a fresh space.
pub fn space_hash(model: &str, dims: usize, chunking: u32) -> String {
	let mut hasher = blake3::Hasher::new_derive_key("quant embedding space v1");
	hasher.update(model.as_bytes());
	hasher.update(&[0]);
	hasher.update(dims.to_string().as_bytes());
	hasher.update(&[0]);
	hasher.update(chunking.to_string().as_bytes());
	let mut hex = hasher.finalize().to_hex().to_string();
	hex.truncate(16);
	hex
}

/// The L2 norm of `vector`.
pub fn l2_norm(vector: &[f32]) -> f32 {
	let mut s0 = 0.0_f32;
	let mut s1 = 0.0_f32;
	let mut s2 = 0.0_f32;
	let mut s3 = 0.0_f32;
	let (chunks, rem) = vector.as_chunks::<4>();
	for chunk in chunks {
		s0 += chunk[0] * chunk[0];
		s1 += chunk[1] * chunk[1];
		s2 += chunk[2] * chunk[2];
		s3 += chunk[3] * chunk[3];
	}
	let mut sum = (s0 + s1) + (s2 + s3);
	for &x in rem {
		sum += x * x;
	}
	sum.sqrt()
}

/// Scale `vector` in place to unit L2 length. A zero-length vector stays as
/// it is.
pub fn l2_normalize(vector: &mut [f32]) {
	let norm = l2_norm(vector);
	if norm > 0.0 && norm.is_finite() {
		let inv = 1.0 / norm;
		for value in vector {
			*value *= inv;
		}
	}
}

/// The cosine similarity of two vectors, in `-1..=1`. Vectors of different
/// lengths compare only their shared prefix; a zero-length side scores 0.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
	let len = a.len().min(b.len());
	let a = &a[..len];
	let b = &b[..len];

	let mut dot0 = 0.0_f32;
	let mut dot1 = 0.0_f32;
	let mut dot2 = 0.0_f32;
	let mut dot3 = 0.0_f32;
	let mut aa0 = 0.0_f32;
	let mut aa1 = 0.0_f32;
	let mut aa2 = 0.0_f32;
	let mut aa3 = 0.0_f32;
	let mut bb0 = 0.0_f32;
	let mut bb1 = 0.0_f32;
	let mut bb2 = 0.0_f32;
	let mut bb3 = 0.0_f32;

	let (a_chunks, a_rem) = a.as_chunks::<4>();
	let (b_chunks, b_rem) = b.as_chunks::<4>();

	for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
		dot0 += ac[0] * bc[0];
		dot1 += ac[1] * bc[1];
		dot2 += ac[2] * bc[2];
		dot3 += ac[3] * bc[3];
		aa0 += ac[0] * ac[0];
		aa1 += ac[1] * ac[1];
		aa2 += ac[2] * ac[2];
		aa3 += ac[3] * ac[3];
		bb0 += bc[0] * bc[0];
		bb1 += bc[1] * bc[1];
		bb2 += bc[2] * bc[2];
		bb3 += bc[3] * bc[3];
	}

	let mut dot = (dot0 + dot1) + (dot2 + dot3);
	let mut aa = (aa0 + aa1) + (aa2 + aa3);
	let mut bb = (bb0 + bb1) + (bb2 + bb3);

	for (&x, &y) in a_rem.iter().zip(b_rem.iter()) {
		dot += x * y;
		aa += x * x;
		bb += y * y;
	}

	if aa == 0.0 || bb == 0.0 {
		0.0
	} else {
		dot / (aa.sqrt() * bb.sqrt())
	}
}

/// Cosine similarity between a query slice (with precomputed `query_norm` when
/// the blob length matches `query.len()`) and a little-endian f32 SQLite blob,
/// without allocating intermediate `Vec<u8>` or `Vec<f32>` buffers.
#[inline]
fn cosine_le_bytes(query: &[f32], query_norm: f32, blob: &[u8]) -> f32 {
	if blob.len() % 4 != 0 || query.is_empty() || blob.is_empty() {
		return 0.0;
	}
	let blob_dims = blob.len() / 4;
	let len = query.len().min(blob_dims);
	let q = &query[..len];
	let bytes = &blob[..len * 4];

	let mut dot0 = 0.0_f32;
	let mut dot1 = 0.0_f32;
	let mut dot2 = 0.0_f32;
	let mut dot3 = 0.0_f32;
	let mut bb0 = 0.0_f32;
	let mut bb1 = 0.0_f32;
	let mut bb2 = 0.0_f32;
	let mut bb3 = 0.0_f32;

	let (q_chunks, q_rem) = q.as_chunks::<4>();
	let (b_chunks, b_rem) = bytes.as_chunks::<16>();

	for (qc, bc) in q_chunks.iter().zip(b_chunks.iter()) {
		let y0 = f32::from_le_bytes([bc[0], bc[1], bc[2], bc[3]]);
		let y1 = f32::from_le_bytes([bc[4], bc[5], bc[6], bc[7]]);
		let y2 = f32::from_le_bytes([bc[8], bc[9], bc[10], bc[11]]);
		let y3 = f32::from_le_bytes([bc[12], bc[13], bc[14], bc[15]]);
		dot0 += qc[0] * y0;
		dot1 += qc[1] * y1;
		dot2 += qc[2] * y2;
		dot3 += qc[3] * y3;
		bb0 += y0 * y0;
		bb1 += y1 * y1;
		bb2 += y2 * y2;
		bb3 += y3 * y3;
	}

	let mut dot = (dot0 + dot1) + (dot2 + dot3);
	let mut bb = (bb0 + bb1) + (bb2 + bb3);

	for (&x, bc) in q_rem.iter().zip(b_rem.as_chunks::<4>().0) {
		let y = f32::from_le_bytes(*bc);
		dot += x * y;
		bb += y * y;
	}

	if bb == 0.0 {
		return 0.0;
	}

	let q_norm = if len == query.len() {
		query_norm
	} else {
		l2_norm(q)
	};
	if q_norm == 0.0 {
		0.0
	} else {
		dot / (q_norm * bb.sqrt())
	}
}

/// Encode `vector` as little-endian f32 bytes, the on-disk form.
pub fn vector_to_bytes(vector: &[f32]) -> Vec<u8> {
	let mut out = Vec::with_capacity(vector.len() * 4);
	for value in vector {
		out.extend_from_slice(&value.to_le_bytes());
	}
	out
}

/// Decode little-endian f32 bytes back into a vector. A byte length that is
/// not a multiple of four decodes as an empty vector.
pub fn vector_from_bytes(bytes: &[u8]) -> Vec<f32> {
	if bytes.len() % 4 != 0 {
		return Vec::new();
	}
	let mut out = Vec::with_capacity(bytes.len() / 4);
	for chunk in bytes.as_chunks::<4>().0 {
		out.push(f32::from_le_bytes(*chunk));
	}
	out
}

impl Store {
	/// Records with text and no embedding in `space`, oldest first, at most
	/// `limit`. Used by the backfill job to drain un-embedded texts in order.
	pub fn pending_embeddings(
		&self,
		space: &str,
		selector: &Selector,
		limit: usize,
	) -> Result<Vec<(i64, String)>> {
		if selector.is_empty() || limit == 0 {
			return Ok(Vec::new());
		}
		let mut params: Vec<Value> = vec![space.to_owned().into()];
		let condition = selector.sql("r.", &mut params);
		params.push(i64::try_from(limit).unwrap_or(i64::MAX).into());
		let limit_index = params.len();
		let sql = format!(
			"SELECT r.rowid, r.text FROM records r
			 WHERE r.text IS NOT NULL
			 AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.record = r.rowid AND e.space = ?1)
			 AND {condition}
			 ORDER BY r.rowid LIMIT ?{limit_index}"
		);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let rows = statement.query_map(params_from_iter(params), |row| {
			Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
		})?;
		rows.map(|row| row.map_err(Into::into)).collect()
	}

	/// Store the embedding vector of record `rowid` in `space`, replacing
	/// any earlier one.
	pub fn set_embedding(&self, space: &str, rowid: i64, bytes: &[u8]) -> Result<()> {
		self.conn().execute(
			"INSERT INTO embeddings (record, space, vector) VALUES (?1, ?2, ?3)
			 ON CONFLICT (record, space) DO UPDATE SET vector = excluded.vector",
			rusqlite::params![rowid, space, bytes],
		)?;
		Ok(())
	}

	/// Store the embedding vector of claim `rowid` in `space`, replacing
	/// any earlier one.
	pub fn set_claim_embedding(&self, space: &str, rowid: i64, bytes: &[u8]) -> Result<()> {
		self.conn().execute(
			"INSERT INTO claim_embeddings (claim, space, vector) VALUES (?1, ?2, ?3)
			 ON CONFLICT (claim, space) DO UPDATE SET vector = excluded.vector",
			rusqlite::params![rowid, space, bytes],
		)?;
		Ok(())
	}

	/// Whether any record embedding exists in `space`.
	pub fn has_embeddings(&self, space: &str) -> Result<bool> {
		let count: i64 = self
			.conn()
			.query_row(
				"SELECT count(*) FROM (SELECT 1 FROM embeddings WHERE space = ?1 LIMIT 1)",
				[space],
				|row| row.get(0),
			)
			.optional()?
			.unwrap_or(0);
		Ok(count > 0)
	}

	/// Whether any claim embedding exists in `space`.
	pub fn has_claim_embeddings(&self, space: &str) -> Result<bool> {
		let count: i64 = self
			.conn()
			.query_row(
				"SELECT count(*) FROM (SELECT 1 FROM claim_embeddings WHERE space = ?1 LIMIT 1)",
				[space],
				|row| row.get(0),
			)
			.optional()?
			.unwrap_or(0);
		Ok(count > 0)
	}

	/// Records most similar to `vector` in `space`, within `query`'s
	/// selector and range, best first. Vectors never leave the store; the
	/// caller passes the query's embedding in.
	pub fn similar(
		&self,
		query: &RecordQuery,
		space: &str,
		vector: &[f32],
	) -> Result<Vec<SearchHit>> {
		const POOL: usize = 2_000;
		if query.selector.is_empty() || query.limit == 0 {
			return Ok(Vec::new());
		}
		let mut params: Vec<Value> = vec![
			space.to_owned().into(),
			crate::micros(query.range.end).into(),
			crate::micros(query.range.start).into(),
			crate::micros(query.range.start).into(),
		];
		let selector = query.selector.sql("r.", &mut params);
		params.push(i64::try_from(POOL).unwrap_or(i64::MAX).into());
		let limit_index = params.len();
		let sql = format!(
			"SELECT r.rowid, r.start_us, e.vector
			 FROM records r CROSS JOIN embeddings e ON e.record = r.rowid AND e.space = ?1
			 WHERE r.start_us < ?2 AND (r.start_us >= ?3 OR coalesce(r.end_us, r.start_us) > ?4)
			 AND {selector}
			 ORDER BY r.start_us DESC LIMIT ?{limit_index}"
		);
		let query_norm = l2_norm(vector);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let mut scored: Vec<(f32, i64, i64)> = Vec::new();
		let mut rows = statement.query(params_from_iter(params))?;
		while let Some(row) = rows.next()? {
			let rowid: i64 = row.get(0)?;
			let start_us: i64 = row.get(1)?;
			let blob = row.get_ref(2)?.as_blob().map_err(rusqlite::Error::from)?;
			let score = cosine_le_bytes(vector, query_norm, blob);
			scored.push((score, rowid, start_us));
		}
		drop(rows);
		drop(statement);

		let cmp = |a: &(f32, i64, i64), b: &(f32, i64, i64)| {
			b.0.total_cmp(&a.0).then_with(|| b.2.cmp(&a.2))
		};
		let top = query.limit.min(scored.len());
		if top < scored.len() {
			scored.select_nth_unstable_by(top, cmp);
			scored.truncate(top);
		}
		scored.sort_by(cmp);

		let mut fetch = conn.prepare_cached(&format!(
			"SELECT {COLUMNS} FROM records r WHERE r.rowid = ?1"
		))?;
		let mut hits = Vec::with_capacity(top);
		for &(_, rowid, _) in &scored {
			let record: Option<Record> = fetch
				.query_row([rowid], record_from_row)
				.optional()?
				.transpose()?;
			let Some(record) = record else {
				continue;
			};
			hits.push(SearchHit {
				record,
				snippet: String::new(),
				rank: 0.0,
			});
		}
		Ok(hits)
	}

	/// Hybrid search: keyword matches and vector matches over `query`,
	/// fused with reciprocal rank fusion. Keyword hits keep their snippets;
	/// vector hits have none. The result is at most `query.limit` records,
	/// best first.
	pub fn hybrid(
		&self,
		query: &RecordQuery,
		space: &str,
		vector: &[f32],
	) -> Result<Vec<SearchHit>> {
		let keyword = self.search(query)?;
		let similar = self.similar(query, space, vector)?;
		if keyword.is_empty() {
			return Ok(similar);
		}
		if similar.is_empty() {
			return Ok(keyword);
		}
		let keyword_ids: Vec<RecordId> = keyword.iter().map(|hit| hit.record.id).collect();
		let similar_ids: Vec<RecordId> = similar.iter().map(|hit| hit.record.id).collect();
		let keyword_map: HashMap<RecordId, SearchHit> = keyword
			.into_iter()
			.map(|hit| (hit.record.id, hit))
			.collect();
		let similar_map: HashMap<RecordId, SearchHit> = similar
			.into_iter()
			.map(|hit| (hit.record.id, hit))
			.collect();
		let mut hits = Vec::new();
		for (id, _) in rrf_fuse(&keyword_ids, &similar_ids, 60) {
			let hit = if keyword_map.contains_key(&id) {
				&keyword_map[&id]
			} else {
				&similar_map[&id]
			};
			hits.push(hit.clone());
			if hits.len() >= query.limit {
				break;
			}
		}
		Ok(hits)
	}

	/// Claims matching `filter` whose embeddings in `space` are most similar
	/// to `vector`, best first, with their evidence.
	pub fn similar_claims(
		&self,
		space: &str,
		filter: &ClaimFilter,
		vector: &[f32],
		limit: usize,
	) -> Result<Vec<Claim>> {
		if limit == 0 {
			return Ok(Vec::new());
		}
		let mut params: Vec<Value> = vec![space.to_owned().into()];
		let condition = filter_sql(filter, &mut params);
		params.push(2_000_i64.into());
		let limit_index = params.len();
		// Sort and limit lightweight `(rowid, updated_at_us)` pairs before
		// looking up the embedding blobs so SQLite never spills multi-kilobyte
		// vectors into a temporary B-tree sorter.
		let sql = format!(
			"WITH candidates AS MATERIALIZED (
				SELECT c.rowid AS claim_rowid
				FROM claim_embeddings ce
				JOIN claims c ON c.rowid = ce.claim
				WHERE ce.space = ?1 AND {condition}
				ORDER BY c.updated_at_us DESC
				LIMIT ?{limit_index}
			 )
			 SELECT candidates.claim_rowid, ce.vector
			 FROM candidates
			 JOIN claim_embeddings ce ON ce.claim = candidates.claim_rowid AND ce.space = ?1"
		);
		let query_norm = l2_norm(vector);
		let conn = self.conn();
		let mut statement = conn.prepare_cached(&sql)?;
		let mut scored: Vec<(f32, usize, i64)> = Vec::new();
		let mut rows = statement.query(params_from_iter(params))?;
		while let Some(row) = rows.next()? {
			let rowid: i64 = row.get(0)?;
			let blob = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
			let score = cosine_le_bytes(vector, query_norm, blob);
			let ord = scored.len();
			scored.push((score, ord, rowid));
		}
		drop(rows);
		drop(statement);

		let cmp = |a: &(f32, usize, i64), b: &(f32, usize, i64)| {
			b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1))
		};
		let top = limit.min(scored.len());
		if top < scored.len() {
			scored.select_nth_unstable_by(top, cmp);
			scored.truncate(top);
		}
		scored.sort_by(cmp);

		let mut fetch = conn.prepare_cached(&format!(
			"SELECT {CLAIM_COLUMNS} FROM claims c WHERE c.rowid = ?1"
		))?;
		let mut claims: Vec<Claim> = Vec::with_capacity(top);
		for &(_, _, rowid) in &scored {
			if let Some(claim) = fetch
				.query_row([rowid], claim_from_row)
				.optional()?
				.transpose()?
			{
				claims.push(claim);
			}
		}
		drop(fetch);
		attach_evidence(&conn, &mut claims)?;
		Ok(claims)
	}
}

#[cfg(test)]
mod tests {
	use jiff::SignedDuration;
	use jiff::Timestamp;
	use quant_core::DeviceId;
	use quant_core::Record;
	use quant_core::TimeRange;

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
	}

	#[test]
	fn rrf_fuses_two_ranked_lists() {
		// A small k makes the scores easy to reason about: 2 is ranked in
		// both lists and wins outright.
		assert_eq!(
			rrf_fuse(&[1, 2], &[2], 1),
			vec![(2, 1.0 / 3.0 + 1.0 / 2.0), (1, 1.0 / 2.0)]
		);
		// With k = 60 an item ranked first in one list and third in the
		// other edges out one ranked second in both.
		let fused = rrf_fuse(&[3, 2, 1], &[1, 58, 2], 60);
		let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
		assert_eq!(ids, vec![1, 2, 3, 58]);
		assert_eq!(rrf_fuse(&[1], &[2], 60).len(), 2);
		// Fusing a list with itself doubles each score but keeps the order.
		assert_eq!(rrf_fuse(&[9, 8], &[9, 8], 60)[0].0, 9);
	}

	#[test]
	fn space_hashes_differ_by_model_dims_and_chunking() {
		let base = space_hash("nomic-embed-text", 768, 1);
		assert_eq!(base, space_hash("nomic-embed-text", 768, 1));
		assert_ne!(base, space_hash("other-model", 768, 1));
		assert_ne!(base, space_hash("nomic-embed-text", 384, 1));
		assert_ne!(base, space_hash("nomic-embed-text", 768, 2));
		assert_eq!(base.len(), 16);
	}

	#[test]
	fn vectors_round_trip_through_bytes_and_normalize() {
		let mut vector = vec![3.0_f32, 4.0, 0.0];
		l2_normalize(&mut vector);
		assert!((l2_norm(&vector) - 1.0).abs() < 1e-6);
		let bytes = vector_to_bytes(&vector);
		let back = vector_from_bytes(&bytes);
		assert_eq!(back, vector);
		assert!(cosine(&vector, &back) > 0.999);
		assert!(vector_from_bytes(&[1, 2, 3]).is_empty());
		// A zero vector normalises to itself and scores nothing.
		let mut zero = vec![0.0_f32, 0.0];
		l2_normalize(&mut zero);
		assert_eq!(cosine(&zero, &vector), 0.0);
	}

	#[test]
	fn cosine_measures_direction_only() {
		let a = vec![1.0_f32, 0.0];
		let b = vec![2.0_f32, 0.0];
		let c = vec![0.0_f32, 1.0];
		assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
		assert!(cosine(&a, &c).abs() < 1e-6);
	}

	#[test]
	fn embeddings_backfill_and_search_in_one_space() {
		let store = store();
		let space = space_hash("fake", 4, 1);
		let selector = Selector::all();
		store
			.insert_records(&[
				frame(1, "espresso machine maintenance"),
				frame(2, "coffee brewing guide"),
				frame(3, "quarterly budget numbers"),
			])
			.unwrap();

		let pending = store.pending_embeddings(&space, &selector, 200).unwrap();
		assert_eq!(pending.len(), 3);
		// Oldest first.
		assert_eq!(pending[0].1, "espresso machine maintenance");

		let records = store
			.records(&RecordQuery::new(selector.clone(), range(0, 10)))
			.unwrap();
		let rowid_of = |id: quant_core::RecordId| {
			store
				.conn()
				.query_row(
					"SELECT rowid FROM records WHERE id = ?1",
					[id.0.as_bytes().as_slice()],
					|r| r.get(0),
				)
				.unwrap()
		};
		let rowids: std::collections::BTreeMap<String, i64> = records
			.iter()
			.map(|r| (r.text.clone().unwrap_or_default(), rowid_of(r.id)))
			.collect();
		// Embed the two coffee records with nearby directions, the budget
		// record far away.
		for (text, vector) in [
			("espresso machine maintenance", vec![1.0_f32, 0.0, 0.0, 0.0]),
			("coffee brewing guide", vec![0.9_f32, 0.1, 0.0, 0.0]),
			("quarterly budget numbers", vec![0.0_f32, 0.0, 0.0, 1.0]),
		] {
			store
				.set_embedding(&space, rowids[text], &vector_to_bytes(&vector))
				.unwrap();
		}
		assert!(store.has_embeddings(&space).unwrap());
		assert!(
			store
				.pending_embeddings(&space, &selector, 200)
				.unwrap()
				.is_empty()
		);

		// A query vector near the coffee records returns them first.
		let query = RecordQuery::new(selector, range(0, 10)).limit(2);
		let hits = store
			.similar(&query, &space, &[1.0_f32, 0.0, 0.0, 0.0])
			.unwrap();
		let texts: Vec<_> = hits
			.iter()
			.map(|h| h.record.text.as_deref().unwrap())
			.collect();
		assert_eq!(
			texts,
			["espresso machine maintenance", "coffee brewing guide"]
		);

		// Hybrid fuses the keyword ranking with the vector ranking; the
		// keyword-matching budget record appears among the results.
		let hybrid = store
			.hybrid(
				&RecordQuery::new(Selector::all(), range(0, 10))
					.search("budget")
					.limit(3),
				&space,
				&[1.0_f32, 0.0, 0.0, 0.0],
			)
			.unwrap();
		assert_eq!(hybrid.len(), 3);
		assert!(
			hybrid
				.iter()
				.any(|h| h.record.text.as_deref() == Some("quarterly budget numbers"))
		);
	}

	#[test]
	fn pending_embeddings_respects_multi_part_selectors() {
		let store = store();
		let space = space_hash("fake", 4, 1);
		store
			.insert_records(&[frame(1, "screen text one"), frame(2, "screen text two")])
			.unwrap();
		let mut selector = Selector::default();
		selector.families.push(quant_core::Family::Screen);
		selector.streams.push("audio.transcript".parse().unwrap());
		selector.streams.push("memory.note".parse().unwrap());
		let pending = store.pending_embeddings(&space, &selector, 200).unwrap();
		assert_eq!(pending.len(), 2, "{pending:?}");
	}

	#[test]
	fn embeddings_follow_records_on_delete() {
		let store = store();
		let record = frame(1, "grounding");
		store.insert_records(std::slice::from_ref(&record)).unwrap();
		let space = space_hash("fake", 4, 1);
		let rowid: i64 = store
			.conn()
			.query_row(
				"SELECT rowid FROM records WHERE id = ?1",
				[record.id.0.as_bytes().as_slice()],
				|r| r.get(0),
			)
			.unwrap();
		store
			.set_embedding(&space, rowid, &vector_to_bytes(&[1.0_f32, 0.0, 0.0, 0.0]))
			.unwrap();
		store.delete_record_rows(&[rowid]).unwrap();
		let left: i64 = store
			.conn()
			.query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
			.unwrap();
		assert_eq!(left, 0);
	}
}
