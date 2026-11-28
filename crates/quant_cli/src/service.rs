use std::net::SocketAddr;
use std::path::Path;
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
		tcp: Option<SocketAddr>,
	},
	/// Stop it and remove it.
	Uninstall,
	/// Whether it is installed and running.
	Status,
}

const LABEL: &str = "dev.leptoskit.quant.daemon";
const LEGACY_LABEL: &str = "dev.leptoskit.quant";

fn home() -> PathBuf {
	std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn plist_path_in(home: &Path, label: &str) -> PathBuf {
	home.join("Library/LaunchAgents")
		.join(format!("{label}.plist"))
}

fn plist_path() -> PathBuf {
	plist_path_in(&home(), LABEL)
}

fn legacy_plist_path() -> PathBuf {
	plist_path_in(&home(), LEGACY_LABEL)
}

fn unit_path() -> PathBuf {
	home().join(".config/systemd/user/quant.service")
}

fn escape(s: &str) -> String {
	s.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
}

fn render_plist(exe: &Path, vault_root: &Path, tcp: Option<SocketAddr>) -> String {
	let logs = vault_root.join("logs");
	format!(
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
		vault = escape(&vault_root.display().to_string()),
		out = escape(&logs.join("quantd.out.log").display().to_string()),
		err = escape(&logs.join("quantd.err.log").display().to_string()),
	)
}

/// Whether `contents` is a plist written by the pre-split daemon installer
/// under `LEGACY_LABEL`.
///
/// The menu-bar app's "Open Quant at login" switch owns `LEGACY_LABEL`, so any
/// plist without the `daemon` argument in `ProgramArguments` must be left
/// alone.
fn is_legacy_daemon_plist(contents: &str) -> bool {
	let Some((_, after_args)) = contents.split_once("<key>ProgramArguments</key>") else {
		return false;
	};
	let args_block = after_args
		.split_once("</array>")
		.map_or(after_args, |(before, _)| before);
	contents.contains(&format!("<string>{LEGACY_LABEL}</string>"))
		&& args_block.contains("<string>daemon</string>")
}

fn has_legacy_daemon_plist(path: &Path) -> bool {
	std::fs::read_to_string(path).is_ok_and(|contents| is_legacy_daemon_plist(&contents))
}

fn remove_legacy_daemon(domain: &str, legacy_plist: &Path) -> Result<()> {
	let _ = Command::new("launchctl")
		.args(["bootout", &format!("{domain}/{LEGACY_LABEL}")])
		.status();
	if legacy_plist.exists() {
		std::fs::remove_file(legacy_plist)?;
	}
	println!("Removed legacy daemon entry {LEGACY_LABEL}.");
	Ok(())
}

/// Whether `launchctl` reports the job for `label` under `domain` as running.
fn launchctl_running(domain: &str, label: &str) -> bool {
	Command::new("launchctl")
		.args(["print", &format!("{domain}/{label}")])
		.output()
		.is_ok_and(|o| {
			o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
		})
}

/// The pre-split daemon entry under [`LEGACY_LABEL`]: whether its plist is
/// installed, and whether its job is running.
#[derive(Clone, Copy)]
struct LegacyDaemon {
	installed: bool,
	running: bool,
}

fn format_macos_status(
	installed: bool,
	running: bool,
	socket: &Path,
	legacy: LegacyDaemon,
) -> String {
	let base = format!(
		"label: {LABEL}\ninstalled: {installed}\nrunning: {running}\nsocket: {}",
		socket.display()
	);
	if !legacy.installed {
		return base;
	}
	let base = format!(
		"{base}\nlegacy label: {LEGACY_LABEL}\nlegacy running: {}",
		legacy.running
	);
	if installed {
		format!(
			"{base}\nnote: an old daemon entry is also installed under {LEGACY_LABEL}, so two \
			 daemons may be running on this vault; run `quant service install` to remove the old \
			 one."
		)
	} else {
		format!(
			"{base}\nnote: the daemon is installed under the legacy label {LEGACY_LABEL}; run \
			 `quant service install` to move it to {LABEL}."
		)
	}
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

fn macos(paths: &VaultPaths, action: &Action, exe: &Path) -> Result<()> {
	let plist = plist_path();
	let legacy_plist = legacy_plist_path();
	let domain = format!("gui/{}", uid());
	match action {
		Action::Install { tcp } => {
			if !paths.exists() {
				bail!(
					"no vault at {}; run `quant init` first",
					paths.root.display()
				);
			}
			std::fs::create_dir_all(paths.root.join("logs"))?;
			let body = render_plist(exe, &paths.root, *tcp);
			std::fs::create_dir_all(plist.parent().context("plist dir")?)?;
			if has_legacy_daemon_plist(&legacy_plist) {
				remove_legacy_daemon(&domain, &legacy_plist)?;
			}
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
			if has_legacy_daemon_plist(&legacy_plist) {
				remove_legacy_daemon(&domain, &legacy_plist)?;
			}
			println!("Removed {LABEL}.");
		}
		Action::Status => {
			let installed = plist.exists();
			let legacy_installed = has_legacy_daemon_plist(&legacy_plist);
			let running = launchctl_running(&domain, LABEL);
			let legacy = LegacyDaemon {
				installed: legacy_installed,
				running: legacy_installed && launchctl_running(&domain, LEGACY_LABEL),
			};
			println!(
				"{}",
				format_macos_status(installed, running, &paths.socket(), legacy)
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

#[cfg(test)]
mod tests {
	use std::net::SocketAddr;
	use std::path::Path;

	use super::LABEL;
	use super::LEGACY_LABEL;
	use super::LegacyDaemon;
	use super::format_macos_status;
	use super::has_legacy_daemon_plist;
	use super::is_legacy_daemon_plist;
	use super::plist_path_in;
	use super::render_plist;

	#[test]
	fn labels_and_plist_paths_are_distinct() {
		assert_eq!(LABEL, "dev.leptoskit.quant.daemon");
		assert_eq!(LEGACY_LABEL, "dev.leptoskit.quant");
		assert_ne!(LABEL, LEGACY_LABEL);

		let fake_home = tempfile::tempdir().expect("tempdir");
		let daemon_path = plist_path_in(fake_home.path(), LABEL);
		let legacy_path = plist_path_in(fake_home.path(), LEGACY_LABEL);
		assert!(daemon_path.ends_with("Library/LaunchAgents/dev.leptoskit.quant.daemon.plist"));
		assert!(legacy_path.ends_with("Library/LaunchAgents/dev.leptoskit.quant.plist"));
		assert_ne!(daemon_path, legacy_path);
	}

	#[test]
	fn render_plist_without_tcp() {
		let body = render_plist(
			Path::new("/usr/local/bin/quant"),
			Path::new("/var/tmp/quant-vault"),
			None,
		);
		assert!(body.contains("<key>Label</key><string>dev.leptoskit.quant.daemon</string>"));
		assert!(body.contains("<string>/usr/local/bin/quant</string>"));
		assert!(body.contains("<string>--vault</string>"));
		assert!(body.contains("<string>/var/tmp/quant-vault</string>"));
		assert!(body.contains("<string>daemon</string>"));
		assert!(body.contains(
			"<key>StandardOutPath</key><string>/var/tmp/quant-vault/logs/quantd.out.log</string>"
		));
		assert!(body.contains(
			"<key>StandardErrorPath</key><string>/var/tmp/quant-vault/logs/quantd.err.log</string>"
		));
		assert!(!body.contains("--tcp"));
	}

	#[test]
	fn render_plist_with_tcp() {
		let addr: SocketAddr = "0.0.0.0:7717".parse().expect("socket addr");
		let body = render_plist(
			Path::new("/usr/local/bin/quant"),
			Path::new("/var/tmp/quant-vault"),
			Some(addr),
		);
		assert!(body.contains("<string>--tcp</string>"));
		assert!(body.contains("<string>0.0.0.0:7717</string>"));
	}

	#[test]
	fn render_plist_escapes_xml_special_characters() {
		let body = render_plist(
			Path::new("/opt/a&b/<quant>/quant"),
			Path::new("/var/tmp/me&you/<vault>"),
			None,
		);
		assert!(body.contains("<string>/opt/a&amp;b/&lt;quant&gt;/quant</string>"));
		assert!(body.contains("<string>/var/tmp/me&amp;you/&lt;vault&gt;</string>"));
		assert!(
			body.contains("<string>/var/tmp/me&amp;you/&lt;vault&gt;/logs/quantd.out.log</string>")
		);
		assert!(
			body.contains("<string>/var/tmp/me&amp;you/&lt;vault&gt;/logs/quantd.err.log</string>")
		);
		assert!(!body.contains("a&b"));
		assert!(!body.contains("<quant>"));
		assert!(!body.contains("<vault>"));
	}

	#[test]
	fn legacy_detector_matches_old_daemon_plist_only() {
		let old_daemon = format!(
			r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{LEGACY_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/bin/quant</string>
		<string>--vault</string>
		<string>/Users/example/Library/Application Support/Quant</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key><true/>
</dict>
</plist>
"#
		);
		assert!(is_legacy_daemon_plist(&old_daemon));

		let app_login_item = format!(
			r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LEGACY_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Applications/Quant.app/Contents/MacOS/App</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#
		);
		assert!(!is_legacy_daemon_plist(&app_login_item));

		let new_daemon = render_plist(
			Path::new("/usr/local/bin/quant"),
			Path::new("/var/tmp/quant-vault"),
			None,
		);
		assert!(!is_legacy_daemon_plist(&new_daemon));

		let dir = tempfile::tempdir().expect("tempdir");
		let legacy_file = plist_path_in(dir.path(), LEGACY_LABEL);
		assert!(!has_legacy_daemon_plist(&legacy_file));

		std::fs::create_dir_all(legacy_file.parent().expect("parent")).expect("mkdir");
		std::fs::write(&legacy_file, &old_daemon).expect("write legacy daemon");
		assert!(has_legacy_daemon_plist(&legacy_file));

		std::fs::write(&legacy_file, &app_login_item).expect("write login item");
		assert!(!has_legacy_daemon_plist(&legacy_file));

		std::fs::write(&legacy_file, [0xff, 0xfe, 0x00, 0x01]).expect("write binary");
		assert!(!has_legacy_daemon_plist(&legacy_file));
	}

	#[test]
	fn status_output_names_new_label_and_reports_legacy_entry() {
		let socket = Path::new("/var/tmp/quant.sock");
		let legacy = |running| {
			LegacyDaemon {
				installed: true,
				running,
			}
		};
		let absent = LegacyDaemon {
			installed: false,
			running: false,
		};

		let legacy_only = format_macos_status(false, false, socket, legacy(false));
		assert!(legacy_only.contains(LABEL));
		assert!(legacy_only.contains(LEGACY_LABEL));
		assert!(legacy_only.contains("legacy running: false"));
		assert!(legacy_only.contains("quant service install"));
		assert!(legacy_only.contains("move it"));

		let legacy_running = format_macos_status(false, false, socket, legacy(true));
		assert!(legacy_running.contains("legacy running: true"));
		assert!(legacy_running.contains("move it"));

		let installed = format_macos_status(true, true, socket, absent);
		assert!(installed.contains(LABEL));
		assert!(!installed.contains("quant service install"));
		assert!(!installed.contains("note:"));
		assert!(!installed.contains("legacy running:"));

		let installed_with_legacy = format_macos_status(true, true, socket, legacy(false));
		assert!(installed_with_legacy.contains(LABEL));
		assert!(installed_with_legacy.contains(LEGACY_LABEL));
		assert!(installed_with_legacy.contains("legacy running: false"));
		assert!(installed_with_legacy.contains("two daemons"));
		assert!(installed_with_legacy.contains("quant service install"));
		assert!(installed_with_legacy.contains("remove the old one"));

		let both_running = format_macos_status(true, true, socket, legacy(true));
		assert!(both_running.contains("running: true"));
		assert!(both_running.contains("legacy running: true"));
		assert!(both_running.contains("two daemons"));

		let neither = format_macos_status(false, false, socket, absent);
		assert!(neither.contains(LABEL));
		assert!(!neither.contains("quant service install"));
		assert!(!neither.contains("note:"));
	}
}
