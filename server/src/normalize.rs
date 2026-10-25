//! Inbound message normalization — peel the agent wrapper off.
//!
//! External callers reach the human from diverse agent ecosystems, each
//! wrapping the user's words in its own envelope: massive system prompts,
//! tool schemas, environment dumps, and proprietary XML-ish tags. A human
//! cannot parse token floods the way an LLM can, so the server extracts
//! the genuine message before anything reaches the timeline.
//!
//! # Design
//!
//! One normalizer per agent family, selected by the already-detected
//! [`ClientType`](crate::profile::ClientType). Each normalizer knows:
//!
//! - which envelope tag carries the real input (`<user_query>`,
//!   `<user_input>`, ...), and
//! - which parts are pure noise (`REQUIREMENT:` blocks, terminal-status
//!   reminders, ...).
//!
//! Normalization runs **per content part**, not on the joined text —
//! multi-part messages carry one envelope per turn, and extracting from
//! the joined string would return the *oldest* turn instead of the newest.
//!
//! # Observed formats (`server/observations/`, 369 captures)
//!
//! | Agent | Shape | Envelope | Noise |
//! |---|---|---|
//! | WorkBuddy `conversation` | `content[]` parts | `<user_query>` per part, newest last | `<system-reminder>` context blocks |
//! | WorkBuddy `conversation_topic` | `content[]` parts | `<session>` | meta-request, never reaches the timeline |
//! | Trae (`hertz`) | `content[]` parts | `<user_input>` | `REQUIREMENT:` language-detection blocks, terminal-status reminders |
//! | Claude Code | plain string | none (bare text) | leading `<system-reminder>` date/context prefix, `<session>` topic requests |
//! | Generic (curl, Go clients) | plain string | none | none |
//!
//! When no envelope is found the text is returned as-is: an unknown agent's
//! message must still reach the human, wrapper and all, rather than being
//! dropped by a normalizer that does not know it.

use crate::openai_compat::{ChatContent, ContentPart};
use crate::profile::ClientType;

/// Extract the human-readable message from an inbound `user` message.
///
/// `client` selects the agent-specific normalizer; unknown clients fall
/// back to generic tag-stripping. Never fails — worst case the raw text
/// passes through untouched.
pub fn normalize_user_message(client: &ClientType, content: &ChatContent) -> String {
    match content {
        ChatContent::Text(s) => normalize_whole(client, s),
        ChatContent::Parts(parts) => normalize_parts(client, parts),
        ChatContent::Null => String::new(),
    }
}

/// Normalize a single-string message.
fn normalize_whole(client: &ClientType, text: &str) -> String {
    match client {
        ClientType::WorkBuddy => {
            // Single-string WorkBuddy requests are rare (one capture in the
            // whole corpus) but share the same envelope.
            extract_tag(text, "user_query").unwrap_or_else(|| text.trim().to_string())
        }
        ClientType::Trae => {
            extract_tag(text, "user_input").unwrap_or_else(|| text.trim().to_string())
        }
        ClientType::ClaudeCode => strip_claude_prefix(text),
        ClientType::DeepSeekHarness if is_dsh_injection(text) => String::new(),
        ClientType::Codex
        | ClientType::Cursor
        | ClientType::Qoder
        | ClientType::DeepSeekHarness
        | ClientType::OpenCode
        | ClientType::Pi
        | ClientType::Unknown => strip_generic_envelope(text),
    }
}

/// Normalize a multi-part message, newest turn wins.
///
/// Each part carries its own envelope; parts are scanned newest-first and
/// the first part that yields a non-empty message wins. Parts that are
/// pure noise (Trae `REQUIREMENT:` blocks, reminder-only parts) contribute
/// nothing and are skipped.
fn normalize_parts(client: &ClientType, parts: &[ContentPart]) -> String {
    for part in parts.iter().rev() {
        if part.part_type != "text" {
            continue;
        }
        let text = match &part.text {
            Some(t) => t,
            None => continue,
        };
        if is_noise_part(client, text) {
            continue;
        }
        let msg = normalize_whole(client, text);
        if !msg.trim().is_empty() {
            return msg;
        }
    }

    // Nothing yielded a message — fall back to the raw text of the newest
    // text part rather than showing an empty bubble.
    parts
        .iter()
        .rev()
        .filter(|p| p.part_type == "text")
        .filter_map(|p| p.text.as_deref())
        .find(|t| !t.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Whether a whole part is agent noise with no user content.
fn is_noise_part(client: &ClientType, text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    match client {
        // Trae prepends a language-detection instruction block that never
        // carries user input.
        ClientType::Trae => trimmed.starts_with("REQUIREMENT:"),
        ClientType::DeepSeekHarness => is_dsh_injection(trimmed),
        _ => false,
    }
}

/// DeepSeek Harness appends its own `user` messages after the real one:
/// a runtime-context snapshot and a `<system-reminder>` skill list. Neither
/// carries anything the person typed.
fn is_dsh_injection(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Current runtime context.")
        || (t.starts_with("<system-reminder") && t.trim_end().ends_with("</system-reminder>"))
}

/// Extract `<tag>...</tag>` content.
///
/// The closing tag is searched **forward from the opening tag**, not with
/// `rfind` across the whole text: WorkBuddy's error-recovery appends retry
/// context that may contain additional closing tags, and `rfind` would
/// swallow the entire recovery block into the "user message".
fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(open.as_str())?;
    let content_start = start + open.len();
    let end_rel = text[content_start..].find(close.as_str())?;
    let content = text[content_start..content_start + end_rel].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// Strip a Claude Code `<system-reminder>...</system-reminder>` prefix.
///
/// Claude Code sends bare text, sometimes preceded by a date/context
/// reminder block. The reminder is dropped; anything else is kept
/// verbatim. A message that is *only* a reminder yields an empty string,
/// which the caller treats as "nothing to extract" and falls back.
fn strip_claude_prefix(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("<system-reminder") {
        // Topic requests wrap the session in <session> tags.
        if let Some(inner) = extract_tag(trimmed, "session") {
            return inner;
        }
        return trimmed.to_string();
    }
    // Find the matching close tag, tolerating attributes on the open tag.
    let Some(open_end) = trimmed.find('>') else {
        return trimmed.to_string();
    };
    let Some(close_rel) = trimmed[open_end..].find("</system-reminder>") else {
        return trimmed.to_string();
    };
    let rest = trimmed[open_end + close_rel + "</system-reminder>".len()..].trim();
    if let Some(inner) = extract_tag(rest, "session") {
        return inner;
    }
    rest.to_string()
}

/// Generic envelope stripping for unknown agents.
///
/// Tries the known envelope tags in order; if none match, the text passes
/// through untouched. An unrecognized agent must still be readable, not
/// silently emptied by a normalizer that does not know it.
fn strip_generic_envelope(text: &str) -> String {
    for tag in ["user_query", "user_input", "session"] {
        if let Some(inner) = extract_tag(text, tag) {
            return inner;
        }
    }
    // A leading system-reminder prefix is noise on every agent observed so
    // far; drop it even when the agent itself is unrecognized.
    let stripped = strip_claude_prefix(text);
    if stripped != text.trim() {
        return stripped;
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    //! Regression tests for per-agent normalization.
    //!
    //! Fixtures are trimmed from real captures in `server/observations/`
    //! (which is git-ignored): the envelope tags and noise blocks below are
    //! verbatim shapes, only the surrounding bulk is cut.

    use super::*;
    use crate::openai_compat::{ChatContent, ContentPart, ImageUrl};

    fn parts(texts: &[&str]) -> ChatContent {
        ChatContent::Parts(
            texts
                .iter()
                .map(|t| ContentPart {
                    part_type: "text".to_string(),
                    text: Some(t.to_string()),
                    image_url: None,
                    file: None,
                })
                .collect(),
        )
    }

    // --- WorkBuddy ---

    #[test]
    fn workbuddy_multipart_yields_newest_user_query() {
        // Real shape: every part carries its own <user_query>, oldest first.
        // Extracting from the joined text returned the OLDEST turn.
        let content = parts(&[
            "<system-reminder>ctx1</system-reminder><user_query>first</user_query>",
            "<system-reminder>ctx2</system-reminder><user_query>second</user_query>",
        ]);
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &content),
            "second"
        );
    }

    #[test]
    fn workbuddy_single_part_envelope() {
        let content =
            parts(&["<system-reminder>ctx</system-reminder><user_query>hello</user_query>"]);
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &content),
            "hello"
        );
    }

    #[test]
    fn workbuddy_closing_tag_searches_forward_not_rfind() {
        // Error-recovery appends retry context with extra closing tags;
        // rfind would swallow it all into the message.
        let content = ChatContent::Text(
            "<user_query>real question</user_query> retry: ... </user_query> junk".to_string(),
        );
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &content),
            "real question"
        );
    }

    // --- Trae ---

    const TRAE_REQUIREMENT: &str =
        "\nREQUIREMENT:\n- Detect the language XX of the user query.\n\n";

    #[test]
    fn trae_user_input_wins_over_noise() {
        let part =
            "\n<user_input>\ntrae test\n</user_input>\n\n<system-reminder>tz</system-reminder>\n";
        let content = parts(&[TRAE_REQUIREMENT, part]);
        assert_eq!(
            normalize_user_message(&ClientType::Trae, &content),
            "trae test"
        );
    }

    #[test]
    fn trae_newest_input_wins_across_parts() {
        let content = parts(&[
            "<user_input>first</user_input>",
            TRAE_REQUIREMENT,
            "<user_input>second</user_input>",
        ]);
        assert_eq!(
            normalize_user_message(&ClientType::Trae, &content),
            "second"
        );
    }

    #[test]
    fn trae_requirement_only_falls_back_to_raw() {
        // No envelope anywhere: pass through rather than empty.
        let content = parts(&[TRAE_REQUIREMENT]);
        assert!(normalize_user_message(&ClientType::Trae, &content).contains("REQUIREMENT"));
    }

    // --- Claude Code ---

    #[test]
    fn claude_bare_text_passes_through() {
        let content = ChatContent::Text("Hello! What are you?".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::ClaudeCode, &content),
            "Hello! What are you?"
        );
    }

    #[test]
    fn claude_system_reminder_prefix_is_stripped() {
        let content = ChatContent::Text(
            "<system-reminder>\n# currentDate\nToday is 2026-08-24.\n</system-reminder>\n\nhello"
                .to_string(),
        );
        assert_eq!(
            normalize_user_message(&ClientType::ClaudeCode, &content),
            "hello"
        );
    }

    #[test]
    fn claude_session_topic_unwraps() {
        let content = ChatContent::Text(
            "<session>\nHello! What are you?\n</session>\n\nWrite the title.".to_string(),
        );
        assert_eq!(
            normalize_user_message(&ClientType::ClaudeCode, &content),
            "Hello! What are you?"
        );
    }

    // --- Generic / unknown ---

    #[test]
    fn unknown_bare_text_passes_through() {
        for raw in ["hi", "hello", "list files", "Review src/auth.rs"] {
            let content = ChatContent::Text(raw.to_string());
            assert_eq!(normalize_user_message(&ClientType::Unknown, &content), raw);
        }
    }

    #[test]
    fn unknown_still_unwraps_known_envelopes() {
        let content = ChatContent::Text("<user_input>wrapped</user_input>".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::Unknown, &content),
            "wrapped"
        );
        let content = ChatContent::Text("<user_query>wrapped</user_query>".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::Unknown, &content),
            "wrapped"
        );
    }

    #[test]
    fn unknown_strips_reminder_prefix() {
        let content = ChatContent::Text(
            "<system-reminder>date context</system-reminder>\n\nSay hello".to_string(),
        );
        assert_eq!(
            normalize_user_message(&ClientType::Unknown, &content),
            "Say hello"
        );
    }

    // --- Edge cases ---

    #[test]
    fn null_content_yields_empty() {
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &ChatContent::Null),
            ""
        );
    }

    #[test]
    fn image_parts_are_skipped_not_rendered() {
        // Normalization extracts text only; images travel via image_urls().
        let content = ChatContent::Parts(vec![
            ContentPart {
                part_type: "text".to_string(),
                text: Some("<user_query>look</user_query>".to_string()),
                image_url: None,
                file: None,
            },
            ContentPart {
                part_type: "image_url".to_string(),
                text: None,
                image_url: Some(ImageUrl {
                    url: "data:image/png;base64,AAA".to_string(),
                    detail: None,
                }),
                file: None,
            },
        ]);
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &content),
            "look"
        );
    }

    #[test]
    fn empty_envelope_falls_back() {
        let content = ChatContent::Text("<user_query>   </user_query>".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::WorkBuddy, &content),
            "<user_query>   </user_query>"
        );
    }

    #[test]
    fn dsh_injections_are_dropped() {
        let runtime = ChatContent::Text(
            "Current runtime context. This snapshot supersedes earlier ones.".to_string(),
        );
        assert_eq!(
            normalize_user_message(&ClientType::DeepSeekHarness, &runtime),
            ""
        );
        let skills =
            ChatContent::Text("<system-reminder>\nskills...\n</system-reminder>".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::DeepSeekHarness, &skills),
            ""
        );
        let real = ChatContent::Text("这里是创造模式".to_string());
        assert_eq!(
            normalize_user_message(&ClientType::DeepSeekHarness, &real),
            "这里是创造模式"
        );
    }
}
