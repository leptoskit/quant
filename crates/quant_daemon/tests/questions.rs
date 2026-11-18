//! Agent accountability end to end: identity at pairing, a purpose on every
//! read, and the question ledger the owner can read back.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::AgentIdentity;
use quant_access::Limits;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::QuestionAnswer;
use quant_api::QuestionListRequest;
use quant_api::QuestionOpen;
use quant_api::QuestionStatus;
use quant_api::RecordsRequest;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_client::Purpose;
use quant_core::DeviceId;
use quant_core::Record;
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

	async fn paired(&self, name: &str, approve: ApproveRequest) -> Client {
		let mut client = Client::new(self.endpoint.clone(), Identity::generate(name));
		let pairing = client
			.pair(ClientKind::Agent, &[], Some(&identity(name)))
			.await
			.unwrap();
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

/// Why an agent is reading.
fn purpose() -> Purpose {
	Purpose::purpose("answering the owner's question about their week")
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

/// Two readable records and one health record, so citations can be tested
/// against what a grant may and may not read.
fn seed(state: &State) -> quant_core::RecordId {
	let device = DeviceId::new();
	let now = Timestamp::now();
	let screen = Record::new("screen.frame".parse().unwrap(), device)
		.starting(now - SignedDuration::from_mins(30))
		.with_text("Wrote the release notes in Zed before noon.");
	for record in [
		screen.clone(),
		Record::new("apps.focus".parse().unwrap(), device)
			.starting(now - SignedDuration::from_mins(60))
			.ending(now - SignedDuration::from_mins(20))
			.with("app", "Zed"),
		Record::new("health.heart_rate".parse().unwrap(), device)
			.starting(now - SignedDuration::from_mins(10))
			.with("bpm", 61.0),
	] {
		state.record(record).unwrap();
	}
	screen.id
}

#[tokio::test]
async fn an_agent_must_say_what_it_is_before_it_can_pair() {
	let h = start().await;
	let mut client = Client::new(h.endpoint.clone(), Identity::generate("Anonymous"));
	let refused = client.pair(ClientKind::Agent, &[], None).await.unwrap_err();
	assert_eq!(refused.code(), Some("invalid"));
	// Nothing was left waiting for the owner to approve.
	assert!(h.owner().await.clients().await.unwrap().is_empty());

	// The pairing request is audited as a failure, with what it claimed.
	let audit = h.state.vault.store.audit_tail(10).unwrap();
	assert!(audit.iter().any(|entry| {
		entry.action == "pair.request"
			&& matches!(entry.outcome, quant_access::audit::Outcome::Failed)
	}));

	// With an identity, it pairs and the owner sees what it said.
	let pairing = client
		.pair(ClientKind::Agent, &[], Some(&identity("Claude Code")))
		.await
		.unwrap();
	let waiting = h.owner().await.clients().await.unwrap();
	assert_eq!(waiting.len(), 1);
	assert_eq!(waiting[0].pairing_code, pairing.pairing_code);
	let agent = waiting[0].agent.as_ref().expect("the identity is shown");
	assert_eq!(
		agent.summary(),
		"Claude Code (Quant tests) test-model 0.1.0 on test-host"
	);
}

#[tokio::test]
async fn an_agent_read_without_a_purpose_is_refused_and_audited() {
	let h = start().await;
	seed(&h.state);
	let agent = h.paired("Reader", approve(&["screen:read"])).await;

	let refused = agent
		.records(&RecordsRequest::default(), None)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("purpose_required"));

	let audit = h
		.owner()
		.await
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	assert!(
		audit.entries.iter().any(|entry| {
			entry.action == "request"
				&& matches!(
					entry.outcome,
					quant_access::audit::Outcome::Denied {
						reason: quant_access::Denied::PurposeRequired
					}
				)
		}),
		"the purposeless read was not audited"
	);

	// A purpose that is not a purpose is refused the same way.
	let too_short = Purpose::purpose("short");
	assert_eq!(
		agent
			.records(&RecordsRequest::default(), Some(&too_short))
			.await
			.unwrap_err()
			.code(),
		Some("purpose_required")
	);
}

#[tokio::test]
async fn a_read_with_a_purpose_is_audited_with_it_and_the_model() {
	let h = start().await;
	seed(&h.state);
	let agent = h.paired("Reader", approve(&["screen:read"])).await;
	let meta = purpose().with_model("claude-opus-5-5");
	let seen = agent
		.records(&RecordsRequest::default(), Some(&meta))
		.await
		.unwrap();
	assert_eq!(seen.records.len(), 1);

	let audit = h
		.owner()
		.await
		.audit(&quant_api::AuditRequest::default())
		.await
		.unwrap();
	let entry = audit
		.entries
		.iter()
		.find(|entry| entry.action == "records")
		.expect("the read was audited");
	assert_eq!(
		entry.purpose.as_deref(),
		Some("answering the owner's question about their week")
	);
	assert_eq!(entry.model.as_deref(), Some("claude-opus-5-5"));
	assert_eq!(entry.question, None);
	assert!(matches!(
		entry.outcome,
		quant_access::audit::Outcome::Allowed { records: 1, .. }
	));
}

#[tokio::test]
async fn a_question_records_its_reads_its_answer_and_its_citations() {
	let h = start().await;
	let screen = seed(&h.state);
	let agent = h.paired("Reader", approve(&["screen:read"])).await;

	let opened = agent
		.open_question(&QuestionOpen {
			question: "Was I writing before noon?".into(),
			purpose: "the owner asked what their mornings look like".into(),
			model: Some("claude-opus-5-5".into()),
		})
		.await
		.unwrap();
	let meta = Purpose::purpose("reading the morning's screens for the owner's question")
		.with_question(opened.id)
		.with_model("claude-opus-5-5");
	let seen = agent
		.records(&RecordsRequest::default(), Some(&meta))
		.await
		.unwrap();
	assert_eq!(seen.records.len(), 1);

	let answered = agent
		.answer_question(
			opened.id,
			&QuestionAnswer {
				answer: "Yes: the release notes were written in Zed before noon.".into(),
				citations: vec![screen],
			},
		)
		.await
		.unwrap();
	assert_eq!(answered.status, QuestionStatus::Answered);
	assert_eq!(answered.citations, vec![screen]);

	// The owner reads the whole trail: the question, the answer, and every
	// read that carried its id, with the purpose and model each stated.
	let trail = h.owner().await.question_trail(opened.id).await.unwrap();
	assert_eq!(trail.question.question, "Was I writing before noon?");
	assert_eq!(
		trail.question.answer.as_deref(),
		Some("Yes: the release notes were written in Zed before noon.")
	);
	assert_eq!(trail.question.records, 1);
	let actions: Vec<&str> = trail.entries.iter().map(|e| e.action.as_str()).collect();
	for expected in ["question.open", "records", "question.answer"] {
		assert!(
			actions.contains(&expected),
			"{expected} missing from {actions:?}"
		);
	}
	assert!(
		trail.entries.iter().all(|e| e.question == Some(opened.id)),
		"an entry in the trail does not carry the question id"
	);
	let read = trail
		.entries
		.iter()
		.find(|e| e.action == "records")
		.unwrap();
	assert_eq!(
		read.purpose.as_deref(),
		Some("reading the morning's screens for the owner's question")
	);
	assert_eq!(read.model.as_deref(), Some("claude-opus-5-5"));

	// The owner's list shows it too, with the same answer.
	let listed = h
		.owner()
		.await
		.questions(&QuestionListRequest::default())
		.await
		.unwrap();
	assert_eq!(listed.len(), 1);
	assert_eq!(listed[0].id, opened.id);
	assert_eq!(listed[0].status, QuestionStatus::Answered);
}

#[tokio::test]
async fn another_agent_can_neither_see_nor_answer_a_question() {
	let h = start().await;
	seed(&h.state);
	let first = h.paired("First", approve(&["screen:read"])).await;
	let second = h.paired("Second", approve(&["screen:read"])).await;
	let opened = first
		.open_question(&QuestionOpen {
			question: "What did I write this morning?".into(),
			purpose: "the owner asked about their morning".into(),
			model: None,
		})
		.await
		.unwrap();

	// The other agent's own list is empty, and it cannot write into the
	// first agent's trail: a question that is not its own is not found.
	assert!(second.my_questions().await.unwrap().is_empty());
	let refused = second
		.answer_question(
			opened.id,
			&QuestionAnswer {
				answer: "Something else entirely.".into(),
				citations: Vec::new(),
			},
		)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("not_found"));

	// A read that claims the other agent's question is refused as unknown,
	// so ids cannot be probed or poisoned.
	let stolen = purpose().with_question(opened.id);
	assert_eq!(
		second
			.records(&RecordsRequest::default(), Some(&stolen))
			.await
			.unwrap_err()
			.code(),
		Some("invalid")
	);
	// The first agent's question is untouched, still open, still empty.
	let mine = first.my_questions().await.unwrap();
	assert_eq!(mine.len(), 1);
	assert_eq!(mine[0].status, QuestionStatus::Open);
	assert!(mine[0].answer.is_none());
}

#[tokio::test]
async fn an_answer_cannot_cite_what_the_grant_cannot_read() {
	let h = start().await;
	let screen = seed(&h.state);
	let unreadable =
		Record::new("health.sleep".parse().unwrap(), DeviceId::new()).with_text("slept six hours");
	h.state.record(unreadable.clone()).unwrap();
	let agent = h.paired("Reader", approve(&["screen:read"])).await;
	let opened = agent
		.open_question(&QuestionOpen {
			question: "How did I sleep?".into(),
			purpose: "the owner asked how they slept".into(),
			model: None,
		})
		.await
		.unwrap();

	// One unreadable citation fails the whole answer, as not found.
	let refused = agent
		.answer_question(
			opened.id,
			&QuestionAnswer {
				answer: "Six hours, apparently.".into(),
				citations: vec![screen, unreadable.id],
			},
		)
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("not_found"));

	// Nothing was written and the question is still open.
	let mine = agent.my_questions().await.unwrap();
	assert_eq!(mine[0].status, QuestionStatus::Open);
	assert!(mine[0].answer.is_none());
	assert!(mine[0].citations.is_empty());

	// A record that was purged cannot be cited either: same answer.
	assert_eq!(
		agent
			.answer_question(
				opened.id,
				&QuestionAnswer {
					answer: "Six hours, apparently.".into(),
					citations: vec![quant_core::RecordId::new()],
				},
			)
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
}

#[tokio::test]
async fn the_hourly_question_cap_is_enforced() {
	let h = start().await;
	let mut request = approve(&["screen:read"]);
	request.limits = Some(Limits {
		questions_per_hour: 2,
		..Limits::default()
	});
	let agent = h.paired("Chatty", request).await;
	for index in 0..2 {
		agent
			.open_question(&QuestionOpen {
				question: format!("Question {index}"),
				purpose: "the owner asked a question".into(),
				model: None,
			})
			.await
			.unwrap();
	}
	let refused = agent
		.open_question(&QuestionOpen {
			question: "One too many".into(),
			purpose: "the owner asked another question".into(),
			model: None,
		})
		.await
		.unwrap_err();
	assert_eq!(refused.code(), Some("rate_limited"));
	assert_eq!(agent.my_questions().await.unwrap().len(), 2);
}

#[tokio::test]
async fn questions_left_unanswered_for_a_day_become_abandoned() {
	let h = start().await;
	let agent = h.paired("Reader", approve(&["screen:read"])).await;
	let me = agent.me().await.unwrap();
	let grant = me.grant.expect("approved while pairing");
	// A question opened a day ago, as the store would hold it.
	let stale = quant_store::Question {
		id: quant_core::QuestionId::new(),
		client: me.client,
		grant: grant.id,
		asked_at: Timestamp::now() - SignedDuration::from_hours(25),
		question: "What did I do yesterday?".into(),
		purpose: "the owner asked what yesterday looked like".into(),
		model: None,
		status: quant_store::QuestionStatus::Open,
		answer: None,
		answered_at: None,
		citations: Vec::new(),
		records: 0,
		bytes: 0,
	};
	h.state.vault.store.insert_question(&stale).unwrap();

	// Listing the agent's questions is enough to close it.
	let mine = agent.my_questions().await.unwrap();
	assert_eq!(mine.len(), 1);
	assert_eq!(mine[0].status, QuestionStatus::Abandoned);

	// An abandoned question takes no answer and no more reads.
	assert_eq!(
		agent
			.answer_question(
				stale.id,
				&QuestionAnswer {
					answer: "Too late.".into(),
					citations: Vec::new(),
				},
			)
			.await
			.unwrap_err()
			.code(),
		Some("not_found")
	);
	assert_eq!(
		agent
			.records(
				&RecordsRequest::default(),
				Some(&purpose().with_question(stale.id))
			)
			.await
			.unwrap_err()
			.code(),
		Some("invalid")
	);
}
