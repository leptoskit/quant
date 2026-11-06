use jiff::SignedDuration;
use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;

use crate::CoreError;
use crate::DeviceId;
use crate::StreamId;

/// A run of numeric samples from one stream on one device.
///
/// Values are row-major: row `i` is `values[i *
/// channels.len()..][..channels.len()]` and was sampled `offsets_ms[i]`
/// milliseconds after `start`. Sensors that sample hundreds of times a second
/// send one batch every few seconds rather than one record per sample.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeriesBatch {
	/// The stream, such as `health.heart_rate`.
	pub stream: StreamId,
	/// The device that sampled it.
	pub device: DeviceId,
	/// Time of the first row's offset origin.
	pub start: Timestamp,
	/// Channel names, in column order.
	pub channels: Vec<String>,
	/// Milliseconds after `start` of each row.
	pub offsets_ms: Vec<u32>,
	/// Row-major sample values.
	pub values: Vec<f64>,
}

impl SeriesBatch {
	/// Check that values and offsets agree with the channels.
	pub fn validate(&self) -> Result<(), CoreError> {
		let channels = self.channels.len();
		if channels == 0 || !self.values.len().is_multiple_of(channels) {
			return Err(CoreError::RaggedSeries {
				values: self.values.len(),
				channels,
			});
		}
		let rows = self.values.len() / channels;
		if rows != self.offsets_ms.len() {
			return Err(CoreError::OffsetMismatch {
				rows,
				offsets: self.offsets_ms.len(),
			});
		}
		Ok(())
	}

	/// Number of rows.
	pub fn len(&self) -> usize {
		self.offsets_ms.len()
	}

	/// Whether the batch holds no rows.
	pub fn is_empty(&self) -> bool {
		self.offsets_ms.is_empty()
	}

	/// Each row's timestamp and values.
	pub fn rows(&self) -> impl Iterator<Item = (Timestamp, &[f64])> {
		let width = self.channels.len().max(1);
		self.offsets_ms
			.iter()
			.zip(self.values.chunks(width))
			.map(move |(offset, row)| {
				(
					self.start + SignedDuration::from_millis(i64::from(*offset)),
					row,
				)
			})
	}

	/// Timestamp of the last row, or `start` if empty.
	pub fn end(&self) -> Timestamp {
		let last = self.offsets_ms.iter().copied().max().unwrap_or(0);
		self.start + SignedDuration::from_millis(i64::from(last))
	}

	/// Index of a channel by name.
	pub fn channel(&self, name: &str) -> Option<usize> {
		self.channels.iter().position(|c| c == name)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn batch(values: Vec<f64>, offsets: Vec<u32>) -> SeriesBatch {
		SeriesBatch {
			stream: "motion.accelerometer".parse().unwrap(),
			device: DeviceId::new(),
			start: Timestamp::UNIX_EPOCH,
			channels: vec!["x".into(), "y".into(), "z".into()],
			offsets_ms: offsets,
			values,
		}
	}

	#[test]
	fn validates_shape() {
		assert!(batch(vec![0.0; 6], vec![0, 10]).validate().is_ok());
		assert_eq!(
			batch(vec![0.0; 5], vec![0, 10]).validate(),
			Err(CoreError::RaggedSeries {
				values: 5,
				channels: 3
			})
		);
		assert_eq!(
			batch(vec![0.0; 6], vec![0]).validate(),
			Err(CoreError::OffsetMismatch {
				rows: 2,
				offsets: 1
			})
		);
	}

	#[test]
	fn iterates_rows_with_timestamps() {
		let b = batch(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], vec![0, 250]);
		let rows: Vec<_> = b.rows().collect();
		assert_eq!(
			rows[1].0,
			Timestamp::UNIX_EPOCH + SignedDuration::from_millis(250)
		);
		assert_eq!(rows[1].1, &[4.0, 5.0, 6.0]);
		assert_eq!(b.end(), rows[1].0);
	}
}
