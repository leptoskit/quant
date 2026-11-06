//! Posture and activity from accelerometers.
//!
//! Each accelerometer sample includes gravity. Over a short window, the mean
//! vector is gravity (so its direction gives the sensor's tilt) and the spread
//! around it is movement. Where the sensor sits decides what the tilt means:
//! a chest strap or vest that is horizontal means lying down; a thigh sensor
//! that is horizontal means sitting (or lying); a phone in a pocket or on a
//! desk says little beyond moving or still. With both a torso and a thigh
//! sensor, sitting and standing can be told apart.

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::StreamId;
use serde::Deserialize;
use serde::Serialize;

use crate::DERIVED_DEVICE;
use crate::stable_id;

/// Where an accelerometer is worn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
	/// On the chest or back: a strap, a vest. Tilt means body tilt.
	Torso,
	/// On the upper leg. Horizontal means sitting or lying.
	Thigh,
	/// Somewhere loose: a phone. Only movement is trusted.
	Loose,
}

/// A configured accelerometer source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
	/// The series stream, such as `motion.accelerometer` or
	/// `sensor.vest.accel`.
	pub stream: StreamId,
	/// Where it is worn.
	pub placement: Placement,
	/// The sensor axis that points up when the wearer stands: `[0, 1, 0]`
	/// for most phones and straps held upright.
	#[serde(default = "default_up")]
	pub up: [f64; 3],
}

fn default_up() -> [f64; 3] {
	[0.0, 1.0, 0.0]
}

/// What a window of samples shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
	/// Lying down.
	Lying,
	/// Leaning back.
	Reclined,
	/// Sitting.
	Sitting,
	/// Standing still.
	Standing,
	/// Upright, sitting or standing (torso sensor alone cannot tell).
	Upright,
	/// Thigh horizontal: sitting or lying (thigh sensor alone cannot tell).
	SittingOrLying,
	/// Still, orientation unknown (a loose sensor).
	Still,
	/// Walking.
	Walking,
	/// Running.
	Running,
}

impl Posture {
	/// The label stored in records.
	pub fn as_str(self) -> &'static str {
		match self {
			Posture::Lying => "lying",
			Posture::Reclined => "reclined",
			Posture::Sitting => "sitting",
			Posture::Standing => "standing",
			Posture::Upright => "upright",
			Posture::SittingOrLying => "sitting_or_lying",
			Posture::Still => "still",
			Posture::Walking => "walking",
			Posture::Running => "running",
		}
	}
}

/// Window length.
pub const WINDOW: SignedDuration = SignedDuration::from_secs(5);
/// RMS movement (m/s²) above which a window is walking.
pub const WALKING: f64 = 0.8;
/// RMS movement (m/s²) above which a window is running.
pub const RUNNING: f64 = 3.5;

/// One classified window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
	/// Window start.
	pub start: Timestamp,
	/// What it showed.
	pub posture: Posture,
}

fn tilt_degrees(gravity: [f64; 3], up: [f64; 3]) -> Option<f64> {
	let g = (gravity[0].powi(2) + gravity[1].powi(2) + gravity[2].powi(2)).sqrt();
	let u = (up[0].powi(2) + up[1].powi(2) + up[2].powi(2)).sqrt();
	if !(6.0..=13.5).contains(&g) || u == 0.0 {
		// Not resting in gravity: falling, in a lift, or bad data.
		return None;
	}
	let cos = (gravity[0] * up[0] + gravity[1] * up[1] + gravity[2] * up[2]) / (g * u);
	Some(cos.clamp(-1.0, 1.0).acos().to_degrees())
}

/// Classify one window of `[x, y, z]` rows.
pub fn classify(rows: &[[f64; 3]], placement: Placement, up: [f64; 3]) -> Option<Posture> {
	if rows.len() < 3 {
		return None;
	}
	let n = rows.len() as f64;
	let mut mean = [0.0; 3];
	for row in rows {
		for axis in 0..3 {
			mean[axis] += row[axis] / n;
		}
	}
	let movement = (rows
		.iter()
		.map(|row| {
			(0..3)
				.map(|axis| (row[axis] - mean[axis]).powi(2))
				.sum::<f64>()
		})
		.sum::<f64>()
		/ n)
		.sqrt();
	if movement > RUNNING {
		return Some(Posture::Running);
	}
	if movement > WALKING {
		return Some(Posture::Walking);
	}
	let tilt = tilt_degrees(mean, up)?;
	Some(match placement {
		Placement::Loose => Posture::Still,
		Placement::Torso if tilt < 35.0 => Posture::Upright,
		Placement::Torso if tilt < 60.0 => Posture::Reclined,
		Placement::Torso => Posture::Lying,
		Placement::Thigh if tilt < 45.0 => Posture::Standing,
		Placement::Thigh => Posture::SittingOrLying,
	})
}

/// Classify every whole window in `batches`, which must all come from one
/// sensor and carry `x`, `y`, `z` channels.
pub fn windows(batches: &[SeriesBatch], placement: Placement, up: [f64; 3]) -> Vec<Window> {
	let mut rows: Vec<(Timestamp, [f64; 3])> = Vec::new();
	for batch in batches {
		let (Some(x), Some(y), Some(z)) =
			(batch.channel("x"), batch.channel("y"), batch.channel("z"))
		else {
			continue;
		};
		rows.extend(batch.rows().map(|(t, row)| (t, [row[x], row[y], row[z]])));
	}
	rows.sort_by_key(|(t, _)| *t);
	let window_us = WINDOW.as_micros();
	let mut out = Vec::new();
	let mut current: Option<i128> = None;
	let mut bucket: Vec<[f64; 3]> = Vec::new();
	let flush = |slot: i128, bucket: &mut Vec<[f64; 3]>, out: &mut Vec<Window>| {
		if let Some(posture) = classify(bucket, placement, up)
			&& let Ok(start) =
				Timestamp::from_microsecond(i64::try_from(slot * window_us).unwrap_or(i64::MAX))
		{
			out.push(Window { start, posture });
		}
		bucket.clear();
	};
	for (t, row) in rows {
		let slot = i128::from(t.as_microsecond()).div_euclid(window_us);
		if current.is_some_and(|c| c != slot) {
			flush(current.expect("checked"), &mut bucket, &mut out);
		}
		current = Some(slot);
		bucket.push(row);
	}
	if let Some(slot) = current {
		flush(slot, &mut bucket, &mut out);
	}
	out
}

/// Combine a torso and a thigh reading taken at the same time.
pub fn fuse(torso: Posture, thigh: Posture) -> Posture {
	match (torso, thigh) {
		(Posture::Running, _) | (_, Posture::Running) => Posture::Running,
		(Posture::Walking, _) | (_, Posture::Walking) => Posture::Walking,
		(Posture::Lying, _) => Posture::Lying,
		(Posture::Upright | Posture::Reclined, Posture::SittingOrLying) => Posture::Sitting,
		(Posture::Upright, Posture::Standing) => Posture::Standing,
		(other, _) => other,
	}
}

/// Fuse torso and thigh windows that share a start.
pub fn fuse_windows(torso: &[Window], thigh: &[Window]) -> Vec<Window> {
	let mut out = Vec::with_capacity(torso.len());
	let mut legs = thigh.iter().peekable();
	for window in torso {
		while legs.peek().is_some_and(|leg| leg.start < window.start) {
			legs.next();
		}
		let posture = match legs.peek() {
			Some(leg) if leg.start == window.start => fuse(window.posture, leg.posture),
			_ => window.posture,
		};
		out.push(Window {
			start: window.start,
			posture,
		});
	}
	out
}

/// A run of one posture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
	/// Start.
	pub start: Timestamp,
	/// End.
	pub end: Timestamp,
	/// What it was.
	pub posture: Posture,
}

/// Merge consecutive windows into spans. Windows separated by a gap longer
/// than `max_gap` start a new span. A span shorter than `min_span` between two
/// spans of the same posture is absorbed (a shuffle while lying stays lying).
pub fn spans(windows: &[Window], max_gap: SignedDuration, min_span: SignedDuration) -> Vec<Span> {
	let mut spans: Vec<Span> = Vec::new();
	for window in windows {
		let end = window.start + WINDOW;
		match spans.last_mut() {
			Some(last)
				if last.posture == window.posture
					&& window.start.duration_since(last.end) <= max_gap =>
			{
				last.end = end;
			}
			_ => {
				spans.push(Span {
					start: window.start,
					end,
					posture: window.posture,
				});
			}
		}
	}
	let mut smoothed: Vec<Span> = Vec::new();
	let mut i = 0;
	while i < spans.len() {
		let span = spans[i];
		let short = span.end.duration_since(span.start) < min_span;
		if short
			&& let (Some(prev), Some(next)) = (smoothed.last_mut(), spans.get(i + 1))
			&& prev.posture == next.posture
			&& span.start.duration_since(prev.end) <= max_gap
			&& next.start.duration_since(span.end) <= max_gap
		{
			prev.end = next.end;
			i += 2;
			continue;
		}
		smoothed.push(span);
		i += 1;
	}
	smoothed
}

/// Turn spans into `motion.posture` records.
pub fn records(spans: &[Span], source: &StreamId, device: DeviceId) -> Vec<Record> {
	let stream: StreamId = "motion.posture".parse().expect("valid stream");
	spans
		.iter()
		.map(|span| {
			let start = span.start.to_string();
			let mut record = Record::new(stream.clone(), DERIVED_DEVICE)
				.starting(span.start)
				.ending(span.end)
				.with("posture", span.posture.as_str())
				.with("derived_from", source.as_str())
				.with("sensor_device", device.to_string());
			record.id = stable_id(&["posture", source.as_str(), &device.to_string(), &start]);
			record
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	const G: f64 = 9.81;

	fn still(gravity: [f64; 3], n: usize) -> Vec<[f64; 3]> {
		(0..n)
			.map(|i| {
				let wobble = if i % 2 == 0 { 0.05 } else { -0.05 };
				[gravity[0] + wobble, gravity[1], gravity[2] - wobble]
			})
			.collect()
	}

	fn moving(amplitude: f64, n: usize) -> Vec<[f64; 3]> {
		(0..n)
			.map(|i| {
				let phase = (i as f64) * 0.8;
				[amplitude * phase.sin(), G + amplitude * phase.cos(), 0.0]
			})
			.collect()
	}

	#[test]
	fn torso_tilt_separates_upright_reclined_and_lying() {
		let up = default_up();
		assert_eq!(
			classify(&still([0.0, G, 0.0], 50), Placement::Torso, up),
			Some(Posture::Upright)
		);
		let reclined = [
			0.0,
			G * 45f64.to_radians().cos(),
			G * 45f64.to_radians().sin(),
		];
		assert_eq!(
			classify(&still(reclined, 50), Placement::Torso, up),
			Some(Posture::Reclined)
		);
		assert_eq!(
			classify(&still([0.0, 0.0, G], 50), Placement::Torso, up),
			Some(Posture::Lying)
		);
	}

	#[test]
	fn movement_beats_tilt() {
		let up = default_up();
		assert_eq!(
			classify(&moving(1.5, 50), Placement::Torso, up),
			Some(Posture::Walking)
		);
		assert_eq!(
			classify(&moving(6.0, 50), Placement::Loose, up),
			Some(Posture::Running)
		);
		assert_eq!(
			classify(&still([0.0, 0.0, G], 50), Placement::Loose, up),
			Some(Posture::Still)
		);
	}

	#[test]
	fn free_fall_and_tiny_windows_are_unknown() {
		let up = default_up();
		assert_eq!(
			classify(&still([0.0, 1.0, 0.0], 50), Placement::Torso, up),
			None
		);
		assert_eq!(
			classify(&still([0.0, G, 0.0], 2), Placement::Torso, up),
			None
		);
	}

	#[test]
	fn torso_and_thigh_fuse_into_sitting_and_standing() {
		assert_eq!(
			fuse(Posture::Upright, Posture::SittingOrLying),
			Posture::Sitting
		);
		assert_eq!(fuse(Posture::Upright, Posture::Standing), Posture::Standing);
		assert_eq!(
			fuse(Posture::Lying, Posture::SittingOrLying),
			Posture::Lying
		);
		assert_eq!(fuse(Posture::Upright, Posture::Walking), Posture::Walking);
	}

	fn batch(start: Timestamp, rows: &[[f64; 3]], hz: u32) -> SeriesBatch {
		SeriesBatch {
			stream: "sensor.vest.accel".parse().unwrap(),
			device: DeviceId::new(),
			start,
			channels: vec!["x".into(), "y".into(), "z".into()],
			offsets_ms: (0..rows.len())
				.map(|i| u32::try_from(i).unwrap() * 1000 / hz)
				.collect(),
			values: rows.iter().flatten().copied().collect(),
		}
	}

	#[test]
	fn windows_become_smoothed_spans() {
		let t0: Timestamp = "2026-11-06T13:00:00Z".parse().unwrap();
		// Ten minutes lying, a 5 s shuffle, then ten more minutes lying,
		// then a minute walking. 10 Hz.
		let mut rows = still([0.0, 0.0, G], 6000);
		rows.extend(moving(1.5, 50));
		rows.extend(still([0.0, 0.0, G], 6000));
		rows.extend(moving(1.5, 600));
		let windows = windows(&[batch(t0, &rows, 10)], Placement::Torso, default_up());
		let spans = spans(
			&windows,
			SignedDuration::from_secs(30),
			SignedDuration::from_secs(20),
		);
		let labels: Vec<_> = spans.iter().map(|s| s.posture).collect();
		assert_eq!(labels, vec![Posture::Lying, Posture::Walking]);
		assert_eq!(spans[0].start, t0);
		assert!(spans[0].end.duration_since(spans[0].start) >= SignedDuration::from_mins(20));

		let records = records(
			&spans,
			&"sensor.vest.accel".parse().unwrap(),
			DeviceId::new(),
		);
		assert_eq!(records[0].data["posture"], "lying");
		assert_eq!(records[0].end, Some(spans[0].end));
	}

	#[test]
	fn gaps_split_spans() {
		let t0: Timestamp = "2026-11-06T13:00:00Z".parse().unwrap();
		let windows = [
			Window {
				start: t0,
				posture: Posture::Lying,
			},
			Window {
				start: t0 + SignedDuration::from_mins(10),
				posture: Posture::Lying,
			},
		];
		assert_eq!(
			spans(
				&windows,
				SignedDuration::from_secs(30),
				SignedDuration::from_secs(20)
			)
			.len(),
			2
		);
	}
}
