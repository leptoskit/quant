//! Quant's sensor-first watch experience for Apple Watch (`watchOS`), Wear OS
//! (Pixel Watch, Samsung Galaxy Watch, etc.), and Huawei Watch (`HarmonyOS`).
//!
//! Watches spend almost all of their time with the display off. Rather than
//! shrinking the phone collector's cards onto a circular face, Quant declares
//! its biometric and motion streams once via [`leptoskit_watch::WearableApp`],
//! synthesizes budget-validated ambient surfaces (page, tile/glance, and face
//! complication) from [`quant_model::phone::Snapshot`], and renders a compact
//! high-contrast wrist screen ([`WatchScreen`]).

use leptoskit::native::reactive::For;
use leptoskit::native::reactive::Show;
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
use leptoskit::prelude::ScrollView;
use leptoskit::prelude::Text;
use leptoskit::prelude::View;
use leptoskit::prelude::component;
use leptoskit_watch::DeliveryMode;
use leptoskit_watch::SamplingPolicy;
use leptoskit_watch::WatchApp;
use leptoskit_watch::WatchError;
use leptoskit_watch::WatchKind;
use leptoskit_watch::WearableApp;
use leptoskit_watch::WearableStatus;
use leptoskit_watch::WearableStream;
use quant_model::phone::Command;
use quant_model::phone::Link;
use quant_model::phone::Snapshot;
use quant_model::phone::Storage;

use crate::phone::Action;
use crate::phone::Grant;
use crate::phone::State;
use crate::phone::Step;

/// All streams Quant collects on watches that expose them.
pub const QUANT_WATCH_STREAMS: [WearableStream; 22] = [
	WearableStream::HeartRate,
	WearableStream::RestingHeartRate,
	WearableStream::HeartRateVariability,
	WearableStream::OxygenSaturation,
	WearableStream::SkinTemperature,
	WearableStream::RespiratoryRate,
	WearableStream::Steps,
	WearableStream::FloorsClimbed,
	WearableStream::ActiveCalories,
	WearableStream::Distance,
	WearableStream::Sleep,
	WearableStream::Workout,
	WearableStream::WearState,
	WearableStream::BodyComposition,
	WearableStream::Stress,
	WearableStream::Accelerometer,
	WearableStream::Gyroscope,
	WearableStream::Barometer,
	WearableStream::Location,
	WearableStream::Battery,
	WearableStream::Thermal,
	WearableStream::BleSensors,
];

/// Watch OS family compiled into the active build target.
pub const fn active_watch_kind() -> WatchKind {
	if cfg!(feature = "watchos") {
		WatchKind::WatchOs
	} else if cfg!(feature = "harmonyos-watch") {
		WatchKind::HarmonyOs
	} else {
		WatchKind::WearOs
	}
}

/// Declares Quant's sensor-first [`WearableApp`] for `kind` (`WearOs`,
/// `WatchOs`, or `HarmonyOs`).
pub fn quant_wearable_app(kind: WatchKind) -> WearableApp {
	WearableApp::new("quant-watch", kind)
		.streams(&QUANT_WATCH_STREAMS)
		.policy(SamplingPolicy::Passive)
		.delivery(DeliveryMode::DirectWithCompanionFallback)
		.auto_status_surface()
}

/// Projects the native service's [`Snapshot`] into a compact [`WearableStatus`]
/// for watch tiles, glances, and complications.
pub fn status_from_snapshot(snapshot: &Snapshot) -> WearableStatus {
	let headline = if matches!(snapshot.storage, Storage::Failed { .. }) {
		"Storage error".to_owned()
	} else {
		match &snapshot.link {
			Link::Unpaired => "Pair via phone".to_owned(),
			Link::Pending { code } => format!("Approve {code}"),
			Link::Revoked => "Access revoked".to_owned(),
			Link::Connecting => "Connecting…".to_owned(),
			Link::Unreachable { .. } => "Offline spool".to_owned(),
			Link::Ready if snapshot.collecting => "Recording".to_owned(),
			Link::Ready => "Paused".to_owned(),
		}
	};
	let notice = if snapshot.watch_connected
		&& matches!(snapshot.link, Link::Unreachable { .. } | Link::Unpaired)
	{
		Some("Syncing via phone".to_owned())
	} else if let Some(problem) = snapshot.upload_problem.as_deref() {
		Some(problem.to_owned())
	} else if snapshot.watch_connected {
		Some(
			snapshot
				.watch_name
				.as_deref()
				.map_or_else(|| "Phone linked".to_owned(), |name| format!("Via {name}")),
		)
	} else {
		None
	};
	WearableStatus {
		headline,
		recording: snapshot.collecting && snapshot.link != Link::Unpaired,
		paired: snapshot.link == Link::Ready,
		heart_rate_bpm: snapshot.latest_heart_rate_bpm,
		steps_today: snapshot.steps_today,
		queued_readings: snapshot.waiting,
		battery_percent: snapshot.battery_percent,
		notice,
	}
}

/// Synthesizes budget-validated watch surfaces (page, tile/glance, and face
/// complication) for `kind` from `snapshot`.
pub fn build_watch_surfaces(kind: WatchKind, snapshot: &Snapshot) -> Result<WatchApp, WatchError> {
	let app = quant_wearable_app(kind);
	let status = status_from_snapshot(snapshot);
	app.render_status_app(&status)
}

/// Minimal high-contrast wrist screen for round and rectangular watches.
#[component]
pub fn WatchScreen(state: State) -> Element {
	let snapshot = state.snapshot;
	let permissions = state.permissions;
	let problem = state.problem;
	let collector = state.collector;
	let live_workout = state.live_workout;
	let toggle_state = state.clone();
	let sync_state = state.clone();
	let workout_state = state.clone();
	let sensor_state = state.clone();
	let reset_state = state.clone();

	let alert = move || {
		let s = snapshot.get();
		problem
			.get()
			.or_else(|| collector.get())
			.or_else(|| status_from_snapshot(&s).notice)
			.or_else(|| s.notice.map(|n| n.text))
			.filter(|text| !text.is_empty())
	};

	ScrollView().class("flex-1 bg-black").child(
		View()
			.class("items-center justify-center gap-2 px-6 py-7")
			.child(
				Text("QUANT")
					.class("text-[10px] font-semibold text-amber-300")
					.test_id("watch-title"),
			)
			.child(
				Text(move || {
					let s = snapshot.get();
					// Validate the synthesized watch surfaces whenever
					// snapshot updates so tiles/complications stay within
					// platform budgets.
					let _ = build_watch_surfaces(active_watch_kind(), &s);
					status_from_snapshot(&s).headline
				})
				.class("text-base font-semibold text-white")
				.test_id("watch-headline"),
			)
			.child(
				Text(move || {
					let s = snapshot.get();
					match (s.latest_heart_rate_bpm, s.steps_today) {
						(Some(bpm), Some(steps)) => format!("{bpm} bpm · {steps} steps"),
						(Some(bpm), None) => format!("{bpm} bpm"),
						(None, Some(steps)) => format!("{steps} steps"),
						(None, None) => "Waiting for wrist sensors".to_owned(),
					}
				})
				.class("text-xs text-stone-300")
				.test_id("watch-vitals"),
			)
			.child(
				Text(move || {
					let s = snapshot.get();
					let queue = match s.waiting {
						0 => "All synced".to_owned(),
						1 => "1 queued".to_owned(),
						n => format!("{n} queued"),
					};
					match s.battery_percent {
						Some(pct) => format!("{queue} · {pct}%"),
						None => queue,
					}
				})
				.class("text-[11px] text-stone-400")
				.test_id("watch-queue"),
			)
			.child(Show(
				move || alert().is_some(),
				move || {
					Text(move || alert().unwrap_or_default())
						.class("text-[11px] text-amber-200")
						.test_id("watch-notice")
				},
			))
			.child(For(
				move || vec![matches!(snapshot.get().storage, Storage::Failed { .. })],
				|failed| *failed,
				move |failed| {
					if !failed {
						return View();
					}
					let reset = reset_state.clone();
					Button("Reset storage")
						.class("px-3 py-1.5 rounded-full bg-red-700 text-white text-[11px]")
						.test_id("watch-reset-storage")
						.on_press(move || reset.act(Action::ResetStorage))
				},
			))
			.child(
				View()
					.class("flex-row items-center justify-center gap-2 pt-1")
					.child(For(
						move || vec![snapshot.get().collecting],
						|collecting| *collecting,
						move |collecting| {
							let toggle = toggle_state.clone();
							Button(if collecting { "Pause" } else { "Record" })
								.class(
									"px-3 py-2 rounded-full bg-amber-400 text-black text-xs \
										 font-semibold",
								)
								.test_id("watch-toggle")
								.on_press(move || {
									toggle.act(Action::Command(Command::Collect {
										enabled: !collecting,
									}));
								})
						},
					))
					.child(
						Button("Sync")
							.class("px-3 py-2 rounded-full bg-stone-800 text-white text-xs")
							.test_id("watch-sync")
							.on_press(move || {
								sync_state.act(Action::Command(Command::Upload));
							}),
					),
			)
			.child(For(
				move || vec![live_workout.get()],
				|active| *active,
				move |active| {
					let workout = workout_state.clone();
					Button(if active {
						"End live workout"
					} else {
						"Start live workout"
					})
					.class(if active {
						"px-3 py-1.5 rounded-full bg-emerald-500 text-black text-[11px] \
							 font-semibold"
					} else {
						"px-3 py-1.5 rounded-full bg-stone-800 text-stone-200 text-[11px]"
					})
					.test_id("watch-workout")
					.on_press(move || workout.act(Action::ToggleLiveWorkout))
				},
			))
			.child(For(
				move || vec![permissions.get().wearable],
				|grant| *grant,
				move |grant| match grant {
					Grant::Unavailable => View(),
					Grant::Granted => Text("Sensors OK")
						.class("text-[11px] text-emerald-400")
						.test_id("watch-sensors-ok"),
					Grant::Needed | Grant::Unknown => {
						let sensor = sensor_state.clone();
						Button("Allow sensors")
							.class(
								"px-3 py-1.5 rounded-full bg-stone-900 text-stone-300 \
										 text-[11px]",
							)
							.test_id("watch-sensors")
							.on_press(move || sensor.act(Action::Allow(Step::Wearable)))
					}
				},
			)),
	)
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::sync::Mutex;

	use leptoskit::prelude::Set;
	use leptoskit_testing::Finder;
	use leptoskit_testing::Queries;
	use leptoskit_testing::WidgetTester;

	use super::*;
	use crate::phone::Permissions;

	#[test]
	fn wearable_app_validates_and_renders_surfaces_for_every_watch_os() {
		let snapshot = Snapshot {
			link: Link::Ready,
			collecting: true,
			waiting: 4,
			latest_heart_rate_bpm: Some(71),
			steps_today: Some(5_420),
			battery_percent: Some(84),
			..Snapshot::default()
		};
		for kind in [WatchKind::WearOs, WatchKind::WatchOs, WatchKind::HarmonyOs] {
			let app = quant_wearable_app(kind);
			app.validate().unwrap();
			assert!(!app.required_permissions().is_empty());
			let surfaces = build_watch_surfaces(kind, &snapshot).unwrap();
			surfaces.validate().unwrap();
			assert_eq!(surfaces.kind(), kind);
		}
		let status = status_from_snapshot(&snapshot);
		assert_eq!(status.battery_percent, Some(84));
	}

	#[test]
	fn status_preserves_upload_problem_and_distinguishes_companion_fallback() {
		let auth_err = Snapshot {
			link: Link::Ready,
			collecting: true,
			upload_problem: Some("Not allowed: missing health:write".into()),
			..Snapshot::default()
		};
		assert_eq!(
			status_from_snapshot(&auth_err).notice.as_deref(),
			Some("Not allowed: missing health:write")
		);

		let offline_with_phone = Snapshot {
			link: Link::Unreachable {
				reason: "no wifi".into(),
			},
			watch_connected: true,
			upload_problem: Some("The vault is out of reach".into()),
			..Snapshot::default()
		};
		assert_eq!(
			status_from_snapshot(&offline_with_phone).notice.as_deref(),
			Some("Syncing via phone")
		);
	}

	#[test]
	fn watch_screen_shows_vitals_and_dispatches_actions() {
		let owner = reactive_graph::owner::Owner::new();
		owner.set();
		std::mem::forget(owner);
		let log: Arc<Mutex<Vec<Action>>> = Arc::new(Mutex::new(Vec::new()));
		let captured = Arc::clone(&log);
		let state = State::new(Arc::new(move |action| {
			captured.lock().unwrap().push(action);
		}));
		state.snapshot.set(Snapshot {
			link: Link::Ready,
			collecting: true,
			waiting: 2,
			latest_heart_rate_bpm: Some(64),
			steps_today: Some(1_250),
			battery_percent: Some(92),
			..Snapshot::default()
		});
		let view_state = state.clone();
		let mut tester =
			WidgetTester::new(move || WatchScreen(view_state.clone()), 450.0, 600.0).unwrap();
		tester.get_by_text("Recording").unwrap();
		tester.get_by_text("64 bpm · 1250 steps").unwrap();
		tester.get_by_text("2 queued · 92%").unwrap();
		tester.tap(&Finder::test_id("watch-toggle")).unwrap();
		tester.tap(&Finder::test_id("watch-sync")).unwrap();
		tester.tap(&Finder::test_id("watch-workout")).unwrap();
		tester.tap(&Finder::test_id("watch-sensors")).unwrap();
		assert_eq!(
			*log.lock().unwrap(),
			vec![
				Action::Command(Command::Collect { enabled: false }),
				Action::Command(Command::Upload),
				Action::ToggleLiveWorkout,
				Action::Allow(Step::Wearable),
			]
		);

		// Once sensors are granted, the Allow sensors button becomes a passive
		// badge.
		state.permissions.set(Permissions {
			wearable: Grant::Granted,
			..Permissions::default()
		});
		state.snapshot.set(Snapshot {
			storage: Storage::Failed {
				reason: "key mismatch".into(),
			},
			..Snapshot::default()
		});
		tester.pump();
		tester.get_by_text("Sensors OK").unwrap();
		tester.get_by_text("Storage error").unwrap();
		tester.tap(&Finder::test_id("watch-reset-storage")).unwrap();
		assert_eq!(log.lock().unwrap().last(), Some(&Action::ResetStorage));
	}
}
