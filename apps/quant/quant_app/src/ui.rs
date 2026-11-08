//! The "Open Quant" window.
//!
//! The service sends a [`Snapshot`] whenever anything changes. Each section
//! below is a region keyed on the part of the snapshot it shows, so a
//! refresh rebuilds only what changed and the passphrase and rename fields
//! keep their text.

use std::sync::Arc;

use leptoskit::native::reactive::For;
use leptoskit::native::reactive::Show;
use leptoskit::prelude::Button;
use leptoskit::prelude::Checkbox;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::PasswordInput;
use leptoskit::prelude::RwSignal;
use leptoskit::prelude::ScrollView;
use leptoskit::prelude::Set;
use leptoskit::prelude::Text;
use leptoskit::prelude::TextInput;
use leptoskit::prelude::View;
use quant_model::AuditRow;
use quant_model::Command;
use quant_model::GrantRow;
use quant_model::Link;
use quant_model::Moment;
use quant_model::OwnerDeviceRow;
use quant_model::OwnerView;
use quant_model::PendingClient;
use quant_model::ScopeChoice;
use quant_model::Snapshot;
use quant_model::SourceRow;

/// The window's sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
	/// The daemon and its capture sources.
	Status,
	/// Saved moments.
	Moments,
	/// Pending pairings and live grants.
	Agents,
	/// Owner devices: Touch ID and enrolled phones.
	Devices,
	/// The audit log.
	Audit,
}

impl Tab {
	const ALL: [Tab; 5] = [
		Tab::Status,
		Tab::Moments,
		Tab::Agents,
		Tab::Devices,
		Tab::Audit,
	];

	fn label(self) -> &'static str {
		match self {
			Tab::Status => "Status",
			Tab::Moments => "Saved moments",
			Tab::Agents => "Agents",
			Tab::Devices => "Devices",
			Tab::Audit => "Audit",
		}
	}
}

/// The owner's choices for one pending client, before approving it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Approval {
	client: String,
	choice: ScopeChoice,
	lookback: usize,
}

/// Sends a command to the service.
pub type Send = Arc<dyn Fn(Command) + std::marker::Send + Sync>;

/// Whether this Mac can set up Touch ID, as `device-keys` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TouchIdAvailability {
	/// Whether there is a Secure Enclave with enrolled biometrics.
	pub available: bool,
	/// Why not, for the window to show.
	pub reason: Option<String>,
}

/// The window's state.
#[derive(Clone)]
pub struct State {
	/// The service's latest snapshot.
	pub snapshot: RwSignal<Snapshot>,
	/// The open section.
	pub tab: RwSignal<Tab>,
	/// The passphrase being typed. Cleared once it is sent.
	pub passphrase: RwSignal<String>,
	/// The moment being renamed.
	pub renaming: RwSignal<Option<String>>,
	/// The title being typed for it.
	pub title: RwSignal<String>,
	/// Choices for pending clients.
	pub approvals: RwSignal<Vec<Approval>>,
	/// Why the last command failed.
	pub problem: RwSignal<Option<String>>,
	/// This Mac's Touch ID hardware and enrollment.
	pub touch_id: RwSignal<TouchIdAvailability>,
	send: Send,
}

impl State {
	/// State that sends commands through `send`.
	pub fn new(send: Send) -> Self {
		Self {
			snapshot: RwSignal::new(Snapshot::default()),
			tab: RwSignal::new(Tab::Status),
			passphrase: RwSignal::new(String::new()),
			renaming: RwSignal::new(None),
			title: RwSignal::new(String::new()),
			approvals: RwSignal::new(Vec::new()),
			problem: RwSignal::new(None),
			touch_id: RwSignal::new(TouchIdAvailability::default()),
			send,
		}
	}

	fn send(&self, command: Command) {
		self.problem.set(None);
		(self.send)(command);
	}

	fn approval(&self, client: &PendingClient) -> Approval {
		self.approvals
			.get()
			.into_iter()
			.find(|a| a.client == client.id)
			.unwrap_or_else(|| {
				Approval {
					client: client.id.clone(),
					choice: ScopeChoice::new(&client.requested),
					lookback: quant_model::DEFAULT_LOOKBACK,
				}
			})
	}

	fn set_approval(&self, approval: Approval) {
		let mut approvals = self.approvals.get();
		approvals.retain(|a| a.client != approval.client);
		approvals.push(approval);
		self.approvals.set(approvals);
	}
}

const MUTED: &str = "text-xs text-gray-500 dark:text-gray-400";
const CARD: &str =
	"gap-2 p-3 rounded-md border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800";
const HEADING: &str = "text-sm font-semibold text-gray-900 dark:text-gray-100";
const BODY: &str = "text-sm text-gray-900 dark:text-gray-100";

/// The whole window.
pub fn screen(state: &State) -> Element {
	View()
		.class("flex-1 bg-gray-50 dark:bg-gray-900")
		.child(header(state))
		.child(tabs(state))
		.child(
			ScrollView()
				.class("flex-1")
				.child(View().class("gap-3 p-4").child(body(state))),
		)
		.child(owner_bar(state))
}

fn header(state: &State) -> Element {
	let snapshot = state.snapshot;
	let problem = state.problem;
	View()
		.class("gap-1 px-4 pt-4 pb-2")
		.child(
			Text(move || snapshot.get().headline)
				.class("text-lg font-semibold text-gray-900 dark:text-gray-100")
				.test_id("headline"),
		)
		.child(
			Text(move || {
				let snapshot = snapshot.get();
				problem
					.get()
					.or_else(|| snapshot.notice.map(|n| n.text))
					.filter(|text| *text != snapshot.headline)
					.unwrap_or_default()
			})
			.class(MUTED)
			.test_id("notice"),
		)
}

fn tabs(state: &State) -> Element {
	let tab = state.tab;
	For(
		move || vec![tab.get()],
		|open| *open,
		move |open| {
			View()
				.class("flex-row gap-1 px-4 pb-2")
				.children(Tab::ALL.into_iter().map(|each| {
					let label = if each == open {
						format!("● {}", each.label())
					} else {
						each.label().to_owned()
					};
					Button(label)
						.test_id(format!("tab-{}", each.label()))
						.on_press(move || tab.set(each))
				}))
		},
	)
}

fn body(state: &State) -> Element {
	let tab = state.tab;
	let (status, moments, agents, devices, audit) = (
		state.clone(),
		state.clone(),
		state.clone(),
		state.clone(),
		state.clone(),
	);
	View()
		.class("gap-3")
		.child(Show(
			move || tab.get() == Tab::Status,
			move || status_tab(&status),
		))
		.child(Show(
			move || tab.get() == Tab::Moments,
			move || moments_tab(&moments),
		))
		.child(Show(
			move || tab.get() == Tab::Agents,
			move || agents_tab(&agents),
		))
		.child(Show(
			move || tab.get() == Tab::Devices,
			move || devices_tab(&devices),
		))
		.child(Show(
			move || tab.get() == Tab::Audit,
			move || audit_tab(&audit),
		))
}

/// What the status section shows, as a region key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StatusPart {
	link: Link,
	daemon: Option<String>,
	paused: bool,
	sources: Vec<SourceRow>,
	opens_at_login: Option<bool>,
}

fn status_tab(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![StatusPart {
				link: s.link,
				daemon: s.daemon,
				paused: s.paused,
				sources: s.sources,
				opens_at_login: s.opens_at_login,
			}]
		},
		Clone::clone,
		move |part| status_view(&state, &part),
	)
}

fn status_view(state: &State, part: &StatusPart) -> Element {
	let ready = part.link == Link::Ready;
	let mut view = View().class("gap-3").child(link_card(part));
	if ready {
		view = view.child(sources_card(state, &part.sources));
		view = view.child(capture_card(state, part.paused));
	}
	if let Some(enabled) = part.opens_at_login {
		let state = state.clone();
		view = view.child(
			View()
				.class("flex-row items-center gap-2")
				.child(
					Checkbox(enabled)
						.accessibility_label("Open Quant at login")
						.test_id("open-at-login")
						.on_toggle(move |enabled| state.send(Command::OpenAtLogin { enabled })),
				)
				.child(Text("Open Quant at login").class(BODY)),
		);
	}
	view
}

fn link_card(part: &StatusPart) -> Element {
	let (title, detail) = match &part.link {
		Link::Starting => ("Connecting to Quant…".to_owned(), String::new()),
		Link::Unreachable { reason } => {
			(
				"Quant isn't running".to_owned(),
				format!("Start it with `quant service install` or `quant daemon`. ({reason})"),
			)
		}
		Link::Pending { code } => {
			(
				format!("Waiting for approval · {code}"),
				format!(
					"Run `quant approve {code}` in a terminal, or unlock below and approve “Quant \
					 desktop” under Agents."
				),
			)
		}
		Link::Revoked => {
			(
				"Access revoked".to_owned(),
				"The owner revoked this app. Delete desktop.identity.json from Quant's clients \
				 folder to pair it again."
					.to_owned(),
			)
		}
		Link::Ready => {
			(
				part.daemon
					.as_deref()
					.map_or_else(|| "Connected".to_owned(), |v| format!("quantd {v}")),
				"The daemon records this Mac; this app only reads its status and saves moments."
					.to_owned(),
			)
		}
	};
	View()
		.class(CARD)
		.child(Text(title).class(HEADING).test_id("link"))
		.children((!detail.is_empty()).then(|| Text(detail).class(MUTED)))
}

fn sources_card(state: &State, sources: &[SourceRow]) -> Element {
	let rows = sources.iter().map(|source| {
		let state = state.clone();
		let mut row = View()
			.class("flex-row items-center gap-2")
			.child(
				Text(&source.name)
					.class("w-20 text-sm font-medium text-gray-900 dark:text-gray-100"),
			)
			.child(
				Text(if source.running { "Running" } else { "Stopped" }).class(if source.running {
					"w-16 text-sm text-green-600"
				} else {
					"w-16 text-sm text-red-600"
				}),
			)
			.child(
				Text(source.detail.clone().unwrap_or_default())
					.class("flex-1 text-xs text-gray-500 dark:text-gray-400"),
			);
		if let Some(pane) = source.settings {
			row = row.child(
				Button("Open Settings")
					.test_id(format!("settings-{}", source.name))
					.on_press(move || state.send(Command::OpenSettings { pane })),
			);
		}
		row
	});
	View()
		.class(CARD)
		.child(Text("Capture sources").class(HEADING))
		.children(rows)
		.children(
			sources
				.is_empty()
				.then(|| Text("No sources have reported yet.").class(MUTED)),
		)
}

fn capture_card(state: &State, paused: bool) -> Element {
	let row = View().class("flex-row flex-wrap gap-2");
	let row = if paused {
		let state = state.clone();
		row.child(
			Button("Resume")
				.test_id("resume")
				.on_press(move || state.send(Command::Resume)),
		)
	} else {
		row.children(
			[
				("Pause 30 minutes", Some(30)),
				("Pause 1 hour", Some(60)),
				("Pause until resumed", None),
			]
			.into_iter()
			.map(|(label, minutes)| {
				let state = state.clone();
				Button(label).on_press(move || state.send(Command::Pause { minutes }))
			}),
		)
	};
	View()
		.class(CARD)
		.child(
			Text(if paused {
				"Capture is paused"
			} else {
				"Capture"
			})
			.class(HEADING),
		)
		.child(row)
}

fn moments_tab(state: &State) -> Element {
	let snapshot = state.snapshot;
	let renaming = state.renaming;
	let state = state.clone();
	For(
		move || {
			vec![(
				snapshot.get().link == Link::Ready,
				snapshot.get().moments,
				renaming.get(),
			)]
		},
		Clone::clone,
		move |(ready, moments, renaming)| {
			moments_view(&state, ready, &moments, renaming.as_deref())
		},
	)
}

fn moments_view(state: &State, ready: bool, moments: &[Moment], renaming: Option<&str>) -> Element {
	let save = View()
		.class("flex-row flex-wrap gap-2")
		.children([5, 15, 30, 60].map(|minutes| {
			let state = state.clone();
			Button(format!("Save last {minutes} min"))
				.enabled(ready)
				.test_id(format!("save-{minutes}"))
				.on_press(move || state.send(Command::Save { minutes }))
		}));
	let list = moments
		.iter()
		.map(|moment| moment_row(state, moment, renaming == Some(&moment.id)));
	View()
		.class("gap-3")
		.child(
			View()
				.class(CARD)
				.child(Text("Save what you just heard").class(HEADING))
				.child(
					Text("Quant keeps a rolling 24-hour audio buffer; saving keeps part of it.")
						.class(MUTED),
				)
				.child(save),
		)
		.children(list)
		.children(moments.is_empty().then(|| {
			Text("No saved moments yet. Save the last few minutes from here or the menu bar.")
				.class(MUTED)
		}))
}

fn moment_row(state: &State, moment: &Moment, renaming: bool) -> Element {
	let id = moment.id.clone();
	let mut card = View().class(CARD);
	if renaming {
		let title = state.title;
		let (save, cancel) = (state.clone(), state.clone());
		let save_id = id.clone();
		card = card.child(
			View()
				.class("flex-row items-center gap-2")
				.child(
					TextInput()
						.class("flex-1")
						.value(moment.title.clone())
						.accessibility_label("Title")
						.test_id("rename-title")
						.on_change(move |text: &str| title.set(text.to_owned())),
				)
				.child(Button("Save").test_id("rename-save").on_press(move || {
					save.send(Command::Rename {
						id: save_id.clone(),
						title: save.title.get(),
					});
					save.renaming.set(None);
				}))
				.child(Button("Cancel").on_press(move || cancel.renaming.set(None))),
		);
	} else {
		card = card.child(Text(&moment.title).class(HEADING));
	}
	card = card.child(Text(format!("{} · {}", moment.when, moment.duration)).class(MUTED));
	if let Some(snippet) = &moment.snippet {
		card = card.child(Text(snippet).class(BODY));
	}
	let (play, rename) = (state.clone(), state.clone());
	let (play_id, title) = (id.clone(), moment.title.clone());
	card.child(
		View()
			.class("flex-row gap-2")
			.child(
				Button("Play")
					.enabled(moment.playable)
					.test_id(format!("play-{id}"))
					.on_press(move || {
						play.send(Command::Play {
							id: play_id.clone(),
						});
					}),
			)
			.child(
				Button("Rename")
					.test_id(format!("rename-{id}"))
					.on_press(move || {
						rename.title.set(title.clone());
						rename.renaming.set(Some(id.clone()));
					}),
			),
	)
}

fn agents_tab(state: &State) -> Element {
	let snapshot = state.snapshot;
	let approvals = state.approvals;
	let state = state.clone();
	For(
		move || vec![(snapshot.get().owner, approvals.get())],
		Clone::clone,
		move |(owner, _)| {
			match owner {
				Some(owner) => agents_view(&state, &owner),
				None => locked("Unlock to see and approve agents."),
			}
		},
	)
}

fn agents_view(state: &State, owner: &OwnerView) -> Element {
	View()
		.class("gap-3")
		.child(Text("Waiting for approval").class(HEADING))
		.children(
			owner
				.pending
				.iter()
				.map(|client| pending_card(state, client)),
		)
		.children(
			owner
				.pending
				.is_empty()
				.then(|| Text("Nobody is waiting.").class(MUTED)),
		)
		.child(Text("Active grants").class(HEADING))
		.children(owner.grants.iter().map(|grant| grant_card(state, grant)))
		.children(
			owner
				.grants
				.is_empty()
				.then(|| Text("No live grants.").class(MUTED)),
		)
}

fn pending_card(state: &State, client: &PendingClient) -> Element {
	let approval = state.approval(client);
	let scopes = approval.choice.choices().iter().map(|(scope, granted)| {
		let state = state.clone();
		let (scope, approval) = (scope.clone(), approval.clone());
		let label = Text(scope.clone()).class(BODY);
		View()
			.class("flex-row items-center gap-2")
			.child(
				Checkbox(*granted)
					.accessibility_label(scope.clone())
					.test_id(format!("scope-{scope}"))
					.on_toggle(move |granted| {
						let mut approval = approval.clone();
						approval.choice.set(&scope, granted);
						state.set_approval(approval);
					}),
			)
			.child(label)
	});
	let lookbacks = View().class("flex-row flex-wrap gap-1").children(
		quant_model::LOOKBACKS
			.iter()
			.enumerate()
			.map(|(index, (label, _))| {
				let state = state.clone();
				let approval = approval.clone();
				let label = if index == approval.lookback {
					format!("● {label}")
				} else {
					(*label).to_owned()
				};
				Button(label).on_press(move || {
					state.set_approval(Approval {
						lookback: index,
						..approval.clone()
					});
				})
			}),
	);
	let command = quant_model::approve(client, &approval.choice, approval.lookback);
	let (approve, deny) = (state.clone(), state.clone());
	let id = client.id.clone();
	View()
		.class(CARD)
		.child(Text(format!("{} ({})", client.name, client.kind)).class(HEADING))
		.child(
			Text(format!(
				"Pairing code {} · asked {}",
				client.code, client.asked
			))
			.class(MUTED),
		)
		.children(scopes)
		.children(
			client
				.requested
				.is_empty()
				.then(|| Text("It asked for no scopes.").class(MUTED)),
		)
		.child(Text("How far back it may see").class(MUTED))
		.child(lookbacks)
		.child(
			View()
				.class("flex-row gap-2")
				.child(
					Button("Approve")
						.enabled(command.is_some())
						.test_id(format!("approve-{}", client.code))
						.on_press(move || {
							if let Some(command) = command.clone() {
								approve.send(command);
							}
						}),
				)
				.child(
					Button("Deny")
						.test_id(format!("deny-{}", client.code))
						.on_press(move || deny.send(Command::Deny { client: id.clone() })),
				),
		)
}

fn grant_card(state: &State, grant: &GrantRow) -> Element {
	let state = state.clone();
	let id = grant.id.clone();
	let expires = grant
		.expires
		.as_ref()
		.map_or_else(String::new, |at| format!(" · expires {at}"));
	View()
		.class(CARD)
		.child(
			View()
				.class("flex-row items-center justify-between gap-2")
				.child(Text(&grant.label).class(HEADING))
				.child(
					Button("Revoke")
						.test_id(format!("revoke-{}", grant.label))
						.on_press(move || state.send(Command::Revoke { grant: id.clone() })),
				),
		)
		.child(Text(grant.scopes.join("  ")).class(BODY))
		.child(Text(format!("Sees back {}{expires}", grant.lookback)).class(MUTED))
}

fn devices_tab(state: &State) -> Element {
	let snapshot = state.snapshot;
	let touch_id = state.touch_id;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			vec![(s.owner, s.touch_id.device_id, touch_id.get())]
		},
		Clone::clone,
		move |(owner, device_id, availability)| {
			match owner {
				Some(owner) => devices_view(&state, &owner, device_id.as_deref(), &availability),
				None => locked("Unlock to manage owner devices."),
			}
		},
	)
}

fn devices_view(
	state: &State,
	owner: &OwnerView,
	device_id: Option<&str>,
	availability: &TouchIdAvailability,
) -> Element {
	View()
		.class("gap-3")
		.child(touch_id_card(state, owner, device_id, availability))
		.child(Text("Owner devices").class(HEADING))
		.children(
			owner
				.devices
				.iter()
				.map(|device| device_card(state, device, device_id == Some(device.id.as_str()))),
		)
		.children(
			owner
				.devices
				.is_empty()
				.then(|| Text("No owner devices enrolled.").class(MUTED)),
		)
}

fn touch_id_card(
	state: &State,
	owner: &OwnerView,
	device_id: Option<&str>,
	availability: &TouchIdAvailability,
) -> Element {
	let state = state.clone();
	let set_up = device_id.is_some();
	let label = owner
		.devices
		.iter()
		.find(|device| device_id == Some(device.id.as_str()))
		.map(|device| device.label.clone());
	let mut card = View().class(CARD).child(Text("Touch ID").class(HEADING));
	if set_up {
		let shown = label.unwrap_or_else(|| "this Mac".to_owned());
		card = card
			.child(
				Text(format!("Touch ID is on for {shown}."))
					.class(BODY)
					.test_id("touch-id-on"),
			)
			.child(
				View().class("flex-row gap-2").child(
					Button("Turn off")
						.test_id("touch-id-off")
						.on_press(move || turn_off_touch_id(&state)),
				),
			);
	} else if availability.available && owner.passphrase_session {
		card = card
			.child(Text("Unlock with a fingerprint instead of the passphrase.").class(MUTED))
			.child(
				Button("Set up Touch ID")
					.test_id("touch-id-set-up")
					.on_press(move || start_set_up_touch_id(&state)),
			);
	} else if let Some(reason) = &availability.reason {
		card = card.child(Text(reason).class(MUTED).test_id("touch-id-unavailable"));
	} else if !owner.passphrase_session {
		card = card.child(Text("Unlock with your passphrase to set up Touch ID.").class(MUTED));
	} else {
		card = card.child(Text("Touch ID isn't available on this Mac.").class(MUTED));
	}
	card
}

fn device_card(state: &State, device: &OwnerDeviceRow, this_mac: bool) -> Element {
	let state = state.clone();
	let id = device.id.clone();
	let last_used = device.last_used.as_deref().map_or_else(
		|| "never used".to_owned(),
		|when| format!("last used {when}"),
	);
	let is_this = this_mac;
	View()
		.class(CARD)
		.child(
			View()
				.class("flex-row items-center justify-between gap-2")
				.child(Text(&device.label).class(HEADING))
				.child(
					Button("Revoke")
						.test_id(format!("revoke-device-{id}"))
						.on_press(move || revoke_device(&state, &id, is_this)),
				),
		)
		.child(Text(format!("fingerprint {} · {last_used}", device.fingerprint)).class(MUTED))
}

fn audit_tab(state: &State) -> Element {
	let snapshot = state.snapshot;
	For(
		move || vec![snapshot.get().owner.map(|o| (o.chain_ok, o.audit))],
		Clone::clone,
		|audit| {
			match audit {
				Some((chain_ok, rows)) => audit_view(chain_ok, &rows),
				None => locked("Unlock to read the audit log."),
			}
		},
	)
}

fn audit_view(chain_ok: Option<bool>, rows: &[AuditRow]) -> Element {
	let (chain, class) = match chain_ok {
		Some(true) => {
			(
				"✓ The audit log's hash chain verifies",
				"text-sm text-green-600",
			)
		}
		Some(false) => {
			(
				"✗ The audit log's hash chain is broken",
				"text-sm text-red-600",
			)
		}
		None => ("The audit log's hash chain wasn't checked", MUTED),
	};
	View()
		.class("gap-2")
		.child(Text(chain).class(class).test_id("chain"))
		.child(Text(format!("The last {} entries, newest first", rows.len())).class(MUTED))
		.children(rows.iter().map(|row| {
			View()
				.class("gap-1 py-1 border-b border-gray-200 dark:border-gray-700")
				.child(
					View()
						.class("flex-row gap-2")
						.child(Text(format!("#{}", row.seq)).class("w-12 text-xs text-gray-500"))
						.child(Text(&row.at).class("w-28 text-xs text-gray-500"))
						.child(
							Text(&row.actor).class(
								"flex-1 text-xs font-medium text-gray-900 dark:text-gray-100",
							),
						)
						.child(
							Text(&row.outcome).class(if row.outcome.starts_with("denied") {
								"text-xs text-red-600"
							} else {
								"text-xs text-gray-500"
							}),
						),
				)
				.child(
					Text(if row.target.is_empty() {
						row.action.clone()
					} else {
						format!("{} {}", row.action, row.target)
					})
					.class(BODY),
				)
		}))
}

fn locked(text: &str) -> Element {
	View().class(CARD).child(Text(text).class(MUTED))
}

/// Touch ID flows that need the native guest's Secure Enclave key. They run
/// only on the native guest; there is no software-key fallback anywhere.
#[cfg(all(target_arch = "wasm32", feature = "native"))]
pub(crate) mod touch_id {
	use leptoskit::prelude::Get;
	use leptoskit::prelude::Set;
	use leptoskit_plugin_device_keys::Biometry;
	use leptoskit_plugin_device_keys::Hardware;
	use leptoskit_plugin_device_keys::KeyError;
	use leptoskit_plugin_device_keys::Policy;
	use leptoskit_plugin_device_keys::guest::DeviceKeys;
	use quant_model::Command;
	use quant_model::ReplyData;

	use super::State;
	use super::TouchIdAvailability;

	/// The Secure Enclave key alias.
	const ALIAS: &str = "quant-owner";

	/// Check the hardware and enrollment, and record it on `state`.
	pub async fn check(state: State) {
		let Ok(keys) = DeviceKeys::new() else {
			state.touch_id.set(TouchIdAvailability {
				available: false,
				reason: Some("Touch ID is not available on this Mac.".to_owned()),
			});
			return;
		};
		let available = keys.available().await;
		let (ok, reason) = if available.hardware != Hardware::SecureEnclave {
			(false, Some("This Mac has no Secure Enclave.".to_owned()))
		} else if available.biometry == Biometry::None {
			(false, Some("This Mac has no Touch ID sensor.".to_owned()))
		} else if !available.enrolled {
			(
				false,
				Some("Add a fingerprint in System Settings, then set up Touch ID.".to_owned()),
			)
		} else {
			(true, None)
		};
		state.touch_id.set(TouchIdAvailability {
			available: ok,
			reason,
		});
	}

	/// Unlock with the Secure Enclave key. On `Cancelled`, `LockedOut`, or
	/// `Unavailable` the passphrase field is left in place; on `Invalidated`
	/// or a daemon refusal the key and settings are cleared and setup starts
	/// over.
	pub async fn unlock(state: State) {
		let Some(device) = state.snapshot.get().touch_id.device_id.clone() else {
			return;
		};
		let Ok(keys) = DeviceKeys::new() else {
			state
				.problem
				.set(Some("Touch ID is not available.".to_owned()));
			return;
		};
		let reply = crate::bridge::send(&Command::TouchIdChallenge).await;
		let Some(ReplyData::TouchIdChallenge { challenge, message }) =
			reply.ok.then(|| reply.data).flatten()
		else {
			state.problem.set(reply.error);
			return;
		};
		match keys.sign(ALIAS, &message, "Unlock Quant").await {
			Ok(signature) => {
				let reply = crate::bridge::send(&Command::UnlockWithDevice {
					device,
					challenge,
					signature: signature.der().to_vec(),
				})
				.await;
				if !reply.ok {
					let _ = keys.delete(ALIAS).await;
					state.send(Command::TouchIdOff);
					state.problem.set(reply.error);
				}
			}
			Err(KeyError::Cancelled) => {
				state
					.problem
					.set(Some("Touch ID cancelled; use your passphrase.".to_owned()));
			}
			Err(KeyError::LockedOut) => {
				state.problem.set(Some(
					"Touch ID is locked out; use your passphrase.".to_owned(),
				));
			}
			Err(KeyError::Unavailable) | Err(KeyError::NotEnrolled) => {
				state.problem.set(Some(
					"Touch ID is unavailable; use your passphrase.".to_owned(),
				));
			}
			Err(KeyError::Invalidated) => {
				let _ = keys.delete(ALIAS).await;
				state.send(Command::TouchIdOff);
				state.problem.set(Some(
					"Your fingerprints changed; set up Touch ID again.".to_owned(),
				));
			}
			Err(KeyError::NotFound) => {
				state.send(Command::TouchIdOff);
				state
					.problem
					.set(Some("Touch ID needs setting up again.".to_owned()));
			}
			Err(KeyError::AlreadyExists) | Err(KeyError::Failed) => {
				state
					.problem
					.set(Some("Touch ID failed; use your passphrase.".to_owned()));
			}
		}
	}

	/// Create the Secure Enclave key and enrol its public half. The window
	/// offers this only with a passphrase session and enrolled biometrics.
	pub async fn set_up(state: State) {
		let Ok(keys) = DeviceKeys::new() else {
			state
				.problem
				.set(Some("Touch ID is not available.".to_owned()));
			return;
		};
		// An earlier attempt may have left a key; start clean.
		let _ = keys.delete(ALIAS).await;
		match keys.generate(ALIAS, Policy::Biometry).await {
			Ok(public) => {
				let reply = crate::bridge::send(&Command::EnrolTouchId {
					public_key: public.sec1().to_vec(),
				})
				.await;
				if !reply.ok {
					state.problem.set(reply.error);
				}
			}
			Err(error) => {
				state.problem.set(Some(match error {
					KeyError::Cancelled => "Touch ID cancelled.".to_owned(),
					KeyError::LockedOut => "Touch ID is locked out.".to_owned(),
					KeyError::NotEnrolled => {
						"Add a fingerprint in System Settings first.".to_owned()
					}
					KeyError::Unavailable => "Touch ID is not available.".to_owned(),
					_ => "Touch ID could not be set up.".to_owned(),
				}));
			}
		}
	}

	/// Delete the Secure Enclave key, best effort.
	pub async fn delete_key() {
		if let Ok(keys) = DeviceKeys::new() {
			let _ = keys.delete(ALIAS).await;
		}
	}
}

fn start_unlock_touch_id(state: &State) {
	#[cfg(all(target_arch = "wasm32", feature = "native"))]
	leptoskit::guest::spawn(touch_id::unlock(state.clone()));
	#[cfg(not(all(target_arch = "wasm32", feature = "native")))]
	let _ = state;
}

fn start_set_up_touch_id(state: &State) {
	#[cfg(all(target_arch = "wasm32", feature = "native"))]
	leptoskit::guest::spawn(touch_id::set_up(state.clone()));
	#[cfg(not(all(target_arch = "wasm32", feature = "native")))]
	let _ = state;
}

fn turn_off_touch_id(state: &State) {
	state.send(Command::TouchIdOff);
	#[cfg(all(target_arch = "wasm32", feature = "native"))]
	leptoskit::guest::spawn(touch_id::delete_key());
}

fn revoke_device(state: &State, id: &str, this_mac: bool) {
	state.send(Command::RevokeDevice {
		device: id.to_owned(),
	});
	if this_mac {
		#[cfg(all(target_arch = "wasm32", feature = "native"))]
		leptoskit::guest::spawn(touch_id::delete_key());
	}
}

fn owner_bar(state: &State) -> Element {
	let snapshot = state.snapshot;
	let (unlock, lock) = (state.clone(), state.clone());
	View()
		.class(
			"gap-2 px-4 py-3 border-t border-gray-200 bg-white dark:border-gray-700 \
			 dark:bg-gray-800",
		)
		.child(Show(
			move || snapshot.get().owner.is_none(),
			move || unlock_form(&unlock),
		))
		.child(Show(
			move || snapshot.get().owner.is_some(),
			move || lock_bar(&lock),
		))
}

fn unlock_form(state: &State) -> Element {
	let snapshot = state.snapshot;
	let passphrase = state.passphrase;
	let submit = state.clone();
	let touch = state.clone();
	View()
		.class("gap-2")
		.child(
			Text(move || {
				snapshot.get().unlock_reason.unwrap_or_else(|| {
					"Approving agents, pausing, and the audit log need your passphrase. It is kept \
					 for this session only, never stored."
						.to_owned()
				})
			})
			.class(MUTED)
			.test_id("unlock-reason"),
		)
		.child(Show(
			move || snapshot.get().touch_id.device_id.is_some(),
			move || {
				let touch = touch.clone();
				View().class("flex-row items-center gap-2").child(
					Button("Unlock with Touch ID")
						.test_id("unlock-touch-id")
						.on_press(move || start_unlock_touch_id(&touch)),
				)
			},
		))
		.child(
			View()
				.class("flex-row items-center gap-2")
				.child(
					PasswordInput()
						.class("flex-1")
						.placeholder("Vault passphrase")
						.accessibility_label("Vault passphrase")
						.test_id("passphrase")
						.on_change(move |text: &str| passphrase.set(text.to_owned())),
				)
				.child(Button("Unlock").test_id("unlock").on_press(move || {
					let typed = submit.passphrase.get();
					if typed.is_empty() {
						return;
					}
					submit.passphrase.set(String::new());
					submit.send(Command::Unlock { passphrase: typed });
				})),
		)
}

fn lock_bar(state: &State) -> Element {
	let snapshot = state.snapshot;
	let state = state.clone();
	View()
		.class("flex-row items-center justify-between gap-2")
		.child(
			Text(move || {
				snapshot
					.get()
					.owner
					.map(|o| format!("Unlocked until {}", o.until))
					.unwrap_or_default()
			})
			.class(MUTED),
		)
		.child(
			Button("Lock")
				.test_id("lock")
				.on_press(move || state.send(Command::Lock)),
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

	fn harness(snapshot: Snapshot) -> (WidgetTester, State, Arc<Mutex<Vec<Command>>>) {
		let sent = Arc::new(Mutex::new(Vec::new()));
		let log = Arc::clone(&sent);
		let state = State::new(Arc::new(move |c| log.lock().unwrap().push(c)));
		state.snapshot.set(snapshot);
		let view = state.clone();
		let tester = WidgetTester::new(move || screen(&view), 520.0, 900.0).unwrap();
		(tester, state, sent)
	}

	fn ready() -> Snapshot {
		Snapshot {
			version: 3,
			headline: "Recording · 2 of 3 sources".into(),
			link: Link::Ready,
			daemon: Some("0.1.0".into()),
			sources: vec![
				SourceRow {
					name: "apps".into(),
					running: true,
					detail: None,
					settings: None,
				},
				SourceRow {
					name: "screen".into(),
					running: false,
					detail: Some("screen recording permission not granted".into()),
					settings: Some(quant_model::Settings::ScreenRecording),
				},
			],
			moments: vec![Moment {
				id: "m1".into(),
				title: "Budget call".into(),
				when: "Today 14:03".into(),
				duration: "15 min".into(),
				snippet: Some("we agreed on Tuesday".into()),
				playable: true,
			}],
			notice: Some(Notice {
				text: "Nothing recorded in that window".into(),
				error: true,
			}),
			opens_at_login: Some(false),
			..Snapshot::default()
		}
	}

	fn unlocked() -> Snapshot {
		Snapshot {
			owner: Some(OwnerView {
				until: "15:42".into(),
				pending: vec![PendingClient {
					id: "c1".into(),
					name: "Claude Code".into(),
					kind: "agent".into(),
					code: "K7QD-M2XA".into(),
					requested: vec!["health:summary".into(), "screen:read".into()],
					asked: "Today 14:00".into(),
				}],
				grants: vec![GrantRow {
					id: "g1".into(),
					label: "Phone".into(),
					scopes: vec!["motion:write".into()],
					lookback: "No limit".into(),
					expires: None,
				}],
				audit: vec![AuditRow {
					seq: 7,
					at: "Today 14:01".into(),
					actor: "owner".into(),
					action: "grant.create".into(),
					target: "c1".into(),
					outcome: "allowed".into(),
				}],
				chain_ok: Some(true),
				devices: vec![OwnerDeviceRow {
					id: "owd-1".into(),
					label: "MacBook Touch ID".into(),
					fingerprint: "a1b2c3d4e5f6a7b8".into(),
					last_used: Some("Today 13:30".into()),
				}],
				passphrase_session: true,
			}),
			..ready()
		}
	}

	#[test]
	fn status_shows_sources_and_offers_settings() {
		let (mut tester, _, sent) = harness(ready());
		tester.get_by_text("Recording · 2 of 3 sources").unwrap();
		tester
			.get_by_text("Nothing recorded in that window")
			.unwrap();
		tester.get_by_text("quantd 0.1.0").unwrap();
		tester.tap(&Finder::test_id("settings-screen")).unwrap();
		assert_eq!(
			sent.lock().unwrap().as_slice(),
			[Command::OpenSettings {
				pane: quant_model::Settings::ScreenRecording
			}]
		);
	}

	#[test]
	fn the_passphrase_is_sent_once_and_cleared() {
		let (mut tester, state, sent) = harness(ready());
		tester
			.enter_text(&Finder::test_id("passphrase"), "correct horse")
			.unwrap();
		tester.tap(&Finder::test_id("unlock")).unwrap();
		tester.tap(&Finder::test_id("unlock")).unwrap();
		assert_eq!(
			sent.lock().unwrap().as_slice(),
			[Command::Unlock {
				passphrase: "correct horse".into()
			}]
		);
		assert_eq!(state.passphrase.get(), "");
	}

	#[test]
	fn moments_save_play_and_rename() {
		let (mut tester, _, sent) = harness(ready());
		tester.tap(&Finder::test_id("tab-Saved moments")).unwrap();
		tester.get_by_text("we agreed on Tuesday").unwrap();
		tester.tap(&Finder::test_id("save-15")).unwrap();
		tester.tap(&Finder::test_id("play-m1")).unwrap();
		tester.tap(&Finder::test_id("rename-m1")).unwrap();
		tester
			.enter_text(&Finder::test_id("rename-title"), "Budget agreed")
			.unwrap();
		tester.tap(&Finder::test_id("rename-save")).unwrap();
		assert_eq!(
			sent.lock().unwrap().as_slice(),
			[
				Command::Save { minutes: 15 },
				Command::Play { id: "m1".into() },
				Command::Rename {
					id: "m1".into(),
					title: "Budget agreed".into()
				},
			]
		);
	}

	#[test]
	fn agents_approve_with_the_ticked_scopes() {
		let (mut tester, _, sent) = harness(unlocked());
		tester.tap(&Finder::test_id("tab-Agents")).unwrap();
		tester.get_by_text("Claude Code (agent)").unwrap();
		tester
			.toggle(&Finder::test_id("scope-screen:read"), false)
			.unwrap();
		tester.tap(&Finder::text("30 days")).unwrap();
		tester.tap(&Finder::test_id("approve-K7QD-M2XA")).unwrap();
		tester.tap(&Finder::test_id("revoke-Phone")).unwrap();
		assert_eq!(
			sent.lock().unwrap().as_slice(),
			[
				Command::Approve {
					client: "c1".into(),
					scopes: vec!["health:summary".into()],
					lookback_hours: Some(720),
				},
				Command::Revoke { grant: "g1".into() },
			]
		);
	}

	#[test]
	fn locked_sections_ask_for_the_passphrase() {
		let (mut tester, ..) = harness(ready());
		tester.tap(&Finder::test_id("tab-Agents")).unwrap();
		tester
			.get_by_text("Unlock to see and approve agents.")
			.unwrap();
		tester.tap(&Finder::test_id("tab-Audit")).unwrap();
		tester.get_by_text("Unlock to read the audit log.").unwrap();
	}

	#[test]
	fn audit_shows_the_chain_and_entries() {
		let (mut tester, ..) = harness(unlocked());
		tester.tap(&Finder::test_id("tab-Audit")).unwrap();
		tester
			.get_by_text("✓ The audit log's hash chain verifies")
			.unwrap();
		tester.get_by_text("grant.create c1").unwrap();
		tester.get_by_text("Unlocked until 15:42").unwrap();
	}

	#[test]
	fn touch_id_unlock_appears_once_set_up() {
		let mut snapshot = ready();
		snapshot.touch_id.device_id = Some("owd-1".into());
		let (tester, ..) = harness(snapshot);
		tester.get_by_test_id("unlock-touch-id").unwrap();
	}

	#[test]
	fn devices_offer_set_up_and_list_devices() {
		let (mut tester, state, ..) = harness(unlocked());
		state.touch_id.set(TouchIdAvailability {
			available: true,
			reason: None,
		});
		tester.tap(&Finder::test_id("tab-Devices")).unwrap();
		tester.get_by_text("Set up Touch ID").unwrap();
		tester.get_by_text("MacBook Touch ID").unwrap();
		tester
			.get_by_text("fingerprint a1b2c3d4e5f6a7b8 · last used Today 13:30")
			.unwrap();
	}

	#[test]
	fn touch_id_turn_off_and_revoke_send_commands() {
		let mut snapshot = unlocked();
		snapshot.touch_id.device_id = Some("owd-1".into());
		let (mut tester, state, sent) = harness(snapshot);
		state.touch_id.set(TouchIdAvailability {
			available: true,
			reason: None,
		});
		tester.tap(&Finder::test_id("tab-Devices")).unwrap();
		tester.tap(&Finder::test_id("touch-id-off")).unwrap();
		tester.tap(&Finder::test_id("revoke-device-owd-1")).unwrap();
		assert_eq!(
			sent.lock().unwrap().as_slice(),
			[
				Command::TouchIdOff,
				Command::RevokeDevice {
					device: "owd-1".into()
				},
			]
		);
	}

	#[test]
	fn touch_id_without_hardware_shows_why() {
		let (mut tester, state, ..) = harness(unlocked());
		state.touch_id.set(TouchIdAvailability {
			available: false,
			reason: Some("This Mac has no Touch ID sensor.".into()),
		});
		tester.tap(&Finder::test_id("tab-Devices")).unwrap();
		tester
			.get_by_text("This Mac has no Touch ID sensor.")
			.unwrap();
	}
}
