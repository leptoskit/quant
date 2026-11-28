//! Shared opt-in companion access & capability management UI for the Vault
//! and Ask segments.
//!
//! Uses the actual companion grant state reported by `quant_phone`. Never asks
//! for an owner passphrase or owner token, and never blocks or alters
//! write-only collection.

use leptoskit::native::reactive::For;
use leptoskit::prelude::ActivityIndicator;
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::GetUntracked;
use leptoskit::prelude::Set;
use leptoskit::prelude::Switch;
use leptoskit::prelude::Text;
use leptoskit::prelude::View;
use quant_model::phone::Command;
use quant_model::phone::CompanionCapability;
use quant_model::phone::CompanionChange;
use quant_model::phone::StatsAccess;
use quant_model::phone::StatsState;

use super::State;
use crate::theme;
use crate::theme::Icon;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AccessPart {
	focus: CompanionCapability,
	this_state: StatsState,
	other_state: StatsState,
	change: Option<CompanionChange>,
	cap_stats: bool,
	cap_ask: bool,
	manage: bool,
}

/// Companion access request, approval, pending capability update, restricted,
/// revoked, and offline cards for `focus` (`StatsSummary` or `AskAgents`).
pub(crate) fn access_card(state: &State, focus: CompanionCapability) -> Element {
	let snapshot = state.snapshot;
	let cap_stats = state.cap_stats;
	let cap_ask = state.cap_ask;
	let manage = state.manage_access;
	let state = state.clone();
	For(
		move || {
			let s = snapshot.get();
			let (this_state, other_state) = match focus {
				CompanionCapability::StatsSummary => (s.stats, s.ask),
				CompanionCapability::AskAgents => (s.ask, s.stats),
			};
			vec![AccessPart {
				focus,
				this_state,
				other_state,
				change: s.companion_change,
				cap_stats: cap_stats.get(),
				cap_ask: cap_ask.get(),
				manage: manage.get(),
			}]
		},
		Clone::clone,
		move |part| render_access(&state, &part),
	)
}

fn render_access(state: &State, part: &AccessPart) -> Element {
	// When the focused capability is already Ready, only render this card if
	// the owner opened Manage Access or has a pending capability change.
	if part.this_state.access == StatsAccess::Ready && !part.manage && part.change.is_none() {
		return View();
	}

	if let Some(change) = &part.change {
		return pending_change_card(state, change, part);
	}

	match &part.this_state.access {
		StatsAccess::Unpaired => {
			View()
				.class(theme::CARD)
				.child(Text("Connect to your vault first").class(theme::HEADING))
				.child(
					Text("Pair this phone with your vault before choosing what it can use.")
						.class(theme::MUTED),
				)
		}
		StatsAccess::Checking => {
			let refresh = state.clone();
			View()
				.class(theme::CARD)
				.child(
					View()
						.class("flex-row items-center gap-3")
						.child(ActivityIndicator(true))
						.child(
							Text("Checking access…")
								.class(theme::HEADING)
								.test_id(format!("companion-checking-{}", cap_id(part.focus))),
						),
				)
				.child(Text("Confirming what this phone can use.").class(theme::MUTED))
				.child(
					Button("Check now")
						.class(theme::SECONDARY)
						.test_id("companion-check")
						.on_press(move || refresh.send(Command::RefreshCompanion)),
				)
		}
		StatsAccess::Pending { code } => {
			pending_card(state, code, part.this_state.approve.as_deref(), part.focus)
		}
		StatsAccess::NotRequested => {
			// If the OTHER capability is already pending approval on the same
			// companion identity, show that pending request and allow adjusting
			// selected capabilities before approval.
			if let StatsAccess::Pending { code } = &part.other_state.access {
				return pending_other_card(state, code, part.other_state.approve.as_deref(), part);
			}
			request_card(state, part, false)
		}
		StatsAccess::Restricted => restricted_card(state, part),
		StatsAccess::Revoked => revoked_card(state, part),
		StatsAccess::Unreachable { reason } => {
			let retry = state.clone();
			let focus = part.focus;
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
									Text("Vault out of reach")
										.class(theme::HEADING)
										.test_id(format!("companion-offline-{}", cap_id(focus))),
								)
								.child(Text(reason.clone()).class(theme::MUTED)),
						),
				)
				.child(
					Text(
						"Recording continues on this phone. Stats and Ask need your vault to \
						 answer.",
					)
					.class(theme::CAPTION),
				)
				.child(
					Button("Try again")
						.class(theme::PRIMARY)
						.test_id("companion-retry")
						.on_press(move || {
							match focus {
								CompanionCapability::StatsSummary => retry.reload_vault(),
								CompanionCapability::AskAgents => retry.reload_ask(),
							}
						}),
				)
		}
		StatsAccess::Ready => request_card(state, part, true),
	}
}

fn capability_toggles(state: &State, part: &AccessPart) -> Element {
	let stats_sig = state.cap_stats;
	let ask_sig = state.cap_ask;
	View()
		.class("gap-3")
		.child(
			View().class(theme::ROW).child(
				View()
					.class("flex-row items-center justify-between gap-3")
					.child(
						View()
							.class("flex-1 gap-1")
							.child(Text("View stats").class(theme::LABEL))
							.child(
								Text(
									"Counts, averages, and durations. Never raw records, text, or \
									 media.",
								)
								.class(theme::CAPTION),
							),
					)
					.child(
						Switch(part.cap_stats)
							.accessibility_label("Select stats access")
							.test_id("cap-stats")
							.on_toggle(move |on| stats_sig.set(on)),
					),
			),
		)
		.child(
			View().class(theme::ROW).child(
				View()
					.class("flex-row items-center justify-between gap-3")
					.child(
						View()
							.class("flex-1 gap-1")
							.child(Text("Ask agents").class(theme::LABEL))
							.child(
								Text(
									"Send questions to your approved agents. Their replies appear \
									 here.",
								)
								.class(theme::CAPTION),
							),
					)
					.child(
						Switch(part.cap_ask)
							.accessibility_label("Select Ask agents")
							.test_id("cap-ask")
							.on_toggle(move |on| ask_sig.set(on)),
					),
			),
		)
}

fn selected_capabilities(state: &State) -> Vec<CompanionCapability> {
	let mut caps = Vec::new();
	if state.cap_stats.get_untracked() {
		caps.push(CompanionCapability::StatsSummary);
	}
	if state.cap_ask.get_untracked() {
		caps.push(CompanionCapability::AskAgents);
	}
	caps
}

fn request_card(state: &State, part: &AccessPart, managing: bool) -> Element {
	let request = state.clone();
	let close = state.clone();
	let forget = state.clone();
	let title = if managing {
		"Change access"
	} else {
		match part.focus {
			CompanionCapability::StatsSummary => "View stats on this phone",
			CompanionCapability::AskAgents => "Ask your approved agents",
		}
	};
	let intro = if managing {
		"Choose what this phone can use. Changes wait for your approval on your computer."
	} else {
		"Recording stays write-only and keeps working. Choose what you want, then request access."
	};
	let button_label = if managing {
		"Request change"
	} else {
		"Request access"
	};
	View()
		.class(theme::CARD)
		.child(
			Text(title)
				.class(theme::HEADING)
				.test_id(format!("companion-request-title-{}", cap_id(part.focus))),
		)
		.child(Text(intro).class(theme::MUTED))
		.child(capability_toggles(state, part))
		.child(
			View()
				.class("gap-2 pt-2")
				.child(
					Button(button_label)
						.class(theme::PRIMARY)
						.test_id("companion-request")
						.on_press(move || {
							let capabilities = selected_capabilities(&request);
							if capabilities.is_empty() {
								request.problem.set(Some(
									"Select at least View stats or Ask agents first.".to_owned(),
								));
							} else {
								request.send(Command::RequestCompanion { capabilities });
							}
						}),
				)
				.children(managing.then(|| {
					Button("Remove access")
						.class(theme::SECONDARY)
						.test_id("companion-forget")
						.on_press(move || {
							forget.manage_access.set(false);
							forget.send(Command::ForgetCompanion);
						})
				}))
				.children(managing.then(|| {
					Button("Done")
						.class(theme::QUIET)
						.test_id("companion-manage-close")
						.on_press(move || close.manage_access.set(false))
				})),
		)
}

fn pending_card(
	state: &State,
	code: &str,
	approve: Option<&str>,
	focus: CompanionCapability,
) -> Element {
	let check = state.clone();
	let forget = state.clone();
	let detail = if approve.is_some() {
		"In desktop Quant, open Agents → Waiting for approval and approve this code. Recording \
		 keeps working. Or run this in a terminal on your computer:"
	} else {
		"In desktop Quant, open Agents → Waiting for approval and approve this code. Recording \
		 keeps working."
	};
	View()
		.class(theme::CARD)
		.child(
			Text("Waiting for approval")
				.class(theme::HEADING)
				.test_id(format!("companion-pending-{}", cap_id(focus))),
		)
		.child(
			View()
				.class(format!("{} items-center", theme::HERO))
				.child(Text("CODE").class(theme::CAPTION))
				.child(
					Text(code.to_owned())
						.class("text-3xl font-bold text-[#F4F1FF]")
						.test_id("companion-code"),
				),
		)
		.child(Text(detail).class(theme::MUTED))
		.children(approve.map(|command| {
			Text(command.to_owned())
				.class(theme::CODE)
				.test_id("companion-approve")
		}))
		.child(
			View()
				.class("gap-2")
				.child(
					Button("I approved it")
						.class(theme::PRIMARY)
						.test_id("companion-check")
						.on_press(move || {
							check.send(Command::RefreshCompanion);
						}),
				)
				.child(
					Button("Cancel request")
						.class(theme::QUIET)
						.test_id("companion-forget")
						.on_press(move || forget.send(Command::ForgetCompanion)),
				),
		)
}

fn pending_other_card(
	state: &State,
	code: &str,
	approve: Option<&str>,
	part: &AccessPart,
) -> Element {
	let update = state.clone();
	let check = state.clone();
	View()
		.class(theme::CARD)
		.child(Text("This phone is waiting for approval").class(theme::HEADING))
		.child(
			Text(format!(
				"Code {code} is already waiting. Choose what you want below, then approve the \
				 updated code in desktop Quant under Agents → Waiting for approval."
			))
			.class(theme::MUTED),
		)
		.child(capability_toggles(state, part))
		.children(approve.map(|command| {
			Text(command.to_owned())
				.class(theme::CODE)
				.test_id("companion-approve")
		}))
		.child(
			View()
				.class("gap-2")
				.child(
					Button("Update request")
						.class(theme::PRIMARY)
						.test_id("companion-request")
						.on_press(move || {
							let capabilities = selected_capabilities(&update);
							if capabilities.is_empty() {
								update.problem.set(Some(
									"Select at least View stats or Ask agents first.".to_owned(),
								));
							} else {
								update.send(Command::RequestCompanion { capabilities });
							}
						}),
				)
				.child(
					Button("I approved it")
						.class(theme::SECONDARY)
						.test_id("companion-check")
						.on_press(move || check.send(Command::RefreshCompanion)),
				),
		)
}

fn pending_change_card(state: &State, change: &CompanionChange, part: &AccessPart) -> Element {
	let check = state.clone();
	let cancel = state.clone();
	let focus = part.focus;
	let labels = change
		.capabilities
		.iter()
		.map(|cap| cap.label())
		.collect::<Vec<_>>()
		.join(" + ");
	View()
		.class(theme::CARD)
		.child(
			Text(format!("Approve {labels} on your computer"))
				.class(theme::HEADING)
				.test_id("companion-change"),
		)
		.child(
			Text(format!(
				"Your current access stays on until you approve this change for code {}. Run this \
				 in a terminal on your computer, then tap I approved it:",
				change.code
			))
			.class(theme::MUTED),
		)
		.child(
			Text(change.approve.clone())
				.class(theme::CODE)
				.test_id("companion-approve"),
		)
		.child(
			View()
				.class("gap-2")
				.child(
					Button("I approved it")
						.class(theme::PRIMARY)
						.test_id("companion-check")
						.on_press(move || {
							check.send(Command::RefreshCompanion);
							match focus {
								CompanionCapability::StatsSummary => check.reload_vault(),
								CompanionCapability::AskAgents => check.reload_ask(),
							}
						}),
				)
				.child(
					Button("Keep current access")
						.class(theme::QUIET)
						.test_id("companion-cancel-change")
						.on_press(move || {
							cancel.manage_access.set(false);
							cancel.send(Command::CancelCompanionChange);
						}),
				),
		)
}

fn restricted_card(state: &State, part: &AccessPart) -> Element {
	let request = state.clone();
	let forget = state.clone();
	let (heading, detail) = match part.focus {
		CompanionCapability::StatsSummary if part.other_state.access == StatsAccess::Ready => {
			(
				"Stats aren't on for this phone",
				"This phone can ask agents but not view stats. Request a change below; your \
				 current access stays on until your computer approves it.",
			)
		}
		CompanionCapability::AskAgents if part.other_state.access == StatsAccess::Ready => {
			(
				"Ask isn't on for this phone",
				"This phone can view stats but not ask agents. Request a change below; your \
				 current access stays on until your computer approves it.",
			)
		}
		CompanionCapability::StatsSummary => {
			(
				"No streams are shared with this phone",
				"Access is approved, but your vault shares no streams. Change what is shared on \
				 your computer, or request a change below.",
			)
		}
		CompanionCapability::AskAgents => {
			(
				"Ask isn't on for this phone",
				"Access is approved, but Ask is not included. Request a change below; your \
				 computer approves the new access.",
			)
		}
	};
	View()
		.class(theme::CARD)
		.child(
			View()
				.class("flex-row items-center gap-3")
				.child(theme::tile_warn(Icon::Lock))
				.child(
					View()
						.class("flex-1 gap-1")
						.child(
							Text(heading)
								.class(theme::HEADING)
								.test_id(format!("companion-restricted-{}", cap_id(part.focus))),
						)
						.child(Text(detail).class(theme::MUTED)),
				),
		)
		.child(capability_toggles(state, part))
		.child(
			View()
				.class("gap-2 pt-2")
				.child(
					Button("Request change")
						.class(theme::PRIMARY)
						.test_id("companion-request")
						.on_press(move || {
							let capabilities = selected_capabilities(&request);
							if capabilities.is_empty() {
								request.problem.set(Some(
									"Select at least View stats or Ask agents first.".to_owned(),
								));
							} else {
								request.send(Command::RequestCompanion { capabilities });
							}
						}),
				)
				.child(
					Button("Remove access")
						.class(theme::QUIET)
						.test_id("companion-forget")
						.on_press(move || forget.send(Command::ForgetCompanion)),
				),
		)
}

fn revoked_card(state: &State, part: &AccessPart) -> Element {
	let request = state.clone();
	let reason = part.this_state.problem.clone().unwrap_or_else(|| {
		"This phone's access was denied, revoked, or expired. Recording keeps working.".to_owned()
	});
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
							Text("Access ended")
								.class(theme::HEADING)
								.test_id(format!("companion-revoked-{}", cap_id(part.focus))),
						)
						.child(Text(reason).class(theme::MUTED)),
				),
		)
		.child(
			Text(
				"Cached data was cleared, and this phone will not reconnect on its own. Choose \
				 what you want to request again below.",
			)
			.class(theme::CAPTION),
		)
		.child(capability_toggles(state, part))
		.child(
			Button("Request access again")
				.class(theme::PRIMARY)
				.test_id("companion-request")
				.on_press(move || {
					let capabilities = selected_capabilities(&request);
					if capabilities.is_empty() {
						request.problem.set(Some(
							"Select at least View stats or Ask agents first.".to_owned(),
						));
					} else {
						request.send(Command::RequestCompanion { capabilities });
					}
				}),
		)
}

fn cap_id(cap: CompanionCapability) -> &'static str {
	match cap {
		CompanionCapability::StatsSummary => "stats",
		CompanionCapability::AskAgents => "ask",
	}
}
