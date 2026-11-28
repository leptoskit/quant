//! The MCP server against a live daemon.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::AgentIdentity;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_api::CompanionMessageStatus;
use quant_api::CompanionSend;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::Sink;
use quant_crypto::KdfParams;
use quant_daemon::State;
use quant_mcp::AgentSetup;
use quant_mcp::Server;
use quant_store::Vault;
use quant_store::VaultPaths;
use serde_json::Value;
use serde_json::json;

const PASS: &str = "correct horse battery staple";

async fn daemon() -> (tempfile::TempDir, Endpoint, Arc<State>) {
	let dir = tempfile::tempdir().unwrap();
	let paths = VaultPaths::new(dir.path().join("v"));
	let state = Arc::new(
		State::new(Vault::create(paths.clone(), PASS, KdfParams::INSECURE_FAST).unwrap()).unwrap(),
	);
	let socket = paths.socket();
	let (serving, path) = (Arc::clone(&state), socket.clone());
	tokio::spawn(async move {
		quant_daemon::serve_socket(serving, &path, std::future::pending())
			.await
			.unwrap();
	});
	while !socket.exists() {
		tokio::time::sleep(std::time::Duration::from_millis(5)).await;
	}
	(dir, Endpoint::Unix(socket), state)
}

/// What the test agent says it is when it pairs.
fn identity() -> AgentIdentity {
	AgentIdentity {
		product: "Claude Code".into(),
		vendor: "Anthropic".into(),
		model: "claude-opus-5-5".into(),
		version: "2.1.0".into(),
		host: "test-host".into(),
		operator: None,
	}
}

/// The setup the server needs to pair: already paired here, so the
/// identity is only what the tool calls carry as the model.
fn setup() -> AgentSetup {
	AgentSetup {
		agent: identity(),
		scopes: Vec::new(),
		identity_path: None,
	}
}

async fn paired(endpoint: &Endpoint, scopes: Option<&[&str]>) -> Client {
	let mut client = Client::new(endpoint.clone(), Identity::generate("Claude"));
	let pairing = client
		.pair(ClientKind::Agent, &[], Some(&identity()))
		.await
		.unwrap();
	if let Some(scopes) = scopes {
		let owner = OwnerSession::open(endpoint.clone(), PASS).await.unwrap();
		owner
			.approve(&ApproveRequest {
				client: pairing.pairing_code,
				scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
				label: None,
				lookback_hours: None,
				expires_in_hours: None,
				limits: None,
				unredacted: false,
				deny_classes: None,
			})
			.await
			.unwrap();
	}
	client
}

/// Pair and approve a companion device with the explicit messaging scope.
async fn paired_phone(endpoint: &Endpoint, name: &str) -> Client {
	let mut client = Client::new(endpoint.clone(), Identity::generate(name));
	let pairing = client.pair(ClientKind::Device, &[], None).await.unwrap();
	let owner = OwnerSession::open(endpoint.clone(), PASS).await.unwrap();
	owner
		.approve(&ApproveRequest {
			client: pairing.pairing_code,
			scopes: Some(vec!["companion:write".into()]),
			label: None,
			lookback_hours: None,
			expires_in_hours: None,
			limits: None,
			unredacted: false,
			deny_classes: None,
		})
		.await
		.unwrap();
	client
}

async fn call(server: &mut Server, id: u64, method: &str, params: Value) -> Value {
	server
		.handle(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
		.await
		.unwrap()
}

fn tool_names(list: &Value) -> Vec<String> {
	list["result"]["tools"]
		.as_array()
		.unwrap()
		.iter()
		.map(|t| t["name"].as_str().unwrap().to_owned())
		.collect()
}

#[tokio::test]
async fn unapproved_agents_only_see_how_to_get_approved() {
	let (_dir, endpoint, _state) = daemon().await;
	let mut server = Server::new(paired(&endpoint, None).await, setup());
	let init = call(
		&mut server,
		1,
		"initialize",
		json!({ "protocolVersion": "2025-06-18" }),
	)
	.await;
	assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
	assert!(
		init["result"]["instructions"]
			.as_str()
			.unwrap()
			.contains("untrusted")
	);
	assert!(
		server
			.handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
			.await
			.is_none()
	);

	let list = call(&mut server, 2, "tools/list", json!({})).await;
	assert_eq!(tool_names(&list), vec!["quant_access_status"]);
	let status = call(
		&mut server,
		3,
		"tools/call",
		json!({ "name": "quant_access_status" }),
	)
	.await;
	assert!(
		status["result"]["content"][0]["text"]
			.as_str()
			.unwrap()
			.contains("quant approve")
	);
	let refused = call(
		&mut server,
		4,
		"tools/call",
		json!({ "name": "quant_search", "arguments": { "query": "x" } }),
	)
	.await;
	assert_eq!(refused["result"]["isError"], true);
}

#[tokio::test]
async fn initialize_pairs_with_what_client_info_says() {
	let (_dir, endpoint, _state) = daemon().await;
	let client = Client::new(endpoint.clone(), Identity::generate("Claude (MCP)"));
	// The configuration names the vendor and model; the MCP host names
	// itself and its version.
	let mut server = Server::new(
		client,
		AgentSetup {
			agent: AgentIdentity {
				product: String::new(),
				version: String::new(),
				..identity()
			},
			scopes: vec!["apps:summary".into()],
			identity_path: None,
		},
	);
	call(
		&mut server,
		1,
		"initialize",
		json!({
			"protocolVersion": "2025-06-18",
			"clientInfo": { "name": "claude-code", "version": "2.1.0" }
		}),
	)
	.await;
	let owner = OwnerSession::open(endpoint, PASS).await.unwrap();
	let clients = owner.clients().await.unwrap();
	let [pending] = clients.as_slice() else {
		panic!("one pending client, got {clients:?}");
	};
	assert_eq!(pending.kind, ClientKind::Agent);
	assert_eq!(pending.requested, vec!["apps:summary".to_owned()]);
	let agent = pending.agent.as_ref().expect("the agent said what it is");
	assert_eq!(agent.product, "claude-code");
	assert_eq!(agent.version, "2.1.0");
	assert_eq!(agent.vendor, "Anthropic");
	assert_eq!(agent.model, "claude-opus-5-5");
}

#[tokio::test]
async fn tools_follow_the_grant() {
	let (_dir, endpoint, state) = daemon().await;
	let device = DeviceId::new();
	let now = Timestamp::now();
	state
		.record(
			Record::new("apps.focus".parse().unwrap(), device)
				.starting(now - SignedDuration::from_mins(30))
				.ending(now - SignedDuration::from_mins(10))
				.with("app", "Zed"),
		)
		.unwrap();

	let mut summary = Server::new(paired(&endpoint, Some(&["apps:summary"])).await, setup());
	let list = call(&mut summary, 1, "tools/list", json!({})).await;
	assert_eq!(
		tool_names(&list),
		vec![
			"quant_access_status",
			"quant_inbox",
			"quant_inbox_answer",
			"quant_begin_question",
			"quant_answer",
			"quant_overview",
			"quant_streams",
			"quant_time_spent",
			"quant_series"
		]
	);
	let spent = call(
		&mut summary,
		2,
		"tools/call",
		json!({ "name": "quant_time_spent", "arguments": { "stream": "apps.focus", "group_by": "app", "from": "-1h", "purpose": "seeing which app the owner used" } }),
	)
	.await;
	let text = spent["result"]["content"][0]["text"].as_str().unwrap();
	let value: Value = serde_json::from_str(text).unwrap();
	assert_eq!(value["items"][0]["group"], "Zed");
	assert!((value["items"][0]["seconds"].as_f64().unwrap() - 1200.0).abs() < 1.0);

	let mut full = Server::new(
		paired(&endpoint, Some(&["*:raw", "memory:write"])).await,
		setup(),
	);
	let list = call(&mut full, 1, "tools/list", json!({})).await;
	for name in [
		"quant_timeline",
		"quant_search",
		"quant_media",
		"quant_note",
	] {
		assert!(tool_names(&list).contains(&name.to_owned()), "{name}");
	}
	assert!(
		!tool_names(&list).contains(&"quant_save_audio".to_owned()),
		"audio:write was never granted"
	);
	let note = call(&mut full, 2, "tools/call", json!({ "name": "quant_note", "arguments": { "text": "Owner focuses best before noon", "tags": ["pattern"] } })).await;
	assert_eq!(note["result"]["isError"], false);
	let found = call(
		&mut full,
		3,
		"tools/call",
		json!({ "name": "quant_search", "arguments": { "query": "noon", "purpose": "finding what the owner wrote about focus" } }),
	)
	.await;
	let text = found["result"]["content"][0]["text"].as_str().unwrap();
	assert!(text.contains("Owner focuses best before noon"));
	assert!(text.contains("captured_text_is_untrusted"));

	let bad_time = call(
		&mut full,
		4,
		"tools/call",
		json!({ "name": "quant_timeline", "arguments": { "from": "whenever", "purpose": "seeing when the owner worked" } }),
	)
	.await;
	assert_eq!(bad_time["result"]["isError"], true);
	let unknown = call(&mut full, 5, "nope", json!({})).await;
	assert_eq!(unknown["error"]["code"], -32601);
}

#[tokio::test]
async fn profile_propose_and_context_follow_the_grant() {
	let (_dir, endpoint, state) = daemon().await;
	let device = DeviceId::new();
	let evidence = Record::new("screen.frame".parse().unwrap(), device)
		.with_text("Wrote in the morning, focused, with espresso");
	let evidence_id = evidence.id;
	state.record(evidence).unwrap();
	let owner = OwnerSession::open(endpoint.clone(), PASS).await.unwrap();
	owner
		.tell(&quant_api::OwnerClaimRequest {
			statement: "Prefers espresso after lunch".into(),
			kind: Some(quant_core::ClaimKind::Preference),
			topic: None,
			subject: Some("me".into()),
			classes: Vec::new(),
		})
		.await
		.unwrap();

	// A grant that can read the profile gets the tools and the resource.
	let mut reader = Server::new(
		paired(
			&endpoint,
			Some(&["memory.profile:read", "memory.profile:write", "screen:read"]),
		)
		.await,
		setup(),
	);
	let list = call(&mut reader, 1, "tools/list", json!({})).await;
	for name in ["quant_profile", "quant_propose", "quant_context"] {
		assert!(tool_names(&list).contains(&name.to_owned()), "{name}");
	}

	// quant_profile serves the owner's claim.
	let profile = call(
		&mut reader,
		2,
		"tools/call",
		json!({ "name": "quant_profile", "arguments": { "purpose": "learning the owner's stated preferences" } }),
	)
	.await;
	let text = profile["result"]["content"][0]["text"].as_str().unwrap();
	assert!(text.contains("Prefers espresso after lunch"));
	assert!(text.contains("\"level\":\"read\""));

	// The resource lists and reads the same content.
	let resources = call(&mut reader, 3, "resources/list", json!({})).await;
	assert_eq!(
		resources["result"]["resources"][0]["uri"],
		"quant://profile"
	);
	let read = call(
		&mut reader,
		4,
		"resources/read",
		json!({ "uri": "quant://profile" }),
	)
	.await;
	let text = read["result"]["contents"][0]["text"].as_str().unwrap();
	assert!(text.contains("Prefers espresso after lunch"));
	assert_eq!(read["error"], json!(null));
	let missing = call(
		&mut reader,
		5,
		"resources/read",
		json!({ "uri": "quant://nope" }),
	)
	.await;
	assert_eq!(missing["error"]["code"], -32002);

	// quant_propose stores a held proposal, grounded in readable evidence.
	let proposed = call(
		&mut reader,
		6,
		"tools/call",
		json!({ "name": "quant_propose", "arguments": {
			"statement": "Focuses best before noon",
			"kind": "routine",
			"confidence": 0.8,
			"evidence": [evidence_id.to_string()],
		} }),
	)
	.await;
	assert_eq!(proposed["result"]["isError"], false);
	let held = owner
		.claims(&quant_api::ClaimListRequest {
			status: Some(quant_core::ClaimStatus::Held),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!(held.len(), 1);

	// quant_context returns a budgeted packet.
	let context = call(
		&mut reader,
		7,
		"tools/call",
		json!({ "name": "quant_context", "arguments": { "task": "espresso", "budget_tokens": 500, "purpose": "planning the owner's afternoon" } }),
	)
	.await;
	let text = context["result"]["content"][0]["text"].as_str().unwrap();
	let value: Value = serde_json::from_str(text).unwrap();
	assert!(value["profile"]["included"].as_bool().unwrap());
	assert!(
		value["profile"]["items"][0]["statement"]
			.as_str()
			.unwrap()
			.contains("espresso")
	);
	assert_eq!(value["profile_claims_are_the_owners_words"], true);

	// A grant without the profile sees none of the three, and no resource.
	let mut blind = Server::new(paired(&endpoint, Some(&["apps:summary"])).await, setup());
	let list = call(&mut blind, 1, "tools/list", json!({})).await;
	for name in ["quant_profile", "quant_propose", "quant_context"] {
		assert!(!tool_names(&list).contains(&name.to_owned()), "{name}");
	}
	let resources = call(&mut blind, 2, "resources/list", json!({})).await;
	assert_eq!(resources["result"]["resources"], json!([]));
	let refused = call(
		&mut blind,
		3,
		"tools/call",
		json!({ "name": "quant_profile", "arguments": { "purpose": "learning the owner's stated preferences" } }),
	)
	.await;
	assert_eq!(refused["result"]["isError"], true);
}

#[tokio::test]
async fn a_question_flows_through_begin_read_and_answer() {
	let (_dir, endpoint, state) = daemon().await;
	let device = DeviceId::new();
	let record = Record::new("screen.frame".parse().unwrap(), device)
		.starting(Timestamp::now() - SignedDuration::from_mins(5))
		.with_text("Wrote the release notes in Zed before noon.");
	let cited = record.id;
	state.record(record).unwrap();

	let mut server = Server::new(paired(&endpoint, Some(&["screen:read"])).await, setup());
	let begun = call(
		&mut server,
		1,
		"tools/call",
		json!({ "name": "quant_begin_question", "arguments": {
			"question": "Was I writing before noon?",
			"purpose": "the owner asked what their mornings look like",
		} }),
	)
	.await;
	assert_eq!(begun["result"]["isError"], false);
	let opened: Value =
		serde_json::from_str(begun["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
	let question = opened["id"].as_str().unwrap().to_owned();

	// The read carries the question id, so it lands in that question's
	// trail rather than the audit log alone.
	let found = call(
		&mut server,
		2,
		"tools/call",
		json!({ "name": "quant_search", "arguments": {
			"query": "release notes",
			"purpose": "checking the owner's morning writing",
			"question_id": question,
		} }),
	)
	.await;
	assert_eq!(found["result"]["isError"], false);

	let answered = call(
		&mut server,
		3,
		"tools/call",
		json!({ "name": "quant_answer", "arguments": {
			"question_id": question,
			"answer": "Yes: the release notes were written in Zed before noon.",
			"citations": [cited.0.to_string()],
		} }),
	)
	.await;
	assert_eq!(answered["result"]["isError"], false);
	let answer: Value =
		serde_json::from_str(answered["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
	assert_eq!(answer["status"], "answered");
	assert_eq!(answer["citations"][0], cited.0.to_string());

	// A data tool without a purpose is refused before it reaches the vault.
	let refused = call(
		&mut server,
		4,
		"tools/call",
		json!({ "name": "quant_search", "arguments": { "query": "anything" } }),
	)
	.await;
	assert_eq!(refused["result"]["isError"], true);
	assert!(
		refused["result"]["content"][0]["text"]
			.as_str()
			.unwrap()
			.contains("purpose")
	);

	// The owner reads the question, the answer, and the read behind it.
	let trail = OwnerSession::open(endpoint, PASS)
		.await
		.unwrap()
		.question_trail(question.parse().unwrap())
		.await
		.unwrap();
	assert_eq!(trail.question.question, "Was I writing before noon?");
	assert_eq!(
		trail.question.answer.as_deref(),
		Some("Yes: the release notes were written in Zed before noon.")
	);
	let actions: Vec<&str> = trail.entries.iter().map(|e| e.action.as_str()).collect();
	for expected in ["question.open", "search", "question.answer"] {
		assert!(
			actions.contains(&expected),
			"{expected} missing from {actions:?}"
		);
	}
}

#[tokio::test]
async fn companion_messages_are_pulled_and_answered_over_mcp() {
	let (_dir, endpoint, state) = daemon().await;
	let record = Record::new("screen.frame".parse().unwrap(), DeviceId::new())
		.starting(Timestamp::now() - SignedDuration::from_mins(5))
		.with_text("Wrote the release notes in Zed before noon.");
	let cited = record.id;
	state.record(record).unwrap();

	let agent = paired(&endpoint, Some(&["screen:read"])).await;
	let agent_id = agent.me().await.unwrap().client;
	let phone = paired_phone(&endpoint, "Phone").await;
	let selected = phone
		.companion_agents()
		.await
		.unwrap()
		.into_iter()
		.find(|agent| agent.id == agent_id)
		.expect("the approved agent is offered to the companion");
	let sent = phone
		.send_companion_message(&CompanionSend {
			to: selected.id,
			question: "Was I writing before noon?".into(),
			purpose: Some("the owner asked from their phone".into()),
			dedupe: "phone-mcp-0001".into(),
		})
		.await
		.unwrap();

	// The inbox and its answer tool are discoverable for an approved agent.
	let mut server = Server::new(agent, setup());
	let list = call(&mut server, 1, "tools/list", json!({})).await;
	let names = tool_names(&list);
	assert!(names.contains(&"quant_inbox".to_owned()));
	assert!(names.contains(&"quant_inbox_answer".to_owned()));

	// Pulling twice is still a pull: the message stays open and unread.
	for id in [2, 3] {
		let inbox = call(
			&mut server,
			id,
			"tools/call",
			json!({ "name": "quant_inbox", "arguments": {} }),
		)
		.await;
		assert_eq!(inbox["result"]["isError"], false);
		let rows: Value =
			serde_json::from_str(inbox["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
		assert_eq!(rows.as_array().unwrap().len(), 1);
		assert_eq!(rows[0]["id"], sent.id.0.to_string());
		assert_eq!(rows[0]["status"], "open");
		assert_eq!(rows[0]["answer"], json!(null));
	}

	// The agent records its actual reply, citing a record it can read.
	let answered = call(
		&mut server,
		4,
		"tools/call",
		json!({ "name": "quant_inbox_answer", "arguments": {
			"message_id": sent.id.0.to_string(),
			"answer": "Yes: the release notes were written in Zed before noon.",
			"citations": [cited.0.to_string()],
		} }),
	)
	.await;
	assert_eq!(answered["result"]["isError"], false);
	let view: Value =
		serde_json::from_str(answered["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
	assert_eq!(view["status"], "answered");
	assert_eq!(view["citations"][0], cited.0.to_string());

	// The companion reads the same persisted reply over its typed client.
	let read = phone.companion_message(sent.id).await.unwrap();
	assert_eq!(read.status, CompanionMessageStatus::Answered);
	assert_eq!(
		read.answer.as_deref(),
		Some("Yes: the release notes were written in Zed before noon.")
	);
	// The inbox is now empty, and another agent never saw it.
	let inbox = call(
		&mut server,
		5,
		"tools/call",
		json!({ "name": "quant_inbox", "arguments": {} }),
	)
	.await;
	assert_eq!(inbox["result"]["content"][0]["text"], "[]");

	let mut other = Server::new(paired(&endpoint, Some(&["screen:read"])).await, setup());
	let other_inbox = call(
		&mut other,
		1,
		"tools/call",
		json!({ "name": "quant_inbox", "arguments": {} }),
	)
	.await;
	assert_eq!(other_inbox["result"]["content"][0]["text"], "[]");
}
