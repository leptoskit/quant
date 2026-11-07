use std::collections::BTreeMap;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::Family;
use quant_core::SeriesBatch;
use quant_core::StreamId;
use quant_core::TimeRange;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::Result;
use crate::Store;
use crate::StoreError;
use crate::from_micros;
use crate::micros;

/// Running statistics for one channel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelStats {
	/// Samples seen.
	pub count: u64,
	/// Smallest value.
	pub min: f64,
	/// Largest value.
	pub max: f64,
	/// Sum of values.
	pub sum: f64,
}

impl Default for ChannelStats {
	fn default() -> Self {
		Self {
			count: 0,
			min: f64::INFINITY,
			max: f64::NEG_INFINITY,
			sum: 0.0,
		}
	}
}

impl ChannelStats {
	fn push(&mut self, value: f64) {
		if value.is_finite() {
			self.count += 1;
			self.min = self.min.min(value);
			self.max = self.max.max(value);
			self.sum += value;
		}
	}

	fn merge(&mut self, other: &ChannelStats) {
		self.count += other.count;
		self.min = self.min.min(other.min);
		self.max = self.max.max(other.max);
		self.sum += other.sum;
	}

	/// The mean, if any samples were seen.
	pub fn mean(&self) -> Option<f64> {
		#[allow(clippy::cast_precision_loss)]
		(self.count > 0).then(|| self.sum / self.count as f64)
	}
}

/// Aggregated samples over one time bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeriesBucket {
	/// Start of the bucket.
	pub start: Timestamp,
	/// Per-channel statistics, by channel name.
	pub channels: BTreeMap<String, ChannelStats>,
}

const F32: u8 = 1;
const F64: u8 = 2;

/// Location needs f64 to stay accurate to the metre; every other sensor is
/// well inside f32's precision and halves its size.
fn encoding_for(stream: &StreamId) -> u8 {
	if stream.family() == Family::Location {
		F64
	} else {
		F32
	}
}

fn encode(batch: &SeriesBatch) -> Vec<u8> {
	let encoding = encoding_for(&batch.stream);
	let width = if encoding == F64 { 8 } else { 4 };
	let mut body = Vec::with_capacity(1 + batch.offsets_ms.len() * 4 + batch.values.len() * width);
	body.push(encoding);
	for offset in &batch.offsets_ms {
		body.extend_from_slice(&offset.to_le_bytes());
	}
	for value in &batch.values {
		if encoding == F64 {
			body.extend_from_slice(&value.to_le_bytes());
		} else {
			#[allow(clippy::cast_possible_truncation)]
			body.extend_from_slice(&(*value as f32).to_le_bytes());
		}
	}
	body
}

fn decode(body: &[u8], rows: usize, channels: usize) -> Result<(Vec<u32>, Vec<f64>)> {
	let corrupt = || StoreError::Corrupt("series body");
	let (&encoding, rest) = body.split_first().ok_or_else(corrupt)?;
	let width = match encoding {
		F32 => 4,
		F64 => 8,
		_ => return Err(corrupt()),
	};
	if rest.len() != rows * 4 + rows * channels * width {
		return Err(corrupt());
	}
	let (offsets, values) = rest.split_at(rows * 4);
	let offsets = offsets
		.as_chunks::<4>()
		.0
		.iter()
		.map(|c| u32::from_le_bytes(*c))
		.collect();
	let values = values
		.chunks_exact(width)
		.map(|c| {
			if width == 8 {
				f64::from_le_bytes(c.try_into().expect("chunk of 8"))
			} else {
				f64::from(f32::from_le_bytes(c.try_into().expect("chunk of 4")))
			}
		})
		.collect();
	Ok((offsets, values))
}

fn stats(batch: &SeriesBatch) -> Vec<ChannelStats> {
	let mut stats = vec![ChannelStats::default(); batch.channels.len()];
	for (_, row) in batch.rows() {
		for (stat, value) in stats.iter_mut().zip(row) {
			stat.push(*value);
		}
	}
	stats
}

struct StoredBatch {
	stream: String,
	device: Vec<u8>,
	start: i64,
	end: i64,
	rows: usize,
	channels: Vec<String>,
	stats: Vec<ChannelStats>,
	body: Vec<u8>,
}

impl StoredBatch {
	fn decode(self) -> Result<SeriesBatch> {
		let (offsets_ms, values) = decode(&self.body, self.rows, self.channels.len())?;
		Ok(SeriesBatch {
			stream: StreamId::new(self.stream).map_err(|_| StoreError::Corrupt("stream"))?,
			device: DeviceId(
				Uuid::from_slice(&self.device).map_err(|_| StoreError::Corrupt("device"))?,
			),
			start: from_micros(self.start)?,
			channels: self.channels,
			offsets_ms,
			values,
		})
	}
}

impl Store {
	/// Store a series batch. Sending the same batch twice stores it once.
	/// Returns whether it was new.
	pub fn insert_series(&self, batch: &SeriesBatch) -> Result<bool> {
		batch
			.validate()
			.map_err(|e| StoreError::Invalid(e.to_string()))?;
		if batch.is_empty() {
			return Ok(false);
		}
		let body = encode(batch);
		let mut hasher = blake3::Hasher::new();
		hasher.update(batch.stream.as_str().as_bytes());
		hasher.update(batch.device.0.as_bytes());
		hasher.update(&micros(batch.start).to_le_bytes());
		hasher.update(serde_json::to_string(&batch.channels)?.as_bytes());
		hasher.update(&body);
		let changed = self.conn().execute(
			"INSERT OR IGNORE INTO series (hash, stream, family, device, start_us, end_us, rows, \
			 channels, stats, body)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
			params![
				hasher.finalize().as_bytes().as_slice(),
				batch.stream.as_str(),
				batch.stream.family().as_str(),
				batch.device.0.as_bytes().as_slice(),
				micros(batch.start),
				micros(batch.end()),
				i64::try_from(batch.len()).unwrap_or(i64::MAX),
				serde_json::to_string(&batch.channels)?,
				serde_json::to_string(&stats(batch))?,
				body,
			],
		)?;
		Ok(changed > 0)
	}

	fn stored_batches(
		&self,
		stream: &StreamId,
		range: TimeRange,
		with_body: bool,
	) -> Result<Vec<StoredBatch>> {
		let conn = self.conn();
		let body = if with_body { "body" } else { "x''" };
		let mut statement = conn.prepare_cached(&format!(
			"SELECT stream, device, start_us, end_us, rows, channels, stats, {body} FROM series
			 WHERE (stream = ?1 OR stream LIKE ?2) AND start_us < ?3 AND end_us >= ?4
			 ORDER BY start_us"
		))?;
		let rows = statement.query_map(
			params![
				stream.as_str(),
				format!("{}.%", stream.as_str()),
				micros(range.end),
				micros(range.start)
			],
			|row| {
				Ok((
					row.get::<_, String>(0)?,
					row.get::<_, Vec<u8>>(1)?,
					row.get::<_, i64>(2)?,
					row.get::<_, i64>(3)?,
					row.get::<_, i64>(4)?,
					row.get::<_, String>(5)?,
					row.get::<_, String>(6)?,
					row.get::<_, Vec<u8>>(7)?,
				))
			},
		)?;
		rows.map(|row| {
			let (stream, device, start, end, rows, channels, stats, body) = row?;
			Ok(StoredBatch {
				stream,
				device,
				start,
				end,
				rows: usize::try_from(rows).map_err(|_| StoreError::Corrupt("series rows"))?,
				channels: serde_json::from_str(&channels)?,
				stats: serde_json::from_str(&stats)?,
				body,
			})
		})
		.collect()
	}

	/// The oldest `limit` series batches with their row numbers, for
	/// draining a spool in order.
	pub fn oldest_series(&self, limit: usize) -> Result<Vec<(i64, SeriesBatch)>> {
		let conn = self.conn();
		let mut statement = conn.prepare_cached(
			"SELECT rowid, stream, device, start_us, end_us, rows, channels, stats, body FROM \
			 series ORDER BY rowid LIMIT ?1",
		)?;
		let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
			Ok((
				row.get::<_, i64>(0)?,
				StoredBatch {
					stream: row.get(1)?,
					device: row.get(2)?,
					start: row.get(3)?,
					end: row.get(4)?,
					rows: usize::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
					channels: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
					stats: Vec::new(),
					body: row.get(8)?,
				},
			))
		})?;
		rows.map(|row| {
			let (rowid, stored) = row?;
			Ok((rowid, stored.decode()?))
		})
		.collect()
	}

	/// Delete series batches by row number.
	pub fn delete_series_rows(&self, rows: &[i64]) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		{
			let mut delete = tx.prepare_cached("DELETE FROM series WHERE rowid = ?1")?;
			for row in rows {
				delete.execute([row])?;
			}
		}
		tx.commit()?;
		Ok(())
	}

	/// Raw samples of `stream` (and its children) in `range`, trimmed to the
	/// range and to at most `max_rows` rows in total.
	pub fn series(
		&self,
		stream: &StreamId,
		range: TimeRange,
		max_rows: usize,
	) -> Result<Vec<SeriesBatch>> {
		let mut out = Vec::new();
		let mut budget = max_rows;
		for stored in self.stored_batches(stream, range, true)? {
			if budget == 0 {
				break;
			}
			let batch = stored.decode()?;
			let width = batch.channels.len();
			let mut trimmed = SeriesBatch {
				offsets_ms: Vec::new(),
				values: Vec::new(),
				..batch.clone()
			};
			for (i, (t, row)) in batch.rows().enumerate() {
				if budget == 0 {
					break;
				}
				if range.contains(t) {
					trimmed.offsets_ms.push(batch.offsets_ms[i]);
					trimmed.values.extend_from_slice(&row[..width]);
					budget -= 1;
				}
			}
			if !trimmed.is_empty() {
				out.push(trimmed);
			}
		}
		Ok(out)
	}

	/// Statistics of `stream` in `range`, in buckets of `bucket` width. A
	/// batch that falls wholly inside one bucket is summarised from its stored
	/// statistics without decoding its samples.
	pub fn series_summary(
		&self,
		stream: &StreamId,
		range: TimeRange,
		bucket: SignedDuration,
	) -> Result<Vec<SeriesBucket>> {
		let width = micros_of(bucket).max(1_000_000);
		let origin = micros(range.start);
		let index = |t_us: i64| (t_us - origin).div_euclid(width);
		let mut buckets: BTreeMap<i64, BTreeMap<String, ChannelStats>> = BTreeMap::new();

		let headers = self.stored_batches(stream, range, false)?;
		let needs_decoding = |b: &StoredBatch| {
			b.start < origin || b.end >= micros(range.end) || index(b.start) != index(b.end)
		};
		let fast: Vec<_> = headers.iter().filter(|b| !needs_decoding(b)).collect();
		for batch in fast {
			let slot = buckets.entry(index(batch.start)).or_default();
			for (name, stat) in batch.channels.iter().zip(&batch.stats) {
				slot.entry(name.clone()).or_default().merge(stat);
			}
		}
		if headers.iter().any(needs_decoding) {
			for stored in self
				.stored_batches(stream, range, true)?
				.into_iter()
				.filter(needs_decoding)
			{
				let batch = stored.decode()?;
				for (t, row) in batch.rows() {
					if !range.contains(t) {
						continue;
					}
					let slot = buckets.entry(index(micros(t))).or_default();
					for (name, value) in batch.channels.iter().zip(row) {
						slot.entry(name.clone()).or_default().push(*value);
					}
				}
			}
		}
		buckets
			.into_iter()
			.map(|(i, channels)| {
				Ok(SeriesBucket {
					start: from_micros(origin + i * width)?,
					channels,
				})
			})
			.collect()
	}
}

fn micros_of(duration: SignedDuration) -> i64 {
	i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::store;

	fn at(seconds: i64) -> Timestamp {
		Timestamp::UNIX_EPOCH
			+ SignedDuration::from_hours(500_000)
			+ SignedDuration::from_secs(seconds)
	}

	fn heart_rate(start: i64, bpm: &[f64]) -> SeriesBatch {
		SeriesBatch {
			stream: "health.heart_rate".parse().unwrap(),
			device: DeviceId(Uuid::nil()),
			start: at(start),
			channels: vec!["bpm".into()],
			offsets_ms: (0..bpm.len())
				.map(|i| u32::try_from(i).unwrap() * 1000)
				.collect(),
			values: bpm.to_vec(),
		}
	}

	#[test]
	fn stores_once_and_reads_back_within_range() {
		let store = store();
		let batch = heart_rate(0, &[60.0, 61.0, 62.0, 63.0, 64.0]);
		assert!(store.insert_series(&batch).unwrap());
		assert!(!store.insert_series(&batch).unwrap());

		let range = TimeRange::new(at(1), at(4)).unwrap();
		let back = store.series(&batch.stream, range, 100).unwrap();
		assert_eq!(back.len(), 1);
		assert_eq!(back[0].values, vec![61.0, 62.0, 63.0]);
		assert_eq!(back[0].offsets_ms, vec![1000, 2000, 3000]);

		let capped = store.series(&batch.stream, range, 2).unwrap();
		assert_eq!(capped[0].values.len(), 2);
	}

	#[test]
	fn location_keeps_full_precision() {
		let store = store();
		let fix = SeriesBatch {
			stream: "location.fix".parse().unwrap(),
			device: DeviceId(Uuid::nil()),
			start: at(0),
			channels: vec!["lat".into(), "lon".into()],
			offsets_ms: vec![0],
			values: vec![51.507_351_234, -0.127_758_987],
		};
		store.insert_series(&fix).unwrap();
		let range = TimeRange::new(at(0), at(10)).unwrap();
		let back = store.series(&fix.stream, range, 10).unwrap();
		assert_eq!(back[0].values, fix.values);
	}

	#[test]
	fn summaries_bucket_and_match_raw_statistics() {
		let store = store();
		// Two batches inside the first minute (fast path), one straddling the
		// minute boundary (decoded path).
		store.insert_series(&heart_rate(0, &[60.0, 70.0])).unwrap();
		store.insert_series(&heart_rate(10, &[80.0])).unwrap();
		store
			.insert_series(&heart_rate(59, &[90.0, 100.0]))
			.unwrap();
		let range = TimeRange::new(at(0), at(120)).unwrap();
		let buckets = store
			.series_summary(
				&"health.heart_rate".parse().unwrap(),
				range,
				SignedDuration::from_mins(1),
			)
			.unwrap();
		assert_eq!(buckets.len(), 2);
		let first = &buckets[0].channels["bpm"];
		assert_eq!((first.count, first.min, first.max), (4, 60.0, 90.0));
		assert_eq!(first.mean(), Some(75.0));
		let second = &buckets[1].channels["bpm"];
		assert_eq!((second.count, second.min), (1, 100.0));
		assert_eq!(buckets[1].start, at(60));
	}

	#[test]
	fn rejects_ragged_batches() {
		let store = store();
		let mut bad = heart_rate(0, &[60.0]);
		bad.values.push(1.0);
		assert!(matches!(
			store.insert_series(&bad),
			Err(StoreError::Invalid(_))
		));
	}
}
