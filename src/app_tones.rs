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
//! Matching is case-insensitive and substring/prefix based (never exact), and
//! the bundle id takes precedence over the human-facing app name — a bundle id
//! is the stricter, more precise identifier, so when both are present the bundle
//! id is checked first.

/// A category rule: one or more lowercase patterns (app name / bundle-id
/// substrings) mapped to a single tone keyword.
struct ToneRule {
    patterns: &'static [&'static str],
    tone: &'static str,
}

/// The tone table, checked in order. Order matters: more specific categories
/// (code vs. the bare "x" social pattern) come first so a name like "Xcode" is
/// not pulled into the social bucket by its trailing "x".
static RULES: &[ToneRule] = &[
    ToneRule {
        patterns: &[
            "slack",
            "teams",
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

/// Look up the tone keyword for a target app.
///
/// `bundle_id` is checked first (higher specificity), then `app_name`, across
/// all rules in order. Matching is case-insensitive and substring/prefix based.
/// Returns `None` for anything unmatched so the caller can keep the default
/// cleanup prompt.
pub fn tone_for(app_name: &str, bundle_id: Option<&str>) -> Option<&'static str> {
    [bundle_id.unwrap_or("").trim(), app_name.trim()]
        .into_iter()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .find_map(|c| {
            RULES
                .iter()
                .find(|rule| rule.patterns.iter().any(|p| c.contains(p)))
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
