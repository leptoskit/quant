//! Grounding: a claim's quote must really be in the record it cites.

use unicode_normalization::UnicodeNormalization;

/// Normalise text for grounding comparisons: NFKC, case folding, and every
/// run of whitespace collapsed to single spaces.
pub fn normalize(text: &str) -> String {
	let mut out = String::new();
	for c in text.nfkc().flat_map(char::to_lowercase) {
		if c.is_whitespace() {
			if !out.ends_with(' ') {
				out.push(' ');
			}
		} else {
			out.push(c);
		}
	}
	out.trim().to_owned()
}

/// Whether `quote` appears in `text` after [`normalize`], as whole words:
/// neither side may touch a letter or digit, and the quote must contain at
/// least one of its own. Both sides are normalised before comparing, so a
/// quote survives case, accent, and whitespace differences but never a
/// changed word.
pub fn quote_grounded(text: &str, quote: &str) -> bool {
	let text = normalize(text);
	let needle = normalize(quote);
	if !needle.chars().any(|c| c.is_alphanumeric()) {
		return false;
	}
	let mut rest = text.as_str();
	while let Some(found) = rest.find(needle.as_str()) {
		let end = found + needle.len();
		let before = rest[..found]
			.chars()
			.next_back()
			.is_some_and(char::is_alphanumeric);
		let after = rest[end..].chars().next().is_some_and(char::is_alphanumeric);
		if !before && !after {
			return true;
		}
		rest = &rest[end..];
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn normalisation_folds_case_accents_and_whitespace() {
		assert_eq!(normalize("  Café   au\nlait\t"), "café au lait");
		assert_eq!(normalize("CAFÉ"), "café");
		// NFKC composes ligatures into their parts.
		assert!(normalize("ﬁle").contains("file"));
	}

	#[test]
	fn quotes_ground_as_whole_words() {
		let text = "We agreed to ship on Friday, then went home.";
		assert!(quote_grounded(text, "agreed to ship on Friday"));
		assert!(quote_grounded(text, "AGREED  TO   SHIP ON FRIDAY"));
		// Mid-word matches do not ground.
		assert!(!quote_grounded(text, "hip"));
		assert!(!quote_grounded(text, "gree"));
		// A whole word does.
		assert!(quote_grounded(text, "ship"));
		// A quote must exist at all.
		assert!(!quote_grounded(text, "agreed to ship on Saturday"));
		assert!(!quote_grounded(text, ""));
		assert!(!quote_grounded(text, "!!!"));
		// Punctuation around the quote is fine; a letter is not.
		assert!(quote_grounded(text, "ship on Friday, then"));
		assert!(!quote_grounded(text, "ship on Friday, then w"));
	}
}
