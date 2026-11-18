//! `quant import`: pull existing history from Screenpipe or `ActivityWatch`
//! into the vault, through the normal signed ingest path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::Subcommand;
use jiff::Timestamp;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::Identity;
use quant_client::when;
use quant_import::ActivityWatchOptions;
use quant_import::ImportReport;
use quant_import::Importer;
use quant_import::ScreenpipeOptions;

use crate::Ctx;

/// Which tool's history to import.
#[derive(Subcommand)]
pub enum Source {
	/// Screenpipe, from its local SQLite database.
	Screenpipe(ScreenpipeArgs),
	/// `ActivityWatch`, from its server or an exported JSON file.
	#[command(name = "activitywatch")]
	ActivityWatch(ActivityWatchArgs),
}

/// The window and mode every import shares.
#[derive(Args)]
pub struct WindowArgs {
	/// Import from this time: `-2h`, `yesterday`, `2026-11-01`, or RFC 3339.
	/// Without it, an earlier import's watermark decides.
	#[arg(long, allow_hyphen_values = true)]
	since: Option<String>,
	/// Import up to this time, in the same forms.
	#[arg(long, allow_hyphen_values = true)]
	until: Option<String>,
	/// Keep importing as new data arrives, instead of importing once.
	#[arg(long)]
	follow: bool,
	/// How often `--follow` polls: `30s`, `5m`, `1h`.
	#[arg(long, default_value = "5m")]
	every: String,
	/// Print what would be imported, and write nothing.
	#[arg(long)]
	dry_run: bool,
}

/// `quant import screenpipe`.
#[derive(Args)]
pub struct ScreenpipeArgs {
	/// The Screenpipe database. Defaults to `~/.screenpipe/db.sqlite`.
	#[arg(long)]
	db: Option<PathBuf>,
	#[command(flatten)]
	window: WindowArgs,
}

/// `quant import activitywatch`.
#[derive(Args)]
pub struct ActivityWatchArgs {
	/// The aw-server REST API. Defaults to `http://localhost:5600`.
	#[arg(long, conflicts_with = "file")]
	url: Option<String>,
	/// An `ActivityWatch` export JSON file, imported offline.
	#[arg(long)]
	file: Option<PathBuf>,
	#[command(flatten)]
	window: WindowArgs,
}

impl WindowArgs {
	fn resolve(&self) -> Result<(Option<Timestamp>, Option<Timestamp>, Duration)> {
		let now = Timestamp::now();
		let since = when::parse_opt(self.since.as_deref(), now).map_err(anyhow::Error::msg)?;
		let until = when::parse_opt(self.until.as_deref(), now).map_err(anyhow::Error::msg)?;
		Ok((since, until, parse_every(&self.every)?))
	}
}

/// A plain interval: `30s`, `5m`, `1h`, or a bare number of minutes.
fn parse_every(input: &str) -> Result<Duration> {
	let input = input.trim();
	let split = input
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(input.len());
	let (number, unit) = input.split_at(split);
	let seconds = match unit.trim() {
		"s" => 1,
		"" | "m" | "min" => 60,
		"h" => 3600,
		other => bail!("cannot read `{other}` as an interval; use s, m, or h"),
	};
	let number: u64 = number.parse().context("the interval needs a number")?;
	Ok(Duration::from_secs(number * seconds))
}

/// The scopes an import writes with, shown when this profile has not paired.
fn scopes(source: &Source) -> Vec<String> {
	let families: &[&str] = match source {
		Source::Screenpipe(_) => &["apps", "screen", "audio"],
		Source::ActivityWatch(_) => &["apps", "device"],
	};
	families
		.iter()
		.map(|family| format!("{family}:write"))
		.collect()
}

/// Run one import.
pub async fn run(cx: &Ctx, source: Source) -> Result<()> {
	let (since, until, every) = match &source {
		Source::Screenpipe(args) => args.window.resolve()?,
		Source::ActivityWatch(args) => args.window.resolve()?,
	};
	let wanted = scopes(&source);
	let client = paired_client(cx, &wanted).await?;
	let mut importer = Importer::new(client, Identity::default_dir())
		.await
		.map_err(|error| hint(error, &wanted))?;
	importer.on_progress(|report| {
		println!("  {} so far…", brief(report));
	});
	let report = match &source {
		Source::Screenpipe(args) => {
			importer
				.screenpipe(ScreenpipeOptions {
					db: args.db.clone(),
					since,
					until,
					follow: args.window.follow,
					every,
					dry_run: args.window.dry_run,
				})
				.await
		}
		Source::ActivityWatch(args) => {
			importer
				.activitywatch(ActivityWatchOptions {
					url: args.url.clone(),
					file: args.file.clone(),
					since,
					until,
					follow: args.window.follow,
					every,
					dry_run: args.window.dry_run,
				})
				.await
		}
	}
	.map_err(|error| hint(error, &wanted))?;
	print_report(&report, args(&source).dry_run);
	Ok(())
}

fn args(source: &Source) -> &WindowArgs {
	match source {
		Source::Screenpipe(args) => &args.window,
		Source::ActivityWatch(args) => &args.window,
	}
}

/// This profile's client, pairing one the first time like `quant mcp` does.
async fn paired_client(cx: &Ctx, scopes: &[String]) -> Result<Client> {
	let path = cx.identity_path();
	if let Ok(identity) = Identity::load(&path) {
		return Ok(Client::new(cx.endpoint.clone(), identity));
	}
	let mut client = Client::new(
		cx.endpoint.clone(),
		Identity::generate(format!("quant import on {}", crate::hostname())),
	);
	match client.pair(ClientKind::Device, scopes, None).await {
		Ok(response) => {
			client.identity().save(&path)?;
			println!(
				"Pairing code: {}. Approve it with:\n  quant approve {}{}\nthen run the import \
				 again.",
				response.pairing_code,
				response.pairing_code,
				scope_flags(scopes)
			);
		}
		Err(error) => eprintln!("quant: could not pair yet: {error}"),
	}
	Ok(client)
}

fn scope_flags(scopes: &[String]) -> String {
	scopes.iter().fold(String::new(), |mut out, scope| {
		let _ = std::fmt::Write::write_fmt(&mut out, format_args!(" --scope {scope}"));
		out
	})
}

/// Point at the missing grant when the daemon refuses a write.
fn hint(error: quant_import::ImportError, wanted: &[String]) -> anyhow::Error {
	let forbidden = matches!(
		&error,
		quant_import::ImportError::Remote(quant_client::ClientError::Api { status: 403, .. })
	);
	if forbidden {
		return anyhow::Error::msg(format!(
			"{error}\nthis profile cannot write what the import needs; pair it with:\n  quant \
			 pair --kind device{} and approve the code",
			scope_flags(wanted)
		));
	}
	anyhow::Error::new(error)
}

/// One line of progress.
fn brief(report: &ImportReport) -> String {
	let total: usize = report.counts.values().sum();
	if report.stored == 0 && report.duplicate == 0 {
		format!("{total} records")
	} else {
		format!("{total} records, {} new", report.stored)
	}
}

/// The summary an import ends with.
fn print_report(report: &ImportReport, dry_run: bool) {
	if report.counts.is_empty() {
		println!("Nothing new to import.");
		return;
	}
	let counts = report
		.counts
		.iter()
		.map(|(stream, count)| format!("{count} {stream}"))
		.collect::<Vec<_>>()
		.join(", ");
	let covered = match (report.from, report.to) {
		(Some(from), Some(to)) => format!(" covering {from} to {to}"),
		_ => String::new(),
	};
	if dry_run {
		println!("Would import {counts}{covered}.");
		println!("Dry run: nothing was written.");
	} else {
		println!(
			"Imported {} new record(s), {} already present{covered}: {counts}.",
			report.stored, report.duplicate
		);
	}
}
