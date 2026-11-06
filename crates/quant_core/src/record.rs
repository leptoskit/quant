use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;

use crate::DeviceId;
use crate::RecordId;
use crate::StreamId;

/// A discrete thing that happened.
///
/// Records carry their id from the device that captured them, so sending the
/// same record twice (a retried sync, say) stores it once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
	/// Unique, time-ordered id assigned at capture.
	pub id: RecordId,
	/// The stream the record belongs to.
	pub stream: StreamId,
	/// The device that captured it.
	pub device: DeviceId,
	/// When it began.
	pub start: Timestamp,
	/// When it ended, for spans.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub end: Option<Timestamp>,
	/// Searchable text: OCR output, a transcript, a note.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub text: Option<String>,
	/// Structured attributes, such as `app`, `window`, `url`, `stage`.
	#[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
	pub data: serde_json::Map<String, serde_json::Value>,
	/// Media attached to the record: a screenshot or an audio clip.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub media: Option<MediaRef>,
}

impl Record {
	/// A record with a fresh id that started now.
	pub fn new(stream: StreamId, device: DeviceId) -> Self {
		Self {
			id: RecordId::new(),
			stream,
			device,
			start: Timestamp::now(),
			end: None,
			text: None,
			data: serde_json::Map::new(),
			media: None,
		}
	}

	/// Set when the record began.
	#[must_use]
	pub fn starting(mut self, start: Timestamp) -> Self {
		self.start = start;
		self
	}

	/// Set when the record ended.
	#[must_use]
	pub fn ending(mut self, end: Timestamp) -> Self {
		self.end = Some(end);
		self
	}

	/// Set the searchable text.
	#[must_use]
	pub fn with_text(mut self, text: impl Into<String>) -> Self {
		self.text = Some(text.into());
		self
	}

	/// Add a structured attribute.
	#[must_use]
	pub fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
		self.data.insert(key.to_owned(), value.into());
		self
	}

	/// Attach media.
	#[must_use]
	pub fn with_media(mut self, media: MediaRef) -> Self {
		self.media = Some(media);
		self
	}

	/// The record's end, or its start for instants.
	pub fn end_or_start(&self) -> Timestamp {
		self.end.unwrap_or(self.start)
	}
}

/// Points at media stored beside a record. The bytes live in the vault's
/// encrypted blob store, addressed by their BLAKE3 hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
	/// Hex BLAKE3 hash of the plaintext bytes.
	pub hash: String,
	/// Media type, such as `image/jpeg` or `audio/wav`.
	pub mime: String,
	/// Size of the plaintext in bytes.
	pub size: u64,
}

impl MediaRef {
	/// Describe `bytes` as media of type `mime`.
	pub fn of(bytes: &[u8], mime: &str) -> Self {
		Self {
			hash: blake3::hash(bytes).to_hex().to_string(),
			mime: mime.to_owned(),
			size: bytes.len() as u64,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn records_round_trip_through_json() {
		let record = Record::new("apps.focus".parse().unwrap(), DeviceId::new())
			.with_text("Quant – design.md")
			.with("app", "Zed")
			.with_media(MediaRef::of(b"jpeg", "image/jpeg"));
		let json = serde_json::to_string(&record).unwrap();
		let back: Record = serde_json::from_str(&json).unwrap();
		assert_eq!(back, record);
	}

	#[test]
	fn empty_fields_are_omitted() {
		let record = Record::new("memory.note".parse().unwrap(), DeviceId::new());
		let json = serde_json::to_value(&record).unwrap();
		let object = json.as_object().unwrap();
		for absent in ["end", "text", "data", "media"] {
			assert!(!object.contains_key(absent), "{absent} should be omitted");
		}
	}
}
