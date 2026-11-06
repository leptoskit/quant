//! `quant`: quantify your life, and decide which AI may read it.

mod daemon;
mod print;
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
use quant_api::ClientKind;
use quant_api::DurationsRequest;
use quant_api::IngestRecords;
use quant_api::OverviewRequest;
use quant_api::PurgeRequest;
use quant_api::RecordsRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_client::when;
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
	/// The vault directory. Defaults to $`QUANT_HOME` or the platform data dir.
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
		#[arg(long, default_value = "-24h")]
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
		#[arg(long)]
		from: String,
		/// End.
		#[arg(long, default_value = "now")]
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
	#[arg(long)]
	from: Option<String>,
	/// End, in the same forms.
	#[arg(long)]
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
}

struct Ctx {
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

	async fn owner(&self) -> Result<OwnerSession> {
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
		} => {
			let (from, to) = window.resolve()?;
			let request = RecordsRequest {
				streams,
				from,
				to,
				search: Some(terms.join(" ")),
				limit: Some(limit),
				oldest_first: false,
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
		Command::Mcp { scopes, setup } => mcp(&cx, &scopes, setup).await,
		Command::Link { host, port } => link(&cx, host, port).await,
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
	let link = format!(
		"quant://link?addr={host}:{port}&fp={fingerprint}&vault={}",
		hello.vault_id
	);
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
