//! Getting data in from hardware Quant has never heard of: register a
//! stream, say where an accelerometer is worn, and pipe samples in.

use std::io::BufRead;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Subcommand;
use clap::ValueEnum;
use jiff::SignedDuration;
use jiff::Timestamp;
use quant_api::IngestRecords;
use quant_api::IngestSeries;
use quant_client::Client;
use quant_core::Channel;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::StreamId;
use quant_core::StreamKind;
use quant_core::StreamSpec;
use serde_json::Value;

use crate::Ctx;

/// `quant stream …`
#[derive(Subcommand)]
pub enum StreamAction {
	/// Register a custom stream, such as `sensor.vest.ecg` (asks for the
	/// passphrase).
	Add {
		/// The stream id; custom hardware goes under `sensor.`.
		id: String,
		/// A channel as `name:unit`, in column order. Repeat for each.
		#[arg(long = "channel")]
		channels: Vec<String>,
		/// What the stream means, for agents.
		#[arg(long)]
		description: String,
		/// Discrete records rather than numeric samples.
		#[arg(long)]
		event: bool,
	},
}

/// `quant derive …`
#[derive(Subcommand)]
pub enum DeriveAction {
	/// Say where an accelerometer stream is worn, so posture can be read
	/// from it (asks for the passphrase).
	Posture {
		/// The series stream, such as `sensor.vest.accel`.
		#[arg(long)]
		stream: String,
		/// Where it is worn.
		#[arg(long, value_enum)]
		placement: Placement,
		/// The sensor axis pointing up when standing, as `x,y,z`.
		#[arg(long, default_value = "0,1,0")]
		up: String,
		/// Stop reading posture from this stream.
		#[arg(long)]
		remove: bool,
	},
	/// Show the derivation settings (asks for the passphrase).
	Show,
}

/// Where a sensor is worn.
#[derive(Clone, Copy, ValueEnum)]
pub enum Placement {
	/// Chest strap, vest.
	Torso,
	/// Upper leg.
	Thigh,
	/// A phone or anything loose.
	Loose,
}

/// Input formats for `quant ingest`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
	/// `timestamp,value,value,…`; a non-numeric first line names the
	/// channels.
	Csv,
	/// One JSON object per line: `{"t": …, "<channel>": value, …}`, or with
	/// `--records`, `{"t": …, "end": …, "text": …, "data": {…}}`.
	Ndjson,
}

pub async fn stream(cx: &Ctx, action: StreamAction) -> Result<()> {
	let StreamAction::Add {
		id,
		channels,
		description,
		event,
	} = action;
	let channels = channels
		.iter()
		.map(|c| {
			let (name, unit) = c
				.split_once(':')
				.with_context(|| format!("channel `{c}` should be name:unit"))?;
			Ok(Channel::new(name, unit))
		})
		.collect::<Result<Vec<_>>>()?;
	if !event && channels.is_empty() {
		bail!("a series stream needs at least one --channel name:unit (or pass --event)");
	}
	let spec = StreamSpec {
		id: id.parse().map_err(|e| anyhow::anyhow!("{e}"))?,
		kind: if event {
			StreamKind::Event
		} else {
			StreamKind::Series
		},
		channels,
		description,
		media_retention_hours: None,
		summary_keys: Vec::new(),
		classes: Vec::new(),
	};
	let stored = cx.owner().await?.register_stream(&spec).await?;
	println!(
		"Registered {}. Devices need `{}:write` to send it.",
		stored.id, stored.id
	);
	Ok(())
}

pub async fn derive(cx: &Ctx, action: DeriveAction) -> Result<()> {
	let owner = cx.owner().await?;
	let mut config = owner.derive_config().await?;
	if let DeriveAction::Posture {
		stream,
		placement,
		up,
		remove,
	} = action
	{
		let up: Vec<f64> = up
			.split(',')
			.map(|v| v.trim().parse())
			.collect::<Result<_, _>>()
			.context("--up should be x,y,z")?;
		if up.len() != 3 {
			bail!("--up should be x,y,z");
		}
		let sources = config["posture_sources"]
			.as_array_mut()
			.context("unexpected settings shape")?;
		sources.retain(|s| s["stream"] != stream.as_str());
		if !remove {
			let placement = match placement {
				Placement::Torso => "torso",
				Placement::Thigh => "thigh",
				Placement::Loose => "loose",
			};
			sources.push(serde_json::json!({ "stream": stream, "placement": placement, "up": up }));
		}
		config = owner.set_derive_config(&config).await?;
	}
	println!("{}", serde_json::to_string_pretty(&config)?);
	Ok(())
}

/// One parsed input line.
#[derive(Debug, PartialEq)]
enum Line {
	/// Column names.
	Header(Vec<String>),
	/// A sample.
	Row(Timestamp, Vec<f64>),
}

fn parse_time(value: &str, now: Timestamp) -> Result<Timestamp> {
	let value = value.trim().trim_matches('"');
	if value.is_empty() {
		return Ok(now);
	}
	if let Ok(number) = value.parse::<f64>() {
		// Unix seconds or milliseconds, whichever is plausible.
		#[allow(clippy::cast_possible_truncation)]
		let millis = if number < 1e11 {
			(number * 1000.0) as i64
		} else {
			number as i64
		};
		return Timestamp::from_millisecond(millis).context("timestamp out of range");
	}
	quant_client::when::parse(value, now).map_err(anyhow::Error::msg)
}

fn json_time(value: Option<&Value>, now: Timestamp) -> Result<Timestamp> {
	match value {
		None | Some(Value::Null) => Ok(now),
		Some(Value::Number(n)) => parse_time(&n.to_string(), now),
		Some(Value::String(s)) => parse_time(s, now),
		Some(other) => bail!("bad timestamp {other}"),
	}
}

fn parse_csv(line: &str, now: Timestamp) -> Result<Line> {
	let fields: Vec<&str> = line.split(',').map(str::trim).collect();
	let values: Result<Vec<f64>, _> = fields[1..].iter().map(|f| f.parse::<f64>()).collect();
	match values {
		Ok(values) if !values.is_empty() => Ok(Line::Row(parse_time(fields[0], now)?, values)),
		_ if fields.len() > 1 => {
			Ok(Line::Header(
				fields[1..].iter().map(|f| (*f).to_owned()).collect(),
			))
		}
		_ => bail!("expected timestamp,value,…"),
	}
}

fn parse_ndjson(line: &str, channels: &[String], now: Timestamp) -> Result<Line> {
	let value: Value = serde_json::from_str(line)?;
	let t = json_time(value.get("t").or_else(|| value.get("time")), now)?;
	if let Some(values) = value.get("values").and_then(Value::as_array) {
		return Ok(Line::Row(
			t,
			values
				.iter()
				.map(|v| v.as_f64().context("non-numeric value"))
				.collect::<Result<_>>()?,
		));
	}
	let values = channels
		.iter()
		.map(|c| {
			value
				.get(c)
				.and_then(Value::as_f64)
				.with_context(|| format!("missing channel `{c}`"))
		})
		.collect::<Result<_>>()?;
	Ok(Line::Row(t, values))
}

/// Accumulates rows into batches of at most `span` or `max_rows`.
struct Batcher {
	stream: StreamId,
	device: DeviceId,
	channels: Vec<String>,
	span: SignedDuration,
	max_rows: usize,
	current: Option<SeriesBatch>,
}

impl Batcher {
	fn push(&mut self, t: Timestamp, values: Vec<f64>) -> Result<Option<SeriesBatch>> {
		if values.len() != self.channels.len() {
			bail!(
				"expected {} values ({}), got {}",
				self.channels.len(),
				self.channels.join(","),
				values.len()
			);
		}
		let full = self.current.as_ref().is_some_and(|b| {
			b.len() >= self.max_rows || t < b.start || t.duration_since(b.start) >= self.span
		});
		let done = if full { self.current.take() } else { None };
		let batch = self.current.get_or_insert_with(|| {
			SeriesBatch {
				stream: self.stream.clone(),
				device: self.device,
				start: t,
				channels: self.channels.clone(),
				offsets_ms: Vec::new(),
				values: Vec::new(),
			}
		});
		let offset =
			u32::try_from(t.duration_since(batch.start).as_millis()).context("batch too long")?;
		batch.offsets_ms.push(offset);
		batch.values.extend(values);
		Ok(done)
	}

	fn finish(&mut self) -> Option<SeriesBatch> {
		self.current.take()
	}
}

fn record_line(line: &str, stream: &StreamId, device: DeviceId, now: Timestamp) -> Result<Record> {
	let value: Value = serde_json::from_str(line)?;
	if value.get("id").is_some() && value.get("stream").is_some() {
		return Ok(serde_json::from_value(value)?);
	}
	let mut record = Record::new(stream.clone(), device).starting(json_time(value.get("t"), now)?);
	if let Some(end) = value.get("end") {
		record = record.ending(json_time(Some(end), now)?);
	}
	if let Some(text) = value.get("text").and_then(Value::as_str) {
		record = record.with_text(text);
	}
	if let Some(Value::Object(data)) = value.get("data") {
		record.data.clone_from(data);
	}
	Ok(record)
}

/// Send batches, waiting and retrying while the grant is rate limited.
async fn send_series(client: &Client, batches: Vec<SeriesBatch>) -> Result<quant_api::Ingested> {
	let request = IngestSeries { batches };
	let mut wait = std::time::Duration::from_secs(1);
	loop {
		match client.ingest_series(&request).await {
			Err(error)
				if error.code() == Some("rate_limited")
					&& wait <= std::time::Duration::from_secs(64) =>
			{
				eprintln!("rate limited; retrying in {}s", wait.as_secs());
				tokio::time::sleep(wait).await;
				wait *= 2;
			}
			other => return Ok(other?),
		}
	}
}

/// Options for `quant ingest`.
pub struct Ingest {
	pub stream: String,
	pub format: Format,
	pub channels: Vec<String>,
	pub records: bool,
	pub batch_seconds: u64,
}

pub async fn ingest(client: &Client, options: Ingest) -> Result<()> {
	let stream: StreamId = options.stream.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
	let id = client
		.identity()
		.client
		.context("this profile has not paired")?;
	let device = DeviceId(id.0);
	let stdin = std::io::stdin();
	let (mut sent, mut duplicates) = (0usize, 0usize);

	if options.records {
		let mut pending = Vec::new();
		for line in stdin.lock().lines() {
			let line = line?;
			if line.trim().is_empty() {
				continue;
			}
			pending.push(record_line(&line, &stream, device, Timestamp::now())?);
			if pending.len() >= 500 {
				let answer = client
					.ingest_records(&IngestRecords {
						records: std::mem::take(&mut pending),
						device_name: None,
						platform: Some("cli".into()),
					})
					.await?;
				sent += answer.stored;
				duplicates += answer.duplicate;
			}
		}
		if !pending.is_empty() {
			let answer = client
				.ingest_records(&IngestRecords {
					records: pending,
					device_name: None,
					platform: Some("cli".into()),
				})
				.await?;
			sent += answer.stored;
			duplicates += answer.duplicate;
		}
		eprintln!("Stored {sent} records ({duplicates} already present).");
		return Ok(());
	}

	let mut batcher = Batcher {
		stream,
		device,
		channels: options.channels,
		span: SignedDuration::from_secs(i64::try_from(options.batch_seconds.max(1)).unwrap_or(1)),
		max_rows: 5_000,
		current: None,
	};
	let mut ready = Vec::new();
	let mut last_send = std::time::Instant::now();
	for line in stdin.lock().lines() {
		let line = line?;
		if line.trim().is_empty() || line.starts_with('#') {
			continue;
		}
		let now = Timestamp::now();
		let parsed = match options.format {
			Format::Csv => parse_csv(&line, now)?,
			Format::Ndjson => parse_ndjson(&line, &batcher.channels, now)?,
		};
		match parsed {
			Line::Header(names) => batcher.channels = names,
			Line::Row(t, values) => {
				if batcher.channels.is_empty() {
					bail!("name the channels with --channels or a CSV header line");
				}
				if let Some(batch) = batcher.push(t, values)? {
					ready.push(batch);
				}
			}
		}
		// Send in bulk: every couple of seconds for a live feed, or once
		// enough has piled up when replaying a file.
		if ready.len() >= 200
			|| (!ready.is_empty() && last_send.elapsed() >= std::time::Duration::from_secs(2))
		{
			let answer = send_series(client, std::mem::take(&mut ready)).await?;
			sent += answer.stored;
			duplicates += answer.duplicate;
			last_send = std::time::Instant::now();
		}
	}
	ready.extend(batcher.finish());
	if !ready.is_empty() {
		let answer = send_series(client, ready).await?;
		sent += answer.stored;
		duplicates += answer.duplicate;
	}
	eprintln!("Stored {sent} batches ({duplicates} already present).");
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn now() -> Timestamp {
		"2026-11-06T12:00:00Z".parse().unwrap()
	}

	#[test]
	fn csv_headers_and_rows() {
		assert_eq!(
			parse_csv("t,x,y,z", now()).unwrap(),
			Line::Header(vec!["x".into(), "y".into(), "z".into()])
		);
		let Line::Row(t, values) = parse_csv("1762430400000,0.1,9.8,0.2", now()).unwrap() else {
			panic!()
		};
		assert_eq!(t.as_millisecond(), 1_762_430_400_000);
		assert_eq!(values, vec![0.1, 9.8, 0.2]);
		let Line::Row(t, _) = parse_csv("1762430400.5,1", now()).unwrap() else {
			panic!()
		};
		assert_eq!(t.as_millisecond(), 1_762_430_400_500);
		let Line::Row(t, _) = parse_csv(",72", now()).unwrap() else {
			panic!()
		};
		assert_eq!(t, now());
	}

	#[test]
	fn ndjson_by_channel_name_or_values() {
		let channels = vec!["bpm".to_owned()];
		let Line::Row(t, values) =
			parse_ndjson(r#"{"t":"2026-11-06T11:00:00Z","bpm":61}"#, &channels, now()).unwrap()
		else {
			panic!()
		};
		assert_eq!(t.to_string(), "2026-11-06T11:00:00Z");
		assert_eq!(values, vec![61.0]);
		let Line::Row(_, values) = parse_ndjson(r#"{"values":[1,2]}"#, &channels, now()).unwrap()
		else {
			panic!()
		};
		assert_eq!(values, vec![1.0, 2.0]);
		assert!(parse_ndjson(r#"{"hr":61}"#, &channels, now()).is_err());
	}

	#[test]
	fn batches_split_by_span_and_validate_width() {
		let mut batcher = Batcher {
			stream: "sensor.vest.ecg".parse().unwrap(),
			device: DeviceId::new(),
			channels: vec!["mv".into()],
			span: SignedDuration::from_secs(2),
			max_rows: 1000,
			current: None,
		};
		let t0 = now();
		assert!(batcher.push(t0, vec![1.0]).unwrap().is_none());
		assert!(
			batcher
				.push(t0 + SignedDuration::from_millis(1500), vec![2.0])
				.unwrap()
				.is_none()
		);
		let done = batcher
			.push(t0 + SignedDuration::from_millis(2500), vec![3.0])
			.unwrap()
			.unwrap();
		assert_eq!(done.offsets_ms, vec![0, 1500]);
		done.validate().unwrap();
		let rest = batcher.finish().unwrap();
		assert_eq!(rest.start, t0 + SignedDuration::from_millis(2500));
		assert!(batcher.push(t0, vec![1.0, 2.0]).is_err());
	}

	#[test]
	fn record_lines() {
		let stream: StreamId = "sensor.door.events".parse().unwrap();
		let record = record_line(
			r#"{"t":"2026-11-06T08:00:00Z","text":"front door opened","data":{"door":"front"}}"#,
			&stream,
			DeviceId::new(),
			now(),
		)
		.unwrap();
		assert_eq!(record.text.as_deref(), Some("front door opened"));
		assert_eq!(record.data["door"], "front");
		assert_eq!(record.stream, stream);
	}
}
