use std::collections::{HashMap, HashSet};

use crate::{ActionItem, MeetingSummary, NoteRef, OpenQuestion, TranscriptSegment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryOptions {
    pub max_overview: usize,
    pub max_decisions: usize,
    pub max_action_items: usize,
    pub max_open_questions: usize,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            max_overview: 5,
            max_decisions: 10,
            max_action_items: 12,
            max_open_questions: 10,
        }
    }
}

#[derive(Debug, Clone)]
struct Sentence<'a> {
    segment: &'a TranscriptSegment,
    text: String,
    order: usize,
}

/// Builds a deliberately extractive summary. Every returned sentence is copied
/// from the transcript and points back to its source segment and timestamp.
pub fn build_local_summary(
    segments: &[TranscriptSegment],
    options: SummaryOptions,
) -> MeetingSummary {
    let sentences = transcript_sentences(segments);
    let frequencies = token_frequencies(&sentences);

    let overview = select_overview(&sentences, &frequencies, options.max_overview)
        .into_iter()
        .map(note_ref)
        .collect();
    let decisions = select_matching(&sentences, options.max_decisions, is_decision)
        .into_iter()
        .map(note_ref)
        .collect();
    let action_items = select_matching(&sentences, options.max_action_items, is_action)
        .into_iter()
        .map(|sentence| ActionItem {
            text: sentence.text.clone(),
            owner: extract_owner(&sentence.text),
            due: extract_due(&sentence.text),
            source: note_ref(sentence),
        })
        .collect();
    let open_questions = select_matching(&sentences, options.max_open_questions, is_question)
        .into_iter()
        .map(|sentence| OpenQuestion {
            text: sentence.text.clone(),
            source: note_ref(sentence),
        })
        .collect();

    MeetingSummary {
        overview,
        decisions,
        action_items,
        open_questions,
        generated_locally: true,
    }
}

fn transcript_sentences(segments: &[TranscriptSegment]) -> Vec<Sentence<'_>> {
    let mut ordered: Vec<_> = segments.iter().collect();
    ordered.sort_by_key(|segment| (segment.start_ms, segment.id));
    let mut result = Vec::new();
    for segment in ordered {
        for text in split_sentences(&segment.text) {
            let text = text.trim();
            if !text.is_empty() {
                let order = result.len();
                result.push(Sentence {
                    segment,
                    text: text.to_string(),
                    order,
                });
            }
        }
    }
    result
}

fn split_sentences(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        if matches!(
            character,
            '.' | '?' | '!' | ';' | '\u{3002}' | '\u{ff1f}' | '\u{ff01}' | '\u{ff1b}'
        ) {
            let end = offset + character.len_utf8();
            result.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        result.push(&text[start..]);
    }
    result
}

fn token_frequencies(sentences: &[Sentence<'_>]) -> HashMap<String, usize> {
    let mut frequencies = HashMap::new();
    for sentence in sentences {
        let unique: HashSet<_> = search_tokens(&sentence.text).into_iter().collect();
        for token in unique {
            *frequencies.entry(token).or_insert(0) += 1;
        }
    }
    frequencies
}

fn select_overview<'a>(
    sentences: &'a [Sentence<'a>],
    frequencies: &HashMap<String, usize>,
    limit: usize,
) -> Vec<&'a Sentence<'a>> {
    if limit == 0 {
        return Vec::new();
    }
    let mut scored: Vec<_> = sentences
        .iter()
        .filter_map(|sentence| {
            let tokens = search_tokens(&sentence.text);
            if sentence.text.chars().count() < 8 || tokens.len() < 2 || sentence.text.len() > 800 {
                return None;
            }
            let topical: usize = tokens
                .iter()
                .map(|token| frequencies.get(token).copied().unwrap_or_default())
                .sum();
            let position_bonus = 12usize.saturating_sub(sentence.order.min(12));
            Some((
                topical.saturating_mul(10) / tokens.len() + position_bonus,
                sentence,
            ))
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.order.cmp(&right.1.order))
    });
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for (_, sentence) in scored {
        let key = normalize(&sentence.text);
        if seen.insert(key) {
            selected.push(sentence);
            if selected.len() == limit {
                break;
            }
        }
    }
    selected.sort_by_key(|sentence| sentence.order);
    selected
}

fn select_matching<'a>(
    sentences: &'a [Sentence<'a>],
    limit: usize,
    predicate: fn(&str) -> bool,
) -> Vec<&'a Sentence<'a>> {
    if limit == 0 {
        return Vec::new();
    }
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for sentence in sentences {
        if predicate(&sentence.text) && seen.insert(normalize(&sentence.text)) {
            selected.push(sentence);
            if selected.len() == limit {
                break;
            }
        }
    }
    selected
}

fn note_ref(sentence: &Sentence<'_>) -> NoteRef {
    NoteRef {
        segment_id: sentence.segment.id,
        at_ms: sentence.segment.start_ms,
        text: sentence.text.clone(),
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    let lower = text.to_lowercase();
    patterns.iter().any(|pattern| lower.contains(pattern))
}

fn is_decision(text: &str) -> bool {
    if is_question(text)
        || contains_any(
            text,
            &[
                "not decided",
                "haven't decided",
                "have not decided",
                "not approved",
                "not agreed",
                "not sure",
                "undecided",
                "还没有决定",
                "没有决定",
                "未决定",
                "尚未决定",
                "不确定",
                "未确定",
                "没有确定",
                "未获批准",
                "没有同意",
                "尚未同意",
                "不同意",
                "不采用",
                "未采用",
                "没有采用",
                "待定",
            ],
        )
    {
        return false;
    }
    contains_any(
        text,
        &[
            "we decided",
            "we agreed",
            "the decision",
            "approved",
            "go with",
            "will use",
            "决定",
            "确定",
            "达成一致",
            "采用",
            "同意",
            "结论",
        ],
    )
}

fn is_action(text: &str) -> bool {
    if is_question(text)
        || contains_any(
            text,
            &[
                "will not",
                "won't",
                "do not need",
                "don't need",
                "does not need",
                "doesn't need",
                "no need to",
                "not responsible",
                "不需要",
                "无需",
                "不用",
                "不要",
                "不负责",
                "不跟进",
                "不必",
                "取消待办",
                "cancelled action",
                "canceled action",
            ],
        )
    {
        return false;
    }
    contains_any(
        text,
        &[
            "action item",
            "todo",
            "to-do",
            "i will",
            "i'll",
            "we will",
            "we'll",
            "need to",
            "needs to",
            "follow up",
            "负责",
            "行动项",
            "待办",
            "需要完成",
            "需要准备",
            "需要提交",
            "需要发送",
            "需要修复",
            "需要测试",
            "需要跟进",
            "我来",
            "请你",
            "跟进",
        ],
    )
}

fn is_question(text: &str) -> bool {
    let trimmed = text.trim_end_matches(char::is_whitespace);
    let explicit = contains_any(
        trimmed,
        &[
            "open question",
            "待确认",
            "还不确定",
            "需要确认",
            "尚未决定",
            "还没有决定",
            "还没决定",
        ],
    );
    let question_mark = trimmed.ends_with('?') || trimmed.ends_with('\u{ff1f}');
    let meaningful = trimmed
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        >= 3;
    let filler = matches!(
        trimmed,
        "你知道吧？" | "对吧？" | "是吧？" | "好吧？" | "对吗？"
    );
    explicit || (question_mark && meaningful && !filler)
}

fn extract_owner(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    for (prefix, owner) in [
        ("i will ", "I"),
        ("i'll ", "I"),
        ("we will ", "We"),
        ("we'll ", "We"),
        ("我来", "我"),
        ("我们来", "我们"),
    ] {
        if lower.starts_with(prefix) || trimmed.starts_with(prefix) {
            return Some(owner.to_string());
        }
    }
    None
}

fn extract_due(text: &str) -> Option<String> {
    // ASCII-only case folding preserves UTF-8 offsets (Unicode lowercase can
    // expand characters such as U+0130 and previously sliced at the wrong byte).
    let lower = text.to_ascii_lowercase();
    for marker in ["之前", "前完成", "前提交", "前发送"] {
        if let Some(offset) = text.find(marker) {
            if let Some(due) = chinese_due_tail(&text[..offset]) {
                return Some(due);
            }
        }
    }
    for marker in [" by ", " before ", " due ", "截止"] {
        if let Some(offset) = lower.find(marker) {
            let start = offset + marker.len();
            let due = text
                .get(start..)?
                .trim()
                .trim_end_matches(['.', '!', '?', '\u{3002}', '\u{ff01}', '\u{ff1f}']);
            if !due.is_empty() && due.len() <= 96 {
                return Some(due.to_string());
            }
        }
    }
    None
}

fn chinese_due_tail(prefix: &str) -> Option<String> {
    // Keep only an explicit source phrase; never invent a calendar date.
    let markers = [
        "下下周",
        "下周",
        "本周",
        "这周",
        "星期",
        "周",
        "明天",
        "后天",
        "今天",
        "今晚",
        "月底",
        "月末",
    ];
    let mut start = markers
        .iter()
        .filter_map(|marker| prefix.rfind(marker))
        .max()?;
    // Include modifiers instead of returning only "周五" from "下周五".
    for marker in ["下下周", "下周", "本周", "这周"] {
        if let Some(offset) = prefix.rfind(marker) {
            if offset + marker.len() > start {
                start = offset;
            }
        }
    }
    let due = prefix[start..].trim();
    if due.is_empty() || due.len() > 60 || due.contains(['。', '，', ',', ';']) {
        None
    } else {
        Some(due.to_string())
    }
}

fn search_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut cjk_run = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            flush_cjk_bigrams(&mut cjk_run, &mut tokens);
            word.push(character.to_ascii_lowercase());
        } else {
            if word.len() >= 2 && !is_stop_word(&word) {
                tokens.push(std::mem::take(&mut word));
            } else {
                word.clear();
            }
            if is_cjk(character) {
                cjk_run.push(character);
            } else {
                flush_cjk_bigrams(&mut cjk_run, &mut tokens);
            }
        }
    }
    if word.len() >= 2 && !is_stop_word(&word) {
        tokens.push(word);
    }
    flush_cjk_bigrams(&mut cjk_run, &mut tokens);
    tokens
}

fn flush_cjk_bigrams(run: &mut String, tokens: &mut Vec<String>) {
    let characters: Vec<_> = run.chars().collect();
    for pair in characters.windows(2) {
        let token: String = pair.iter().collect();
        if !is_cjk_stop_word(&token) {
            tokens.push(token);
        }
    }
    run.clear();
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff)
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "the" | "and" | "that" | "this" | "with" | "for" | "are" | "was" | "but" | "you" | "our"
    )
}

fn is_cjk_stop_word(word: &str) -> bool {
    matches!(
        word,
        "这个"
            | "就是"
            | "一个"
            | "我们"
            | "你们"
            | "他们"
            | "然后"
            | "所以"
            | "觉得"
            | "可以"
            | "需要"
            | "没有"
            | "什么"
            | "怎么"
            | "来说"
            | "对于"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioSource;

    #[test]
    fn semantic_regressions_do_not_promote_negation_or_drop_questions() {
        for text in [
            "我们还没有决定采用哪个方案。",
            "还不确定是否采用这个方案。",
            "The deployment was not approved.",
        ] {
            assert!(!is_decision(text), "{text}");
        }
        for text in [
            "We will not deploy this release.",
            "我们不需要完成这个任务。",
            "谁负责？",
        ] {
            assert!(!is_action(text), "{text}");
        }
        assert!(is_question("这个问题怎么解决呢？"));
        assert_eq!(extract_due("我来在周五之前完成测试。"), Some("周五".into()));
        assert_eq!(
            extract_due("我来在下周五之前完成测试。"),
            Some("下周五".into())
        );
        assert_eq!(extract_due("小徐负责明天前完成测试。"), Some("明天".into()));
        assert_eq!(
            extract_due("İpek will finish by Friday."),
            Some("Friday".into())
        );
    }

    #[test]
    fn zero_limits_produce_no_notes() {
        let summary = build_local_summary(
            &[segment(
                1,
                "We decided to ship. I will prepare it by Friday. What remains?",
            )],
            SummaryOptions {
                max_overview: 0,
                max_decisions: 0,
                max_action_items: 0,
                max_open_questions: 0,
            },
        );
        assert!(
            summary.overview.is_empty()
                && summary.decisions.is_empty()
                && summary.action_items.is_empty()
                && summary.open_questions.is_empty()
        );
    }

    fn segment(id: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id,
            start_ms: id * 1_000,
            end_ms: id * 1_000 + 900,
            speaker_id: "speaker-1".to_string(),
            source: AudioSource::Imported,
            text: text.to_string(),
        }
    }

    #[test]
    fn summary_is_extractive_and_source_linked() {
        let segments = vec![
            segment(
                1,
                "We decided to ship the desktop build. I will prepare it by Friday.",
            ),
            segment(
                2,
                "What remains untested? The desktop build still needs a recovery test.",
            ),
        ];
        let summary = build_local_summary(&segments, SummaryOptions::default());
        assert!(summary.generated_locally);
        assert_eq!(
            summary.decisions[0].text,
            "We decided to ship the desktop build."
        );
        assert_eq!(summary.decisions[0].segment_id, 1);
        assert_eq!(summary.action_items[0].owner.as_deref(), Some("I"));
        assert_eq!(summary.action_items[0].due.as_deref(), Some("Friday"));
        assert_eq!(summary.open_questions[0].text, "What remains untested?");
        for item in summary.overview {
            assert!(segments
                .iter()
                .any(|segment| segment.text.contains(&item.text)));
        }
    }

    #[test]
    fn chinese_notes_are_detected_without_generating_new_claims() {
        let segments = vec![segment(
            1,
            "我们决定采用本地识别。小徐负责明天前完成测试。还有什么需要确认？",
        )];
        let summary = build_local_summary(&segments, SummaryOptions::default());
        assert_eq!(summary.decisions[0].text, "我们决定采用本地识别。");
        assert_eq!(summary.action_items[0].text, "小徐负责明天前完成测试。");
        assert_eq!(summary.open_questions[0].text, "还有什么需要确认？");
    }

    #[test]
    fn generic_chinese_fillers_are_not_promoted_to_tasks_or_questions() {
        let segments = vec![segment(
            1,
            "这些行业的人很需要。你知道吧？我们需要完成本地录音测试。谁负责？",
        )];
        let summary = build_local_summary(&segments, SummaryOptions::default());
        assert_eq!(summary.action_items.len(), 1);
        assert_eq!(summary.action_items[0].text, "我们需要完成本地录音测试。");
        assert_eq!(summary.open_questions.len(), 1);
        assert_eq!(summary.open_questions[0].text, "谁负责？");
    }
}
