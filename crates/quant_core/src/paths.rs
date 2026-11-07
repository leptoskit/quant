//! Where a vault lives on this machine, and where its daemon listens.
//!
//! The daemon and every local client agree on these, so a client finds the
//! socket without opening the vault.

use std::path::Path;
use std::path::PathBuf;

/// Unix socket paths must fit in `sun_path` (104 bytes on macOS).
const MAX_SOCKET_PATH: usize = 100;

/// `$QUANT_HOME`, or the platform's per-user data directory.
pub fn default_vault_root() -> PathBuf {
	if let Some(home) = std::env::var_os("QUANT_HOME") {
		return PathBuf::from(home);
	}
	let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
	if cfg!(target_os = "macos") {
		home.join("Library/Application Support/Quant")
	} else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
		PathBuf::from(data).join("quant")
	} else {
		home.join(".local/share/quant")
	}
}

/// The daemon's socket for the vault at `root`: `quant.sock` in the vault,
/// or, when that path is too long for a Unix socket, a file in a private
/// `/tmp/quant-<uid>` directory named after the vault path.
pub fn socket_path(root: &Path) -> PathBuf {
	let beside = root.join("quant.sock");
	if beside.as_os_str().len() < MAX_SOCKET_PATH {
		return beside;
	}
	let name = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
	short_socket_dir().join(format!("{}.sock", &name[..16]))
}

/// Make sure the directory of `root`'s socket exists and only this user can
/// use it. The daemon calls this before binding; clients call it before
/// connecting, so nobody else can stand up a fake daemon to collect the
/// owner's passphrase.
pub fn secure_socket_dir(root: &Path) -> std::io::Result<()> {
	let socket = socket_path(root);
	let dir = socket.parent().expect("sockets live in a directory");
	if dir == root {
		return Ok(());
	}
	check_private_dir(dir)
}

#[cfg(unix)]
fn uid() -> u32 {
	#[allow(unsafe_code)]
	// SAFETY: getuid has no preconditions and cannot fail.
	unsafe {
		libc::getuid()
	}
}

#[cfg(not(unix))]
fn uid() -> u32 {
	0
}

fn short_socket_dir() -> PathBuf {
	PathBuf::from(format!("/tmp/quant-{}", uid()))
}

#[cfg(unix)]
fn check_private_dir(dir: &Path) -> std::io::Result<()> {
	use std::os::unix::fs::DirBuilderExt;
	use std::os::unix::fs::MetadataExt;
	use std::os::unix::fs::PermissionsExt;

	if !dir.exists() {
		std::fs::DirBuilder::new().mode(0o700).create(dir)?;
	}
	let meta = std::fs::symlink_metadata(dir)?;
	if !meta.is_dir() || meta.uid() != uid() || meta.permissions().mode() & 0o077 != 0 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::PermissionDenied,
			format!(
				"{} must be a directory owned by you with mode 0700",
				dir.display()
			),
		));
	}
	Ok(())
}

#[cfg(not(unix))]
fn check_private_dir(dir: &Path) -> std::io::Result<()> {
	std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn short_roots_keep_the_socket_beside_the_vault() {
		let root = Path::new("/Users/me/Library/Application Support/Quant");
		assert_eq!(socket_path(root), root.join("quant.sock"));
		secure_socket_dir(root).unwrap();
	}

	#[test]
	fn long_roots_move_the_socket_to_a_private_directory() {
		let root = PathBuf::from("/").join("deep".repeat(30));
		let socket = socket_path(&root);
		assert!(socket.starts_with(short_socket_dir()));
		assert_eq!(socket, socket_path(&root));
		assert!(socket.as_os_str().len() < MAX_SOCKET_PATH);
	}
}
