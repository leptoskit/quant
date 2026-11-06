use crate::MediaRef;
use crate::Record;
use crate::SeriesBatch;
use crate::StreamId;

/// Where capture sources send what they capture.
///
/// The daemon implements it by writing straight into the vault; a phone
/// implements it by queueing for upload. Capture code depends only on this.
pub trait Sink: Send + Sync {
	/// Store a record.
	fn record(&self, record: Record) -> Result<(), SinkError>;

	/// Store a batch of samples.
	fn series(&self, batch: SeriesBatch) -> Result<(), SinkError>;

	/// Store media for a record on `stream`, returning its reference.
	fn media(&self, stream: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError>;

	/// Report how a capture source is doing.
	fn status(&self, source: &str, running: bool, detail: Option<String>);
}

/// A sink refused or failed to store something.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

impl SinkError {
	/// An error from anything displayable.
	pub fn from_display(error: impl std::fmt::Display) -> Self {
		Self(error.to_string())
	}
}
