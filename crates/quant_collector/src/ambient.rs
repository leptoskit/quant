//! Native recorders hand finalized AAC files to this adapter. They remove the
//! plaintext file only after this method has durably sealed it in the spool.

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::Sink;
use quant_core::SinkError;

use crate::Spool;

/// The recorder's measurements for a finalized mono AAC `.m4a` segment.
#[derive(Clone, Copy, Debug)]
pub struct AmbientAudio {
	/// When recording began, from the native recorder rather than upload time.
	pub start: Timestamp,
	/// Segment duration; normally 300,000 ms, shorter when interrupted/stopped.
	pub duration_ms: u32,
	/// Milliseconds whose level exceeded the recording activity threshold.
	/// This measures sound, not whether the sound is speech.
	pub active_ms: u32,
	/// Loudest observed peak, in dBFS, between -120 and zero.
	pub peak_dbfs: f64,
}

impl Spool {
	/// Seal and enqueue non-silent phone audio. Silence returns `None` without
	/// writing media or records. A recovered file produces the same record id,
	/// making replay safe across a crash or an interrupted upload.
	pub fn ambient_audio(
		&self,
		device: DeviceId,
		segment: AmbientAudio,
		bytes: &[u8],
	) -> Result<Option<RecordId>, SinkError> {
		if segment.duration_ms == 0
			|| segment.duration_ms > 3_600_000
			|| segment.active_ms > segment.duration_ms
			|| !(-120.0..=0.0).contains(&segment.peak_dbfs)
		{
			return Err(SinkError::from_display(
				"invalid ambient audio measurements",
			));
		}
		if segment.active_ms == 0 {
			return Ok(None);
		}
		if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
			return Err(SinkError::from_display(
				"ambient audio is empty or exceeds 16 MiB",
			));
		}
		let end = segment
			.start
			.checked_add(SignedDuration::from_millis(i64::from(segment.duration_ms)))
			.map_err(SinkError::from_display)?;
		let stream = "audio.ambient".parse().expect("built-in stream");
		let media = self.media(&stream, bytes, "audio/mp4")?;
		let mut hash = blake3::Hasher::new_derive_key("quant ambient audio record v1");
		hash.update(device.0.as_bytes());
		hash.update(&segment.start.as_microsecond().to_le_bytes());
		hash.update(&segment.duration_ms.to_le_bytes());
		hash.update(media.hash.as_bytes());
		let id: RecordId = hash.finalize().to_hex()[..32]
			.parse()
			.expect("hash is a UUID-sized hex string");
		let mut record = Record::new(stream, device)
			.starting(segment.start)
			.ending(end)
			.with("source", "phone")
			.with("duration_ms", segment.duration_ms)
			.with("active_ms", segment.active_ms)
			.with("peak_dbfs", segment.peak_dbfs)
			.with_media(media);
		record.id = id;
		self.record(record)?;
		Ok(Some(id))
	}
}

#[cfg(test)]
mod tests {
	use quant_crypto::VaultKey;

	use super::*;

	#[test]
	fn silence_stays_off_disk_and_recovered_files_are_idempotent() {
		let dir = tempfile::tempdir().unwrap();
		let key = VaultKey::generate();
		let spool = Spool::open(dir.path(), &key).unwrap();
		let device = DeviceId::new();
		let mut segment = AmbientAudio {
			start: Timestamp::now(),
			duration_ms: 300_000,
			active_ms: 0,
			peak_dbfs: -120.0,
		};
		assert!(
			spool
				.ambient_audio(device, segment, b"silence")
				.unwrap()
				.is_none()
		);
		assert_eq!(spool.pending().unwrap().records, 0);
		assert_eq!(
			std::fs::read_dir(dir.path().join("media")).unwrap().count(),
			0
		);
		segment.active_ms = 30;
		segment.peak_dbfs = -12.0;
		let id = spool
			.ambient_audio(device, segment, b"short sound")
			.unwrap()
			.unwrap();
		drop(spool);
		let spool = Spool::open(dir.path(), &key).unwrap();
		assert_eq!(
			spool
				.ambient_audio(device, segment, b"short sound")
				.unwrap(),
			Some(id)
		);
		assert_eq!(spool.pending().unwrap().records, 1);
		let record = spool.store.record(id).unwrap().unwrap();
		assert_eq!(record.data["active_ms"], 30);
		assert_eq!(record.stream.as_str(), "audio.ambient");
		assert_eq!(
			spool.media.get(&record.media.unwrap().hash).unwrap(),
			b"short sound"
		);
		assert_ne!(
			spool
				.ambient_audio(DeviceId::new(), segment, b"short sound")
				.unwrap(),
			Some(id)
		);
	}

	#[test]
	fn corrupt_measurements_are_not_saved() {
		let dir = tempfile::tempdir().unwrap();
		let spool = Spool::open(dir.path(), &VaultKey::generate()).unwrap();
		let mut segment = AmbientAudio {
			start: Timestamp::now(),
			duration_ms: 300_000,
			active_ms: 100,
			peak_dbfs: f64::NAN,
		};
		assert!(
			spool
				.ambient_audio(DeviceId::new(), segment, b"audio")
				.is_err()
		);
		segment.peak_dbfs = -12.0;
		segment.active_ms = 300_001;
		assert!(
			spool
				.ambient_audio(DeviceId::new(), segment, b"audio")
				.is_err()
		);
		assert_eq!(spool.pending().unwrap().records, 0);
	}
}
