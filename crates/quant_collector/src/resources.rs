//! Admission limits keep one upload allocation independent of backlog size.

use std::io;
use std::io::Write;

use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::SinkError;

/// Largest phone media item. Default five-minute 24 kbps audio is about 0.9 MB.
pub const MAX_MEDIA_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_RECORD_BYTES: usize = 64 * 1024;
pub(crate) const MAX_SERIES_BYTES: usize = 256 * 1024;

struct Counter(usize);

impl Write for Counter {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		self.0 = self.0.saturating_add(bytes.len());
		if self.0 > MAX_RECORD_BYTES {
			return Err(io::Error::other("phone record exceeds 64 KiB"));
		}
		Ok(bytes.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

pub(crate) fn check_record(record: &Record) -> Result<(), SinkError> {
	// Count encoded bytes without constructing another large JSON buffer.
	serde_json::to_writer(Counter(0), record).map_err(SinkError::from_display)
}

pub(crate) fn check_series(batch: &SeriesBatch) -> Result<(), SinkError> {
	if batch.values.len() > 16_384
		|| batch.offsets_ms.len() > 16_384
		|| batch.channels.len() > 32
		|| batch.channels.iter().any(|name| name.len() > 64)
	{
		return Err(SinkError::from_display(
			"phone sensor batch is too large; flush smaller batches",
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use quant_core::DeviceId;
	use quant_core::Sink;
	use quant_crypto::VaultKey;

	use super::*;
	use crate::Spool;

	#[test]
	fn oversized_capture_payloads_are_rejected_before_storage() {
		let dir = tempfile::tempdir().unwrap();
		let spool = Spool::open(dir.path(), &VaultKey::generate()).unwrap();
		let record = Record::new("apps.focus".parse().unwrap(), DeviceId::new())
			.with_text("a".repeat(MAX_RECORD_BYTES));
		assert!(spool.record(record).is_err());
		let batch = SeriesBatch {
			stream: "motion.orientation".parse().unwrap(),
			device: DeviceId::new(),
			start: jiff::Timestamp::now(),
			channels: vec!["pitch".into()],
			offsets_ms: vec![0; 16_385],
			values: vec![0.0; 16_385],
		};
		assert!(spool.series(batch).is_err());
		assert_eq!(spool.pending().unwrap(), crate::Pending::default());
	}
}
