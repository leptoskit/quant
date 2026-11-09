//! Durable transcription work, committed alongside incoming phone records.

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::Record;
use quant_core::RecordId;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::Result;
use crate::Store;
use crate::micros;
use crate::records::COLUMNS;
use crate::records::record_from_row;

impl Store {
	/// The next eligible phone audio segment, including late offline imports.
	/// Failures wait with backoff rather than blocking newer segments.
	pub fn pending_ambient_audio(&self, now: Timestamp) -> Result<Option<Record>> {
		let conn = self.conn();
		conn.query_row(
			&format!(
				"SELECT {COLUMNS} FROM transcriptions t JOIN records r ON r.rowid = t.record
			 WHERE t.next_us <= ?1 ORDER BY t.next_us, t.record LIMIT 1"
			),
			[micros(now)],
			record_from_row,
		)
		.optional()?
		.transpose()
	}

	/// Finish a segment atomically: store a searchable transcript without raw
	/// media, mark the source, and remove its job. If the owner purged the
	/// segment during recognition, nothing is recreated. Repeats are harmless.
	pub fn finish_ambient_audio(&self, id: RecordId, text: &str, now: Timestamp) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let text = text.trim();
		let new_id = RecordId::new();
		if !text.is_empty() {
			tx.execute(
				"INSERT INTO records
				 (id, stream, family, device, start_us, end_us, observed_us, text, data)
				 SELECT ?1, 'audio.transcript', 'audio', r.device, r.start_us, r.end_us, ?2, ?3,
				 json_object('chunk_id', ?4, 'source_stream', 'audio.ambient', 'source',
				 coalesce(json_extract(r.data, '$.source'), 'phone'))
				 FROM records r JOIN transcriptions t ON t.record = r.rowid WHERE r.id = ?5",
				params![
					new_id.0.as_bytes().as_slice(),
					micros(now),
					text,
					id.to_string(),
					id.0.as_bytes().as_slice()
				],
			)?;
		}
		let status = if text.is_empty() {
			"no speech found"
		} else {
			"done"
		};
		tx.execute(
			"UPDATE records SET data = json_set(coalesce(data, '{}'), '$.transcript_status', ?1)
			 WHERE id = ?2 AND rowid IN (SELECT record FROM transcriptions)",
			params![status, id.0.as_bytes().as_slice()],
		)?;
		tx.execute(
			"DELETE FROM transcriptions WHERE record = (SELECT rowid FROM records WHERE id = ?1)",
			[id.0.as_bytes().as_slice()],
		)?;
		tx.commit()?;
		Ok(())
	}

	/// Persist a failure and retry it with exponential backoff, capped at an
	/// hour. A restart preserves the delay. Missing/expired media is terminal.
	pub fn retry_ambient_audio(
		&self,
		id: RecordId,
		reason: &str,
		now: Timestamp,
		expired: bool,
	) -> Result<()> {
		let mut conn = self.conn();
		let tx = conn.transaction()?;
		let attempts: Option<u32> = tx
			.query_row(
				"SELECT attempts FROM transcriptions t JOIN records r ON r.rowid = t.record WHERE \
				 r.id = ?1",
				[id.0.as_bytes().as_slice()],
				|row| row.get(0),
			)
			.optional()?;
		let Some(attempts) = attempts else {
			return Ok(());
		};
		let seconds = (60_i64 * 2_i64.pow(attempts.min(6))).min(3600);
		let reason: String = reason.chars().take(500).collect();
		tx.execute(
			"UPDATE records SET data = json_set(coalesce(data, '{}'), '$.transcript_status', ?1)
			 WHERE id = ?2",
			params![
				if expired { "media expired" } else { "retry" },
				id.0.as_bytes().as_slice()
			],
		)?;
		if expired {
			tx.execute(
				"DELETE FROM transcriptions WHERE record = (SELECT rowid FROM records WHERE id = \
				 ?1)",
				[id.0.as_bytes().as_slice()],
			)?;
		} else {
			tx.execute(
				"UPDATE transcriptions SET attempts = attempts + 1, next_us = ?1, last_error = ?2
				 WHERE record = (SELECT rowid FROM records WHERE id = ?3)",
				params![
					micros(now + SignedDuration::from_secs(seconds)),
					reason,
					id.0.as_bytes().as_slice()
				],
			)?;
		}
		tx.commit()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use quant_core::DeviceId;
	use quant_core::MediaRef;
	use quant_core::TimeRange;
	use quant_crypto::VaultKey;

	use super::*;
	use crate::RecordQuery;
	use crate::Selector;

	fn chunk(at: Timestamp) -> Record {
		Record::new("audio.ambient".parse().unwrap(), DeviceId::new())
			.starting(at)
			.ending(at + SignedDuration::from_secs(300))
			.with_media(MediaRef::of(b"AAC", "audio/mp4"))
	}

	fn transcripts(store: &Store) -> Vec<Record> {
		store
			.records(&RecordQuery::new(
				Selector::stream("audio.transcript".parse().unwrap()),
				TimeRange::new(Timestamp::MIN, Timestamp::MAX).unwrap(),
			))
			.unwrap()
	}

	#[test]
	fn restart_late_import_and_duplicate_delivery_do_not_lose_work() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("vault.db");
		let key = VaultKey::generate();
		let now = Timestamp::now();
		let first = chunk(now);
		let store = Store::open(&path, &key).unwrap();
		store.insert_records(std::slice::from_ref(&first)).unwrap();
		drop(store);
		let store = Store::open(&path, &key).unwrap();
		assert_eq!(
			store.pending_ambient_audio(now).unwrap().unwrap().id,
			first.id
		);
		store
			.finish_ambient_audio(first.id, "remember the conversation", now)
			.unwrap();
		store
			.finish_ambient_audio(first.id, "duplicate", now)
			.unwrap();
		store.insert_records(std::slice::from_ref(&first)).unwrap();
		assert!(store.pending_ambient_audio(now).unwrap().is_none());
		let found = transcripts(&store);
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].text.as_deref(), Some("remember the conversation"));
		assert_eq!(found[0].device, first.device);
		assert_eq!(found[0].start, first.start);
		assert_eq!(found[0].end, first.end);
		assert_eq!(found[0].data["chunk_id"], first.id.to_string());
		assert!(found[0].media.is_none());
		let hits = store
			.records(
				&RecordQuery::new(
					Selector::all(),
					TimeRange::new(Timestamp::MIN, Timestamp::MAX).unwrap(),
				)
				.search("conversation"),
			)
			.unwrap();
		assert_eq!(hits.len(), 1);
		let late = chunk(now - SignedDuration::from_hours(48));
		store.insert_records(std::slice::from_ref(&late)).unwrap();
		assert_eq!(
			store.pending_ambient_audio(now).unwrap().unwrap().id,
			late.id
		);
	}

	#[test]
	fn failures_back_off_without_blocking_other_audio() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("vault.db");
		let key = VaultKey::generate();
		let now = Timestamp::now();
		let a = chunk(now);
		let b = chunk(now);
		let store = Store::open(&path, &key).unwrap();
		store.insert_records(&[a.clone(), b.clone()]).unwrap();
		store
			.retry_ambient_audio(a.id, "model unavailable", now, false)
			.unwrap();
		assert_eq!(store.pending_ambient_audio(now).unwrap().unwrap().id, b.id);
		store.finish_ambient_audio(b.id, "", now).unwrap();
		drop(store);
		let store = Store::open(&path, &key).unwrap();
		assert!(store.pending_ambient_audio(now).unwrap().is_none());
		assert_eq!(
			store
				.pending_ambient_audio(now + SignedDuration::from_secs(61))
				.unwrap()
				.unwrap()
				.id,
			a.id
		);
		store
			.retry_ambient_audio(a.id, "media is gone", now, true)
			.unwrap();
		assert!(
			store
				.pending_ambient_audio(now + SignedDuration::from_hours(2))
				.unwrap()
				.is_none()
		);
		assert!(transcripts(&store).is_empty());
		assert_eq!(
			store.record(a.id).unwrap().unwrap().data["transcript_status"],
			"media expired"
		);
	}

	#[test]
	fn purging_during_recognition_cannot_resurrect_data() {
		let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
		let now = Timestamp::now();
		let record = chunk(now);
		store.insert_records(std::slice::from_ref(&record)).unwrap();
		let rows = store
			.oldest_records(10)
			.unwrap()
			.into_iter()
			.map(|(row, _)| row)
			.collect::<Vec<_>>();
		store.delete_record_rows(&rows).unwrap();
		store
			.finish_ambient_audio(record.id, "must not reappear", now)
			.unwrap();
		assert!(transcripts(&store).is_empty());
		assert!(store.pending_ambient_audio(now).unwrap().is_none());
	}

	#[test]
	fn a_failed_write_keeps_the_job_and_source_intact() {
		let store = Store::open_in_memory(&VaultKey::generate()).unwrap();
		let now = Timestamp::now();
		let record = chunk(now);
		store.insert_records(std::slice::from_ref(&record)).unwrap();
		store
			.conn()
			.execute_batch(
				"CREATE TRIGGER fail_transcript BEFORE INSERT ON records
			WHEN new.stream = 'audio.transcript' BEGIN SELECT RAISE(ABORT, 'disk failure'); END;",
			)
			.unwrap();
		assert!(
			store
				.finish_ambient_audio(record.id, "cannot commit", now)
				.is_err()
		);
		assert_eq!(
			store.pending_ambient_audio(now).unwrap().unwrap().id,
			record.id
		);
		assert!(transcripts(&store).is_empty());
		assert!(
			store
				.record(record.id)
				.unwrap()
				.unwrap()
				.data
				.get("transcript_status")
				.is_none()
		);
		store
			.conn()
			.execute_batch("DROP TRIGGER fail_transcript")
			.unwrap();
		store
			.finish_ambient_audio(record.id, "can commit", now)
			.unwrap();
		assert_eq!(transcripts(&store).len(), 1);
	}
}
