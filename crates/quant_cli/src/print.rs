//! Human-readable output.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_api::Audit;
use quant_api::ClientView;
use quant_api::Durations;
use quant_api::Me;
use quant_api::Overview;
use quant_api::OwnerDeviceView;
use quant_api::Records;
use quant_api::Series;
use quant_api::SessionSource;
use quant_api::Status;
use quant_api::StreamAccess;

fn local(t: Timestamp) -> String {
	t.to_zoned(TimeZone::system())
		.strftime("%Y-%m-%d %H:%M:%S")
		.to_string()
}

fn short(text: &str, width: usize) -> String {
	let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
	if flat.chars().count() <= width {
		flat
	} else {
		let mut cut: String = flat.chars().take(width.saturating_sub(1)).collect();
		cut.push('…');
		cut
	}
}

fn duration(seconds: f64) -> String {
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	let total = seconds.round().max(0.0) as u64;
	match total {
		0..60 => format!("{total}s"),
		60..3600 => format!("{}m", total / 60),
		_ => format!("{}h {:02}m", total / 3600, (total % 3600) / 60),
	}
}

pub fn clients(clients: &[ClientView]) {
	if clients.is_empty() {
		println!("No clients.");
	}
	for client in clients {
		println!(
			"{}  {:<9} {:?}  {} ({:?})",
			client.pairing_code,
			format!("{:?}", client.status).to_lowercase(),
			client.kind,
			client.name,
			client.id
		);
		if let Some(grant) = &client.grant {
			println!("    scopes: {}", grant.scopes.to_strings().join(", "));
			if let Some(lookback) = grant.lookback {
				println!("    lookback: {}", duration(lookback.as_secs_f64()));
			}
			if let Some(expiry) = grant.expires_at {
				println!("    expires: {}", local(expiry));
			}
			println!(
				"    limits: {}/min, {} records/h, {} MB/day{}",
				grant.limits.requests_per_minute,
				grant.limits.records_per_hour,
				grant.limits.bytes_per_day / (1024 * 1024),
				if grant.unredacted { ", unredacted" } else { "" }
			);
		} else if !client.requested.is_empty() {
			println!("    asked for: {}", client.requested.join(", "));
		}
		if let Some(seen) = client.last_seen {
			println!("    last seen: {}", local(seen));
		}
	}
}

pub fn status(status: &Status) {
	println!("quantd {} — vault {}", status.version, status.vault_id);
	println!("running since {}", local(status.started_at));
	println!(
		"session opened with {}",
		match status.session {
			SessionSource::Passphrase => "the passphrase",
			SessionSource::Device => "an owner device",
		}
	);
	println!(
		"{} active grants, {} pairings waiting",
		status.active_grants, status.pending_clients
	);
	match status.paused {
		Some(quant_api::Paused::Until(until)) => println!("capture paused until {}", local(until)),
		Some(quant_api::Paused::UntilResumed) => println!("capture paused until resumed"),
		None => {}
	}
	for source in &status.capture {
		println!(
			"  {:<9} {:<8} {:>8} captured{}{}",
			source.source,
			if source.running { "running" } else { "stopped" },
			source.captured,
			source
				.last
				.map(|t| format!(", last {}", local(t)))
				.unwrap_or_default(),
			source
				.detail
				.as_ref()
				.map(|d| format!(" — {d}"))
				.unwrap_or_default(),
		);
	}
}

/// One enrolled owner device, for the owner.
pub fn owner_device(device: &OwnerDeviceView) {
	println!(
		"{}  {:<8} {:<22} {} ({})",
		device.fingerprint,
		if device.revoked_at.is_some() {
			"revoked"
		} else {
			"active"
		},
		device.policy.as_str(),
		device.label,
		device.id
	);
	if let Some(used) = device.last_used {
		println!("    last used: {}", local(used));
	}
}

/// The enrolled owner devices, for the owner.
pub fn owner_devices(devices: &[OwnerDeviceView]) {
	if devices.is_empty() {
		println!("No owner devices. `quant owner devices add …` enrols one.");
	}
	for device in devices {
		self::owner_device(device);
	}
}

pub fn audit(audit: &Audit) {
	for entry in &audit.entries {
		let actor = match &entry.actor {
			quant_access::audit::Actor::Owner => "owner".to_owned(),
			quant_access::audit::Actor::System => "system".to_owned(),
			quant_access::audit::Actor::Client { client, .. } => client.to_string(),
			quant_access::audit::Actor::Unknown { claimed } => {
				format!("unknown({})", claimed.as_deref().unwrap_or("?"))
			}
		};
		let outcome = match &entry.outcome {
			quant_access::audit::Outcome::Allowed { records, bytes } => {
				format!("ok {records} rec {bytes} B")
			}
			quant_access::audit::Outcome::Denied { reason } => format!("DENIED {reason}"),
			quant_access::audit::Outcome::Failed => "failed".to_owned(),
		};
		println!(
			"{:>6} {}  {:<16} {:<38} {:<20} {}",
			entry.seq,
			local(entry.at),
			entry.action,
			actor,
			short(&entry.target, 20),
			outcome
		);
	}
	match (audit.chain_ok, audit.chain_len) {
		(Some(true), Some(n)) => println!("\nAudit chain intact ({n} entries)."),
		(Some(false), _) => {
			println!(
				"\nWARNING: the audit chain does not verify; entries were altered or removed."
			);
		}
		_ => {}
	}
}

pub fn me(me: &Me) {
	println!("{} ({:?}) — {:?}", me.name, me.client, me.status);
	if let Some(grant) = &me.grant {
		println!("scopes: {}", grant.scopes.to_strings().join(", "));
	}
}

pub fn streams(streams: &[StreamAccess]) {
	for stream in streams {
		println!(
			"{:<24} {:<8} {}",
			stream.spec.id,
			format!("{:?}", stream.level).to_lowercase(),
			stream.spec.description
		);
	}
}

pub fn overview(overview: &Overview) {
	println!("{} → {}", local(overview.from), local(overview.to));
	if overview.streams.is_empty() {
		println!("Nothing captured that this client may see.");
	}
	for item in &overview.streams {
		println!(
			"{:<24} {:>9}  {} → {}",
			item.stream,
			item.count,
			local(item.first),
			local(item.last)
		);
	}
}

pub fn records(records: &Records) {
	for record in &records.records {
		let text = record
			.snippet
			.as_deref()
			.or(record.text.as_deref())
			.unwrap_or_default();
		let app = record
			.data
			.get("app")
			.and_then(|v| v.as_str())
			.unwrap_or_default();
		let span = record
			.end
			.map(|end| {
				format!(
					" ({})",
					duration(end.duration_since(record.start).as_secs_f64())
				)
			})
			.unwrap_or_default();
		println!(
			"{}{span}  {:<18} {:<12} {}",
			local(record.start),
			record.stream,
			short(app, 12),
			short(text, 90)
		);
	}
	if records.truncated {
		println!("… more matched; narrow the range or raise --limit.");
	}
	if !records.withheld.is_empty() {
		println!(
			"Not readable at record level with this grant: {}",
			records.withheld.join(", ")
		);
	}
}

pub fn series(series: &Series) {
	if let Some(note) = &series.note {
		println!("note: {note}");
	}
	for bucket in &series.buckets {
		let channels: Vec<String> = bucket
			.channels
			.iter()
			.map(|(name, s)| {
				format!(
					"{name} {:.1} ({:.1}–{:.1}, n={})",
					s.mean, s.min, s.max, s.count
				)
			})
			.collect();
		println!("{}  {}", local(bucket.start), channels.join("  "));
	}
	for batch in &series.batches {
		for (t, row) in batch.rows() {
			let values: Vec<String> = batch
				.channels
				.iter()
				.zip(row)
				.map(|(c, v)| format!("{c}={v}"))
				.collect();
			println!("{}  {}", local(t), values.join(" "));
		}
	}
}

pub fn durations(durations: &Durations) {
	for item in &durations.items {
		println!(
			"{}  {:<28} {:>10}  ({} spans)",
			local(item.start),
			item.group.as_deref().unwrap_or("—"),
			duration(item.seconds),
			item.count
		);
	}
}

/// One claim, for the owner.
pub fn claim(claim: &quant_core::Claim) {
	let classes: Vec<String> = claim
		.classes
		.iter()
		.map(|class| format!("[{class}]"))
		.collect();
	println!(
		"{}  {:<11} {:<16} {:<10} {:>4} {}\"{}\"",
		claim.id,
		claim.status.as_str(),
		claim.authority.as_str(),
		claim.kind.as_str(),
		format!("{:.0}%", claim.confidence * 100.0),
		classes.join(" "),
		short(&claim.statement, 90)
	);
	if let Some(topic) = &claim.topic {
		println!("    topic: {topic}");
	}
	if let Some(by) = claim.superseded_by {
		println!("    superseded by {by}");
	}
}

/// Claims, for the owner's list and profile.
pub fn claims(claims: &[quant_core::Claim]) {
	if claims.is_empty() {
		println!("No claims yet. `quant tell \"…\"` states one.");
	}
	for claim in claims {
		self::claim(claim);
	}
}

/// The on-device learning settings, for the owner.
pub fn learn_config(config: &serde_json::Value) {
	if config == &serde_json::Value::Null
		|| config.as_object().is_some_and(serde_json::Map::is_empty)
	{
		println!("On-device learning is off.");
		println!("`quant learn set --embed-endpoint http://127.0.0.1:11434 …` turns it on.");
		return;
	}
	if let Some(embed) = config.get("embed").and_then(serde_json::Value::as_object) {
		println!(
			"embeddings: {} ({}) {} dims",
			embed
				.get("endpoint")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("?"),
			embed
				.get("model")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("?"),
			embed
				.get("dims")
				.map_or_else(|| "?".into(), serde_json::Value::to_string),
		);
	} else {
		println!("embeddings: off");
	}
	if let Some(extract) = config.get("extract").and_then(serde_json::Value::as_object) {
		println!(
			"extraction: {} ({}) — {} calls/day, {} claims/run",
			extract
				.get("endpoint")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("?"),
			extract
				.get("model")
				.and_then(serde_json::Value::as_str)
				.unwrap_or("?"),
			extract
				.get("calls_per_day")
				.map_or_else(|| "?".into(), serde_json::Value::to_string),
			extract
				.get("claims_per_run")
				.map_or_else(|| "?".into(), serde_json::Value::to_string),
		);
	} else {
		println!("extraction: off");
	}
	if let Some(streams) = config.get("streams").and_then(serde_json::Value::as_array) {
		println!(
			"learns from: {}",
			streams
				.iter()
				.filter_map(serde_json::Value::as_str)
				.collect::<Vec<_>>()
				.join(", ")
		);
	}
}
