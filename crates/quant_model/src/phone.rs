//! What the phone's screen and its native service say to each other.
//!
//! On a phone the screen is the only code that can reach the Leptoskit
//! plugins (location, app usage, battery, thermal), and the native service is
//! the only code that can open the encrypted spool and sign uploads. So the
//! screen asks for permissions, starts the plugins, and hands every reply to
//! the service unread, as a [`Reading`]; the service decodes it, queues it, and
//! delivers it to the vault. The screen never sees a signing key, and nothing
//! it hands over is trusted until the service has decoded it.

use serde::Deserialize;
use serde::Serialize;

use crate::Notice;

/// The service's capability name.
pub const SERVICE: &str = "quant-phone";

/// Wait for a snapshot newer than the version sent (a little-endian `u64`).
/// Answers a JSON [`Snapshot`].
pub const WATCH: u32 = 1;

/// Run a JSON [`Command`]. Answers a JSON [`Reply`].
pub const COMMAND: u32 = 2;

/// Queue an encoded [`Reading`]. Answers a JSON [`Reply`] as soon as the
/// frame is accepted; a reading the spool refuses shows on its source in the
/// next [`Snapshot`].
pub const READING: u32 = 3;

/// The write scopes the phone pairs with: where it has been, which apps were
/// in front, and its screen, battery and thermal state. None of them lets the
/// phone read anything back.
pub const PHONE_SCOPES: [&str; 3] = ["location:write", "apps:write", "device:write"];

/// How often the screen asks the service to look again and upload.
pub const TICK_SECONDS: u64 = 60;

/// How often the screen reads app usage.
pub const USAGE_SECONDS: u64 = 5 * 60;

/// Everything the phone's screen shows that the service knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
	/// Bumped whenever anything below changes.
	pub version: u64,
	/// The status line, such as "Collecting · 3 waiting".
	pub headline: String,
	/// How this phone stands with the vault.
	pub link: Link,
	/// The vault's address, once a pairing link was accepted.
	pub vault: Option<String>,
	/// The command that approves this phone, while it waits.
	pub approve: Option<String>,
	/// The encrypted spool.
	pub storage: Storage,
	/// Whether the owner wants this phone to collect.
	pub collecting: bool,
	/// Readings waiting to be delivered.
	pub waiting: u64,
	/// When the vault last took a delivery, such as "Today 14:02".
	pub delivered: Option<String>,
	/// Why the last delivery stopped, when it did.
	pub upload_problem: Option<String>,
	/// What each source has handed over since the app started.
	pub sources: Vec<SourceRow>,
	/// The result of the last action.
	pub notice: Option<Notice>,
}

/// How this phone stands with the vault.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Link {
	/// No pairing link yet.
	#[default]
	Unpaired,
	/// A link was accepted and the vault has not answered yet.
	Connecting,
	/// The vault did not answer. Readings keep queueing.
	Unreachable {
		/// Why.
		reason: String,
	},
	/// Paired, waiting for the owner.
	Pending {
		/// The pairing code the owner approves.
		code: String,
	},
	/// The owner refused or revoked this phone, or its grant ran out.
	Revoked,
	/// Approved.
	Ready,
}

/// The encrypted spool on this phone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Storage {
	/// Waiting for its key from the phone's keystore.
	#[default]
	Locked,
	/// Open.
	Open,
	/// It could not be opened.
	Failed {
		/// Why.
		reason: String,
	},
}

/// What the phone collects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
	/// Where the phone is: `location.fix`.
	Location,
	/// Which apps were in front, and screen and lock changes: `apps.focus`
	/// and `device.state`.
	Usage,
	/// Battery level, charging, low-power mode and thermal state:
	/// `device.battery` and `device.power`.
	Power,
}

impl Source {
	/// Every source, in the order the screen lists them.
	pub const ALL: [Source; 3] = [Source::Location, Source::Usage, Source::Power];

	/// The source's name on the screen.
	pub fn label(self) -> &'static str {
		match self {
			Source::Location => "Location",
			Source::Usage => "App usage",
			Source::Power => "Battery and temperature",
		}
	}
}

/// What one source has handed over since the app started.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRow {
	/// Which source.
	pub source: Source,
	/// Readings queued since the app started.
	pub readings: u64,
	/// When the last one arrived, such as "14:02".
	pub last: Option<String>,
	/// Why the last reading was refused, or what the phone lost.
	pub problem: Option<String>,
}

/// Something the screen asks the service to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
	/// Look at the vault again and deliver what is waiting.
	Tick,
	/// Deliver what is waiting now, without the usual pause between tries.
	Upload,
	/// Pair with the vault a `quant://link?…` link names.
	Pair {
		/// The link, as `quant link` prints it.
		link: String,
	},
	/// Forget the vault and this phone's identity. Readings stay queued.
	Forget,
	/// Make a new key for the encrypted spool. Answers
	/// [`ReplyData::StorageKey`]; the screen puts it in the phone's keystore
	/// and sends it back with [`Command::OpenStorage`].
	NewStorageKey,
	/// Open the encrypted spool with the key from the phone's keystore.
	OpenStorage {
		/// The key, as [`ReplyData::StorageKey`] gave it.
		key: String,
		/// Whether the key was made just now because the keystore had none.
		/// A spool left from an earlier key can never be read again, so the
		/// service clears it and starts an empty one.
		fresh: bool,
	},
	/// Turn collecting on or off.
	Collect {
		/// Whether to collect.
		enabled: bool,
	},
	/// Ask which stretch of app usage to read next. Answers
	/// [`ReplyData::UsageRange`], or no data when there is nothing to read
	/// yet.
	UsagePlan,
}

/// Extra data a [`Command`]'s [`Reply`] can carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyData {
	/// A fresh spool key, base64.
	StorageKey {
		/// The key.
		key: String,
	},
	/// The stretch of app usage to read next, in Unix milliseconds. The
	/// screen has no clock of its own.
	UsageRange {
		/// From, inclusive.
		since_ms: i64,
		/// To, exclusive.
		until_ms: i64,
	},
}

/// The answer to a [`Command`] or a reading. Details arrive in the next
/// [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
	/// Whether it worked.
	pub ok: bool,
	/// What went wrong.
	pub error: Option<String>,
	/// Extra data for commands that answer more than success.
	pub data: Option<ReplyData>,
}

impl Reply {
	/// Success with nothing more to say.
	pub fn ok() -> Self {
		Self {
			ok: true,
			error: None,
			data: None,
		}
	}

	/// Success carrying `data`.
	pub fn data(data: ReplyData) -> Self {
		Self {
			ok: true,
			error: None,
			data: Some(data),
		}
	}

	/// A failure.
	pub fn failed(error: impl Into<String>) -> Self {
		Self {
			ok: false,
			error: Some(error.into()),
			data: None,
		}
	}
}

/// The most app usage event replies one [`Reading::Usage`] carries.
pub const MAX_EVENT_PAGES: usize = 8;

const KIND_LOCATION: u8 = 1;
const KIND_USAGE: u8 = 2;
const KIND_POWER: u8 = 3;

/// One plugin reply, handed to the service exactly as the plugin answered.
///
/// The frame is one kind byte, then length-prefixed plugin replies
/// (little-endian `u32` lengths). The service decodes each reply with the
/// plugin's own decoder, so a frame that does not parse is refused whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading<'a> {
	/// A location `next_updates` reply.
	Location(&'a [u8]),
	/// App usage for a range that ends at `until_ms`: one `sessions` reply
	/// and the `events` replies that cover the same range.
	Usage {
		/// The end of the range read, which becomes the next range's start
		/// once everything in it is queued.
		until_ms: i64,
		/// The `sessions` reply.
		sessions: &'a [u8],
		/// The `events` replies, oldest first.
		events: Vec<&'a [u8]>,
	},
	/// The battery and thermal snapshots as they stand, each missing when
	/// its plugin is unavailable.
	Power {
		/// A battery `snapshot` or `wait_for_change` reply.
		battery: Option<&'a [u8]>,
		/// A thermal `snapshot` or `wait_for_change` reply.
		thermal: Option<&'a [u8]>,
	},
}

impl<'a> Reading<'a> {
	/// The source a reading belongs to.
	pub fn source(&self) -> Source {
		match self {
			Reading::Location(_) => Source::Location,
			Reading::Usage { .. } => Source::Usage,
			Reading::Power { .. } => Source::Power,
		}
	}

	/// The frame for this reading, or `None` when a part is too large to
	/// frame or there are more than [`MAX_EVENT_PAGES`] event replies.
	pub fn encode(&self) -> Option<Vec<u8>> {
		let mut frame = Vec::new();
		match self {
			Reading::Location(updates) => {
				frame.push(KIND_LOCATION);
				part(&mut frame, updates)?;
			}
			Reading::Usage {
				until_ms,
				sessions,
				events,
			} => {
				if events.len() > MAX_EVENT_PAGES {
					return None;
				}
				frame.push(KIND_USAGE);
				frame.extend_from_slice(&until_ms.to_le_bytes());
				part(&mut frame, sessions)?;
				frame.push(u8::try_from(events.len()).ok()?);
				for page in events {
					part(&mut frame, page)?;
				}
			}
			Reading::Power { battery, thermal } => {
				frame.push(KIND_POWER);
				optional(&mut frame, *battery)?;
				optional(&mut frame, *thermal)?;
			}
		}
		Some(frame)
	}

	/// Read a frame back. `None` for anything that is not exactly one
	/// reading: an unknown kind, a short part, or trailing bytes.
	pub fn decode(frame: &'a [u8]) -> Option<Self> {
		let (kind, mut rest) = frame.split_first()?;
		let reading = match *kind {
			KIND_LOCATION => Reading::Location(take_part(&mut rest)?),
			KIND_USAGE => {
				let (until, tail) = rest.split_first_chunk::<8>()?;
				rest = tail;
				let sessions = take_part(&mut rest)?;
				let (pages, tail) = rest.split_first()?;
				rest = tail;
				if usize::from(*pages) > MAX_EVENT_PAGES {
					return None;
				}
				let mut events = Vec::with_capacity(usize::from(*pages));
				for _ in 0..*pages {
					events.push(take_part(&mut rest)?);
				}
				Reading::Usage {
					until_ms: i64::from_le_bytes(*until),
					sessions,
					events,
				}
			}
			KIND_POWER => {
				Reading::Power {
					battery: take_optional(&mut rest).ok()?,
					thermal: take_optional(&mut rest).ok()?,
				}
			}
			_ => return None,
		};
		rest.is_empty().then_some(reading)
	}
}

/// The length that marks a missing optional part.
const ABSENT: u32 = u32::MAX;

fn part(frame: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
	let length = u32::try_from(bytes.len())
		.ok()
		.filter(|len| *len != ABSENT)?;
	frame.extend_from_slice(&length.to_le_bytes());
	frame.extend_from_slice(bytes);
	Some(())
}

fn optional(frame: &mut Vec<u8>, bytes: Option<&[u8]>) -> Option<()> {
	if let Some(bytes) = bytes {
		return part(frame, bytes);
	}
	frame.extend_from_slice(&ABSENT.to_le_bytes());
	Some(())
}

fn take_length(rest: &mut &[u8]) -> Option<u32> {
	let (length, tail) = rest.split_first_chunk::<4>()?;
	*rest = tail;
	Some(u32::from_le_bytes(*length))
}

fn take_bytes<'a>(rest: &mut &'a [u8], length: u32) -> Option<&'a [u8]> {
	let (bytes, tail) = rest.split_at_checked(usize::try_from(length).ok()?)?;
	*rest = tail;
	Some(bytes)
}

fn take_part<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
	match take_length(rest)? {
		ABSENT => None,
		length => take_bytes(rest, length),
	}
}

/// An optional part: `Err` for a frame cut short, `Ok(None)` for a part
/// that was left out.
fn take_optional<'a>(rest: &mut &'a [u8]) -> Result<Option<&'a [u8]>, ()> {
	match take_length(rest).ok_or(())? {
		ABSENT => Ok(None),
		length => take_bytes(rest, length).map(Some).ok_or(()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn round_trip(reading: &Reading<'_>) {
		let frame = reading.encode().unwrap();
		assert_eq!(Reading::decode(&frame).as_ref(), Some(reading));
	}

	#[test]
	fn readings_round_trip() {
		round_trip(&Reading::Location(&[1, 2, 3]));
		round_trip(&Reading::Location(&[]));
		round_trip(&Reading::Usage {
			until_ms: 1_700_000_000_000,
			sessions: &[0, 0, 0, 0],
			events: vec![&[9, 9], &[]],
		});
		round_trip(&Reading::Usage {
			until_ms: -1,
			sessions: &[],
			events: Vec::new(),
		});
		round_trip(&Reading::Power {
			battery: Some(&[1; 16]),
			thermal: None,
		});
		round_trip(&Reading::Power {
			battery: None,
			thermal: Some(&[]),
		});
	}

	#[test]
	fn a_missing_part_is_not_an_empty_one() {
		let missing = Reading::Power {
			battery: None,
			thermal: None,
		};
		let empty = Reading::Power {
			battery: Some(&[]),
			thermal: Some(&[]),
		};
		assert_ne!(missing.encode(), empty.encode());
		round_trip(&missing);
		round_trip(&empty);
	}

	#[test]
	fn frames_that_are_not_exactly_one_reading_are_refused() {
		assert_eq!(Reading::decode(&[]), None);
		assert_eq!(Reading::decode(&[0]), None);
		assert_eq!(Reading::decode(&[9, 0, 0, 0, 0]), None);

		let frame = Reading::Location(&[1, 2, 3]).encode().unwrap();
		for length in 0..frame.len() {
			assert_eq!(Reading::decode(&frame[..length]), None, "cut at {length}");
		}
		let mut trailing = frame.clone();
		trailing.push(0);
		assert_eq!(Reading::decode(&trailing), None);

		// A required part cannot be marked missing.
		let mut absent = vec![1];
		absent.extend_from_slice(&u32::MAX.to_le_bytes());
		assert_eq!(Reading::decode(&absent), None);
	}

	#[test]
	fn a_usage_reading_carries_a_bounded_number_of_event_replies() {
		let page: &[u8] = &[0, 0, 0, 0];
		let full = Reading::Usage {
			until_ms: 5,
			sessions: page,
			events: vec![page; MAX_EVENT_PAGES],
		};
		round_trip(&full);
		let over = Reading::Usage {
			until_ms: 5,
			sessions: page,
			events: vec![page; MAX_EVENT_PAGES + 1],
		};
		assert_eq!(over.encode(), None);

		let mut frame = full.encode().unwrap();
		// The page count sits after the kind, the range end and the sessions.
		let count = 1 + 8 + 4 + page.len();
		frame[count] = u8::try_from(MAX_EVENT_PAGES + 1).unwrap();
		assert_eq!(Reading::decode(&frame), None);
	}

	#[test]
	fn commands_and_replies_round_trip_as_tagged_json() {
		let command = Command::Pair {
			link: "quant://link?addr=192.168.1.4:7717&fp=ab".into(),
		};
		let json = serde_json::to_string(&command).unwrap();
		assert_eq!(
			json,
			r#"{"command":"pair","link":"quant://link?addr=192.168.1.4:7717&fp=ab"}"#
		);
		assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);

		let reply = Reply::data(ReplyData::UsageRange {
			since_ms: 1,
			until_ms: 2,
		});
		let json = serde_json::to_string(&reply).unwrap();
		assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);
		assert!(!Reply::failed("no").ok);
		assert!(Reply::ok().ok);
	}

	#[test]
	fn the_phone_asks_only_for_write_scopes() {
		assert!(PHONE_SCOPES.iter().all(|scope| scope.ends_with(":write")));
	}
}
