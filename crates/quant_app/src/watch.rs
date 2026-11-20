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
use leptoskit::prelude::Button;
use leptoskit::prelude::Element;
use leptoskit::prelude::Get;
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
	let headline = match &snapshot.link {
		Link::Unpaired => "Pair via phone".to_owned(),
		Link::Pending { code } => format!("Approve {code}"),
		Link::Revoked => "Access revoked".to_owned(),
		Link::Connecting => "Connecting…".to_owned(),
		Link::Unreachable { .. } => "Offline spool".to_owned(),
		Link::Ready if snapshot.collecting => "Recording".to_owned(),
		Link::Ready => "Paused".to_owned(),
	};
	let notice = snapshot
		.upload_problem
		.as_deref()
		.map(|_| "Syncing via phone".to_owned())
		.or_else(|| {
			snapshot.watch_connected.then(|| {
				snapshot
					.watch_name
					.as_deref()
					.map_or_else(|| "Phone linked".to_owned(), |name| format!("Via {name}"))
			})
		});
	WearableStatus {
		headline,
		recording: snapshot.collecting && snapshot.link != Link::Unpaired,
		paired: snapshot.link == Link::Ready,
		heart_rate_bpm: snapshot.latest_heart_rate_bpm,
		steps_today: snapshot.steps_today,
		queued_readings: snapshot.waiting,
		battery_percent: None,
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
	let toggle_state = state.clone();
	let sync_state = state.clone();
	let sensor_state = state.clone();

	View()
		.class("flex-1 items-center justify-center gap-2 bg-black px-5 py-6")
		.child(
			Text("QUANT")
				.class("text-[10px] font-semibold text-amber-300")
				.test_id("watch-title"),
		)
		.child(
			Text(move || status_from_snapshot(&snapshot.get()).headline)
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
				match s.waiting {
					0 => "All synced".to_owned(),
					1 => "1 queued".to_owned(),
					n => format!("{n} queued"),
				}
			})
			.class("text-[11px] text-stone-400")
			.test_id("watch-queue"),
		)
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
			move || vec![permissions.get().wearable == Grant::Granted],
			|granted| *granted,
			move |granted| {
				let sensor = sensor_state.clone();
				Button(if granted {
					"Sensors OK"
				} else {
					"Allow sensors"
				})
				.class("px-3 py-1.5 rounded-full bg-stone-900 text-stone-300 text-[11px]")
				.test_id("watch-sensors")
				.on_press(move || {
					sensor.act(Action::Allow(Step::Wearable));
				})
			},
		))
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

	#[test]
	fn wearable_app_validates_and_renders_surfaces_for_every_watch_os() {
		let snapshot = Snapshot {
			link: Link::Ready,
			collecting: true,
			waiting: 4,
			latest_heart_rate_bpm: Some(71),
			steps_today: Some(5_420),
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
			..Snapshot::default()
		});
		let view_state = state.clone();
		let mut tester =
			WidgetTester::new(move || WatchScreen(view_state.clone()), 450.0, 450.0).unwrap();
		tester.get_by_text("Recording").unwrap();
		tester.get_by_text("64 bpm · 1250 steps").unwrap();
		tester.get_by_text("2 queued").unwrap();
		tester.tap(&Finder::test_id("watch-toggle")).unwrap();
		tester.tap(&Finder::test_id("watch-sync")).unwrap();
		tester.tap(&Finder::test_id("watch-sensors")).unwrap();
		assert_eq!(
			*log.lock().unwrap(),
			vec![
				Action::Command(Command::Collect { enabled: false }),
				Action::Command(Command::Upload),
				Action::Allow(Step::Wearable),
			]
		);
	}
}
