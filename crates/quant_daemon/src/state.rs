use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::RwLock;

use jiff::SignedDuration;
use jiff::Timestamp;
use quant_access::Limiter;
use quant_access::auth::RequestVerifier;
use quant_api::CaptureStatus;
use quant_core::MediaRef;
use quant_core::Record;
use quant_core::SeriesBatch;
use quant_core::Sink;
use quant_core::SinkError;
use quant_core::StreamId;
use quant_core::StreamSpec;
use quant_core::builtin_streams;
use quant_store::Vault;

/// How long an owner session lasts.
pub const OWNER_SESSION: SignedDuration = SignedDuration::from_mins(15);

/// Everything the daemon shares between requests and capture.
pub struct State {
	/// The open vault.
	pub vault: Vault,
	/// Known stream specs.
	pub specs: RwLock<BTreeMap<StreamId, StreamSpec>>,
	/// Per-grant rate limits.
	pub limiter: Mutex<Limiter>,
	/// Signature and replay checks.
	pub verifier: Mutex<RequestVerifier>,
	/// Owner sessions and unlock throttling.
	pub owner: Mutex<OwnerGate>,
	/// Capture sources by name.
	pub capture: Mutex<BTreeMap<String, CaptureStatus>>,
	/// When the daemon started.
	pub started_at: Timestamp,
	/// The TLS certificate fingerprint, once a network listener is up.
	pub tls_fingerprint: RwLock<Option<String>>,
}

/// Owner sessions, keyed by token hash, and the unlock throttle.
#[derive(Debug, Default)]
pub struct OwnerGate {
	sessions: HashMap<[u8; 32], Timestamp>,
	failures: u32,
	locked_until: Option<Timestamp>,
}

impl OwnerGate {
	/// Seconds to wait before another unlock attempt is allowed.
	pub fn wait(&self, now: Timestamp) -> Option<u64> {
		self.locked_until.filter(|until| *until > now).map(|until| {
			u64::try_from(until.duration_since(now).as_secs())
				.unwrap_or(0)
				.max(1)
		})
	}

	/// Note a wrong passphrase: each failure doubles the wait, up to an hour.
	pub fn fail(&mut self, now: Timestamp) {
		self.failures += 1;
		let seconds = 2_i64.saturating_pow(self.failures.min(12)).min(3600);
		self.locked_until = Some(now + SignedDuration::from_secs(seconds));
	}

	/// Start a session, returning its token.
	pub fn open(&mut self, now: Timestamp) -> (String, Timestamp) {
		self.failures = 0;
		self.locked_until = None;
		let mut token = [0u8; 32];
		getrandom::fill(&mut token).expect("the OS random source is available");
		let token = hex::encode(token);
		let expires = now + OWNER_SESSION;
		self.sessions.retain(|_, expiry| *expiry > now);
		self.sessions
			.insert(*blake3::hash(token.as_bytes()).as_bytes(), expires);
		(token, expires)
	}

	/// Whether `token` is a live session.
	pub fn check(&mut self, token: &str, now: Timestamp) -> bool {
		self.sessions.retain(|_, expiry| *expiry > now);
		self.sessions
			.contains_key(blake3::hash(token.as_bytes()).as_bytes())
	}

	/// End a session.
	pub fn close(&mut self, token: &str) {
		self.sessions
			.remove(blake3::hash(token.as_bytes()).as_bytes());
	}
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl State {
	/// State around an open vault.
	pub fn new(vault: Vault) -> Result<Self, quant_store::StoreError> {
		let mut specs: BTreeMap<StreamId, StreamSpec> = builtin_streams()
			.into_iter()
			.map(|spec| (spec.id.clone(), spec))
			.collect();
		for spec in vault.store.custom_streams()? {
			specs.insert(spec.id.clone(), spec);
		}
		Ok(Self {
			vault,
			specs: RwLock::new(specs),
			limiter: Mutex::new(Limiter::new()),
			verifier: Mutex::new(RequestVerifier::new()),
			owner: Mutex::new(OwnerGate::default()),
			capture: Mutex::new(BTreeMap::new()),
			started_at: Timestamp::now(),
			tls_fingerprint: RwLock::new(None),
		})
	}

	/// The limiter.
	pub fn limiter(&self) -> MutexGuard<'_, Limiter> {
		lock(&self.limiter)
	}

	/// The request verifier.
	pub fn verifier(&self) -> MutexGuard<'_, RequestVerifier> {
		lock(&self.verifier)
	}

	/// The owner gate.
	pub fn owner(&self) -> MutexGuard<'_, OwnerGate> {
		lock(&self.owner)
	}

	/// The spec for `stream`, or for the nearest registered parent.
	pub fn spec(&self, stream: &StreamId) -> Option<StreamSpec> {
		let specs = self
			.specs
			.read()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		let mut name = stream.as_str();
		loop {
			if let Some(spec) = StreamId::new(name).ok().and_then(|id| specs.get(&id)) {
				return Some(spec.clone());
			}
			name = name.rsplit_once('.')?.0;
		}
	}

	/// Every known spec.
	pub fn specs(&self) -> Vec<StreamSpec> {
		self.specs
			.read()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.values()
			.cloned()
			.collect()
	}

	/// Register a custom stream.
	pub fn register_stream(&self, spec: StreamSpec) -> Result<(), quant_store::StoreError> {
		self.vault.store.register_stream(&spec)?;
		self.specs
			.write()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.insert(spec.id.clone(), spec);
		Ok(())
	}

	fn count(&self, stream: &StreamId, n: u64) {
		let mut capture = lock(&self.capture);
		let status = capture
			.entry(stream.family().as_str().to_owned())
			.or_insert_with(|| {
				CaptureStatus {
					source: stream.family().as_str().to_owned(),
					running: true,
					detail: None,
					captured: 0,
					last: None,
				}
			});
		status.captured += n;
		status.last = Some(Timestamp::now());
	}

	/// Capture status by source.
	pub fn capture_status(&self) -> Vec<CaptureStatus> {
		lock(&self.capture).values().cloned().collect()
	}
}

impl Sink for State {
	fn record(&self, record: Record) -> Result<(), SinkError> {
		let stream = record.stream.clone();
		let report = self
			.vault
			.store
			.insert_records(&[record])
			.map_err(SinkError::from_display)?;
		self.count(&stream, report.stored as u64);
		Ok(())
	}

	fn series(&self, batch: SeriesBatch) -> Result<(), SinkError> {
		let rows = batch.len() as u64;
		if self
			.vault
			.store
			.insert_series(&batch)
			.map_err(SinkError::from_display)?
		{
			self.count(&batch.stream, rows);
		}
		Ok(())
	}

	fn media(&self, stream: &StreamId, bytes: &[u8], mime: &str) -> Result<MediaRef, SinkError> {
		let retention = self
			.spec(stream)
			.and_then(|spec| spec.media_retention_hours);
		self.vault
			.put_media(bytes, mime, retention)
			.map_err(SinkError::from_display)
	}

	fn status(&self, source: &str, running: bool, detail: Option<String>) {
		let mut capture = lock(&self.capture);
		let status = capture.entry(source.to_owned()).or_insert_with(|| {
			CaptureStatus {
				source: source.to_owned(),
				running,
				detail: None,
				captured: 0,
				last: None,
			}
		});
		status.running = running;
		status.detail = detail;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unlock_failures_back_off_exponentially() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		assert_eq!(gate.wait(now), None);
		gate.fail(now);
		assert_eq!(gate.wait(now), Some(2));
		gate.fail(now);
		gate.fail(now);
		assert_eq!(gate.wait(now), Some(8));
		assert_eq!(gate.wait(now + SignedDuration::from_secs(9)), None);
		for _ in 0..20 {
			gate.fail(now);
		}
		assert_eq!(gate.wait(now), Some(3600));
	}

	#[test]
	fn sessions_expire_and_close() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		let (token, _) = gate.open(now);
		assert!(gate.check(&token, now));
		assert!(!gate.check("forged", now));
		assert!(!gate.check(&token, now + OWNER_SESSION + SignedDuration::from_secs(1)));
		let (token, _) = gate.open(now);
		gate.close(&token);
		assert!(!gate.check(&token, now));
	}
}
