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
/// in front, its screen, battery and thermal state, plus health and motion
/// readings from `HealthKit` / Health Connect and paired BLE sensors. None of
/// them lets the phone read anything back.
pub const PHONE_SCOPES: [&str; 6] = [
	"location:write",
	"apps:write",
	"device:write",
	"health:write",
	"motion:write",
	"sensor:write",
];

/// The write scopes a watch pairs with (Wear OS, watchOS, Huawei Watch).
pub const WATCH_SCOPES: [&str; 5] = [
	"health:write",
	"motion:write",
	"location:write",
	"device:write",
	"sensor:write",
];

/// How often the screen asks the service to look again and upload.
pub const TICK_SECONDS: u64 = 60;

/// How often the screen reads app usage.
pub const USAGE_SECONDS: u64 = 5 * 60;

/// How often the screen polls `HealthKit` / Health Connect incremental changes.
pub const HEALTH_SECONDS: u64 = 5 * 60;

/// Everything the phone's or watch's screen shows that the service knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
	/// Bumped whenever anything below changes.
	pub version: u64,
	/// The status line, such as "Collecting · 3 waiting".
	pub headline: String,
	/// How this device stands with the vault.
	pub link: Link,
	/// The vault's address, once a pairing link was accepted.
	pub vault: Option<String>,
	/// The command that approves this device, while it waits.
	pub approve: Option<String>,
	/// The encrypted spool.
	pub storage: Storage,
	/// Whether the owner wants this device to collect.
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
	/// Latest observed heart rate in BPM (for glanceable watch surfaces).
	#[serde(default)]
	pub latest_heart_rate_bpm: Option<u16>,
	/// Cumulative steps observed in the current session/day.
	#[serde(default)]
	pub steps_today: Option<u64>,
	/// Whether a companion watch/phone link is currently reachable.
	#[serde(default)]
	pub watch_connected: bool,
	/// Display name of the paired watch/phone peer, when known.
	#[serde(default)]
	pub watch_name: Option<String>,
	/// Connected BLE sensor names (e.g. "Polar H10").
	#[serde(default)]
	pub ble_devices: Vec<String>,
	/// Latest observed battery percentage of this device (`0..=100`).
	#[serde(default)]
	pub battery_percent: Option<u8>,
	/// Stable device id string of this phone or watch.
	#[serde(default)]
	pub device_id: Option<String>,
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

/// What the phone or watch collects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
	/// Where the device is: `location.fix`.
	Location,
	/// Which apps were in front, and screen and lock changes: `apps.focus`
	/// and `device.state`.
	Usage,
	/// Battery level, charging, low-power mode and thermal state:
	/// `device.battery` and `device.power`.
	Power,
	/// `HealthKit` / Health Connect / `HarmonyOS` Health store records:
	/// heart rate, HRV, `SpO2`, steps, sleep, workouts, skin temp, calories.
	Health,
	/// Direct on-watch wrist biometric sensors (Wear OS Health Services,
	/// Samsung Galaxy Watch sensors, watchOS live workouts, Huawei Watch).
	Wearable,
	/// Pedometer, accelerometer, gyroscope, and barometer motion streams.
	Motion,
	/// Companion phone ↔ watch synced batches.
	WatchSync,
	/// Paired BLE GATT straps, rings, oximeters, and fitness sensors.
	Ble,
}

impl Source {
	/// Every source, in the order the screen lists them.
	pub const ALL: [Source; 8] = [
		Source::Location,
		Source::Usage,
		Source::Power,
		Source::Health,
		Source::Wearable,
		Source::Motion,
		Source::WatchSync,
		Source::Ble,
	];

	/// The source's name on the screen.
	pub fn label(self) -> &'static str {
		match self {
			Source::Location => "Location",
			Source::Usage => "App usage",
			Source::Power => "Battery and temperature",
			Source::Health => "Health store",
			Source::Wearable => "Watch sensors",
			Source::Motion => "Steps and motion",
			Source::WatchSync => "Watch companion sync",
			Source::Ble => "Bluetooth sensors",
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
	/// Ask for the next health-store window and saved changes token. Answers
	/// [`ReplyData::HealthWindow`].
	HealthPlan,
	/// Persist the latest opaque health-store incremental sync token.
	SetHealthToken {
		/// Next token from `Health::changes_token` / `Health::changes`.
		token: Option<String>,
	},
	/// Advance the windowed health-store watermark to `until_ms` after a
	/// windowed read completes.
	AdvanceHealthUntil {
		/// End of the window just read, in Unix milliseconds.
		until_ms: i64,
	},
	/// Update the companion watch/phone peer connectivity state reported by
	/// `watch-sync`.
	WatchPeerStatus {
		/// Whether the paired peer is currently reachable.
		connected: bool,
		/// Peer display name, when known.
		#[serde(default)]
		name: Option<String>,
	},
	/// Ask for the saved vault pairing link so the phone can forward it to a
	/// paired watch over `watch-sync`. Answers [`ReplyData::PairLink`].
	ExportPairLink,
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
	/// The next health-store read window and incremental changes token.
	HealthWindow {
		/// From, inclusive.
		since_ms: i64,
		/// To, exclusive.
		until_ms: i64,
		/// Previously persisted incremental sync token, when available.
		token: Option<String>,
	},
	/// The saved `quant://link?...` URI to push to a paired watch.
	PairLink {
		/// Pairing link URI.
		link: String,
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
const KIND_HEALTH: u8 = 4;
const KIND_WEARABLE: u8 = 5;
const KIND_PEDOMETER: u8 = 6;
const KIND_MOTION: u8 = 7;
const KIND_BLE: u8 = 8;
const KIND_WATCH_SYNC: u8 = 9;

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
	/// A `leptoskit_plugin_health` `Page` reply (or `Changes` reply when
	/// `is_changes` is true).
	Health {
		/// True when `payload` is a `Changes` reply; false for a `Page` reply.
		is_changes: bool,
		/// Raw reply bytes from `health`.
		payload: &'a [u8],
	},
	/// A `leptoskit_plugin_wearable` `Batch` reply from wrist sensors.
	Wearable(&'a [u8]),
	/// A `leptoskit_plugin_pedometer` `Reading` reply.
	Pedometer(&'a [u8]),
	/// A `leptoskit_sensor` batch reply tagged with sensor kind code
	/// (0 = accel, 2 = gyro, 4 = barometer).
	Motion {
		/// Sensor kind wire code.
		kind_code: u32,
		/// Raw batch reply bytes.
		batch: &'a [u8],
	},
	/// A `leptoskit_plugin_ble` `NotificationBatch` reply.
	Ble(&'a [u8]),
	/// A companion-synced inner `Reading` frame ferried from a paired watch.
	WatchSync(&'a [u8]),
}

impl<'a> Reading<'a> {
	/// The source a reading belongs to.
	pub fn source(&self) -> Source {
		match self {
			Reading::Location(_) => Source::Location,
			Reading::Usage { .. } => Source::Usage,
			Reading::Power { .. } => Source::Power,
			Reading::Health { .. } => Source::Health,
			Reading::Wearable(_) => Source::Wearable,
			Reading::Pedometer(_) | Reading::Motion { .. } => Source::Motion,
			Reading::Ble(_) => Source::Ble,
			Reading::WatchSync(_) => Source::WatchSync,
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
			Reading::Health {
				is_changes,
				payload,
			} => {
				frame.push(KIND_HEALTH);
				frame.push(u8::from(*is_changes));
				part(&mut frame, payload)?;
			}
			Reading::Wearable(batch) => {
				frame.push(KIND_WEARABLE);
				part(&mut frame, batch)?;
			}
			Reading::Pedometer(reading) => {
				frame.push(KIND_PEDOMETER);
				part(&mut frame, reading)?;
			}
			Reading::Motion { kind_code, batch } => {
				frame.push(KIND_MOTION);
				frame.extend_from_slice(&kind_code.to_le_bytes());
				part(&mut frame, batch)?;
			}
			Reading::Ble(batch) => {
				frame.push(KIND_BLE);
				part(&mut frame, batch)?;
			}
			Reading::WatchSync(inner) => {
				frame.push(KIND_WATCH_SYNC);
				part(&mut frame, inner)?;
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
			KIND_POWER => Reading::Power {
				battery: take_optional(&mut rest).ok()?,
				thermal: take_optional(&mut rest).ok()?,
			},
			KIND_HEALTH => {
				let (flag, tail) = rest.split_first()?;
				rest = tail;
				let is_changes = match *flag {
					0 => false,
					1 => true,
					_ => return None,
				};
				Reading::Health {
					is_changes,
					payload: take_part(&mut rest)?,
				}
			}
			KIND_WEARABLE => Reading::Wearable(take_part(&mut rest)?),
			KIND_PEDOMETER => Reading::Pedometer(take_part(&mut rest)?),
			KIND_MOTION => {
				let (code, tail) = rest.split_first_chunk::<4>()?;
				rest = tail;
				Reading::Motion {
					kind_code: u32::from_le_bytes(*code),
					batch: take_part(&mut rest)?,
				}
			}
			KIND_BLE => Reading::Ble(take_part(&mut rest)?),
			KIND_WATCH_SYNC => Reading::WatchSync(take_part(&mut rest)?),
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
		round_trip(&Reading::Health {
			is_changes: false,
			payload: &[4, 5, 6],
		});
		round_trip(&Reading::Health {
			is_changes: true,
			payload: &[7, 8],
		});
		round_trip(&Reading::Wearable(&[1, 2, 3, 4]));
		round_trip(&Reading::Pedometer(&[0; 32]));
		round_trip(&Reading::Motion {
			kind_code: 0,
			batch: &[9, 10, 11],
		});
		round_trip(&Reading::Ble(&[12, 13]));
		round_trip(&Reading::WatchSync(&[1, 2, 0, 0, 0, 3, 4]));
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
		assert_eq!(Reading::decode(&[99, 0, 0, 0, 0]), None);

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
		assert!(WATCH_SCOPES.iter().all(|scope| scope.ends_with(":write")));
	}
}
