use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use quant_access::Level;
use quant_access::Scope;
use quant_api::ClaimProposal;
use quant_api::ClientStatus;
use quant_api::ContextRequest;
use quant_api::DurationsRequest;
use quant_api::IngestRecords;
use quant_api::Me;
use quant_api::OverviewRequest;
use quant_api::ProposeRequest;
use quant_api::RecordsRequest;
use quant_api::SeriesRequest;
use quant_client::Client;
use quant_client::when;
use quant_core::ClaimKind;
use quant_core::Class;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::StreamId;
use serde_json::Value;
use serde_json::json;

/// The built-in stream that serves the owner's profile claims.
const PROFILE: &str = "memory.profile";

fn profile_stream() -> StreamId {
	PROFILE.parse().expect("valid built-in stream")
}

/// A tool the server can offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
	/// Whether this agent is approved, and what it may do.
	AccessStatus,
	/// What exists over a period.
	Overview,
	/// Streams this agent may see.
	Streams,
	/// Time spent per app, posture, sleep stage, …
	TimeSpent,
	/// Numeric streams: heart rate, steps, motion.
	Series,
	/// Records in time order.
	Timeline,
	/// Full-text search over screens and transcripts.
	Search,
	/// A screenshot or audio clip.
	Media,
	/// Write a note into memory.
	Note,
	/// Save a moment from the rolling audio buffer.
	SaveAudio,
	/// What the owner has stated about themselves.
	Profile,
	/// Propose a claim about the owner for their approval.
	Propose,
	/// A budgeted context packet assembled for one task.
	Context,
}

/// The tools a caller with `me` may use.
pub fn tools_for(me: Option<&Me>) -> Vec<Tool> {
	let Some(grant) = me
		.filter(|me| me.status == ClientStatus::Active)
		.and_then(|me| me.grant.as_ref())
	else {
		return vec![Tool::AccessStatus];
	};
	if grant.check_live(Timestamp::now()).is_err() {
		return vec![Tool::AccessStatus];
	}
	let levels: Vec<Level> = grant
		.scopes
		.0
		.iter()
		.filter_map(|scope| {
			match scope {
				Scope::Read { level, .. } => Some(*level),
				_ => None,
			}
		})
		.collect();
	let mut tools = vec![Tool::AccessStatus];
	if !levels.is_empty() {
		tools.extend([Tool::Overview, Tool::Streams, Tool::TimeSpent, Tool::Series]);
	}
	if levels.iter().any(|l| *l >= Level::Read) {
		tools.extend([Tool::Timeline, Tool::Search]);
	}
	if levels.contains(&Level::Raw) {
		tools.push(Tool::Media);
	}
	if grant
		.scopes
		.can_write(&"memory.note".parse().expect("valid stream"))
	{
		tools.push(Tool::Note);
	}
	if grant
		.scopes
		.can_write(&"audio.buffer".parse().expect("valid stream"))
	{
		tools.push(Tool::SaveAudio);
	}
	if grant.scopes.read_level(&profile_stream()).is_some() {
		tools.push(Tool::Profile);
	}
	if grant.scopes.can_write(&profile_stream()) {
		tools.push(Tool::Propose);
	}
	if grant.scopes.read_level(&profile_stream()).is_some()
		|| levels.iter().any(|l| *l >= Level::Read)
	{
		tools.push(Tool::Context);
	}
	tools
}

fn time_props() -> Value {
	json!({
		"from": { "type": "string", "description": "Start: RFC 3339, YYYY-MM-DD, today, yesterday, or an offset like -2h or -7d. Defaults to 24 hours before `to`." },
		"to": { "type": "string", "description": "End, in the same forms. Defaults to now." },
	})
}

fn schema(extra: &Value, required: &[&str]) -> Value {
	let mut properties = time_props();
	if let (Some(props), Some(extra)) = (properties.as_object_mut(), extra.as_object()) {
		for (k, v) in extra {
			props.insert(k.clone(), v.clone());
		}
	}
	json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

pub(crate) fn error_result(message: &str) -> Value {
	json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn ok_result(value: &Value) -> Value {
	json!({ "content": [{ "type": "text", "text": value.to_string() }], "isError": false })
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
	args.get(key)
		.and_then(Value::as_str)
		.filter(|s| !s.trim().is_empty())
}

fn streams_arg(args: &Value) -> Vec<String> {
	match args.get("streams") {
		Some(Value::Array(items)) => {
			items
				.iter()
				.filter_map(Value::as_str)
				.map(str::to_owned)
				.collect()
		}
		Some(Value::String(s)) => {
			s.split(',')
				.map(|s| s.trim().to_owned())
				.filter(|s| !s.is_empty())
				.collect()
		}
		_ => Vec::new(),
	}
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
	args.get(key)
		.and_then(Value::as_u64)
		.and_then(|n| usize::try_from(n).ok())
}

impl Tool {
	/// The tool's MCP name.
	pub fn name(self) -> &'static str {
		match self {
			Tool::AccessStatus => "quant_access_status",
			Tool::Overview => "quant_overview",
			Tool::Streams => "quant_streams",
			Tool::TimeSpent => "quant_time_spent",
			Tool::Series => "quant_series",
			Tool::Timeline => "quant_timeline",
			Tool::Search => "quant_search",
			Tool::Media => "quant_media",
			Tool::Note => "quant_note",
			Tool::SaveAudio => "quant_save_audio",
			Tool::Profile => "quant_profile",
			Tool::Propose => "quant_propose",
			Tool::Context => "quant_context",
		}
	}

	/// The tool's MCP description.
	pub fn describe(&self) -> Value {
		let (title, description, input) = match self {
			Tool::AccessStatus => {
				(
					"Access status",
					"Whether this agent is approved to read the owner's Quant vault, what its \
					 grant allows (scopes, lookback, rate limits), and how to get approved if it \
					 is not.",
					json!({ "type": "object", "properties": {}, "additionalProperties": false }),
				)
			}
			Tool::Overview => {
				(
					"Overview",
					"What was captured over a period: each stream with its sample count and \
					 first/last times. Use first to see what data exists.",
					schema(&json!({}), &[]),
				)
			}
			Tool::Streams => {
				(
					"Streams",
					"Every stream this agent may see, what it means, its channels, and the level \
					 of detail allowed (summary, read, raw).",
					json!({ "type": "object", "properties": {}, "additionalProperties": false }),
				)
			}
			Tool::TimeSpent => {
				(
					"Time spent",
					"How time was spent in an event stream, grouped by an attribute: apps.focus \
					 by app, motion.posture by posture (lying, sitting, walking…), health.sleep \
					 by stage, location.visit by place. Optionally bucketed (e.g. 3600 for \
					 hourly).",
					schema(
						&json!({
							"stream": { "type": "string", "description": "Event stream, such as apps.focus or motion.posture." },
							"group_by": { "type": "string", "description": "Attribute to group by, such as app, posture, stage, place." },
							"bucket_seconds": { "type": "integer", "minimum": 60, "description": "Bucket width; omit for one total over the range." },
						}),
						&["stream"],
					),
				)
			}
			Tool::Series => {
				(
					"Series",
					"Numeric streams over time: health.heart_rate, health.hrv, health.steps, \
					 motion.accelerometer, location.fix, sensor.*. Returns min/max/mean per \
					 bucket by default; raw=true returns samples if the grant allows.",
					schema(
						&json!({
							"stream": { "type": "string", "description": "Series stream, such as health.heart_rate." },
							"bucket_seconds": { "type": "integer", "minimum": 60, "description": "Bucket width for summaries. Defaults to the range / 48." },
							"raw": { "type": "boolean", "description": "Return raw samples instead of buckets." },
							"max_rows": { "type": "integer", "minimum": 1, "maximum": 20000 },
						}),
						&["stream"],
					),
				)
			}
			Tool::Timeline => {
				(
					"Timeline",
					"Records in time order: screen frames with on-screen text, transcripts, focus \
					 spans, notifications, notes. Filter by streams or families (screen, audio, \
					 apps, comms, memory).",
					schema(
						&json!({
							"streams": { "type": "array", "items": { "type": "string" }, "description": "Streams or families; omit for all readable." },
							"limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 50." },
							"oldest_first": { "type": "boolean" },
						}),
						&[],
					),
				)
			}
			Tool::Search => {
				(
					"Search",
					"Full-text search over everything readable: on-screen text, transcripts, \
					 notes. Terms are ANDed; end a term with * for prefix match. Returns matches \
					 with snippets, best first. Mode: keyword, similar (vector similarity over \
					 on-device embeddings), or hybrid (both fused); defaults to hybrid when \
					 embeddings exist.",
					schema(
						&json!({
							"query": { "type": "string", "description": "Search terms." },
							"streams": { "type": "array", "items": { "type": "string" } },
							"limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 20." },
							"mode": { "type": "string", "enum": ["keyword", "similar", "hybrid"], "description": "How to match; defaults to hybrid when embeddings exist for the vault's current space." },
						}),
						&["query"],
					),
				)
			}
			Tool::Media => {
				(
					"Media",
					"Fetch a screenshot or audio clip by the media hash from a record.",
					json!({
						"type": "object",
						"properties": { "hash": { "type": "string", "description": "The media hash from a record." } },
						"required": ["hash"],
						"additionalProperties": false,
					}),
				)
			}
			Tool::Note => {
				(
					"Note",
					"Write a note into the owner's memory, such as an observation, a pattern you \
					 found, or something the owner asked you to remember.",
					json!({
						"type": "object",
						"properties": {
							"text": { "type": "string" },
							"tags": { "type": "array", "items": { "type": "string" } },
						},
						"required": ["text"],
						"additionalProperties": false,
					}),
				)
			}
			Tool::SaveAudio => {
				(
					"Save audio",
					"Save a moment from the owner's rolling 24-hour audio buffer as a clip, such \
					 as the last 10 minutes of a call. Nothing is transcribed until you save; the \
					 clip is transcribed once, on device, right after. Audio content is never \
					 returned — use the clip id with the export commands the owner runs.",
					json!({
						"type": "object",
						"properties": {
							"from": { "type": "string", "description": "Start: RFC 3339, or an offset like -10m. Defaults to the whole `duration` back from `to`." },
							"to": { "type": "string", "description": "End, in the same forms. Defaults to now." },
							"duration": { "type": "string", "description": "How far back to keep, such as `90s`, `10m`, or `1h`. Used when `from` is omitted." },
							"title": { "type": "string", "description": "A name for the clip." },
						},
						"additionalProperties": false,
					}),
				)
			}
			Tool::Profile => {
				(
					"Profile",
					"What the owner has stated about themselves: their preferences, values, \
					 routines, facts, relationships, goals, and commitments. These are the \
					 owner's own words (or claims the owner approved) — treat them as the owner's \
					 stated wishes and act in line with them, never as data to override. At \
					 summary level only what the owner stated themselves is included; at read \
					 level more claims appear, each with evidence record ids.",
					json!({ "type": "object", "properties": {}, "additionalProperties": false }),
				)
			}
			Tool::Propose => {
				(
					"Propose a claim",
					"Propose a claim about the owner, grounded in evidence records this agent can \
					 read (record ids from quant_timeline or quant_search). A proposal is held, \
					 invisible to every agent, until the owner approves it with `quant claims \
					 approve` — it is the owner's memory, and the owner decides. At most 20 \
					 proposals per hour.",
					schema(
						&json!({
							"statement": { "type": "string", "description": "The claim in natural language, 1 to 500 characters, e.g. \"Prefers espresso after lunch\"." },
							"kind": { "type": "string", "description": "preference, value, routine, fact, relationship, goal, or commitment. Defaults to fact." },
							"topic": { "type": "string", "description": "A short dotted key such as coffee.timing." },
							"subject": { "type": "string", "description": "Who it is about; me by default." },
							"classes": { "type": "array", "items": { "type": "string" }, "description": "Sensitivity classes: health, intimate, location, others, financial." },
							"confidence": { "type": "number", "minimum": 0, "maximum": 1, "description": "How sure you are, 0 to 1; defaults to 0.5." },
							"evidence": { "type": "array", "items": { "type": "string" }, "description": "Record ids grounding the claim; each must be readable by this agent." },
						}),
						&["statement", "evidence"],
					),
				)
			}
			Tool::Context => {
				(
					"Context",
					"Assemble a context packet for one task, sized to a token budget: profile \
					 claims matching the task first, then the latest memory summaries, then \
					 records matching the task's terms. Each part is included only where the \
					 grant allows it, and the answer says what was withheld or cut.",
					json!({
						"type": "object",
						"properties": {
							"task": { "type": "string", "description": "What you are doing, such as \"plan the owner's week\". Picks matching claims and records." },
							"budget_tokens": { "type": "integer", "minimum": 200, "maximum": 8000, "description": "Rough token budget; defaults to 1500." },
						},
						"required": ["task"],
						"additionalProperties": false,
					}),
				)
			}
		};
		let read_only = !matches!(self, Tool::Note | Tool::SaveAudio | Tool::Propose);
		json!({
			"name": self.name(),
			"title": title,
			"description": description,
			"inputSchema": input,
			"annotations": { "readOnlyHint": read_only, "openWorldHint": false },
		})
	}

	/// Run the tool.
	pub async fn call(self, client: &Client, me: Option<&Me>, args: &Value) -> Value {
		if self == Tool::Media {
			return media(client, args).await;
		}
		match self.run(client, me, args).await {
			Ok(value) => ok_result(&value),
			Err(message) => error_result(&message),
		}
	}

	async fn run(self, client: &Client, me: Option<&Me>, args: &Value) -> Result<Value, String> {
		let now = Timestamp::now();
		let from = when::parse_opt(arg(args, "from"), now)?;
		let to = when::parse_opt(arg(args, "to"), now)?;
		let failed = |e: quant_client::ClientError| e.to_string();
		match self {
			Tool::AccessStatus => Ok(access_status(client, me)),
			Tool::Overview => {
				let overview = client
					.overview(&OverviewRequest { from, to })
					.await
					.map_err(failed)?;
				Ok(serde_json::to_value(overview).unwrap_or_default())
			}
			Tool::Streams => {
				let streams = client.streams().await.map_err(failed)?;
				Ok(serde_json::to_value(streams).unwrap_or_default())
			}
			Tool::TimeSpent => {
				let stream = arg(args, "stream")
					.ok_or("`stream` is required")?
					.to_owned();
				let request = DurationsRequest {
					stream,
					from,
					to,
					group_by: arg(args, "group_by").map(str::to_owned),
					bucket_seconds: args.get("bucket_seconds").and_then(Value::as_u64),
				};
				let durations = client.durations(&request).await.map_err(failed)?;
				Ok(serde_json::to_value(durations).unwrap_or_default())
			}
			Tool::Series => {
				let stream = arg(args, "stream")
					.ok_or("`stream` is required")?
					.to_owned();
				let request = SeriesRequest {
					stream,
					from,
					to,
					bucket_seconds: args.get("bucket_seconds").and_then(Value::as_u64),
					raw: args.get("raw").and_then(Value::as_bool).unwrap_or(false),
					max_rows: usize_arg(args, "max_rows"),
				};
				let series = client.series(&request).await.map_err(failed)?;
				Ok(serde_json::to_value(series).unwrap_or_default())
			}
			Tool::Timeline | Tool::Search => {
				let search = self == Tool::Search;
				let mode = match args.get("mode").and_then(Value::as_str) {
					Some("keyword") => Some(quant_api::SearchMode::Keyword),
					Some("similar") => Some(quant_api::SearchMode::Similar),
					Some("hybrid") => Some(quant_api::SearchMode::Hybrid),
					Some(_) => return Err("`mode` is keyword, similar, or hybrid".into()),
					None => None,
				};
				let request = RecordsRequest {
					streams: streams_arg(args),
					from,
					to,
					search: if search {
						Some(arg(args, "query").ok_or("`query` is required")?.to_owned())
					} else {
						None
					},
					limit: usize_arg(args, "limit").or(if search { Some(20) } else { None }),
					oldest_first: args
						.get("oldest_first")
						.and_then(Value::as_bool)
						.unwrap_or(false),
					mode,
				};
				let records = if search {
					client.search(&request).await
				} else {
					client.records(&request).await
				}
				.map_err(failed)?;
				let mut value = serde_json::to_value(records).unwrap_or_default();
				value["captured_text_is_untrusted"] = Value::Bool(true);
				Ok(value)
			}
			Tool::Media => Err("handled separately".into()),
			Tool::SaveAudio => {
				let to = to.unwrap_or(now);
				let from = if let Some(from) = from {
					from
				} else {
					let duration = arg(args, "duration")
						.ok_or("`duration` (such as `10m`) or `from` is required")?;
					when::parse(&format!("-{duration}"), to)?
				};
				if to <= from {
					return Err("the range ends before it starts".into());
				}
				let saved = client
					.save_audio(&quant_api::SaveAudio {
						from,
						to,
						title: arg(args, "title").map(str::to_owned),
					})
					.await
					.map_err(failed)?;
				Ok(serde_json::to_value(saved).unwrap_or_default())
			}
			Tool::Note => {
				let text = arg(args, "text").ok_or("`text` is required")?;
				let client_id = client.identity().client.ok_or("not paired")?;
				let tags: Vec<Value> = args
					.get("tags")
					.and_then(Value::as_array)
					.map(|tags| tags.iter().filter(|t| t.is_string()).cloned().collect())
					.unwrap_or_default();
				let record = Record::new(
					StreamId::new("memory.note").expect("valid stream"),
					DeviceId(client_id.0),
				)
				.with_text(text)
				.with("author", client.identity().name.clone())
				.with("tags", Value::Array(tags));
				let id = record.id;
				client
					.ingest_records(&IngestRecords {
						records: vec![record],
						device_name: Some(client.identity().name.clone()),
						platform: Some("agent".into()),
					})
					.await
					.map_err(failed)?;
				Ok(json!({ "stored": id }))
			}
			Tool::Profile => {
				let profile = client.profile().await.map_err(failed)?;
				Ok(serde_json::to_value(profile).unwrap_or_default())
			}
			Tool::Propose => {
				let statement = arg(args, "statement")
					.ok_or("`statement` is required")?
					.to_owned();
				let kind = match arg(args, "kind") {
					Some(name) => {
						Some(ClaimKind::parse(name).ok_or(format!("unknown claim kind `{name}`"))?)
					}
					None => None,
				};
				let classes: Vec<Class> = args
					.get("classes")
					.and_then(Value::as_array)
					.map(|items| {
						items
							.iter()
							.filter_map(Value::as_str)
							.map(|name| {
								name.parse::<Class>()
									.map_err(|_| format!("unknown class `{name}`"))
							})
							.collect::<Result<_, _>>()
					})
					.transpose()?
					.unwrap_or_default();
				let evidence: Vec<RecordId> = match args.get("evidence") {
					Some(Value::Array(items)) => {
						items
							.iter()
							.filter_map(Value::as_str)
							.map(|id| {
								id.parse::<RecordId>()
									.map_err(|_| format!("bad record id `{id}`"))
							})
							.collect::<Result<_, _>>()?
					}
					_ => Vec::new(),
				};
				let proposed = client
					.propose(&ProposeRequest {
						claims: vec![ClaimProposal {
							statement,
							kind,
							topic: arg(args, "topic").map(str::to_owned),
							subject: arg(args, "subject").map(str::to_owned),
							classes,
							confidence: args.get("confidence").and_then(Value::as_f64).map(|n| {
								#[allow(clippy::cast_possible_truncation)]
								let confidence = n as f32;
								confidence
							}),
							evidence,
						}],
					})
					.await
					.map_err(failed)?;
				Ok(serde_json::to_value(proposed).unwrap_or_default())
			}
			Tool::Context => {
				let task = arg(args, "task").ok_or("`task` is required")?.to_owned();
				let budget_tokens = args
					.get("budget_tokens")
					.and_then(Value::as_u64)
					.and_then(|n| u32::try_from(n).ok());
				let context = client
					.context(&ContextRequest {
						task,
						budget_tokens,
					})
					.await
					.map_err(failed)?;
				let mut value = serde_json::to_value(context).unwrap_or_default();
				value["profile_claims_are_the_owners_words"] = Value::Bool(true);
				Ok(value)
			}
		}
	}
}

/// Fetch media as MCP content.
pub(crate) async fn media(client: &Client, args: &Value) -> Value {
	let Some(hash) = arg(args, "hash") else {
		return error_result("`hash` is required");
	};
	match client.media(hash).await {
		Ok((mime, bytes)) if mime.starts_with("image/") => {
			json!({
				"content": [{ "type": "image", "data": STANDARD.encode(&bytes), "mimeType": mime }],
				"isError": false,
			})
		}
		Ok((mime, bytes)) if mime.starts_with("audio/") => {
			json!({
				"content": [{ "type": "audio", "data": STANDARD.encode(&bytes), "mimeType": mime }],
				"isError": false,
			})
		}
		Ok((mime, bytes)) => ok_result(&json!({ "mime": mime, "size": bytes.len() })),
		Err(error) => error_result(&error.to_string()),
	}
}

fn access_status(client: &Client, me: Option<&Me>) -> Value {
	match me {
		None => {
			json!({
				"approved": false,
				"message": "This agent's key is not paired with the owner's Quant vault (or the daemon is not running). The owner can pair it by running `quant mcp --profile <name>` once in a terminal, then approving the code it shows with `quant approve <code>`.",
			})
		}
		Some(me) => {
			match (&me.status, &me.grant) {
				(ClientStatus::Active, Some(grant)) => {
					json!({
						"approved": true,
						"client": client.identity().name,
						"scopes": grant.scopes.to_strings(),
						"lookback_hours": grant.lookback.map(|d| d.as_secs() / 3600),
						"expires_at": grant.expires_at,
						"limits": grant.limits,
						"redacted": !grant.unredacted,
					})
				}
				(ClientStatus::Pending, _) => {
					json!({
						"approved": false,
						"message": format!(
							"Waiting for the owner to approve this agent. Ask them to run `quant approve {}` (they will be asked for their passphrase).",
							client.identity().key().public().pairing_code()
						),
					})
				}
				_ => {
					json!({
						"approved": false,
						"message": "The owner revoked or refused this agent's access.",
					})
				}
			}
		}
	}
}
