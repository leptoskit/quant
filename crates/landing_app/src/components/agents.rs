//! The permission model, made visible: a mock grant beside a mock audit log.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::FillStyle;
use skribble_leptoskit::HighlightKind;
use skribble_leptoskit::WiredBadge;
use skribble_leptoskit::WiredCard;
use skribble_leptoskit::WiredHighlight;

use crate::components::Para;
use crate::components::Section;
use crate::theme;

/// The scopes the mock grant allows, and what each one really permits.
const SCOPES: [(&str, &str); 3] = [
	(
		"health:summary",
		"average heart rate per hour, never a raw sample",
	),
	(
		"apps:summary",
		"how long each app had focus, never a window title",
	),
	(
		"memory.profile:summary",
		"your own words about you, nothing inferred",
	),
];

/// The sensitivity classes the mock grant denies outright.
const DENIED: [&str; 4] = ["location", "intimate", "others", "financial"];

/// One line of the mock audit log.
struct Entry {
	/// When it happened.
	time: &'static str,
	/// Who did what.
	action: &'static str,
	/// What came back.
	result: &'static str,
	/// The entry's hash, which covers the one before it.
	hash: &'static str,
	/// Whether the request was refused.
	refused: bool,
}

/// A plausible afternoon in the audit log. Every line illustrates the shape
/// the log records (who acted, what they asked for, how much was served),
/// never the data itself.
const ENTRIES: [Entry; 4] = [
	Entry {
		time: "14:02",
		action: "claude  read apps.focus",
		result: "24 buckets",
		hash: "4be1",
		refused: false,
	},
	Entry {
		time: "14:03",
		action: "claude  search “focus”",
		result: "12 records",
		hash: "e3a7",
		refused: false,
	},
	Entry {
		time: "14:05",
		action: "claude  read location.fix",
		result: "refused",
		hash: "91cc",
		refused: true,
	},
	Entry {
		time: "14:06",
		action: "you  approved a grant",
		result: "passphrase",
		hash: "08f2",
		refused: false,
	},
];

/// The section that shows the permission model at a glance: a grant card
/// beside the audit log it writes to.
#[component]
pub fn Agents() -> Element {
	view! {
		<Section
			id="permissions"
			number="02"
			kicker="the permission model"
			heading="What your AI may know about you, and what it may not."
			seed=51
		>
			<View id="permissions-intro">
				<Para text="A new agent sees one tool: the one that tells it how to ask you for access. What it gets back is a grant: named scopes, a lookback window, rate limits, redaction. Every request it makes, and every refusal, lands in a log you can review and check." />
				<Para text="A grant can also deny whole sensitivity classes. Anything denied is treated exactly like data that is out of scope. Approving a grant, purging data and reading the full audit log need your passphrase, so an agent with a shell on your machine cannot approve itself." />
			</View>
			<View id="permissions-cards">
				<GrantCard />
				<AuditLog />
			</View>
		</Section>
	}
}

/// The mock grant: the scopes an agent may read, the classes denied to it,
/// and the bounds it works within.
#[component]
fn GrantCard() -> Element {
	let ink = theme::ink();

	view! {
		<View id="grant" role=Role::Group aria_label="An example grant">
			<WiredCard seed=52 theme=ink.skribble>
				<View id="grant-head">
					<Heading aria_level=3>"Grant"</Heading>
					<Text id="grant-agent">"for your coding agent · an example"</Text>
				</View>
				<View id="grant-scopes" role=Role::List aria_label="Allowed scopes">
					{SCOPES
						.iter()
						.enumerate()
						.map(|(index, (scope, meaning))| {
							view! {
								<View id=format!("scope-{index}") role=Role::ListItem>
									<Text>{*scope}</Text>
									<Text>{*meaning}</Text>
								</View>
							}
						})
						.collect::<Vec<_>>()}
				</View>
				<View id="grant-denied">
					<Text id="grant-denied-label">"denied outright"</Text>
					<View id="grant-denied-classes" role=Role::List aria_label="Denied classes">
						{DENIED
							.iter()
							.enumerate()
							.map(|(index, class)| {
								view! {
									<View id=format!("denied-{index}") role=Role::ListItem>
										<WiredHighlight
											kind=HighlightKind::Strikethrough
											seed=54
												+ i64::try_from(index).expect("small fixed list")
											theme=ink.skribble
											color=ink.coral
										>
											<Text>{*class}</Text>
										</WiredHighlight>
									</View>
								}
							})
							.collect::<Vec<_>>()}
					</View>
				</View>
				<View id="grant-bounds">
					<Text>"lookback 30 days"</Text>
					<Text>"12 requests a minute"</Text>
					<Text>"expires in 7 days"</Text>
				</View>
				<View id="grant-seal" aria_hidden=true>
					<WiredBadge seed=53 theme=ink.skribble fill_style=FillStyle::None>
						<Text>"approved by you"</Text>
					</WiredBadge>
				</View>
			</WiredCard>
		</View>
	}
}

/// The mock audit log: who acted, what they asked for, how much was served,
/// and the hash chain checked by the audit command.
#[component]
fn AuditLog() -> Element {
	let ink = theme::ink();

	view! {
		<View id="audit" role=Role::Group aria_label="An example audit log">
			<WiredCard seed=57 theme=ink.skribble>
				<View id="audit-head">
					<Heading aria_level=3>"Audit log"</Heading>
					<Text id="audit-sub">"hash-chained · an example"</Text>
				</View>
				<View id="audit-entries" role=Role::List aria_label="Entries">
					{ENTRIES
						.iter()
						.enumerate()
						.map(|(index, entry)| {
							view! {
								<View id=format!("audit-{index}") role=Role::ListItem>
									<Text>{entry.time}</Text>
									<Text>{entry.action}</Text>
									<Text
										id=format!("audit-{index}-result")
										aria_label=if entry.refused {
											"refused"
										} else {
											entry.result
										}
									>
										{entry.result}
									</Text>
									<Text aria_hidden=true>{format!("#{}", entry.hash)}</Text>
								</View>
							}
						})
						.collect::<Vec<_>>()}
				</View>
				<View id="audit-note">
					<WiredBadge seed=59 theme=ink.skribble>
						<Text>"chain verifies"</Text>
					</WiredBadge>
					<Text>
						"Each entry carries the hash of the one before it. quant audit checks that those links match."
					</Text>
				</View>
			</WiredCard>
		</View>
	}
}
