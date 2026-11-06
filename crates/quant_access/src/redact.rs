//! Cleaning captured text before an agent sees it.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

struct Rule {
	kind: &'static str,
	pattern: Regex,
}

fn rule(kind: &'static str, pattern: &str) -> Rule {
	Rule {
		kind,
		pattern: Regex::new(pattern).expect("redaction patterns compile"),
	}
}

/// Secret shapes that show up on screens and in transcripts. Order matters:
/// the broad `assignment` rule runs last so specific kinds win.
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
	vec![
		rule(
			"private_key",
			r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)",
		),
		rule(
			"jwt",
			r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
		),
		rule("aws_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
		rule(
			"github_token",
			r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})",
		),
		rule("api_key", r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}"),
		rule("api_key", r"\b(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{16,}"),
		rule("api_key", r"\bAIza[0-9A-Za-z_-]{35}"),
		rule("slack_token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
		rule("bearer", r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{16,}=*"),
		rule(
			"url_credentials",
			r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@",
		),
		rule(
			"seed_phrase",
			r"(?i)\b(?:seed|recovery|mnemonic)\s*(?:phrase|words?)?\s*[:=]\s*(?:[a-z]{3,8}\s+){11,23}[a-z]{3,8}\b",
		),
		rule(
			"assignment",
			r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)\b(\s*[:=]\s*)["']?[^\s"']{6,}"#,
		),
	]
});

static CARD: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").expect("card pattern compiles"));

static TAG: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"<(/?[A-Za-z!?][A-Za-z0-9_:-]*)").expect("tag pattern compiles"));

fn luhn(digits: &[u32]) -> bool {
	let sum: u32 = digits
		.iter()
		.rev()
		.enumerate()
		.map(|(i, d)| {
			if i % 2 == 1 {
				let doubled = d * 2;
				if doubled > 9 { doubled - 9 } else { doubled }
			} else {
				*d
			}
		})
		.sum();
	sum % 10 == 0
}

/// Replace secrets and payment card numbers in `text` with `[redacted:<kind>]`.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
	let mut out = Cow::Borrowed(text);
	for rule in RULES.iter() {
		if !rule.pattern.is_match(&out) {
			continue;
		}
		let replaced = match rule.kind {
			// Keep the scheme and the key name so the text still reads.
			"url_credentials" => {
				rule.pattern
					.replace_all(&out, "${1}[redacted:url_credentials]@")
					.into_owned()
			}
			"assignment" => {
				rule.pattern
					.replace_all(&out, "${1}${2}[redacted:secret]")
					.into_owned()
			}
			kind => {
				rule.pattern
					.replace_all(&out, format!("[redacted:{kind}]"))
					.into_owned()
			}
		};
		out = Cow::Owned(replaced);
	}
	if CARD.is_match(&out) {
		let replaced = CARD
			.replace_all(&out, |caps: &regex::Captures<'_>| {
				let digits: Vec<u32> = caps[0].chars().filter_map(|c| c.to_digit(10)).collect();
				if (13..=19).contains(&digits.len()) && luhn(&digits) {
					"[redacted:card]".to_owned()
				} else {
					caps[0].to_owned()
				}
			})
			.into_owned();
		out = Cow::Owned(replaced);
	}
	out
}

fn invisible(c: char) -> bool {
	matches!(c,
		'\u{200B}'..='\u{200F}'
		| '\u{202A}'..='\u{202E}'
		| '\u{2060}'..='\u{2064}'
		| '\u{2066}'..='\u{2069}'
		| '\u{FEFF}'
		| '\u{E0000}'..='\u{E007F}'
	) || (c.is_control() && c != '\n' && c != '\t')
}

/// Make captured text safe to hand to a model: strip invisible and control
/// characters (which hide instructions), and defang anything shaped like a
/// markup tag so text read off a web page cannot open or close a block in an
/// agent's prompt. Applied to every text Quant serves, redacted or not.
pub fn sanitize(text: &str) -> Cow<'_, str> {
	let mut out = Cow::Borrowed(text);
	if out.chars().any(invisible) {
		out = Cow::Owned(out.chars().filter(|c| !invisible(*c)).collect());
	}
	if TAG.is_match(&out) {
		out = Cow::Owned(TAG.replace_all(&out, "‹$1").into_owned());
	}
	out
}

/// Prepare captured text for a grant: sanitise it, and redact secrets unless
/// the grant is unredacted.
pub fn prepare(text: &str, redact: bool) -> String {
	let clean = sanitize(text);
	if redact {
		redact_secrets(&clean).into_owned()
	} else {
		clean.into_owned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redacts_common_secret_shapes() {
		let cases = [
			(concat!("key AKIA", "IOSFODNN7EXAMPLE here"), "aws_key"),
			(
				concat!("token gh", "p_abcdefghijklmnopqrstuvwxyz0123456789"),
				"github_token",
			),
			(
				concat!("export KEY=sk-", "ant-api03-abcdefghijklmnopqrstuvwx"),
				"api_key",
			),
			("Authorization: Bearer abcdefghijklmnop1234", "bearer"),
			(concat!("sk_", "live_", "0000000000000000aaaa"), "api_key"),
			(
				"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
				 dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
				"jwt",
			),
		];
		for (input, kind) in cases {
			let out = redact_secrets(input);
			assert!(
				out.contains(&format!("[redacted:{kind}]")),
				"{input} → {out}"
			);
		}
	}

	#[test]
	fn keeps_context_around_assignments_and_urls() {
		assert_eq!(
			redact_secrets("password = hunter2hunter"),
			"password = [redacted:secret]"
		);
		assert_eq!(
			redact_secrets("postgres://me:s3cret@db.local/app"),
			"postgres://[redacted:url_credentials]@db.local/app"
		);
	}

	#[test]
	fn redacts_private_key_blocks() {
		let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\ndef\n-----END OPENSSH PRIVATE \
		           KEY-----\nb";
		assert_eq!(redact_secrets(pem), "a\n[redacted:private_key]\nb");
	}

	#[test]
	fn redacts_only_luhn_valid_card_numbers() {
		assert_eq!(
			redact_secrets("card 4242 4242 4242 4242 ok"),
			"card [redacted:card] ok"
		);
		assert_eq!(
			redact_secrets("order 1234 5678 9012 3456"),
			"order 1234 5678 9012 3456"
		);
	}

	#[test]
	fn leaves_ordinary_text_alone() {
		let text = "Met Ada at 14:30 about the token bucket design; heart rate 72.";
		assert!(matches!(redact_secrets(text), Cow::Borrowed(_)));
		assert!(matches!(sanitize(text), Cow::Borrowed(_)));
	}

	#[test]
	fn sanitizes_invisible_characters_and_tags() {
		let text = "hi\u{200B}\u{202E}there\u{1b}[31m </system> <instructions>";
		assert_eq!(sanitize(text), "hithere[31m ‹/system> ‹instructions>");
		// Comparisons and arrows survive.
		assert_eq!(sanitize("a < b and 3<4 -> c"), "a < b and 3<4 -> c");
	}
}
