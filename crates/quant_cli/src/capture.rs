//! `quant capture`: what this machine records.

use anyhow::Context;
use anyhow::Result;
use clap::Subcommand;
use clap::ValueEnum;
use quant_api::PauseRequest;
use serde_json::Value;
use serde_json::json;

use crate::Ctx;
use crate::print;

/// `quant capture` actions. Each asks for the passphrase.
#[derive(Subcommand)]
pub enum CaptureAction {
	/// Show the capture settings.
	Show,
	/// Change what is captured. Running sources restart with the new
	/// settings.
	Set {
		/// Screenshots with on-device text recognition.
		#[arg(long, value_enum)]
		screen: Option<Toggle>,
		/// Focused app and window spans, lock and sleep transitions.
		#[arg(long, value_enum)]
		apps: Option<Toggle>,
		/// Key, click, and scroll counts (never content).
		#[arg(long, value_enum)]
		input: Option<Toggle>,
		/// Microphone speech segments.
		#[arg(long, value_enum)]
		audio: Option<Toggle>,
		/// Keep screenshots, not just their text.
		#[arg(long, value_enum)]
		images: Option<Toggle>,
		/// Keep audio clips, not just transcripts.
		#[arg(long, value_enum)]
		clips: Option<Toggle>,
		/// Text recognition effort.
		#[arg(long, value_enum)]
		ocr: Option<Ocr>,
		/// Seconds between screenshots.
		#[arg(long)]
		interval: Option<u64>,
		/// Never capture this app (a bundle id such as
		/// `com.1password.1password`).
		#[arg(long = "exclude-app")]
		exclude: Vec<String>,
		/// Stop excluding this app.
		#[arg(long = "include-app")]
		include: Vec<String>,
		/// A ggml Whisper model for transcripts (needs a build with the
		/// `whisper` feature).
		#[arg(long)]
		whisper_model: Option<std::path::PathBuf>,
	},
	/// Stop capturing until resumed, or for a while.
	Pause {
		/// Resume by itself after this many minutes.
		#[arg(long)]
		minutes: Option<u32>,
	},
	/// Resume capturing.
	Resume,
}

/// On or off.
#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
	/// Enable.
	On,
	/// Disable.
	Off,
}

impl Toggle {
	fn value(self) -> Value {
		Value::Bool(matches!(self, Self::On))
	}
}

/// Text recognition effort.
#[derive(Clone, Copy, ValueEnum)]
pub enum Ocr {
	/// Quicker and lighter.
	Fast,
	/// About four times the CPU; better on small or low-contrast text.
	Accurate,
}

/// Run a `quant capture` action.
pub async fn run(cx: &Ctx, action: CaptureAction) -> Result<()> {
	let owner = cx.owner().await?;
	match action {
		CaptureAction::Show => {
			let config = owner.capture_config().await?;
			println!("{}", serde_json::to_string_pretty(&config)?);
		}
		CaptureAction::Set {
			screen,
			apps,
			input,
			audio,
			images,
			clips,
			ocr,
			interval,
			exclude,
			include,
			whisper_model,
		} => {
			let mut config = owner.capture_config().await?;
			let mut put = |section: &str, key: &str, value: Value| -> Result<()> {
				config[section]
					.as_object_mut()
					.context("unexpected settings shape")?
					.insert(key.to_owned(), value);
				Ok(())
			};
			for (section, toggle) in [
				("screen", screen),
				("apps", apps),
				("input", input),
				("audio", audio),
			] {
				if let Some(toggle) = toggle {
					put(section, "enabled", toggle.value())?;
				}
			}
			if let Some(images) = images {
				put("screen", "store_images", images.value())?;
			}
			if let Some(clips) = clips {
				put("audio", "store_audio", clips.value())?;
			}
			if let Some(ocr) = ocr {
				let level = match ocr {
					Ocr::Fast => "fast",
					Ocr::Accurate => "accurate",
				};
				put("screen", "ocr_level", json!(level))?;
			}
			if let Some(interval) = interval {
				put("screen", "interval_secs", json!(interval.max(1)))?;
			}
			if let Some(model) = whisper_model {
				put("audio", "whisper_model", json!(model))?;
			}
			if !exclude.is_empty() || !include.is_empty() {
				let ids = config["screen"]["excluded_bundle_ids"]
					.as_array_mut()
					.context("unexpected settings shape")?;
				ids.retain(|id| !include.iter().any(|i| id == i.as_str()));
				for id in exclude {
					if !ids.iter().any(|existing| existing == id.as_str()) {
						ids.push(json!(id));
					}
				}
			}
			let config = owner.set_capture_config(&config).await?;
			println!("{}", serde_json::to_string_pretty(&config)?);
		}
		CaptureAction::Pause { minutes } => {
			let status = owner
				.pause_capture(&PauseRequest {
					paused: true,
					minutes,
				})
				.await?;
			cx.show(&status, print::status);
		}
		CaptureAction::Resume => {
			let status = owner
				.pause_capture(&PauseRequest {
					paused: false,
					minutes: None,
				})
				.await?;
			cx.show(&status, print::status);
		}
	}
	Ok(())
}
