//! The phone's screen.
//!
//! A phone has one job: record where it is, which apps were in front and how
//! its battery is doing, and deliver that to the owner's vault. The screen
//! pairs the phone, asks for what the system makes the owner grant in
//! person, and shows what is waiting. Everything else lives in the app's
//! native service (`quant_phone`) and in the plugins the collector drives.
//!
//! As on the desktop, each card is a region keyed on the part of the state
//! it shows, so a new snapshot rebuilds only what changed and the pairing
//! link being typed keeps its text.

pub mod bridge;
#[cfg(all(target_arch = "wasm32", feature = "phone"))]
pub mod collect;
pub mod splash;

use std::sync::Arc;

use leptoskit::native::animation::Presence;
use leptoskit::native::animation::Spring;
use leptoskit::native::reactive::For;
use leptoskit::native::reactive::Show;
use leptoskit::prelude::ActivityIndicator;
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::RwSignal;
use leptoskit::prelude::ScrollView;
use leptoskit::prelude::Set;
use leptoskit::prelude::Switch;
use leptoskit::prelude::Text;
use leptoskit::prelude::TextInput;
use leptoskit::prelude::View;
use leptoskit::prelude::component;
use quant_model::phone::Command;
use quant_model::phone::Link;
use quant_model::phone::Snapshot;
use quant_model::phone::Source;
use quant_model::phone::SourceRow;
use quant_model::phone::Storage;

use crate::theme;
use crate::theme::Icon;

/// Where one thing the owner has to allow stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Grant {
	/// Not asked the system yet.
	#[default]
	Unknown,
	/// The owner has not allowed it.
	Needed,
	/// Allowed.
	Granted,
	/// This phone has nothing to allow here.
	Unavailable,
}

/// A `quant://link` the operating system delivered, held for the owner to
/// review.
///
/// Receiving one changes nothing: it is shown with a Connect action, and only
/// that action sends [`Command::Pair`]. The link itself stays out of the
/// screen's text; the card names the vault it would reach. The service checks
/// the fingerprint and address again when it connects, so this parse only
/// decides what is worth showing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Invite {
	link: String,
	place: String,
	relay: bool,
}

impl Invite {
	/// The longest inbound link accepted from a platform entry point.
	pub const MAX_BYTES: usize = 4096;

	/// Read the address a pairing link names, without exposing the whole URL.
	///
	/// Only a `quant://link?…` with a full SHA-256 fingerprint is accepted.
	/// A direct address wins when a link carries both a direct address and a
	/// relay, because the phone tries that first.
	pub fn parse(text: &str) -> Option<Self> {
		let text = text.trim();

		if text.is_empty() || text.len() > Self::MAX_BYTES || text.chars().any(char::is_control) {
			return None;
		}

		let query = text.strip_prefix("quant://link")?.strip_prefix('?')?;
		let mut addr = None;
		let mut fingerprint = None;
		let mut relay = None;
		let mut rid = None;

		for pair in query.split('&').filter(|pair| !pair.is_empty()) {
			let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
			let value = percent_decode(value);

			match name {
				"addr" => addr = Some(value),
				"fp" => fingerprint = Some(value),
				"relay" => relay = Some(value),
				"rid" => rid = Some(value),
				_ => {}
			}
		}

		let fingerprint = fingerprint?.replace(':', "");
		if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			return None;
		}

		let addr = addr.filter(|value| display_host(value).is_some());
		let relay = if rid.as_deref().is_some_and(valid_rid) {
			relay.and_then(|value| relay_authority(&value))
		} else {
			None
		};

		let (place, relay) = match (addr, relay) {
			(Some(addr), _) => (addr, false),
			(None, Some(relay)) => (relay, true),
			(None, None) => return None,
		};

		Some(Self {
			link: text.to_owned(),
			place,
			relay,
		})
	}

	/// The exact link a [`Command::Pair`] should receive.
	pub fn link(&self) -> &str {
		&self.link
	}

	/// The address or relay the card names.
	pub fn place(&self) -> &str {
		&self.place
	}

	/// Whether [`Invite::place`] is a relay rather than a direct address.
	pub fn through_relay(&self) -> bool {
		self.relay
	}
}

/// A display-safe `host:port`, kept ASCII so the card cannot be spoofed with
/// control or bidirectional text.
fn display_host(value: &str) -> Option<&str> {
	let value = display_authority(value)?;
	value.contains(':').then_some(value)
}

/// A display-safe host or `host:port`.
fn display_authority(value: &str) -> Option<&str> {
	let value = value.trim();
	if value.is_empty()
		|| !value.is_ascii()
		|| value.len() > 255
		|| value.chars().any(|c| c.is_whitespace() || c.is_control())
	{
		return None;
	}
	Some(value)
}

/// The authority of a `wss://` or `https://` relay URL.
fn relay_authority(value: &str) -> Option<String> {
	let value = value.trim();
	let (scheme, rest) = value.split_once("://")?;
	if !matches!(scheme, "wss" | "https") {
		return None;
	}
	let authority = rest.split('/').next()?;
	let authority = display_authority(authority)?;
	Some(authority.to_owned())
}

/// The rendezvous ids the relay issues: 26 lowercase base32 characters.
fn valid_rid(value: &str) -> bool {
	value.len() == 26
		&& value
			.bytes()
			.all(|byte| matches!(byte, b'a'..=b'z' | b'2'..=b'7'))
}

/// Decode the few escapes `quant link` uses, passing bad escapes through.
fn percent_decode(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;

	while index < bytes.len() {
		if bytes[index] == b'%' && index + 2 < bytes.len() {
			let high = (bytes[index + 1] as char).to_digit(16);
			let low = (bytes[index + 2] as char).to_digit(16);
			if let (Some(high), Some(low)) = (high, low) {
				out.push(u8::try_from(high * 16 + low).expect("two hex digits are a byte"));
				index += 3;
				continue;
			}
		}
		out.push(bytes[index]);
		index += 1;
	}

	String::from_utf8_lossy(&out).into_owned()
}

/// What the system makes the owner allow in person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
	/// Location while the app is in use. Background collection cannot start
	/// without it.
	Location,
	/// Location all the time.
	Always,
	/// The notification Android shows while the app works in the background.
	/// The iPhone app has none.
	Notifications,
	/// Usage Access, which Android grants only on its own settings screen.
	Usage,
	/// An exemption from battery optimisation, which only Android has.
	Battery,
	/// Health store (`HealthKit`, Health Connect, `HarmonyOS` Health).
	Health,
	/// Direct on-watch body sensors (heart rate, `SpO2`, temperature, steps).
	Wearable,
	/// Bluetooth scanning for chest straps, rings, and BLE sensors.
	Bluetooth,
}

impl Step {
	/// Every step, in the order the screen lists them.
	pub const ALL: [Step; 8] = [
		Step::Location,
		Step::Always,
		Step::Notifications,
		Step::Usage,
		Step::Battery,
		Step::Health,
		Step::Wearable,
		Step::Bluetooth,
	];

	fn title(self) -> &'static str {
		match self {
			Step::Location => "Location",
			Step::Always => "Location all the time",
			Step::Notifications => "Notification",
			Step::Usage => "App usage",
			Step::Battery => "Battery",
			Step::Health => "Health store",
			Step::Wearable => "Watch sensors",
			Step::Bluetooth => "Bluetooth sensors",
		}
	}

	fn why(self) -> &'static str {
		match self {
			Step::Location => {
				"Records where this phone goes. Nothing is collected in the background until you \
				 allow it."
			}
			Step::Always if cfg!(feature = "ios") => {
				"Keeps location coming while Quant is in the background. Choose “Change to Always \
				 Allow”. If iOS doesn't ask, set Location to Always in Quant's settings, then tap \
				 this again."
			}
			Step::Always => {
				"Keeps location coming when Quant is restarted in the background. Choose “Allow \
				 all the time”."
			}
			Step::Notifications => {
				"The system shows a notification for as long as Quant collects in the background."
			}
			Step::Usage => {
				"Records which apps were in front. The system grants this on its own screen: find \
				 Quant there and switch it on."
			}
			Step::Battery => "Stops the system pausing Quant to save battery.",
			Step::Health => {
				"Reads heart rate, HRV, SpO2, sleep, workouts, steps, and wrist temperature from \
				 HealthKit or Health Connect into your vault."
			}
			Step::Wearable => {
				"Reads direct wrist biometrics (heart rate, HRV, SpO2, skin temperature, steps, \
				 and wear state) on your watch."
			}
			Step::Bluetooth => {
				"Connects to Bluetooth chest straps, smart rings, oximeters, and fitness sensors."
			}
		}
	}

	fn ask(self) -> &'static str {
		match self {
			Step::Location => "Allow location",
			Step::Always => "Allow all the time",
			Step::Notifications => "Allow notifications",
			Step::Usage => "Open Usage Access",
			Step::Battery => "Keep Quant running",
			Step::Health => "Allow health data",
			Step::Wearable => "Allow watch sensors",
			Step::Bluetooth => "Allow Bluetooth",
		}
	}

	fn id(self) -> &'static str {
		match self {
			Step::Location => "location",
			Step::Always => "always",
			Step::Notifications => "notifications",
			Step::Usage => "usage",
			Step::Battery => "battery",
			Step::Health => "health",
			Step::Wearable => "wearable",
			Step::Bluetooth => "bluetooth",
		}
	}

	/// The line icon that names this step in its row.
	fn icon(self) -> Icon {
		match self {
			Step::Location | Step::Always => Icon::Pin,
			Step::Notifications => Icon::Bell,
			Step::Usage => Icon::Apps,
			Step::Battery => Icon::Battery,
			Step::Health => Icon::Heart,
			Step::Wearable => Icon::Watch,
			Step::Bluetooth => Icon::Bluetooth,
		}
	}
}

/// Where every [`Step`] stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Permissions {
	/// Location while in use.
	pub location: Grant,
	/// Location all the time.
	pub always: Grant,
	/// The background notification.
	pub notifications: Grant,
	/// Usage Access.
	pub usage: Grant,
	/// The battery optimisation exemption.
	pub battery: Grant,
	/// Health store access.
	pub health: Grant,
	/// Direct watch body sensors.
	pub wearable: Grant,
	/// Bluetooth scanning/subscriptions.
	pub bluetooth: Grant,
}

impl Permissions {
	/// Where `step` stands.
	pub fn get(&self, step: Step) -> Grant {
		match step {
			Step::Location => self.location,
			Step::Always => self.always,
			Step::Notifications => self.notifications,
			Step::Usage => self.usage,
			Step::Battery => self.battery,
			Step::Health => self.health,
			Step::Wearable => self.wearable,
			Step::Bluetooth => self.bluetooth,
		}
	}

	/// Record where `step` stands.
	pub fn set(&mut self, step: Step, grant: Grant) {
		match step {
			Step::Location => self.location = grant,
			Step::Always => self.always = grant,
			Step::Notifications => self.notifications = grant,
			Step::Usage => self.usage = grant,
			Step::Battery => self.battery = grant,
			Step::Health => self.health = grant,
			Step::Wearable => self.wearable = grant,
			Step::Bluetooth => self.bluetooth = grant,
		}
	}
}

/// Something the owner asked for on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
	/// Work for the native service.
	Command(Command),
	/// Ask the system for one permission.
	Allow(Step),
	/// Give up on storage that can't be opened and start an empty one.
	ResetStorage,
	/// Push this phone's saved vault pairing link to a paired watch over
	/// `watch-sync`.
	PushPairLinkToWatch,
	/// Scan for nearby BLE GATT heart-rate, pulse-oximeter, thermometer, and
	/// fitness sensors and subscribe automatically.
	ScanBleSensors,
	/// Start or stop a continuous live wrist-sensor / workout session on a
	/// watch target.
	ToggleLiveWorkout,
}

/// Carries out an [`Action`].
pub type Act = Arc<dyn Fn(Action) + std::marker::Send + Sync>;

/// The screen's state.
#[derive(Clone)]
pub struct State {
	/// The service's latest snapshot.
	pub snapshot: RwSignal<Snapshot>,
	/// Where each permission stands.
	pub permissions: RwSignal<Permissions>,
	/// The pairing link being typed or pasted.
	pub link: RwSignal<String>,
	/// A link the operating system delivered, waiting for review.
	pub invite: RwSignal<Option<Invite>>,
	/// Why the last action failed.
	pub problem: RwSignal<Option<String>>,
	/// What stops the collector on this phone, when something does.
	pub collector: RwSignal<Option<String>>,
	/// Whether a high-frequency live workout sensor session is active on a
	/// watch.
	pub live_workout: RwSignal<bool>,
	/// Whether the launch animation still covers the screen.
	pub splash: RwSignal<bool>,
	/// Whether the owner has left the welcome screen to pair this phone.
	pub pairing: RwSignal<bool>,
	act: Act,
}

impl State {
	/// State that carries out actions through `act`.
	pub fn new(act: Act) -> Self {
		Self {
			snapshot: RwSignal::new(Snapshot::default()),
			permissions: RwSignal::new(Permissions::default()),
			link: RwSignal::new(String::new()),
			invite: RwSignal::new(None),
			problem: RwSignal::new(None),
			collector: RwSignal::new(None),
			live_workout: RwSignal::new(false),
			splash: RwSignal::new(false),
			pairing: RwSignal::new(false),
			act,
		}
	}

	pub(crate) fn act(&self, action: Action) {
		self.problem.set(None);
		(self.act)(action);
	}

	fn send(&self, command: Command) {
		self.act(Action::Command(command));
	}
}

/// The whole screen. One stage is built at a time, keyed so a pairing step
/// keeps its typed link and a snapshot does not rebuild the stage beneath it.
#[component]
pub fn Screen(state: State) -> Element {
	let splash = state.splash;
	View()
		.class(theme::SCREEN)
		.child(
			ScrollView().class("flex-1").child(
				View()
					.class(theme::CONTENT)
					.child(storage_card(&state))
					.child(invite_card(&state))
					.child(stage_view(&state)),
			),
		)
		// The splash must be a direct child: `Show` would wrap it in a
		// zero-height region, and its `absolute inset-0` would then cover
		// that wrapper instead of the screen.
		.children(splash.get().then(splash::splash))
}

/// Which stage the screen shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Stage {
	/// The welcome screen, before the owner asks to pair.
	Welcome,
	/// Pairing: the link and the approval code.
	Pair,
	/// Paired, or paired and waiting for the vault to answer.
	Home,
}

/// The stage a link and the owner's choice lead to.
fn stage(link: &Link, pairing: bool) -> Stage {
	match link {
		Link::Unpaired if !pairing => Stage::Welcome,
		Link::Unpaired | Link::Connecting | Link::Pending { .. } => Stage::Pair,
		Link::Unreachable { .. } | Link::Revoked | Link::Ready => Stage::Home,
	}
}

fn stage_view(state: &State) -> Element {
	let snapshot = state.snapshot;
	let pairing = state.pairing;
	let state = state.clone();
	For(
		move || vec![stage(&snapshot.get().link, pairing.get())],
		Clone::clone,
		move |stage| {
			match stage {
				Stage::Welcome => welcome_view(&state),
				Stage::Pair => pair_stage(&state),
				Stage::Home => home_view(&state),
			}
		},
	)
}

/// The card for one link the operating system delivered.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InvitePart {
	invite: Invite,
	replace: bool,
}

/// Shows a link the app received, above whichever stage is open.
///
/// Receiving a link only fills this card. The vault is replaced or paired
/// when, and only when, the owner taps Connect; Not now drops the link
/// without sending anything.
fn invite_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let invite = state.invite;
	let state = state.clone();
	For(
		move || {
			let replace = !matches!(snapshot.get().link, Link::Unpaired);
			invite
				.get()
				.map(|invite| InvitePart { invite, replace })
				.into_iter()
				.collect()
		},
		Clone::clone,
		move |part| {
			let connect = state.clone();
			let dismiss = state.clone();
			let link = part.invite.link().to_owned();
			let route = if part.invite.through_relay() {
				format!("through the relay at {}", part.invite.place())
			} else {
				format!("at {}", part.invite.place())
			};
			View()
				.class(theme::CARD)
				.child(Text("LINK RECEIVED").class(theme::EYEBROW))
				.child(
					Text(if part.replace {
						"Connect to another vault?"
					} else {
						"Connect to this vault?"
					})
					.class(theme::HEADING)
					.test_id("invite"),
				)
				.child(
					Text(format!(
						"A link opened Quant for the vault {route}. Quant checks the certificate \
						 it pinned, then asks your computer to approve this phone. Nothing is \
						 sent until you tap Connect, so only connect if you just ran `quant link` \
						 yourself."
					))
					.class(theme::MUTED),
				)
				.children(part.replace.then(|| {
					Text(
						"This phone is already connected. Connecting switches it to this vault; \
						 readings waiting on this phone stay.",
					)
					.class(theme::MUTED)
				}))
				.child(
					View()
						.class("gap-2")
						.child(
							Button("Connect")
								.class(theme::PRIMARY)
								.accessibility_label("Connect to the vault in the link")
								.test_id("invite-connect")
								.on_press(move || {
									connect.invite.set(None);
									connect.pairing.set(true);
									connect.send(Command::Pair { link: link.clone() });
								}),
						)
						.child(
							Button("Not now")
								.class(theme::QUIET)
								.test_id("invite-dismiss")
								.on_press(move || dismiss.invite.set(None)),
						),
				)
		},
	)
}

/// The first thing a new owner sees: what Quant is, and the three promises
/// that make it worth handing over a phone.
fn welcome_view(state: &State) -> Element {
	let start = state.clone();
	View()
		.class("gap-8 pt-6")
		.child(
			theme::logo("w-[88px] h-[88px]")
				.entering(Presence::fade().scale(0.9).spring(Spring::GENTLE)),
		)
		.child(
			View()
				.class("gap-3")
				.child(Text("YOUR PRIVATE MEMORY").class(theme::EYEBROW))
				.child(
					Text("Measure your life. Keep it yours.")
						.class(theme::DISPLAY)
						.test_id("welcome"),
				)
				.child(
					Text(
						"Quant turns this phone into a quiet sensor for the encrypted vault on \
						 your computer.",
					)
					.class(theme::BODY),
				),
		)
		.child(
			View()
				.class("gap-3")
				.child(promise(
					0,
					Icon::Pulse,
					"Collects quietly",
					"Where you go, the apps you use, your battery and health readings, gathered \
					 in the background.",
				))
				.child(promise(
					1,
					Icon::Lock,
					"Sealed on your computer",
					"Readings travel encrypted to your vault. This phone can add to it, never \
					 read it back.",
				))
				.child(promise(
					2,
					Icon::Key,
					"You decide who reads",
					"AI agents see only what you grant, for as long as you allow, and every read \
					 is logged.",
				)),
		)
		.child(
			View()
				.class("gap-3")
				.child(
					Button("Connect to my vault")
						.class(theme::PRIMARY)
						.test_id("get-started")
						.on_press(move || {
							start.problem.set(None);
							start.pairing.set(true);
						}),
				)
				.child(
					Text("You'll need the computer where Quant keeps your vault.")
						.class(theme::CAPTION),
				),
		)
}

/// One of the welcome screen's three promises, rising into place in turn.
fn promise(n: u8, icon: Icon, title: &'static str, text: &'static str) -> Element {
	View()
		.class("flex-row items-start gap-4 p-4 rounded-2xl bg-[#17143A]")
		.entering(
			Presence::fade()
				.slide_by(0.0, 18.0)
				.spring(Spring::GENTLE)
				.delay(150.0 + 120.0 * f32::from(n)),
		)
		.child(theme::tile(icon))
		.child(
			View()
				.class("flex-1 gap-1")
				.child(Text(title).class(theme::LABEL))
				.child(Text(text).class(theme::MUTED)),
		)
}

/// Pairing: the link, then the code the owner approves.
fn pair_stage(state: &State) -> Element {
	let snapshot = state.snapshot;
	let problem = state.problem;
	let message = move || {
		let snapshot = snapshot.get();
		problem
			.get()
			.or_else(|| snapshot.notice.map(|notice| notice.text))
			.filter(|text| !text.is_empty() && *text != snapshot.headline)
	};
	let back = state.clone();
	View()
		.class("gap-6 pt-2")
		.child(Show(
			move || snapshot.get().link == Link::Unpaired,
			move || {
				let back = back.clone();
				Button("Back")
					.class(theme::QUIET)
					.test_id("back")
					.on_press(move || {
						back.problem.set(None);
						back.pairing.set(false);
					})
			},
		))
		.child(vault_card(state))
		.child(Show(
			move || message().is_some(),
			move || {
				Text(move || message().unwrap_or_default())
					.class(theme::PROBLEM)
					.test_id("notice")
			},
		))
}

/// Paired: the mark, the state of the vault, and everything this phone
/// records, in one stack.
fn home_view(state: &State) -> Element {
	View()
		.class("gap-5")
		.child(top_bar(state))
		.child(hero(state))
		.child(vault_card(state))
		.child(permissions_card(state))
		.child(delivery_card(state))
		// The watch card sits above the source list rather than last: with
		// every permission still unasked, the cards above it already fill a
		// 3200-point test viewport, and the unchanged wearables test must be
		// able to reach its buttons.
		.child(wearables_card(state))
		.child(collecting_card(state))
}

/// The mark and wordmark on the left, whether the phone is recording on the
/// right.
fn top_bar(state: &State) -> Element {
	let snapshot = state.snapshot;
	View()
		.class("flex-row items-center justify-between")
		.child(
			View()
				.class("flex-row items-center gap-2")
				.child(theme::logo("w-8 h-8"))
				.child(theme::wordmark("w-[68px] h-[24px]")),
		)
		.child(For(
			move || vec![snapshot.get().collecting],
			Clone::clone,
			status_pill,
		))
}

/// The recording or paused pill.
fn status_pill(collecting: bool) -> Element {
	let (pill, dot, label, ink) = if collecting {
		(
			"flex-row items-center gap-2 px-3 py-1.5 rounded-full bg-[#2B1C3A]",
			"w-2 h-2 rounded-full bg-[#FF7A59] animate-pulse",
			"RECORDING",
			"text-xs font-semibold text-[#FF9C82]",
		)
	} else {
		(
			"flex-row items-center gap-2 px-3 py-1.5 rounded-full bg-[#221E4F]",
			"w-2 h-2 rounded-full bg-[#6F68A8]",
			"PAUSED",
			"text-xs font-semibold text-[#A9A3D6]",
		)
	};
	View()
		.class(pill)
		.child(View().class(dot))
		.child(Text(label).class(ink))
}

/// What the phone is doing now, and the switch that starts or stops it.
fn hero(state: &State) -> Element {
	let snapshot = state.snapshot;
	let problem = state.problem;
	let message = move || {
		let snapshot = snapshot.get();
		problem
			.get()
			.or_else(|| snapshot.notice.map(|notice| notice.text))
			.filter(|text| !text.is_empty() && *text != snapshot.headline)
	};
	let toggle = state.clone();
	View()
		.class(theme::HERO)
		.child(
			Text(move || snapshot.get().headline)
				.class(theme::TITLE)
				.test_id("headline"),
		)
		.child(Show(
			move || message().is_some(),
			move || {
				Text(move || message().unwrap_or_default())
					.class(theme::MUTED)
					.test_id("notice")
			},
		))
		.child(For(
			move || vec![snapshot.get().collecting],
			Clone::clone,
			move |collecting| {
				let toggle = toggle.clone();
				View()
					.class(
						"flex-row items-center justify-between gap-3 pt-4 border-t \
						 border-[#3A3380]",
					)
					.child(
						View()
							.class("flex-1 gap-1")
							.child(Text("Collect on this phone").class(theme::LABEL))
							.child(
								Text("Readings wait here, encrypted, until your vault takes them.")
									.class(theme::CAPTION),
							),
					)
					.child(
						Switch(collecting)
							.accessibility_label("Collect on this phone")
							.test_id("collecting")
							.on_toggle(move |enabled| {
								toggle.send(Command::Collect { enabled });
							}),
					)
			},
		))
}

/// Shown only when the encrypted spool can't be opened.
fn storage_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	For(
		move || vec![snapshot.get().storage],
		Clone::clone,
		move |storage| {
			let Storage::Failed { reason } = storage else {
				return View();
			};
			let state = state.clone();
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(theme::tile_bad(Icon::Lock))
						.child(Text("This phone's storage can't be opened").class(theme::HEADING)),
				)
				.child(
					Text(format!(
						"Readings wait in an encrypted store whose key lives in this phone's \
						 keystore, and the two no longer match. ({reason})"
					))
					.class(theme::MUTED),
				)
				.child(
					Text(
						"Starting over deletes anything this phone has not delivered yet. What \
						 your vault already holds is untouched.",
					)
					.class(theme::MUTED),
				)
				.child(
					Button("Start storage over")
						.class(theme::SECONDARY)
						.test_id("reset-storage")
						.on_press(move || state.act(Action::ResetStorage)),
				)
		},
	)
}

/// What the vault card shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct VaultPart {
	link: Link,
	vault: Option<String>,
	approve: Option<String>,
}

fn vault_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![VaultPart {
				link: s.link,
				vault: s.vault,
				approve: s.approve,
			}]
		},
		Clone::clone,
		move |part| vault_view(&state, &part),
	)
}

fn vault_view(state: &State, part: &VaultPart) -> Element {
	let place = part.vault.clone().unwrap_or_default();
	match &part.link {
		Link::Unpaired => pair_view(state),
		Link::Connecting => {
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(ActivityIndicator(true))
						.child(
							Text("Connecting to your vault…")
								.class(theme::HEADING)
								.test_id("vault"),
						),
				)
				.child(Text(place).class(theme::MUTED))
		}
		Link::Pending { code } => {
			let again = state.clone();
			View()
				.class("gap-2")
				.child(Text("STEP 2 OF 2").class(theme::EYEBROW))
				.child(
					Text("Approve this phone")
						.class(theme::TITLE)
						.test_id("vault"),
				)
				.child(
					View()
						.class(format!("{} items-center", theme::HERO))
						.child(Text("PAIRING CODE").class(theme::CAPTION))
						.child(
							Text(code.clone())
								.class("text-4xl font-bold text-[#F4F1FF]")
								.test_id("code"),
						),
				)
				.child(
					Text(
						"On your computer, check the code matches and approve it. The phone can \
						 then add to your vault; it can never read from it.",
					)
					.class(theme::MUTED),
				)
				.children(
					part.approve
						.clone()
						.map(|command| Text(command).class(theme::CODE).test_id("approve")),
				)
				.child(
					View()
						.class("gap-2")
						.child(
							Button("I approved it")
								.class(theme::PRIMARY)
								.test_id("retry")
								.on_press(move || again.send(Command::Upload)),
						)
						.child(forget_button(state, "Use another link", theme::SECONDARY)),
				)
		}
		Link::Unreachable { reason } => {
			let again = state.clone();
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(theme::tile_warn(Icon::Sync))
						.child(
							View()
								.class("flex-1 gap-1")
								.child(
									Text("Your vault is out of reach")
										.class(theme::LABEL)
										.test_id("vault"),
								)
								.child(
									Text(format!(
										"Readings keep waiting on this phone and go out when it \
										 answers. {place} ({reason})"
									))
									.class(theme::MUTED),
								),
						),
				)
				.child(
					View()
						.class("gap-2")
						.child(
							Button("Try again")
								.class(theme::PRIMARY)
								.test_id("retry")
								.on_press(move || again.send(Command::Upload)),
						)
						.child(forget_button(state, "Use another link", theme::SECONDARY)),
				)
		}
		Link::Revoked => {
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(theme::tile_bad(Icon::Lock))
						.child(
							View()
								.class("flex-1 gap-1")
								.child(
									Text("Your vault no longer accepts this phone")
										.class(theme::LABEL)
										.test_id("vault"),
								)
								.child(
									Text(
										"Its access was revoked or ran out. Forget the vault, \
										 then pair again with a new link. What is waiting stays \
										 on this phone.",
									)
									.class(theme::MUTED),
								),
						),
				)
				.child(forget_button(state, "Forget this vault", theme::SECONDARY))
		}
		Link::Ready => {
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(theme::tile_ok(Icon::Shield))
						.child(
							View()
								.class("flex-1 gap-1")
								.child(
									Text("Connected to your vault")
										.class(theme::LABEL)
										.test_id("vault"),
								)
								.child(
									Text(format!(
										"{place} · This phone adds to your vault and can read \
										 nothing back."
									))
									.class(theme::MUTED),
								),
						),
				)
				.child(forget_button(state, "Forget this vault", theme::QUIET))
		}
	}
}

/// The two steps that pair this phone: run the command, paste the link.
fn pair_view(state: &State) -> Element {
	let link = state.link;
	let connect = state.clone();
	View()
		.class("gap-2")
		.child(Text("STEP 1 OF 2").class(theme::EYEBROW))
		.child(
			Text("Connect to your vault")
				.class(theme::TITLE)
				.test_id("vault"),
		)
		.child(
			View()
				.class(theme::CARD)
				.child(step(1, "On your computer, run"))
				.child(Text("quant link").class(theme::CODE)),
		)
		.child(
			View()
				.class(theme::CARD)
				.child(step(
					2,
					"Paste the link it prints. It starts with quant://link.",
				))
				.child(
					TextInput()
						.class(theme::INPUT)
						.value(link.get())
						.placeholder("quant://link?addr=…")
						.accessibility_label("Pairing link")
						.test_id("link")
						.on_change(move |text: &str| link.set(text.to_owned())),
				)
				.child(
					Button("Connect")
						.class(theme::PRIMARY)
						.test_id("connect")
						.on_press(move || {
							let link = connect.link.get();
							if link.trim().is_empty() {
								connect.problem.set(Some(
									"Paste the link `quant link` prints first".to_owned(),
								));
							} else {
								connect.send(Command::Pair { link });
							}
						}),
				),
		)
}

/// One numbered step in the pairing instructions.
fn step(n: u8, text: &'static str) -> Element {
	View()
		.class("flex-row items-center gap-3")
		.child(
			View()
				.class("w-8 h-8 rounded-full items-center justify-center bg-[#251F5C]")
				.child(Text(n.to_string()).class("text-sm font-bold text-[#9D90FF]")),
		)
		.child(Text(text).class(theme::BODY).class("flex-1"))
}

fn forget_button(state: &State, label: &'static str, class: &'static str) -> Element {
	let state = state.clone();
	Button(label)
		.class(class)
		.test_id("forget")
		.on_press(move || {
			state.link.set(String::new());
			state.send(Command::Forget);
		})
}

/// What the collecting card shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CollectingPart {
	sources: Vec<SourceRow>,
	collector: Option<String>,
}

fn collecting_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let collector = state.collector;
	For(
		move || {
			let s = snapshot.get();
			vec![CollectingPart {
				sources: s.sources,
				collector: collector.get(),
			}]
		},
		Clone::clone,
		move |part| {
			View()
				.class(theme::CARD)
				.child(Text("What this phone records").class(theme::HEADING))
				.children(
					part.collector
						.clone()
						.map(|problem| Text(problem).class(theme::PROBLEM).test_id("collector")),
				)
				.children(
					part.sources
						.iter()
						.filter(|row| {
							cfg!(feature = "watch")
								|| row.source != Source::Wearable
								|| row.readings > 0
								|| row.problem.is_some()
						})
						.map(source_row),
				)
		},
	)
}

fn source_row(row: &SourceRow) -> Element {
	let count = match (row.readings, &row.last) {
		(0, _) => "Nothing yet".to_owned(),
		(1, Some(last)) => format!("1 reading · last {last}"),
		(n, Some(last)) => format!("{n} readings · last {last}"),
		(n, None) => format!("{n} readings"),
	};
	View()
		.class(theme::ROW)
		.child(
			View()
				.class("flex-row items-center gap-3")
				.child(theme::tile(source_icon(row.source)))
				.child(Text(row.source.label()).class(theme::LABEL).class("flex-1"))
				.child(Text(count).class(theme::CAPTION)),
		)
		.children(
			row.problem
				.clone()
				.map(|problem| Text(problem).class(theme::PROBLEM)),
		)
}

/// The line icon that names a source in its row.
fn source_icon(source: Source) -> Icon {
	match source {
		Source::Location => Icon::Pin,
		Source::Usage => Icon::Apps,
		Source::Power => Icon::Battery,
		Source::Health => Icon::Heart,
		Source::Wearable => Icon::Watch,
		Source::Motion => Icon::Motion,
		Source::WatchSync => Icon::Sync,
		Source::Ble => Icon::Bluetooth,
	}
}

fn permissions_card(state: &State) -> Element {
	let permissions = state.permissions;
	let state = state.clone();
	For(
		move || vec![permissions.get()],
		Clone::clone,
		move |permissions| {
			let steps: Vec<_> = Step::ALL
				.into_iter()
				.filter(|step| permissions.get(*step) != Grant::Unavailable)
				.collect();
			if steps.is_empty() {
				return View();
			}
			let granted = steps
				.iter()
				.filter(|step| permissions.get(**step) == Grant::Granted)
				.count();
			let total = steps.len();
			View()
				.class(theme::CARD)
				.child(
					Text(if granted == total {
						"Permissions"
					} else {
						"Finish setup"
					})
					.class(theme::HEADING),
				)
				.child(Text(format!("{granted} of {total} allowed")).class(theme::CAPTION))
				.children(
					steps
						.into_iter()
						.map(|step| permission_row(&state, step, permissions.get(step))),
				)
		},
	)
}

fn permission_row(state: &State, step: Step, grant: Grant) -> Element {
	let state = state.clone();
	let granted = grant == Grant::Granted;
	View()
		.class(theme::ROW)
		.child(
			View()
				.class("flex-row items-center gap-3")
				.child(if granted {
					theme::tile_ok(Icon::Check)
				} else {
					theme::tile(step.icon())
				})
				.child(
					View()
						.class("flex-1 gap-0.5")
						.child(Text(step.title()).class(theme::LABEL))
						.child(
							Text(if granted {
								"Allowed"
							} else {
								"Not allowed yet"
							})
							.class(theme::CAPTION)
							.test_id(format!("grant-{}", step.id())),
						),
				),
		)
		.children((!granted).then(|| Text(step.why()).class(theme::MUTED)))
		.children((!granted).then(|| {
			Button(step.ask())
				.class(if step == Step::Location {
					theme::PRIMARY
				} else {
					theme::SECONDARY
				})
				.test_id(format!("allow-{}", step.id()))
				.on_press(move || state.act(Action::Allow(step)))
		}))
}

/// What the delivery card shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DeliveryPart {
	ready: bool,
	waiting: u64,
	delivered: Option<String>,
	problem: Option<String>,
}

fn delivery_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![DeliveryPart {
				ready: s.link == Link::Ready,
				waiting: s.waiting,
				delivered: s.delivered,
				problem: s.upload_problem,
			}]
		},
		Clone::clone,
		move |part| {
			let upload = state.clone();
			let waiting = match part.waiting {
				0 => "Nothing is waiting".to_owned(),
				1 => "1 reading is waiting on this phone".to_owned(),
				n => format!("{n} readings are waiting on this phone"),
			};
			let delivered = part.delivered.as_deref().map_or_else(
				|| "Nothing delivered since the app opened".to_owned(),
				|when| format!("Last delivered {when}"),
			);
			View()
				.class(theme::CARD)
				.child(Text("Delivery").class(theme::HEADING))
				.child(
					View()
						.class("flex-row items-center gap-4")
						.child(Text(part.waiting.to_string()).class(theme::STAT))
						.child(
							View()
								.class("flex-1 gap-1")
								.child(Text(waiting).class(theme::BODY).test_id("waiting"))
								.child(Text(delivered).class(theme::MUTED).test_id("delivered")),
						),
				)
				.children(part.problem.clone().map(|problem| {
					Text(problem)
						.class(theme::PROBLEM)
						.test_id("upload-problem")
				}))
				.children((part.ready && part.waiting > 0).then(|| {
					Button("Deliver now")
						.class(theme::SECONDARY)
						.test_id("upload")
						.on_press(move || upload.send(Command::Upload))
				}))
		},
	)
}

/// What the wearables & companion watch card shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WearablesPart {
	vault_ready: bool,
	watch_connected: bool,
	watch_name: Option<String>,
	latest_heart_rate_bpm: Option<u16>,
	steps_today: Option<u64>,
	ble_devices: Vec<String>,
}

fn wearables_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![WearablesPart {
				vault_ready: s.link == Link::Ready,
				watch_connected: s.watch_connected,
				watch_name: s.watch_name,
				latest_heart_rate_bpm: s.latest_heart_rate_bpm,
				steps_today: s.steps_today,
				ble_devices: s.ble_devices,
			}]
		},
		Clone::clone,
		move |part| {
			let push = state.clone();
			let scan = state.clone();
			let watch_line = if part.watch_connected {
				part.watch_name.as_deref().map_or_else(
					|| "Watch connected".to_owned(),
					|name| format!("Watch connected · {name}"),
				)
			} else {
				"No watch linked yet · Wear OS, Apple Watch, Huawei Watch".to_owned()
			};
			let vitals = match (part.latest_heart_rate_bpm, part.steps_today) {
				(Some(bpm), Some(steps)) => Some(format!("Latest: {bpm} bpm · {steps} steps")),
				(Some(bpm), None) => Some(format!("Latest: {bpm} bpm")),
				(None, Some(steps)) => Some(format!("Latest: {steps} steps")),
				(None, None) => None,
			};
			let ble_line = if part.ble_devices.is_empty() {
				"No Bluetooth straps or rings connected".to_owned()
			} else {
				format!("Bluetooth sensors: {}", part.ble_devices.join(", "))
			};
			View()
				.class(theme::CARD)
				.child(Text("Watches and sensors").class(theme::HEADING))
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(theme::tile(Icon::Watch))
						.child(
							Text(watch_line)
								.class(theme::BODY)
								.class("flex-1")
								.test_id("watch-status"),
						),
				)
				.children(vitals.map(|line| Text(line).class(theme::MUTED).test_id("phone-vitals")))
				.child(Text(ble_line).class(theme::MUTED).test_id("ble-status"))
				.child(
					View()
						.class("gap-2")
						.children(part.vault_ready.then(|| {
							Button("Pair watch with vault")
								.class(theme::SECONDARY)
								.test_id("push-watch-link")
								.on_press(move || push.act(Action::PushPairLinkToWatch))
						}))
						.child(
							Button("Scan Bluetooth sensors")
								.class(theme::SECONDARY)
								.test_id("scan-ble")
								.on_press(move || scan.act(Action::ScanBleSensors)),
						),
				)
		},
	)
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use leptoskit_testing::Finder;
	use leptoskit_testing::Queries;
	use leptoskit_testing::WidgetTester;
	use quant_model::Notice;

	use super::*;

	fn harness(snapshot: Snapshot) -> (WidgetTester, State, Arc<Mutex<Vec<Action>>>) {
		// See the desktop window's harness: signals need a reactive owner.
		let owner = reactive_graph::owner::Owner::new();
		owner.set();
		std::mem::forget(owner);
		let done = Arc::new(Mutex::new(Vec::new()));
		let log = Arc::clone(&done);
		let state = State::new(Arc::new(move |action| log.lock().unwrap().push(action)));
		state.snapshot.set(snapshot);
		let view = state.clone();
		// Taller than a phone, so a test can reach every card without
		// scrolling.
		let tester = WidgetTester::new(move || Screen(view.clone()), 390.0, 3200.0).unwrap();
		(tester, state, done)
	}

	fn rows(location: u64) -> Vec<SourceRow> {
		Source::ALL
			.into_iter()
			.map(|source| {
				SourceRow {
					source,
					readings: if source == Source::Location {
						location
					} else {
						0
					},
					last: (source == Source::Location && location > 0).then(|| "14:02".into()),
					problem: None,
				}
			})
			.collect()
	}

	fn ready() -> Snapshot {
		Snapshot {
			version: 4,
			headline: "Collecting · 3 readings waiting".into(),
			link: Link::Ready,
			vault: Some("192.168.1.4:7717".into()),
			approve: None,
			storage: Storage::Open,
			collecting: true,
			waiting: 3,
			delivered: Some("Today 14:00".into()),
			upload_problem: None,
			sources: rows(12),
			notice: None,
			..Snapshot::default()
		}
	}

	#[test]
	fn an_unpaired_phone_asks_for_the_link_and_sends_it() {
		let (mut tester, _, done) = harness(Snapshot {
			headline: "Not connected to your vault".into(),
			collecting: true,
			sources: rows(0),
			..Snapshot::default()
		});
		tester
			.get_by_text("Measure your life. Keep it yours.")
			.unwrap();
		tester.tap(&Finder::test_id("get-started")).unwrap();
		tester.get_by_text("Connect to your vault").unwrap();

		// An empty field explains itself instead of asking the service.
		tester.tap(&Finder::test_id("connect")).unwrap();
		tester
			.get_by_text("Paste the link `quant link` prints first")
			.unwrap();
		assert!(done.lock().unwrap().is_empty());

		let link = "quant://link?addr=192.168.1.4:7717&fp=ab";
		tester.enter_text(&Finder::test_id("link"), link).unwrap();
		tester.tap(&Finder::test_id("connect")).unwrap();
		assert_eq!(
			*done.lock().unwrap(),
			[Action::Command(Command::Pair { link: link.into() })]
		);
	}

	#[test]
	fn the_welcome_screen_leads_to_pairing_and_back() {
		let (mut tester, _, done) = harness(Snapshot {
			headline: "Not connected to your vault".into(),
			collecting: true,
			sources: rows(0),
			..Snapshot::default()
		});
		tester.tap(&Finder::test_id("get-started")).unwrap();
		tester.get_by_text("STEP 1 OF 2").unwrap();

		tester.tap(&Finder::test_id("back")).unwrap();
		tester
			.get_by_text("Measure your life. Keep it yours.")
			.unwrap();
		assert!(tester.query_by_text("STEP 1 OF 2").unwrap().is_none());
		assert!(done.lock().unwrap().is_empty());
	}

	#[test]
	fn the_splash_covers_the_screen_until_it_is_cleared() {
		let (mut tester, state, _) = harness(ready());
		state.splash.set(true);
		tester.pump();
		tester
			.get_by_text("Your life, measured. Yours alone.")
			.unwrap();

		state.splash.set(false);
		tester.pump();
		assert!(
			tester
				.query_by_text("Your life, measured. Yours alone.")
				.unwrap()
				.is_none()
		);
	}

	#[test]
	fn a_waiting_phone_shows_the_code_and_the_command_that_approves_it() {
		let approve = "quant approve K7QD-M2XA --scope location:write --scope apps:write --scope \
		               device:write";
		let (mut tester, _, done) = harness(Snapshot {
			headline: "Waiting for approval · K7QD-M2XA".into(),
			link: Link::Pending {
				code: "K7QD-M2XA".into(),
			},
			vault: Some("192.168.1.4:7717".into()),
			approve: Some(approve.into()),
			sources: rows(0),
			..Snapshot::default()
		});
		tester.get_by_text("K7QD-M2XA").unwrap();
		tester.get_by_text(approve).unwrap();
		assert!(tester.query_by_text("Connect").unwrap().is_none());
		tester.tap(&Finder::test_id("retry")).unwrap();
		tester.tap(&Finder::test_id("forget")).unwrap();
		assert_eq!(
			*done.lock().unwrap(),
			[
				Action::Command(Command::Upload),
				Action::Command(Command::Forget)
			]
		);
	}

	#[test]
	fn a_paired_phone_shows_what_it_collected_and_what_is_waiting() {
		let (mut tester, state, done) = harness(ready());
		tester.get_by_text("Connected to your vault").unwrap();
		tester.get_by_text("12 readings · last 14:02").unwrap();
		tester
			.get_by_text("3 readings are waiting on this phone")
			.unwrap();
		tester.get_by_text("Last delivered Today 14:00").unwrap();
		tester.tap(&Finder::test_id("upload")).unwrap();
		tester.tap(&Finder::test_id("collecting")).unwrap();
		assert_eq!(
			*done.lock().unwrap(),
			[
				Action::Command(Command::Upload),
				Action::Command(Command::Collect { enabled: false })
			]
		);

		// A new snapshot rebuilds the cards it changes.
		state.snapshot.set(Snapshot {
			version: 5,
			headline: "Collecting · vault out of reach · 3 readings waiting".into(),
			link: Link::Unreachable {
				reason: "timed out".into(),
			},
			upload_problem: Some("The vault is out of reach".into()),
			notice: Some(Notice {
				text: "The vault did not answer: timed out".into(),
				error: true,
			}),
			..ready()
		});
		tester.pump();
		tester.get_by_text("Your vault is out of reach").unwrap();
		tester.get_by_text("The vault is out of reach").unwrap();
		tester
			.get_by_text("The vault did not answer: timed out")
			.unwrap();
		// Nothing can be delivered to a vault that is not answering.
		assert!(tester.query_by_text("Deliver now").unwrap().is_none());
	}

	#[test]
	fn each_permission_is_asked_for_on_its_own_and_disappears_once_allowed() {
		let (mut tester, state, done) = harness(ready());
		state.permissions.set(Permissions {
			location: Grant::Needed,
			always: Grant::Unknown,
			notifications: Grant::Granted,
			usage: Grant::Needed,
			battery: Grant::Unavailable,
			health: Grant::Unavailable,
			wearable: Grant::Unavailable,
			bluetooth: Grant::Unavailable,
		});
		state
			.collector
			.set(Some("Allow location to collect in the background".into()));
		tester.pump();
		tester
			.get_by_text("Allow location to collect in the background")
			.unwrap();
		assert!(
			tester
				.query_by_text("Allow notifications")
				.unwrap()
				.is_none()
		);
		// A phone with nothing to allow for a step does not list it.
		assert!(tester.query_by_text("Battery").unwrap().is_none());
		tester.tap(&Finder::test_id("allow-location")).unwrap();
		tester.tap(&Finder::test_id("allow-usage")).unwrap();
		assert_eq!(
			*done.lock().unwrap(),
			[Action::Allow(Step::Location), Action::Allow(Step::Usage)]
		);

		let mut allowed = state.permissions.get();
		allowed.set(Step::Location, Grant::Granted);
		state.permissions.set(allowed);
		tester.pump();
		assert!(tester.query_by_text("Allow location").unwrap().is_none());
		tester.get_by_text("Open Usage Access").unwrap();
	}

	#[test]
	fn wearables_card_pushes_pair_link_and_scans_ble_sensors() {
		let (mut tester, _, done) = harness(Snapshot {
			watch_connected: true,
			watch_name: Some("Galaxy Watch 7".into()),
			latest_heart_rate_bpm: Some(67),
			steps_today: Some(4_100),
			ble_devices: vec!["Polar H10".into()],
			..ready()
		});
		tester
			.get_by_text("Watch connected · Galaxy Watch 7")
			.unwrap();
		tester.get_by_text("Latest: 67 bpm · 4100 steps").unwrap();
		tester.get_by_text("Bluetooth sensors: Polar H10").unwrap();
		tester.tap(&Finder::test_id("push-watch-link")).unwrap();
		tester.tap(&Finder::test_id("scan-ble")).unwrap();
		assert_eq!(
			*done.lock().unwrap(),
			[Action::PushPairLinkToWatch, Action::ScanBleSensors]
		);
	}

	#[test]
	fn storage_that_cannot_be_opened_offers_to_start_over() {
		let (mut tester, _, done) = harness(Snapshot {
			headline: "This phone's storage can't be opened".into(),
			storage: Storage::Failed {
				reason: "file is not a database".into(),
			},
			..ready()
		});
		tester.tap(&Finder::test_id("reset-storage")).unwrap();
		assert_eq!(*done.lock().unwrap(), [Action::ResetStorage]);
	}

	/// A SHA-256 fingerprint as `quant link` prints it.
	const FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

	fn inbound(addr: &str) -> String {
		format!("quant://link?addr={addr}&fp={FINGERPRINT}&vault=v1")
	}

	#[test]
	fn an_inbound_link_names_its_vault_without_showing_the_url() {
		let link = inbound("192.168.1.4:7717");
		let invite = Invite::parse(&link).unwrap();
		assert_eq!(invite.link(), link.as_str());
		assert_eq!(invite.place(), "192.168.1.4:7717");
		assert!(!invite.through_relay());
	}

	#[test]
	fn a_relay_link_names_the_relay_and_a_direct_address_wins() {
		let rid = "abcdefghijklmnopqrstuvwxyz";
		let relay =
			format!("quant://link?relay=wss%3A%2F%2Frelay.example&rid={rid}&fp={FINGERPRINT}");
		let invite = Invite::parse(&relay).unwrap();
		assert_eq!(invite.place(), "relay.example");
		assert!(invite.through_relay());

		let both = format!(
			"quant://link?addr=192.168.1.4:7717&relay=wss%3A%2F%2Frelay.example&rid={rid}&\
			 fp={FINGERPRINT}"
		);
		let invite = Invite::parse(&both).unwrap();
		assert_eq!(invite.place(), "192.168.1.4:7717");
		assert!(!invite.through_relay());
	}

	#[test]
	fn malformed_and_unpinned_links_are_refused() {
		let malformed = [
			String::new(),
			"https://example.com/link".into(),
			"quant://link".into(),
			"quant://pair?addr=192.168.1.4:7717".into(),
			"quant://link?addr=192.168.1.4:7717".into(),
			"quant://link?addr=192.168.1.4:7717&fp=ab".into(),
			"quant://link?addr=192.168.1.4:7717&fp=zz".into(),
			format!("quant://link?relay=wss%3A%2F%2Frelay.example&fp={FINGERPRINT}"),
			format!("quant://link?addr=192.168.1.4%3A7717%00&fp={FINGERPRINT}"),
			format!(
				"quant://link?addr=192.168.1.4:7717&fp={FINGERPRINT}&pad={}",
				"a".repeat(Invite::MAX_BYTES)
			),
		];
		for malformed in malformed {
			assert!(Invite::parse(&malformed).is_none(), "{malformed}");
		}
	}

	#[test]
	fn an_incoming_link_is_reviewed_before_any_connection() {
		let link = inbound("192.168.1.4:7717");
		let (mut tester, state, done) = harness(Snapshot {
			headline: "Not connected to your vault".into(),
			collecting: true,
			sources: rows(0),
			..Snapshot::default()
		});
		state.invite.set(Some(Invite::parse(&link).unwrap()));
		tester.pump();

		tester.get_by_text("Connect to this vault?").unwrap();
		assert!(tester.query_by_text(&link).unwrap().is_none());
		assert!(done.lock().unwrap().is_empty());

		tester.tap(&Finder::test_id("invite-connect")).unwrap();
		assert!(state.invite.get().is_none());
		assert!(state.pairing.get());
		assert_eq!(
			*done.lock().unwrap(),
			[Action::Command(Command::Pair { link })]
		);
	}

	#[test]
	fn a_link_can_be_dismissed_without_sending_anything() {
		let link = inbound("192.168.1.4:7717");
		let (mut tester, state, done) = harness(Snapshot {
			headline: "Not connected to your vault".into(),
			collecting: true,
			sources: rows(0),
			..Snapshot::default()
		});
		state.invite.set(Some(Invite::parse(&link).unwrap()));
		tester.pump();

		tester.tap(&Finder::test_id("invite-dismiss")).unwrap();
		assert!(state.invite.get().is_none());
		assert!(done.lock().unwrap().is_empty());
		assert_eq!(state.snapshot.get().link, Link::Unpaired);
	}

	#[test]
	fn a_link_while_paired_warns_and_waits_for_connect() {
		let link = inbound("192.168.1.4:7717");
		let (mut tester, state, done) = harness(ready());
		state.invite.set(Some(Invite::parse(&link).unwrap()));
		tester.pump();

		tester.get_by_text("Connect to another vault?").unwrap();
		tester
			.get_by_text(
				"This phone is already connected. Connecting switches it to this vault; readings \
				 waiting on this phone stay.",
			)
			.unwrap();
		assert!(done.lock().unwrap().is_empty());
		assert_eq!(state.snapshot.get().link, Link::Ready);
	}

	#[test]
	fn one_link_at_a_time_is_shown() {
		let link = inbound("192.168.1.4:7717");
		let (mut tester, state, _) = harness(ready());
		let invite = Invite::parse(&link).unwrap();
		state.invite.set(Some(invite.clone()));
		state.invite.set(Some(invite));
		tester.pump();
		assert_eq!(tester.query_all(&Finder::test_id("invite")).len(), 1);
	}
}
