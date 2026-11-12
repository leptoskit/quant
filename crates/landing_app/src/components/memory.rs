//! "Memory you can correct": claims about you, ranked by authority, with
//! the correction you can make yourself on the page.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::ButtonVariant;
use skribble_leptoskit::FillStyle;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::WiredBadge;
use skribble_leptoskit::WiredButton;
use skribble_leptoskit::WiredCard;
use skribble_leptoskit::WiredHighlight;
use skribble_leptoskit::WiredInput;

use crate::components::Para;
use crate::components::Section;
use crate::store::Store;
use crate::theme;

/// The proposal the section lets the visitor approve or correct: a claim an
/// agent made, held until the owner decides.
const HELD_CLAIM: &str = "Prefers tea when travelling";

/// The authority ladder, top first.
const LADDER: [(&str, &str); 4] = [
	("Owner", "what you state, and your corrections"),
	("Owner-authored", "what you write yourself"),
	("Observed", "what Quant derives, grounded in your records"),
	("Inferred", "what an agent proposes, held until you approve"),
];

/// The memory section: a mock profile of claims with their authority, and
/// one held proposal the visitor can approve or correct for real.
#[component]
pub fn Memory() -> Element {
	view! {
		<Section
			id="memory"
			number="03"
			kicker="what it believes about you"
			heading="Memory you can correct."
			seed=61
		>
			<View id="memory-intro">
				<Para text="Quant also holds claims about you: preferences, routines, values, goals. Each one points at the records it came from, and each has an authority. Your corrections outrank what you wrote, which outranks what Quant observed, which outranks what an agent proposed." />
				<Para text="Agents can learn who you are, but they can only propose. A proposal is held, seen by no one but you, until you approve it. The world never reads any of it unless a grant you approved allows it." />
			</View>
			<View id="memory-cards">
				<Profile />
				<Ladder />
			</View>
		</Section>
	}
}

/// A small badge naming a claim's authority.
fn authority(label: &'static str, seed: i64, solid: bool) -> Element {
	let ink = theme::ink();

	view! {
		<WiredBadge
			seed=seed
			theme=ink.skribble
			fill_style=if solid {
				FillStyle::Solid
			} else {
				FillStyle::None
			}
		>
			<Text>{label}</Text>
		</WiredBadge>
	}
}

/// The mock profile: the owner's own word above an observed claim, and one
/// held proposal the visitor can approve or correct.
#[component]
fn Profile() -> Element {
	let ink = theme::ink();

	view! {
		<View id="profile" role=Role::Group aria_label="An example profile of claims">
			<WiredCard seed=62 theme=ink.skribble>
				<View id="profile-head">
					<Heading aria_level=3>"Profile"</Heading>
					<Text id="profile-sub">"claims about you · an example"</Text>
				</View>
				<View id="claim-owner" role=Role::Group aria_label="Owner claim">
					<View id="claim-owner-badge">{authority("owner", 63, true)}</View>
					<Text id="claim-owner-text">"Espresso, but never after 16:00"</Text>
					<Text id="claim-owner-why">
						"your correction. It replaced an observed claim."
					</Text>
				</View>
				<View id="claim-observed" role=Role::Group aria_label="Observed claim">
					<View id="claim-observed-badge">{authority("observed", 66, false)}</View>
					<Text id="claim-observed-text">"Focuses best between 9 and 12"</Text>
					<Text id="claim-observed-why">
						"grounded, word for word, in the records behind it"
					</Text>
				</View>
				<HeldClaim />
			</WiredCard>
		</View>
	}
}

/// The held proposal, which the visitor can approve or correct on the page.
#[component]
fn HeldClaim() -> Element {
	let ink = theme::ink();
	let store = Store::current();
	let approve = store.clone();
	let correct = store.clone();
	let typing = store.clone();
	let approved = store.clone();
	let corrected = store.clone();
	let struck = store.clone();
	let retired = store.clone();
	let actions = store.clone();
	let say = store.clone();
	let owner_line = store.clone();

	view! {
		<View id="claim-held" role=Role::Group aria_label="Held proposal">
			<View id="claim-held-badge">
				{authority("inferred", 68, false)}
				<Text id="claim-held-state">
					{move || {
						if approved.claim_approved() {
							"approved. It stays marked as inferred."
						} else if corrected.claim_correction().is_some() {
							"retired. Your word now outranks it."
						} else {
							"proposed by an agent, held for you"
						}
					}}
				</Text>
			</View>
			{Show(move || owner_line.claim_correction().is_some(), {
				let store = store.clone();
				move || {
					let store = store.clone();
					view! {
						<View id="claim-fixed" role=Role::Group aria_label="Your correction">
							<View id="claim-fixed-badge">{authority("owner", 70, true)}</View>
							<Text id="claim-fixed-text">
								{move || store.claim_correction().unwrap_or_default()}
							</Text>
						</View>
					}
				}
			})}
			{Show(
				move || struck.claim_correction().is_none(),
				|| view! { <Text id="claim-held-text">{HELD_CLAIM}</Text> },
			)}
			{Show(
				move || retired.claim_correction().is_some(),
				move || {
					view! {
						<View id="claim-held-struck">
							<WiredHighlight
								kind=HighlightKind::Strikethrough
								seed=69
								theme=ink.skribble
								color=ink.coral
							>
								<Text id="claim-held-text-struck">{HELD_CLAIM}</Text>
							</WiredHighlight>
						</View>
					}
				},
			)}
			{Show(
				move || !actions.claim_approved() && actions.claim_correction().is_none(),
				move || {
					let approve = approve.clone();
					let correct = correct.clone();
					let typing = typing.clone();
					let draft = say.claim_draft();
					view! {
						<View id="claim-actions">
							<Text id="claim-hint">"try it: say what is actually true"</Text>
							<View id="claim-field">
								<WiredInput
									label="Your correction to the held claim"
									value=draft
									placeholder="Prefers tea only on trains"
									seed=71
									theme=ink.skribble
									on_change=move |text: &str| {
										super::focus::with_input_focus("claim-field", || {
											typing.set_claim_draft(text.to_owned());
										});
									}
								/>
							</View>
							<View id="claim-buttons">
								<WiredButton
									label="Correct this"
									seed=72
									theme=ink.skribble
									on_press=move || correct.correct_claim()
								/>
								<WiredButton
									label="Approve"
									variant=ButtonVariant::Secondary
									seed=73
									theme=ink.skribble
									on_press=move || approve.approve_claim()
								/>
							</View>
						</View>
					}
				},
			)}
		</View>
	}
}

/// The authority ladder, one rung at a time.
#[component]
fn Ladder() -> Element {
	let ink = theme::ink();

	view! {
		<View id="ladder" role=Role::Group aria_label="The authority ladder">
			<Heading aria_level=3>"The authority ladder"</Heading>
			<View id="ladder-rungs" role=Role::List>
				{LADDER
					.iter()
					.enumerate()
					.map(|(index, (name, meaning))| {
						view! {
							<View id=format!("rung-{index}") role=Role::ListItem>
								<WiredHighlight
									kind=HighlightKind::Underline
									seed=80 + i64::try_from(index).expect("small fixed list")
									theme=ink.skribble
									color=ink.violet
								>
									<Text>{*name}</Text>
								</WiredHighlight>
								<Text>{*meaning}</Text>
							</View>
						}
					})
					.collect::<Vec<_>>()}
			</View>
			<Para text="Purging the records behind a claim takes the observed and inferred ones with it. Claims you made yourself are never deleted by a purge." />
		</View>
	}
}
