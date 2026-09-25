//! Language-independent pause boundaries. These are not speech/no-speech
//! decisions: all samples are retained, and continuous speech is never cut
//! merely to meet a time budget.

/// Find a completed pause anywhere in the unprocessed audio, not only at its
/// trailing edge. The returned index partitions the audio without overlap or
/// gaps. `minimum_ms` keeps on-release predecoding coarser than progressive text.
pub fn pause_boundary(samples: &[f32], rate: u32, minimum_ms: u32) -> Option<usize> {
    if rate < 100 || samples.iter().any(|s| !s.is_finite()) {
        return None;
    }
    let minimum = rate as usize * minimum_ms as usize / 1000;
    if samples.len() < minimum {
        return None;
    }
    let frame = rate as usize / 100; // 10 ms, including non-16 kHz captures
    let energies: Vec<f32> = samples
        .chunks_exact(frame)
        .map(|values| {
            let mean = values.iter().sum::<f32>() / values.len() as f32;
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
        })
        .collect();
    let peak = energies.iter().copied().fold(0.0_f32, f32::max);
    // Relative to this phrase so quiet speech isn't treated as silence by a
    // fixed 0.015 RMS threshold. The ceiling prevents a loud click from making
    // ordinary speech into a pause. DC/true silence does not trigger decoding.
    if peak <= 0.00001 {
        return None;
    }
    let quiet = (peak * 0.08).min(0.003);
    let mut quiet_frames = 0;
    let mut voiced_frames = 0;
    for (i, &rms) in energies.iter().enumerate() {
        if rms <= quiet {
            quiet_frames += 1;
        } else {
            quiet_frames = 0;
            voiced_frames += 1;
        }
        let end = (i + 1) * frame;
        if quiet_frames >= 24 && voiced_frames >= 20 && end >= minimum {
            // Include the rest of trailing silence rather than producing a
            // tiny, silence-only final decode at release. An interior pause
            // leaves all following speech for the next chunk.
            return Some(if energies[i + 1..].iter().all(|&rms| rms <= quiet) {
                // Only include a partial last frame when it too is quiet.
                let remainder = &samples[energies.len() * frame..];
                if remainder.iter().all(|v| v.abs() <= quiet) {
                    samples.len()
                } else {
                    end
                }
            } else {
                end
            });
        }
    }
    None
}

/// Chinese characters and Japanese kana: scripts written without spaces
/// between words (Hangul is spaced).
pub fn is_unspaced_script(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30ff | 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x2fa1f)
}

/// Stable phrase boundaries must not glue English sentences together after
/// punctuation. Preserve CJK adjacency, while allowing other spaced scripts
/// (including accented Latin and Hangul) to keep their word separation.
pub fn join_separator(previous: &str, next: &str) -> &'static str {
    let (Some(left), Some(right)) = (previous.chars().last(), next.chars().next()) else {
        return "";
    };
    if left.is_whitespace()
        || right.is_whitespace()
        || is_unspaced_script(left)
        || is_unspaced_script(right)
    {
        return "";
    }
    if (left.is_alphanumeric()
        || matches!(
            left,
            '.' | ','
                | '!'
                | '?'
                | ';'
                | ':'
                | ')'
                | ']'
                | '}'
                | '"'
                | '”'
                | '\''
                | '’'
                | '»'
                | '…'
                | '؟'
                | '،'
                | '؛'
                | '।'
                | '॥'
        ))
        && (right.is_alphanumeric()
            || matches!(right, '(' | '[' | '{' | '"' | '“' | '\'' | '‘' | '«'))
    {
        " "
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, ms: usize, amplitude: f32) -> Vec<f32> {
        (0..rate as usize * ms / 1000)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn catches_an_interior_pause_after_speech_has_resumed_at_multiple_rates() {
        for rate in [8000, 16000, 48000] {
            let mut audio = tone(rate, 1000, 0.1);
            audio.extend(vec![0.; rate as usize * 300 / 1000]);
            audio.extend(tone(rate, 500, 0.1));
            assert_eq!(
                pause_boundary(&audio, rate, 300),
                Some(rate as usize * 1240 / 1000)
            );
        }
    }

    #[test]
    fn preserves_quiet_continuous_speech_and_skips_silence_or_dc() {
        for amplitude in [0.1, 0.004, 0.0001] {
            assert_eq!(
                pause_boundary(&tone(16000, 3000, amplitude), 16000, 300),
                None
            );
        }
        assert_eq!(pause_boundary(&vec![0.; 32000], 16000, 300), None);
        assert_eq!(pause_boundary(&vec![0.1; 32000], 16000, 300), None);
        assert_eq!(pause_boundary(&[f32::NAN; 8000], 16000, 300), None);
        assert_eq!(pause_boundary(&[0.; 8000], 0, 300), None);
    }

    #[test]
    fn coarser_predecode_keeps_short_utterances_whole() {
        let mut audio = tone(16000, 5000, 0.05);
        audio.extend(vec![0.; 16000]);
        assert_eq!(pause_boundary(&audio, 16000, 8000), None);
        assert_eq!(pause_boundary(&audio, 16000, 300), Some(audio.len()));
        audio.extend(tone(16000, 3000, 0.05));
        audio.extend(vec![0.; 4000]);
        assert_eq!(pause_boundary(&audio, 16000, 8000), Some(audio.len()));
    }

    #[test]
    fn spaces_after_english_punctuation_without_inserting_cjk_spaces() {
        for (a, b, sep) in [
            ("Hello.", "Next", " "),
            ("Hello,", "world", " "),
            ("hello", "world", " "),
            ("café", "au lait", " "),
            ("你好。", "世界", ""),
            ("こんにちは", "世界", ""),
            ("안녕", "하세요", " "),
            ("already ", "spaced", ""),
            ("", "first", ""),
            ("word", ",", ""),
        ] {
            assert_eq!(join_separator(a, b), sep, "{a:?} + {b:?}");
        }
    }

    #[test]
    fn keeps_word_spacing_after_quotes_ellipsis_and_indic_or_arabic_punctuation() {
        for (a, b) in [
            ("He said 'wait.'", "Next"),
            ("He said ‘wait.’", "Next"),
            ("Pause…", "Continue"),
            ("«Bonjour»", "Ensuite"),
            ("यह सही है।", "अगला वाक्य"),
            ("هل انتهيت؟", "نعم"),
            ("First.", "‘Second’"),
        ] {
            assert_eq!(join_separator(a, b), " ", "{a:?} + {b:?}");
        }
        // Punctuation-only continuations and CJK boundaries stay adjacent.
        assert_eq!(join_separator("Wait", "."), "");
        assert_eq!(join_separator("不要。", "继续"), "");
        assert_eq!(join_separator("待って。", "次です"), "");
    }
}
