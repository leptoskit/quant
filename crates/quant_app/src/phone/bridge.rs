//! Calls to the phone's native service, `quant-phone`.

use quant_model::phone::Command;
use quant_model::phone::Reply;
use quant_model::phone::Snapshot;

/// Wait for a snapshot newer than `seen`.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub async fn watch(seen: u64) -> Result<Snapshot, String> {
	let bytes = call(quant_model::phone::WATCH, &seen.to_le_bytes()).await?;
	serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Ask the service to do something.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub async fn send(command: &Command) -> Reply {
	let payload = match serde_json::to_vec(command) {
		Ok(payload) => payload,
		Err(error) => return Reply::failed(error.to_string()),
	};
	answer(call(quant_model::phone::COMMAND, &payload).await)
}

/// Hand the service one encoded [`quant_model::phone::Reading`].
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub async fn reading(frame: &[u8]) -> Reply {
	answer(call(quant_model::phone::READING, frame).await)
}

#[cfg(all(target_arch = "wasm32", feature = "native"))]
fn answer(bytes: Result<Vec<u8>, String>) -> Reply {
	bytes
		.and_then(|bytes| serde_json::from_slice::<Reply>(&bytes).map_err(|e| e.to_string()))
		.unwrap_or_else(Reply::failed)
}

#[cfg(all(target_arch = "wasm32", feature = "native"))]
async fn call(method: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
	let client = leptoskit_bridge::guest::Client::new(quant_model::phone::SERVICE)
		.map_err(|e| e.to_string())?;
	let bytes = client
		.call(leptoskit_bridge::MethodId(method), payload)
		.map_err(|e| e.to_string())?
		.await
		.map_err(|e| e.to_string())?;
	Ok(bytes.to_vec())
}

/// Outside the native guest there is no service.
#[cfg(not(all(target_arch = "wasm32", feature = "native")))]
pub async fn watch(_seen: u64) -> Result<Snapshot, String> {
	std::future::pending().await
}

/// Outside the native guest there is no service.
#[cfg(not(all(target_arch = "wasm32", feature = "native")))]
#[allow(
	clippy::unused_async,
	reason = "it stands in for the guest's async call"
)]
pub async fn send(_command: &Command) -> Reply {
	Reply::failed("no native service")
}

/// Outside the native guest there is no service.
#[cfg(not(all(target_arch = "wasm32", feature = "native")))]
#[allow(
	clippy::unused_async,
	reason = "it stands in for the guest's async call"
)]
pub async fn reading(_frame: &[u8]) -> Reply {
	Reply::failed("no native service")
}
