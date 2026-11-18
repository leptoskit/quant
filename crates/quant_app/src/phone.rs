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

use std::sync::Arc;

use leptoskit::native::reactive::For;
use leptoskit::native::reactive::Show;
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
use quant_model::phone::SourceRow;
use quant_model::phone::Storage;

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

/// What the system makes the owner allow in person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
	/// Location while the app is in use. Background collection cannot start
	/// without it.
	Location,
	/// Location all the time.
	Always,
	/// The notification Android shows while the app works in the background.
	Notifications,
	/// Usage Access, which Android grants only on its own settings screen.
	Usage,
	/// An exemption from battery optimisation.
	Battery,
}

impl Step {
	/// Every step, in the order the screen lists them.
	pub const ALL: [Step; 5] = [
		Step::Location,
		Step::Always,
		Step::Notifications,
		Step::Usage,
		Step::Battery,
	];

	fn title(self) -> &'static str {
		match self {
			Step::Location => "Location",
			Step::Always => "Location all the time",
			Step::Notifications => "Notification",
			Step::Usage => "App usage",
			Step::Battery => "Battery",
		}
	}

	fn why(self) -> &'static str {
		match self {
			Step::Location => {
				"Records where this phone goes. Nothing is collected in the background until you \
				 allow it."
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
		}
	}

	fn ask(self) -> &'static str {
		match self {
			Step::Location => "Allow location",
			Step::Always => "Allow all the time",
			Step::Notifications => "Allow notifications",
			Step::Usage => "Open Usage Access",
			Step::Battery => "Keep Quant running",
		}
	}

	fn id(self) -> &'static str {
		match self {
			Step::Location => "location",
			Step::Always => "always",
			Step::Notifications => "notifications",
			Step::Usage => "usage",
			Step::Battery => "battery",
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
	/// Why the last action failed.
	pub problem: RwSignal<Option<String>>,
	/// What stops the collector on this phone, when something does.
	pub collector: RwSignal<Option<String>>,
	act: Act,
}

impl State {
	/// State that carries out actions through `act`.
	pub fn new(act: Act) -> Self {
		Self {
			snapshot: RwSignal::new(Snapshot::default()),
			permissions: RwSignal::new(Permissions::default()),
			link: RwSignal::new(String::new()),
			problem: RwSignal::new(None),
			collector: RwSignal::new(None),
			act,
		}
	}

	fn act(&self, action: Action) {
		self.problem.set(None);
		(self.act)(action);
	}

	fn send(&self, command: Command) {
		self.act(Action::Command(command));
	}
}

const MUTED: &str = "text-sm leading-relaxed text-stone-500 dark:text-stone-400";
const CARD: &str = "gap-3 p-4 rounded-2xl border border-stone-200 bg-[#fffdf7] \
                    dark:border-stone-700 dark:bg-[#252520]";
const CONTROL: &str = "px-4 py-3 rounded-xl bg-stone-100 text-stone-800 dark:bg-stone-700 \
                       dark:text-stone-100 text-sm";
const PRIMARY: &str = "px-4 py-3 rounded-xl bg-[#ffd43b] text-stone-900 text-sm font-semibold";
const HEADING: &str = "text-base font-semibold text-stone-900 dark:text-stone-100";
const BODY: &str = "text-sm leading-relaxed text-stone-800 dark:text-stone-100";
const PROBLEM: &str = "text-sm leading-relaxed text-red-700 dark:text-red-300";
const CODE: &str = "p-3 rounded-xl bg-stone-100 text-stone-900 dark:bg-stone-800 \
                    dark:text-stone-100 text-xs leading-relaxed";

/// The whole screen.
#[component]
pub fn Screen(state: State) -> Element {
	ScrollView()
		.class("flex-1 bg-[#fbf6ea] dark:bg-[#1b1a17]")
		.child(
			View()
				.class("gap-4 px-5 pt-safe-offset-6 pb-safe-offset-8")
				.child(header(&state))
				.child(storage_card(&state))
				.child(vault_card(&state))
				.child(permissions_card(&state))
				.child(collecting_card(&state))
				.child(delivery_card(&state)),
		)
}

fn header(state: &State) -> Element {
	let snapshot = state.snapshot;
	let problem = state.problem;
	let message = move || {
		let snapshot = snapshot.get();
		problem
			.get()
			.or_else(|| snapshot.notice.map(|notice| notice.text))
			.filter(|text| !text.is_empty() && *text != snapshot.headline)
	};
	View()
		.class("gap-2 pb-2")
		.child(
			View()
				.class("flex-row items-center justify-between gap-4")
				.child(
					Text("quant")
						.class("text-3xl font-semibold text-stone-900 dark:text-stone-100"),
				)
				.child(
					Text("YOUR PRIVATE MEMORY")
						.class("text-[10px] text-stone-500 dark:text-stone-400"),
				),
		)
		.child(
			Text(move || snapshot.get().headline)
				.class("text-base font-medium text-stone-700 dark:text-stone-200")
				.test_id("headline"),
		)
		.child(Show(
			move || message().is_some(),
			move || {
				Text(move || message().unwrap_or_default())
					.class(MUTED)
					.test_id("notice")
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
				.class(CARD)
				.child(Text("This phone's storage can't be opened").class(HEADING))
				.child(
					Text(format!(
						"Readings wait in an encrypted store whose key lives in this phone's \
						 keystore, and the two no longer match. ({reason})"
					))
					.class(MUTED),
				)
				.child(
					Text(
						"Starting over deletes anything this phone has not delivered yet. What \
						 your vault already holds is untouched.",
					)
					.class(MUTED),
				)
				.child(
					Button("Start storage over")
						.class(CONTROL)
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
	let card = View().class(CARD);
	let place = part.vault.clone().unwrap_or_default();
	match &part.link {
		Link::Unpaired => pair_view(state, card),
		Link::Connecting => {
			card.child(
				Text("Connecting to your vault…")
					.class(HEADING)
					.test_id("vault"),
			)
			.child(Text(place).class(MUTED))
		}
		Link::Unreachable { reason } => {
			let (again, forget) = (state.clone(), state.clone());
			card.child(
				Text("Your vault is out of reach")
					.class(HEADING)
					.test_id("vault"),
			)
			.child(
				Text(format!(
					"Readings keep waiting on this phone and go out when it answers. {place} \
					 ({reason})"
				))
				.class(MUTED),
			)
			.child(
				View()
					.class("flex-row flex-wrap gap-2")
					.child(
						Button("Try again")
							.class(PRIMARY)
							.test_id("retry")
							.on_press(move || again.send(Command::Upload)),
					)
					.child(forget_button(&forget, "Use another link")),
			)
		}
		Link::Pending { code } => {
			let again = state.clone();
			card.child(Text("Approve this phone").class(HEADING).test_id("vault"))
				.child(
					Text(code.clone())
						.class("text-2xl font-semibold text-stone-900 dark:text-stone-100")
						.test_id("code"),
				)
				.child(
					Text(
						"On your computer, check the code matches and approve it. The phone can \
						 then add to your vault; it can never read from it.",
					)
					.class(MUTED),
				)
				.children(
					part.approve
						.clone()
						.map(|command| Text(command).class(CODE).test_id("approve")),
				)
				.child(
					View()
						.class("flex-row flex-wrap gap-2")
						.child(
							Button("I approved it")
								.class(PRIMARY)
								.test_id("retry")
								.on_press(move || again.send(Command::Upload)),
						)
						.child(forget_button(state, "Use another link")),
				)
		}
		Link::Revoked => {
			card.child(
				Text("Your vault no longer accepts this phone")
					.class(HEADING)
					.test_id("vault"),
			)
			.child(
				Text(
					"Its access was revoked or ran out. Forget the vault, then pair again with a \
					 new link. What is waiting stays on this phone.",
				)
				.class(MUTED),
			)
			.child(forget_button(state, "Forget this vault"))
		}
		Link::Ready => {
			card.child(
				Text("Connected to your vault")
					.class(HEADING)
					.test_id("vault"),
			)
			.child(
				Text(format!(
					"{place} · This phone adds to your vault and can read nothing back."
				))
				.class(MUTED),
			)
			.child(forget_button(state, "Forget this vault"))
		}
	}
}

fn pair_view(state: &State, card: Element) -> Element {
	let link = state.link;
	let connect = state.clone();
	card.child(
		Text("Connect to your vault")
			.class(HEADING)
			.test_id("vault"),
	)
	.child(
		Text(
			"On the computer that holds your vault, run `quant link`. Paste the link it prints \
			 here. It starts with quant://link.",
		)
		.class(MUTED),
	)
	.child(
		TextInput()
			.class("px-3 py-3 rounded-xl border border-stone-300 dark:border-stone-600 text-sm")
			.value(link.get())
			.placeholder("quant://link?addr=…")
			.accessibility_label("Pairing link")
			.test_id("link")
			.on_change(move |text: &str| link.set(text.to_owned())),
	)
	.child(
		Button("Connect")
			.class(PRIMARY)
			.test_id("connect")
			.on_press(move || {
				let link = connect.link.get();
				if link.trim().is_empty() {
					connect
						.problem
						.set(Some("Paste the link `quant link` prints first".to_owned()));
				} else {
					connect.send(Command::Pair { link });
				}
			}),
	)
}

fn forget_button(state: &State, label: &'static str) -> Element {
	let state = state.clone();
	Button(label)
		.class(CONTROL)
		.test_id("forget")
		.on_press(move || {
			state.link.set(String::new());
			state.send(Command::Forget);
		})
}

/// What the collecting card shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CollectingPart {
	collecting: bool,
	sources: Vec<SourceRow>,
	collector: Option<String>,
}

fn collecting_card(state: &State) -> Element {
	let snapshot = state.snapshot;
	let collector = state.collector;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![CollectingPart {
				collecting: s.collecting,
				sources: s.sources,
				collector: collector.get(),
			}]
		},
		Clone::clone,
		move |part| {
			let toggle = state.clone();
			View()
				.class(CARD)
				.child(
					View()
						.class("flex-row items-center justify-between gap-3")
						.child(Text("Collect on this phone").class(HEADING))
						.child(
							Switch(part.collecting)
								.accessibility_label("Collect on this phone")
								.test_id("collecting")
								.on_toggle(move |enabled| {
									toggle.send(Command::Collect { enabled });
								}),
						),
				)
				.children(
					part.collector
						.clone()
						.map(|problem| Text(problem).class(PROBLEM).test_id("collector")),
				)
				.children(part.sources.iter().map(source_row))
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
		.class("gap-1 pt-3 border-t border-stone-200 dark:border-stone-700")
		.child(
			View()
				.class("flex-row items-center justify-between gap-3")
				.child(Text(row.source.label()).class(BODY))
				.child(Text(count).class(MUTED)),
		)
		.children(
			row.problem
				.clone()
				.map(|problem| Text(problem).class(PROBLEM)),
		)
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
			View()
				.class(CARD)
				.child(Text("What this phone may collect").class(HEADING))
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
		.class("gap-2 pt-3 border-t border-stone-200 dark:border-stone-700")
		.child(
			View()
				.class("flex-row items-center justify-between gap-3")
				.child(Text(step.title()).class(BODY))
				.child(
					Text(if granted {
						"Allowed"
					} else {
						"Not allowed yet"
					})
					.class(MUTED)
					.test_id(format!("grant-{}", step.id())),
				),
		)
		.children((!granted).then(|| Text(step.why()).class(MUTED)))
		.children((!granted).then(|| {
			Button(step.ask())
				.class(if step == Step::Location {
					PRIMARY
				} else {
					CONTROL
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
				.class(CARD)
				.child(Text("Delivery").class(HEADING))
				.child(Text(waiting).class(BODY).test_id("waiting"))
				.child(Text(delivered).class(MUTED).test_id("delivered"))
				.children(
					part.problem
						.clone()
						.map(|problem| Text(problem).class(PROBLEM).test_id("upload-problem")),
				)
				.children((part.ready && part.waiting > 0).then(|| {
					Button("Deliver now")
						.class(CONTROL)
						.test_id("upload")
						.on_press(move || upload.send(Command::Upload))
				}))
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
	use quant_model::phone::Source;

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
		let tester = WidgetTester::new(move || Screen(view.clone()), 390.0, 2600.0).unwrap();
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
		tester.get_by_text("Not connected to your vault").unwrap();
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
}
