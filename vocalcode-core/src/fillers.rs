//! Conservative, CPU-only English/Chinese pause-word removal. This is not
//! grammar correction or language detection. Unknown/other languages fail open.
//! No regex substitutions inside words, no model downloads, no semantic rewrite.

#[derive(Debug, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    pub removed: u32,
}

pub fn clean(text: &str, language: &str) -> Cleaned {
    if is_chinese(language) {
        return clean_chinese(text);
    }
    let unchanged = || Cleaned {
        text: text.into(),
        removed: 0,
    };
    if !(language.eq_ignore_ascii_case("en")
        || language.get(..3).is_some_and(|s| s.eq_ignore_ascii_case("en-")))
        || text.len() > 64 * 1024
        // Preserve code, quoted passages, URLs, addresses and mixed scripts.
        // Skipping the whole utterance is intentional: guessing scope is riskier
        // than leaving a filler in a technical or metalinguistic sentence.
        || text.chars().any(|c| {
            matches!(c, '`' | '"' | '“' | '”' | '[' | ']' | '{' | '}' | '<' | '>'
                | '=' | '/' | '\\' | '@' | '_' | ':' | '\n' | '\r' | '\t'
                | '(' | ')' | '+' | '*' | '|' | '&' | '^' | '%' | '#')
                || (!c.is_ascii() && c.is_alphanumeric())
        })
    {
        return unchanged();
    }
    // Apostrophes inside contractions are fine; quotation delimiters (including
    // unmatched ones) cause a conservative whole-utterance bypass.
    for (i, c) in text.char_indices() {
        if c == '‘'
            || (matches!(c, '\'' | '’')
                && !(text[..i].ends_with(|c: char| c.is_ascii_alphabetic())
                    && text[i + c.len_utf8()..].starts_with(|c: char| c.is_ascii_alphabetic())))
        {
            return unchanged();
        }
    }
    let word_char = |c: char| c.is_alphanumeric() || matches!(c, '\'' | '’' | '-');
    let mut words = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        if word_char(c) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            words.push((s, i));
        }
    }
    if let Some(s) = start {
        words.push((s, text.len()));
    }
    if words.iter().any(|&(a, b)| {
        matches!(
            text[a..b].to_ascii_lowercase().as_str(),
            "word"
                | "words"
                | "token"
                | "variable"
                | "function"
                | "spell"
                | "spelled"
                | "spelling"
                | "called"
                | "command"
                | "literal"
                | "identifier"
                | "unit"
                | "micrometer"
                | "micrometers"
                | "snippet"
                | "return"
                | "const"
                | "var"
                | "def"
                | "printf"
                | "echo"
        )
    }) {
        return unchanged();
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut removed = 0;
    for (index, &(a, b)) in words.iter().enumerate() {
        let word = &text[a..b];
        // All-capitals UM/UH/ERM may be acronyms. Title case is allowed only at
        // the beginning of a sentence, never silently lowercased first.
        let title = matches!(word, "Um" | "Uh" | "Erm" | "Uhm");
        if !matches!(word, "um" | "uh" | "erm" | "uhm") && !title {
            continue;
        }
        if title
            && text[..a]
                .trim_end()
                .chars()
                .next_back()
                .is_some_and(|c| !matches!(c, '.' | '!' | '?'))
        {
            return unchanged();
        }
        // Do not treat a unit after a number (5 um) as a hesitation.
        if index > 0
            && text[words[index - 1].0..words[index - 1].1]
                .chars()
                .any(|c| c.is_ascii_digit())
        {
            continue;
        }
        let left = text[..a].chars().next_back();
        let right = text[b..].chars().next();
        if left.is_some_and(|c| !matches!(c, ' ' | ',' | '.' | '!' | '?' | ';'))
            || right.is_some_and(|c| !matches!(c, ' ' | ',' | '.' | '!' | '?' | ';'))
        {
            continue;
        }
        let mut begin = a;
        let mut end = b;
        // Eat only the filler-associated comma and spaces. Keep sentence-ending
        // punctuation when there is preceding substantive text.
        if text[end..].starts_with(',') {
            end += 1;
            let left = text[..a].trim_end_matches(' ');
            if left.ends_with(',') {
                begin = left.len() - 1;
            }
        }
        while text[end..].starts_with(' ') {
            end += 1;
        }
        spans.push((begin, end));
        removed += 1;
    }
    if removed == 0 {
        return unchanged();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let removed_prefix = spans[0].0 == text.len() - text.trim_start().len();
    for (a, b) in spans {
        out.push_str(&text[cursor..a.max(cursor)]);
        cursor = cursor.max(b);
        if out.ends_with(|c: char| c.is_alphanumeric())
            && text[cursor..].starts_with(|c: char| c.is_alphanumeric())
        {
            out.push(' ');
        }
    }
    out.push_str(&text[cursor..]);
    // Only after a deletion: trim orphan punctuation at edges and remove a
    // dangling comma before sentence punctuation. Leave internal content alone.
    let mut out = out
        .trim()
        .trim_start_matches([',', '.', '!', '?', ';', ' '])
        .trim_end_matches([',', ';', ' '])
        .to_string();
    for mark in ['.', '!', '?'] {
        out = out
            .replace(&format!(", {mark}"), &mark.to_string())
            .replace(&format!(" {mark}"), &mark.to_string())
            .replace(&format!(",{mark}"), &mark.to_string());
    }
    if removed_prefix && out.starts_with(|c: char| c.is_ascii_lowercase()) {
        out.replace_range(..1, &out[..1].to_ascii_uppercase());
    }
    Cleaned { text: out, removed }
}

/// A selected spoken-language hint, not automatic language detection.
pub fn is_chinese(language: &str) -> bool {
    language.eq_ignore_ascii_case("zh")
        || language
            .get(..3)
            .is_some_and(|s| s.eq_ignore_ascii_case("zh-"))
}

fn clean_chinese(text: &str) -> Cleaned {
    let unchanged = || Cleaned {
        text: text.into(),
        removed: 0,
    };
    if text.len() > 64 * 1024
        || text.chars().any(|c| {
            matches!(
                c,
                '`' | '"'
                    | '\''
                    | '‘'
                    | '’'
                    | '“'
                    | '”'
                    | '「'
                    | '」'
                    | '『'
                    | '』'
                    | '《'
                    | '》'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '='
                    | '/'
                    | '\\'
                    | '@'
                    | '_'
                    | ':'
                    | '：'
                    | '\n'
                    | '\r'
                    | '\t'
                    | '+'
                    | '*'
                    | '|'
                    | '&'
                    | '^'
                    | '%'
                    | '#'
            )
        })
        || [
            "语气词",
            "語氣詞",
            "停顿词",
            "停頓詞",
            "这个字",
            "這個字",
            "这个词",
            "這個詞",
            "变量",
            "變量",
            "字符串",
            "字串",
            "命令",
            "拼写",
            "拼寫",
            "词块",
            "詞塊",
            "人名",
        ]
        .iter()
        .any(|term| text.contains(term))
    {
        return unchanged();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let comma = |c: char| matches!(c, ',' | '，' | '、');
    let stop = |c: char| matches!(c, '.' | '。' | '!' | '！' | '?' | '？' | ';' | '；');
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (a, c) = chars[i];
        if !matches!(c, '呃' | '嗯') {
            i += 1;
            continue;
        }
        let first = i;
        while i < chars.len() && chars[i].1 == c {
            i += 1;
        }
        let b = chars.get(i).map(|&(index, _)| index).unwrap_or(text.len());
        let run_len = i - first;
        // Repeated 嗯嗯 is usually an acknowledgement, not a removable stutter.
        if run_len > 4 || (c == '嗯' && run_len != 1) {
            continue;
        }
        let before = text[..a].trim_end_matches(' ');
        let after = text[b..].trim_start_matches(' ');
        let left = before.chars().next_back();
        let right = after.chars().next();
        // SenseVoice rarely brackets a hesitation with commas: it writes
        // "这件事情呃需要再讨论一下" (voice-corpus replay, 2026-09-24). 呃 between
        // Chinese characters is still a pause, except in the word 呃逆
        // (hiccup). 嗯 keeps the stricter comma-only rule: it is often a word.
        let han = |ch: char| matches!(ch as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF);
        let han_word = |ch: char| han(ch) && ch != '逆';
        let glued_pause = c == '呃'
            && (left.is_some_and(han) || right.is_some_and(han_word))
            && left.is_none_or(|ch| han(ch) || comma(ch) || stop(ch))
            && right.is_none_or(|ch| han_word(ch) || comma(ch) || stop(ch));
        if glued_pause {
            // Only the hesitation goes; surrounding punctuation stays, so no
            // two clauses are ever merged.
            spans.push((a, b));
            continue;
        }
        if left.is_some_and(|ch| !(comma(ch) || stop(ch)))
            || right.is_some_and(|ch| !(comma(ch) || stop(ch)))
        {
            continue;
        }
        if c == '嗯' {
            // Never delete an initial/standalone answer, or one after a full
            // stop. Only a single comma-isolated medial hesitation is eligible.
            if !left.is_some_and(comma) || !right.is_some_and(comma) {
                continue;
            }
            let prior = before
                .trim_end_matches(comma)
                .rsplit([',', '，', '。', '.', '！', '!', '？', '?', '；', ';'])
                .next()
                .unwrap_or("");
            let following = after.trim_start_matches(comma).trim_start_matches(' ');
            let following_clause = following
                .split([',', '，', '。', '.', '！', '!', '？', '?', '；', ';'])
                .next()
                .unwrap_or("");
            if prior.chars().filter(|ch| ch.is_alphanumeric()).count() < 2
                || following_clause
                    .chars()
                    .filter(|ch| ch.is_alphanumeric())
                    .count()
                    < 4
                || [
                    "好",
                    "对",
                    "對",
                    "是",
                    "可以",
                    "行",
                    "没问题",
                    "沒問題",
                    "没错",
                    "沒錯",
                    "知道",
                    "我知道",
                    "明白",
                    "我明白",
                    "收到",
                    "了解",
                    "同意",
                ]
                .iter()
                .any(|prefix| following.starts_with(prefix))
                || [
                    "说", "說", "回答", "回复", "回覆", "回应", "回應", "答应", "答應",
                ]
                .iter()
                .any(|term| prior.contains(term))
            {
                continue;
            }
        }
        let mut begin = a;
        let mut end = b + text[b..].len() - after.len();
        if right.is_some_and(comma) {
            // Keep the preceding comma: deleting a hesitation must not merge
            // substantive clauses. Consume its following comma and spaces.
            end += right.unwrap().len_utf8();
            while text[end..].starts_with(' ') {
                end += 1;
            }
        } else if left.is_some_and(comma) {
            begin = before.len() - left.unwrap().len_utf8();
        }
        spans.push((begin, end));
    }
    if spans.is_empty() {
        return unchanged();
    }
    let removed = spans.len() as u32;
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (a, b) in spans {
        out.push_str(&text[cursor..a.max(cursor)]);
        cursor = cursor.max(b);
    }
    out.push_str(&text[cursor..]);
    let out = out
        .trim()
        .trim_start_matches(|c| comma(c) || stop(c) || c == ' ')
        .trim_end_matches(|c| comma(c) || c == ' ')
        .to_string();
    Cleaned { text: out, removed }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chinese_removes_delimited_hesitations_not_substantive_content() {
        for (before, after, count) in [
            ("呃，我想试一下。", "我想试一下。", 1),
            ("呃呃，这次不要改金额。", "这次不要改金额。", 1),
            ("这件事情，嗯，需要再讨论。", "这件事情，需要再讨论。", 1),
            ("我们，嗯，明天再试一下。", "我们，明天再试一下。", 1),
            ("事情定了，呃。", "事情定了。", 1),
            ("呃，呃。", "", 2),
            ("呃，嗯，好。", "嗯，好。", 1),
            ("先修API，呃，再运行测试。", "先修API，再运行测试。", 1),
            (
                "金额，呃，是-1200.50美元，不能批准。",
                "金额，是-1200.50美元，不能批准。",
                1,
            ),
            (
                "這個問題，嗯，需要重新討論。",
                "這個問題，需要重新討論。",
                1,
            ),
            ("呃, 我们再试一次。", "我们再试一次。", 1),
        ] {
            let got = clean(before, "zh");
            assert_eq!(got.text, after, "{before}");
            assert_eq!(got.removed, count);
            assert_eq!(clean(&got.text, "zh").text, got.text);
        }
    }
    #[test]
    fn chinese_keeps_acknowledgements_and_ambiguous_particle_boundaries() {
        for text in [
            "嗯",
            "嗯嗯",
            "嗯嗯嗯",
            "嗯，好。",
            "嗯，我觉得可以。",
            "嗯，我们明天再试。",
            "啊？",
            "好啊",
            "哦，是的",
            "哎呀",
            "嗯哼",
            "这个嗯需要改",
            "你先嗯一下",
            "他嗯了一声",
            "呃逆",
            "金额",
            "这个额不对",
            "然后我们继续，就是这样",
            "他说，嗯，明天还会再来。",
            "需要改动，嗯，可以明天提交。",
            "事情定了，嗯，我知道了。",
            "这件事情，嗯，不。",
            "上次做完了。嗯，现在再试。",
            "嗯，呃",
            "呃呃呃呃呃",
        ] {
            // 呃 in a standalone mixed answer is deliberately allowed to be
            // removed; the meaningful 嗯 must survive it.
            if text == "嗯，呃" {
                assert_eq!(clean(text, "zh").text, "嗯");
                continue;
            }
            assert_eq!(
                clean(text, "zh"),
                Cleaned {
                    text: text.into(),
                    removed: 0
                },
                "{text}"
            );
        }
    }
    /// SenseVoice output from the 2026-09-24 voice-corpus replay: the pause
    /// arrives glued to the sentence, without the commas the rule expected.
    #[test]
    fn chinese_glued_hesitation_from_real_recognition() {
        for (text, expected) in [
            ("这件事情呃需要再讨论一下。", "这件事情需要再讨论一下。"),
            ("这件事情呃，需要再讨论一下。", "这件事情，需要再讨论一下。"),
            ("这件事情，呃需要再讨论一下。", "这件事情，需要再讨论一下。"),
            ("呃我想试一下。", "我想试一下。"),
        ] {
            assert_eq!(clean(text, "zh").text, expected, "{text}");
        }
        for text in ["呃逆", "他一直在呃逆。", "变量呃=3", "这个嗯需要改"] {
            assert_eq!(clean(text, "zh").removed, 0, "{text}");
        }
    }

    #[test]
    fn chinese_keeps_quotes_code_and_discussions_of_words() {
        for text in [
            "他说‘呃，再试一下’",
            "他说\"呃，再试一下\"",
            "「呃，我们继续」",
            "这个字，呃，怎么写",
            "这个词，嗯，需要翻译",
            "删除语气词，呃，然后再试",
            "变量呃=3",
            "`呃`",
            "https://呃.cn",
            "mail@嗯.cn",
            "呃\n我们继续",
            "呃：继续",
        ] {
            assert_eq!(clean(text, "zh").removed, 0, "{text}");
        }
    }
    #[test]
    fn chinese_language_scope_does_not_delete_english_or_other_languages() {
        for lang in ["zh", "zh-CN", "zh-Hans", "zh-Hant", "ZH-tw"] {
            assert_eq!(clean("呃，我们继续。", lang).text, "我们继续。");
            assert_eq!(clean("um we should uh retry", lang).removed, 0);
        }
        for lang in ["en", "ja", "ko", "hi", "de", "auto", ""] {
            assert_eq!(clean("呃，我们继续。", lang).removed, 0);
        }
        let large = "呃，".repeat(20_000);
        assert_eq!(clean(&large, "zh").removed, 0);
    }
    #[test]
    fn removes_only_explicit_pause_words() {
        for (before, after, count) in [
            ("Um, we should, uh, retry.", "We should retry.", 2),
            ("we should uh retry", "we should retry", 1),
            ("uh um erm uhm", "", 4),
            ("We should retry, uh.", "We should retry.", 1),
            (
                "uh, I do not approve 1200 USD.",
                "I do not approve 1200 USD.",
                1,
            ),
        ] {
            let got = clean(before, "en");
            assert_eq!(got.text, after);
            assert_eq!(got.removed, count);
            assert_eq!(clean(&got.text, "en").text, got.text);
        }
    }
    #[test]
    fn meaningful_or_ambiguous_content_is_preserved() {
        for text in [
            "hmm",
            "ah",
            "uh-huh",
            "well I like it you know",
            "aluminum human thumb",
            "Use UM and ERM.",
            "5 um",
            "I said 'um'",
            "He said “um”",
            "He said 'we should uh retry'",
            "The word um is a filler",
            "let um = 3",
            "`uh`",
            "https://um.dev",
            "user@uh.dev",
            "foo_um",
            "我们 um 测试",
            "嗯，啊",
            "uh\nretry",
            "um(foo)",
            "Try Um today",
            "say the snippet um",
        ] {
            assert_eq!(
                clean(text, "en"),
                Cleaned {
                    text: text.into(),
                    removed: 0
                }
            );
        }
    }
    #[test]
    fn language_and_bounds_fail_open() {
        for lang in ["de", "pt", "hi", "zh", "auto", "", "english"] {
            assert_eq!(clean("um zehn Uhr", lang).removed, 0);
        }
        assert_eq!(clean("uh retry", "en-US").removed, 1);
        let large = "uh ".repeat(30_000);
        assert_eq!(clean(&large, "en").removed, 0);
    }

    #[test]
    fn punctuation_repeated_pauses_and_contractions() {
        for (text, expected) in [
            ("Um, uh, we should retry.", "We should retry."),
            (
                "I, um, uh, don't approve -1200 USD.",
                "I don't approve -1200 USD.",
            ),
            ("we should um um retry", "we should retry"),
            ("Um. We should retry.", "We should retry."),
            ("I like apples, uh oranges.", "I like apples, oranges."),
        ] {
            assert_eq!(clean(text, "en").text, expected);
        }
    }

    #[test]
    fn ambiguous_names_and_plain_code_are_not_partially_rewritten() {
        for text in [
            "um Try Um today",
            "uh Um retry",
            "return um",
            "echo uh",
            "we use um + 3",
        ] {
            assert_eq!(clean(text, "en").removed, 0);
        }
    }
}
