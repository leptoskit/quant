//! `quant`: quantify your life, and decide which AI may read it.

mod capture;
mod daemon;
mod hardware;
mod print;
mod relay;
mod rewind;
mod service;

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use jiff::Timestamp;
use quant_access::Limits;
use quant_api::ApproveRequest;
use quant_api::AuditRequest;
use quant_api::ClaimCorrectRequest;
use quant_api::ClaimListRequest;
use quant_api::ClientKind;
use quant_api::DurationsRequest;
use quant_api::IngestRecords;
use quant_api::OverviewRequest;
use quant_api::OwnerClaimRequest;
use quant_api::PurgeRequest;
use quant_api::RecordsRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_client::when;
use quant_core::ClaimId;
use quant_core::ClaimKind;
use quant_core::ClaimStatus;
use quant_core::Class;
use quant_core::DeviceId;
use quant_core::Record;
use quant_store::VaultPaths;

#[derive(Parser)]
#[command(
	name = "quant",
	version,
	about = "Quantify your life, and decide which AI may read it."
)]
struct Cli {
	/// The vault directory. Defaults to the platform data dir.
	#[arg(long, global = true, env = "QUANT_HOME")]
	vault: Option<PathBuf>,
	/// Reach the daemon somewhere other than the vault's socket:
	/// `tls://host:port#<fingerprint>`, `tcp://127.0.0.1:port`, or a socket
	/// path.
	#[arg(long, global = true, env = "QUANT_ADDR")]
	addr: Option<String>,
	/// Which client identity to use for queries.
	#[arg(long, global = true, default_value = "cli", env = "QUANT_PROFILE")]
	profile: String,
	/// Print JSON instead of tables.
	#[arg(long, global = true)]
	json: bool,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Create a vault, sealed under a passphrase you choose.
	Init {
		/// Don't keep a copy of the vault key in the OS keychain. The daemon
		/// will then ask for the passphrase every time it starts.
		#[arg(long)]
		no_keychain: bool,
	},
	/// Run the daemon in the foreground.
	Daemon(daemon::DaemonArgs),
	/// Install, remove, or inspect the background service.
	Service {
		#[command(subcommand)]
		action: service::Action,
	},
	/// Ask the vault to pair this terminal (or a named profile).
	Pair {
		/// A name the owner will recognise.
		#[arg(long)]
		name: Option<String>,
		/// What kind of client this is.
		#[arg(long, value_enum, default_value_t = Kind::Cli)]
		kind: Kind,
		/// Scopes to ask for, such as `health:summary`.
		#[arg(long = "scope")]
		scopes: Vec<String>,
	},
	/// Approve a pairing (asks for the passphrase).
	Approve(ApproveArgs),
	/// Refuse a pairing (asks for the passphrase).
	Deny {
		/// Pairing code or client id.
		client: String,
	},
	/// Revoke a grant (asks for the passphrase).
	Revoke {
		/// Grant id, client id, or pairing code.
		id: String,
	},
	/// List clients and their grants (asks for the passphrase).
	Clients,
	/// Daemon and capture status (asks for the passphrase).
	Status,
	/// Read the audit log and check its chain (asks for the passphrase).
	Audit {
		/// Entries since this time.
		#[arg(long, default_value = "-24h", allow_hyphen_values = true)]
		since: String,
		/// Most entries.
		#[arg(long, default_value_t = 100)]
		limit: usize,
	},
	/// Permanently delete data (asks for the passphrase).
	Purge {
		/// Streams or families, such as `screen` or `audio.transcript`.
		#[arg(long = "stream", required = true)]
		streams: Vec<String>,
		/// Start.
		#[arg(long, allow_hyphen_values = true)]
		from: String,
		/// End.
		#[arg(long, default_value = "now", allow_hyphen_values = true)]
		to: String,
	},
	/// This client's grant.
	Me,
	/// Streams this client may see.
	Streams,
	/// What was captured over a period.
	Overview(Window),
	/// Records in time order.
	Timeline {
		#[command(flatten)]
		window: Window,
		/// Streams or families.
		#[arg(long = "stream")]
		streams: Vec<String>,
		/// Most records.
		#[arg(long, default_value_t = 50)]
		limit: usize,
	},
	/// Full-text search over screens, transcripts, and notes.
	Search {
		/// Search terms.
		terms: Vec<String>,
		#[command(flatten)]
		window: Window,
		/// Streams or families.
		#[arg(long = "stream")]
		streams: Vec<String>,
		/// Most results.
		#[arg(long, default_value_t = 20)]
		limit: usize,
		/// How to match: keyword, similar (on-device embeddings), or hybrid
		/// (both fused; the default when embeddings exist).
		#[arg(long, value_enum)]
		mode: Option<SearchModeArg>,
	},
	/// A numeric stream, summarised.
	Series {
		/// Such as `health.heart_rate`.
		stream: String,
		#[command(flatten)]
		window: Window,
		/// Bucket width in seconds.
		#[arg(long)]
		bucket: Option<u64>,
		/// Raw samples instead of buckets.
		#[arg(long)]
		raw: bool,
	},
	/// Time spent per app, posture, sleep stage, …
	Time {
		/// Such as `apps.focus` or `motion.posture`.
		stream: String,
		/// Attribute to group by, such as `app`.
		#[arg(long = "by")]
		group_by: Option<String>,
		#[command(flatten)]
		window: Window,
		/// Bucket width in seconds.
		#[arg(long)]
		bucket: Option<u64>,
	},
	/// Write a note into memory.
	Note {
		/// The note.
		text: Vec<String>,
	},
	/// State something about yourself (asks for the passphrase).
	Tell {
		/// The statement, in your own words.
		statement: Vec<String>,
		/// What sort of claim.
		#[arg(long, value_enum, default_value_t = ClaimKindArg::Preference)]
		kind: ClaimKindArg,
		/// A short dotted key such as `coffee.timing`. Active claims with the
		/// same subject and topic and a lower authority are superseded.
		#[arg(long)]
		topic: Option<String>,
		/// Who it is about; `me` by default.
		#[arg(long, default_value = "me")]
		subject: String,
		/// Sensitivity classes, such as `health`.
		#[arg(long = "class", value_enum, value_delimiter = ',')]
		classes: Vec<ClassArg>,
	},
	/// See and manage the claims Quant holds about you (asks for the
	/// passphrase).
	Claims {
		#[command(subcommand)]
		action: ClaimsAction,
	},
	/// Your profile: what you told Quant about yourself, then the rest by
	/// confidence (asks for the passphrase).
	Profile,
	/// Register custom streams for new hardware.
	Stream {
		#[command(subcommand)]
		action: hardware::StreamAction,
	},
	/// Choose what this machine captures, or pause it.
	Capture {
		#[command(subcommand)]
		action: capture::CaptureAction,
	},
	/// Point the daemon at a relay so clients reach it from anywhere
	/// (asks for the passphrase).
	Relay(relay::RelayArgs),
	/// Save moments from the rolling audio buffer, and see what it holds.
	Rewind {
		#[command(subcommand)]
		action: rewind::RewindAction,
	},
	/// Configure derived memory, such as where accelerometers are worn.
	Derive {
		#[command(subcommand)]
		action: hardware::DeriveAction,
	},
	/// Configure on-device learning (asks for the passphrase): a loopback
	/// embedding endpoint for similarity search and a loopback chat model
	/// that proposes claims, grounded in the records it read.
	Learn {
		#[command(subcommand)]
		action: LearnAction,
	},
	/// Send samples or records from stdin: pipe a serial port, a script, or
	/// an export into the vault. Needs a profile with `<stream>:write`.
	Ingest {
		/// The stream, such as `sensor.vest.ecg`.
		#[arg(long)]
		stream: String,
		/// Input format.
		#[arg(long, value_enum, default_value_t = hardware::Format::Csv)]
		format: hardware::Format,
		/// Channel names in column order, unless a CSV header names them.
		#[arg(long, value_delimiter = ',')]
		channels: Vec<String>,
		/// Lines are records (JSON), not samples.
		#[arg(long)]
		records: bool,
		/// Seconds of samples per uploaded batch.
		#[arg(long, default_value_t = 2)]
		batch_seconds: u64,
	},
	/// Print the link (and QR code) a phone scans to reach this daemon over
	/// the network. The daemon must be running with `--tcp`.
	Link {
		/// The address phones should use; defaults to this machine's LAN
		/// address.
		#[arg(long)]
		host: Option<String>,
		/// The port `quant daemon --tcp` listens on.
		#[arg(long, default_value_t = 7717)]
		port: u16,
	},
	/// Run the MCP server on stdio for an AI agent.
	Mcp {
		/// Scopes to ask for if this profile has not paired yet.
		#[arg(long = "scope", default_values_t = ["*:summary".to_owned()])]
		scopes: Vec<String>,
		/// Print the command that registers this server with Claude Code, then
		/// exit.
		#[arg(long)]
		setup: bool,
	},
}

#[derive(Args)]
struct Window {
	/// Start: RFC 3339, YYYY-MM-DD, today, yesterday, or -2h / -7d.
	#[arg(long, allow_hyphen_values = true)]
	from: Option<String>,
	/// End, in the same forms.
	#[arg(long, allow_hyphen_values = true)]
	to: Option<String>,
}

impl Window {
	fn resolve(&self) -> Result<(Option<Timestamp>, Option<Timestamp>)> {
		let now = Timestamp::now();
		Ok((
			when::parse_opt(self.from.as_deref(), now).map_err(anyhow::Error::msg)?,
			when::parse_opt(self.to.as_deref(), now).map_err(anyhow::Error::msg)?,
		))
	}
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Kind {
	Agent,
	Cli,
	Device,
	App,
}

impl From<Kind> for ClientKind {
	fn from(kind: Kind) -> Self {
		match kind {
			Kind::Agent => ClientKind::Agent,
			Kind::Cli => ClientKind::Cli,
			Kind::Device => ClientKind::Device,
			Kind::App => ClientKind::App,
		}
	}
}

/// A claim kind, on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ClaimKindArg {
	Preference,
	Value,
	Routine,
	Fact,
	Relationship,
	Goal,
	Commitment,
}

impl From<ClaimKindArg> for ClaimKind {
	fn from(kind: ClaimKindArg) -> Self {
		match kind {
			ClaimKindArg::Preference => ClaimKind::Preference,
			ClaimKindArg::Value => ClaimKind::Value,
			ClaimKindArg::Routine => ClaimKind::Routine,
			ClaimKindArg::Fact => ClaimKind::Fact,
			ClaimKindArg::Relationship => ClaimKind::Relationship,
			ClaimKindArg::Goal => ClaimKind::Goal,
			ClaimKindArg::Commitment => ClaimKind::Commitment,
		}
	}
}

/// A sensitivity class, on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ClassArg {
	Health,
	Intimate,
	Location,
	Others,
	Financial,
}

impl From<ClassArg> for Class {
	fn from(class: ClassArg) -> Self {
		match class {
			ClassArg::Health => Class::Health,
			ClassArg::Intimate => Class::Intimate,
			ClassArg::Location => Class::Location,
			ClassArg::Others => Class::Others,
			ClassArg::Financial => Class::Financial,
		}
	}
}

/// A claim status, on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum StatusArg {
	Active,
	Held,
	Rejected,
	Retracted,
	Superseded,
}

impl From<StatusArg> for ClaimStatus {
	fn from(status: StatusArg) -> Self {
		match status {
			StatusArg::Active => ClaimStatus::Active,
			StatusArg::Held => ClaimStatus::Held,
			StatusArg::Rejected => ClaimStatus::Rejected,
			StatusArg::Retracted => ClaimStatus::Retracted,
			StatusArg::Superseded => ClaimStatus::Superseded,
		}
	}
}

/// A search mode, on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum SearchModeArg {
	Keyword,
	Similar,
	Hybrid,
}

impl From<SearchModeArg> for quant_api::SearchMode {
	fn from(mode: SearchModeArg) -> Self {
		match mode {
			SearchModeArg::Keyword => quant_api::SearchMode::Keyword,
			SearchModeArg::Similar => quant_api::SearchMode::Similar,
			SearchModeArg::Hybrid => quant_api::SearchMode::Hybrid,
		}
	}
}

/// What to do with on-device learning.
#[derive(Subcommand)]
enum LearnAction {
	/// Show the learning settings.
	Show,
	/// Set the embedding or extraction endpoint. Endpoints must be literal
	/// loopback addresses (`127.0.0.1` or `[::1]`), so vault text can never
	/// leave the machine through a changed DNS name or a proxy.
	Set {
		/// The embedding endpoint, such as `http://127.0.0.1:11434`
		/// (Ollama or llama.cpp server).
		#[arg(long)]
		embed_endpoint: Option<String>,
		/// The embedding model name.
		#[arg(long)]
		embed_model: Option<String>,
		/// The embedding model's dimensions.
		#[arg(long)]
		embed_dims: Option<usize>,
		/// The extraction endpoint, such as `http://127.0.0.1:8080`.
		#[arg(long)]
		extract_endpoint: Option<String>,
		/// The extraction model name.
		#[arg(long)]
		extract_model: Option<String>,
		/// Hard cap on extraction calls per day.
		#[arg(long)]
		calls_per_day: Option<u32>,
		/// Hard cap on claims stored per extraction run.
		#[arg(long)]
		claims_per_run: Option<usize>,
		/// Streams or families to embed; defaults to screen text, speech
		/// transcripts, and memory records.
		#[arg(long = "stream")]
		streams: Vec<String>,
	},
	/// Turn on-device learning off.
	Off,
}

/// What to do with claims.
#[derive(Subcommand)]
enum ClaimsAction {
	/// List claims.
	List {
		/// Only this status.
		#[arg(long, value_enum)]
		status: Option<StatusArg>,
		/// Most claims.
		#[arg(long)]
		limit: Option<usize>,
	},
	/// Approve a held proposal; it becomes active.
	Approve {
		/// The claim id.
		id: ClaimId,
	},
	/// Refuse a held proposal.
	Reject {
		/// The claim id.
		id: ClaimId,
	},
	/// Withdraw a claim.
	Retract {
		/// The claim id.
		id: ClaimId,
	},
	/// Correct a claim; a new owner claim supersedes it.
	Correct {
		/// The claim id.
		id: ClaimId,
		/// The corrected statement, in your own words.
		statement: Vec<String>,
	},
}

#[derive(Args)]
struct ApproveArgs {
	/// Pairing code or client id.
	client: String,
	/// Scopes to grant; defaults to what the client asked for.
	#[arg(long = "scope")]
	scopes: Vec<String>,
	/// A label for the grant.
	#[arg(long)]
	label: Option<String>,
	/// How far back it may see, in hours.
	#[arg(long)]
	lookback_hours: Option<u32>,
	/// Hours until the grant expires.
	#[arg(long)]
	expires_in_hours: Option<u32>,
	/// Requests per minute.
	#[arg(long)]
	requests_per_minute: Option<u32>,
	/// Records per hour.
	#[arg(long)]
	records_per_hour: Option<u32>,
	/// Megabytes per day.
	#[arg(long)]
	mb_per_day: Option<u64>,
	/// Serve text without redacting secrets.
	#[arg(long)]
	unredacted: bool,
	/// Sensitivity classes the grant never sees.
	#[arg(long = "deny", value_enum, value_delimiter = ',')]
	deny_classes: Vec<ClassArg>,
}

pub(crate) struct Ctx {
	paths: VaultPaths,
	endpoint: Endpoint,
	profile: String,
	json: bool,
}

impl Ctx {
	fn identity_path(&self) -> PathBuf {
		Identity::path(&Identity::default_dir(), &self.profile)
	}

	fn client(&self) -> Result<Client> {
		let path = self.identity_path();
		let identity = Identity::load(&path).with_context(|| {
			format!(
				"no identity for profile `{}`; run `quant pair --profile {}` first",
				self.profile, self.profile
			)
		})?;
		Ok(Client::new(self.endpoint.clone(), identity))
	}

	pub(crate) async fn owner(&self) -> Result<OwnerSession> {
		let passphrase =
			rpassword::prompt_password("Vault passphrase: ").context("reading the passphrase")?;
		Ok(OwnerSession::open(self.endpoint.clone(), &passphrase).await?)
	}

	fn show<T: serde::Serialize>(&self, value: &T, human: impl FnOnce(&T)) {
		if self.json {
			println!(
				"{}",
				serde_json::to_string_pretty(value).unwrap_or_default()
			);
		} else {
			human(value);
		}
	}
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let paths = VaultPaths::new(cli.vault.clone().unwrap_or_else(VaultPaths::default_root));
	let endpoint = match &cli.addr {
		Some(addr) => Endpoint::parse(addr).map_err(anyhow::Error::msg)?,
		None => Endpoint::Unix(paths.socket()),
	};
	let connects = !matches!(
		cli.command,
		Command::Init { .. } | Command::Daemon(_) | Command::Service { .. }
	);
	if connects && matches!(endpoint, Endpoint::Unix(_)) {
		paths.secure_socket_dir()?;
	}
	let cx = Ctx {
		paths,
		endpoint,
		profile: cli.profile.clone(),
		json: cli.json,
	};

	match cli.command {
		Command::Init { no_keychain } => init(&cx, no_keychain),
		Command::Daemon(args) => daemon::run(&cx.paths, args).await,
		Command::Service { action } => service::run(&cx.paths, &action),
		Command::Pair { name, kind, scopes } => pair(&cx, name, kind, &scopes).await,
		Command::Approve(args) => approve(&cx, args).await,
		Command::Deny { client } => {
			cx.owner().await?.deny(&client).await?;
			println!("Refused {client}.");
			Ok(())
		}
		Command::Revoke { id } => {
			let grant = cx.owner().await?.revoke(&id).await?;
			println!("Revoked grant {} ({}).", grant.id, grant.label);
			Ok(())
		}
		Command::Clients => {
			let clients = cx.owner().await?.clients().await?;
			cx.show(&clients, |c| print::clients(c));
			Ok(())
		}
		Command::Status => {
			let status = cx.owner().await?.status().await?;
			cx.show(&status, print::status);
			Ok(())
		}
		Command::Audit { since, limit } => {
			let since = when::parse(&since, Timestamp::now()).map_err(anyhow::Error::msg)?;
			let audit = cx
				.owner()
				.await?
				.audit(&AuditRequest {
					since: Some(since),
					limit: Some(limit),
					newest: false,
				})
				.await?;
			cx.show(&audit, print::audit);
			Ok(())
		}
		Command::Purge { streams, from, to } => purge(&cx, streams, &from, &to).await,
		Command::Me => {
			let me = cx.client()?.me().await?;
			cx.show(&me, print::me);
			Ok(())
		}
		Command::Streams => {
			let streams = cx.client()?.streams().await?;
			cx.show(&streams, |s| print::streams(s));
			Ok(())
		}
		Command::Overview(window) => {
			let (from, to) = window.resolve()?;
			let overview = cx.client()?.overview(&OverviewRequest { from, to }).await?;
			cx.show(&overview, print::overview);
			Ok(())
		}
		Command::Timeline {
			window,
			streams,
			limit,
		} => {
			let (from, to) = window.resolve()?;
			let request = RecordsRequest {
				streams,
				from,
				to,
				search: None,
				limit: Some(limit),
				oldest_first: false,
				mode: None,
			};
			let records = cx.client()?.records(&request).await?;
			cx.show(&records, print::records);
			Ok(())
		}
		Command::Search {
			terms,
			window,
			streams,
			limit,
			mode,
		} => {
			let (from, to) = window.resolve()?;
			let request = RecordsRequest {
				streams,
				from,
				to,
				search: Some(terms.join(" ")),
				limit: Some(limit),
				oldest_first: false,
				mode: mode.map(quant_api::SearchMode::from),
			};
			let records = cx.client()?.search(&request).await?;
			cx.show(&records, print::records);
			Ok(())
		}
		Command::Series {
			stream,
			window,
			bucket,
			raw,
		} => {
			let (from, to) = window.resolve()?;
			let request = SeriesRequest {
				stream,
				from,
				to,
				bucket_seconds: bucket,
				raw,
				max_rows: None,
			};
			let series = cx.client()?.series(&request).await?;
			cx.show(&series, print::series);
			Ok(())
		}
		Command::Time {
			stream,
			group_by,
			window,
			bucket,
		} => {
			let (from, to) = window.resolve()?;
			let request = DurationsRequest {
				stream,
				from,
				to,
				group_by,
				bucket_seconds: bucket,
			};
			let durations = cx.client()?.durations(&request).await?;
			cx.show(&durations, print::durations);
			Ok(())
		}
		Command::Note { text } => note(&cx, &text.join(" ")).await,
		Command::Tell {
			statement,
			kind,
			topic,
			subject,
			classes,
		} => {
			let claim = cx
				.owner()
				.await?
				.tell(&OwnerClaimRequest {
					statement: statement.join(" "),
					kind: Some(kind.into()),
					topic,
					subject: Some(subject),
					classes: classes.into_iter().map(Class::from).collect(),
				})
				.await?;
			cx.show(&claim, print::claim);
			Ok(())
		}
		Command::Claims { action } => claims(&cx, action).await,
		Command::Profile => profile(&cx).await,
		Command::Mcp { scopes, setup } => mcp(&cx, &scopes, setup).await,
		Command::Link { host, port } => link(&cx, host, port).await,
		Command::Stream { action } => hardware::stream(&cx, action).await,
		Command::Capture { action } => capture::run(&cx, action).await,
		Command::Relay(args) => relay::run(&cx, args).await,
		Command::Rewind { action } => rewind::run(&cx, action).await,
		Command::Derive { action } => hardware::derive(&cx, action).await,
		Command::Learn { action } => learn(&cx, action).await,
		Command::Ingest {
			stream,
			format,
			channels,
			records,
			batch_seconds,
		} => {
			let options = hardware::Ingest {
				stream,
				format,
				channels,
				records,
				batch_seconds,
			};
			hardware::ingest(&cx.client()?, options).await
		}
	}
}

fn init(cx: &Ctx, no_keychain: bool) -> Result<()> {
	if cx.paths.exists() {
		bail!("a vault already exists at {}", cx.paths.root.display());
	}
	println!("Creating a Quant vault at {}.", cx.paths.root.display());
	println!(
		"Choose a passphrase of at least {} characters. Without it the vault cannot be",
		quant_crypto::MIN_PASSPHRASE
	);
	println!("recovered on another machine, and no agent can be granted access.");
	let first = rpassword::prompt_password("New passphrase: ")?;
	let second = rpassword::prompt_password("Again: ")?;
	if first != second {
		bail!("the passphrases differ");
	}
	let vault =
		quant_store::Vault::create(cx.paths.clone(), &first, quant_crypto::KdfParams::DEFAULT)?;
	if no_keychain {
		println!(
			"The vault key is not in the keychain; `quant daemon` will ask for the passphrase."
		);
	} else {
		quant_crypto::keychain::store(&vault.manifest.vault_id, vault.key())?;
		println!("Saved the vault key to the keychain so the daemon can start at login.");
	}
	println!("\nNext: `quant service install` to capture in the background, then");
	println!("`quant mcp --setup` to connect an AI agent.");
	Ok(())
}

async fn pair(cx: &Ctx, name: Option<String>, kind: Kind, scopes: &[String]) -> Result<()> {
	let path = cx.identity_path();
	let identity = match Identity::load(&path) {
		Ok(identity) => identity,
		Err(_) => {
			Identity::generate(
				name.unwrap_or_else(|| format!("quant {} on {}", cx.profile, hostname())),
			)
		}
	};
	let mut client = Client::new(cx.endpoint.clone(), identity);
	let response = client.pair(kind.into(), scopes).await?;
	client.identity().save(&path)?;
	println!("Pairing code: {}", response.pairing_code);
	println!("Status: {:?}", response.status);
	if response.status == quant_api::ClientStatus::Pending {
		println!(
			"\nApprove it with:\n  quant approve {}{}",
			response.pairing_code,
			scope_flags(scopes)
		);
	}
	Ok(())
}

fn scope_flags(scopes: &[String]) -> String {
	scopes.iter().fold(String::new(), |mut out, s| {
		let _ = std::fmt::Write::write_fmt(&mut out, format_args!(" --scope {s}"));
		out
	})
}

async fn approve(cx: &Ctx, args: ApproveArgs) -> Result<()> {
	let owner = cx.owner().await?;
	let limits = (args.requests_per_minute.is_some()
		|| args.records_per_hour.is_some()
		|| args.mb_per_day.is_some())
	.then(|| {
		let defaults = Limits::default();
		Limits {
			requests_per_minute: args
				.requests_per_minute
				.unwrap_or(defaults.requests_per_minute),
			records_per_hour: args.records_per_hour.unwrap_or(defaults.records_per_hour),
			bytes_per_day: args
				.mb_per_day
				.map_or(defaults.bytes_per_day, |mb| mb * 1024 * 1024),
		}
	});
	let request = ApproveRequest {
		client: args.client,
		scopes: (!args.scopes.is_empty()).then_some(args.scopes),
		label: args.label,
		lookback_hours: args.lookback_hours,
		expires_in_hours: args.expires_in_hours,
		limits,
		unredacted: args.unredacted,
		deny_classes: (!args.deny_classes.is_empty())
			.then(|| args.deny_classes.into_iter().map(Class::from).collect()),
	};
	let view = owner.approve(&request).await?;
	cx.show(&view, |v| print::clients(std::slice::from_ref(v)));
	Ok(())
}

async fn purge(cx: &Ctx, streams: Vec<String>, from: &str, to: &str) -> Result<()> {
	let now = Timestamp::now();
	let from = when::parse(from, now).map_err(anyhow::Error::msg)?;
	let to = when::parse(to, now).map_err(anyhow::Error::msg)?;
	println!(
		"This permanently deletes {} from {from} to {to}.",
		streams.join(", ")
	);
	let owner = cx.owner().await?;
	let purged = owner.purge(&PurgeRequest { streams, from, to }).await?;
	println!(
		"Deleted {} records, {} series batches, {} media files.",
		purged.records, purged.series, purged.media
	);
	Ok(())
}

async fn note(cx: &Ctx, text: &str) -> Result<()> {
	let client = cx.client()?;
	let id = client
		.identity()
		.client
		.context("this profile has not paired")?;
	let record = Record::new("memory.note".parse()?, DeviceId(id.0))
		.with_text(text)
		.with("author", client.identity().name.clone());
	let stored = client
		.ingest_records(&IngestRecords {
			records: vec![record],
			device_name: None,
			platform: Some("cli".into()),
		})
		.await?;
	println!("Stored {} note.", stored.stored);
	Ok(())
}

async fn claims(cx: &Ctx, action: ClaimsAction) -> Result<()> {
	match action {
		ClaimsAction::List { status, limit } => {
			let claims = cx
				.owner()
				.await?
				.claims(&ClaimListRequest {
					status: status.map(ClaimStatus::from),
					authority: None,
					kind: None,
					subject: None,
					topic: None,
					limit,
				})
				.await?;
			cx.show(&claims, |c| print::claims(c));
		}
		ClaimsAction::Approve { id } => {
			let claim = cx.owner().await?.claim_approve(id).await?;
			cx.show(&claim, print::claim);
		}
		ClaimsAction::Reject { id } => {
			let claim = cx.owner().await?.claim_reject(id).await?;
			cx.show(&claim, print::claim);
		}
		ClaimsAction::Retract { id } => {
			let claim = cx.owner().await?.claim_retract(id).await?;
			cx.show(&claim, print::claim);
		}
		ClaimsAction::Correct { id, statement } => {
			let claim = cx
				.owner()
				.await?
				.claim_correct(&ClaimCorrectRequest {
					id,
					statement: statement.join(" "),
				})
				.await?;
			cx.show(&claim, print::claim);
		}
	}
	Ok(())
}

async fn profile(cx: &Ctx) -> Result<()> {
	let mut claims = cx
		.owner()
		.await?
		.claims(&ClaimListRequest::default())
		.await?;
	// The owner's view: their own words first, then the rest by confidence.
	claims.sort_by(|a, b| {
		let own = |c: &quant_core::Claim| {
			matches!(
				c.authority,
				quant_core::Authority::Owner | quant_core::Authority::OwnerAuthored
			)
		};
		own(b)
			.cmp(&own(a))
			.then(b.confidence.total_cmp(&a.confidence))
			.then(b.updated_at.cmp(&a.updated_at))
	});
	cx.show(&claims, |c| print::claims(c));
	Ok(())
}

async fn learn(cx: &Ctx, action: LearnAction) -> Result<()> {
	let owner = cx.owner().await?;
	match action {
		LearnAction::Show => {
			let config = owner.learn_config().await?;
			cx.show(&config, print::learn_config);
		}
		LearnAction::Off => {
			owner.learn_off().await?;
			println!("On-device learning is off.");
		}
		LearnAction::Set {
			embed_endpoint,
			embed_model,
			embed_dims,
			extract_endpoint,
			extract_model,
			calls_per_day,
			claims_per_run,
			streams,
		} => {
			let mut config: quant_learn::LearnConfig =
				serde_json::from_value(owner.learn_config().await?)?;
			let embed_flags = [&embed_endpoint, &embed_model]
				.iter()
				.any(|flag| flag.is_some())
				|| embed_dims.is_some();
			if embed_flags && config.embed.is_none() {
				bail!(
					"new embedding settings need --embed-endpoint, --embed-model, and \
					 --embed-dims together"
				);
			}
			if let Some(embed) = &mut config.embed {
				if let Some(endpoint) = embed_endpoint {
					embed.endpoint = endpoint;
				}
				if let Some(model) = embed_model {
					embed.model = model;
				}
				if let Some(dims) = embed_dims {
					embed.dims = dims;
				}
			}
			let extract_flags = [&extract_endpoint, &extract_model]
				.iter()
				.any(|flag| flag.is_some())
				|| calls_per_day.is_some()
				|| claims_per_run.is_some();
			if extract_flags && config.extract.is_none() {
				bail!(
					"new extraction settings need --extract-endpoint and --extract-model together"
				);
			}
			if let Some(extract) = &mut config.extract {
				if let Some(endpoint) = extract_endpoint {
					extract.endpoint = endpoint;
				}
				if let Some(model) = extract_model {
					extract.model = model;
				}
				if let Some(calls) = calls_per_day {
					extract.calls_per_day = calls;
				}
				if let Some(claims) = claims_per_run {
					extract.claims_per_run = claims;
				}
			}
			if !streams.is_empty() {
				config.streams = Some(streams);
			}
			config.validate().map_err(anyhow::Error::msg)?;
			let answer = owner
				.set_learn_config(&serde_json::to_value(&config)?)
				.await?;
			cx.show(&answer, print::learn_config);
		}
	}
	Ok(())
}

async fn mcp(cx: &Ctx, scopes: &[String], setup: bool) -> Result<()> {
	if setup {
		let exe = std::env::current_exe()?;
		println!("Register Quant with Claude Code:\n");
		println!(
			"  claude mcp add quant -- {} mcp --profile {}",
			exe.display(),
			cx.profile
		);
		println!(
			"\nThe first time it runs it asks the vault to pair, scoped to {}.",
			scopes.join(", ")
		);
		println!(
			"Approve it with `quant approve <code>`; the code is printed to stderr and shown by"
		);
		println!("the agent's quant_access_status tool.");
		return Ok(());
	}
	let path = cx.identity_path();
	let client = if let Ok(identity) = Identity::load(&path) {
		Client::new(cx.endpoint.clone(), identity)
	} else {
		let mut client = Client::new(
			cx.endpoint.clone(),
			Identity::generate(format!("{} (MCP)", cx.profile)),
		);
		match client.pair(ClientKind::Agent, scopes).await {
			Ok(response) => {
				client.identity().save(&path)?;
				eprintln!(
					"quant: asked the vault to pair this agent. Approve it with `quant approve \
					 {}`.",
					response.pairing_code
				);
			}
			Err(error) => eprintln!("quant: could not pair yet: {error}"),
		}
		client
	};
	quant_mcp::serve_stdio(client).await?;
	Ok(())
}

async fn link(cx: &Ctx, host: Option<String>, port: u16) -> Result<()> {
	let hello = Client::new(cx.endpoint.clone(), Identity::generate("link"))
		.hello()
		.await?;
	let fingerprint = hello.tls_fingerprint.context(
		"the daemon is not listening on the network; start it with `quant daemon --tcp \
		 0.0.0.0:7717`",
	)?;
	let host = match host {
		Some(host) => host,
		None => lan_address().context("could not find this machine's LAN address; pass --host")?,
	};
	let mut link = format!(
		"quant://link?addr={host}:{port}&fp={fingerprint}&vault={}",
		hello.vault_id
	);
	let relayed = hello.relay.is_some();
	if let Some(relay) = hello.relay
		&& let Some(rid) = hello.relay_rid
	{
		let encoded = relay.replace(':', "%3A").replace('/', "%2F");
		link.push_str("&relay=");
		link.push_str(&encoded);
		link.push_str("&rid=");
		link.push_str(&rid);
	}
	let code = qrcode::QrCode::new(link.as_bytes())?;
	println!(
		"{}",
		code.render::<qrcode::render::unicode::Dense1x2>()
			.quiet_zone(true)
			.build()
	);
	println!("{link}\n");
	println!("Scan this in the Quant app on your phone. It will ask to pair; approve it with");
	println!("`quant approve <code>` and the scopes the phone should write, such as");
	println!("--scope health:write --scope motion:write --scope location:write.");
	if relayed {
		println!("The link carries a relay, so the phone can also reach this vault from anywhere.");
	}
	Ok(())
}

/// The address this machine uses to reach the network (no packet is sent).
fn lan_address() -> Option<String> {
	let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
	socket.connect("192.0.2.1:9").ok()?;
	Some(socket.local_addr().ok()?.ip().to_string())
}

fn hostname() -> String {
	std::process::Command::new("hostname")
		.arg("-s")
		.output()
		.ok()
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty())
		.unwrap_or_else(|| "this machine".into())
}
