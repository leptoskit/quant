use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Subcommand;
use quant_store::VaultPaths;

/// What to do with the background service.
#[derive(Subcommand)]
pub enum Action {
	/// Start the daemon at login and keep it running.
	Install {
		/// Also serve phones over TLS on this address, such as `0.0.0.0:7717`.
		#[arg(long)]
		tcp: Option<std::net::SocketAddr>,
	},
	/// Stop it and remove it.
	Uninstall,
	/// Whether it is installed and running.
	Status,
}

const LABEL: &str = "dev.leptoskit.quant";

fn home() -> PathBuf {
	std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn plist_path() -> PathBuf {
	home()
		.join("Library/LaunchAgents")
		.join(format!("{LABEL}.plist"))
}

fn unit_path() -> PathBuf {
	home().join(".config/systemd/user/quant.service")
}

fn escape(s: &str) -> String {
	s.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
}

fn uid() -> String {
	Command::new("id")
		.arg("-u")
		.output()
		.ok()
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map_or_else(|| "501".into(), |s| s.trim().to_owned())
}

/// Run a service action.
pub fn run(paths: &VaultPaths, action: &Action) -> Result<()> {
	let exe = std::env::current_exe()?.canonicalize()?;
	if cfg!(target_os = "macos") {
		macos(paths, action, &exe)
	} else if cfg!(target_os = "linux") {
		linux(paths, action, &exe)
	} else {
		bail!("background services are supported on macOS and Linux")
	}
}

fn macos(paths: &VaultPaths, action: &Action, exe: &std::path::Path) -> Result<()> {
	let plist = plist_path();
	let domain = format!("gui/{}", uid());
	match action {
		Action::Install { tcp } => {
			if !paths.exists() {
				bail!(
					"no vault at {}; run `quant init` first",
					paths.root.display()
				);
			}
			let logs = paths.root.join("logs");
			std::fs::create_dir_all(&logs)?;
			let body = format!(
				r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>--vault</string>
		<string>{vault}</string>
		<string>daemon</string>{tcp}
	</array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
	<key>ProcessType</key><string>Background</string>
	<key>LowPriorityIO</key><true/>
	<key>Nice</key><integer>5</integer>
	<key>StandardOutPath</key><string>{out}</string>
	<key>StandardErrorPath</key><string>{err}</string>
</dict>
</plist>
"#,
				exe = escape(&exe.display().to_string()),
				tcp = tcp
					.map(|a| format!("\n\t\t<string>--tcp</string>\n\t\t<string>{a}</string>"))
					.unwrap_or_default(),
				vault = escape(&paths.root.display().to_string()),
				out = escape(&logs.join("quantd.out.log").display().to_string()),
				err = escape(&logs.join("quantd.err.log").display().to_string()),
			);
			std::fs::create_dir_all(plist.parent().context("plist dir")?)?;
			let _ = Command::new("launchctl")
				.args(["bootout", &format!("{domain}/{LABEL}")])
				.status();
			std::fs::write(&plist, body)?;
			let status = Command::new("launchctl")
				.args(["bootstrap", &domain])
				.arg(&plist)
				.status()?;
			if !status.success() {
				bail!("launchctl bootstrap failed");
			}
			println!("Installed {LABEL}; the daemon starts now and at every login.");
			println!(
				"macOS will ask for Screen Recording permission for {}.",
				exe.display()
			);
		}
		Action::Uninstall => {
			let _ = Command::new("launchctl")
				.args(["bootout", &format!("{domain}/{LABEL}")])
				.status();
			if plist.exists() {
				std::fs::remove_file(&plist)?;
			}
			println!("Removed {LABEL}.");
		}
		Action::Status => {
			let installed = plist.exists();
			let running = Command::new("launchctl")
				.args(["print", &format!("{domain}/{LABEL}")])
				.output()
				.is_ok_and(|o| {
					o.status.success()
						&& String::from_utf8_lossy(&o.stdout).contains("state = running")
				});
			println!(
				"installed: {installed}\nrunning: {running}\nsocket: {}",
				paths.socket().display()
			);
		}
	}
	Ok(())
}

fn linux(paths: &VaultPaths, action: &Action, exe: &std::path::Path) -> Result<()> {
	let unit = unit_path();
	let systemctl = |args: &[&str]| Command::new("systemctl").arg("--user").args(args).status();
	match action {
		Action::Install { tcp } => {
			std::fs::create_dir_all(unit.parent().context("unit dir")?)?;
			std::fs::write(
				&unit,
				format!(
					"[Unit]\nDescription=Quant daemon\n\n[Service]\nExecStart={} --vault {} \
					 daemon{}\nRestart=on-failure\nNice=5\n\n[Install]\nWantedBy=default.target\n",
					exe.display(),
					paths.root.display(),
					tcp.map(|a| format!(" --tcp {a}")).unwrap_or_default(),
				),
			)?;
			systemctl(&["daemon-reload"])?;
			systemctl(&["enable", "--now", "quant.service"])?;
			println!("Installed quant.service.");
		}
		Action::Uninstall => {
			let _ = systemctl(&["disable", "--now", "quant.service"]);
			if unit.exists() {
				std::fs::remove_file(&unit)?;
			}
			println!("Removed quant.service.");
		}
		Action::Status => {
			systemctl(&["status", "quant.service", "--no-pager"])?;
		}
	}
	Ok(())
}
