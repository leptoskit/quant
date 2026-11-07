//! The Quant relay's entrypoint: migrate, then serve, or manage invites.

use clap::Parser;
use clap::Subcommand;
use leptoskit_axum::Readiness;
use leptoskit_db::Db;
use relay::address;
use relay::limits_from_env;
use relay::serve;
use relay_db::RelayDb;

/// What the relay binary does.
#[derive(Parser)]
#[command(
	name = "relay",
	version,
	about = "The Quant relay: a blind pipe to quant daemon"
)]
struct Cli {
	#[command(subcommand)]
	command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
	/// Serve the relay (the default).
	Serve,
	/// Manage the invite codes that gate registration.
	Invite {
		#[command(subcommand)]
		action: InviteAction,
	},
}

#[derive(Subcommand)]
enum InviteAction {
	/// Add an invite code, or a random one when none is given.
	Add {
		/// The code; a random one is printed when omitted.
		code: Option<String>,
	},
	/// List the invites, and which rid spent each.
	List,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let cli = Cli::parse();
	let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://relay.sqlite3".to_owned());
	let db = Db::connect(&url).await?;
	for name in db.migrate(relay_db::MIGRATIONS).await? {
		println!("applied migration {name}");
	}
	match cli.command.unwrap_or(Command::Serve) {
		Command::Serve => {
			let readiness = Readiness::new();
			let migrated = readiness.check("migrations");
			let store = RelayDb::new(db.clone());
			let listener = tokio::net::TcpListener::bind(address()).await?;
			migrated.pass();
			println!("relay listening on {}", listener.local_addr()?);
			serve(listener, store, limits_from_env(), readiness).await?;
			Ok(())
		}
		Command::Invite { action } => {
			let store = RelayDb::new(db.clone());
			match action {
				InviteAction::Add { code } => invite_add(store, code).await?,
				InviteAction::List => invite_list(store).await?,
			}
			Ok(())
		}
	}
}

/// Create an invite; creating any invite turns registration gating on.
async fn invite_add(
	store: RelayDb,
	code: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
	let code = code.unwrap_or_else(|| {
		let mut bytes = [0u8; 12];
		getrandom::fill(&mut bytes).expect("the OS random source is available");
		data_encoding::BASE32_NOPAD
			.encode(&bytes)
			.to_ascii_lowercase()
	});
	relay_db::NewInvite {
		code: code.clone(),
		used_by: None,
		used_at: None,
	}
	.insert(store.db())
	.await?;
	println!("{code}");
	Ok(())
}

/// Print the invites and who spent them.
async fn invite_list(store: RelayDb) -> Result<(), Box<dyn std::error::Error>> {
	for invite in relay_db::Invite::query()
		.order_by(|invite| invite.created_at.asc())
		.all(store.db())
		.await?
	{
		match invite.used_by {
			Some(rid) => println!("{}  spent by {rid}", invite.code),
			None => println!("{}  unused", invite.code),
		}
	}
	Ok(())
}
