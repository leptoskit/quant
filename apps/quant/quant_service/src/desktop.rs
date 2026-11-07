//! The menu-bar item and the few `AppKit` calls the service makes.
//!
//! On macOS the item is a [`leptoskit_plugin_tray`] status item with a
//! template icon and a [`leptoskit_plugin_menu`] menu. Elsewhere (the
//! Android collector, later) there is no menu bar, and these do nothing.

use std::sync::Arc;

use quant_model::Settings;

use crate::daemon::Shared;

/// What the person chose in the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
	/// They opened the menu: look again so it is current.
	Looked,
	/// Open Quant…
	Open,
	/// Save the last minutes.
	Save(u32),
	/// Pause for minutes, or until resumed.
	Pause(Option<u32>),
	/// Resume.
	Resume,
	/// Open a privacy pane.
	Settings(Settings),
	/// Quit.
	Quit,
}

#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(not(target_os = "macos"))]
pub use other::*;

#[cfg(not(target_os = "macos"))]
mod other {
	use std::sync::Arc;

	use super::Action;
	use crate::daemon::Shared;

	/// No menu bar here.
	pub struct Desktop;

	impl Desktop {
		/// Nothing to install.
		pub fn install(_: Arc<Shared>) -> Self {
			Self
		}

		/// No choices.
		pub fn drain(&mut self) -> Vec<Action> {
			Vec::new()
		}

		/// Nothing to show.
		pub fn show(&mut self, _: &quant_model::Snapshot) {}

		/// Whether the app is in front.
		pub fn is_open(&mut self) -> bool {
			false
		}

		/// Nothing to remove.
		pub fn close(&mut self) {}
	}

	/// The window's service polls the core here.
	pub fn poll_soon() {}

	/// Not here.
	pub fn open_url(_: &str) {}

	/// Not here.
	pub fn quit() {}

	/// The guest's own screen is the window.
	pub fn open_window() {}

	/// Not here.
	pub fn set_opens_at_login(_: bool) -> Option<bool> {
		None
	}

	/// Not here.
	pub fn opens_at_login() -> Option<bool> {
		None
	}
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "AppKit's action dispatch takes a selector")]
mod macos {
	use std::rc::Rc;
	use std::sync::Mutex;
	use std::sync::PoisonError;
	use std::sync::atomic::AtomicBool;
	use std::sync::atomic::Ordering;
	use std::time::Duration;
	use std::time::Instant;

	use leptoskit_plugin_menu::Command;
	use leptoskit_plugin_menu::Definition;
	use leptoskit_plugin_menu::DesktopMenu;
	use leptoskit_plugin_menu::EventChannel;
	use leptoskit_plugin_menu::EventSender;
	use leptoskit_plugin_menu::Item;
	use leptoskit_plugin_menu::Separator;
	use leptoskit_plugin_menu::muda;
	use leptoskit_plugin_tray::EventKind;
	use leptoskit_plugin_tray::Options;
	use leptoskit_plugin_tray::RgbaIcon;
	use leptoskit_plugin_tray::TrayHost;
	use leptoskit_plugin_tray::TrayId;
	use objc2::MainThreadMarker;
	use objc2::sel;
	use objc2_app_kit::NSApplication;
	use quant_model::Link;
	use quant_model::Snapshot;

	use super::Action;
	use super::Arc;
	use super::Settings;
	use super::Shared;
	use crate::words;

	mod icon {
		include!(concat!(env!("OUT_DIR"), "/tray_icon.rs"));
	}
	static ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_icon.rgba"));

	/// How long after the menu opens to keep looking every 10 seconds. `AppKit`
	/// does not say when a status item's menu closes.
	const MENU_OPEN_FOR: Duration = Duration::from_secs(60);

	/// The process-wide muda callback forwards into whichever menu owner is
	/// current.
	static MENU_ROUTE: Mutex<Option<(EventSender, Arc<Shared>)>> = Mutex::new(None);
	static MENU_HANDLER: std::sync::Once = std::sync::Once::new();

	/// The menu-bar item.
	pub struct Desktop {
		tray: Option<Tray>,
		looked_until: Option<Instant>,
	}

	struct Tray {
		host: TrayHost,
		id: TrayId,
		menu: Rc<DesktopMenu>,
		items: Vec<Item>,
		events: EventChannel,
		tooltip: String,
	}

	impl Desktop {
		/// Put the item in the menu bar. Without one (no `NSApplication`
		/// running, say) the window still works.
		pub fn install(shared: Arc<Shared>) -> Self {
			let tray = match Tray::install(shared) {
				Ok(tray) => Some(tray),
				Err(error) => {
					eprintln!("quant: no menu-bar item: {error}");
					None
				}
			};
			Self {
				tray,
				looked_until: None,
			}
		}

		/// The menu choices and icon clicks since the last call.
		pub fn drain(&mut self) -> Vec<Action> {
			let Some(tray) = &mut self.tray else {
				return Vec::new();
			};
			let mut actions = Vec::new();
			if tray
				.host
				.drain()
				.events
				.iter()
				.any(|event| matches!(event.kind, EventKind::Pressed(_)))
			{
				self.looked_until = Some(Instant::now() + MENU_OPEN_FOR);
				actions.push(Action::Looked);
			}
			while let Some(event) = tray.events.try_recv() {
				let Some(chosen) = tray.menu.handle_event(&event) else {
					continue;
				};
				if let Some(action) = action(&chosen.id) {
					actions.push(action);
				}
			}
			actions
		}

		/// Bring the menu in step with a snapshot.
		pub fn show(&mut self, snapshot: &Snapshot) {
			if let Some(tray) = &mut self.tray
				&& let Err(error) = tray.show(snapshot)
			{
				eprintln!("quant: menu update failed: {error}");
			}
		}

		/// Whether the menu was opened recently or a window is on screen.
		pub fn is_open(&mut self) -> bool {
			if self
				.looked_until
				.is_some_and(|until| until <= Instant::now())
			{
				self.looked_until = None;
			}
			self.looked_until.is_some() || window_visible()
		}

		/// Remove the item.
		pub fn close(&mut self) {
			*MENU_ROUTE.lock().unwrap_or_else(PoisonError::into_inner) = None;
			self.tray = None;
		}
	}

	impl Tray {
		fn install(shared: Arc<Shared>) -> Result<Self, String> {
			let tray_wake = Arc::clone(&shared);
			let mut host =
				TrayHost::install(move || tray_wake.wake()).map_err(|e| e.to_string())?;
			let events = EventChannel::new(32).map_err(|e| e.to_string())?;
			*MENU_ROUTE.lock().unwrap_or_else(PoisonError::into_inner) =
				Some((events.sender(), shared));
			MENU_HANDLER.call_once(|| {
				muda::MenuEvent::set_event_handler(Some(|event: muda::MenuEvent| {
					let route = MENU_ROUTE
						.lock()
						.unwrap_or_else(PoisonError::into_inner)
						.clone();
					if let Some((sender, shared)) = route {
						sender.forward(event);
						shared.wake();
					}
				}));
			});
			let items = items(&Snapshot::default());
			let menu = Rc::new(menu(&items)?);
			let icon = RgbaIcon::new(ICON.to_vec(), icon::WIDTH, icon::HEIGHT)
				.map_err(|e| e.to_string())?;
			let tooltip = "Quant".to_owned();
			let options = Options {
				tooltip: Some(tooltip.clone()),
				title: None,
				template: true,
			};
			let id = host
				.create(icon, &options, Some(Rc::clone(&menu)))
				.map_err(|e| e.to_string())?;
			Ok(Self {
				host,
				id,
				menu,
				items,
				events,
				tooltip,
			})
		}

		fn show(&mut self, snapshot: &Snapshot) -> Result<(), String> {
			let items = items(snapshot);
			if ids(&items) == ids(&self.items) {
				// Same entries: relabel in place, so an open menu updates.
				for (now, before) in items.iter().zip(&self.items) {
					if let (
						Item::Command {
							id, label, enabled, ..
						},
						Item::Command {
							label: was,
							enabled: was_enabled,
							..
						},
					) = (now, before)
					{
						if label != was {
							self.menu.set_label(id, label).map_err(|e| e.to_string())?;
						}
						if enabled != was_enabled {
							self.menu
								.set_enabled(id, *enabled)
								.map_err(|e| e.to_string())?;
						}
					}
				}
			} else {
				let menu = Rc::new(menu(&items)?);
				self.host
					.set_menu(self.id, Some(Rc::clone(&menu)))
					.map_err(|e| e.to_string())?;
				self.menu = menu;
			}
			self.items = items;
			let tooltip = tooltip(&snapshot.headline);
			if tooltip != self.tooltip {
				self.host
					.set_tooltip(self.id, Some(&tooltip))
					.map_err(|e| e.to_string())?;
				self.tooltip = tooltip;
			}
			Ok(())
		}
	}

	fn menu(items: &[Item]) -> Result<DesktopMenu, String> {
		let definition = Definition::new(items.to_vec()).map_err(|e| e.to_string())?;
		DesktopMenu::new(definition).map_err(|e| e.to_string())
	}

	fn ids(items: &[Item]) -> Vec<Option<&str>> {
		items
			.iter()
			.map(|item| {
				match item {
					Item::Command { id, .. } => Some(id.as_str()),
					_ => None,
				}
			})
			.collect()
	}

	fn tooltip(headline: &str) -> String {
		let mut tooltip = format!("Quant · {headline}");
		while tooltip.encode_utf16().count() > 127 {
			tooltip.pop();
		}
		tooltip
	}

	/// The menu for a snapshot.
	pub(crate) fn items(snapshot: &Snapshot) -> Vec<Item> {
		let ready = snapshot.link == Link::Ready;
		let headline = if snapshot.headline.is_empty() {
			"Connecting to Quant…"
		} else {
			&snapshot.headline
		};
		let mut items: Vec<Item> = vec![Command("status").label(headline).enabled(false).into()];
		if let Some(notice) = snapshot.notice.as_ref().filter(|n| n.text != headline) {
			items.push(Command("notice").label(&notice.text).enabled(false).into());
		}
		let mut panes: Vec<Settings> = Vec::new();
		for pane in snapshot.sources.iter().filter_map(|s| s.settings) {
			if !panes.contains(&pane) {
				panes.push(pane);
			}
		}
		for pane in panes {
			items.push(
				Command(settings_id(pane))
					.label(format!("⚠︎ {}", words::permission_warning(pane)))
					.into(),
			);
		}
		items.push(Separator());
		for minutes in [5, 15, 30, 60] {
			items.push(
				Command(format!("save.{minutes}"))
					.label(words::save_label(minutes))
					.enabled(ready)
					.into(),
			);
		}
		items.push(Separator());
		if snapshot.paused {
			items.push(Command("resume").label("Resume").enabled(ready).into());
		} else {
			for (id, label) in [
				("pause.30", "Pause for 30 minutes"),
				("pause.60", "Pause for 1 hour"),
				("pause.forever", "Pause until resumed"),
			] {
				items.push(Command(id).label(label).enabled(ready).into());
			}
		}
		items.push(Separator());
		items.push(Command("open").label("Open Quant…").into());
		items.push(
			Command("quit")
				.label("Quit Quant")
				.accelerator("CmdOrCtrl+Q")
				.into(),
		);
		items
	}

	fn settings_id(pane: Settings) -> &'static str {
		match pane {
			Settings::ScreenRecording => "settings.screen",
			Settings::Microphone => "settings.microphone",
		}
	}

	pub(crate) fn action(id: &str) -> Option<Action> {
		Some(match id {
			"open" => Action::Open,
			"quit" => Action::Quit,
			"resume" => Action::Resume,
			"pause.30" => Action::Pause(Some(30)),
			"pause.60" => Action::Pause(Some(60)),
			"pause.forever" => Action::Pause(None),
			"settings.screen" => Action::Settings(Settings::ScreenRecording),
			"settings.microphone" => Action::Settings(Settings::Microphone),
			_ => Action::Save(id.strip_prefix("save.")?.parse().ok()?),
		})
	}

	fn window_visible() -> bool {
		let Some(main) = MainThreadMarker::new() else {
			return false;
		};
		// The status item and its menu are windows too; only a window that
		// can become main is the app's own.
		NSApplication::sharedApplication(main)
			.windows()
			.iter()
			.any(|window| window.isVisible() && window.canBecomeMainWindow())
	}

	/// Poll the resident core on the main thread soon. Wakes coalesce until
	/// that poll runs; this only enqueues, so it is safe from any thread.
	pub fn poll_soon() {
		static QUEUED: AtomicBool = AtomicBool::new(false);
		if !QUEUED.swap(true, Ordering::AcqRel) {
			dispatch2::DispatchQueue::main().exec_async(|| {
				QUEUED.store(false, Ordering::Release);
				crate::resident::poll();
			});
		}
	}

	/// Ask the app delegate to show the window.
	pub fn open_window() {
		let Some(main) = MainThreadMarker::new() else {
			return;
		};
		let app = NSApplication::sharedApplication(main);
		// SAFETY: `openQuant:` takes one object argument, as an action does;
		// a nil target walks the responder chain to the delegate.
		unsafe { app.sendAction_to_from(sel!(openQuant:), None, None) };
	}

	/// Open a URL, such as a System Settings pane, without blocking.
	pub fn open_url(url: &str) {
		let url = url.to_owned();
		std::thread::spawn(move || {
			let _ = std::process::Command::new("/usr/bin/open")
				.arg(url)
				.status();
		});
	}

	/// Quit the app; the host closes the service on the way out.
	pub fn quit() {
		if let Some(main) = MainThreadMarker::new() {
			NSApplication::sharedApplication(main).terminate(None);
		}
	}

	/// Whether `SMAppService` is available. objc2 finds classes at run time,
	/// and the host links only the frameworks Swift imports, so load
	/// `ServiceManagement` here before asking it anything.
	fn service_management() -> bool {
		use objc2::runtime::AnyClass;
		use objc2_foundation::NSBundle;
		use objc2_foundation::NSString;

		if AnyClass::get(c"SMAppService").is_none() {
			let path = NSString::from_str("/System/Library/Frameworks/ServiceManagement.framework");
			if let Some(bundle) = NSBundle::bundleWithPath(&path) {
				// SAFETY: loading a system framework runs only its own
				// initialisers.
				unsafe { bundle.load() };
			}
		}
		AnyClass::get(c"SMAppService").is_some()
	}

	/// Open at login, or stop, through `SMAppService.mainApp`. Returns the
	/// state afterwards.
	pub fn set_opens_at_login(enabled: bool) -> Option<bool> {
		use objc2_service_management::SMAppService;

		if !service_management() {
			return None;
		}
		// SAFETY: plain Objective-C calls on the shared main-app service.
		let service = unsafe { SMAppService::mainAppService() };
		let result = if enabled {
			unsafe { service.registerAndReturnError() }
		} else {
			unsafe { service.unregisterAndReturnError() }
		};
		if let Err(error) = result {
			eprintln!("quant: open at login: {}", error.localizedDescription());
		}
		opens_at_login()
	}

	/// Whether Quant opens at login.
	pub fn opens_at_login() -> Option<bool> {
		use objc2_service_management::SMAppService;
		use objc2_service_management::SMAppServiceStatus;

		if !service_management() {
			return None;
		}
		// SAFETY: plain Objective-C calls on the shared main-app service.
		let status = unsafe { SMAppService::mainAppService().status() };
		Some(status == SMAppServiceStatus::Enabled)
	}

	#[cfg(test)]
	mod tests {
		use quant_model::Notice;
		use quant_model::SourceRow;

		use super::*;

		fn labels(items: &[Item]) -> Vec<(String, bool)> {
			items
				.iter()
				.filter_map(|item| {
					match item {
						Item::Command { label, enabled, .. } => Some((label.clone(), *enabled)),
						_ => None,
					}
				})
				.collect()
		}

		#[test]
		fn a_recording_menu_offers_saving_and_pausing() {
			let snapshot = Snapshot {
				headline: "Recording · 4 sources".into(),
				link: Link::Ready,
				..Snapshot::default()
			};
			let items = items(&snapshot);
			let labels = labels(&items);
			assert_eq!(labels[0], ("Recording · 4 sources".into(), false));
			assert!(labels.contains(&("Save the last 15 minutes".into(), true)));
			assert!(labels.contains(&("Pause for 1 hour".into(), true)));
			assert!(!labels.iter().any(|(l, _)| l == "Resume"));
			assert_eq!(labels.last().unwrap().0, "Quit Quant");
			Definition::new(items).unwrap();
		}

		#[test]
		fn a_paused_menu_offers_resume_and_warns_about_permissions() {
			let snapshot = Snapshot {
				headline: "Paused until 15:30".into(),
				link: Link::Ready,
				paused: true,
				notice: Some(Notice {
					text: "Nothing recorded in that window".into(),
					error: true,
				}),
				sources: vec![
					SourceRow {
						name: "screen".into(),
						running: false,
						detail: None,
						settings: Some(Settings::ScreenRecording),
					},
					SourceRow {
						name: "apps".into(),
						running: true,
						detail: None,
						settings: Some(Settings::ScreenRecording),
					},
				],
				..Snapshot::default()
			};
			let labels = labels(&items(&snapshot));
			assert_eq!(labels[1].0, "Nothing recorded in that window");
			assert_eq!(
				labels
					.iter()
					.filter(|(l, _)| l.contains("Screen Recording is off"))
					.count(),
				1
			);
			assert!(labels.contains(&("Resume".into(), true)));
			assert!(!labels.iter().any(|(l, _)| l.starts_with("Pause for")));
		}

		#[test]
		fn nothing_needs_the_daemon_until_it_approves_us() {
			let snapshot = Snapshot {
				headline: "Waiting for approval · K7QD-M2XA".into(),
				link: Link::Pending {
					code: "K7QD-M2XA".into(),
				},
				..Snapshot::default()
			};
			for (label, enabled) in labels(&items(&snapshot)) {
				if label.starts_with("Save") || label.starts_with("Pause") {
					assert!(!enabled, "{label} is enabled");
				}
			}
		}

		#[test]
		fn menu_ids_map_to_actions() {
			assert_eq!(action("save.30"), Some(Action::Save(30)));
			assert_eq!(action("pause.forever"), Some(Action::Pause(None)));
			assert_eq!(
				action("settings.microphone"),
				Some(Action::Settings(Settings::Microphone))
			);
			assert_eq!(action("status"), None);
			assert_eq!(action("save.x"), None);
		}
	}
}
