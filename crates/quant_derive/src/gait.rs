//! Gait: how someone walked, not just that they walked.
//!
//! Within a walking bout found by the posture classifier, the acceleration
//! magnitude minus gravity oscillates once per step — except on the thigh,
//! where the whole leg swings once per *stride*, so each of its peaks is two
//! steps. Counting peaks with a refractory period gives the cadence, the
//! autocorrelation at the dominant step lag tells a regular stride from an
//! uneven one, and the spread of the dynamic acceleration is the intensity.
//! When cadence falls more than 10% below the day's earlier bouts — the
//! afternoon shuffle — the bout where it dropped says so.

use std::fmt::Write;

use jiff::SignedDuration;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::StreamId;

use crate::DERIVED_DEVICE;
use crate::posture::Placement;
use crate::posture::Posture;
use crate::posture::WINDOW;
use crate::posture::Window;
use crate::posture::windows;
use crate::stable_id;

/// A bout must last this long to say anything about gait.
pub const MIN_BOUT: SignedDuration = SignedDuration::from_secs(30);
/// Walking windows closer together than this join into one bout.
pub const MAX_GAP: SignedDuration = WINDOW;
/// Smallest gap between counted peaks: a step can hardly repeat faster.
pub const REFRACTORY: f64 = 0.25;
/// Width of the moving average that tracks gravity on the magnitude.
pub const SMOOTH: f64 = 0.2;
/// A drop larger than this fraction of the earlier cadence is a slow-down.
pub const SLOWED: f64 = 0.10;
/// Cadence is watched over blocks of this length to find a slow-down.
pub const BLOCK: SignedDuration = SignedDuration::from_secs(20);
/// A shorter trailing block joins the one before it.
pub const MIN_BLOCK: SignedDuration = SignedDuration::from_secs(10);
/// Largest lag searched for the dominant step: 40 steps a minute.
pub const STEP_LAG: f64 = 1.5;

/// One walking bout with its step metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GaitBout {
	/// Start.
	pub start: Timestamp,
	/// End.
	pub end: Timestamp,
	/// Steps per minute.
	pub cadence: f64,
	/// Steps taken over the bout.
	pub steps: u32,
	/// Autocorrelation at the dominant step lag, 0 to 1.
	pub regularity: f64,
	/// RMS of the dynamic acceleration, m/s².
	pub intensity: f64,
	/// Where the sensor was worn.
	pub placement: Placement,
	/// Whether cadence dropped more than 10% partway through the bout.
	pub slowed: bool,
	/// Whether most of the bout was running rather than walking.
	pub ran: bool,
}

impl GaitBout {
	/// Rebuild a bout from its stored record. Only what the slow-down trend
	/// reads — the cadence and the start — is faithful; the rest is best
	/// effort, and the step counts are not kept.
	pub fn stored(record: &Record) -> Option<GaitBout> {
		Some(GaitBout {
			start: record.start,
			end: record.end.unwrap_or(record.start),
			cadence: record.data.get("cadence")?.as_f64()?,
			steps: 0,
			regularity: record
				.data
				.get("regularity")
				.and_then(serde_json::Value::as_f64)
				.unwrap_or_default(),
			intensity: record
				.data
				.get("intensity")
				.and_then(serde_json::Value::as_f64)
				.unwrap_or_default(),
			placement: serde_json::from_value(record.data.get("placement")?.clone()).ok()?,
			slowed: record
				.data
				.get("slowed")
				.and_then(serde_json::Value::as_bool)
				.unwrap_or_default(),
			ran: false,
		})
	}
}

/// Steps one acceleration peak stands for: a thigh swings the whole leg once
/// per stride, so its peaks are strides; anywhere else a peak is a step.
fn steps_per_peak(placement: Placement) -> u32 {
	match placement {
		Placement::Thigh => 2,
		Placement::Torso | Placement::Loose => 1,
	}
}

/// A time in seconds as a whole number of samples, at least one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn samples(seconds: f64, rate: f64) -> usize {
	(seconds * rate).round().max(1.0) as usize
}

/// The magnitude of every `[x, y, z]` row, oldest first.
fn magnitudes(batches: &[SeriesBatch]) -> Vec<(Timestamp, f64)> {
	let mut rows: Vec<(Timestamp, f64)> = Vec::new();
	for batch in batches {
		let (Some(x), Some(y), Some(z)) =
			(batch.channel("x"), batch.channel("y"), batch.channel("z"))
		else {
			continue;
		};
		rows.extend(batch.rows().map(|(t, row)| {
			(
				t,
				(row[x] * row[x] + row[y] * row[y] + row[z] * row[z]).sqrt(),
			)
		}));
	}
	rows.sort_by_key(|(t, _)| *t);
	rows
}

/// Samples per second, from how tightly the rows sit together.
fn sample_rate(samples: &[(Timestamp, f64)]) -> f64 {
	let (Some((first, _)), Some((last, _))) = (samples.first(), samples.last()) else {
		return 0.0;
	};
	let seconds = last.duration_since(*first).as_secs_f64();
	if seconds <= 0.0 {
		return 0.0;
	}
	(samples.len() - 1) as f64 / seconds
}

/// The rate and the dynamic acceleration (magnitude minus a moving average
/// that tracks gravity) of a stretch of samples.
fn dynamic_of(samples: &[(Timestamp, f64)]) -> (f64, Vec<f64>) {
	let rate = sample_rate(samples);
	let values: Vec<f64> = samples.iter().map(|(_, m)| *m).collect();
	(rate, detrend(&values, rate))
}

/// Subtract a moving average (gravity and slow drift) from the magnitude,
/// leaving the dynamic acceleration of steps and strides.
fn detrend(values: &[f64], rate: f64) -> Vec<f64> {
	let half = samples(SMOOTH, rate) / 2;
	let mut sums = vec![0.0; values.len() + 1];
	for (i, value) in values.iter().enumerate() {
		sums[i + 1] = sums[i] + value;
	}
	values
		.iter()
		.enumerate()
		.map(|(i, value)| {
			let lo = i.saturating_sub(half);
			let hi = (i + half).min(values.len() - 1);
			let n = (hi - lo + 1) as f64;
			value - (sums[hi + 1] - sums[lo]) / n
		})
		.collect()
}

/// Index the peaks of the dynamic acceleration: local maxima above half its
/// RMS, no closer than the refractory period. Within the refractory period
/// the larger peak wins.
fn detect_peaks(values: &[f64], rate: f64) -> Vec<usize> {
	let n = values.len();
	if n < 3 {
		return Vec::new();
	}
	let rms = (values.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
	let threshold = rms / 2.0;
	let refractory = samples(REFRACTORY, rate);
	let mut peaks: Vec<usize> = Vec::new();
	for i in 1..n - 1 {
		if values[i] <= threshold || values[i] <= values[i - 1] || values[i] < values[i + 1] {
			continue;
		}
		match peaks.last() {
			Some(&last) if i - last < refractory => {
				if values[i] > values[last] {
					*peaks.last_mut().expect("checked") = i;
				}
			}
			_ => peaks.push(i),
		}
	}
	peaks
}

/// Autocorrelation at the dominant step lag: 1 is metronome regular, 0 is
/// indistinguishable from noise.
fn regularity(values: &[f64], rate: f64) -> f64 {
	let n = values.len();
	if n < 4 {
		return 0.0;
	}
	let mean = values.iter().sum::<f64>() / n as f64;
	let centered: Vec<f64> = values.iter().map(|v| v - mean).collect();
	let energy: f64 = centered.iter().map(|v| v * v).sum();
	if energy <= 0.0 {
		return 0.0;
	}
	let min_lag = samples(REFRACTORY, rate).clamp(1, n - 2);
	let max_lag = samples(STEP_LAG, rate).clamp(min_lag, n - 2);
	let mut best = 0.0f64;
	for lag in min_lag..=max_lag {
		let overlap = n - lag;
		if overlap < min_lag {
			break;
		}
		let dot: f64 = centered[..overlap]
			.iter()
			.zip(centered[lag..].iter())
			.map(|(a, b)| a * b)
			.sum();
		best = best.max(dot / energy);
	}
	best.clamp(0.0, 1.0)
}

/// Step metrics for one stretch of samples. The cadence divides the peaks by
/// `start..end`; the regularity and intensity read the samples themselves.
fn analyze(
	samples: &[(Timestamp, f64)],
	start: Timestamp,
	end: Timestamp,
	placement: Placement,
) -> Option<(f64, u32, f64, f64)> {
	if samples.len() < 4 || end <= start {
		return None;
	}
	let (rate, dynamic) = dynamic_of(samples);
	if rate <= 0.0 {
		return None;
	}
	let peaks = detect_peaks(&dynamic, rate);
	let factor = f64::from(steps_per_peak(placement));
	let cadence = factor * peaks.len() as f64 / end.duration_since(start).as_secs_f64() * 60.0;
	let intensity = (dynamic.iter().map(|v| v * v).sum::<f64>() / dynamic.len() as f64).sqrt();
	let steps =
		u32::try_from(peaks.len() * usize::try_from(steps_per_peak(placement)).unwrap_or(1))
			.unwrap_or(u32::MAX);
	Some((cadence, steps, regularity(&dynamic, rate), intensity))
}

/// Median of an unsorted slice.
fn median(values: &mut [f64]) -> f64 {
	values.sort_by(f64::total_cmp);
	let mid = values.len() / 2;
	if values.len() % 2 == 0 {
		values[mid - 1].midpoint(values[mid])
	} else {
		values[mid]
	}
}

/// Cadence over consecutive blocks of a bout, to find where a walk slowed.
/// A trailing block shorter than `MIN_BLOCK` joins the one before it.
fn block_cadences(
	samples: &[(Timestamp, f64)],
	peaks: &[usize],
	start: Timestamp,
	end: Timestamp,
	placement: Placement,
) -> Vec<(Timestamp, f64)> {
	let factor = f64::from(steps_per_peak(placement));
	let mut blocks: Vec<(Timestamp, Timestamp, usize)> = Vec::new();
	let mut cursor = start;
	while cursor < end {
		let next = (cursor + BLOCK).min(end);
		let lo = peaks.partition_point(|&i| samples[i].0 < cursor);
		let hi = peaks.partition_point(|&i| samples[i].0 < next);
		blocks.push((cursor, next, hi - lo));
		cursor = next;
	}
	let short_tail = matches!(blocks.last(),
		Some(last) if blocks.len() >= 2 && last.1.duration_since(last.0) < MIN_BLOCK);
	if short_tail {
		let last = blocks.pop().expect("checked");
		let previous = blocks.last_mut().expect("checked");
		previous.1 = last.1;
		previous.2 += last.2;
	}
	blocks
		.into_iter()
		.map(|(start, end, count)| {
			(
				start,
				factor * count as f64 / end.duration_since(start).as_secs_f64() * 60.0,
			)
		})
		.collect()
}

/// Where a bout's cadence fell more than `SLOWED` below its earlier blocks,
/// if the pieces either side are long enough to stand as bouts on their own.
fn split_point(blocks: &[(Timestamp, f64)], start: Timestamp, end: Timestamp) -> Option<Timestamp> {
	for i in 1..blocks.len() {
		let baseline = median(&mut blocks[..i].iter().map(|(_, c)| *c).collect::<Vec<_>>());
		let low = |cadence: f64| cadence < baseline * (1.0 - SLOWED);
		let confirmed = i + 1 == blocks.len() || low(blocks[i + 1].1);
		if low(blocks[i].1) && confirmed {
			let at = blocks[i].0;
			if at.duration_since(start) >= MIN_BOUT && end.duration_since(at) >= MIN_BOUT {
				return Some(at);
			}
		}
	}
	None
}

/// Whether most of `[start, end)` was running rather than walking.
fn mostly_running(active: &[&Window], start: Timestamp, end: Timestamp) -> bool {
	let lo = active.partition_point(|w| w.start < start);
	let hi = active.partition_point(|w| w.start < end);
	let mut total = 0;
	let mut running = 0;
	for window in &active[lo..hi] {
		total += 1;
		running += usize::from(window.posture == Posture::Running);
	}
	total > 0 && running * 2 > total
}

/// One piece of a bout: sample range, window bounds, and whether it is the
/// slower tail of a split.
struct Piece {
	from: usize,
	to: usize,
	start: Timestamp,
	end: Timestamp,
	slowed: bool,
}

/// Find the walking bouts in `batches`. Walking or running windows merge into
/// bouts of at least `MIN_BOUT`; a bout whose cadence drops more than 10%
/// partway through splits there, the slower piece marked `slowed`.
pub fn bouts(batches: &[SeriesBatch], placement: Placement, up: [f64; 3]) -> Vec<GaitBout> {
	let windows = windows(batches, placement, up);
	let active: Vec<&Window> = windows
		.iter()
		.filter(|w| matches!(w.posture, Posture::Walking | Posture::Running))
		.collect();
	let mut raw: Vec<(Timestamp, Timestamp)> = Vec::new();
	for window in &active {
		let end = window.start + WINDOW;
		match raw.last_mut() {
			Some(last) if window.start.duration_since(last.1) <= MAX_GAP => last.1 = end,
			_ => raw.push((window.start, end)),
		}
	}
	let samples = magnitudes(batches);
	let mut out = Vec::new();
	for (start, end) in raw {
		if end.duration_since(start) < MIN_BOUT {
			continue;
		}
		let lo = samples.partition_point(|(t, _)| *t < start);
		let hi = samples.partition_point(|(t, _)| *t < end);
		let mine = &samples[lo..hi];
		if mine.len() < 4 {
			continue;
		}
		let (rate, dynamic) = dynamic_of(mine);
		let peaks = detect_peaks(&dynamic, rate);
		let split = split_point(
			&block_cadences(mine, &peaks, start, end, placement),
			start,
			end,
		);
		let pieces = match split {
			Some(at) => {
				let mid = mine.partition_point(|(t, _)| *t < at);
				vec![
					Piece {
						from: 0,
						to: mid,
						start,
						end: at,
						slowed: false,
					},
					Piece {
						from: mid,
						to: mine.len(),
						start: at,
						end,
						slowed: true,
					},
				]
			}
			None => {
				vec![Piece {
					from: 0,
					to: mine.len(),
					start,
					end,
					slowed: false,
				}]
			}
		};
		for piece in pieces {
			let samples = &mine[piece.from..piece.to];
			let Some((cadence, steps, regular, intensity)) =
				analyze(samples, piece.start, piece.end, placement)
			else {
				continue;
			};
			out.push(GaitBout {
				start: piece.start,
				end: piece.end,
				cadence,
				steps,
				regularity: regular,
				intensity,
				placement,
				slowed: piece.slowed,
				ran: mostly_running(&active, piece.start, piece.end),
			});
		}
	}
	out
}

/// Compare the last bout against the median cadence of the bouts before it.
/// A drop of more than 10% gives a sentence like "Cadence fell from 114 to
/// 98 steps/min after 15:10.", with the clock time in `tz`.
pub fn trend(bouts: &[GaitBout], tz: &TimeZone) -> Option<String> {
	let (last, earlier) = bouts.split_last()?;
	let mut cadences: Vec<f64> = earlier
		.iter()
		.map(|bout| bout.cadence)
		.filter(|cadence| *cadence > 0.0)
		.collect();
	if cadences.is_empty() {
		return None;
	}
	let baseline = median(&mut cadences);
	if last.cadence >= baseline * (1.0 - SLOWED) {
		return None;
	}
	let clock = last.start.to_zoned(tz.clone());
	Some(format!(
		"Cadence fell from {:.0} to {:.0} steps/min after {:02}:{:02}.",
		baseline,
		last.cadence,
		clock.hour(),
		clock.minute()
	))
}

/// A bout as a sentence: "Walked 12 min, 112 steps/min, regular stride
/// (0.86)."
fn describe(bout: &GaitBout, note: Option<&str>) -> String {
	let seconds = bout.end.duration_since(bout.start).as_secs_f64();
	let mut text = if bout.ran { "Ran" } else { "Walked" }.to_owned();
	if seconds < 45.0 {
		let _ = write!(text, " {seconds:.0} s");
	} else {
		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
		let minutes = (seconds / 60.0).round().max(1.0) as u64;
		if minutes < 60 {
			let _ = write!(text, " {minutes} min");
		} else {
			let _ = write!(text, " {}h {:02} min", minutes / 60, minutes % 60);
		}
	}
	let stride = if bout.regularity >= 0.7 {
		"regular"
	} else {
		"uneven"
	};
	let _ = write!(
		text,
		", {:.0} steps/min, {} stride ({:.2}).",
		bout.cadence, stride, bout.regularity
	);
	if let Some(note) = note {
		text.push(' ');
		text.push_str(note);
	}
	text
}

/// Turn bouts into `motion.gait` records. `earlier` holds the bouts that came
/// before `bouts` that day — read back from the store on a recompute — so the
/// slow-down trend reaches past the recomputed stretch.
pub fn records(
	bouts: &[GaitBout],
	earlier: &[GaitBout],
	source: &StreamId,
	device: DeviceId,
	tz: &TimeZone,
) -> Vec<Record> {
	let stream: StreamId = "motion.gait".parse().expect("valid stream");
	let mut history: Vec<GaitBout> = earlier.to_vec();
	bouts
		.iter()
		.map(|bout| {
			history.push(*bout);
			let mut bout = *bout;
			let note = trend(&history, tz);
			if note.is_some() {
				bout.slowed = true;
			}
			let placement = match bout.placement {
				Placement::Torso => "torso",
				Placement::Thigh => "thigh",
				Placement::Loose => "loose",
			};
			let start = bout.start.to_string();
			let mut record = Record::new(stream.clone(), DERIVED_DEVICE)
				.starting(bout.start)
				.ending(bout.end)
				.with_text(describe(&bout, note.as_deref()))
				.with("cadence", bout.cadence)
				.with("steps", bout.steps)
				.with("regularity", bout.regularity)
				.with("intensity", bout.intensity)
				.with("placement", placement)
				.with("derived_from", source.as_str())
				.with("sensor_device", device.to_string());
			if bout.slowed {
				record = record.with("slowed", true);
			}
			record.id = stable_id(&["gait", source.as_str(), &device.to_string(), &start]);
			record
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	const G: f64 = 9.81;
	const UP: [f64; 3] = [0.0, 1.0, 0.0];

	/// A tiny deterministic PRNG, so the tests need no new dependency.
	struct Lcg(u64);

	impl Lcg {
		fn noise(&mut self) -> f64 {
			self.0 = self
				.0
				.wrapping_mul(6_364_136_223_846_793_005)
				.wrapping_add(1_442_695_040_888_963_407);
			((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
		}
	}

	/// A 50 Hz accelerometer: `[segments]` are `(seconds, step frequency in
	/// Hz)` runs, all at `amplitude` on the up axis, with a little noise.
	fn signal(start: Timestamp, segments: &[(u32, f64)], amplitude: f64) -> SeriesBatch {
		let hz = 50;
		let mut rng = Lcg(0x2545_F491_4F6C_DD1D);
		let mut values = Vec::new();
		let mut offsets_ms = Vec::new();
		let mut ms = 0u32;
		for &(seconds, step_hz) in segments {
			for i in 0..seconds * hz {
				let offset = ms + i * 1000 / hz;
				let t = f64::from(offset) / 1000.0;
				values.extend([
					rng.noise() * 0.05,
					G + amplitude * (2.0 * std::f64::consts::PI * step_hz * t).sin()
						+ rng.noise() * 0.05,
					rng.noise() * 0.05,
				]);
				offsets_ms.push(offset);
			}
			ms += seconds * 1000;
		}
		SeriesBatch {
			stream: "motion.accelerometer".parse().unwrap(),
			device: DeviceId::new(),
			start,
			channels: vec!["x".into(), "y".into(), "z".into()],
			offsets_ms,
			values,
		}
	}

	fn bout(start: Timestamp, cadence: f64) -> GaitBout {
		GaitBout {
			start,
			end: start + SignedDuration::from_mins(10),
			cadence,
			steps: 1080,
			regularity: 0.9,
			intensity: 1.0,
			placement: Placement::Torso,
			slowed: false,
			ran: false,
		}
	}

	#[test]
	fn a_steady_walk_gives_cadence_and_a_regular_stride() {
		let t0: Timestamp = "2026-11-06T13:00:00Z".parse().unwrap();
		let bouts = bouts(&[signal(t0, &[(60, 1.8)], 1.5)], Placement::Torso, UP);
		assert_eq!(bouts.len(), 1, "{bouts:?}");
		assert!(
			(bouts[0].cadence - 108.0).abs() < 3.0,
			"{} steps/min",
			bouts[0].cadence
		);
		assert!(bouts[0].regularity > 0.7, "{:?}", bouts[0].regularity);
		assert_eq!(bouts[0].steps, 108);

		let source: StreamId = "motion.accelerometer".parse().unwrap();
		let records = records(&bouts, &[], &source, DeviceId::new(), &TimeZone::UTC);
		let text = records[0].text.as_deref().unwrap();
		assert!(
			text.contains("Walked 1 min, 108 steps/min, regular stride"),
			"{text}"
		);
		assert!(records[0].data.get("slowed").is_none());
	}

	#[test]
	fn still_signals_have_no_bouts() {
		let t0: Timestamp = "2026-11-06T13:00:00Z".parse().unwrap();
		let idle = [(60u32, 0.0)];
		assert!(bouts(&[signal(t0, &idle, 0.0)], Placement::Torso, UP).is_empty());
		assert!(bouts(&[signal(t0, &idle, 0.0)], Placement::Loose, UP).is_empty());
	}

	#[test]
	fn a_walk_that_slows_splits_into_a_slowed_bout() {
		let t0: Timestamp = "2026-11-06T14:00:00Z".parse().unwrap();
		let bouts = bouts(
			&[signal(t0, &[(60, 1.8), (90, 1.4)], 1.5)],
			Placement::Torso,
			UP,
		);
		assert_eq!(bouts.len(), 2, "{bouts:?}");
		assert!((bouts[0].cadence - 108.0).abs() < 3.0, "{:?}", bouts[0]);
		assert!((bouts[1].cadence - 84.0).abs() < 3.0, "{:?}", bouts[1]);
		assert!(!bouts[0].slowed);
		assert!(bouts[1].slowed);

		let source: StreamId = "motion.accelerometer".parse().unwrap();
		let records = records(&bouts, &[], &source, DeviceId::new(), &TimeZone::UTC);
		let tail = records[1].text.as_deref().unwrap();
		assert!(
			tail.contains("Cadence fell from 108 to 84 steps/min after 14:01"),
			"{tail}"
		);
		assert_eq!(records[1].data["slowed"], true);
		assert!(records[0].data.get("slowed").is_none());
	}

	#[test]
	fn thigh_peaks_count_as_strides() {
		let t0: Timestamp = "2026-11-06T13:00:00Z".parse().unwrap();
		let walk = signal(t0, &[(60, 1.8)], 1.5);
		let torso = bouts(std::slice::from_ref(&walk), Placement::Torso, UP);
		let thigh = bouts(&[walk], Placement::Thigh, UP);
		assert_eq!(thigh.len(), 1);
		// The same peaks, but each one is a stride of two steps.
		assert!(
			(thigh[0].cadence - torso[0].cadence * 2.0).abs() < 0.01,
			"{:?} vs {:?}",
			thigh[0],
			torso[0]
		);
		assert_eq!(thigh[0].steps, torso[0].steps * 2);
	}

	#[test]
	fn trend_needs_a_drop_and_history() {
		let morning: Timestamp = "2026-11-06T10:00:00Z".parse().unwrap();
		let afternoon: Timestamp = "2026-11-06T15:10:00Z".parse().unwrap();
		// One bout alone has nothing to fall from.
		assert_eq!(trend(&[bout(morning, 114.0)], &TimeZone::UTC), None);
		assert_eq!(
			trend(
				&[bout(morning, 114.0), bout(afternoon, 110.0)],
				&TimeZone::UTC
			),
			None
		);
		assert_eq!(
			trend(
				&[bout(morning, 98.0), bout(afternoon, 114.0)],
				&TimeZone::UTC
			),
			None
		);
		let note = trend(
			&[bout(morning, 114.0), bout(afternoon, 98.0)],
			&TimeZone::UTC,
		)
		.unwrap();
		assert_eq!(note, "Cadence fell from 114 to 98 steps/min after 15:10.");
	}
}
