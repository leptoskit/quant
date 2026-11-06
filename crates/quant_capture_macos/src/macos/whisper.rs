//! Speech-to-text with whisper.cpp, behind the `whisper` feature.

use std::thread;

use whisper_rs::FullParams;
use whisper_rs::SamplingStrategy;
use whisper_rs::WhisperContext;
use whisper_rs::WhisperContextParameters;
use whisper_rs::WhisperState;

use crate::config::AudioConfig;

/// A loaded whisper model, ready to transcribe 16 kHz mono speech.
pub(crate) struct Transcriber {
	state: WhisperState,
	threads: i32,
}

impl Transcriber {
	/// Load the configured model. Returns `None` and a note for the status
	/// line when there is no model or it fails to load.
	pub(crate) fn load(config: &AudioConfig) -> (Option<Self>, Option<String>) {
		let Some(path) = &config.whisper_model else {
			return (
				None,
				Some("transcription off: no whisper_model configured".to_owned()),
			);
		};
		let loaded = WhisperContext::new_with_params(path, WhisperContextParameters::default())
			.and_then(|context| context.create_state());
		match loaded {
			Ok(state) => {
				// Leave most cores to the user; segments are short.
				let threads =
					thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
				(
					Some(Self {
						state,
						threads: i32::try_from(threads).unwrap_or(2),
					}),
					None,
				)
			}
			Err(error) => {
				(
					None,
					Some(format!(
						"transcription off: could not load {}: {error}",
						path.display()
					)),
				)
			}
		}
	}

	/// Transcribe one segment of 16 kHz mono samples.
	pub(crate) fn transcribe(
		&mut self,
		samples: &[i16],
		config: &AudioConfig,
	) -> Result<String, String> {
		let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
		params.set_n_threads(self.threads);
		params.set_language(config.language.as_deref());
		params.set_no_context(true);
		params.set_suppress_blank(true);
		params.set_print_special(false);
		params.set_print_progress(false);
		params.set_print_realtime(false);
		params.set_print_timestamps(false);
		let audio: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_768.0).collect();
		self.state
			.full(params, &audio)
			.map_err(|error| error.to_string())?;
		let mut text = String::new();
		for segment in self.state.as_iter() {
			let piece = segment.to_str_lossy().map_err(|error| error.to_string())?;
			text.push_str(&piece);
		}
		Ok(text.trim().to_owned())
	}
}
