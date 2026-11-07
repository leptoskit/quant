//! `quant relay`: point the daemon at a relay, or turn it off.

use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::Subcommand;

use crate::Ctx;

/// Options for `quant relay`.
#[derive(Args)]
pub struct RelayArgs {
	#[command(subcommand)]
	action: Action,
}

#[derive(Subcommand)]
enum Action {
	/// Keep a listener on this relay, reachable from anywhere. Asks for the
	/// passphrase.
	Set {
		/// The relay, such as `wss://relay.example.com`.
		url: String,
		/// An invite code, on relays that gate registration.
		#[arg(long)]
		invite: Option<String>,
	},
	/// Stop listening on a relay. Asks for the passphrase.
	Off,
	/// Show the relay in use and this daemon's rendezvous id. Asks for the
	/// passphrase.
	Status,
}

/// Run `quant relay …`.
pub async fn run(cx: &Ctx, args: RelayArgs) -> Result<()> {
	let owner = cx.owner().await?;
	match args.action {
		Action::Set { url, invite } => {
			quant_relay::RelayUrl::parse(&url).map_err(|error| anyhow::anyhow!("{error}"))?;
			let state = owner
				.set_relay(&quant_api::RelaySetRequest {
					url: Some(url),
					invite,
				})
				.await?;
			println!("Relay on: {}", state.url.expect("just set"));
			println!("Rendezvous id: {}", state.rid);
			println!("Reconnect or restart the daemon to open its listener; `quant link` now");
			println!("includes the relay.");
		}
		Action::Off => {
			owner
				.set_relay(&quant_api::RelaySetRequest {
					url: None,
					invite: None,
				})
				.await?;
			println!("Relay off.");
		}
		Action::Status => {
			let state = owner.relay().await?;
			match state.url {
				Some(url) => {
					println!("Relay: {url}");
					println!("Rendezvous id: {}", state.rid);
					println!("Invite held: {}", if state.invite { "yes" } else { "no" });
				}
				None => {
					bail!("no relay is set; use `quant relay set <url>`");
				}
			}
		}
	}
	Ok(())
}
