//! `quant rewind`: save moments from the rolling audio buffer.
//!
//! `save` keeps the last 10 minutes, or an explicit range; `list` shows the
//! saved clips; `export` and `play` fetch one; `status` says how much of
//! the last day is still buffered.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Subcommand;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_api::RecordsRequest;
use quant_api::SaveAudio;
use quant_client::when;

use crate::Ctx;

/// `quant rewind` actions.
#[derive(Subcommand)]
pub enum RewindAction {
	/// Save a moment from the rolling buffer as a clip (needs
	/// `audio:write`).
	Save {
		/// How far back to keep, such as `90s`, `10m`, or `1h`.
		duration: Option<String>,
		/// Start of the range instead of a duration.
		#[arg(long, allow_hyphen_values = true)]
		from: Option<String>,
		/// End of the range; defaults to now.
		#[arg(long, default_value = "now", allow_hyphen_values = true)]
		to: String,
		/// A name for the clip.
		#[arg(long)]
		title: Option<String>,
	},
	/// Saved clips (needs `audio:read`).
	List {
		/// Clips since this time.
		#[arg(long, default_value = "-7d", allow_hyphen_values = true)]
		from: String,
	},
	/// Write a clip's audio to a file (needs `audio:raw`).
	Export {
		/// The clip id from `quant rewind list`.
		clip: String,
		/// Where to write the `.m4a`.
		out: std::path::PathBuf,
	},
	/// Play a clip through the speakers (needs `audio:raw`).
	Play {
		/// The clip id from `quant rewind list`.
		clip: String,
	},
	/// How much of the last day is still buffered, and the disk it uses
	/// (needs `audio:read`).
	Status,
}

/// Run a `quant rewind` action.
pub async fn run(cx: &Ctx, action: RewindAction) -> Result<()> {
	match action {
		RewindAction::Save {
			duration,
			from,
			to,
			title,
		} => {
			let client = cx.client()?;
			let now = Timestamp::now();
			let (from, to) = match (&duration, &from) {
				(Some(_), Some(_)) => {
					bail!("pass either a duration or --from/--to, not both");
				}
				(Some(duration), None) => {
					let from =
						when::parse(&format!("-{duration}"), now).map_err(anyhow::Error::msg)?;
					(from, now)
				}
				(None, from) => {
					let from = when::parse_opt(from.as_deref(), now).map_err(anyhow::Error::msg)?;
					let to = when::parse(&to, now).map_err(anyhow::Error::msg)?;
					(from.unwrap_or(to), to)
				}
			};
			if to <= from {
				bail!("the range ends before it starts");
			}
			let saved = client
				.save_audio(&SaveAudio { from, to, title })
				.await
				.with_context(|| format!("saving {from} to {to}"))?;
			let span = to.duration_since(from);
			println!(
				"Saved clip {} ({} chunks, {} KB) covering {} → {}.",
				saved.id,
				saved.chunks,
				saved.bytes / 1024,
				saved.from,
				saved.to
			);
			let asked = span.as_secs_f64();
			#[allow(clippy::cast_possible_truncation)]
			let got = saved.to.duration_since(saved.from).as_secs_f64();
			if got + 5.0 < asked {
				println!("Only {got:.0} of the {asked:.0} seconds asked for were still buffered.");
			}
			Ok(())
		}
		RewindAction::List { from } => {
			let client = cx.client()?;
			let now = Timestamp::now();
			let from = when::parse(&from, now).map_err(anyhow::Error::msg)?;
			let clips = client
				.records(&RecordsRequest {
					streams: vec!["audio.clip".into()],
					from: Some(from),
					to: None,
					search: None,
					limit: Some(200),
					oldest_first: false,
				})
				.await?;
			if clips.records.is_empty() {
				println!("No saved clips.");
				return Ok(());
			}
			for clip in &clips.records {
				let title = clip
					.data
					.get("title")
					.and_then(|v| v.as_str())
					.unwrap_or("untitled")
					.split_whitespace()
					.collect::<Vec<_>>()
					.join(" ");
				let status = clip
					.data
					.get("transcript_status")
					.and_then(|value| value.as_str())
					.unwrap_or_default();
				println!(
					"{}  {:>9}  {:<8}  {}",
					clip.start,
					clip.end
						.map(|end| { format!("{}s", end.duration_since(clip.start).as_secs()) })
						.unwrap_or_default(),
					status,
					title
				);
			}
			Ok(())
		}
		RewindAction::Export { clip, out } => {
			let bytes = export(cx, &clip).await?;
			std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
			println!("Wrote {} ({} KB).", out.display(), bytes.len() / 1024);
			Ok(())
		}
		RewindAction::Play { clip } => {
			let out = std::env::temp_dir().join(format!("quant-clip-{clip}.m4a"));
			let bytes = export(cx, &clip).await?;
			std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
			let played = std::process::Command::new("afplay")
				.arg(&out)
				.status()
				.with_context(|| "playing the clip with afplay")?;
			let _ = std::fs::remove_file(&out);
			if !played.success() {
				bail!("afplay failed");
			}
			Ok(())
		}
		RewindAction::Status => {
			let client = cx.client()?;
			let clips = client
				.records(&RecordsRequest {
					streams: vec!["audio.buffer".into()],
					from: None,
					to: None,
					search: None,
					limit: Some(500),
					oldest_first: false,
				})
				.await?;
			let now = Timestamp::now();
			let mut seconds = 0_u64;
			let mut bytes = 0_u64;
			let mut newest = None;
			for chunk in &clips.records {
				let end = chunk.end.unwrap_or(chunk.start);
				seconds += end
					.duration_since(chunk.start)
					.as_secs()
					.max(0)
					.unsigned_abs();
				if let Some(size) = chunk.data.get("bytes").and_then(serde_json::Value::as_u64) {
					bytes += size;
				}
				if newest.is_none() {
					newest = Some(end);
				}
			}
			#[allow(clippy::cast_precision_loss)]
			let hours = seconds as f64 / 3600.0;
			println!(
				"Buffering {:.1} hours of audio, {} MB of chunks{}.",
				hours,
				bytes / (1024 * 1024),
				if clips.truncated {
					" (counting the newest 500 chunks)"
				} else {
					""
				},
			);
			if let Some(newest) = newest {
				let age = now.duration_since(newest);
				println!("Newest chunk closed {} ago.", hum(&age));
			} else {
				println!("Nothing buffered yet.");
			}
			Ok(())
		}
	}
}

/// Fetch a clip's audio bytes. `audio:raw` is what the media route needs.
async fn export(cx: &Ctx, clip: &str) -> Result<Vec<u8>> {
	let client = cx.client()?;
	let id: quant_core::RecordId = clip
		.parse()
		.with_context(|| format!("`{clip}` is not a clip id"))?;
	let found = client
		.records(&RecordsRequest {
			streams: vec!["audio.clip".into()],
			from: Some(Timestamp::now() - SignedDuration::from_hours(24 * 30)),
			to: None,
			search: None,
			limit: Some(500),
			oldest_first: false,
		})
		.await?;
	let record = found
		.records
		.iter()
		.find(|record| record.id == id)
		.with_context(|| {
			"no such clip (is it less than 30 days old, and does this grant read audio?)"
		})?;
	let media = record
		.media
		.as_ref()
		.context("this grant cannot fetch audio; it needs audio:raw")?;
	let (_, bytes) = client.media(&media.hash).await?;
	Ok(bytes.to_vec())
}

/// A short human duration.
fn hum(span: &SignedDuration) -> String {
	let seconds = span.as_secs().max(0);
	match seconds {
		0..60 => format!("{seconds}s"),
		60..3600 => format!("{}m", seconds / 60),
		_ => format!("{}h", seconds / 3600),
	}
}
