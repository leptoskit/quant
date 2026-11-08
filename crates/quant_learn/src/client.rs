//! OpenAI-compatible HTTP calls to the loopback model endpoints.

use std::time::Duration;

use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::Method;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::json;

use crate::endpoint::ModelEndpoint;

/// A model call failed: the endpoint could not be reached, timed out, or
/// answered unusably. The message is for logs, never for agents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelError(pub String);

/// How long an endpoint may take to answer.
const TIMEOUT: Duration = Duration::from_secs(60);

/// POST `body` to `path` on the endpoint and return the JSON answer.
async fn post(endpoint: &ModelEndpoint, path: &str, body: serde_json::Value) -> Result<serde_json::Value, ModelError> {
	let address = endpoint.address();
	let stream = tokio::net::TcpStream::connect(&address)
		.await
		.map_err(|error| ModelError(format!("cannot reach the model endpoint {address}: {error}")))?;
	let (mut sender, connection) =
		hyper::client::conn::http1::handshake(TokioIo::new(stream))
			.await
			.map_err(|error| ModelError(error.to_string()))?;
	tokio::spawn(connection);
	let bytes = hyper::body::Bytes::from(serde_json::to_vec(&body).map_err(|error| {
		ModelError(error.to_string())
	})?);
	let request = Request::builder()
		.method(Method::POST)
		.uri(path)
		.header("host", &address)
		.header("content-type", "application/json")
		.body(Full::new(bytes))
		.map_err(|error| ModelError(error.to_string()))?;
	let sending = sender.send_request(request);
	let response = tokio::time::timeout(TIMEOUT, sending)
		.await
		.map_err(|_| ModelError("the model endpoint timed out".into()))?
		.map_err(|error| ModelError(error.to_string()))?;
	let status = response.status();
	let body = response
		.into_body()
		.collect()
		.await
		.map_err(|error| ModelError(error.to_string()))?
		.to_bytes();
	if !status.is_success() {
		return Err(ModelError(format!("the model endpoint answered {status}")));
	}
	serde_json::from_slice(&body).map_err(|error| ModelError(format!("bad answer from the model endpoint: {error}")))
}

/// Embed `texts` with the endpoint's `/v1/embeddings` call, one vector per
/// text in the same order.
pub async fn embed(
	endpoint: &ModelEndpoint,
	model: &str,
	texts: &[&str],
) -> Result<Vec<Vec<f32>>, ModelError> {
	let answer = post(
		endpoint,
		"/v1/embeddings",
		json!({ "model": model, "input": texts }),
	)
	.await?;
	let data = answer
		.get("data")
		.and_then(serde_json::Value::as_array)
		.ok_or_else(|| ModelError("the embedding answer has no `data`".into()))?;
	let mut vectors = Vec::with_capacity(data.len());
	for item in data {
		#[allow(clippy::cast_possible_truncation)]
		let vector: Vec<f32> = item
			.get("embedding")
			.and_then(serde_json::Value::as_array)
			.ok_or_else(|| ModelError("an embedding answer has no `embedding`".into()))?
			.iter()
			.map(|value| value.as_f64().unwrap_or_default() as f32)
			.collect();
		vectors.push(vector);
	}
	if vectors.len() != texts.len() {
		return Err(ModelError(format!(
			"the embedding endpoint returned {} vectors for {} texts",
			vectors.len(),
			texts.len()
		)));
	}
	Ok(vectors)
}

/// Ask `model` one user message and return the assistant's text.
pub async fn chat(
	endpoint: &ModelEndpoint,
	model: &str,
	prompt: &str,
) -> Result<String, ModelError> {
	let answer = post(
		endpoint,
		"/v1/chat/completions",
		json!({
			"model": model,
			"messages": [{ "role": "user", "content": prompt }],
			"temperature": 0.0,
		}),
	)
	.await?;
	answer
		.pointer("/choices/0/message/content")
		.and_then(serde_json::Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| ModelError("the chat answer has no message content".into()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn embed_future_is_send() {
		fn assert_send<T: Send>(_: T) {}
		let endpoint = ModelEndpoint::parse("http://127.0.0.1:9").unwrap();
		assert_send(embed(&endpoint, "m", &["x"]));
		assert_send(chat(&endpoint, "m", "hello"));
		assert_send(post(&endpoint, "/v1/x", serde_json::json!({})));
	}
}
