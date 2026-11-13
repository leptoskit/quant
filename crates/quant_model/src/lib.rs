//! What the Quant desktop window and its native service say to each other.
//!
//! The window is a Wasm guest. It never talks to the daemon: it asks the
//! native service, which holds the signed client and the owner session, and
//! renders the [`Snapshot`] the service sends back. Everything here is plain
//! data, already worded and formatted for display, so the guest needs no
//! clock, time zone, or Quant crate.

use serde::Deserialize;
use serde::Serialize;

/// The service's capability name.
pub const SERVICE: &str = "quant-desktop";

/// Wait for a snapshot newer than the version sent (a little-endian `u64`).
/// Answers a JSON [`Snapshot`].
pub const WATCH: u32 = 1;

/// Run a JSON [`Command`]. Answers a JSON [`Reply`].
pub const COMMAND: u32 = 2;

/// The scopes the desktop app pairs with: saving the rolling audio buffer,
/// listing saved moments, and playing them back.
pub const DESKTOP_SCOPES: [&str; 3] = ["audio:write", "audio:read", "audio:raw"];

/// Everything the window shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
	/// Bumped whenever anything below changes.
	pub version: u64,
	/// The status line, as the tray shows it.
	pub headline: String,
	/// How this app stands with the daemon.
	pub link: Link,
	/// The daemon's version, once it answered.
	pub daemon: Option<String>,
	/// Whether the owner paused capture.
	pub paused: bool,
	/// Capture sources.
	pub sources: Vec<SourceRow>,
	/// Saved moments, newest first.
	pub moments: Vec<Moment>,
	/// The owner session, when unlocked.
	pub owner: Option<OwnerView>,
	/// Why the window asks for the passphrase, such as "Unlock to pause
	/// capture".
	pub unlock_reason: Option<String>,
	/// The result of the last action, such as "Saved the last 15 minutes".
	pub notice: Option<Notice>,
	/// Whether Quant opens at login, when the system can say.
	pub opens_at_login: Option<bool>,
	/// Touch ID on this Mac.
	pub touch_id: TouchId,
}

/// Touch ID on this Mac: a hardware key in the Secure Enclave that signs the
/// unlock challenge after a fingerprint check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchId {
	/// This Mac's enrolled owner device id, when Touch ID is set up.
	pub device_id: Option<String>,
}

/// How this app stands with the daemon.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Link {
	/// Not asked yet.
	#[default]
	Starting,
	/// The daemon did not answer.
	Unreachable {
		/// Why.
		reason: String,
	},
	/// Paired, waiting for the owner.
	Pending {
		/// The pairing code the owner approves.
		code: String,
	},
	/// The owner refused or revoked this app.
	Revoked,
	/// Approved.
	Ready,
}

/// One capture source.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRow {
	/// Source name, such as `screen`.
	pub name: String,
	/// Whether it is running.
	pub running: bool,
	/// What it is doing or why it stopped.
	pub detail: Option<String>,
	/// The System Settings pane that would let it run.
	pub settings: Option<Settings>,
}

/// A privacy pane in System Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settings {
	/// Privacy & Security → Screen Recording.
	ScreenRecording,
	/// Privacy & Security → Microphone.
	Microphone,
}

impl Settings {
	/// The URL that opens the pane.
	pub fn url(self) -> &'static str {
		match self {
			Self::ScreenRecording => {
				"x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
			}
			Self::Microphone => {
				"x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
			}
		}
	}

	/// The permission's name.
	pub fn name(self) -> &'static str {
		match self {
			Self::ScreenRecording => "Screen Recording",
			Self::Microphone => "Microphone",
		}
	}
}

/// A saved moment: a clip kept from the rolling audio buffer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Moment {
	/// The clip's record id.
	pub id: String,
	/// Its title.
	pub title: String,
	/// When it starts, such as "Today 14:03".
	pub when: String,
	/// How long it is, such as "15 min".
	pub duration: String,
	/// The start of its transcript.
	pub snippet: Option<String>,
	/// Whether its audio can be played.
	pub playable: bool,
}

/// What an unlocked owner session shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OwnerView {
	/// When the session lapses, such as "15:42".
	pub until: String,
	/// Clients waiting for approval.
	pub pending: Vec<PendingClient>,
	/// Live grants.
	pub grants: Vec<GrantRow>,
	/// The newest audit entries, newest first.
	pub audit: Vec<AuditRow>,
	/// Whether the audit log's hash chain verifies.
	pub chain_ok: Option<bool>,
	/// Enrolled owner devices (this Mac's Touch ID, phones, …).
	pub devices: Vec<OwnerDeviceRow>,
	/// Whether the session was opened with the passphrase, so it may enrol a
	/// new device.
	pub passphrase_session: bool,
}

/// An enrolled owner device: a hardware key that unlocks owner sessions after
/// a biometric check.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OwnerDeviceRow {
	/// Its id.
	pub id: String,
	/// Its label, such as "`MacBook` Touch ID".
	pub label: String,
	/// A short hash of its public key, to tell devices apart at a glance.
	pub fingerprint: String,
	/// When it last opened a session, such as "Today 14:03".
	pub last_used: Option<String>,
}

/// A client waiting for the owner.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PendingClient {
	/// Its client id.
	pub id: String,
	/// Its name.
	pub name: String,
	/// What sort: agent, device, cli, app.
	pub kind: String,
	/// Its pairing code.
	pub code: String,
	/// The scopes it asked for.
	pub requested: Vec<String>,
	/// When it asked, such as "Today 14:03".
	pub asked: String,
}

/// A live grant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GrantRow {
	/// The grant id.
	pub id: String,
	/// Its label.
	pub label: String,
	/// Its scopes.
	pub scopes: Vec<String>,
	/// How far back it sees, such as "30 days".
	pub lookback: String,
	/// When it expires, if it does.
	pub expires: Option<String>,
}

/// One audit entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuditRow {
	/// Position in the chain.
	pub seq: u64,
	/// When, such as "14:03:12".
	pub at: String,
	/// Who: "owner", a grant label, "system".
	pub actor: String,
	/// What, such as `query` or `grant.create`.
	pub action: String,
	/// On what, such as a stream id.
	pub target: String,
	/// "allowed", "denied: out of scope", "failed".
	pub outcome: String,
}

/// The result of an action.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Notice {
	/// What happened.
	pub text: String,
	/// Whether it went wrong.
	pub error: bool,
}

/// Something the window asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
	/// Look again now.
	Refresh,
	/// Open an owner session. The service keeps only the session token, in
	/// memory, and never stores the passphrase.
	Unlock {
		/// The vault passphrase.
		passphrase: String,
	},
	/// End the owner session.
	Lock,
	/// Pause capture, for some minutes or until resumed.
	Pause {
		/// Minutes, or `None` for until resumed.
		minutes: Option<u32>,
	},
	/// Resume capture.
	Resume,
	/// Keep the last minutes of the rolling audio buffer.
	Save {
		/// How many minutes.
		minutes: u32,
	},
	/// Approve a pending client.
	Approve {
		/// Its client id.
		client: String,
		/// The scopes granted.
		scopes: Vec<String>,
		/// How far back it may see, in hours; `None` for no limit.
		lookback_hours: Option<u32>,
	},
	/// Refuse a pending client.
	Deny {
		/// Its client id.
		client: String,
	},
	/// Revoke a grant.
	Revoke {
		/// The grant id.
		grant: String,
	},
	/// Play a saved moment.
	Play {
		/// Its record id.
		id: String,
	},
	/// Rename a saved moment.
	Rename {
		/// Its record id.
		id: String,
		/// The new title.
		title: String,
	},
	/// Open a System Settings privacy pane.
	OpenSettings {
		/// Which.
		pane: Settings,
	},
	/// Open Quant at login, or stop.
	OpenAtLogin {
		/// Whether to.
		enabled: bool,
	},
	/// Fetch a challenge for this Mac's Touch ID key to sign. Answers a
	/// [`Reply`] whose data carries the challenge and the message.
	TouchIdChallenge,
	/// Open an owner session with an enrolled device's signature.
	UnlockWithDevice {
		/// The enrolled device id.
		device: String,
		/// The challenge the signature answers, base64, as the daemon gave it.
		challenge: String,
		/// The ECDSA P-256/SHA-256 signature over the unlock message, DER.
		signature: Vec<u8>,
	},
	/// Enrol this Mac's Touch ID key as an owner device. The key is created in
	/// the Secure Enclave and signed through the biometric prompt; only its
	/// public half leaves the machine.
	EnrolTouchId {
		/// Its P-256 public key: an uncompressed SEC1 point (65 bytes).
		public_key: Vec<u8>,
	},
	/// Turn off Touch ID on this Mac: revoke the device on the daemon and
	/// forget it here.
	TouchIdOff,
	/// Revoke an enrolled owner device.
	RevokeDevice {
		/// Its id.
		device: String,
	},
}

/// Extra data a [`Command`]'s [`Reply`] can carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyData {
	/// A Touch ID unlock challenge: the bytes the window's key signs, and the
	/// challenge to send back with the signature.
	TouchIdChallenge {
		/// The daemon's challenge, base64, to send back unchanged.
		challenge: String,
		/// `quant_api::owner_unlock_message(vault id, challenge)`: the exact
		/// bytes to sign.
		message: Vec<u8>,
	},
}

/// The answer to a [`Command`]. Details arrive in the next [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
	/// Whether it worked.
	pub ok: bool,
	/// What went wrong.
	pub error: Option<String>,
	/// Extra data for commands that answer more than success.
	pub data: Option<ReplyData>,
}

/// How far back an approved client may see.
pub const LOOKBACKS: [(&str, Option<u32>); 4] = [
	("1 day", Some(24)),
	("1 week", Some(24 * 7)),
	("30 days", Some(24 * 30)),
	("No limit", None),
];

/// The lookback offered first: the narrowest one.
pub const DEFAULT_LOOKBACK: usize = 0;

/// Which of a pending client's requested scopes the owner ticked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeChoice {
	choices: Vec<(String, bool)>,
}

impl ScopeChoice {
	/// Every requested scope, ticked. Duplicates and blanks are dropped.
	pub fn new(requested: &[String]) -> Self {
		let mut choices: Vec<(String, bool)> = Vec::new();
		for scope in requested {
			let scope = scope.trim();
			if !scope.is_empty() && !choices.iter().any(|(s, _)| s == scope) {
				choices.push((scope.to_owned(), true));
			}
		}
		Self { choices }
	}

	/// Tick or untick a scope. Scopes it was not asked for are ignored: the
	/// window never widens a request.
	pub fn set(&mut self, scope: &str, granted: bool) {
		if let Some(choice) = self.choices.iter_mut().find(|(s, _)| s == scope) {
			choice.1 = granted;
		}
	}

	/// The scopes and whether each is ticked, in the order asked.
	pub fn choices(&self) -> &[(String, bool)] {
		&self.choices
	}

	/// The ticked scopes, or `None` when nothing is ticked and approving
	/// would grant nothing.
	pub fn granted(&self) -> Option<Vec<String>> {
		let granted: Vec<String> = self
			.choices
			.iter()
			.filter(|(_, granted)| *granted)
			.map(|(scope, _)| scope.clone())
			.collect();
		(!granted.is_empty()).then_some(granted)
	}
}

/// The [`Command::Approve`] for a pending client, with the ticked scopes and
/// the lookback at `lookback` in [`LOOKBACKS`].
pub fn approve(client: &PendingClient, choice: &ScopeChoice, lookback: usize) -> Option<Command> {
	let (_, hours) = LOOKBACKS.get(lookback)?;
	Some(Command::Approve {
		client: client.id.clone(),
		scopes: choice.granted()?,
		lookback_hours: *hours,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pending(requested: &[&str]) -> PendingClient {
		PendingClient {
			id: "c1".into(),
			name: "Claude Code".into(),
			kind: "agent".into(),
			code: "K7QD-M2XA".into(),
			requested: requested.iter().map(|s| (*s).to_owned()).collect(),
			asked: "Today 14:03".into(),
		}
	}

	#[test]
	fn every_requested_scope_starts_ticked_once() {
		let client = pending(&["health:summary", " apps:summary ", "health:summary", ""]);
		let choice = ScopeChoice::new(&client.requested);
		assert_eq!(
			choice.granted().unwrap(),
			vec!["health:summary".to_owned(), "apps:summary".to_owned()]
		);
	}

	#[test]
	fn unticking_narrows_and_unknown_scopes_never_widen() {
		let client = pending(&["health:summary", "screen:read"]);
		let mut choice = ScopeChoice::new(&client.requested);
		choice.set("screen:read", false);
		choice.set("audio:raw", true);
		assert_eq!(choice.granted().unwrap(), vec!["health:summary".to_owned()]);
		assert_eq!(
			approve(&client, &choice, 2),
			Some(Command::Approve {
				client: "c1".into(),
				scopes: vec!["health:summary".into()],
				lookback_hours: Some(720),
			})
		);
	}

	#[test]
	fn nothing_ticked_approves_nothing() {
		let client = pending(&["health:summary"]);
		let mut choice = ScopeChoice::new(&client.requested);
		choice.set("health:summary", false);
		assert_eq!(choice.granted(), None);
		assert_eq!(approve(&client, &choice, DEFAULT_LOOKBACK), None);
		assert_eq!(approve(&pending(&[]), &ScopeChoice::new(&[]), 0), None);
	}

	#[test]
	fn the_last_lookback_has_no_limit() {
		let client = pending(&["health:summary"]);
		let choice = ScopeChoice::new(&client.requested);
		let Some(Command::Approve { lookback_hours, .. }) =
			approve(&client, &choice, LOOKBACKS.len() - 1)
		else {
			panic!("approves");
		};
		assert_eq!(lookback_hours, None);
		assert_eq!(approve(&client, &choice, LOOKBACKS.len()), None);
	}

	#[test]
	fn commands_round_trip_as_tagged_json() {
		let command = Command::Pause { minutes: Some(30) };
		let json = serde_json::to_string(&command).unwrap();
		assert_eq!(json, r#"{"command":"pause","minutes":30}"#);
		assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);
	}

	#[test]
	fn settings_open_the_right_privacy_panes() {
		assert!(
			Settings::ScreenRecording
				.url()
				.ends_with("?Privacy_ScreenCapture")
		);
		assert!(Settings::Microphone.url().ends_with("?Privacy_Microphone"));
	}

	#[test]
	fn reply_data_round_trips_as_tagged_json() {
		let data = ReplyData::TouchIdChallenge {
			challenge: "Q0hBTExFTkdF".into(),
			message: vec![1, 2, 3],
		};
		let json = serde_json::to_string(&data).unwrap();
		assert_eq!(
			json,
			r#"{"kind":"touch_id_challenge","challenge":"Q0hBTExFTkdF","message":[1,2,3]}"#
		);
		assert_eq!(serde_json::from_str::<ReplyData>(&json).unwrap(), data);
	}
}
