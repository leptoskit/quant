//! The Ask segment: choose an approved agent from the vault directory, edit
//! and send a question with idempotent retry protection, and view actual
//! replies with citations.
//!
//! Typed questions, unresolved drafts, and answers live only in memory and
//! clear on vault or access changes. Quant never infers whether an agent is
//! online and never invents a reply.

use leptoskit::native::reactive::For;
use leptoskit::prelude::ActivityIndicator;
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::GetUntracked;
use leptoskit::prelude::Set;
use leptoskit::prelude::Text;
use leptoskit::prelude::TextInput;
use leptoskit::prelude::View;
use quant_model::phone::CompanionAgentRow;
use quant_model::phone::CompanionCapability;
use quant_model::phone::CompanionMessageRow;
use quant_model::phone::CompanionReplyStatus;
use quant_model::phone::StatsAccess;

use super::State;
use super::companion;
use crate::theme;

/// In-memory session state for the Ask segment. Never persisted to disk or the
/// collector spool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskUiState {
	/// Monotonic sequence for agent directory requests.
	pub agents_seq: u64,
	/// Monotonic sequence for message list / single-message refresh requests.
	pub messages_seq: u64,
	/// Monotonic sequence for send requests.
	pub send_seq: u64,
	/// Monotonic counter for minting unique idempotency keys within this
	/// launch.
	pub draft_counter: u64,
	/// Whether the agent directory is loading.
	pub loading_agents: bool,
	/// Whether messages are loading.
	pub loading_messages: bool,
	/// Whether at least one agents fetch has completed.
	pub agents_loaded: bool,
	/// Whether at least one messages fetch has completed.
	pub messages_loaded: bool,
	/// Approved agents returned by `companion_agents`.
	pub agents: Vec<CompanionAgentRow>,
	/// This companion's own messages returned by `companion_messages`.
	pub messages: Vec<CompanionMessageRow>,
	/// When messages were last refreshed.
	pub fetched: Option<String>,
	/// Currently chosen recipient agent id (survives segment switches).
	pub recipient_id: Option<String>,
	/// Editable question text (survives segment switches).
	pub draft_question: String,
	/// Idempotency key retained across unresolved retries of the same draft.
	pub draft_dedupe: Option<String>,
	/// Whether a send request is currently in flight (blocks duplicate taps).
	pub sending: bool,
	/// Last error loading agents or messages.
	pub load_error: Option<String>,
	/// Last error submitting the current draft.
	pub send_error: Option<String>,
	/// Current list limit (starts at 50, can grow up to 200).
	pub message_limit: usize,
}

impl AskUiState {
	/// Clear fetched session data and unresolved drafts on vault or revoked
	/// access changes, bumping sequences so stale responses are ignored.
	pub fn clear_all(&mut self) {
		self.agents_seq = self.agents_seq.wrapping_add(1);
		self.messages_seq = self.messages_seq.wrapping_add(1);
		self.send_seq = self.send_seq.wrapping_add(1);
		self.loading_agents = false;
		self.loading_messages = false;
		self.agents_loaded = false;
		self.messages_loaded = false;
		self.agents.clear();
		self.messages.clear();
		self.fetched = None;
		self.recipient_id = None;
		self.draft_question.clear();
		self.draft_dedupe = None;
		self.sending = false;
		self.load_error = None;
		self.send_error = None;
		self.message_limit = 50;
	}

	/// Clear only transient in-flight loading state when the vault becomes
	/// temporarily unreachable, preserving the user's draft and its dedupe key
	/// for an idempotent retry once reachable again.
	pub fn clear_transient_offline(&mut self) {
		self.agents_seq = self.agents_seq.wrapping_add(1);
		self.messages_seq = self.messages_seq.wrapping_add(1);
		self.loading_agents = false;
		self.loading_messages = false;
		self.sending = false;
	}

	/// Whether any loaded message is currently waiting for an agent reply.
	pub fn has_waiting(&self) -> bool {
		self.messages
			.iter()
			.any(|msg| msg.status == CompanionReplyStatus::Waiting)
	}
}

/// The Ask segment root.
pub(crate) fn ask_segment(state: &State) -> Element {
	let snapshot = state.snapshot;
	let ask_ui = state.ask_ui;
	let state_for_access = state.clone();
	let state_for_body = state.clone();
	View()
		.class("gap-5")
		.child(companion::access_card(
			&state_for_access,
			CompanionCapability::AskAgents,
		))
		.child(For(
			move || {
				let access = snapshot.get().ask.access;
				if access != StatsAccess::Ready {
					return Vec::new();
				}
				let ui = ask_ui.get();
				// Key on structural state excluding live typing edits so the
				// TextInput keeps focus and text while the owner types.
				vec![AskViewKey {
					agents: ui.agents,
					messages: ui.messages,
					loading_agents: ui.loading_agents,
					loading_messages: ui.loading_messages,
					agents_loaded: ui.agents_loaded,
					messages_loaded: ui.messages_loaded,
					fetched: ui.fetched,
					recipient_id: ui.recipient_id,
					sending: ui.sending,
					has_dedupe: ui.draft_dedupe.is_some(),
					draft_clear_epoch: ui.draft_question.is_empty(),
					load_error: ui.load_error,
					send_error: ui.send_error,
					message_limit: ui.message_limit,
				}]
			},
			Clone::clone,
			move |_| {
				let ui = state_for_body.ask_ui.get_untracked();
				ask_ready_view(&state_for_body, &ui)
			},
		))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AskViewKey {
	agents: Vec<CompanionAgentRow>,
	messages: Vec<CompanionMessageRow>,
	loading_agents: bool,
	loading_messages: bool,
	agents_loaded: bool,
	messages_loaded: bool,
	fetched: Option<String>,
	recipient_id: Option<String>,
	sending: bool,
	has_dedupe: bool,
	draft_clear_epoch: bool,
	load_error: Option<String>,
	send_error: Option<String>,
	message_limit: usize,
}

fn ask_ready_view(state: &State, ui: &AskUiState) -> Element {
	let refresh = state.clone();
	let manage = state.clone();
	let mut root = View().class("gap-3").child(
		View()
			.class("gap-2.5 p-4 rounded-3xl border border-[#2A2560] bg-[#17143A]")
			.child(Text("Targeted agent inbox").class(theme::HEADING))
			.child(
				Text(
					"Approved agents retrieve questions when they check their Quant inbox. \
					 Replies appear here once an agent actually answers.",
				)
				.class(theme::CAPTION)
				.test_id("ask-inbox-note"),
			)
			.child(
				View()
					.class("flex-row items-center justify-between gap-2")
					.child(
						Button("Refresh")
							.class(theme::COMPACT)
							.test_id("ask-refresh")
							.on_press(move || refresh.reload_ask()),
					)
					.child(
						Button("Manage access")
							.class(theme::QUIET)
							.test_id("ask-manage-access")
							.on_press(move || {
								let now = !manage.manage_access.get_untracked();
								if now {
									manage.sync_capability_toggles(CompanionCapability::AskAgents);
								}
								manage.manage_access.set(now);
							}),
					),
			)
			.children(
				ui.fetched
					.as_ref()
					.map(|when| Text(format!("Updated {when}")).class(theme::CAPTION)),
			),
	);

	if let Some(error) = &ui.load_error {
		let retry = state.clone();
		root = root.child(
			View()
				.class(theme::CARD)
				.child(Text("Could not reach your vault").class(theme::HEADING))
				.child(
					Text(error.clone())
						.class(theme::PROBLEM)
						.test_id("ask-load-error"),
				)
				.child(
					Button("Try again")
						.class(theme::SECONDARY)
						.test_id("ask-retry-load")
						.on_press(move || retry.reload_ask()),
				),
		);
	}

	root = root
		.child(agent_directory_card(state, ui))
		.child(composer_card(state, ui))
		.child(messages_card(state, ui));

	root
}

fn agent_directory_card(state: &State, ui: &AskUiState) -> Element {
	let mut card = View()
		.class(theme::CARD)
		.child(Text("Approved agents").class(theme::HEADING));

	if ui.loading_agents && !ui.agents_loaded {
		return card.child(
			View()
				.class("flex-row items-center gap-3")
				.child(ActivityIndicator(true))
				.child(
					Text("Loading approved agents…")
						.class(theme::MUTED)
						.test_id("ask-agents-loading"),
				),
		);
	}

	if ui.agents.is_empty() {
		return card.child(
			Text(
				"No approved agents are available in your vault yet. Approve an agent on your \
				 computer to send it questions here.",
			)
			.class(theme::MUTED)
			.test_id("ask-agents-empty"),
		);
	}

	for agent in &ui.agents {
		let selected = ui.recipient_id.as_deref() == Some(agent.id.as_str());
		let choose = state.clone();
		let id = agent.id.clone();
		card = card.child(
			View().class(theme::ROW).child(
				View()
					.class("flex-row items-center justify-between gap-3")
					.child(
						View()
							.class("flex-1 gap-0.5")
							.child(Text(agent.name.clone()).class(theme::LABEL))
							.children(
								agent
									.detail
									.as_ref()
									.map(|detail| Text(detail.clone()).class(theme::CAPTION)),
							),
					)
					.child(
						Button(if selected { "Selected" } else { "Choose" })
							.class(if selected {
								theme::COMPACT_ACTIVE
							} else {
								theme::COMPACT
							})
							.accessibility_label(format!("Choose agent {}", agent.name))
							.test_id(format!("agent-{}", agent.id))
							.on_press(move || choose.select_ask_recipient(id.clone())),
					),
			),
		);
	}
	card
}

fn composer_card(state: &State, ui: &AskUiState) -> Element {
	let chosen = ui
		.recipient_id
		.as_deref()
		.and_then(|id| ui.agents.iter().find(|agent| agent.id == id));
	let recipient_text = chosen.map_or_else(
		|| "Recipient: choose an approved agent above".to_owned(),
		|agent| format!("To: {}", agent.name),
	);
	let typing = state.clone();
	let send = state.clone();
	let button_label = if ui.sending {
		"Sending…"
	} else if ui.draft_dedupe.is_some() && ui.send_error.is_some() {
		"Retry send"
	} else {
		"Send question"
	};

	View()
		.class(theme::CARD)
		.child(Text("New question").class(theme::HEADING))
		.child(
			Text(recipient_text)
				.class(theme::LABEL)
				.test_id("ask-recipient"),
		)
		.child(
			TextInput()
				.class(theme::INPUT)
				.value(ui.draft_question.clone())
				.placeholder("Ask this agent about your vault…")
				.accessibility_label("Question for selected agent")
				.test_id("ask-question")
				.on_change(move |text: &str| typing.set_ask_draft(text)),
		)
		.children(ui.send_error.as_ref().map(|error| {
			Text(error.clone())
				.class(theme::PROBLEM)
				.test_id("ask-send-error")
		}))
		.child(
			Button(button_label)
				.class(theme::PRIMARY)
				.test_id("ask-send")
				.on_press(move || send.submit_ask_question()),
		)
}

fn messages_card(state: &State, ui: &AskUiState) -> Element {
	let mut card = View()
		.class(theme::CARD)
		.child(Text("Your agent replies").class(theme::HEADING));

	if ui.loading_messages && !ui.messages_loaded {
		return card.child(
			View()
				.class("flex-row items-center gap-3")
				.child(ActivityIndicator(true))
				.child(
					Text("Loading replies…")
						.class(theme::MUTED)
						.test_id("ask-messages-loading"),
				),
		);
	}

	if ui.messages.is_empty() {
		return card.child(
			Text("Questions you send from this phone and their actual agent replies appear here.")
				.class(theme::MUTED)
				.test_id("ask-messages-empty"),
		);
	}

	for msg in &ui.messages {
		card = card.child(message_row(state, msg));
	}

	let limit = ui.message_limit.max(50);
	if ui.messages.len() >= limit && limit < 200 {
		let more = state.clone();
		let next_limit = (limit + 50).min(200);
		card = card.child(
			Button("Load older replies")
				.class(theme::SECONDARY)
				.test_id("ask-load-more")
				.on_press(move || more.load_more_ask_messages(next_limit)),
		);
	}

	card
}

fn message_row(state: &State, msg: &CompanionMessageRow) -> Element {
	let (badge_text, badge_class) = match msg.status {
		CompanionReplyStatus::Waiting => ("WAITING FOR AGENT", theme::BADGE_WARN),
		CompanionReplyStatus::Answered => ("REPLIED", theme::BADGE_OK),
		CompanionReplyStatus::Closed => ("CLOSED", theme::BADGE_MUTED),
	};
	let check = state.clone();
	let check_id = msg.id.clone();
	let followup = state.clone();
	let followup_agent = msg.to_id.clone();

	let mut row = View()
		.class(theme::ROW)
		.test_id(format!("msg-{}", msg.id))
		.child(
			View()
				.class("flex-row items-center justify-between gap-2")
				.child(Text(format!("To {}", msg.to_name)).class(theme::LABEL))
				.child(
					Text(badge_text)
						.class(badge_class)
						.test_id(format!("msg-status-{}", msg.id)),
				),
		)
		.child(Text(format!("Sent {}", msg.submitted)).class(theme::CAPTION))
		.child(Text(msg.question.clone()).class(theme::BODY));

	match msg.status {
		CompanionReplyStatus::Waiting => {
			row = row
				.child(
					Text(format!(
						"Waiting in {}'s Quant inbox. No reply has arrived yet.",
						msg.to_name
					))
					.class(theme::MUTED),
				)
				.child(
					View()
						.class("flex-row items-center gap-2")
						.child(
							Button("Check reply")
								.class(theme::COMPACT)
								.test_id(format!("msg-check-{}", msg.id))
								.on_press(move || check.check_ask_message(check_id.clone())),
						)
						.child(
							Button("Ask follow-up")
								.class(theme::QUIET)
								.test_id(format!("msg-followup-{}", msg.id))
								.on_press(move || {
									followup.prepare_ask_followup(followup_agent.clone())
								}),
						),
				);
		}
		CompanionReplyStatus::Answered => {
			if let Some(answered) = &msg.answered {
				row = row.child(Text(format!("Answered {answered}")).class(theme::CAPTION));
			}
			if let Some(answer) = &msg.answer {
				row = row.child(
					View()
						.class("gap-2 p-4 rounded-2xl border border-[#2A2560] bg-[#0B0A1D]")
						.child(
							Text(answer.clone())
								.class(theme::BODY)
								.test_id(format!("msg-answer-{}", msg.id)),
						),
				);
			}
			if !msg.citations.is_empty() {
				let label = if msg.citations.len() == 1 {
					"1 citation".to_owned()
				} else {
					format!("{} citations", msg.citations.len())
				};
				row = row.child(
					View()
						.class("gap-1")
						.child(
							Text(label)
								.class(theme::EYEBROW)
								.test_id(format!("msg-citations-{}", msg.id)),
						)
						.children(
							msg.citations
								.iter()
								.map(|id| Text(id.clone()).class(theme::CAPTION)),
						),
				);
			}
			row = row.child(
				Button("Ask follow-up")
					.class(theme::COMPACT)
					.test_id(format!("msg-followup-{}", msg.id))
					.on_press(move || followup.prepare_ask_followup(followup_agent.clone())),
			);
		}
		CompanionReplyStatus::Closed => {
			row = row
				.child(
					Text("Closed without an answer (expired or access changed).")
						.class(theme::MUTED),
				)
				.child(
					Button("Ask again")
						.class(theme::COMPACT)
						.test_id(format!("msg-followup-{}", msg.id))
						.on_press(move || followup.prepare_ask_followup(followup_agent.clone())),
				);
		}
	}

	row
}
