//! Companion messaging against the vault's approved-agent directory and
//! targeted inbox.
//!
//! Nothing here touches the collector's write spool, local disk, or the
//! owner-wide questions ledger. Every call is signed with the phone's opt-in
//! companion identity carrying `companion:write`.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use quant_api::CompanionAgent;
use quant_api::CompanionMessageId;
use quant_api::CompanionMessageStatus;
use quant_api::CompanionMessageView;
use quant_api::CompanionSend;
use quant_client::Client;
use quant_client::ClientError;
use quant_core::ClientId;
use quant_model::phone::CompanionAgentRow;
use quant_model::phone::CompanionMessageRow;
use quant_model::phone::CompanionReplyStatus;

use crate::words;

/// List approved agents this companion may address.
pub async fn agents(client: &Client) -> Result<Vec<CompanionAgentRow>, ClientError> {
	let mut agents = client.companion_agents().await?;
	agents.sort_by(|left, right| {
		left.name
			.to_ascii_lowercase()
			.cmp(&right.name.to_ascii_lowercase())
			.then_with(|| left.id.cmp(&right.id))
	});
	Ok(agents.into_iter().map(shape_agent).collect())
}

/// List this companion's own sent messages and their actual replies, newest
/// first.
pub async fn messages(
	client: &Client,
	limit: Option<usize>,
	tz: &TimeZone,
) -> Result<(Vec<CompanionMessageRow>, String), ClientError> {
	let now = Timestamp::now();
	let items = client.companion_messages(limit).await?;
	let rows = items
		.into_iter()
		.map(|item| shape_message(item, now, tz))
		.collect();
	Ok((rows, words::when(now, now, tz)))
}

/// Send one question to an approved agent, reusing `dedupe` across unresolved
/// retries so the vault returns the original message instead of creating a
/// duplicate.
pub async fn send(
	client: &Client,
	to: ClientId,
	question: String,
	dedupe: String,
	tz: &TimeZone,
) -> Result<CompanionMessageRow, ClientError> {
	let now = Timestamp::now();
	let message = client
		.send_companion_message(&CompanionSend {
			to,
			question,
			purpose: None,
			dedupe,
		})
		.await?;
	Ok(shape_message(message, now, tz))
}

/// Fetch one of this companion's own messages and any actual reply.
pub async fn message(
	client: &Client,
	id: CompanionMessageId,
	tz: &TimeZone,
) -> Result<CompanionMessageRow, ClientError> {
	let now = Timestamp::now();
	let message = client.companion_message(id).await?;
	Ok(shape_message(message, now, tz))
}

fn shape_agent(agent: CompanionAgent) -> CompanionAgentRow {
	let detail = agent.agent.and_then(|identity| {
		let product = identity.product.trim();
		let model = identity.model.trim();
		match (product.is_empty(), model.is_empty()) {
			(false, false) => Some(words::shorten(&format!("{product} · {model}"), 80)),
			(false, true) => Some(words::shorten(product, 80)),
			(true, false) => Some(words::shorten(model, 80)),
			(true, true) => None,
		}
	});
	CompanionAgentRow {
		id: agent.id.to_string(),
		name: words::shorten(&agent.name, 80),
		detail,
	}
}

fn shape_message(view: CompanionMessageView, now: Timestamp, tz: &TimeZone) -> CompanionMessageRow {
	let status = match view.status {
		CompanionMessageStatus::Open => CompanionReplyStatus::Waiting,
		CompanionMessageStatus::Answered => CompanionReplyStatus::Answered,
		CompanionMessageStatus::Abandoned => CompanionReplyStatus::Closed,
	};
	CompanionMessageRow {
		id: view.id.to_string(),
		to_id: view.to.to_string(),
		to_name: words::shorten(&view.to_name, 80),
		question: view.question,
		submitted: words::when(view.submitted_at, now, tz),
		submitted_ms: view.submitted_at.as_millisecond(),
		status,
		answer: view.answer,
		answered: view.answered_at.map(|at| words::when(at, now, tz)),
		answered_ms: view.answered_at.map(Timestamp::as_millisecond),
		citations: view
			.citations
			.into_iter()
			.map(|id| id.to_string())
			.collect(),
	}
}
