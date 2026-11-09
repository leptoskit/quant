//! One low-priority recognition job at a time. Work lives in the encrypted
//! database, so restarting the daemon never loses an uploaded phone segment.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use quant_store::StoreError;

use crate::State;

/// Start continuous transcription of uploaded `audio.ambient` segments.
/// Recognition is local, sequential, and separate from API request handling.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		loop {
			let working = Arc::clone(&state);
			let result = tokio::task::spawn_blocking(move || {
				quant_capture_macos::lower_priority();
				tick(&working)
			})
			.await;
			match result {
				Ok(Ok(true)) => continue,
				Ok(Ok(false)) => {}
				Ok(Err(error)) => tracing::warn!(%error, "ambient transcription queue failed"),
				Err(error) => tracing::warn!(%error, "ambient transcription worker failed"),
			}
			tokio::time::sleep(Duration::from_secs(10)).await;
		}
	})
}

fn tick(state: &State) -> Result<bool, StoreError> {
	let now = Timestamp::now();
	let Some(record) = state.vault.store.pending_ambient_audio(now)? else {
		return Ok(false);
	};
	let Some(media) = record.media else {
		return Ok(false);
	};
	let bytes = match state.vault.get_media(&media.hash) {
		Ok(bytes) => bytes,
		Err(error) => {
			state.vault.store.retry_ambient_audio(
				record.id,
				&error.to_string(),
				now,
				matches!(error, StoreError::NotFound),
			)?;
			return Ok(true);
		}
	};
	let config = crate::capture::config(state)?;
	let settings = quant_capture_macos::Transcribe {
		whisper_model: config.audio.whisper_model,
		language: config.audio.language,
	};
	match quant_capture_macos::transcribe(&bytes, &settings) {
		Ok(text) => {
			state
				.vault
				.store
				.finish_ambient_audio(record.id, &text, Timestamp::now())?;
		}
		Err(reason) => {
			// The reason stays in the encrypted queue; logs do not need details
			// returned by a recognizer handling private audio.
			state
				.vault
				.store
				.retry_ambient_audio(record.id, &reason, Timestamp::now(), false)?;
		}
	}
	Ok(true)
}
