use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Settings for every capture source. Missing fields take their defaults, so
/// a config file only needs to name what it changes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
	/// Periodic screenshots with OCR.
	pub screen: ScreenConfig,
	/// Focused app and window spans, plus lock and sleep transitions.
	pub apps: AppsConfig,
	/// Keyboard and pointer activity counts.
	pub input: InputConfig,
	/// Microphone speech segments.
	pub audio: AudioConfig,
	/// Rolling audio buffer ("Rewind").
	pub rewind: RewindConfig,
}

/// How hard Vision tries when reading text off the screen.
///
/// On an M-series Mac a text-dense 1600 px frame costs about 0.2 s of CPU
/// with `fast` and about 0.8 s with `accurate`, so `accurate` with a 10 s
/// interval can use several percent of a core while the screen keeps
/// changing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrLevel {
	/// Quicker and lighter; reads ordinary UI and document text well.
	#[default]
	Fast,
	/// About four times the CPU, with language correction; better on tiny,
	/// stylised or low-contrast text.
	Accurate,
}

/// Settings for the `screen` source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenConfig {
	/// Whether to capture the screen at all.
	pub enabled: bool,
	/// Seconds between capture attempts.
	pub interval_secs: u64,
	/// Pause capture once the user has been idle this many seconds.
	pub idle_pause_secs: u64,
	/// How many of the 256 difference-hash bits must differ from the last
	/// stored frame for a new frame to be kept. Zero keeps every frame.
	pub min_change: u32,
	/// Text recognition effort.
	pub ocr_level: OcrLevel,
	/// Keep the JPEG beside the OCR text. When false only text is stored.
	pub store_images: bool,
	/// JPEG quality from 0.0 to 1.0.
	pub jpeg_quality: f32,
	/// Widest a captured frame may be, in pixels; larger displays are scaled
	/// down before OCR and encoding.
	pub max_width: u32,
	/// Apps whose windows are never captured, by bundle id. A trailing `*`
	/// matches any suffix. Case is ignored.
	pub excluded_bundle_ids: Vec<String>,
	/// Window title fragments that skip a frame when the focused window's
	/// title contains one. Case is ignored.
	pub excluded_title_patterns: Vec<String>,
	/// Ask macOS for Screen Recording permission (showing the system prompt)
	/// when it has not been granted.
	pub request_permission: bool,
}

impl Default for ScreenConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			interval_secs: 10,
			idle_pause_secs: 120,
			min_change: 4,
			ocr_level: OcrLevel::Fast,
			store_images: true,
			jpeg_quality: 0.5,
			max_width: 1600,
			excluded_bundle_ids: default_excluded_bundle_ids(),
			excluded_title_patterns: default_excluded_title_patterns(),
			request_permission: true,
		}
	}
}

/// Password managers and credential stores, never captured by default.
pub fn default_excluded_bundle_ids() -> Vec<String> {
	[
		"com.1password.*",
		"com.agilebits.onepassword*",
		"com.bitwarden.desktop",
		"org.keepassxc.keepassxc",
		"com.keepassium.*",
		"com.hicknhacksoftware.macpass",
		"com.markmcguill.strongbox*",
		"com.apple.keychainaccess",
		"com.apple.passwords",
		"com.dashlane.*",
		"com.lastpass.*",
		"me.proton.pass*",
		"in.sinew.enpass*",
		"com.nordsec.nordpass*",
		"com.roboform.*",
	]
	.into_iter()
	.map(str::to_owned)
	.collect()
}

/// Title fragments of private browser windows, never captured by default.
pub fn default_excluded_title_patterns() -> Vec<String> {
	["Private Browsing", "Incognito", "InPrivate"]
		.into_iter()
		.map(str::to_owned)
		.collect()
}

/// Settings for the `apps` source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppsConfig {
	/// Whether to track focus and device state.
	pub enabled: bool,
	/// Milliseconds between focus checks.
	pub poll_ms: u64,
	/// Seconds without input after which the user counts as idle and the
	/// open focus span is closed.
	pub idle_secs: u64,
}

impl Default for AppsConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			poll_ms: 1000,
			idle_secs: 120,
		}
	}
}

/// Settings for the `input` source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
	/// Whether to count input events.
	pub enabled: bool,
	/// Seconds covered by each emitted row.
	pub interval_secs: u64,
}

impl Default for InputConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			interval_secs: 60,
		}
	}
}

/// Settings for the `audio` source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
	/// Whether to listen to the microphone. Off by default because it needs
	/// the Microphone permission and records speech.
	pub enabled: bool,
	/// Longest a single speech segment may run before it is cut.
	pub max_segment_secs: u32,
	/// Segments with less speech than this are dropped as noise.
	pub min_speech_ms: u32,
	/// Speech must last this long before a segment starts.
	pub onset_ms: u32,
	/// How long speech may pause before the segment ends.
	pub hangover_ms: u32,
	/// Audio kept from before speech began, so first syllables survive.
	pub preroll_ms: u32,
	/// How far above the tracked noise floor a frame must be to count as
	/// speech, in dB.
	pub margin_db: f32,
	/// Frames quieter than this (dBFS) are never speech.
	pub min_level_dbfs: f32,
	/// Keep the WAV clip beside the transcript.
	pub store_audio: bool,
	/// Path to a whisper.cpp `ggml` model. Used only when built with the
	/// `whisper` feature.
	pub whisper_model: Option<PathBuf>,
	/// Spoken language hint for whisper, such as `en`; `None` auto-detects.
	pub language: Option<String>,
}

impl Default for AudioConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			max_segment_secs: 30,
			min_speech_ms: 400,
			onset_ms: 90,
			hangover_ms: 700,
			preroll_ms: 300,
			margin_db: 12.0,
			min_level_dbfs: -55.0,
			store_audio: true,
			whisper_model: None,
			language: None,
		}
	}
}

/// Settings for the `rewind` source: a rolling, always-on buffer of the last
/// `hours` of audio that can be saved as a clip after the fact.
///
/// Unlike the `audio` source this never transcribes while recording; the
/// whole point is to spend well under one percent of a core buffering, and
/// transcribe only the moments worth keeping. It keeps recording while the
/// screen is locked or the user is idle, and stops only when the owner pauses
/// capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RewindConfig {
	/// Whether to keep a rolling audio buffer. On by default; the microphone
	/// and system-audio flags below trim what it listens to.
	pub enabled: bool,
	/// How long buffered audio is kept before the media sweep deletes it.
	pub hours: u32,
	/// Buffer the default microphone (needs the Microphone permission).
	pub mic: bool,
	/// Buffer what the machine is playing, through `SCStream` (needs the
	/// Screen Recording permission).
	pub system_audio: bool,
	/// AAC bits per second. 24 kbps mono is telephone-clear speech for about
	/// 260 MB a day.
	pub bitrate: u32,
	/// Sample rate of the encoded audio, in Hz.
	pub sample_rate: u32,
	/// Seconds of audio per `audio.buffer` chunk.
	pub chunk_secs: u64,
}

impl Default for RewindConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			hours: 24,
			mic: true,
			system_audio: true,
			bitrate: 24_000,
			sample_rate: 16_000,
			chunk_secs: 60,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_enable_everything_but_audio_segments() {
		let config = CaptureConfig::default();
		assert!(config.screen.enabled && config.apps.enabled && config.input.enabled);
		assert!(!config.audio.enabled);
		assert_eq!(config.screen.interval_secs, 10);
		assert_eq!(config.audio.max_segment_secs, 30);
	}

	#[test]
	fn rewind_defaults_buffer_a_day_of_speech() {
		let rewind = RewindConfig::default();
		assert!(rewind.enabled && rewind.mic && rewind.system_audio);
		assert_eq!(rewind.hours, 24);
		assert_eq!(rewind.chunk_secs, 60);
		assert_eq!(rewind.sample_rate, 16_000);
		// A day of mono AAC at the default bitrate is roughly 260 MB.
		assert_eq!(u64::from(rewind.bitrate) * 3600 * 24 / 8 / 1_000_000, 259);
	}

	#[test]
	fn partial_json_fills_in_defaults() {
		let config: CaptureConfig =
			serde_json::from_str(r#"{"screen":{"interval_secs":30,"ocr_level":"accurate"}}"#)
				.unwrap();
		assert_eq!(config.screen.interval_secs, 30);
		assert_eq!(config.screen.ocr_level, OcrLevel::Accurate);
		assert_eq!(config.screen.max_width, 1600);
		assert_eq!(config.apps, AppsConfig::default());
		let json = serde_json::to_string(&config).unwrap();
		assert_eq!(
			serde_json::from_str::<CaptureConfig>(&json).unwrap(),
			config
		);
	}
}
