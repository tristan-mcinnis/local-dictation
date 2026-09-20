//! Per-app tone keywords for cleanup.
//!
//! A short, single-word tone directive (e.g. "casual chat", "technical") is
//! appended to the cleanup prompt when the user is dictating into a known app,
//! so the small cleanup model adapts the writing style to the target surface.
//! This module is a pure, dependency-free lookup table — no macOS, no network,
//! no models. It is intentionally tiny (the cleanup model is a 1B) and
//! deterministic: unknown apps return `None` and the default cleanup prompt is
//! used unchanged.
//!
//! Matching is case-insensitive and on WHOLE SEGMENTS: the bundle id or app
//! name is split on anything that is not a letter or digit
//! (`com.apple.TextEdit` → `com`, `apple`, `textedit`), and a pattern must equal
//! one segment (or, for a multi-word pattern, a run of them). It used to be a
//! substring test, so the pattern "x" made Firefox, Excel, TextEdit and Dropbox
//! "social", and "word" made 1Password "neutral prose". The bundle id takes
//! precedence over the human-facing app name.

/// A category rule: one or more lowercase patterns (app name / bundle-id
/// substrings) mapped to a single tone keyword.
struct ToneRule {
    patterns: &'static [&'static str],
    tone: &'static str,
}

/// The tone table, checked in order. A pattern is one or more whole segments
/// (see the module docs), so "x" is the app named X and nothing else. Bundle
/// ids whose segment is not the product name get their own pattern
/// (`slackmacgap`, `mobilesms`, `teams2`).
static RULES: &[ToneRule] = &[
    ToneRule {
        patterns: &[
            "slack",
            "slackmacgap",
            "teams",
            "teams2",
            "mobilesms",
            "whatsapp",
            "messages",
            "imessage",
            "ichat",
            "signal",
            "telegram",
            "discord",
        ],
        tone: "casual chat",
    },
    ToneRule {
        patterns: &["mail", "outlook", "gmail", "superhuman"],
        tone: "email",
    },
    ToneRule {
        patterns: &[
            "notion",
            "obsidian",
            "bear",
            "notes",
            "google docs",
            "word",
        ],
        tone: "neutral prose",
    },
    ToneRule {
        patterns: &[
            "visual studio code",
            "vscode",
            "cursor",
            "xcode",
            "sublime",
            "intellij",
        ],
        tone: "technical",
    },
    ToneRule {
        patterns: &["twitter", "x", "linkedin", "instagram", "reddit"],
        tone: "social",
    },
];

/// Lowercased alphanumeric segments of a bundle id or app name.
fn segments(candidate: &str) -> Vec<String> {
    candidate
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// True when `pattern`'s words equal a contiguous run of `segs`.
fn pattern_matches(segs: &[String], pattern: &str) -> bool {
    let words: Vec<&str> = pattern.split_whitespace().collect();
    !words.is_empty()
        && segs
            .windows(words.len())
            .any(|w| w.iter().zip(&words).all(|(seg, word)| seg == word))
}

/// Look up the tone keyword for a target app.
///
/// `bundle_id` is checked first (higher specificity), then `app_name`, across
/// all rules in order. Matching is case-insensitive and whole-segment (see the
/// module docs). Returns `None` for anything unmatched so the caller can keep
/// the default cleanup prompt.
pub fn tone_for(app_name: &str, bundle_id: Option<&str>) -> Option<&'static str> {
    [bundle_id.unwrap_or(""), app_name]
        .into_iter()
        .map(segments)
        .filter(|segs| !segs.is_empty())
        .find_map(|segs| {
            RULES
                .iter()
                .find(|rule| rule.patterns.iter().any(|p| pattern_matches(&segs, p)))
                .map(|rule| rule.tone)
        })
}

/// The short prompt fragment appended to a cleanup prompt when a tone is known.
/// Kept to one sentence so the small model isn't distracted from its narrow job.
pub fn tone_suffix(tone: &str) -> String {
    format!("\n\nStyle: adapt the tone to {tone}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_must_be_a_whole_segment_not_a_substring() {
        // Regression: every one of these used to get a tone from a substring
        // ("x" in textedit/excel/firefox/dropbox, "word" in 1password).
        for (name, bundle) in [
            ("TextEdit", Some("com.apple.TextEdit")),
            ("Microsoft Excel", Some("com.microsoft.Excel")),
            ("Firefox", Some("org.mozilla.firefox")),
            ("Dropbox", Some("com.getdropbox.dropbox")),
            ("1Password", Some("com.1password.1password")),
            ("Pixelmator Pro", Some("com.pixelmatorteam.pixelmator")),
            ("Mailchimp", None),
            ("Keynotes", None),
            ("Perplexity", Some("ai.perplexity.mac")),
        ] {
            assert_eq!(tone_for(name, bundle), None, "{name} / {bundle:?}");
        }
    }

    #[test]
    fn real_bundle_ids_and_names_still_match() {
        for (name, bundle, tone) in [
            ("Slack", Some("com.tinyspeck.slackmacgap"), "casual chat"),
            ("Messages", Some("com.apple.MobileSMS"), "casual chat"),
            ("Microsoft Teams", Some("com.microsoft.teams2"), "casual chat"),
            ("WhatsApp", Some("net.whatsapp.WhatsApp"), "casual chat"),
            ("Signal", Some("org.whispersystems.signal-desktop"), "casual chat"),
            ("Mail", Some("com.apple.mail"), "email"),
            ("Microsoft Outlook", Some("com.microsoft.Outlook"), "email"),
            ("Microsoft Word", Some("com.microsoft.Word"), "neutral prose"),
            ("Notes", Some("com.apple.Notes"), "neutral prose"),
            ("Obsidian", Some("md.obsidian"), "neutral prose"),
            ("Code", Some("com.microsoft.VSCode"), "technical"),
            ("Xcode", Some("com.apple.dt.Xcode"), "technical"),
            ("Cursor", Some("com.todesktop.230313mzl4w4u92"), "technical"),
            ("Sublime Text", Some("com.sublimetext.4"), "technical"),
            ("Visual Studio Code", None, "technical"),
            ("X", None, "social"),
            ("LinkedIn", None, "social"),
        ] {
            assert_eq!(tone_for(name, bundle), Some(tone), "{name} / {bundle:?}");
        }
    }

    #[test]
    fn bundle_id_matches() {
        assert_eq!(
            tone_for("Some App", Some("com.microsoft.VSCode")),
            Some("technical")
        );
        assert_eq!(
            tone_for("", Some("com.slack.Slack")),
            Some("casual chat")
        );
        assert_eq!(tone_for("", Some("com.apple.mail")), Some("email"));
    }

    #[test]
    fn app_name_substring_matches() {
        assert_eq!(tone_for("Slack", None), Some("casual chat"));
        assert_eq!(tone_for("Visual Studio Code", None), Some("technical"));
        assert_eq!(tone_for("Microsoft Word", None), Some("neutral prose"));
        assert_eq!(tone_for("Notion", None), Some("neutral prose"));
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            tone_for("SLACK", Some("COM.SLACK.SLACK")),
            Some("casual chat")
        );
        assert_eq!(tone_for("SLACK", None), Some("casual chat"));
        assert_eq!(tone_for("xcode", None), Some("technical"));
    }

    #[test]
    fn bundle_id_takes_precedence_over_name() {
        // App name alone would be "technical", but the bundle id names a chat app.
        assert_eq!(
            tone_for("Xcode", Some("com.slack.Slack")),
            Some("casual chat")
        );
    }

    #[test]
    fn social_x_via_name() {
        assert_eq!(tone_for("X", None), Some("social"));
        assert_eq!(tone_for("LinkedIn", None), Some("social"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(tone_for("Some Unknown App", None), None);
        assert_eq!(tone_for("", Some("com.unknown.app")), None);
    }

    #[test]
    fn tone_suffix_content() {
        assert_eq!(
            tone_suffix("casual chat"),
            "\n\nStyle: adapt the tone to casual chat."
        );
        assert_eq!(
            tone_suffix("technical"),
            "\n\nStyle: adapt the tone to technical."
        );
    }
}
