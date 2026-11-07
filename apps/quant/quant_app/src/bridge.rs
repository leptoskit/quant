//! Calls to the app's native service: the window's only way out.

use quant_model::Command;
use quant_model::Reply;
use quant_model::Snapshot;

/// Wait for a snapshot newer than `seen`.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub async fn watch(seen: u64) -> Result<Snapshot, String> {
	let bytes = client()?
		.call(
			leptoskit_bridge::MethodId(quant_model::WATCH),
			&seen.to_le_bytes(),
		)
		.map_err(|e| e.to_string())?
		.await
		.map_err(|e| e.to_string())?;
	serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Ask the service to do something.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub async fn send(command: &Command) -> Reply {
	let call = async {
		let payload = serde_json::to_vec(command).map_err(|e| e.to_string())?;
		let bytes = client()?
			.call(leptoskit_bridge::MethodId(quant_model::COMMAND), &payload)
			.map_err(|e| e.to_string())?
			.await
			.map_err(|e| e.to_string())?;
		serde_json::from_slice::<Reply>(&bytes).map_err(|e| e.to_string())
	};
	call.await.unwrap_or_else(|error| {
		Reply {
			ok: false,
			error: Some(error),
		}
	})
}

#[cfg(all(target_arch = "wasm32", feature = "native"))]
fn client() -> Result<leptoskit_bridge::guest::Client, String> {
	leptoskit_bridge::guest::Client::new(quant_model::SERVICE).map_err(|e| e.to_string())
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
	Reply {
		ok: false,
		error: Some("no native service".to_owned()),
	}
}
