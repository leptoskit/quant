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
use quant_api::SessionSource;
use quant_core::Class;
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

/// How long an owner-device challenge stays usable.
pub const CHALLENGE_TTL: SignedDuration = SignedDuration::from_secs(60);

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
	/// This machine's capture sources.
	pub capture_control: crate::capture::Control,
	/// When the daemon started.
	pub started_at: Timestamp,
	/// The TLS certificate fingerprint, once a network listener is up.
	pub tls_fingerprint: RwLock<Option<String>>,
	/// The rendezvous key the daemon proves itself to relays with.
	pub relay_key: quant_relay::RelayKey,
}

/// Most device challenges outstanding at once, so an unauthenticated caller
/// cannot grow the table without end.
const MAX_CHALLENGES: usize = 64;

/// Owner sessions, keyed by token hash, the unlock throttle, and the
/// outstanding device challenges.
#[derive(Debug, Default)]
pub struct OwnerGate {
	sessions: HashMap<[u8; 32], LiveSession>,
	challenges: HashMap<[u8; 32], Timestamp>,
	failures: u32,
	locked_until: Option<Timestamp>,
}

/// One live owner session.
#[derive(Debug)]
struct LiveSession {
	/// When it lapses.
	expires: Timestamp,
	/// How it was opened.
	via: SessionSource,
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

	/// Note a wrong passphrase or device unlock: each failure doubles the
	/// wait, up to an hour. The counter is shared, so alternating between the
	/// two gains nothing.
	pub fn fail(&mut self, now: Timestamp) {
		self.failures += 1;
		let seconds = 2_i64.saturating_pow(self.failures.min(12)).min(3600);
		self.locked_until = Some(now + SignedDuration::from_secs(seconds));
	}

	/// Start a session, returning its token.
	pub fn open(&mut self, via: SessionSource, now: Timestamp) -> (String, Timestamp) {
		self.failures = 0;
		self.locked_until = None;
		let mut token = [0u8; 32];
		getrandom::fill(&mut token).expect("the OS random source is available");
		let token = hex::encode(token);
		let expires = now + OWNER_SESSION;
		self.sessions.retain(|_, session| session.expires > now);
		self.sessions.insert(
			*blake3::hash(token.as_bytes()).as_bytes(),
			LiveSession { expires, via },
		);
		(token, expires)
	}

	/// Whether `token` is a live session, and how it was opened.
	pub fn check(&mut self, token: &str, now: Timestamp) -> Option<SessionSource> {
		self.sessions.retain(|_, session| session.expires > now);
		self.sessions
			.get(blake3::hash(token.as_bytes()).as_bytes())
			.map(|session| session.via)
	}

	/// End a session.
	pub fn close(&mut self, token: &str) {
		self.sessions
			.remove(blake3::hash(token.as_bytes()).as_bytes());
	}

	/// Mint a challenge for an owner device to sign. Outstanding challenges
	/// are bounded; when the table is full the soonest-expiring one is
	/// dropped, and a dropped challenge fails exactly like an unknown one.
	pub fn issue_challenge(&mut self, now: Timestamp) -> ([u8; 32], Timestamp) {
		let mut challenge = [0u8; 32];
		getrandom::fill(&mut challenge).expect("the OS random source is available");
		self.challenges.retain(|_, expires| *expires > now);
		if self.challenges.len() >= MAX_CHALLENGES
			&& let Some(oldest) = self
				.challenges
				.iter()
				.min_by_key(|(_, expires)| **expires)
				.map(|(challenge, _)| *challenge)
		{
			self.challenges.remove(&oldest);
		}
		let expires = now + CHALLENGE_TTL;
		self.challenges.insert(challenge, expires);
		(challenge, expires)
	}

	/// Consume a challenge, returning whether it was outstanding and still
	/// live. Each challenge works once, even when the unlock fails.
	pub fn take_challenge(&mut self, challenge: &[u8; 32], now: Timestamp) -> bool {
		matches!(self.challenges.remove(challenge), Some(expires) if expires > now)
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
		let relay_key = quant_relay::RelayKey::from_vault(vault.key());
		let state = Self {
			vault,
			specs: RwLock::new(specs),
			limiter: Mutex::new(Limiter::new()),
			verifier: Mutex::new(RequestVerifier::new()),
			owner: Mutex::new(OwnerGate::default()),
			capture: Mutex::new(BTreeMap::new()),
			capture_control: crate::capture::Control::default(),
			started_at: Timestamp::now(),
			tls_fingerprint: RwLock::new(None),
			relay_key,
		};
		// The rolling buffer's retention follows the capture settings, not
		// the built-in default.
		if let Ok(config) = crate::capture::config(&state) {
			state.set_media_retention(
				&StreamId::new("audio.buffer").expect("built-in stream"),
				Some(config.rewind.hours),
			);
		}
		Ok(state)
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

	/// The known stream specs, by id.
	pub fn specs_lock(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<StreamId, StreamSpec>> {
		self.specs
			.read()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// The sensitivity classes of `stream`, from its own spec or the nearest
	/// registered parent.
	pub fn classes(&self, stream: &StreamId) -> Vec<Class> {
		self.spec(stream).map_or_else(Vec::new, |spec| spec.classes)
	}

	/// Whether a grant denying `deny` may see `stream` at all.
	pub fn class_denied(&self, stream: &StreamId, deny: &[Class]) -> bool {
		self.classes(stream)
			.iter()
			.any(|class| deny.contains(class))
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

	/// Point a built-in stream's media retention at a new setting. The
	/// daemon uses this so `audio.buffer` media is kept exactly as long as
	/// the rolling buffer is configured to hold it.
	pub fn set_media_retention(&self, stream: &StreamId, hours: Option<u32>) {
		let mut specs = self
			.specs
			.write()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		if let Some(spec) = specs.get_mut(stream) {
			spec.media_retention_hours = hours;
		}
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
		let (token, _) = gate.open(SessionSource::Passphrase, now);
		assert_eq!(gate.check(&token, now), Some(SessionSource::Passphrase));
		assert_eq!(gate.check("forged", now), None);
		assert_eq!(
			gate.check(&token, now + OWNER_SESSION + SignedDuration::from_secs(1)),
			None
		);
		let (token, _) = gate.open(SessionSource::Device, now);
		assert_eq!(gate.check(&token, now), Some(SessionSource::Device));
		gate.close(&token);
		assert_eq!(gate.check(&token, now), None);
	}

	#[test]
	fn a_successful_unlock_clears_the_backoff() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		gate.fail(now);
		gate.open(SessionSource::Device, now);
		assert_eq!(gate.wait(now), None);
	}

	#[test]
	fn challenges_work_once_and_expire() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		let (challenge, expires) = gate.issue_challenge(now);
		assert_eq!(expires, now + CHALLENGE_TTL);
		assert!(gate.take_challenge(&challenge, now));
		// A second attempt, successful or not, finds nothing left.
		assert!(!gate.take_challenge(&challenge, now));
		assert!(!gate.take_challenge(&challenge, now + CHALLENGE_TTL));
	}

	#[test]
	fn an_expired_challenge_is_refused_and_consumed() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		let before = now - CHALLENGE_TTL - SignedDuration::from_secs(1);
		let (challenge, _) = gate.issue_challenge(before);
		assert!(!gate.take_challenge(&challenge, now));
		// And it is gone, not merely expired.
		assert!(!gate.take_challenge(&challenge, now));
	}

	#[test]
	fn outstanding_challenges_are_bounded() {
		let mut gate = OwnerGate::default();
		let now = Timestamp::now();
		let mut latest = None;
		for _ in 0..(MAX_CHALLENGES + 10) {
			latest = Some(gate.issue_challenge(now).0);
		}
		assert_eq!(gate.challenges.len(), MAX_CHALLENGES);
		// The table stays bounded and keeps serving.
		assert!(gate.take_challenge(&latest.unwrap(), now));

		// Expired challenges are pruned rather than displacing live ones.
		let mut gate = OwnerGate::default();
		let stale = now - CHALLENGE_TTL - SignedDuration::from_secs(1);
		for _ in 0..MAX_CHALLENGES {
			gate.issue_challenge(stale);
		}
		assert_eq!(gate.challenges.len(), MAX_CHALLENGES);
		gate.issue_challenge(now);
		assert_eq!(gate.challenges.len(), 1);
	}
}
