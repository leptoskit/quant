//! The MCP server against a live daemon.

use std::sync::Arc;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_api::ApproveRequest;
use quant_api::ClientKind;
use quant_client::Client;
use quant_client::Endpoint;
use quant_client::Identity;
use quant_client::OwnerSession;
use quant_core::DeviceId;
use quant_core::Record;
use quant_core::Sink;
use quant_crypto::KdfParams;
use quant_daemon::State;
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

async fn paired(endpoint: &Endpoint, scopes: Option<&[&str]>) -> Client {
	let mut client = Client::new(endpoint.clone(), Identity::generate("Claude"));
	let pairing = client.pair(ClientKind::Agent, &[]).await.unwrap();
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
	let mut server = Server::new(paired(&endpoint, None).await);
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

	let mut summary = Server::new(paired(&endpoint, Some(&["apps:summary"])).await);
	let list = call(&mut summary, 1, "tools/list", json!({})).await;
	assert_eq!(
		tool_names(&list),
		vec![
			"quant_access_status",
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
		json!({ "name": "quant_time_spent", "arguments": { "stream": "apps.focus", "group_by": "app", "from": "-1h" } }),
	)
	.await;
	let text = spent["result"]["content"][0]["text"].as_str().unwrap();
	let value: Value = serde_json::from_str(text).unwrap();
	assert_eq!(value["items"][0]["group"], "Zed");
	assert!((value["items"][0]["seconds"].as_f64().unwrap() - 1200.0).abs() < 1.0);

	let mut full = Server::new(paired(&endpoint, Some(&["*:raw", "memory:write"])).await);
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
		json!({ "name": "quant_search", "arguments": { "query": "noon" } }),
	)
	.await;
	let text = found["result"]["content"][0]["text"].as_str().unwrap();
	assert!(text.contains("Owner focuses best before noon"));
	assert!(text.contains("captured_text_is_untrusted"));

	let bad_time = call(
		&mut full,
		4,
		"tools/call",
		json!({ "name": "quant_timeline", "arguments": { "from": "whenever" } }),
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
		json!({ "name": "quant_profile" }),
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
		json!({ "name": "quant_context", "arguments": { "task": "espresso", "budget_tokens": 500 } }),
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
	let mut blind = Server::new(paired(&endpoint, Some(&["apps:summary"])).await);
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
		json!({ "name": "quant_profile" }),
	)
	.await;
	assert_eq!(refused["result"]["isError"], true);
}
