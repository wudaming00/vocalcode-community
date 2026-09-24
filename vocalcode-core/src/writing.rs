//! Opt-in writing transforms for one complete, on-release dictation.
//!
//! Independent, deterministic rules, each off unless the person turns it on:
//!
//! * **Spoken formatting** — "new line" / "new paragraph" (换行 / 另起一段)
//!   become line breaks.
//! * **Scratch that** — "scratch that" / "strike that" (删掉上一句) removes the
//!   clause or sentence spoken just before it, inside the same dictation.
//! * **Spoken lists** — "first … second … third …" (第一，… 第二，…) becomes a
//!   numbered list.
//! * **Coding words** — "camel case user id" → `userId`, "snake case …",
//!   "open paren" → `(`, "underscore" → `_`.
//! * **Press enter** — a dictation ending in "press enter" (回车) is typed and
//!   then sent with Enter.
//! * **Style** — Formal leaves punctuation and casing as recognised; Casual
//!   drops the final period of each line, the way people write chat messages;
//!   Very casual also lower-cases ordinary sentence-initial words.
//!
//! These are CPU-only string rules. Nothing here loads a model, guesses a
//! language, or rewrites wording: a phrase that does not match exactly, in
//! exactly the position described, is left alone. Where a rule could plausibly
//! be ordinary prose ("add a new line to the file", "let's scratch that idea")
//! the rule declines — typing a phrase literally is recoverable, deleting what
//! somebody meant to say is not.

use serde::{Deserialize, Serialize};

/// How much punctuation and casing a dictation keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    /// Punctuation and casing exactly as recognised.
    #[default]
    Formal,
    /// No period at the end of each line.
    Casual,
    /// Casual, plus ordinary sentence-initial words in lower case.
    VeryCasual,
}

impl Style {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "formal" => Some(Self::Formal),
            "casual" => Some(Self::Casual),
            "very_casual" => Some(Self::VeryCasual),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formal => "formal",
            Self::Casual => "casual",
            Self::VeryCasual => "very_casual",
        }
    }
}

/// Which rules run for one dictation. Resolved by the host at utterance start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub commands: bool,
    pub backtrack: bool,
    pub lists: bool,
    /// "camel case user id" → `userId`, "open paren" → `(`.
    pub code: bool,
    /// A dictation that ends with "press enter" (回车) is sent afterwards.
    pub press_enter: bool,
    pub style: Style,
}

impl Options {
    pub fn is_noop(&self) -> bool {
        !self.commands
            && !self.backtrack
            && !self.lists
            && !self.code
            && !self.press_enter
            && self.style == Style::Formal
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub text: String,
    /// Number of individual rule applications (0 = text unchanged).
    pub edits: u32,
    /// The dictation ended with a spoken "press enter": deliver the text,
    /// then press Enter in the same field.
    pub send: bool,
}

/// Inputs larger than this are returned unchanged. Dictation is far shorter;
/// the bound only keeps the quadratic-in-matches rebuilds trivially cheap.
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Upper bound on command applications per utterance.
const MAX_EDITS: u32 = 64;

pub fn apply(text: &str, language: &str, options: &Options) -> Written {
    let unchanged = || Written {
        text: text.to_string(),
        edits: 0,
        send: false,
    };
    if options.is_noop() || text.is_empty() || text.len() > MAX_INPUT_BYTES {
        return unchanged();
    }
    let english = is_english(language);
    let chinese = crate::fillers::is_chinese(language);
    let mut out = text.to_string();
    let mut edits = 0;
    let mut send = false;
    // Decided on the text as spoken, before anything else rearranges its end.
    if options.press_enter && (english || chinese) {
        if let Some(head) = strip_press_enter(&out, chinese) {
            out = head;
            send = true;
            edits += 1;
        }
    }
    // Order matters: line breaks first, because a spoken "new line" is a
    // boundary "scratch that" must not retract across ("你好，换行，…。删掉上一句。"
    // keeps the greeting); list items are split before the style pass trims
    // their punctuation.
    if options.commands && (english || chinese) && !looks_like_code(&out) {
        let (next, n) = spoken_breaks(&out, english, chinese);
        out = next;
        edits += n;
    }
    if options.backtrack && (english || chinese) {
        let (next, n) = backtrack(&out, english, chinese);
        out = next;
        edits += n;
    }
    if options.lists && (english || chinese) && !looks_like_code(&out) {
        let (next, n) = spoken_list(&out, english, chinese);
        out = next;
        edits += n;
    }
    if options.code && (english || chinese) {
        let (next, n) = code_words(&out);
        out = next;
        edits += n;
    }
    if options.style != Style::Formal {
        let (next, n) = restyle(&out, options.style);
        out = next;
        edits += n;
    }
    if edits == 0 {
        return unchanged();
    }
    Written {
        text: out,
        edits,
        send,
    }
}

/// A selected spoken-language hint, not automatic language detection.
pub fn is_english(language: &str) -> bool {
    language.eq_ignore_ascii_case("en")
        || language
            .get(..3)
            .is_some_and(|s| s.eq_ignore_ascii_case("en-"))
}

fn looks_like_code(text: &str) -> bool {
    text.contains('`') || text.contains("```")
}

const SENTENCE_MARKS: &[char] = &['.', '!', '?', '。', '！', '？', '\n', '…'];
const CLAUSE_MARKS: &[char] = &[',', ';', ':', '，', '；', '：', '、'];

fn is_sentence_mark(c: char) -> bool {
    SENTENCE_MARKS.contains(&c)
}

fn is_clause_mark(c: char) -> bool {
    CLAUSE_MARKS.contains(&c)
}

fn is_mark(c: char) -> bool {
    is_sentence_mark(c) || is_clause_mark(c)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

/// Word tokens with byte spans. A word is a run of alphanumerics, with an
/// apostrophe allowed between two letters ("don't", "it’s").
#[derive(Debug, Clone, Copy)]
struct Tok {
    start: usize,
    end: usize,
}

fn tokens(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (k, &(i, c)) in chars.iter().enumerate() {
        let word = c.is_alphanumeric() && !is_cjk(c)
            || (matches!(c, '\'' | '’')
                && start.is_some()
                && chars
                    .get(k + 1)
                    .is_some_and(|&(_, n)| n.is_alphabetic() && !is_cjk(n)));
        if word {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push(Tok { start: s, end: i });
        }
    }
    if let Some(s) = start {
        out.push(Tok {
            start: s,
            end: text.len(),
        });
    }
    out
}

fn word<'a>(text: &'a str, tok: &Tok) -> &'a str {
    &text[tok.start..tok.end]
}

/// Does `phrase` (lower-case ASCII words) match the tokens starting at `i`,
/// separated only by whitespace? Returns the end byte of the last word.
fn phrase_at(text: &str, toks: &[Tok], i: usize, phrase: &[&str]) -> Option<usize> {
    for (k, expected) in phrase.iter().enumerate() {
        let tok = toks.get(i + k)?;
        if !word(text, tok).eq_ignore_ascii_case(expected) {
            return None;
        }
        if k > 0
            && !text[toks[i + k - 1].end..tok.start]
                .chars()
                .all(char::is_whitespace)
        {
            return None;
        }
    }
    Some(toks[i + phrase.len() - 1].end)
}

/// Last non-whitespace character before `at`, if any.
fn prev_char(text: &str, at: usize) -> Option<char> {
    text[..at].trim_end().chars().next_back()
}

/// First non-whitespace character at or after `at`, if any.
fn next_char(text: &str, at: usize) -> Option<char> {
    text[at..].trim_start().chars().next()
}

/// Begins a clause: at the start of the text or right after punctuation.
fn at_clause_start(text: &str, at: usize) -> bool {
    prev_char(text, at).is_none_or(is_mark)
}

/// Ends a clause: at the end of the text or right before punctuation.
fn at_clause_end(text: &str, at: usize) -> bool {
    next_char(text, at).is_none_or(is_mark)
}

/// English recognisers often drop the punctuation at a pause but still
/// capitalise the word that starts the next sentence ("…the branch Second,
/// run…"). A capital at `at` therefore counts as a sentence start. Lower-case
/// "let's scratch that idea" does not.
fn starts_capitalized(text: &str, at: usize) -> bool {
    text[at..].chars().next().is_some_and(char::is_uppercase)
}

/// The command at `start..end` begins a new clause and the words after it
/// start another, judged by punctuation or, failing that, by capitals.
fn stands_alone_en(text: &str, start: usize, end: usize) -> bool {
    let begins = at_clause_start(text, start) || starts_capitalized(text, start);
    let rest = text[end..].trim_start();
    let ends = at_clause_end(text, end)
        || (text[end..].starts_with(char::is_whitespace) && starts_capitalized(rest, 0));
    begins && ends
}

/// Byte offset just past punctuation and whitespace that directly follow `at`.
fn skip_marks_and_space(text: &str, at: usize) -> usize {
    let rest = &text[at..];
    let trimmed = rest.trim_start_matches(|c: char| c.is_whitespace() || is_mark(c) && c != '\n');
    at + (rest.len() - trimmed.len())
}

fn capitalize_first_word(text: &str) -> String {
    // Only an all-lower-case alphabetic word: never touch "iPhone", "npm", …
    // except that npm is all lower case too — a plain word is the best proxy
    // available without a lexicon, and a sentence start is where English
    // capitalises regardless.
    let trimmed = text.trim_start();
    let lead = text.len() - trimmed.len();
    let end = trimmed
        .find(|c: char| !c.is_alphabetic())
        .unwrap_or(trimmed.len());
    let first = &trimmed[..end];
    if first.is_empty() || !first.chars().all(|c| c.is_ascii_lowercase()) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..lead]);
    out.push_str(&first[..1].to_ascii_uppercase());
    out.push_str(&trimmed[1..]);
    out
}

// ---------------------------------------------------------------- backtrack

const EN_BACKTRACK: &[&[&str]] = &[&["scratch", "that"], &["strike", "that"]];
const ZH_BACKTRACK: &[&str] = &["删掉上一句", "删除上一句", "上一句删掉", "撤回上一句"];

/// Find the first retraction command that stands as its own clause.
fn find_backtrack(text: &str, english: bool, chinese: bool) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    if english || chinese {
        let toks = tokens(text);
        for i in 0..toks.len() {
            for phrase in EN_BACKTRACK {
                if let Some(end) = phrase_at(text, &toks, i, phrase) {
                    let start = toks[i].start;
                    if stands_alone_en(text, start, end) {
                        best = Some((start, end));
                        break;
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }
    }
    if chinese {
        for phrase in ZH_BACKTRACK {
            let mut from = 0;
            while let Some(found) = text[from..].find(phrase) {
                let start = from + found;
                let end = start + phrase.len();
                if at_clause_end(text, end) && best.is_none_or(|(s, _)| start < s) {
                    best = Some((start, end));
                    break;
                }
                from = end;
            }
        }
    }
    best
}

fn backtrack(text: &str, english: bool, chinese: bool) -> (String, u32) {
    let mut out = text.to_string();
    let mut edits = 0;
    while edits < MAX_EDITS {
        let Some((start, end)) = find_backtrack(&out, english, chinese) else {
            break;
        };
        // Trim spaces only: a line break right before the command is itself
        // the boundary it follows.
        let before = out[..start].trim_end_matches([' ', '\t']);
        // What the command retracts depends on the punctuation it followed:
        // after a comma, the clause since the previous mark; after a full stop
        // or a line break, the sentence since the previous one.
        let cut = match before.chars().next_back() {
            None => 0,
            Some(d) => {
                let head = &before[..before.len() - d.len_utf8()];
                let boundary = if is_clause_mark(d) {
                    head.rfind(is_mark)
                } else {
                    head.rfind(is_sentence_mark)
                };
                boundary
                    .map(|b| b + head[b..].chars().next().map_or(1, char::len_utf8))
                    .unwrap_or(0)
            }
        };
        // Keep a line break that ends the kept part; it was spoken, not retracted.
        let prefix = out[..cut].trim_end_matches([' ', '\t']).to_string();
        let rest = out[skip_marks_and_space(&out, end)..].to_string();
        let cjk_rest = rest.chars().next().is_some_and(is_cjk);
        let mut next = prefix.clone();
        if rest.is_empty() {
            // Nothing follows: a dangling comma becomes the sentence end.
            if let Some(last) = next.chars().next_back().filter(|c| is_clause_mark(*c)) {
                next.truncate(next.len() - last.len_utf8());
                next.push(if is_cjk_punct(last) { '。' } else { '.' });
            }
        } else if prefix.is_empty() {
            next = if english {
                capitalize_first_word(&rest)
            } else {
                rest
            };
        } else {
            let sentence_end = prefix.chars().next_back().is_some_and(is_sentence_mark);
            if !cjk_rest
                && !prefix.ends_with('\n')
                && !prefix.ends_with(|c: char| is_cjk(c) || is_cjk_punct(c))
            {
                next.push(' ');
            }
            if sentence_end && english {
                next.push_str(&capitalize_first_word(&rest));
            } else {
                next.push_str(&rest);
            }
        }
        out = next;
        edits += 1;
    }
    (out, edits)
}

fn is_cjk_punct(c: char) -> bool {
    matches!(c, '，' | '；' | '：' | '、' | '。' | '！' | '？')
}

// ------------------------------------------------------- spoken line breaks

const EN_BREAKS: &[(&[&str], &str)] = &[
    (&["new", "paragraph"], "\n\n"),
    (&["new", "line"], "\n"),
    (&["newline"], "\n"),
];
const ZH_BREAKS: &[(&str, &str)] = &[
    ("另起一段", "\n\n"),
    ("新段落", "\n\n"),
    ("新的一段", "\n\n"),
    ("另起一行", "\n"),
    ("换行", "\n"),
];

/// Words that make "new line" a noun phrase or a topic rather than a command:
/// "add a new line", "the new line character", "new line between them".
const EN_BREAK_PREV_BLOCK: &[&str] = &[
    "a",
    "an",
    "the",
    "this",
    "that",
    "these",
    "those",
    "another",
    "each",
    "every",
    "per",
    "one",
    "no",
    "any",
    "some",
    "extra",
    "blank",
    "empty",
    "trailing",
    "leading",
    "single",
    "double",
    "last",
    "final",
    "next",
    "previous",
    "of",
    "add",
    "adds",
    "added",
    "adding",
    "insert",
    "inserts",
    "inserted",
    "inserting",
    "with",
    "without",
    "remove",
    "removes",
    "removed",
    "removing",
    "delete",
    "deletes",
    "deleted",
    "deleting",
    "print",
    "prints",
    "printed",
    "printing",
    "missing",
    "my",
    "your",
    "his",
    "her",
    "its",
    "our",
    "their",
    "on",
    "into",
    "onto",
    "by",
    "in",
    "for",
    "as",
    "use",
    "uses",
    "using",
    "write",
    "writes",
    "type",
    "types",
    "typed",
    "typing",
    "is",
    "was",
    "are",
    "were",
];
const EN_BREAK_NEXT_BLOCK: &[&str] = &[
    "character",
    "characters",
    "char",
    "chars",
    "break",
    "breaks",
    "ending",
    "endings",
    "feed",
    "feeds",
    "separator",
    "separators",
    "separated",
    "delimited",
    "delimiter",
    "delimiters",
    "mode",
    "symbol",
    "symbols",
    "escape",
    "sequence",
    "sequences",
    "is",
    "was",
    "are",
    "were",
    "of",
    "for",
    "between",
    "and",
    "or",
    "in",
    "at",
    "to",
    "from",
    "that",
    "which",
    "with",
];

struct Span {
    start: usize,
    end: usize,
    insert: &'static str,
}

fn spoken_breaks(text: &str, english: bool, chinese: bool) -> (String, u32) {
    let mut spans: Vec<Span> = Vec::new();
    // English commands are also honoured when Chinese is selected: people who
    // speak Chinese at work say "new line" in English, and it only ever
    // matches as whole ASCII words.
    if english || chinese {
        let toks = tokens(text);
        let mut i = 0;
        while i < toks.len() {
            let mut matched = false;
            for (phrase, insert) in EN_BREAKS {
                let Some(end) = phrase_at(text, &toks, i, phrase) else {
                    continue;
                };
                let prev_word = i
                    .checked_sub(1)
                    .filter(|&p| {
                        text[toks[p].end..toks[i].start]
                            .chars()
                            .all(char::is_whitespace)
                    })
                    .map(|p| word(text, &toks[p]).to_ascii_lowercase());
                let after = i + phrase.len();
                let next_word = toks
                    .get(after)
                    .filter(|t| text[end..t.start].chars().all(char::is_whitespace))
                    .map(|t| word(text, t).to_ascii_lowercase());
                if prev_word
                    .as_deref()
                    .is_some_and(|w| EN_BREAK_PREV_BLOCK.contains(&w))
                    || next_word
                        .as_deref()
                        .is_some_and(|w| EN_BREAK_NEXT_BLOCK.contains(&w))
                {
                    continue;
                }
                spans.push(Span {
                    start: toks[i].start,
                    end,
                    insert,
                });
                i = after;
                matched = true;
                break;
            }
            if !matched {
                i += 1;
            }
        }
    }
    if chinese {
        for (phrase, insert) in ZH_BREAKS {
            let mut from = 0;
            while let Some(found) = text[from..].find(phrase) {
                let start = from + found;
                let end = start + phrase.len();
                from = end;
                // A Chinese command must be its own clause: "这里需要换行"
                // is a sentence about line breaks, "…，换行，…" is a command.
                if !(at_clause_start(text, start) && at_clause_end(text, end)) {
                    continue;
                }
                if spans.iter().any(|s| start < s.end && s.start < end) {
                    continue;
                }
                spans.push(Span { start, end, insert });
            }
        }
    }
    if spans.is_empty() {
        return (text.to_string(), 0);
    }
    spans.sort_by_key(|s| s.start);
    spans.truncate(MAX_EDITS as usize);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let edits = spans.len() as u32;
    for span in &spans {
        if span.start < cursor {
            continue;
        }
        out.push_str(text[cursor..span.start].trim_end_matches([' ', '\t']));
        out.push_str(span.insert);
        cursor = skip_marks_and_space(text, span.end);
        if english || !text[cursor..].starts_with(|c: char| is_cjk(c)) {
            let rest = capitalize_first_word(&text[cursor..]);
            // Re-borrow through the capitalised copy only for its first word.
            let first_len = text[cursor..]
                .find(|c: char| !c.is_alphabetic())
                .unwrap_or(text.len() - cursor);
            out.push_str(&rest[..first_len]);
            cursor += first_len;
        }
    }
    out.push_str(&text[cursor..]);
    (out, edits)
}

// ------------------------------------------------------------ spoken lists

const EN_ORDINALS: &[&[&str]] = &[
    &["first", "firstly"],
    &["second", "secondly"],
    &["third", "thirdly"],
    &["fourth", "fourthly"],
    &["fifth", "fifthly"],
    &["sixth"],
    &["seventh"],
    &["eighth"],
    &["ninth"],
    &["tenth"],
];
const EN_NUMBERS: &[&str] = &[
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];
const ZH_ORDINALS: &[&str] = &[
    "第一", "第二", "第三", "第四", "第五", "第六", "第七", "第八", "第九", "第十",
];

/// A located list marker: where it starts, and where the item text begins.
#[derive(Debug, Clone, Copy)]
struct Marker {
    start: usize,
    content: usize,
}

fn en_marker(text: &str, toks: &[Tok], i: usize, n: usize) -> Option<(Marker, usize)> {
    let start = toks[i].start;
    // "…, and third, …" / "…, then second …": a conjunction may sit between
    // the clause mark and the ordinal. The item before sheds it later.
    let after_conjunction = i.checked_sub(1).is_some_and(|p| {
        matches!(
            word(text, &toks[p]).to_ascii_lowercase().as_str(),
            "and" | "then"
        ) && text[toks[p].end..start].chars().all(char::is_whitespace)
            && at_clause_start(text, toks[p].start)
    });
    if !at_clause_start(text, start)
        && !starts_capitalized(text, start)
        && !(n > 0 && after_conjunction)
    {
        return None;
    }
    let (end, used) = if EN_ORDINALS[n]
        .iter()
        .any(|w| word(text, &toks[i]).eq_ignore_ascii_case(w))
    {
        (toks[i].end, 1)
    } else {
        (phrase_at(text, toks, i, &["number", EN_NUMBERS[n]])?, 2)
    };
    // "First of all" and "first things first" are idioms, not list items.
    if n == 0 {
        let following: Vec<String> = toks[i + used..]
            .iter()
            .take(2)
            .map(|t| word(text, t).to_ascii_lowercase())
            .collect();
        if following.first().is_some_and(|w| w == "things")
            || following.iter().map(String::as_str).eq(["of", "all"])
        {
            return None;
        }
    }
    // The marker must be followed by a comma, colon or a space, then words.
    let after = &text[end..];
    let lead = after.len()
        - after
            .trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, ',' | ':' | '.' | '，' | '：')
            })
            .len();
    if lead == 0 {
        return None;
    }
    let content = end + lead;
    if !text[content..].starts_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    Some((Marker { start, content }, i + used))
}

fn en_markers(text: &str) -> Vec<Marker> {
    let toks = tokens(text);
    let mut markers = Vec::new();
    let mut i = 0;
    while i < toks.len() && markers.len() < EN_ORDINALS.len() {
        if let Some((marker, next)) = en_marker(text, &toks, i, markers.len()) {
            markers.push(marker);
            i = next;
        } else {
            i += 1;
        }
    }
    markers
}

fn zh_markers(text: &str) -> Vec<Marker> {
    let mut markers = Vec::new();
    let mut from = 0;
    for ordinal in ZH_ORDINALS {
        let mut found_one = false;
        let mut search = from;
        while let Some(found) = text[search..].find(ordinal) {
            let start = search + found;
            let mut end = start + ordinal.len();
            search = end;
            // "第十" must not be the head of "第十一"; "第一" not of "第一次".
            let rest = &text[end..];
            let rest = if let Some(r) = rest.strip_prefix(['点', '条', '步']) {
                end += rest.len() - r.len();
                r
            } else {
                rest
            };
            if !at_clause_start(text, start) {
                continue;
            }
            let Some(mark) = rest
                .chars()
                .next()
                // A recogniser may close the ordinal with a full stop at the
                // pause ("第二。运行测试"); the ordinal itself still has to
                // open a clause, so "他得了第二。" is not a marker.
                .filter(|c| matches!(c, '，' | ',' | '：' | ':' | '、' | '。' | '.'))
            else {
                continue;
            };
            let content = end + mark.len_utf8();
            let content = content + (text[content..].len() - text[content..].trim_start().len());
            if text[content..].is_empty() {
                continue;
            }
            markers.push(Marker { start, content });
            from = content;
            found_one = true;
            break;
        }
        if !found_one {
            break;
        }
    }
    markers
}

fn trim_item(item: &str, english: bool) -> String {
    let mut item = item
        .trim()
        .trim_end_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ';' | '，' | '；' | '、')
        })
        .to_string();
    if english {
        // "…, and third, …" leaves a dangling conjunction on the item before.
        loop {
            let lower = item.to_ascii_lowercase();
            let Some(cut) = ["and then", "and", "then", "also"]
                .iter()
                .find(|w| {
                    lower.ends_with(*w) && lower[..lower.len() - w.len()].ends_with([' ', ','])
                })
                .map(|w| item.len() - w.len())
            else {
                break;
            };
            item.truncate(cut);
            item = item
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';'))
                .to_string();
        }
    }
    // A single-sentence item loses its full stop, like every hand-written list.
    let inner = item.trim_end_matches(['.', '。']);
    if !inner.contains(is_sentence_mark) {
        item.truncate(inner.len());
    }
    if english {
        capitalize_first_word(&item)
    } else {
        item
    }
}

fn spoken_list(text: &str, english: bool, chinese: bool) -> (String, u32) {
    let markers = if chinese {
        let zh = zh_markers(text);
        if zh.len() >= 2 {
            zh
        } else {
            en_markers(text)
        }
    } else {
        en_markers(text)
    };
    if markers.len() < 2 {
        return (text.to_string(), 0);
    }
    let english_items = english || !chinese;
    let last = markers[markers.len() - 1];
    // The final item runs to the end of its sentence; anything after that is
    // ordinary text that follows the list.
    let tail_at = text[last.content..]
        .find(['.', '!', '?', '。', '！', '？', '\n'])
        .map(|i| {
            let i = last.content + i;
            i + text[i..].chars().next().map_or(1, char::len_utf8)
        })
        .unwrap_or(text.len());
    let mut items = Vec::with_capacity(markers.len());
    for (k, marker) in markers.iter().enumerate() {
        let end = markers.get(k + 1).map_or(tail_at, |m| m.start);
        let item = trim_item(&text[marker.content..end], english_items);
        if item.is_empty() {
            return (text.to_string(), 0);
        }
        items.push(format!("{}. {}", k + 1, item));
    }
    let lead_in = &text[..markers[0].start];
    let mut preamble = lead_in.trim_end().to_string();
    let same_line = !lead_in[preamble.len()..].contains('\n');
    if let Some(last) = preamble
        .chars()
        .next_back()
        .filter(|c| same_line && matches!(c, ',' | '，'))
    {
        preamble.truncate(preamble.len() - last.len_utf8());
        preamble.push(if last == ',' { ':' } else { '：' });
    }
    let mut out = preamble;
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&items.join("\n"));
    let tail = text[tail_at..].trim();
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(tail);
    }
    (out, markers.len() as u32)
}

// ------------------------------------------------------------- press enter

/// Words that turn a trailing "press enter" into an instruction to somebody
/// else: "…open the terminal and press enter", "you should press enter".
const PRESS_ENTER_PREV_BLOCK: &[&str] = &[
    "to", "and", "then", "you", "should", "must", "can", "could", "will", "would", "i", "we",
    "they", "don't", "not", "never", "or", "just",
];

/// The text before a final, standalone "press enter" (回车), or `None`.
fn strip_press_enter(text: &str, chinese: bool) -> Option<String> {
    let body =
        text.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '!' | '。' | '！'));
    let toks = tokens(body);
    if toks.len() >= 2 {
        let i = toks.len() - 2;
        if phrase_at(body, &toks, i, &["press", "enter"]).is_some_and(|end| end == body.len()) {
            let blocked = i
                .checked_sub(1)
                .filter(|&p| {
                    body[toks[p].end..toks[i].start]
                        .chars()
                        .all(char::is_whitespace)
                })
                .is_some_and(|p| {
                    PRESS_ENTER_PREV_BLOCK
                        .contains(&word(body, &toks[p]).to_ascii_lowercase().as_str())
                });
            if !blocked {
                return Some(finish_head(&body[..toks[i].start]));
            }
        }
    }
    if chinese {
        for phrase in ["按回车", "回车"] {
            if let Some(head) = body.strip_suffix(phrase) {
                if prev_char(head, head.len()).is_none_or(is_mark) {
                    return Some(finish_head(head));
                }
            }
        }
    }
    None
}

/// What precedes a removed trailing command: drop the separating comma, keep
/// a real sentence end.
fn finish_head(head: &str) -> String {
    head.trim_end()
        .trim_end_matches([',', ';', ':', '，', '；', '：', '、'])
        .trim_end()
        .to_string()
}

// ------------------------------------------------------------ coding words

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Camel,
    Pascal,
    Snake,
    Kebab,
    Constant,
}

const CASE_MARKERS: &[(&[&str], Case)] = &[
    (&["camel", "case"], Case::Camel),
    (&["caml", "case"], Case::Camel),
    (&["camelcase"], Case::Camel),
    (&["pascal", "case"], Case::Pascal),
    (&["pascalcase"], Case::Pascal),
    (&["snake", "case"], Case::Snake),
    (&["snakecase"], Case::Snake),
    (&["kebab", "case"], Case::Kebab),
    (&["kebabcase"], Case::Kebab),
    (&["constant", "case"], Case::Constant),
    (&["screaming", "snake", "case"], Case::Constant),
];

/// Words that end an identifier: "camel case user id to camel case account
/// id" names two identifiers, not one.
const CASE_STOP: &[&str] = &[
    "to", "and", "or", "in", "on", "for", "with", "from", "the", "a", "an", "is", "of", "as",
    "into", "at", "by", "then", "but", "so", "if", "equals", "plus", "press", "hit", "please",
    "now", "here", "there",
];
const CASE_MAX_WORDS: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Glue {
    /// Spaces on both sides are kept.
    Spaced,
    /// Attaches to the word after it: "(" "#" "$".
    Right,
    /// Attaches to the word before it: ")" "?" ";".
    Left,
    /// Attaches on both sides: "_" "/" "\".
    Both,
}

const SYMBOLS: &[(&[&str], &str, Glue)] = &[
    (&["open", "parenthesis"], "(", Glue::Right),
    (&["open", "paren"], "(", Glue::Right),
    (&["openparen"], "(", Glue::Right),
    (&["left", "paren"], "(", Glue::Right),
    (&["close", "parenthesis"], ")", Glue::Left),
    (&["close", "paren"], ")", Glue::Left),
    (&["closeparen"], ")", Glue::Left),
    (&["right", "paren"], ")", Glue::Left),
    (&["open", "bracket"], "[", Glue::Right),
    (&["close", "bracket"], "]", Glue::Left),
    (&["open", "curly", "brace"], "{", Glue::Right),
    (&["open", "brace"], "{", Glue::Right),
    (&["close", "curly", "brace"], "}", Glue::Left),
    (&["close", "brace"], "}", Glue::Left),
    (&["underscore"], "_", Glue::Both),
    (&["forward", "slash"], "/", Glue::Both),
    (&["backslash"], "\\", Glue::Both),
    (&["backtick"], "`", Glue::Both),
    (&["tilde"], "~", Glue::Both),
    (&["hashtag"], "#", Glue::Right),
    (&["hash", "sign"], "#", Glue::Right),
    (&["at", "sign"], "@", Glue::Right),
    (&["dollar", "sign"], "$", Glue::Right),
    (&["percent", "sign"], "%", Glue::Left),
    (&["equals", "sign"], "=", Glue::Spaced),
    (&["double", "equals"], "==", Glue::Spaced),
    (&["plus", "sign"], "+", Glue::Spaced),
    (&["ampersand"], "&", Glue::Spaced),
    (&["asterisk"], "*", Glue::Spaced),
    (&["vertical", "bar"], "|", Glue::Spaced),
    (&["semicolon"], ";", Glue::Left),
    (&["question", "mark"], "?", Glue::Left),
    (&["exclamation", "mark"], "!", Glue::Left),
    (&["exclamation", "point"], "!", Glue::Left),
];

/// A replacement produced by the coding pass.
struct Piece {
    start: usize,
    end: usize,
    text: String,
    glue: Glue,
}

fn case_join(words: &[String], case: Case) -> String {
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let cap = |w: &str| {
        let mut c = w.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect::<String>())
            .unwrap_or_default()
    };
    match case {
        Case::Camel => lower
            .iter()
            .enumerate()
            .map(|(i, w)| if i == 0 { w.clone() } else { cap(w) })
            .collect(),
        Case::Pascal => lower.iter().map(|w| cap(w)).collect(),
        Case::Snake => lower.join("_"),
        Case::Kebab => lower.join("-"),
        Case::Constant => lower.join("_").to_uppercase(),
    }
}

fn code_words(text: &str) -> (String, u32) {
    let toks = tokens(text);
    let mut pieces: Vec<Piece> = Vec::new();
    let mut i = 0;
    'scan: while i < toks.len() {
        let prev_word = i
            .checked_sub(1)
            .filter(|&p| {
                text[toks[p].end..toks[i].start]
                    .chars()
                    .all(char::is_whitespace)
            })
            .map(|p| word(text, &toks[p]).to_ascii_lowercase());
        for (marker, case) in CASE_MARKERS {
            let Some(marker_end) = phrase_at(text, &toks, i, marker) else {
                continue;
            };
            let mut words = Vec::new();
            let mut j = i + marker.len();
            let mut end = marker_end;
            while j < toks.len() && words.len() < CASE_MAX_WORDS {
                let gap = &text[end..toks[j].start];
                let w = word(text, &toks[j]);
                // A Titlecase word after the first ("…account ID Press …") is
                // the recogniser starting a new clause, not part of the name.
                let titlecase = w.chars().next().is_some_and(char::is_uppercase)
                    && w.chars().count() > 1
                    && w.chars().skip(1).all(|c| !c.is_uppercase());
                if !gap.chars().all(char::is_whitespace)
                    || (!words.is_empty()
                        && (titlecase || CASE_STOP.contains(&w.to_ascii_lowercase().as_str())))
                {
                    break;
                }
                words.push(w.to_string());
                end = toks[j].end;
                j += 1;
            }
            if words.is_empty() {
                continue;
            }
            pieces.push(Piece {
                start: toks[i].start,
                end,
                text: case_join(&words, *case),
                glue: Glue::Spaced,
            });
            i = j;
            continue 'scan;
        }
        if prev_word
            .as_deref()
            .is_none_or(|w| !EN_BREAK_PREV_BLOCK.contains(&w))
        {
            for (phrase, symbol, glue) in SYMBOLS {
                if let Some(end) = phrase_at(text, &toks, i, phrase) {
                    pieces.push(Piece {
                        start: toks[i].start,
                        end,
                        text: (*symbol).to_string(),
                        glue: *glue,
                    });
                    i += phrase.len();
                    continue 'scan;
                }
            }
        }
        i += 1;
    }
    if pieces.is_empty() {
        return (text.to_string(), 0);
    }
    let edits = pieces.len() as u32;
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut glue_next = false;
    for piece in &pieces {
        let mut gap = &text[cursor..piece.start];
        let attach_left = matches!(piece.glue, Glue::Left | Glue::Both);
        if glue_next {
            gap = gap.trim_start_matches([' ', '\t']);
        }
        if attach_left {
            gap = gap.trim_end_matches([' ', '\t']);
        }
        out.push_str(gap);
        if attach_left {
            let trimmed = out.trim_end_matches([' ', '\t']).len();
            out.truncate(trimmed);
        }
        out.push_str(&piece.text);
        glue_next = matches!(piece.glue, Glue::Right | Glue::Both);
        cursor = piece.end;
    }
    let rest = &text[cursor..];
    if glue_next {
        out.push_str(rest.trim_start_matches([' ', '\t']));
    } else {
        out.push_str(rest);
    }
    (out, edits)
}

// -------------------------------------------------------------------- style

fn restyle(text: &str, style: Style) -> (String, u32) {
    let mut edits = 0;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let mut line = line.to_string();
            if style == Style::VeryCasual {
                let (lowered, n) = lower_sentence_starts(&line);
                line = lowered;
                edits += n;
            }
            let trimmed = line.trim_end();
            let keep = trimmed.len();
            if let Some(stripped) = strip_final_period(trimmed) {
                let removed = keep - stripped.len();
                line.replace_range(keep - removed..keep, "");
                edits += 1;
            }
            line
        })
        .collect();
    (lines.join("\n"), edits)
}

/// The line without its closing period, when that period is plainly a
/// sentence end — not an ellipsis, an abbreviation ("U.S.", "e.g."), or a
/// version number.
fn strip_final_period(line: &str) -> Option<&str> {
    if let Some(head) = line.strip_suffix('。') {
        return Some(head);
    }
    let head = line.strip_suffix('.')?;
    if head.ends_with('.') || head.is_empty() {
        return None;
    }
    let last_word = head
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .unwrap_or(head);
    if last_word.contains('.') {
        return None;
    }
    // "…in the U" / list numbering "1." are left alone.
    if last_word.len() == 1
        && last_word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return None;
    }
    Some(head)
}

/// Lower-case a Titlecase word at the start of each sentence: "Sounds good.
/// See you" → "sounds good. see you". Leaves "I", acronyms and mixed case
/// ("GitHub", "iOS") alone.
fn lower_sentence_starts(line: &str) -> (String, u32) {
    let mut out = String::with_capacity(line.len());
    let mut edits = 0;
    let mut sentence_start = true;
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        if sentence_start && c.is_alphabetic() {
            let end = rest
                .find(|c: char| !(c.is_alphabetic() || c == '\'' || c == '’'))
                .unwrap_or(rest.len());
            let w = &rest[..end];
            let mut chars = w.chars();
            let first = chars.next().unwrap_or(c);
            let titlecase = first.is_uppercase()
                && chars.clone().count() > 0
                && chars.all(|c| !c.is_uppercase());
            let pronoun_i = w == "I" || w.starts_with("I'") || w.starts_with("I’");
            if titlecase && !pronoun_i {
                out.extend(first.to_lowercase());
                out.push_str(&w[first.len_utf8()..]);
                edits += 1;
            } else {
                out.push_str(w);
            }
            rest = &rest[end..];
            sentence_start = false;
            continue;
        }
        if c.is_alphanumeric() {
            sentence_start = false;
        } else if matches!(c, '.' | '!' | '?') {
            sentence_start = true;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en(text: &str, o: Options) -> String {
        apply(text, "en", &o).text
    }
    fn zh(text: &str, o: Options) -> String {
        apply(text, "zh", &o).text
    }
    const CMD: Options = Options {
        commands: true,
        backtrack: false,
        lists: false,
        code: false,
        press_enter: false,
        style: Style::Formal,
    };
    const BACK: Options = Options {
        commands: false,
        backtrack: true,
        lists: false,
        code: false,
        press_enter: false,
        style: Style::Formal,
    };
    const LIST: Options = Options {
        commands: false,
        backtrack: false,
        lists: true,
        code: false,
        press_enter: false,
        style: Style::Formal,
    };
    fn style(style: Style) -> Options {
        Options {
            style,
            ..Options::default()
        }
    }

    #[test]
    fn nothing_enabled_is_a_verbatim_passthrough() {
        let w = apply("Hi, new line. Scratch that.", "en", &Options::default());
        assert_eq!(w.text, "Hi, new line. Scratch that.");
        assert_eq!(w.edits, 0);
    }

    #[test]
    fn spoken_line_and_paragraph_breaks() {
        assert_eq!(
            en("Hi John, new line. Thanks for the update.", CMD),
            "Hi John,\nThanks for the update."
        );
        assert_eq!(
            en("Hi team new paragraph the build is green", CMD),
            "Hi team\n\nThe build is green"
        );
        assert_eq!(en("Done. Newline. Next item", CMD), "Done.\nNext item");
        assert_eq!(en("Thanks, new line", CMD), "Thanks,\n");
    }

    #[test]
    fn line_break_phrases_used_as_prose_are_kept() {
        for text in [
            "Add a new line to the config file.",
            "The new line character breaks the parser.",
            "Insert new line at the end.",
            "We need a new paragraph about pricing.",
            "Use a newline between them.",
            "It prints new line and exits.",
        ] {
            assert_eq!(en(text, CMD), text, "{text}");
        }
    }

    #[test]
    fn code_like_utterances_bypass_commands() {
        let text = "Rename `new line` handler, new line, then ship.";
        assert_eq!(en(text, CMD), text);
    }

    #[test]
    fn chinese_breaks_must_stand_alone() {
        assert_eq!(
            zh("你好，换行，谢谢你的更新。", CMD),
            "你好，\n谢谢你的更新。"
        );
        assert_eq!(
            zh("第一段结束。另起一段。第二段开始。", CMD),
            "第一段结束。\n\n第二段开始。"
        );
        assert_eq!(zh("这里需要换行吗？", CMD), "这里需要换行吗？");
        // English commands also work while Chinese is selected.
        assert_eq!(zh("好的，new line，明天见。", CMD), "好的，\n明天见。");
    }

    #[test]
    fn other_languages_are_unchanged_by_commands() {
        let text = "Hola, new line, gracias.";
        assert_eq!(apply(text, "es", &CMD).text, text);
    }

    #[test]
    fn scratch_that_retracts_the_previous_sentence() {
        assert_eq!(
            en(
                "Let's meet Tuesday. Scratch that. Let's meet Wednesday.",
                BACK
            ),
            "Let's meet Wednesday."
        );
        assert_eq!(
            en(
                "The build passed. It's slow. Scratch that. It's fast.",
                BACK
            ),
            "The build passed. It's fast."
        );
    }

    #[test]
    fn scratch_that_after_a_comma_retracts_only_the_clause() {
        assert_eq!(
            en(
                "Let's meet tomorrow, at three, scratch that, at four.",
                BACK
            ),
            "Let's meet tomorrow, at four."
        );
        assert_eq!(en("Bring milk, eggs, strike that.", BACK), "Bring milk.");
    }

    #[test]
    fn scratch_that_inside_prose_is_kept() {
        for text in [
            "Let's scratch that idea for now.",
            "We should strike that balance carefully.",
            "Scratch that itch.",
        ] {
            assert_eq!(en(text, BACK), text, "{text}");
        }
    }

    #[test]
    fn scratch_that_alone_leaves_nothing_to_type() {
        assert_eq!(en("Scratch that.", BACK), "");
        assert_eq!(en("Scratch that. Ship it today.", BACK), "Ship it today.");
    }

    #[test]
    fn chinese_retraction() {
        assert_eq!(
            zh("我们三点开会。删掉上一句。我们四点开会。", BACK),
            "我们四点开会。"
        );
        assert_eq!(
            zh("我想删掉上一句话的错字。", BACK),
            "我想删掉上一句话的错字。"
        );
    }

    #[test]
    fn a_spoken_line_break_is_a_boundary_for_scratch_that() {
        let both = Options {
            commands: true,
            backtrack: true,
            ..Options::default()
        };
        // Found by using the Writing page's own Try-it example.
        assert_eq!(
            zh("你好，换行，今天三点开会。删掉上一句。今天四点开会。", both),
            "你好，\n今天四点开会。"
        );
        assert_eq!(
            en(
                "Hi Sam, new line. It's at three. Scratch that. It's at four.",
                both
            ),
            "Hi Sam,\nIt's at four."
        );
        assert_eq!(
            zh("你好，换行。今天三点开会删掉上一句，今天4点开会。", both),
            "你好，\n今天4点开会。"
        );
    }

    #[test]
    fn repeated_retractions_apply_in_order() {
        assert_eq!(
            en("One. Two. Scratch that. Three. Scratch that. Four.", BACK),
            "One. Four."
        );
    }

    #[test]
    fn ordinal_lists_become_numbered_lines() {
        assert_eq!(
            en(
                "Here's the plan. First, update the README. Second, run the tests. Third, ship it.",
                LIST
            ),
            "Here's the plan.\n1. Update the README\n2. Run the tests\n3. Ship it"
        );
        assert_eq!(
            en(
                "Todo, first buy milk, second call mom, and third pay rent. Thanks.",
                LIST
            ),
            "Todo:\n1. Buy milk\n2. Call mom\n3. Pay rent\nThanks."
        );
        assert_eq!(
            en("Number one, fix login. Number two, add tests.", LIST),
            "1. Fix login\n2. Add tests"
        );
    }

    #[test]
    fn an_ordinal_closed_by_a_full_stop_still_opens_an_item() {
        // Live microphone run, 2026-09-23: the recogniser put 。 after 第二.
        assert_eq!(
            zh(
                "明天要做三件事，第一，合并分支。第二。运行测试。第三，发布新版本。",
                LIST
            ),
            "明天要做三件事：\n1. 合并分支\n2. 运行测试\n3. 发布新版本"
        );
        assert_eq!(
            en("Plan. First. Pull the branch. Second. Run the tests.", LIST),
            "Plan.\n1. Pull the branch\n2. Run the tests"
        );
        assert_eq!(
            zh("他得了第二。我们很高兴。", LIST),
            "他得了第二。我们很高兴。"
        );
    }

    #[test]
    fn a_lone_or_idiomatic_ordinal_is_not_a_list() {
        for text in [
            "First, I want to thank everyone.",
            "First of all, thanks. Second, the budget.",
            "I came first and you came second.",
            "At first it failed, second attempt worked.",
        ] {
            assert_eq!(en(text, LIST), text, "{text}");
        }
    }

    #[test]
    fn chinese_ordinal_lists() {
        assert_eq!(
            zh(
                "今天要做三件事：第一，买牛奶；第二，给妈妈打电话；第三，交房租。就这些。",
                LIST
            ),
            "今天要做三件事：\n1. 买牛奶\n2. 给妈妈打电话\n3. 交房租\n就这些。"
        );
        assert_eq!(
            zh("这是第一次，也是第二次。", LIST),
            "这是第一次，也是第二次。"
        );
    }

    #[test]
    fn casual_drops_only_a_plain_final_period() {
        assert_eq!(
            en("Sounds good. See you at five.", style(Style::Casual)),
            "Sounds good. See you at five"
        );
        assert_eq!(
            en("Are you coming?", style(Style::Casual)),
            "Are you coming?"
        );
        assert_eq!(en("Wait...", style(Style::Casual)), "Wait...");
        assert_eq!(
            en("We shipped to the U.S.", style(Style::Casual)),
            "We shipped to the U.S."
        );
        assert_eq!(
            en("Upgrade to 3.5.", style(Style::Casual)),
            "Upgrade to 3.5."
        );
        assert_eq!(zh("好的，明天见。", style(Style::Casual)), "好的，明天见");
        assert_eq!(en("Hi,\nThanks.", style(Style::Casual)), "Hi,\nThanks");
    }

    #[test]
    fn very_casual_lowers_ordinary_sentence_starts() {
        assert_eq!(
            en(
                "Sounds good. See you at GitHub HQ.",
                style(Style::VeryCasual)
            ),
            "sounds good. see you at GitHub HQ"
        );
        assert_eq!(
            en("I think so. OK then.", style(Style::VeryCasual)),
            "I think so. OK then"
        );
        assert_eq!(
            en("I'll be there.", style(Style::VeryCasual)),
            "I'll be there"
        );
        assert_eq!(
            en("GitHub is down.", style(Style::VeryCasual)),
            "GitHub is down"
        );
    }

    #[test]
    fn all_rules_compose() {
        let all = Options {
            commands: true,
            backtrack: true,
            lists: true,
            code: false,
            press_enter: false,
            style: Style::Casual,
        };
        assert_eq!(
            en(
                "Hi Sam, new line. Scratch that. Hey Sam, new line. First, pull. Second, rebase.",
                all
            ),
            "Hey Sam,\n1. Pull\n2. Rebase"
        );
    }

    const CODE: Options = Options {
        commands: false,
        backtrack: false,
        lists: false,
        code: true,
        press_enter: false,
        style: Style::Formal,
    };
    const SEND: Options = Options {
        commands: false,
        backtrack: false,
        lists: false,
        code: false,
        press_enter: true,
        style: Style::Formal,
    };

    #[test]
    fn spoken_identifier_casing() {
        assert_eq!(
            en("Rename camel case user ID to camel case account ID.", CODE),
            "Rename userId to accountId."
        );
        assert_eq!(
            en("Call snake case max retry count", CODE),
            "Call max_retry_count"
        );
        assert_eq!(
            en("Pascal case http client wrapper", CODE),
            "HttpClientWrapper"
        );
        assert_eq!(
            en("Set constant case api base url.", CODE),
            "Set API_BASE_URL."
        );
        assert_eq!(en("kebab case main nav bar", CODE), "main-nav-bar");
        // A marker with nothing after it is left as spoken.
        assert_eq!(en("I prefer camel case.", CODE), "I prefer camel case.");
    }

    #[test]
    fn spoken_symbols_attach_like_code() {
        assert_eq!(
            en("Call fetch open paren url close paren", CODE),
            "Call fetch (url)"
        );
        assert_eq!(en("user underscore id", CODE), "user_id");
        assert_eq!(en("Is it ready question mark", CODE), "Is it ready?");
        assert_eq!(en("Tag it hashtag release", CODE), "Tag it #release");
        assert_eq!(en("x equals sign 5 semicolon", CODE), "x = 5;");
        // Prose that names the symbol is not a command.
        assert_eq!(
            en("Add a question mark there.", CODE),
            "Add a question mark there."
        );
    }

    #[test]
    fn trailing_press_enter_sends() {
        let w = apply("Fix the failing test, press enter.", "en", &SEND);
        assert_eq!((w.text.as_str(), w.send), ("Fix the failing test", true));
        let w = apply("Looks good. Press Enter", "en", &SEND);
        assert_eq!((w.text.as_str(), w.send), ("Looks good.", true));
        let w = apply("好的，就这样。回车。", "zh", &SEND);
        assert_eq!((w.text.as_str(), w.send), ("好的，就这样。", true));
    }

    #[test]
    fn press_enter_as_an_instruction_is_typed() {
        for text in [
            "Open the terminal and press enter.",
            "You should press enter.",
            "Press enter to continue.",
            "我按回车没反应。",
        ] {
            let lang = if text.is_ascii() { "en" } else { "zh" };
            let w = apply(text, lang, &SEND);
            assert_eq!((w.text.as_str(), w.send), (text, false), "{text}");
        }
    }

    #[test]
    fn real_recogniser_output_without_pause_punctuation() {
        // SenseVoice English drops the full stops at pauses but capitalises
        // the next sentence; Chinese glued the command onto the sentence.
        let all = Options {
            commands: true,
            backtrack: true,
            lists: true,
            code: true,
            press_enter: true,
            style: Style::Formal,
        };
        assert_eq!(
            en(
                "Let's meet Tuesday Scratch that Let's meet Wednesday at 10.",
                all
            ),
            "Let's meet Wednesday at 10."
        );
        assert_eq!(
            en(
                "Plan for today First, pull the branch Second, run the tests, third ship it.",
                all
            ),
            "Plan for today\n1. Pull the branch\n2. Run the tests\n3. Ship it"
        );
        assert_eq!(
            zh("你好，换行。今天三点开会删掉上一句，今天4点开会。", all),
            "你好，\n今天4点开会。"
        );
        assert_eq!(
            en("Call fetch Openparen URL Closeparen.", all),
            "Call fetch (URL)."
        );
        assert_eq!(
            en("Rename Caml case user ID to Caml case account ID.", all),
            "Rename userId to accountId."
        );
        assert_eq!(
            en(
                "Rename Caml case user ID to Caml case account ID press En.",
                all
            ),
            "Rename userId to accountId press En."
        );
        // Lower case keeps prose prose even without punctuation.
        assert_eq!(
            en("let's scratch that idea and move on", all),
            "let's scratch that idea and move on"
        );
        assert_eq!(
            en("I came first and you came second", all),
            "I came first and you came second"
        );
    }

    #[test]
    fn style_parses_and_round_trips() {
        for s in [Style::Formal, Style::Casual, Style::VeryCasual] {
            assert_eq!(Style::parse(s.as_str()), Some(s));
        }
        assert_eq!(Style::parse("loud"), None);
    }

    #[test]
    fn oversized_input_is_unchanged() {
        let text = "new line ".repeat(10_000);
        assert_eq!(apply(&text, "en", &CMD).text, text);
    }
}
