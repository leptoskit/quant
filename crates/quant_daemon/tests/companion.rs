//! Companion messaging end to end: a separately approved device chooses one
//! approved agent, submits a question, the addressed agent pulls it from its
//! own inbox and records an actual reply, and the device reads that reply
//! back. The agent-opened question ledger stays separate.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::AgentIdentity;
use quant_access::Limits;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::CompanionAnswer;
use quant_api::CompanionMessageStatus;
use quant_api::CompanionSend;
use quant_api::QuestionOpen;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::RecordId;
use quant_core::Sink;
use quant_crypto::KdfParams;
use quant_daemon::State;
use quant_store::Vault;
use quant_store::VaultPaths;

const PASS: &str = "correct horse battery staple";

struct Harness {
	_dir: tempfile::TempDir,
	endpoint: Endpoint,
	state: Arc<State>,
	_stop: tokio::sync::oneshot::Sender<()>,
}

async fn start() -> Harness {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("v"));
	let vault = Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap();
	let state = Arc::new(State::new(vault).unwrap());
	let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
	let socket = paths.socket();
	let serving = Arc::clone(&state);
	let path = socket.clone();
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &path, async {
			let _ = stopped.await;
		})
		.await
		.unwrap();
	});
	for _ in 0..100 {
		if socket.exists() {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	Harness {
		_dir: dir,
		endpoint: Endpoint::Unix(socket),
		state,
		_stop: stop,
	}
}

impl Harness {
	async fn owner(&self) -> OwnerSession {
		OwnerSession::open(self.endpoint.clone(), PASS)
			.await
			.unwrap()
	}

	async fn paired(&self, name: &str, kind: ClientKind, approve: ApproveRequest) -> Client {
		let mut client = Client::new(self.endpoint.clone(), Identity::generate(name));
		let agent = (kind == ClientKind::Agent).then(|| identity(name));
		let pairing = client.pair(kind, &[], agent.as_ref()).await.unwrap();
		self.owner()
			.await
			.approve(&ApproveRequest {
				client: pairing.pairing_code,
				..approve
			})
			.await
			.unwrap();
		client
	}
}

async fn id(client: &Client) -> quant_core::ClientId {
	client.me().await.unwrap().client
}

/// What an agent says about itself when it pairs.
fn identity(name: &str) -> AgentIdentity {
	AgentIdentity {
		product: name.to_owned(),
		vendor: "Quant tests".into(),
		model: "test-model".into(),
		version: env!("CARGO_PKG_VERSION").into(),
		host: "test-host".into(),
		operator: None,
	}
}

fn approve(scopes: &[&str]) -> ApproveRequest {
	ApproveRequest {
		client: String::new(),
		scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
		label: None,
		lookback_hours: None,
		expires_in_hours: None,
		limits: None,
		unredacted: false,
		deny_classes: None,
	}
}

fn send(to: quant_core::ClientId, question: &str, dedupe: &str) -> CompanionSend {
	CompanionSend {
		to,
		question: question.into(),
		purpose: Some("the owner asked from their phone".into()),
		dedupe: dedupe.into(),
	}
}

/// A readable screen record, so an agent can cite something real.
fn seed(state: &State) -> RecordId {
	let record = Record::new("screen.frame".parse().unwrap(), DeviceId::new())
		.starting(Timestamp::now() - SignedDuration::from_mins(30))
		.with_text("Wrote the release notes in Zed before noon.");
	let id = record.id;
	state.record(record).unwrap();
	id
}

#[tokio::test]
async fn a_message_flows_from_selection_to_pickup_to_reply() {
	let h = start().await;
	let screen = seed(&h.state);
	let unreadable = Record::new("health.sleep".parse().unwrap(), DeviceId::new())
		.starting(Timestamp::now() - SignedDuration::from_mins(20))
		.with_text("Slept six hours.");
	h.state.record(unreadable.clone()).unwrap();
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let other_agent = h
		.paired("Other agent", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let phone = h
		.paired("Phone", ClientKind::Device, approve(&["companion:write"]))
		.await;
	let other_phone = h
		.paired(
			"Other phone",
			ClientKind::Device,
			approve(&["companion:write"]),
		)
		.await;
	let agent_id = id(&agent).await;
	let other_agent_id = id(&other_agent).await;
	let phone_id = id(&phone).await;

	// The companion chooses from approved agents only: both agents are
	// visible, neither phone is.
	let agents = phone.companion_agents().await.unwrap();
	let listed: Vec<quant_core::ClientId> = agents.iter().map(|agent| agent.id).collect();
	assert!(listed.contains(&agent_id));
	assert!(listed.contains(&other_agent_id));
	assert!(!listed.contains(&phone_id));
	assert!(
		agents
			.iter()
			.all(|agent| agent.agent.as_ref().is_some_and(|a| !a.product.is_empty()))
	);

	// Submit, then retry the same key: one persisted message, the same id.
	let sent = phone
		.send_companion_message(&send(
			agent_id,
			"Was I writing before noon?",
			"phone-00000001",
		))
		.await
		.unwrap();
	assert_eq!(sent.status, CompanionMessageStatus::Open);
	assert_eq!(sent.from, phone_id);
	assert_eq!(sent.to, agent_id);
	assert!(sent.answer.is_none());
	let retried = phone
		.send_companion_message(&send(
			agent_id,
			"Was I writing before noon?",
			"phone-00000001",
		))
		.await
		.unwrap();
	assert_eq!(
		retried.id, sent.id,
		"a retry must not create a second message"
	);
	assert_eq!(phone.companion_messages(None).await.unwrap().len(), 1);
	assert_eq!(phone.companion_messages(Some(10)).await.unwrap().len(), 1);
	assert_eq!(sent.id, retried.id);

	// The addressed agent pulls it; no other agent does. Listing twice
	// changes nothing and marks nothing read.
	let inbox = agent.inbox(None).await.unwrap();
	assert_eq!(inbox.len(), 1);
	assert_eq!(inbox[0].id, sent.id);
	assert_eq!(inbox[0].question, "Was I writing before noon?");
	assert_eq!(inbox[0].status, CompanionMessageStatus::Open);
	assert_eq!(agent.inbox(None).await.unwrap().len(), 1);
	assert_eq!(agent.inbox(Some(10)).await.unwrap().len(), 1);
	assert!(other_agent.inbox(None).await.unwrap().is_empty());

	// The agent-opened ledger is not this inbox.
	assert!(agent.my_questions().await.unwrap().is_empty());
	assert!(
		h.owner()
			.await
			.questions(&quant_api::QuestionListRequest::default())
			.await
			.unwrap()
			.is_empty()
	);

	// A different agent cannot answer someone else's message.
	let refused = other_agent
		.answer_message(
			sent.id,
			&CompanionAnswer {
				answer: "Not mine.".into(),
				citations: Vec::new(),
			},
		)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("not_found"));

	// A citation outside the agent's grant fails the whole reply, leaving
	// the message open.
	let unreadable_answer = CompanionAnswer {
		answer: "Six hours, apparently.".into(),
		citations: vec![screen, unreadable.id],
	};
	assert_eq!(
		agent
			.answer_message(sent.id, &unreadable_answer)
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
	assert_eq!(
		agent.inbox(None).await.unwrap()[0].status,
		CompanionMessageStatus::Open
	);

	// The addressed agent records the actual reply with a readable
	// citation; retrying it is idempotent.
	let answer = CompanionAnswer {
		answer: "Yes: the release notes were written in Zed before noon.".into(),
		citations: vec![screen],
	};
	let answered = agent.answer_message(sent.id, &answer).await.unwrap();
	assert_eq!(answered.status, CompanionMessageStatus::Answered);
	assert_eq!(
		answered.answer.as_deref(),
		Some("Yes: the release notes were written in Zed before noon.")
	);
	assert_eq!(answered.citations, vec![screen]);
	assert_eq!(
		agent.answer_message(sent.id, &answer).await.unwrap(),
		answered,
		"the same reply retried is the same persisted answer"
	);
	assert!(agent.inbox(None).await.unwrap().is_empty());

	// The requester reads the persisted reply later.
	let read = phone.companion_message(sent.id).await.unwrap();
	assert_eq!(read.status, CompanionMessageStatus::Answered);
	assert_eq!(read.answer, answered.answer);
	assert_eq!(read.citations, vec![screen]);
	let history = phone.companion_messages(None).await.unwrap();
	assert_eq!(history.len(), 1);
	assert_eq!(history[0].id, sent.id);

	// Another companion cannot read this conversation, and the addressed
	// agent cannot use the companion history route.
	assert!(
		other_phone
			.companion_messages(None)
			.await
			.unwrap()
			.is_empty()
	);
	assert_eq!(
		other_phone
			.companion_message(sent.id)
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
	assert_eq!(
		agent.companion_message(sent.id).await.unwrap_err().code(),
		Some("forbidden")
	);

	// The audit trail records the submission and the reply without storing
	// their text in the entry.
	let audit = h
		.owner()
		.await
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	let actions: Vec<&str> = audit.entries.iter().map(|e| e.action.as_str()).collect();
	for action in ["companion.send", "companion.inbox", "companion.answer"] {
		assert!(
			actions.contains(&action),
			"{action} missing from {actions:?}"
		);
	}
	let send_entry = audit
		.entries
		.iter()
		.find(|e| e.action == "companion.send")
		.unwrap();
	assert!(
		!send_entry
			.detail
			.to_string()
			.contains("writing before noon"),
		"audit details must not carry message text"
	);
}

#[tokio::test]
async fn messaging_needs_its_own_permission_and_an_approved_agent() {
	let h = start().await;
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let agent_id = id(&agent).await;
	// A device holding only ordinary read and write scopes is not a
	// companion. `*:raw` and `*:write` must never imply messaging.
	let legacy = h
		.paired(
			"Legacy device",
			ClientKind::Device,
			approve(&["*:raw", "*:write"]),
		)
		.await;
	assert_eq!(
		legacy.companion_agents().await.unwrap_err().code(),
		Some("forbidden")
	);
	assert_eq!(
		legacy
			.send_companion_message(&send(agent_id, "Hello?", "legacy-0001"))
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);
	assert_eq!(
		legacy.companion_messages(None).await.unwrap_err().code(),
		Some("forbidden")
	);

	// An agent cannot act as a companion even when it holds the scope, and a
	// device with the scope cannot pull an agent's inbox.
	let agent_with_scope = h
		.paired(
			"Curious agent",
			ClientKind::Agent,
			approve(&["screen:read", "companion:write"]),
		)
		.await;
	let phone = h
		.paired("Phone", ClientKind::Device, approve(&["companion:write"]))
		.await;
	assert_eq!(
		agent_with_scope
			.companion_agents()
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);
	assert_eq!(
		agent_with_scope
			.send_companion_message(&send(agent_id, "Hello?", "agent-00001"))
			.await
			.unwrap_err()
			.code(),
		Some("forbidden")
	);
	assert_eq!(
		phone.inbox(None).await.unwrap_err().code(),
		Some("forbidden")
	);

	// A companion cannot choose itself, and cannot reach an agent that is
	// still waiting for approval.
	let phone_id = id(&phone).await;
	assert_eq!(
		phone
			.send_companion_message(&send(phone_id, "Hello?", "self-00001"))
			.await
			.unwrap_err()
			.code(),
		Some("invalid")
	);
	let mut pending = Client::new(h.endpoint.clone(), Identity::generate("Pending"));
	let pending_id = pending
		.pair(ClientKind::Agent, &[], Some(&identity("Pending")))
		.await
		.unwrap()
		.client;
	assert_eq!(
		phone
			.send_companion_message(&send(pending_id, "Hello?", "pending-001"))
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
	// And the pending agent is not offered in the picker.
	assert!(
		!phone
			.companion_agents()
			.await
			.unwrap()
			.iter()
			.any(|agent| agent.id == pending_id)
	);
}

#[tokio::test]
async fn submissions_are_bounded_idempotent_and_rate_limited() {
	let h = start().await;
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let agent_id = id(&agent).await;
	let mut request = approve(&["companion:write"]);
	request.limits = Some(Limits {
		questions_per_hour: 2,
		..Limits::default()
	});
	let phone = h.paired("Phone", ClientKind::Device, request).await;

	// Empty text, a bad key, and a key reused for different text are
	// refused before they reach the inbox.
	for (body, code) in [
		(send(agent_id, "   ", "phone-00000001"), "invalid"),
		(send(agent_id, "Hello?", "short"), "invalid"),
		(send(agent_id, "Hello?", "bad key with spaces"), "invalid"),
	] {
		assert_eq!(
			phone
				.send_companion_message(&body)
				.await
				.unwrap_err()
				.code(),
			Some(code)
		);
	}
	let first = phone
		.send_companion_message(&send(agent_id, "First question", "phone-00000001"))
		.await
		.unwrap();
	assert_eq!(
		phone
			.send_companion_message(&send(agent_id, "A different question", "phone-00000001"))
			.await
			.unwrap_err()
			.code(),
		Some("invalid")
	);
	phone
		.send_companion_message(&send(agent_id, "Second question", "phone-00000002"))
		.await
		.unwrap();
	let refused = phone
		.send_companion_message(&send(agent_id, "Third question", "phone-00000003"))
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("rate_limited"));
	assert_eq!(phone.companion_messages(None).await.unwrap().len(), 2);
	// Retrying the first submission after the cap still returns the original
	// message: idempotent retries are not new submissions.
	let retried = phone
		.send_companion_message(&send(agent_id, "First question", "phone-00000001"))
		.await
		.unwrap();
	assert_eq!(retried.id, first.id);
}

#[tokio::test]
async fn revoking_either_side_closes_open_messages() {
	let h = start().await;
	let screen = seed(&h.state);
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let agent_id = id(&agent).await;
	let phone = h
		.paired("Phone", ClientKind::Device, approve(&["companion:write"]))
		.await;

	let answered = phone
		.send_companion_message(&send(agent_id, "First", "phone-00000001"))
		.await
		.unwrap();
	agent
		.answer_message(
			answered.id,
			&CompanionAnswer {
				answer: "A real reply".into(),
				citations: vec![screen],
			},
		)
		.await
		.unwrap();
	let pending = phone
		.send_companion_message(&send(agent_id, "Second", "phone-00000002"))
		.await
		.unwrap();

	// Revoking the agent closes the pending message, refuses new ones, and
	// leaves the already-recorded reply readable by its requester.
	h.owner().await.revoke(&agent_id.to_string()).await.unwrap();
	assert_eq!(
		phone.companion_message(pending.id).await.unwrap().status,
		CompanionMessageStatus::Abandoned
	);
	assert_eq!(
		phone.companion_message(answered.id).await.unwrap().status,
		CompanionMessageStatus::Answered
	);
	assert_eq!(
		phone
			.send_companion_message(&send(agent_id, "Third", "phone-00000003"))
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
	assert_eq!(
		agent.inbox(None).await.unwrap_err().code(),
		Some("unauthenticated")
	);

	// Revoking the companion closes its remaining open messages and stops
	// it reading anything, including its old reply.
	let phone_id = id(&phone).await;
	h.owner().await.revoke(&phone_id.to_string()).await.unwrap();
	assert_eq!(
		phone
			.companion_message(answered.id)
			.await
			.unwrap_err()
			.code(),
		Some("unauthenticated")
	);
	assert_eq!(
		h.state
			.vault
			.store
			.message(pending.id)
			.unwrap()
			.unwrap()
			.status,
		quant_store::MessageStatus::Abandoned
	);
}

#[tokio::test]
async fn withdrawing_companion_permission_abandons_open_messages() {
	let h = start().await;
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let agent_id = id(&agent).await;
	let phone = h
		.paired("Phone", ClientKind::Device, approve(&["companion:write"]))
		.await;
	let phone_id = id(&phone).await;
	let open = phone
		.send_companion_message(&send(agent_id, "Still there?", "phone-00000001"))
		.await
		.unwrap();

	// Re-approving the same companion with ordinary write scopes only
	// withdraws messaging, and closes what was waiting.
	h.owner()
		.await
		.approve(&ApproveRequest {
			client: phone_id.to_string(),
			scopes: Some(vec!["health:write".into()]),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();
	assert_eq!(
		phone.companion_messages(None).await.unwrap_err().code(),
		Some("forbidden")
	);
	assert_eq!(
		h.state
			.vault
			.store
			.message(open.id)
			.unwrap()
			.unwrap()
			.status,
		quant_store::MessageStatus::Abandoned
	);
	assert!(agent.inbox(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_legacy_agent_question_remains_its_own_ledger() {
	let h = start().await;
	let screen = seed(&h.state);
	let agent = h
		.paired("Claude", ClientKind::Agent, approve(&["screen:read"]))
		.await;
	let agent_id = id(&agent).await;
	let phone = h
		.paired("Phone", ClientKind::Device, approve(&["companion:write"]))
		.await;

	// An agent opens its own question, answers it, and sees it in its ledger.
	let opened = agent
		.open_question(&QuestionOpen {
			question: "What did I write?".into(),
			purpose: "the owner asked about their morning".into(),
			model: None,
		})
		.await
		.unwrap();
	agent
		.answer_question(
			opened.id,
			&quant_api::QuestionAnswer {
				answer: "Release notes.".into(),
				citations: vec![screen],
			},
		)
		.await
		.unwrap();
	let mine = agent.my_questions().await.unwrap();
	assert_eq!(mine.len(), 1);
	assert_eq!(mine[0].id, opened.id);

	// A companion message does not join that ledger, and the agent's
	// question does not appear in the inbox or companion history.
	let sent = phone
		.send_companion_message(&send(agent_id, "Companion question", "phone-00000001"))
		.await
		.unwrap();
	assert_eq!(agent.my_questions().await.unwrap().len(), 1);
	assert_eq!(agent.inbox(None).await.unwrap()[0].id, sent.id);
	assert!(phone.my_questions().await.unwrap().is_empty());
	assert_eq!(phone.companion_messages(None).await.unwrap().len(), 1);

	// A message id is not a question id on a data read.
	let stolen =
		quant_client::Purpose::purpose("checking the owner's morning").with_question(sent.id);
	assert_eq!(
		agent
			.records(&quant_api::RecordsRequest::default(), Some(&stolen))
			.await
			.unwrap_err()
			.code(),
		Some("invalid")
	);
}
