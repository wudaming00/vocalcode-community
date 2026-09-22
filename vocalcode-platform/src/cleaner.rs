//! Text cleaners applied on release. The universal one is a lightweight
//! punctuation model (CT-Transformer, zh+en) that runs on CPU on any machine —
//! this is what restores the punctuation/casing Paraformer omits. An optional
//! LLM cleaner can be added later for machines that can run one.

use sherpa_onnx::{OfflinePunctuation, OfflinePunctuationConfig, OfflinePunctuationModelConfig};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::TextCleaner;
use zhconv::{zhconv, Variant};

/// Convert Traditional Chinese to Simplified (Whisper tends to emit Traditional).
/// No-op for non-CJK text. Pure Rust, universal.
pub struct T2sCleaner;

impl TextCleaner for T2sCleaner {
    fn clean(&mut self, text: &str) -> Result<String> {
        Ok(zhconv(text, Variant::ZhHans))
    }
}

/// Collapses runs of 2+ space-separated single letters into an uppercase
/// acronym, fixing spelled acronyms the recognizer emits letter-by-letter
/// ("m c p" → "MCP", "a p i" → "API"). Pure text, universal, no model.
pub struct AcronymCollapser;

impl TextCleaner for AcronymCollapser {
    fn clean(&mut self, text: &str) -> Result<String> {
        let tokens: Vec<&str> = text.split(' ').collect();
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let mut j = i;
            while j < tokens.len() && is_single_letter(tokens[j]) {
                j += 1;
            }
            if j - i >= 2 {
                out.push(tokens[i..j].iter().map(|t| t.to_uppercase()).collect());
                i = j;
            } else {
                out.push(tokens[i].to_string());
                i += 1;
            }
        }
        Ok(out.join(" "))
    }
}

fn is_single_letter(t: &str) -> bool {
    let mut chars = t.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_alphabetic())
}

/// Removes spaces that sit between two spaceless-script (CJK / kana / hangul)
/// characters. SenseVoice inserts spaces at internal morpheme boundaries —
/// "ギット ハブ" for GitHub, "ハブ は 便利" at particle joins — and Japanese has
/// no native inter-word spaces, so every such space is a recognition artifact.
/// Collapsing them both reads correctly and lets the recognition-correction
/// dictionary match its unspaced katakana entries (which run *after* the
/// cleaners). It is added **only on the Japanese route**: Korean writes real
/// word spaces and collapsing hangul-hangul spaces would fuse its words, which
/// is exactly why `wants_cjk_space_collapse` gates this to `ja`.
pub struct JapaneseSpaceCollapser;

impl TextCleaner for JapaneseSpaceCollapser {
    fn clean(&mut self, text: &str) -> Result<String> {
        Ok(collapse_spaceless_script_spaces(text))
    }
}

/// Scripts written without inter-word spaces: CJK ideographs, Japanese kana,
/// and Korean hangul. ASCII, digits, and Latin letters stay word-separated and
/// are deliberately excluded, so an embedded "GitHub react" keeps its space.
fn is_spaceless_script(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x9FFF        // CJK Unified Ideographs (+ Extension A)
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0x2_0000..=0x2_FA1F  // CJK Extension B and beyond
        | 0x3040..=0x30FF      // hiragana + katakana
        | 0x31F0..=0x31FF      // katakana phonetic extensions
        | 0xFF65..=0xFF9F      // halfwidth katakana
        | 0xAC00..=0xD7A3      // hangul syllables
        | 0x1100..=0x11FF      // hangul jamo
        | 0x3130..=0x318F      // hangul compatibility jamo
    )
}

fn collapse_spaceless_script_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let prev = out.chars().next_back();
            let next = chars.get(i + 1).copied();
            if prev.is_some_and(is_spaceless_script) && next.is_some_and(is_spaceless_script) {
                continue; // drop the artifact space between two spaceless chars
            }
        }
        out.push(c);
    }
    out
}

/// Adds punctuation and casing via sherpa-onnx's offline punctuation model.
/// Small + CPU-real-time, so it works on the general/universal build.
pub struct SherpaPunctuator {
    punct: OfflinePunctuation,
}

impl SherpaPunctuator {
    pub fn new(model: &str) -> Result<Self> {
        let punct = OfflinePunctuation::create(&OfflinePunctuationConfig {
            model: OfflinePunctuationModelConfig {
                ct_transformer: Some(model.to_string()),
                debug: false,
                num_threads: 2,
                provider: Some("cpu".to_string()),
            },
        })
        .ok_or_else(|| VocalCodeError::Asr("load punctuation model".to_string()))?;
        Ok(Self { punct })
    }
}

impl TextCleaner for SherpaPunctuator {
    fn clean(&mut self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(text.to_string());
        }
        // If the text already has sentence punctuation (e.g. Whisper output),
        // don't re-punctuate — that would double it up. Just normalize.
        let has_punct = text.chars().any(|c| {
            matches!(
                c,
                '.' | ',' | '?' | '!' | ';' | ':' | '。' | '，' | '？' | '！' | '、'
            )
        });
        let punctuated = if has_punct {
            text.to_string()
        } else {
            self.punct
                .add_punctuation(text)
                .ok_or_else(|| VocalCodeError::Asr("punctuation returned no result".to_string()))?
        };
        Ok(normalize(&punctuated))
    }
}

/// Standalone normalization pass for models that punctuate their own output
/// (Parakeet). Paraformer's chain already normalizes inside [`SherpaPunctuator`];
/// running this again after it is an idempotent no-op, so the app appends it to
/// every chain unconditionally. Before this existed, Parakeet languages skipped
/// normalization entirely — a native Spanish tester's transcript arrived with
/// no space after sentence periods and lowercase sentence starts.
pub struct Normalizer;

impl TextCleaner for Normalizer {
    fn clean(&mut self, text: &str) -> Result<String> {
        Ok(normalize(text))
    }
}

/// Per-mark normalization: a full-width mark becomes ASCII only when it follows
/// an ASCII letter/digit (English context); after Chinese it stays full-width.
/// Then collapse spaces and capitalize sentence starts. Handles mixed
/// zh/en text correctly.
fn normalize(s: &str) -> String {
    let mut mapped = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        let prev_ascii = mapped
            .chars()
            .rev()
            .find(|p| !p.is_whitespace())
            .map(|p| p.is_ascii_alphanumeric())
            .unwrap_or(false);
        match c {
            '，' | '、' if prev_ascii => mapped.push_str(", "),
            '。' if prev_ascii => mapped.push_str(". "),
            '！' if prev_ascii => mapped.push_str("! "),
            '？' if prev_ascii => mapped.push_str("? "),
            '；' if prev_ascii => mapped.push_str("; "),
            '：' if prev_ascii => mapped.push_str(": "),
            _ => mapped.push(c),
        }
    }

    // Collapse runs of ASCII spaces; trim. Also repair a missing space at a
    // sentence boundary the recognizer glued together: lowercase letter,
    // sentence-ender, uppercase letter ("funciona.Simplemente"). The uppercase
    // requirement is what keeps this off "vocalcode.app", "index.html", "e.g."
    // (all lowercase after the dot) and off decimals like "4.6" (digit before
    // the dot, digit after).
    let mut collapsed = String::with_capacity(mapped.len() + 8);
    let mut prev_space = false;
    for c in mapped.chars() {
        if c == ' ' {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            if c.is_uppercase() {
                let mut back = collapsed.chars().rev();
                let ender = back.next();
                let before = back.next();
                if matches!(ender, Some('.') | Some('!') | Some('?'))
                    && before.is_some_and(|b| b.is_lowercase())
                {
                    collapsed.push(' ');
                }
            }
            collapsed.push(c);
            prev_space = false;
        }
    }

    // Capitalize ASCII sentence starts.
    //
    // "Sentence start" has to mean the very next character, not the next ASCII
    // letter however far away. It used to mean the latter, and the flag would
    // sail past any Chinese at the start of the line and land on the first
    // English word inside it: "跑 test" came out as "跑 Test", "打开 vs code" as
    // "打开 Vs code". For a user dictating Chinese with English terms in it —
    // which is the main way this app is used — that fired constantly.
    //
    // So any non-space character consumes the flag, whether or not it could be
    // capitalised. Full-width stops open a new sentence too: after Chinese they
    // survive the mapping above as 。！？, and a following English word really
    // is starting a sentence.
    // `is_alphabetic` rather than `is_ascii_alphabetic` so Spanish/French
    // sentence starts capitalize too ("él" → "Él"). CJK chars pass through
    // to_uppercase unchanged and consume the flag exactly as before.
    //
    // A letter directly glued to the ender ("index.html", "vocalcode.app") is
    // NOT a sentence start: the flag only survives into a letter across
    // whitespace (or at the very start of the text). Real glued sentence
    // boundaries were already repaired with a space above, so they still
    // capitalize.
    let mut out = String::with_capacity(collapsed.len());
    let mut cap_next = true;
    let mut prev: Option<char> = None;
    for c in collapsed.trim().chars() {
        // Full-width stops count as boundaries themselves: Chinese has no
        // spaces, so "你好。hello" really is a new sentence at the 'h'.
        let at_boundary = prev.is_none_or(|p| p.is_whitespace() || matches!(p, '。' | '！' | '？'));
        if cap_next && c.is_alphabetic() && at_boundary {
            out.extend(c.to_uppercase());
            cap_next = false;
        } else {
            out.push(c);
            if !c.is_whitespace() {
                cap_next = false;
            }
        }
        if matches!(c, '.' | '!' | '?' | '。' | '！' | '？') {
            cap_next = true;
        }
        prev = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acronyms(s: &str) -> String {
        AcronymCollapser.clean(s).unwrap()
    }

    fn collapse(s: &str) -> String {
        JapaneseSpaceCollapser.clean(s).unwrap()
    }

    /// SenseVoice space-splits Japanese; the collapser fuses those artifact
    /// spaces so the katakana dictionary entries (unspaced) can match and the
    /// sentence reads as real Japanese.
    #[test]
    fn japanese_artifact_spaces_are_collapsed() {
        assert_eq!(
            collapse("ギット ハブ は 便利 です。"),
            "ギットハブは便利です。"
        );
        assert_eq!(collapse("ジャバ スクリプト"), "ジャバスクリプト");
        assert_eq!(collapse("クローダオ コード"), "クローダオコード");
    }

    /// A space with a non-CJK neighbour is a real separator and stays: an
    /// embedded Latin word, or a digit, keeps its space.
    #[test]
    fn spaces_touching_non_cjk_survive() {
        // The space after the Latin "GitHub" survives (b is not spaceless);
        // the は|便 space between two spaceless chars collapses.
        assert_eq!(collapse("GitHub は 便利"), "GitHub は便利");
        assert_eq!(collapse("パイソン 3 です"), "パイソン 3 です");
        assert_eq!(collapse("hello world"), "hello world");
    }

    /// The collapser fuses hangul-hangul spaces too — which is precisely why it
    /// must never run on the Korean route (`wants_cjk_space_collapse` gates it
    /// to Japanese). This pins that hazard so the gate is never widened by
    /// accident.
    #[test]
    fn hangul_spaces_would_fuse_korean_words_hence_japanese_only() {
        assert_eq!(collapse("기터부 정말 편리해요"), "기터부정말편리해요");
    }

    #[test]
    fn spelled_out_acronyms_collapse() {
        assert_eq!(acronyms("open the m c p server"), "open the MCP server");
        assert_eq!(acronyms("a p i key"), "API key");
        assert_eq!(acronyms("u r l"), "URL");
    }

    /// A lone letter is a word ("a cat"), not a one-letter acronym.
    #[test]
    fn single_letters_are_left_alone() {
        assert_eq!(acronyms("a cat sat"), "a cat sat");
        assert_eq!(acronyms("plan b"), "plan b");
    }

    /// Runs are found anywhere, and the rest of the sentence is untouched.
    #[test]
    fn only_the_run_is_rewritten() {
        assert_eq!(acronyms("the c s v file is here"), "the CSV file is here");
        assert_eq!(acronyms("中文 a p i 混合"), "中文 API 混合");
    }

    /// The collapser cannot tell an acronym from consecutive one-letter words,
    /// so it will join them. Pinned deliberately: it is the known cost of the
    /// rule, and the dictionary is the escape hatch if it ever bites.
    #[test]
    fn consecutive_one_letter_words_are_joined_too() {
        assert_eq!(acronyms("i d like that"), "ID like that");
    }

    #[test]
    fn empty_and_spacing_survive() {
        assert_eq!(acronyms(""), "");
        assert_eq!(acronyms("m c p"), "MCP");
    }

    /// A full-width mark after English becomes ASCII with a space; after Chinese
    /// it stays full-width. This is the whole point of doing it per-mark rather
    /// than with a blanket replace, and mixed-language dictation is the norm here.
    #[test]
    fn punctuation_follows_the_surrounding_language() {
        assert_eq!(normalize("hello，world"), "Hello, world");
        assert_eq!(normalize("你好，世界"), "你好，世界");
        assert_eq!(normalize("跑 test，然后 commit"), "跑 test, 然后 commit");
    }

    /// From the first native Spanish tester report: Parakeet glued a sentence
    /// boundary ("funciona.Simplemente"). Lowercase-ender-uppercase means a
    /// boundary; repair the space. All-lowercase dots (domains, file names,
    /// "e.g.") and decimals must never gain one.
    #[test]
    fn glued_sentence_boundaries_gain_a_space() {
        assert_eq!(
            normalize("no funciona.Simplemente vamos"),
            "No funciona. Simplemente vamos"
        );
        assert_eq!(
            normalize("open vocalcode.app now"),
            "Open vocalcode.app now"
        );
        // Known cost, pinned: without a lexicon, "e.g. " is indistinguishable
        // from a sentence end, so the word after it capitalizes. Dictated
        // speech says "for example"; the dictionary is the escape hatch.
        assert_eq!(
            normalize("see index.html and e.g. this file"),
            "See index.html and e.g. This file"
        );
        assert_eq!(normalize("con 4.6 en vez de 4.8"), "Con 4.6 en vez de 4.8");
    }

    /// Also from that report: lowercase sentence starts. Capitalization must
    /// cross whitespace after an ender — and only whitespace, so a letter glued
    /// to a dot (a file extension) stays lowercase.
    #[test]
    fn sentence_starts_capitalize_across_whitespace_only() {
        assert_eq!(
            normalize("va a ser difícil. simplemente no funciona"),
            "Va a ser difícil. Simplemente no funciona"
        );
        assert_eq!(normalize("hola. él llegó"), "Hola. Él llegó");
        assert_eq!(
            normalize("open index.html please"),
            "Open index.html please"
        );
    }

    /// The same repairs must hold across the other Parakeet scripts, not just
    /// Spanish: Cyrillic and Greek capitalize via Unicode, German's glued
    /// "z.B." becomes the orthographically correct "z. B.", and French accented
    /// capitals work. All 25 Parakeet languages share this one pipeline.
    #[test]
    fn other_parakeet_languages_get_the_same_repairs() {
        // Russian: glued boundary + lowercase sentence start.
        assert_eq!(
            normalize("это не работает.Просто нет"),
            "Это не работает. Просто нет"
        );
        assert_eq!(normalize("привет. это тест"), "Привет. Это тест");
        // Greek.
        assert_eq!(
            normalize("δεν λειτουργεί. δοκίμασε"),
            "Δεν λειτουργεί. Δοκίμασε"
        );
        // German: "z.B." gains the space Duden wants anyway; nouns untouched.
        assert_eq!(normalize("das ist z.B. das Haus"), "Das ist z. B. Das Haus");
        // French: accented capital after a boundary.
        assert_eq!(
            normalize("c'est fini.Après on verra"),
            "C'est fini. Après on verra"
        );
        // Ordinals with digits before the dot never gain a space.
        assert_eq!(normalize("am 3. mai fahren wir"), "Am 3. Mai fahren wir");
    }

    /// The standalone cleaner is what Parakeet chains run; it must be the same
    /// normalize, and running it twice must change nothing (Paraformer chains
    /// normalize inside the punctuator and then again at the chain tail).
    #[test]
    fn normalizer_cleaner_is_idempotent() {
        let once = Normalizer.clean("no funciona.Simplemente vamos").unwrap();
        let twice = Normalizer.clean(&once).unwrap();
        assert_eq!(once, "No funciona. Simplemente vamos");
        assert_eq!(once, twice);
    }

    #[test]
    fn ascii_sentences_are_capitalised() {
        assert_eq!(
            normalize("hello there. how are you?"),
            "Hello there. How are you?"
        );
        assert_eq!(normalize("one. two! three? four"), "One. Two! Three? Four");
    }

    #[test]
    fn runs_of_spaces_collapse_and_edges_trim() {
        assert_eq!(normalize("  too   many  spaces  "), "Too many spaces");
    }

    /// Chinese carries no casing, so a Chinese-only line must come back byte
    /// for byte — a stray capitalisation or space would be visible damage.
    /// The regression this pinned down: the sentence-start flag used to skip
    /// over leading Chinese and capitalise the first English word inside the
    /// line, which is most lines for a bilingual user.
    #[test]
    fn english_inside_a_chinese_sentence_is_not_capitalised() {
        assert_eq!(normalize("跑 test，然后 commit"), "跑 test, 然后 commit");
        assert_eq!(normalize("打开 vs code"), "打开 vs code");
        assert_eq!(normalize("用 git 提交"), "用 git 提交");
    }

    /// But a full stop still starts a new sentence, in either script.
    #[test]
    fn a_full_stop_still_opens_a_new_sentence() {
        assert_eq!(normalize("你好。hello world"), "你好。Hello world");
        assert_eq!(normalize("done. next thing"), "Done. Next thing");
    }

    /// A line that genuinely starts in English is still capitalised.
    #[test]
    fn english_at_the_start_is_still_capitalised() {
        assert_eq!(normalize("hello 世界"), "Hello 世界");
    }

    #[test]
    fn chinese_only_text_is_untouched() {
        assert_eq!(normalize("今天天气很好。"), "今天天气很好。");
    }

    #[test]
    fn traditional_chinese_is_converted_to_simplified() {
        assert_eq!(T2sCleaner.clean("開發").unwrap(), "开发");
        assert_eq!(T2sCleaner.clean("hello").unwrap(), "hello");
    }

    /// Pins both phrase conversion and mixed-language preservation after the
    /// bundled rule source moved from MediaWiki (GPL) to OpenCC (Apache-2.0).
    /// A feature-graph regression must fail behavior tests as well as the
    /// release-time dependency audit.
    #[test]
    fn opencc_simplification_preserves_ascii_and_converts_phrases() {
        assert_eq!(
            T2sCleaner
                .clean("VocalCode 開發團隊使用資料庫與網路")
                .unwrap(),
            "VocalCode 开发团队使用资料库与网路"
        );
        assert_eq!(
            T2sCleaner.clean("滑鼠、軟體、記憶體").unwrap(),
            "滑鼠、软体、记忆体"
        );
        assert_eq!(
            T2sCleaner
                .clean("結果一目瞭然，房間乾乾淨淨，仍談乾坤")
                .unwrap(),
            "结果一目了然，房间干干净净，仍谈乾坤"
        );
    }
}
