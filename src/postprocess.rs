use crate::config::PostprocessingConfig;
use crate::filter::is_hallucination;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy)]
struct SpeechReplacement {
    phrase: &'static str,
    replacement: &'static str,
    adjust_preceding_punct: bool,
}

static SPEECH_REPLACEMENTS: &[SpeechReplacement] = &[
    // Sentence-ending / punctuation-adjusting entries
    SpeechReplacement { phrase: "period", replacement: ".", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "comma", replacement: ",", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "question mark", replacement: "?", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "exclamation mark", replacement: "!", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "exclamation point", replacement: "!", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "colon", replacement: ":", adjust_preceding_punct: true },
    SpeechReplacement { phrase: "semicolon", replacement: ";", adjust_preceding_punct: true },
    // Control characters
    SpeechReplacement { phrase: "new line", replacement: "\n", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "tab", replacement: "\t", adjust_preceding_punct: false },
    // Dashes and connectors
    SpeechReplacement { phrase: "dash dash", replacement: "--", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "dash", replacement: "-", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "hyphen", replacement: "-", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "underscore", replacement: "_", adjust_preceding_punct: false },
    // Grouping delimiters
    SpeechReplacement { phrase: "open parentheses", replacement: "(", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "open parenthesis", replacement: "(", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "open paren", replacement: "(", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "close parentheses", replacement: ")", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "close parenthesis", replacement: ")", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "close paren", replacement: ")", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "open bracket", replacement: "[", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "close bracket", replacement: "]", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "open brace", replacement: "{", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "close brace", replacement: "}", adjust_preceding_punct: false },
    // Special symbols
    SpeechReplacement { phrase: "at symbol", replacement: "@", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "hash", replacement: "#", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "dollar sign", replacement: "$", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "percent", replacement: "%", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "caret", replacement: "^", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "ampersand", replacement: "&", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "asterisk", replacement: "*", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "plus", replacement: "+", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "equals", replacement: "=", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "less than", replacement: "<", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "greater than", replacement: ">", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "slash", replacement: "/", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "backslash", replacement: "\\", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "pipe", replacement: "|", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "tilde", replacement: "~", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "grave", replacement: "`", adjust_preceding_punct: false },
    // Quotes
    SpeechReplacement { phrase: "double quote", replacement: "\"", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "single quote", replacement: "'", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "quote", replacement: "\"", adjust_preceding_punct: false },
    SpeechReplacement { phrase: "apostrophe", replacement: "'", adjust_preceding_punct: false },
];

/// Regex matching any spoken punctuation phrase (longest first to avoid prefix collisions).
static SPEECH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    let mut entries: Vec<&SpeechReplacement> = SPEECH_REPLACEMENTS.iter().collect();
    entries.sort_by(|a, b| b.phrase.len().cmp(&a.phrase.len()));
    let alternates: String = entries
        .iter()
        .map(|e| regex::escape(e.phrase))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!(r"(?i)\b(?P<cmd>{alternates})\b[.!?,;:]*");
    Regex::new(&pattern).expect("valid speech replacement regex")
});

static SPACE_BEFORE_PUNCT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+([,.;:!?])").unwrap());
static OPEN_PAREN_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\( +").unwrap());
static CLOSE_PAREN_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" +\)").unwrap());
static COLLAPSE_SPACES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" +").unwrap());

/// Apply spoken punctuation replacements.
fn apply_spoken_punctuation(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;

    for m in SPEECH_REGEX.find_iter(text) {
        // Append text between matches
        result.push_str(&text[last_end..m.start()]);

        // Find which entry matched (case-insensitive)
        let matched_lower = m.as_str().to_lowercase();
        if let Some(entry) = SPEECH_REPLACEMENTS
            .iter()
            .find(|e| matched_lower.starts_with(e.phrase))
        {
            if entry.adjust_preceding_punct {
                // Strip trailing whitespace (save it)
                let mut trailing_ws = Vec::new();
                while result.ends_with(' ') || result.ends_with('\t') {
                    trailing_ws.push(result.pop().unwrap());
                }
                // Strip existing trailing punctuation
                while result.ends_with(['.', ',', '!', '?', ';', ':'])
                {
                    result.pop();
                }
                result.push_str(entry.replacement);
                // Restore whitespace in original order
                for ch in trailing_ws.into_iter().rev() {
                    result.push(ch);
                }
            } else {
                result.push_str(entry.replacement);
            }
        } else {
            result.push_str(m.as_str());
        }
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}

/// Remove space before left-attaching punctuation, after opening parens.
fn fix_spacing(text: &str) -> String {
    let s = SPACE_BEFORE_PUNCT.replace_all(text, "$1");
    let s = OPEN_PAREN_SPACE.replace_all(&s, "(");
    let s = CLOSE_PAREN_SPACE.replace_all(&s, ")");
    let s = COLLAPSE_SPACES.replace_all(&s, " ");
    s.into_owned()
}

/// Capitalize the first letter after sentence-ending punctuation.
fn capitalize_after_period(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut capitalize_next = true;
    for c in text.chars() {
        if capitalize_next && c.is_alphabetic() {
            for upper in c.to_uppercase() {
                result.push(upper);
            }
            capitalize_next = false;
        } else {
            result.push(c);
            if matches!(c, '.' | '?' | '!') {
                capitalize_next = true;
            } else if !c.is_whitespace() {
                capitalize_next = false;
            }
        }
    }
    result
}

/// Full post-processing pipeline. Returns None for hallucinations.
pub fn process_text(text: &str, config: &PostprocessingConfig) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if config.hallucination_filter && is_hallucination(trimmed) {
        tracing::debug!("filtered hallucination: {trimmed:?}");
        return None;
    }

    let mut result = trimmed.to_string();

    if config.spoken_punctuation {
        result = apply_spoken_punctuation(&result);
    }

    result = fix_spacing(&result);
    result = capitalize_after_period(&result);
    let result = result.trim().to_string();

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> PostprocessingConfig {
        PostprocessingConfig {
            hallucination_filter: true,
            spoken_punctuation: true,
        }
    }

    #[test]
    fn spoken_period() {
        // Raw output preserves trailing space; fix_spacing + trim clean it up
        let r = apply_spoken_punctuation("hello period");
        assert_eq!(fix_spacing(&r).trim(), "hello.");
    }

    #[test]
    fn spoken_comma() {
        let r = apply_spoken_punctuation("yes comma no");
        assert_eq!(fix_spacing(&r), "yes, no");
    }

    #[test]
    fn adjust_preceding_strips_existing_punct() {
        let r = apply_spoken_punctuation("hello. period");
        assert_eq!(fix_spacing(&r).trim(), "hello.");
    }

    #[test]
    fn multi_word_match() {
        let r = apply_spoken_punctuation("what question mark");
        assert_eq!(fix_spacing(&r).trim(), "what?");
    }

    #[test]
    fn spacing_fix() {
        assert_eq!(fix_spacing("hello , world"), "hello, world");
        assert_eq!(fix_spacing("( hello )"), "(hello)");
    }

    #[test]
    fn capitalize() {
        assert_eq!(capitalize_after_period("hello. world"), "Hello. World");
    }

    #[test]
    fn full_pipeline() {
        let config = default_config();
        let r = process_text("hello period how are you question mark", &config);
        assert_eq!(r, Some("Hello. How are you?".to_string()));
    }

    #[test]
    fn filters_hallucination() {
        let config = default_config();
        assert_eq!(process_text("thanks for watching", &config), None);
    }
}
