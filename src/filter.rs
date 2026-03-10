/// Phrase-level hallucination blocklist.
/// Applied via substring containment only when transcript length < 40 chars.
const HALLUCINATION_PHRASES: &[&str] = &[
    "thanks for watching",
    "thank you for watching",
    "thanks for listening",
    "thank you for listening",
    "subscribe",
    "like and subscribe",
    "see you next time",
    "the end",
    "silence",
    "no speech",
    "inaudible",
    "[music]",
    "(music)",
];

/// Single-word hallucinations: rejected only when the ENTIRE transcript
/// equals one of these words. Sorted for binary_search.
const HALLUCINATION_WORDS: &[&str] = &[
    "ah", "bye", "goodbye", "hmm", "huh", "i", "oh", "so", "uh", "um", "you",
];

/// Two-tier hallucination filter from talktype.
/// Returns true if text should be dropped.
pub fn is_hallucination(text: &str) -> bool {
    let t = text.to_lowercase();
    let t = t.trim();

    // Gate 1: minimum length
    if t.len() < 3 {
        return true;
    }

    // Gate 2: single-word phantom set (exact equality)
    if HALLUCINATION_WORDS.binary_search(&t).is_ok() {
        return true;
    }

    // Gate 3: phrase blocklist (substring, only for short outputs)
    if t.len() < 40 {
        return HALLUCINATION_PHRASES.iter().any(|phrase| t.contains(phrase));
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_text() {
        assert!(is_hallucination(""));
        assert!(is_hallucination("hi"));
        assert!(is_hallucination("a"));
    }

    #[test]
    fn rejects_phantom_words() {
        assert!(is_hallucination("uh"));
        assert!(is_hallucination("Um"));
        assert!(is_hallucination("YOU"));
        assert!(is_hallucination("hmm"));
    }

    #[test]
    fn rejects_hallucination_phrases() {
        assert!(is_hallucination("Thanks for watching"));
        assert!(is_hallucination("like and subscribe"));
    }

    #[test]
    fn passes_real_text() {
        assert!(!is_hallucination("hello world this is a test"));
        assert!(!is_hallucination("the quick brown fox"));
    }

    #[test]
    fn long_text_skips_phrase_check() {
        // 40+ chars containing "subscribe" should pass
        let long = "please subscribe to our newsletter for the latest updates and news";
        assert!(!is_hallucination(long));
    }
}
