use std::collections::HashSet;

use crate::Meeting;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub meeting_id: crate::MeetingId,
    pub title: String,
    pub score: u32,
    pub matched_segment_id: Option<u64>,
    pub matched_at_ms: Option<u64>,
    pub excerpt: String,
}

pub fn search_meetings(query: &str, meetings: &[Meeting]) -> Vec<SearchHit> {
    let query_tokens = tokens(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let query_normalized = normalize(query);
    let mut hits = Vec::new();
    for meeting in meetings {
        let mut best_score = field_score(&meeting.title, &query_tokens, &query_normalized, 10);
        let mut excerpt = meeting.title.clone();
        let mut segment_id = None;
        let mut at_ms = None;

        if let Some(summary) = &meeting.summary {
            for note in summary
                .overview
                .iter()
                .chain(summary.decisions.iter())
                .chain(summary.action_items.iter().map(|item| &item.source))
                .chain(summary.open_questions.iter().map(|item| &item.source))
            {
                let score = field_score(&note.text, &query_tokens, &query_normalized, 6);
                if score > best_score {
                    best_score = score;
                    excerpt = note.text.clone();
                    segment_id = Some(note.segment_id);
                    at_ms = Some(note.at_ms);
                }
            }
        }
        for segment in &meeting.segments {
            let score = field_score(&segment.text, &query_tokens, &query_normalized, 3);
            if score > best_score {
                best_score = score;
                excerpt = segment.text.clone();
                segment_id = Some(segment.id);
                at_ms = Some(segment.start_ms);
            }
        }
        if best_score > 0 {
            hits.push(SearchHit {
                meeting_id: meeting.id.clone(),
                title: meeting.title.clone(),
                score: best_score,
                matched_segment_id: segment_id,
                matched_at_ms: at_ms,
                excerpt: truncate_excerpt(&excerpt, 240),
            });
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.title.cmp(&right.title))
    });
    hits
}

fn field_score(text: &str, query: &HashSet<String>, normalized_query: &str, weight: u32) -> u32 {
    let normalized = normalize(text);
    let exact = (!normalized_query.is_empty() && normalized.contains(normalized_query)) as u32;
    let fields = tokens(text);
    let matches = query.intersection(&fields).count() as u32;
    (matches + exact.saturating_mul(query.len() as u32 + 2)).saturating_mul(weight)
}

fn tokens(text: &str) -> HashSet<String> {
    let mut result = HashSet::new();
    let mut word = String::new();
    let mut cjk = Vec::new();
    for character in text.chars() {
        if character.is_alphanumeric() && !is_cjk(character) {
            word.extend(character.to_lowercase());
        } else {
            if !word.is_empty() {
                result.insert(std::mem::take(&mut word));
            }
            if is_cjk(character) {
                let value = character.to_string();
                result.insert(value);
                cjk.push(character);
            } else {
                add_cjk_bigrams(&mut result, &mut cjk);
            }
        }
    }
    if !word.is_empty() {
        result.insert(word);
    }
    add_cjk_bigrams(&mut result, &mut cjk);
    result
}

fn add_cjk_bigrams(result: &mut HashSet<String>, characters: &mut Vec<char>) {
    for pair in characters.windows(2) {
        result.insert(pair.iter().collect());
    }
    characters.clear();
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn truncate_excerpt(text: &str, max_characters: usize) -> String {
    let mut characters = text.chars();
    let prefix: String = characters.by_ref().take(max_characters).collect();
    if characters.next().is_some() {
        format!("{prefix}\u{2026}")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioRetention, MeetingId, MeetingSource, MeetingStatus, SCHEMA_VERSION};

    fn meeting(title: &str, transcript: &str) -> Meeting {
        Meeting {
            schema_version: SCHEMA_VERSION,
            id: MeetingId::parse("1787796747000-42-7").unwrap(),
            title: title.to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            started_at_ms: 1,
            ended_at_ms: Some(2),
            duration_ms: 1,
            segment_count: 1,
            status: MeetingStatus::Completed,
            source: MeetingSource::Imported {
                file_name: "call.wav".to_string(),
            },
            language: "auto".to_string(),
            audio_retention: AudioRetention::DeleteAfterTranscription,
            speakers: Vec::new(),
            bookmarks: Vec::new(),
            summary: None,
            error: None,
            warnings: Vec::new(),
            end_reason: None,
            filtered_noise_segments: 0,
            segments: vec![crate::TranscriptSegment {
                id: 1,
                start_ms: 500,
                end_ms: 1_000,
                speaker_id: "speaker-1".to_string(),
                source: crate::AudioSource::Imported,
                text: transcript.to_string(),
            }],
        }
    }

    #[test]
    fn searches_title_transcript_and_chinese_bigrams() {
        let meetings = vec![meeting("Planning", "决定下周发布本地会议记录")];
        let title = search_meetings("planning", &meetings);
        assert_eq!(title[0].matched_segment_id, None);
        let transcript = search_meetings("会议记录", &meetings);
        assert_eq!(transcript[0].matched_segment_id, Some(1));
        assert!(search_meetings("cloud upload", &meetings).is_empty());
    }
}
