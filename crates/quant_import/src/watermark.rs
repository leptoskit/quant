//! Per-source watermarks, kept beside the client's other state — never in
//! the vault, which the importer never opens directly.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use serde::Deserialize;
use serde::Serialize;

use crate::ImportError;

/// How far one source has been imported, and what span is still open.
///
/// Marks are exclusive: the next pass reads events after the mark. An open
/// span (a Screenpipe focus span still being extended by `--follow`) is
/// carried with it, so a restart resumes the span rather than losing it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceState {
	/// What was imported (source-dependent keys, such as `frames`).
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub marks: BTreeMap<String, Timestamp>,
	/// Whether `open` and `last_text` were written by a `--follow` run.
	#[serde(default)]
	pub follow: bool,
	/// The span still being extended, when one is open.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub open: Option<serde_json::Value>,
	/// The last OCR text seen, with its app and window, so deduplication
	/// survives a restart.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub last_text: Option<(String, String, String)>,
}

impl SourceState {
	/// The state stored for `file`, or a fresh one.
	pub(crate) fn load(file: &Path) -> Self {
		std::fs::read(file)
			.ok()
			.and_then(|bytes| serde_json::from_slice(&bytes).ok())
			.unwrap_or_default()
	}

	/// Write the state atomically, readable by the owner only.
	pub(crate) fn save(&self, file: &Path) -> Result<(), ImportError> {
		if let Some(parent) = file.parent() {
			std::fs::create_dir_all(parent)?;
		}
		let temp = file.with_extension("tmp");
		std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
		}
		std::fs::rename(temp, file)?;
		Ok(())
	}

	/// The mark for `key`, when one has been set.
	pub(crate) fn mark(&self, key: &str) -> Option<Timestamp> {
		self.marks.get(key).copied()
	}

	/// Move the mark for `key` forward, never back.
	pub(crate) fn advance(&mut self, key: &str, to: Timestamp) {
		let mark = self.marks.entry(key.to_owned()).or_insert(to);
		if to > *mark {
			*mark = to;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn marks_advance_and_survive_a_round_trip() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("screenpipe-test.json");
		let mut state = SourceState::default();
		assert_eq!(state.mark("frames"), None);
		state.advance("frames", Timestamp::from_second(100).unwrap());
		state.advance("frames", Timestamp::from_second(50).unwrap());
		state.advance("frames", Timestamp::from_second(200).unwrap());
		state.save(&file).unwrap();
		assert_eq!(
			SourceState::load(&file).mark("frames").unwrap().as_second(),
			200
		);
	}

	#[test]
	fn a_missing_file_is_a_fresh_state() {
		assert_eq!(
			SourceState::load(Path::new("/nonexistent/x.json")),
			SourceState::default()
		);
	}
}
