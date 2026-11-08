//! On-device learning for Quant.
//!
//! Quant learns from a vault only through endpoints on this machine's own
//! loopback: embeddings for similarity search, and a local chat model that
//! proposes claims about the owner. Everything is off until the owner
//! configures it, and everything the model proposes is checked before it is
//! stored — every quote must really be in the record it cites, and anything
//! shaped like instructions to an AI is dropped — so captured text cannot
//! plant claims in the owner's memory.

pub mod backfill;
pub mod client;
pub mod config;
pub mod endpoint;
pub mod extract;
pub mod ground;
pub mod guard;

pub use config::LearnConfig;
