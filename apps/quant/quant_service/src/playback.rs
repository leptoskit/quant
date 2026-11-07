//! Playing a saved moment: its audio is written, readable by the owner only,
//! to the app's cache for as long as it plays, then removed.

use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;

const PREFIX: &str = "moment-";

/// Write a clip's audio to `cache` for playing.
pub fn write(cache: &Path, id: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
	let name: String = id
		.chars()
		.filter(|c| c.is_ascii_alphanumeric() || *c == '-')
		.collect();
	std::fs::create_dir_all(cache)?;
	let path = cache.join(format!("{PREFIX}{name}.m4a"));
	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	std::io::Write::write_all(&mut options.open(&path)?, bytes)?;
	Ok(path)
}

/// Play a clip with `afplay`, the system's command-line player.
pub fn play(path: &Path) -> std::io::Result<Child> {
	Command::new("/usr/bin/afplay")
		.arg(path)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
}

/// Remove clips left by an earlier run.
pub fn clear(cache: &Path) {
	let Ok(entries) = std::fs::read_dir(cache) else {
		return;
	};
	for entry in entries.flatten() {
		let name = entry.file_name();
		if name.to_string_lossy().starts_with(PREFIX) {
			let _ = std::fs::remove_file(entry.path());
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clips_are_private_and_cleared() {
		let dir = std::env::temp_dir().join(format!("quant-playback-{}", std::process::id()));
		let path = write(&dir, "0193/../x", b"m4a").unwrap();
		assert_eq!(path.parent(), Some(dir.as_path()));
		assert_eq!(path.file_name().unwrap(), "moment-0193x.m4a");
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mode = std::fs::metadata(&path).unwrap().permissions().mode();
			assert_eq!(mode & 0o777, 0o600);
		}
		clear(&dir);
		assert!(!path.exists());
		std::fs::remove_dir(&dir).unwrap();
	}
}
