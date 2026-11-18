//! Which apps and windows are never captured.

/// Matches apps and window titles that must not be captured. Capture code
/// matches bundle ids; importers, which only see display names, match those
/// through [`Exclusions::with_app_names`].
#[derive(Clone, Debug, Default)]
pub struct Exclusions {
	bundle_ids: Vec<BundlePattern>,
	title_patterns: Vec<String>,
	app_names: Vec<String>,
}

#[derive(Clone, Debug)]
enum BundlePattern {
	Exact(String),
	Prefix(String),
}

impl Exclusions {
	/// Build from bundle id patterns (a trailing `*` matches any suffix) and
	/// title fragments. Matching ignores case; blank entries are ignored.
	pub fn new(bundle_ids: &[String], title_patterns: &[String]) -> Self {
		let bundle_ids = bundle_ids
			.iter()
			.map(|pattern| pattern.trim().to_lowercase())
			.filter(|pattern| !pattern.is_empty() && pattern != "*")
			.map(|pattern| {
				match pattern.strip_suffix('*') {
					Some(prefix) => BundlePattern::Prefix(prefix.to_owned()),
					None => BundlePattern::Exact(pattern),
				}
			})
			.collect();
		let title_patterns = title_patterns
			.iter()
			.map(|pattern| pattern.trim().to_lowercase())
			.filter(|pattern| !pattern.is_empty())
			.collect();
		Self {
			bundle_ids,
			title_patterns,
			app_names: Vec::new(),
		}
	}

	/// Also match apps by display name (a name is excluded when it contains
	/// one of these, case ignored). Importers have no bundle ids, so they
	/// reuse the same exclusions through the names password managers and
	/// private windows show up under.
	#[must_use]
	pub fn with_app_names(mut self, app_names: &[String]) -> Self {
		self.app_names = app_names
			.iter()
			.map(|name| name.trim().to_lowercase())
			.filter(|name| !name.is_empty())
			.collect();
		self
	}

	/// Whether the app with this bundle id is excluded.
	pub fn excludes_app(&self, bundle_id: Option<&str>) -> bool {
		let Some(bundle_id) = bundle_id else {
			return false;
		};
		let bundle_id = bundle_id.to_lowercase();
		self.bundle_ids.iter().any(|pattern| {
			match pattern {
				BundlePattern::Exact(exact) => *exact == bundle_id,
				BundlePattern::Prefix(prefix) => bundle_id.starts_with(prefix.as_str()),
			}
		})
	}

	/// Whether a window with this title is excluded.
	pub fn excludes_title(&self, title: Option<&str>) -> bool {
		let Some(title) = title else {
			return false;
		};
		let title = title.to_lowercase();
		self.title_patterns
			.iter()
			.any(|pattern| title.contains(pattern.as_str()))
	}

	/// Whether an app shown under this display name is excluded. A name
	/// matches when it contains one of the configured names, so "1Password 7"
	/// matches `1password`.
	pub fn excludes_name(&self, app_name: Option<&str>) -> bool {
		let Some(app_name) = app_name else {
			return false;
		};
		let app_name = app_name.to_lowercase();
		self.app_names
			.iter()
			.any(|name| app_name.contains(name.as_str()))
	}

	/// Whether either the app or the window title is excluded.
	pub fn excludes(&self, bundle_id: Option<&str>, title: Option<&str>) -> bool {
		self.excludes_app(bundle_id) || self.excludes_title(title)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::default_excluded_bundle_ids;
	use crate::config::default_excluded_title_patterns;

	fn defaults() -> Exclusions {
		Exclusions::new(
			&default_excluded_bundle_ids(),
			&default_excluded_title_patterns(),
		)
	}

	#[test]
	fn default_password_managers_are_excluded() {
		let exclusions = defaults();
		for bundle in [
			"com.1password.1password",
			"com.agilebits.onepassword7",
			"com.bitwarden.desktop",
			"org.keepassxc.keepassxc",
			"com.apple.keychainaccess",
			"com.apple.Passwords",
			"com.dashlane.dashlanephonefinal",
			"com.lastpass.LastPass",
			"me.proton.pass.electron",
		] {
			assert!(
				exclusions.excludes_app(Some(bundle)),
				"{bundle} should be excluded"
			);
		}
		for bundle in ["com.apple.Safari", "dev.zed.Zed", "com.apple.passwordsx"] {
			assert!(
				!exclusions.excludes_app(Some(bundle)),
				"{bundle} should be captured"
			);
		}
		assert!(!exclusions.excludes_app(None));
	}

	#[test]
	fn exact_patterns_do_not_match_prefixes() {
		let exclusions = Exclusions::new(&["com.bitwarden.desktop".into()], &[]);
		assert!(!exclusions.excludes_app(Some("com.bitwarden.desktop.helper")));
		assert!(exclusions.excludes_app(Some("COM.Bitwarden.Desktop")));
	}

	#[test]
	fn private_window_titles_are_excluded_ignoring_case() {
		let exclusions = defaults();
		assert!(exclusions.excludes_title(Some("Mozilla Firefox — Private Browsing")));
		assert!(exclusions.excludes_title(Some("New Tab - Google Chrome (incognito)")));
		assert!(exclusions.excludes_title(Some("Bing - InPrivate - Microsoft Edge")));
		assert!(!exclusions.excludes_title(Some("Quant – design.md")));
		assert!(!exclusions.excludes_title(None));
		assert!(exclusions.excludes(Some("com.apple.Safari"), Some("Incognito")));
		assert!(!exclusions.excludes(Some("com.apple.Safari"), Some("Inbox")));
	}

	#[test]
	fn blank_and_bare_wildcard_patterns_match_nothing() {
		let exclusions = Exclusions::new(&[String::new(), "*".into()], &["  ".into()]);
		assert!(!exclusions.excludes(Some("com.apple.Safari"), Some("anything")));
	}

	#[test]
	fn password_manager_names_are_excluded_for_importers() {
		let exclusions = Exclusions::new(&[], &default_excluded_title_patterns())
			.with_app_names(&crate::config::default_excluded_app_names());
		for name in [
			"1Password 7",
			"Bitwarden",
			"KeePassXC",
			"Keychain Access",
			"Proton Pass",
			"Dashlane",
			"LastPass",
		] {
			assert!(
				exclusions.excludes_name(Some(name)),
				"{name} should be excluded"
			);
		}
		for name in ["Zed", "Google Chrome", "Safari", "Notes"] {
			assert!(
				!exclusions.excludes_name(Some(name)),
				"{name} should be captured"
			);
		}
		assert!(!exclusions.excludes_name(None));
	}
}
