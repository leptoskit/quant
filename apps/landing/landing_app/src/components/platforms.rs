//! Where Quant runs: what is available now, and what is coming.

use leptoskit::native::Role;
use leptoskit::prelude::*;
use skribble_leptoskit::FillStyle;
use skribble_leptoskit::WiredBadge;

use crate::components::Para;
use crate::components::Section;
use crate::theme;

/// One place Quant runs, and whether it can be had today.
struct Platform {
	/// The platform's name.
	name: &'static str,
	/// What it does.
	body: &'static str,
	/// Whether it exists today, or is still coming.
	available: bool,
}

/// The platforms, in the order the page lists them. Only what is built is
/// marked available.
const PLATFORMS: [Platform; 4] = [
	Platform {
		name: "macOS menu bar",
		body: "A remote for the daemon: see what each source is doing, pause and resume, save the \
		       last minutes of audio, approve agents, read the audit log, and unlock with Touch \
		       ID. It can open at login.",
		available: true,
	},
	Platform {
		name: "CLI and MCP server",
		body: "quant on the command line, and quant mcp for Claude Code and other agents. The \
		       tools an agent sees follow its grant.",
		available: true,
	},
	Platform {
		name: "Android collector",
		body: "An always-on collector, built with leptoskit, that spools captures on the phone \
		       and sends them to your vault. The daemon already accepts signed, write-only \
		       uploads from devices; the app itself is not released yet.",
		available: false,
	},
	Platform {
		name: "iOS",
		body: "On the way, after Android.",
		available: false,
	},
];

/// The command that connects an agent, as the readme gives it.
const CONNECT: [&str; 3] = [
	"# connect Claude Code; the first run prints a pairing code",
	"claude mcp add quant -- quant mcp --profile claude --scope '*:summary'",
	"quant approve K7QD-M2XA --scope health:summary --scope apps:summary --lookback-hours 720",
];

/// The platforms section, with a code sample for connecting an agent.
#[component]
pub fn Platforms() -> Element {
	view! {
		<Section
			id="platforms"
			number="05"
			kicker="where it runs"
			heading="On your Mac today. Your phone next."
			seed=111
		>
			<View id="platform-list" role=Role::List>
				{PLATFORMS
					.iter()
					.enumerate()
					.map(|(index, platform)| platform_card(index, platform))
					.collect::<Vec<_>>()}
			</View>
			<View id="connect">
				<View id="connect-copy">
					<Heading aria_level=3>"Connect an agent"</Heading>
					<Para text="The agent starts with one tool, the one that tells it how to ask. You approve it with the scopes and lookback you choose; nothing is readable until you do." />
				</View>
				<View
					id="connect-code"
					role=Role::Figure
					aria_label="Commands that connect an agent over MCP"
				>
					<View role=Role::Code>
						{CONNECT
							.iter()
							.enumerate()
							.map(|(index, line)| {
								view! { <Text id=format!("code-line-{index}")>{*line}</Text> }
							})
							.collect::<Vec<_>>()}
					</View>
				</View>
			</View>
		</Section>
	}
}

/// One platform: its name, its status badge, and what it does.
fn platform_card(index: usize, platform: &Platform) -> Element {
	let ink = theme::ink();

	view! {
		<View id=format!("platform-{index}") role=Role::ListItem>
			<View id=format!("platform-{index}-head")>
				<Heading aria_level=3>{platform.name}</Heading>
				<WiredBadge
					seed=121 + i64::try_from(index).expect("small fixed list")
					theme=ink.skribble
					fill_style=if platform.available {
						FillStyle::Solid
					} else {
						FillStyle::None
					}
				>
					<Text>
						{if platform.available {
							"available"
						} else {
							"coming"
						}}
					</Text>
				</WiredBadge>
			</View>
			<View role=Role::Paragraph>
				<Text>{platform.body}</Text>
			</View>
		</View>
	}
}
