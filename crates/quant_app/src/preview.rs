//! Synthetic, disconnected data for reviewing the real native interface.

use std::sync::Arc;

use leptoskit::prelude::RwSignal;
use leptoskit::prelude::Update;
use quant_model::AuditRow;
use quant_model::Command;
use quant_model::GrantRow;
use quant_model::Link;
use quant_model::Moment;
use quant_model::Notice;
use quant_model::OwnerView;
use quant_model::PendingClient;
use quant_model::Snapshot;
use quant_model::SourceRow;

use crate::ui::State;

pub(crate) fn state() -> State {
	let data = snapshot();
	let owner = data.owner.clone();
	let signal = RwSignal::new(data);
	let mut state = State::new(Arc::new(move |command| {
		signal.update(|data| {
			match command {
				Command::Pause { .. } => {
					data.paused = true;
					data.headline = "Capture paused · take your time".into();
				}
				Command::Resume => {
					data.paused = false;
					data.headline = "Recording quietly · 4 sources active".into();
				}
				Command::Lock => data.owner = None,
				Command::Unlock { .. } => data.owner.clone_from(&owner),
				Command::OpenAtLogin { enabled } => data.opens_at_login = Some(enabled),
				_ => {}
			}
			data.version += 1;
			data.notice = Some(Notice {
				text: "Preview only · no vault data or settings were changed.".into(),
				error: false,
			});
		});
	}));
	state.snapshot = signal;
	state
}

fn snapshot() -> Snapshot {
	Snapshot {
		version: 1,
		headline: "Recording quietly · 4 sources active".into(),
		link: Link::Ready,
		daemon: Some("0.1.0".into()),
		opens_at_login: Some(false),
		sources: [
			("Apps", "Your working context, remembered"),
			("Screen", "Text captured when the screen changes"),
			("Audio", "A rolling buffer, ready when you need it"),
			("Activity", "Active time and idle periods"),
		]
		.into_iter()
		.map(|(name, detail)| {
			SourceRow {
				name: name.into(),
				running: true,
				detail: Some(detail.into()),
				settings: None,
			}
		})
		.collect(),
		moments: [
			(
				"m1",
				"A good idea, before it slips away",
				"Today, 10:42",
				"5 min",
				"What if the best memory is the one you don't have to think about keeping?",
			),
			(
				"m2",
				"The Friday plan",
				"Today, 09:15",
				"15 min",
				"Start with the smallest version. Keep the useful part, and leave room to change \
				 your mind.",
			),
			(
				"m3",
				"A walk and a thought",
				"Yesterday, 16:30",
				"5 min",
				"A little distance makes the next step easier to see.",
			),
		]
		.into_iter()
		.map(|(id, title, when, duration, snippet)| {
			Moment {
				id: id.into(),
				title: title.into(),
				when: when.into(),
				duration: duration.into(),
				snippet: Some(snippet.into()),
				playable: false,
			}
		})
		.collect(),
		owner: Some(OwnerView {
			until: "14:45".into(),
			pending: vec![PendingClient {
				id: "sample-agent".into(),
				name: "Claude Code".into(),
				kind: "agent".into(),
				code: "DEMO-ONLY".into(),
				requested: vec!["health:summary".into(), "apps:summary".into()],
				asked: "Just now".into(),
			}],
			grants: vec![GrantRow {
				id: "sample-grant".into(),
				label: "My research assistant".into(),
				scopes: vec!["memory.profile:summary".into()],
				lookback: "7 days".into(),
				expires: Some("Tomorrow".into()),
			}],
			audit: vec![
				AuditRow {
					seq: 24,
					at: "10:42".into(),
					actor: "You".into(),
					action: "moment.save".into(),
					target: "Last 5 minutes".into(),
					outcome: "allowed".into(),
				},
				AuditRow {
					seq: 23,
					at: "09:30".into(),
					actor: "My research assistant".into(),
					action: "memory.profile".into(),
					target: "Summary only".into(),
					outcome: "allowed".into(),
				},
			],
			chain_ok: Some(true),
			devices: vec![],
			passphrase_session: true,
		}),
		..Snapshot::default()
	}
}
