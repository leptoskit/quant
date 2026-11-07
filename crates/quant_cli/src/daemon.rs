use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use quant_daemon::State;
use quant_store::Vault;
use quant_store::VaultPaths;

/// Options for `quant daemon`.
#[derive(Args)]
pub struct DaemonArgs {
	/// Also listen on the network over TLS, for phones and sensor bridges,
	/// such as `0.0.0.0:7717`. Run `quant link` to pair a phone.
	#[arg(long)]
	tcp: Option<SocketAddr>,
	/// Serve the vault without capturing this machine's screen, apps,
	/// input, or microphone.
	#[arg(long)]
	no_capture: bool,
	/// Do not keep a listener on the relay, even though one is set with
	/// `quant relay set`.
	#[arg(long)]
	no_relay: bool,
}

fn open(paths: &VaultPaths) -> Result<Vault> {
	if !paths.exists() {
		bail!(
			"no vault at {}; run `quant init` first",
			paths.root.display()
		);
	}
	let manifest = Vault::read_manifest(paths)?;
	if let Some(key) = quant_crypto::keychain::load(&manifest.vault_id).unwrap_or_else(|error| {
		tracing::warn!(%error, "could not read the keychain");
		None
	}) {
		return Ok(Vault::open(paths.clone(), key)?);
	}
	if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
		bail!(
			"the vault key is not in the keychain and there is no terminal to ask for the \
			 passphrase"
		);
	}
	let passphrase = rpassword::prompt_password("Vault passphrase: ")?;
	Ok(Vault::unlock(paths.clone(), &passphrase)?)
}

async fn shutdown() {
	let ctrl_c = tokio::signal::ctrl_c();
	#[cfg(unix)]
	{
		let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
			.expect("signal handlers install");
		tokio::select! {
			_ = ctrl_c => {}
			_ = term.recv() => {}
		}
	}
	#[cfg(not(unix))]
	let _ = ctrl_c.await;
}

/// Run the daemon until interrupted.
pub async fn run(paths: &VaultPaths, args: DaemonArgs) -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
		)
		.with_writer(std::io::stderr)
		.init();
	let vault = open(paths)?;
	let state = Arc::new(State::new(vault)?);
	let maintenance = quant_daemon::spawn_maintenance(Arc::clone(&state));
	let derivation = quant_daemon::spawn_derivation(Arc::clone(&state));
	if !args.no_capture {
		quant_daemon::capture::start(&state)?;
	}

	let (stop, _) = tokio::sync::broadcast::channel::<()>(1);
	let mut tasks = Vec::new();
	if !args.no_relay {
		// Starts on its own only when the vault names a relay; returns at
		// once otherwise.
		let relay = match quant_daemon::relay::config(&state.vault.store) {
			Ok(Some(config)) => Some(config),
			Ok(None) => None,
			Err(error) => {
				tracing::warn!(%error, "could not read the relay settings");
				None
			}
		};
		if let Some(config) = relay {
			let (serving, mut stopped) = (Arc::clone(&state), stop.subscribe());
			tasks.push(tokio::spawn(async move {
				let url = config.url;
				quant_daemon::relay::listen(serving, async move {
					let _ = stopped.recv().await;
				})
				.await;
				tracing::info!(%url, "relay listener stopped");
				Ok::<(), std::io::Error>(())
			}));
		}
	}
	if let Some(address) = args.tcp {
		let (serving, mut stopped) = (Arc::clone(&state), stop.subscribe());
		tasks.push(tokio::spawn(async move {
			let identity = quant_daemon::tls::TlsIdentity::load_or_create(&serving.vault.store)
				.map_err(std::io::Error::other)?;
			quant_daemon::serve_tls(serving, address, &identity, async move {
				let _ = stopped.recv().await;
			})
			.await
		}));
	}
	paths.secure_socket_dir()?;
	let socket = paths.socket();
	let (serving, mut stopped) = (Arc::clone(&state), stop.subscribe());
	tasks.push(tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &socket, async move {
			let _ = stopped.recv().await;
		})
		.await
	}));

	shutdown().await;
	tracing::info!("stopping");
	let _ = stop.send(());
	maintenance.abort();
	derivation.abort();
	for task in tasks {
		task.await.context("server task")??;
	}
	let stopping = Arc::clone(&state);
	tokio::task::spawn_blocking(move || quant_daemon::capture::stop(&stopping)).await?;
	Ok(())
}
