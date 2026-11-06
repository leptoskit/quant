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
