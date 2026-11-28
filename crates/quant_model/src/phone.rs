//! What the phone's screen and its native service say to each other.
//!
//! On a phone the screen is the only code that can reach the Leptoskit
//! plugins (location, app usage, battery, thermal), and the native service is
//! the only code that can open the encrypted spool and sign uploads. So the
//! screen asks for permissions, starts the plugins, and hands every reply to
//! the service unread, as a [`Reading`]; the service decodes it, queues it, and
//! delivers it to the vault. The screen never sees a signing key, and nothing
//! it hands over is trusted until the service has decoded it.
//!
//! The collector is write-only. When the owner asks for it, the service also
//! keeps an opt-in companion identity with its own key and grant, approved
//! through the same pairing workflow, so the screen can show bounded
//! aggregates of the vault. The companion never widens the collector's
//! scopes, never handles the owner's passphrase, and a failure to read it
//! never stops recording, queueing, or delivery.

use serde::Deserialize;
use serde::Serialize;

use crate::Notice;

/// The service's capability name.
pub const SERVICE: &str = "quant-phone";

/// The app-owned bridge plugin that holds an inbound `quant://link`.
///
/// The platform host registers it; the screen asks for the next link. A link
/// waits in the plugin until the screen takes it, so a cold launch cannot
/// lose it. It never pairs anything by itself.
pub const LINK_PLUGIN: &str = "quant-link";

/// Wait for the next inbound link. Answers its UTF-8 bytes, or empty when the
/// plugin is closing. The call stays pending while no link has arrived.
pub const LINK_NEXT: u32 = 1;

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
/// them lets the collector read anything back; stats use the separate,
/// opt-in companion identity in [`STATS_SCOPES`].
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

/// The scopes the opt-in stats companion asks for: aggregates of every
/// stream the owner chose to share, never records, text, or media.
pub const STATS_SCOPES: [&str; 1] = ["*:summary"];

/// The scope the opt-in Ask companion asks for: sending questions to approved
/// agents and reading their actual replies, never reading vault streams
/// directly.
pub const ASK_SCOPES: [&str; 1] = ["companion:write"];

/// One capability the opt-in companion identity can ask the owner to
/// approve.
///
/// The companion has its own signing key and its own grant, separate from
/// the write-only collector's. Capabilities are separately selectable on that
/// identity; approving one never grants another without explicit owner
/// approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionCapability {
	/// Read bounded aggregates from the streams the owner shares.
	StatsSummary,
	/// Send questions to approved agents and read their actual replies.
	AskAgents,
}

impl CompanionCapability {
	/// Every capability this build can request, in approval order.
	pub const ALL: [Self; 2] = [Self::StatsSummary, Self::AskAgents];

	/// The scopes this capability asks the owner for. A capability never
	/// widens another one's request.
	pub fn scopes(self) -> &'static [&'static str] {
		match self {
			Self::StatsSummary => &STATS_SCOPES,
			Self::AskAgents => &ASK_SCOPES,
		}
	}

	/// The capability's name on the screen.
	pub fn label(self) -> &'static str {
		match self {
			Self::StatsSummary => "Stats",
			Self::AskAgents => "Ask",
		}
	}
}

/// The stretch a stats summary covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatsWindow {
	/// The last 24 hours.
	#[default]
	Day,
	/// The last 7 days.
	Week,
}

impl StatsWindow {
	/// Every window the screen can ask for.
	pub const ALL: [Self; 2] = [Self::Day, Self::Week];

	/// How far back the window reaches, in milliseconds.
	pub fn span_ms(self) -> i64 {
		match self {
			Self::Day => 24 * 60 * 60 * 1000,
			Self::Week => 7 * 24 * 60 * 60 * 1000,
		}
	}

	/// How the window reads on the screen.
	pub fn label(self) -> &'static str {
		match self {
			Self::Day => "Past 24 hours",
			Self::Week => "Past 7 days",
		}
	}
}

/// Where the opt-in stats companion stands with the vault.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StatsAccess {
	/// No vault is paired yet, so there is nothing to read from.
	Unpaired,
	/// The owner has not asked this phone for stats.
	#[default]
	NotRequested,
	/// A companion identity exists and its grant has not been checked since
	/// launch. Asking for a summary resolves this.
	Checking,
	/// Waiting for the owner to approve the companion.
	Pending {
		/// The pairing code the owner approves.
		code: String,
	},
	/// Approved, and allowed to read at least one stream.
	Ready,
	/// Approved, but the grant shares no stream this capability can read.
	Restricted,
	/// The owner refused or revoked the companion, or its grant ran out.
	Revoked,
	/// The vault, or this phone's companion identity, could not be read.
	Unreachable {
		/// Why.
		reason: String,
	},
}

/// What the screen knows about one opt-in companion capability without asking
/// the vault.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StatsState {
	/// How this capability stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the last request or query stopped, when it did.
	pub problem: Option<String>,
}

/// An explicit capability change on an already active companion identity,
/// waiting for the owner to approve the updated grant on their computer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanionChange {
	/// The companion identity's pairing code on the vault.
	pub code: String,
	/// The capabilities requested for the updated grant.
	pub capabilities: Vec<CompanionCapability>,
	/// The exact command the owner runs to approve the updated grant.
	pub approve: String,
}

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
	/// The opt-in stats companion. Collection and delivery never depend on
	/// it.
	#[serde(default)]
	pub stats: StatsState,
	/// The opt-in Ask companion (`companion:write`) on the same companion
	/// identity. Collection and delivery never depend on it.
	#[serde(default)]
	pub ask: StatsState,
	/// A requested capability change on an already active companion identity,
	/// waiting for explicit owner approval.
	#[serde(default)]
	pub companion_change: Option<CompanionChange>,
	/// Bumped whenever the paired vault or companion identity/grant changes,
	/// so the screen drops cached session data and ignores stale responses.
	#[serde(default)]
	pub vault_generation: u64,
	/// Per-launch random prefix used by the screen to mint collision-free
	/// idempotency keys for companion messages.
	#[serde(default)]
	pub session_nonce: String,
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

/// Whether a stream holds discrete records or numeric samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatsKind {
	/// Discrete records.
	Event,
	/// Numeric samples.
	Series,
}

/// The most streams one summary lists.
pub const MAX_STATS_STREAMS: usize = 24;
/// The most streams whose detail is read in one summary.
pub const MAX_STATS_DETAILS: usize = 8;
/// The most channels one numeric stream lists.
pub const MAX_STATS_CHANNELS: usize = 8;
/// The most buckets one numeric stream lists.
pub const MAX_STATS_BUCKETS: usize = 12;
/// The most groups one duration breakdown lists.
pub const MAX_STATS_GROUPS: usize = 8;

/// One channel's aggregates across a range. `samples` is the number of
/// readings, never the sum of their values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsChannel {
	/// Channel name, such as `bpm` or `steps`.
	pub name: String,
	/// Unit, such as `bpm`, `count`, or `m`.
	pub unit: String,
	/// Number of samples behind the statistics.
	pub samples: u64,
	/// Smallest value, when there is a sample.
	pub min: Option<f64>,
	/// Largest value, when there is a sample.
	pub max: Option<f64>,
	/// Mean value, when there is a sample.
	pub mean: Option<f64>,
}

/// One channel's aggregates inside one bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsBucketChannel {
	/// Channel name.
	pub name: String,
	/// Unit, such as `bpm`, `count`, or `m`.
	pub unit: String,
	/// Number of samples in the bucket.
	pub samples: u64,
	/// Smallest value.
	pub min: f64,
	/// Largest value.
	pub max: f64,
	/// Mean value.
	pub mean: f64,
}

/// One bucket of a numeric stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsBucket {
	/// The bucket's start, as the screen words it.
	pub start: String,
	/// The bucket's start, in Unix milliseconds.
	pub start_ms: i64,
	/// Every channel with a sample in this bucket.
	pub channels: Vec<StatsBucketChannel>,
}

/// One group's share of time in an event stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsDuration {
	/// The group, such as an app or sleep stage.
	pub group: Option<String>,
	/// Records in the group.
	pub count: u64,
	/// Seconds the group covered.
	pub seconds: f64,
	/// The same span, as the screen words it, such as "1h 20m".
	pub duration: String,
}

/// How much detail one stream's summary carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatsDetail {
	/// Bucketed numeric aggregates, with each channel's unit.
	Numeric {
		/// Every channel, including ones with no samples in the range.
		channels: Vec<StatsChannel>,
		/// The buckets that hold samples.
		buckets: Vec<StatsBucket>,
		/// Whether channels were left out by the display bound.
		channels_truncated: bool,
		/// Whether buckets were left out by the display bound.
		buckets_truncated: bool,
	},
	/// Time spent per group in an event stream whose spec names summary
	/// keys.
	Durations {
		/// The attribute the groups are.
		group_by: String,
		/// Groups, longest first.
		groups: Vec<StatsDuration>,
		/// Whether groups were left out by the display bound.
		truncated: bool,
	},
	/// Only the record count is shown.
	Counted,
	/// Readable, but the summary's detail budget was already spent.
	Bounded,
}

/// One stream in a bounded summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsStream {
	/// The stream id, such as `health.heart_rate`.
	pub stream: String,
	/// The stream's short name on the screen.
	pub label: String,
	/// The family's name on the screen.
	pub family: String,
	/// One line explaining the stream.
	pub description: String,
	/// Records or numeric samples.
	pub kind: StatsKind,
	/// Records for event streams, samples for series streams. Never a sum
	/// of their values.
	pub count: u64,
	/// The unit for `count`: `records` or `samples`.
	pub count_unit: String,
	/// First reading, as the screen words it.
	pub first: Option<String>,
	/// Last reading, as the screen words it.
	pub last: Option<String>,
	/// First reading, in Unix milliseconds.
	pub first_ms: Option<i64>,
	/// Last reading, in Unix milliseconds.
	pub last_ms: Option<i64>,
	/// The detail the vault allowed.
	pub detail: StatsDetail,
}

/// A bounded, display-ready summary of what the paired vault holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VaultStats {
	/// The window that was asked for.
	pub window: StatsWindow,
	/// How the window reads, such as "Past 24 hours".
	pub span: String,
	/// The effective range's start, as the screen words it. The vault may
	/// clip it to the companion grant's lookback.
	pub from: String,
	/// The effective range's end, as the screen words it.
	pub to: String,
	/// The effective range's start, in Unix milliseconds.
	pub from_ms: i64,
	/// The effective range's end, in Unix milliseconds.
	pub to_ms: i64,
	/// The streams the companion can read, including empty ones.
	pub streams: Vec<StatsStream>,
	/// Whether readable streams were left out by the display bound.
	pub truncated: bool,
	/// Whether some stream detail was left out by the detail bound.
	pub details_truncated: bool,
	/// Starting offset of this page within the permitted stream catalog.
	#[serde(default)]
	pub offset: usize,
	/// Total permitted streams in the catalog.
	#[serde(default)]
	pub total_streams: usize,
	/// Next catalog offset when more permitted streams follow this page.
	#[serde(default)]
	pub next_offset: Option<usize>,
	/// When the vault answered, as the screen words it.
	pub fetched: String,
}

/// The answer to a stats request: where access stands, and the summary
/// when the vault answered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsReply {
	/// How the companion stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the request or summary stopped, when it did.
	pub problem: Option<String>,
	/// The bounded summary, when the vault answered.
	pub summary: Option<VaultStats>,
}

/// The answer to [`Command::VaultStreamDetail`]: on-demand detail for one
/// permitted stream with bounded channel and duration-group paging.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamDetailReply {
	/// How the stats capability stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the request stopped, when it did.
	pub problem: Option<String>,
	/// The window that was asked for.
	pub window: StatsWindow,
	/// How the window reads, such as "Past 24 hours".
	pub span: String,
	/// The effective range's start, as the screen words it.
	pub from: Option<String>,
	/// The effective range's end, as the screen words it.
	pub to: Option<String>,
	/// The effective range's start, in Unix milliseconds.
	pub from_ms: Option<i64>,
	/// The effective range's end, in Unix milliseconds.
	pub to_ms: Option<i64>,
	/// When the vault answered, as the screen words it.
	pub fetched: Option<String>,
	/// The stream's detail, when the vault answered.
	pub stream: Option<StatsStream>,
	/// Starting offset of the returned channels within numeric detail.
	#[serde(default)]
	pub channel_offset: usize,
	/// Total channels on this stream.
	#[serde(default)]
	pub total_channels: usize,
	/// Next channel offset when more channels follow this page.
	#[serde(default)]
	pub next_channel_offset: Option<usize>,
	/// Starting offset of the returned duration groups.
	#[serde(default)]
	pub group_offset: usize,
	/// Total duration groups on this stream.
	#[serde(default)]
	pub total_groups: usize,
	/// Next group offset when more duration groups follow this page.
	#[serde(default)]
	pub next_group_offset: Option<usize>,
}

/// One approved agent the companion may message.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanionAgentRow {
	/// The agent client's id string.
	pub id: String,
	/// The name the owner approved.
	pub name: String,
	/// Product/model detail the agent declared at pairing, when present.
	#[serde(default)]
	pub detail: Option<String>,
}

/// Where one companion message stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionReplyStatus {
	/// Waiting in the addressed agent's Quant inbox for an actual reply.
	Waiting,
	/// Answered by the addressed agent.
	Answered,
	/// Closed without an answer (expired or access withdrawn).
	Closed,
}

/// One question this companion sent to an approved agent and any actual reply.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompanionMessageRow {
	/// The message id string.
	pub id: String,
	/// The addressed agent's id string.
	pub to_id: String,
	/// The addressed agent's name.
	pub to_name: String,
	/// The question submitted.
	pub question: String,
	/// When it was submitted, as the screen words it.
	pub submitted: String,
	/// When it was submitted, in Unix milliseconds.
	pub submitted_ms: i64,
	/// Where it stands.
	pub status: CompanionReplyStatus,
	/// The agent's actual reply, once answered. Never manufactured.
	#[serde(default)]
	pub answer: Option<String>,
	/// When the agent answered, as the screen words it.
	#[serde(default)]
	pub answered: Option<String>,
	/// When the agent answered, in Unix milliseconds.
	#[serde(default)]
	pub answered_ms: Option<i64>,
	/// Record ids cited by the reply.
	#[serde(default)]
	pub citations: Vec<String>,
}

/// The answer to [`Command::CompanionAgents`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionAgentsReply {
	/// How the Ask capability stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the request stopped, when it did.
	pub problem: Option<String>,
	/// Approved agents this phone may address.
	pub agents: Vec<CompanionAgentRow>,
}

/// The answer to [`Command::CompanionMessages`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionMessagesReply {
	/// How the Ask capability stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the request stopped, when it did.
	pub problem: Option<String>,
	/// This companion's own messages, newest first.
	pub messages: Vec<CompanionMessageRow>,
	/// When the vault answered, as the screen words it.
	#[serde(default)]
	pub fetched: Option<String>,
}

/// The answer to [`Command::SendCompanionMessage`] and
/// [`Command::CompanionMessage`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionMessageReply {
	/// How the Ask capability stands.
	pub access: StatsAccess,
	/// The command that approves the companion, while it waits.
	pub approve: Option<String>,
	/// Why the request stopped, when it did.
	pub problem: Option<String>,
	/// The message, when the vault answered.
	pub message: Option<CompanionMessageRow>,
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
	/// Ask the owner to approve the opt-in stats companion. This is the
	/// only way the companion identity is ever requested; collection never
	/// asks for it on its own.
	RequestStats,
	/// Ask the owner to approve or update the selected companion capabilities
	/// on this phone's companion identity.
	RequestCompanion {
		/// Selected capabilities (stats, ask, or both).
		capabilities: Vec<CompanionCapability>,
	},
	/// Check the live companion grant against the vault without creating a
	/// new identity or widening scopes.
	RefreshCompanion,
	/// Dismiss an unapproved capability change prompt while keeping the
	/// current companion grant intact.
	CancelCompanionChange,
	/// Forget the stats companion and its signing key. The collector's
	/// identity, the encrypted spool, and everything waiting are untouched.
	ForgetStats,
	/// Forget the companion identity and its signing key (alias for
	/// [`Command::ForgetStats`]).
	ForgetCompanion,
	/// Read a bounded summary through the approved stats companion. Answers
	/// [`ReplyData::VaultStats`] with its access, approval, offline, or empty
	/// state.
	VaultStats {
		/// Which window to summarise.
		window: StatsWindow,
	},
	/// Read one bounded page of the permitted stream catalog with effective
	/// range, counts, and first/last timestamps. Answers
	/// [`ReplyData::VaultStats`].
	VaultCatalog {
		/// Which window to summarise.
		window: StatsWindow,
		/// Starting index in the sorted stream catalog.
		offset: usize,
		/// Maximum streams to return in this page (clamped by the service).
		limit: usize,
	},
	/// Read on-demand detail for one permitted stream, with bounded channel
	/// and duration-group paging. Answers [`ReplyData::VaultStreamDetail`].
	VaultStreamDetail {
		/// The stream id, such as `health.heart_rate` or a custom stream.
		stream: String,
		/// Which window to read.
		window: StatsWindow,
		/// Starting channel index for numeric streams.
		#[serde(default)]
		channel_offset: usize,
		/// Starting duration-group index for event streams.
		#[serde(default)]
		group_offset: usize,
	},
	/// List approved agents this companion may address. Answers
	/// [`ReplyData::CompanionAgents`].
	CompanionAgents,
	/// List this companion's own messages and actual replies. Answers
	/// [`ReplyData::CompanionMessages`].
	CompanionMessages {
		/// Maximum messages to return (bounded by the daemon).
		#[serde(default)]
		limit: Option<usize>,
	},
	/// Send a question to one approved agent with an idempotency key.
	/// Answers [`ReplyData::CompanionMessage`].
	SendCompanionMessage {
		/// Target approved agent id string.
		to: String,
		/// Question text.
		question: String,
		/// Idempotency key retained across unresolved retries.
		dedupe: String,
	},
	/// Fetch one of this companion's messages and any actual reply. Answers
	/// [`ReplyData::CompanionMessage`].
	CompanionMessage {
		/// Message id string.
		id: String,
	},
}

/// Extra data a [`Command`]'s [`Reply`] can carry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
	/// The answer to [`Command::VaultStats`] and [`Command::VaultCatalog`].
	VaultStats {
		/// Where access stands, and the bounded summary when it answered.
		stats: StatsReply,
	},
	/// The answer to [`Command::VaultStreamDetail`].
	VaultStreamDetail {
		/// Where access stands, and the stream detail when it answered.
		detail: StreamDetailReply,
	},
	/// The answer to [`Command::CompanionAgents`].
	CompanionAgents {
		/// Where access stands, and the approved agent directory.
		reply: CompanionAgentsReply,
	},
	/// The answer to [`Command::CompanionMessages`].
	CompanionMessages {
		/// Where access stands, and this companion's messages.
		reply: CompanionMessagesReply,
	},
	/// The answer to [`Command::SendCompanionMessage`] and
	/// [`Command::CompanionMessage`].
	CompanionMessage {
		/// Where access stands, and the message.
		reply: CompanionMessageReply,
	},
}

/// The answer to a [`Command`] or a reading. Details arrive in the next
/// [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
			KIND_POWER => {
				Reading::Power {
					battery: take_optional(&mut rest).ok()?,
					thermal: take_optional(&mut rest).ok()?,
				}
			}
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

	#[test]
	fn the_companion_capabilities_keep_separate_scopes() {
		assert_eq!(CompanionCapability::StatsSummary.scopes(), ["*:summary"]);
		assert_eq!(CompanionCapability::AskAgents.scopes(), ["companion:write"]);
		assert_eq!(
			CompanionCapability::ALL,
			[
				CompanionCapability::StatsSummary,
				CompanionCapability::AskAgents
			]
		);
		assert!(
			CompanionCapability::StatsSummary
				.scopes()
				.iter()
				.all(|scope| scope.ends_with(":summary"))
		);
		assert_eq!(CompanionCapability::StatsSummary.label(), "Stats");
		assert_eq!(CompanionCapability::AskAgents.label(), "Ask");
	}

	#[test]
	fn stats_windows_are_bounded_and_readable() {
		assert_eq!(StatsWindow::Day.span_ms(), 24 * 60 * 60 * 1000);
		assert_eq!(StatsWindow::Week.span_ms(), 7 * 24 * 60 * 60 * 1000);
		assert_eq!(StatsWindow::default(), StatsWindow::Day);
		assert_eq!(StatsWindow::Day.label(), "Past 24 hours");
		assert_eq!(StatsWindow::ALL.len(), 2);
	}

	#[test]
	fn stats_and_ask_commands_and_replies_round_trip_as_tagged_json() {
		let command = Command::VaultStats {
			window: StatsWindow::Week,
		};
		let json = serde_json::to_string(&command).unwrap();
		assert_eq!(json, r#"{"command":"vault_stats","window":"week"}"#);
		assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);

		for extra in [
			Command::RequestCompanion {
				capabilities: vec![
					CompanionCapability::StatsSummary,
					CompanionCapability::AskAgents,
				],
			},
			Command::RefreshCompanion,
			Command::CancelCompanionChange,
			Command::ForgetCompanion,
			Command::VaultCatalog {
				window: StatsWindow::Day,
				offset: 24,
				limit: 24,
			},
			Command::VaultStreamDetail {
				stream: "health.heart_rate".into(),
				window: StatsWindow::Week,
				channel_offset: 8,
				group_offset: 0,
			},
			Command::CompanionAgents,
			Command::CompanionMessages { limit: Some(50) },
			Command::SendCompanionMessage {
				to: "agent-1".into(),
				question: "How did I sleep?".into(),
				dedupe: "m-abc12345-1".into(),
			},
			Command::CompanionMessage { id: "msg-1".into() },
		] {
			let encoded = serde_json::to_string(&extra).unwrap();
			assert_eq!(serde_json::from_str::<Command>(&encoded).unwrap(), extra);
		}

		let reply = Reply::data(ReplyData::VaultStats {
			stats: StatsReply {
				access: StatsAccess::Pending {
					code: "K7QD-M2XA".into(),
				},
				approve: Some("quant approve K7QD-M2XA --scope '*:summary'".into()),
				problem: None,
				summary: Some(VaultStats {
					window: StatsWindow::Day,
					span: StatsWindow::Day.label().into(),
					from: "Yesterday 14:03".into(),
					to: "Today 13:07".into(),
					from_ms: 1,
					to_ms: 2,
					streams: vec![StatsStream {
						stream: "health.steps".into(),
						label: "Steps".into(),
						family: "Health".into(),
						description: "Steps counted per interval.".into(),
						kind: StatsKind::Series,
						count: 12,
						count_unit: "samples".into(),
						first: None,
						last: None,
						first_ms: None,
						last_ms: None,
						detail: StatsDetail::Numeric {
							channels: vec![StatsChannel {
								name: "steps".into(),
								unit: "count".into(),
								samples: 12,
								min: Some(4.0),
								max: Some(30.0),
								mean: Some(12.5),
							}],
							buckets: vec![StatsBucket {
								start: "Today 12:00".into(),
								start_ms: 2,
								channels: vec![StatsBucketChannel {
									name: "steps".into(),
									unit: "count".into(),
									samples: 4,
									min: 4.0,
									max: 30.0,
									mean: 12.5,
								}],
							}],
							channels_truncated: false,
							buckets_truncated: false,
						},
					}],
					truncated: false,
					details_truncated: false,
					offset: 0,
					total_streams: 1,
					next_offset: None,
					fetched: "Today 13:07".into(),
				}),
			},
		});
		let json = serde_json::to_string(&reply).unwrap();
		assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);

		let ask_reply = Reply::data(ReplyData::CompanionMessage {
			reply: CompanionMessageReply {
				access: StatsAccess::Ready,
				approve: None,
				problem: None,
				message: Some(CompanionMessageRow {
					id: "msg-1".into(),
					to_id: "agent-1".into(),
					to_name: "Coach".into(),
					question: "What was my heart rate?".into(),
					submitted: "Today 13:00".into(),
					submitted_ms: 1000,
					status: CompanionReplyStatus::Answered,
					answer: Some("Average was 72 bpm.".into()),
					answered: Some("Today 13:02".into()),
					answered_ms: Some(2000),
					citations: vec!["rec-1".into()],
				}),
			},
		});
		let json = serde_json::to_string(&ask_reply).unwrap();
		assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), ask_reply);
	}

	#[test]
	fn a_snapshot_without_companion_fields_still_loads() {
		let mut json = serde_json::to_value(Snapshot::default()).unwrap();
		let object = json.as_object_mut().unwrap();
		object.remove("stats");
		object.remove("ask");
		object.remove("companion_change");
		object.remove("vault_generation");
		object.remove("session_nonce");
		let snapshot: Snapshot = serde_json::from_value(json).unwrap();
		assert_eq!(snapshot.stats, StatsState::default());
		assert_eq!(snapshot.ask, StatsState::default());
		assert_eq!(snapshot.companion_change, None);
		assert_eq!(snapshot.vault_generation, 0);
		assert_eq!(snapshot.session_nonce, "");
	}
}
