//! The learning settings, stored in the vault under `learn.config`.

use quant_core::Family;
use quant_core::StreamId;
use quant_store::Selector;
use serde::Deserialize;
use serde::Serialize;

use crate::endpoint::ModelEndpoint;

/// How texts are chunked for embedding. Bump this when the truncation
/// policy changes; it is baked into the space hash, so a change re-embeds
/// instead of mixing vectors.
pub const CHUNKING_VERSION: u32 = 1;

/// The longest text an embedding request carries, in characters. Longer
/// texts are truncated to this.
pub const MAX_EMBED_CHARS: usize = 24_000;

/// The streams learning draws on by default: screen text, speech
/// transcripts, and memory records other than the profile.
pub const DEFAULT_STREAMS: [&str; 5] = [
	"screen",
	"audio.transcript",
	"memory.daily",
	"memory.episode",
	"memory.note",
];

/// The embedding model: an OpenAI-compatible server reachable only on this
/// machine's loopback, so vault text can never leave it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedConfig {
	/// A literal loopback URL such as `http://127.0.0.1:11434`.
	pub endpoint: String,
	/// The model name, as the endpoint knows it.
	pub model: String,
	/// The model's embedding dimensions.
	pub dims: usize,
}

/// The extraction model, and the budgets that bound it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractConfig {
	/// A literal loopback URL such as `http://127.0.0.1:8080`.
	pub endpoint: String,
	/// The model name, as the endpoint knows it.
	pub model: String,
	/// Hard cap on model calls per local day.
	#[serde(default = "default_calls_per_day")]
	pub calls_per_day: u32,
	/// Hard cap on claims stored per extraction run.
	#[serde(default = "default_claims_per_run")]
	pub claims_per_run: usize,
}

/// The default number of model calls per local day.
fn default_calls_per_day() -> u32 {
	50
}

/// The default number of claims stored per extraction run.
fn default_claims_per_run() -> usize {
	20
}

/// Everything learning needs. Everything is off until the owner configures
/// it: without `embed` there is no similarity search, without `extract`
/// nothing is learned.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnConfig {
	/// Similarity search embeddings. `None`: keyword search only.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub embed: Option<EmbedConfig>,
	/// Daily claim extraction. `None`: nothing is learned.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub extract: Option<ExtractConfig>,
	/// Which streams are embedded, as families or stream ids. Defaults to
	/// [`DEFAULT_STREAMS`].
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streams: Option<Vec<String>>,
}

impl LearnConfig {
	/// Check every configured endpoint: it must be a literal loopback
	/// address, so a DNS change or a proxy can never redirect vault text
	/// off the machine.
	pub fn validate(&self) -> Result<(), String> {
		if let Some(embed) = &self.embed {
			ModelEndpoint::parse(&embed.endpoint)?;
			if embed.model.trim().is_empty() {
				return Err("the embedding model needs a name".into());
			}
			if embed.dims == 0 {
				return Err("the embedding model needs its dimensions".into());
			}
		}
		if let Some(extract) = &self.extract {
			ModelEndpoint::parse(&extract.endpoint)?;
			if extract.model.trim().is_empty() {
				return Err("the extraction model needs a name".into());
			}
		}
		if let Some(streams) = &self.streams {
			learn_selector(streams)?;
		}
		Ok(())
	}

	/// The embedding space of the configured embedding model, if any.
	#[must_use]
	pub fn space(&self) -> Option<String> {
		self.embed
			.as_ref()
			.map(|embed| quant_store::space_hash(&embed.model, embed.dims, CHUNKING_VERSION))
	}
}

/// The selector of streams to learn from: families and stream ids from the
/// settings, or the defaults.
pub fn learn_selector(streams: &[String]) -> Result<Selector, String> {
	let mut selector = Selector::default();
	for name in streams {
		if let Ok(stream) = name.parse::<StreamId>() {
			selector.streams.push(stream);
		} else if let Ok(family) = name.parse::<Family>() {
			selector.families.push(family);
		} else {
			return Err(format!("unknown stream or family `{name}`"));
		}
	}
	Ok(selector)
}

/// The selector for the default streams.
#[must_use]
pub fn default_selector() -> Selector {
	learn_selector(&DEFAULT_STREAMS.map(str::to_owned)).expect("built-in streams")
}

/// Cut a text down to the model's window, on a character boundary. The
/// chunking version records this policy in the space hash.
pub fn truncate(text: &str) -> &str {
	let end = text
		.char_indices()
		.nth(MAX_EMBED_CHARS)
		.map_or(text.len(), |(index, _)| index);
	&text[..end]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn settings_validate_their_endpoints() {
		let good = LearnConfig {
			embed: Some(EmbedConfig {
				endpoint: "http://127.0.0.1:11434".into(),
				model: "nomic".into(),
				dims: 768,
			}),
			extract: Some(ExtractConfig {
				endpoint: "http://[::1]:8080".into(),
				model: "llama".into(),
				calls_per_day: 50,
				claims_per_run: 20,
			}),
			streams: None,
		};
		assert!(good.validate().is_ok());
		assert_eq!(
			good.space(),
			Some(quant_store::space_hash("nomic", 768, CHUNKING_VERSION))
		);

		let bad = LearnConfig {
			extract: Some(ExtractConfig {
				endpoint: "http://model.hub:8080".into(),
				model: "llama".into(),
				calls_per_day: 50,
				claims_per_run: 20,
			}),
			..LearnConfig::default()
		};
		assert!(bad.validate().is_err());
		let bad_streams = LearnConfig {
			streams: Some(vec!["bogus".into()]),
			..LearnConfig::default()
		};
		assert!(bad_streams.validate().is_err());
	}

	#[test]
	fn defaults_carry_the_advertised_budgets() {
		let config: LearnConfig =
			serde_json::from_str(r#"{"extract":{"endpoint":"http://127.0.0.1:1","model":"m"}}"#)
				.unwrap();
		let extract = config.extract.unwrap();
		assert_eq!(extract.calls_per_day, 50);
		assert_eq!(extract.claims_per_run, 20);
	}

	#[test]
	fn texts_are_truncated_at_the_window() {
		assert_eq!(truncate("short"), "short");
		let long = "x".repeat(MAX_EMBED_CHARS + 10);
		assert_eq!(truncate(&long).chars().count(), MAX_EMBED_CHARS);
	}
}
