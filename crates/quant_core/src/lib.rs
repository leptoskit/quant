//! The shared vocabulary of Quant.
//!
//! Everything Quant knows about a life arrives as either a [`Record`] (a
//! discrete thing that happened: a screen frame, a transcript segment, a
//! focus change) or a [`SeriesBatch`] (a run of numeric samples from a
//! sensor: heart rate, acceleration, orientation). Both name a [`StreamId`],
//! and every stream belongs to a [`Family`] that access grants are scoped to.

mod id;
mod record;
mod series;
mod sink;
mod stream;
mod time;

pub use id::*;
pub use record::*;
pub use series::*;
pub use sink::*;
pub use stream::*;
pub use time::*;

/// Errors from validating Quant values.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoreError {
	/// A stream name did not match `family.name[.name…]`.
	#[error("invalid stream id `{0}`: use lowercase `family.name` segments of a-z, 0-9 and _")]
	InvalidStream(String),
	/// A stream family is not one Quant knows.
	#[error("unknown stream family `{0}`")]
	UnknownFamily(String),
	/// A series batch's values do not divide evenly into its channels.
	#[error("series batch has {values} values for {channels} channels")]
	RaggedSeries {
		/// Number of values supplied.
		values: usize,
		/// Number of channels declared.
		channels: usize,
	},
	/// A series batch's timestamps do not match its row count.
	#[error("series batch has {rows} rows but {offsets} offsets")]
	OffsetMismatch {
		/// Rows implied by values / channels.
		rows: usize,
		/// Offsets supplied.
		offsets: usize,
	},
	/// A time range ends before it starts.
	#[error("time range ends before it starts")]
	InvertedRange,
}
