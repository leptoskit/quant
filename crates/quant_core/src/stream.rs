use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::CoreError;

/// The top-level group a stream belongs to. Access grants are scoped to
/// families (and optionally to single streams), so the families are fixed:
/// adding a new wearable means adding a stream under [`Family::Sensor`], never
/// a new family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
	/// Screen frames and the text read off them.
	Screen,
	/// Microphone and system audio, and transcripts of it.
	Audio,
	/// Which app, window, and page is in focus.
	Apps,
	/// Keyboard and pointer activity levels (counts, never keystrokes).
	Input,
	/// Heart rate, HRV, blood oxygen, sleep, steps.
	Health,
	/// Acceleration, rotation, orientation, posture, gait.
	Motion,
	/// Position fixes and visits.
	Location,
	/// Battery, screen on/off, connectivity.
	Device,
	/// Notifications, calls, and message metadata.
	Comms,
	/// Anything from custom hardware: `sensor.<vendor>.<channel>`.
	Sensor,
	/// What Quant (or an agent) derived: episodes, summaries, notes.
	Memory,
}

impl Family {
	/// Every family, in a stable order.
	pub const ALL: [Family; 11] = [
		Family::Screen,
		Family::Audio,
		Family::Apps,
		Family::Input,
		Family::Health,
		Family::Motion,
		Family::Location,
		Family::Device,
		Family::Comms,
		Family::Sensor,
		Family::Memory,
	];

	/// The family's name as it appears in stream ids and scopes.
	pub fn as_str(self) -> &'static str {
		match self {
			Family::Screen => "screen",
			Family::Audio => "audio",
			Family::Apps => "apps",
			Family::Input => "input",
			Family::Health => "health",
			Family::Motion => "motion",
			Family::Location => "location",
			Family::Device => "device",
			Family::Comms => "comms",
			Family::Sensor => "sensor",
			Family::Memory => "memory",
		}
	}
}

impl fmt::Display for Family {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

impl FromStr for Family {
	type Err = CoreError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Family::ALL
			.into_iter()
			.find(|family| family.as_str() == s)
			.ok_or_else(|| CoreError::UnknownFamily(s.to_owned()))
	}
}

/// A stream name such as `health.heart_rate` or `sensor.vest.ecg`: a family
/// followed by one or more lowercase segments.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StreamId {
	name: String,
	family: Family,
}

impl StreamId {
	/// Parse and validate a stream id.
	pub fn new(name: impl Into<String>) -> Result<Self, CoreError> {
		let name = name.into();
		let mut segments = name.split('.');
		let family = segments.next().unwrap_or_default();
		let rest: Vec<&str> = segments.collect();
		let valid_segment = |segment: &str| {
			!segment.is_empty()
				&& segment.len() <= 48
				&& segment
					.bytes()
					.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
		};
		if rest.is_empty() || !rest.iter().all(|s| valid_segment(s)) {
			return Err(CoreError::InvalidStream(name));
		}
		let family = family.parse()?;
		Ok(Self { name, family })
	}

	/// The family the stream belongs to.
	pub fn family(&self) -> Family {
		self.family
	}

	/// The full dotted name.
	pub fn as_str(&self) -> &str {
		&self.name
	}
}

impl fmt::Display for StreamId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.name)
	}
}

impl FromStr for StreamId {
	type Err = CoreError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::new(s)
	}
}

impl TryFrom<String> for StreamId {
	type Error = CoreError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		Self::new(value)
	}
}

impl From<StreamId> for String {
	fn from(value: StreamId) -> Self {
		value.name
	}
}

/// Whether a stream carries discrete records or numeric samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
	/// Discrete [`Record`](crate::Record)s.
	Event,
	/// Dense numeric [`SeriesBatch`](crate::SeriesBatch)es.
	Series,
}

/// One column of a series stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
	/// Column name, such as `bpm` or `x`.
	pub name: String,
	/// Unit, such as `bpm`, `m/s2`, `deg`.
	pub unit: String,
}

impl Channel {
	/// A channel with a name and unit.
	pub fn new(name: &str, unit: &str) -> Self {
		Self {
			name: name.to_owned(),
			unit: unit.to_owned(),
		}
	}
}

/// Describes a stream so agents know what it holds and devices know what to
/// send. Custom hardware registers its own specs at runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamSpec {
	/// The stream's id.
	pub id: StreamId,
	/// Event or series.
	pub kind: StreamKind,
	/// For series streams, the columns of every row.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub channels: Vec<Channel>,
	/// One line telling an agent what the stream means.
	pub description: String,
	/// How long raw media attached to this stream is kept, in hours. `None`
	/// keeps it until the owner deletes it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub media_retention_hours: Option<u32>,
	/// Attributes of event records that summaries may group by, such as
	/// `app` for focus spans or `posture` for posture spans. Only these are
	/// shown to a grant with summary access; titles, URLs and text never are.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub summary_keys: Vec<String>,
}

impl StreamSpec {
	fn event(id: &str, description: &str) -> Self {
		Self {
			id: StreamId::new(id).expect("built-in stream ids are valid"),
			kind: StreamKind::Event,
			channels: Vec::new(),
			description: description.to_owned(),
			media_retention_hours: None,
			summary_keys: Vec::new(),
		}
	}

	fn series(id: &str, channels: &[(&str, &str)], description: &str) -> Self {
		Self {
			kind: StreamKind::Series,
			channels: channels.iter().map(|(n, u)| Channel::new(n, u)).collect(),
			..Self::event(id, description)
		}
	}

	fn media_hours(mut self, hours: u32) -> Self {
		self.media_retention_hours = Some(hours);
		self
	}

	fn summarise_by(mut self, keys: &[&str]) -> Self {
		self.summary_keys = keys.iter().map(|k| (*k).to_owned()).collect();
		self
	}
}

/// The streams Quant understands out of the box.
pub fn builtin_streams() -> Vec<StreamSpec> {
	vec![
		StreamSpec::event(
			"screen.frame",
			"A screenshot moment: focused app, window title, URL, and the text read off the \
			 screen.",
		)
		.media_hours(24 * 7)
		.summarise_by(&["app"]),
		StreamSpec::event(
			"audio.transcript",
			"A transcribed span of speech heard by a device microphone.",
		)
		.media_hours(24)
		.summarise_by(&["source", "speaker"]),
		StreamSpec::event("apps.focus", "A span of time one app and window had focus.")
			.summarise_by(&["app"]),
		StreamSpec::series(
			"input.activity",
			&[("keys", "count"), ("clicks", "count"), ("scroll", "count")],
			"Keyboard and pointer activity per interval. Counts only, never content.",
		),
		StreamSpec::series(
			"health.heart_rate",
			&[("bpm", "bpm")],
			"Heart rate samples from a watch, strap, or phone.",
		),
		StreamSpec::series(
			"health.hrv",
			&[("rmssd", "ms")],
			"Heart rate variability (RMSSD).",
		),
		StreamSpec::series(
			"health.spo2",
			&[("percent", "%")],
			"Blood oxygen saturation.",
		),
		StreamSpec::series(
			"health.steps",
			&[("steps", "count")],
			"Steps counted per interval.",
		),
		StreamSpec::event(
			"health.sleep",
			"A sleep stage span: awake, light, deep, or rem.",
		)
		.summarise_by(&["stage"]),
		StreamSpec::event("health.workout", "An exercise session.").summarise_by(&["activity"]),
		StreamSpec::series(
			"motion.accelerometer",
			&[("x", "m/s2"), ("y", "m/s2"), ("z", "m/s2")],
			"Raw acceleration including gravity.",
		),
		StreamSpec::series(
			"motion.gyroscope",
			&[("x", "rad/s"), ("y", "rad/s"), ("z", "rad/s")],
			"Rotation rate.",
		),
		StreamSpec::series(
			"motion.orientation",
			&[("pitch", "deg"), ("roll", "deg"), ("yaw", "deg")],
			"Device or body orientation.",
		),
		StreamSpec::event(
			"motion.posture",
			"A span spent lying, sitting, standing, walking, running, or in a vehicle.",
		)
		.summarise_by(&["posture"]),
		StreamSpec::event(
			"motion.gait",
			"A derived walking bout: cadence, steps, stride regularity, and intensity.",
		)
		.summarise_by(&["placement"]),
		StreamSpec::series(
			"location.fix",
			&[
				("lat", "deg"),
				("lon", "deg"),
				("accuracy", "m"),
				("altitude", "m"),
			],
			"Position fixes.",
		),
		StreamSpec::event("location.visit", "A stay at one place.").summarise_by(&["place"]),
		StreamSpec::series(
			"device.battery",
			&[("level", "%")],
			"Battery level of a capturing device.",
		),
		StreamSpec::event(
			"device.state",
			"Screen on/off, lock, sleep, and wake transitions.",
		)
		.summarise_by(&["state"]),
		StreamSpec::event(
			"comms.notification",
			"A notification shown on a device: app, title, and text.",
		)
		.summarise_by(&["app"]),
		StreamSpec::event(
			"memory.episode",
			"A derived span of activity: what was happening, where, and how the body was doing.",
		)
		.summarise_by(&["activity"]),
		StreamSpec::event("memory.daily", "A derived summary of one day."),
		StreamSpec::event(
			"memory.note",
			"A note written by the owner or an authorised agent.",
		),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_valid_stream_ids() {
		let id = StreamId::new("sensor.vest.ecg_lead_2").unwrap();
		assert_eq!(id.family(), Family::Sensor);
		assert_eq!(id.as_str(), "sensor.vest.ecg_lead_2");
	}

	#[test]
	fn rejects_invalid_stream_ids() {
		for bad in [
			"health",
			"health.",
			"Health.bpm",
			"nope.thing",
			"health.heart-rate",
			"",
		] {
			assert!(StreamId::new(bad).is_err(), "{bad} should be rejected");
		}
	}

	#[test]
	fn builtin_streams_are_unique() {
		let specs = builtin_streams();
		let mut names: Vec<_> = specs.iter().map(|s| s.id.as_str()).collect();
		names.sort_unstable();
		names.dedup();
		assert_eq!(names.len(), specs.len());
	}

	#[test]
	fn stream_ids_serialize_as_strings() {
		let id = StreamId::new("motion.posture").unwrap();
		let json = serde_json::to_string(&id).unwrap();
		assert_eq!(json, "\"motion.posture\"");
		let back: StreamId = serde_json::from_str(&json).unwrap();
		assert_eq!(back, id);
		assert!(serde_json::from_str::<StreamId>("\"bogus\"").is_err());
	}
}
