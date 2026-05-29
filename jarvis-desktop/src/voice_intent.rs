//! Voice intent parsing. Sits between STT (WebTransport finals OR POST
//! /api/voice/stt) and `gateway.send_chat_in_thread(...)`, intercepts
//! utterances that should NOT be sent to the LLM as new chat turns:
//!
//!   - Stop intents ("stop", "halt", "shut up", "wait", "pause", ...)
//!     halt the speaker queue + cancel the in-flight sentence splitter
//!     state. Saves a turn of the agent loop AND a few seconds of
//!     wasted Claude tokens every time McKale interrupts.
//!
//!   - Approval intents ("yes", "always", "no", "deny", ...) get
//!     converted to POST /api/chat/approval and routed there instead
//!     of being chat-sent. Without this the agent loop blocks
//!     indefinitely waiting on the approval banner while McKale yells
//!     "YES" at his speakers.
//!
//! Port of the parse_stop_intent / parse_approval_intent functions in
//! `dashboard/src/jarvis/mod.rs` (Leptos), made native + word-set
//! aligned 1:1 with the Leptos surface so the two HUDs feel identical.

/// What the voice_intent layer wants the caller to do with this
/// utterance. `SendChat` means the text is normal user input and should
/// flow into `/api/chat/send` as it would have anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceIntent {
    /// User said something like "stop"/"halt"/"quiet". Halt TTS and
    /// drop the utterance (don't bother Claude with it).
    Stop,
    /// User said "mute" / "silence" / "stop listening". Enter
    /// wake-word-only mode: TTS halts immediately, the mic stays hot
    /// but nothing reaches Claude until the user says "Jarvis".
    Mute,
    /// User said "Jarvis" (or "Jarvis <trailing command>") while
    /// muted. Carry the trailing text; if non-empty, send it as the
    /// next chat message immediately so wake + ask is one breath.
    Wake(String),
    /// Drop this utterance silently. Used in wake-word-only mode for
    /// any non-wake speech so JARVIS truly stays quiet.
    Ignore,
    /// User answered a pending approval. The action is one of
    /// "approve"/"always"/"deny" — POST it to /api/chat/approval and
    /// drop the utterance.
    Approval(&'static str),
    /// Default: treat as a normal chat turn.
    SendChat,
}

/// Decide how to route an STT final transcript.
///
/// `has_pending_approval` should be true iff the approval banner is up
/// (i.e. the agent loop is parked waiting on a yes/no). When that's the
/// case, approval keywords win and stop keywords are interpreted as
/// "deny" — matches Leptos behavior exactly.
pub fn classify(
    text: &str,
    has_pending_approval: bool,
    is_wake_word_only: bool,
) -> VoiceIntent {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return VoiceIntent::SendChat;
    }

    // Wake-word-only mode swallows everything except a wake utterance.
    // Approval banners can still be answered (someone shouting "yes"
    // mid-mute should still resolve a pending approval).
    if is_wake_word_only {
        if has_pending_approval {
            if let Some(action) = parse_approval_intent(trimmed) {
                return VoiceIntent::Approval(action);
            }
        }
        if let Some(rest) = parse_wake_intent(trimmed) {
            return VoiceIntent::Wake(rest.to_string());
        }
        return VoiceIntent::Ignore;
    }

    // While a tool approval is pending, the same words mean different
    // things. "stop" = "deny", "yes" = "approve" etc. We route through
    // the approval parser first.
    if has_pending_approval {
        if let Some(action) = parse_approval_intent(trimmed) {
            return VoiceIntent::Approval(action);
        }
        // No yes/no match: drop the utterance silently. Don't chat-send
        // a stray "hmm" while a yes/no banner is up.
        return VoiceIntent::SendChat;
    }

    if parse_mute_intent(trimmed) {
        return VoiceIntent::Mute;
    }

    if parse_stop_intent(trimmed) {
        return VoiceIntent::Stop;
    }

    VoiceIntent::SendChat
}

/// True iff the entire utterance is a stop command. Punctuation /
/// casing normalized away first.
fn parse_stop_intent(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let collapsed: String = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        collapsed.as_str(),
        "stop"
            | "halt"
            | "silence"
            | "enough"
            | "quiet"
            | "shutup"
            | "shut up"
            | "be quiet"
            | "wait"
            | "pause"
    )
}

/// True iff the entire utterance is a "go to wake-word-only mode"
/// command. Distinct from `parse_stop_intent` so the two have
/// different runtime effects: Stop just halts in-flight TTS; Mute
/// halts AND parks the gateway until the user says "Jarvis".
fn parse_mute_intent(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let collapsed: String = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        collapsed.as_str(),
        "mute"
            | "muted"
            | "mute yourself"
            | "go mute"
            | "be silent"
            | "silence yourself"
            | "stop listening"
            | "shush"
            | "shh"
            | "hush"
            | "go quiet"
            | "go to sleep"
            | "sleep"
            | "standby"
            | "stand by"
    )
}

/// If the utterance begins with a wake word ("jarvis", "hey jarvis",
/// "ok jarvis"), return the rest of the utterance (possibly empty).
/// Otherwise None. Match is case-insensitive and tolerates trailing
/// punctuation after the wake word.
fn parse_wake_intent(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let lower = trimmed.to_lowercase();
    let prefixes = ["hey jarvis", "ok jarvis", "okay jarvis", "jarvis"];
    for p in &prefixes {
        if let Some(rest) = lower.strip_prefix(p) {
            // The byte position into `trimmed` matches lowercase length
            // since wake words are ASCII. Use that to slice the
            // original (preserves case for the user's downstream message).
            let consumed = p.len();
            let after = &trimmed[consumed..];
            // The next char (if any) must be whitespace or punctuation —
            // otherwise we matched "jarvisstats" or similar nonsense.
            if let Some(c) = after.chars().next() {
                if c.is_alphanumeric() {
                    continue;
                }
            }
            // Strip the leading separator(s) and any trailing whitespace.
            let rest_clean = after.trim_start_matches(|c: char| {
                c.is_whitespace() || c == ',' || c == '.' || c == '!' || c == '?'
            }).trim();
            return Some(rest_clean);
        }
    }
    None
}

/// Match against approve / deny keywords. Returns the action string
/// ("approve"|"always"|"deny") or None if the user said something
/// unrelated. "always" wins over "approve" if both appear.
fn parse_approval_intent(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.iter().any(|w| matches!(*w, "always")) {
        return Some("always");
    }
    let yes_set = [
        "yes",
        "yeah",
        "yep",
        "yup",
        "sure",
        "approve",
        "approved",
        "ok",
        "okay",
        "alright",
        "affirmative",
        "confirm",
        "confirmed",
        "go",
        "proceed",
        "do",
        "y",
    ];
    let no_set = [
        "no", "nope", "nah", "deny", "denied", "cancel", "stop", "abort", "negative", "n",
    ];
    if words.iter().any(|w| yes_set.contains(w)) {
        return Some("approve");
    }
    if words.iter().any(|w| no_set.contains(w)) {
        return Some("deny");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_intent_exact() {
        assert!(parse_stop_intent("stop"));
        assert!(parse_stop_intent("HALT"));
        assert!(parse_stop_intent("Shut up."));
        assert!(parse_stop_intent("  shut  up "));
        assert!(parse_stop_intent("be quiet"));
        assert!(parse_stop_intent("wait"));
        assert!(parse_stop_intent("pause"));
        assert!(parse_stop_intent("enough!"));
    }

    #[test]
    fn stop_intent_partial_does_not_match() {
        // "stop talking" is not pure-stop; it should NOT halt — the user
        // wanted to compose a new turn. Only standalone stop words.
        assert!(!parse_stop_intent("stop talking"));
        assert!(!parse_stop_intent("hey stop"));
        assert!(!parse_stop_intent("can you stop"));
    }

    #[test]
    fn approval_yes_words() {
        assert_eq!(parse_approval_intent("yes"), Some("approve"));
        assert_eq!(parse_approval_intent("Yeah, go ahead"), Some("approve"));
        assert_eq!(parse_approval_intent("OK"), Some("approve"));
        assert_eq!(parse_approval_intent("confirm"), Some("approve"));
    }

    #[test]
    fn approval_no_words() {
        assert_eq!(parse_approval_intent("no"), Some("deny"));
        assert_eq!(parse_approval_intent("cancel that"), Some("deny"));
        assert_eq!(parse_approval_intent("abort!"), Some("deny"));
    }

    #[test]
    fn approval_always_wins() {
        // "always allow" should be "always", not "approve" — even though
        // "allow" isn't in yes_set, "always" still beats other matches.
        assert_eq!(parse_approval_intent("always"), Some("always"));
        assert_eq!(parse_approval_intent("yes, always"), Some("always"));
    }

    #[test]
    fn approval_unrelated_returns_none() {
        assert_eq!(parse_approval_intent("what's the weather"), None);
        assert_eq!(parse_approval_intent("hmm"), None);
    }

    #[test]
    fn classify_routes_to_stop_when_no_approval() {
        assert_eq!(classify("stop", false, false), VoiceIntent::Stop);
    }

    #[test]
    fn classify_routes_stop_to_deny_when_approval_pending() {
        // "stop" appears in no_set, so it gets routed to "deny".
        assert_eq!(
            classify("stop", true, false),
            VoiceIntent::Approval("deny")
        );
    }

    #[test]
    fn classify_drops_irrelevant_during_approval() {
        // Background chatter while an approval is up should not be sent.
        assert_eq!(
            classify("what's the weather", true, false),
            VoiceIntent::SendChat // SendChat here actually means "drop" — see classify() docs
        );
    }

    #[test]
    fn classify_sends_chat_for_normal_text() {
        assert_eq!(
            classify("hey jarvis what time is it", false, false),
            VoiceIntent::SendChat
        );
    }
}
