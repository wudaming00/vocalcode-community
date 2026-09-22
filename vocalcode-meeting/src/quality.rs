//! Conservative, language-independent transcript hygiene. Never discard a
//! word merely because it is short: acknowledgements, negations and numbers
//! can change the meaning of a meeting.

pub fn punctuation_only(text: &str) -> bool {
    text.chars().all(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '.' | ','
                    | '!'
                    | '?'
                    | ';'
                    | ':'
                    | '。'
                    | '，'
                    | '！'
                    | '？'
                    | '；'
                    | '：'
                    | '、'
                    | '…'
                    | '·'
                    | '।'
                    | '॥'
                    | '،'
                    | '؟'
            )
    })
}

/// A review hint, NOT a deletion or an ASR confidence score. Long acoustic
/// segments containing very little text are worth checking against the audio.
pub fn sparse_long_segment(text: &str, duration_ms: u64) -> bool {
    duration_ms >= 15_000
        && text.chars().filter(|c| c.is_alphanumeric()).count() <= 8
        && !punctuation_only(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_unambiguous_punctuation() {
        for text in ["", " \n", "。", "...", "，。！？", "…", "। ॥"] {
            assert!(punctuation_only(text), "{text}");
        }
        for text in [
            "不。",
            "是",
            "嗯",
            "OK",
            "No!",
            "はい",
            "아니요",
            "नहीं",
            "7",
            "+",
            "C++",
            "×",
        ] {
            assert!(!punctuation_only(text), "{text}");
        }
    }

    #[test]
    fn short_answers_are_not_quality_failures() {
        assert!(!sparse_long_segment("不。", 700));
        assert!(!sparse_long_segment("OK", 1200));
        assert!(sparse_long_segment("情况刷。", 45_000));
        assert!(!sparse_long_segment("。", 45_000));
    }
}
