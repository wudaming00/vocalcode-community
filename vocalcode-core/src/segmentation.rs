//! Language-independent pause boundaries. These are not speech/no-speech
//! decisions: all samples are retained, and continuous speech is never cut
//! merely to meet a time budget.

/// Background level of one utterance's room, learned from its own audio.
///
/// The fixed pause ceiling below (0.003 RMS, about -50 dBFS) sits under the
/// floor of an ordinary room: a fan or an office is nearer -46 dBFS, so no
/// pause was ever found there and release latency grew with dictation length.
/// Every second of scanned audio offers its 10th-percentile frame energy and
/// the floor keeps the lowest offer of the utterance. It never rises: a second
/// of unbroken speech has no noise-only frames, its low percentile is quiet
/// *speech*, and on the voice corpus that per-window estimate cut words in two
/// (even in clean recordings). A floor that only falls errs toward the fixed
/// ceiling. Create a new one for every utterance.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NoiseFloor {
    floor: Option<f32>,
    /// Threshold that found the latest pause; see [`only_room`].
    pause: f32,
}

impl NoiseFloor {
    /// One-second blocks: in a long scan (the first one after a slow decode)
    /// a 300 ms pause is under a tenth of the frames. Counted back from the
    /// newest audio, so a pause just heard is always inside a whole block.
    /// A block offers a floor only with half a second of sound in it.
    const BLOCK_FRAMES: usize = 100;
    const MIN_FRAMES: usize = 50;
    /// About -100 dBFS. No microphone in a real room is this quiet; a stream
    /// is, when it delivers zeros. Those frames are left out: counted as the
    /// room, six of them in a second would pin the floor at zero for the rest
    /// of the utterance.
    const DIGITAL_SILENCE: f32 = 0.00001;
    /// Zeros for 150 ms after sound in one scan: a microphone that silences
    /// its own pauses (a noise gate or suppressor). The fixed ceiling finds
    /// those pauses, and every frame it lets through is the speaker, so its
    /// floor is zero. Shorter runs are a dropout, and zeros before any sound
    /// in a scan are a device warming up: every press opens a new stream, and
    /// a Bluetooth headset switching profiles can take hundreds of ms.
    const GATED_FRAMES: usize = 15;

    fn observe(&mut self, energies: &[f32]) {
        let silent = |energy: &f32| *energy <= Self::DIGITAL_SILENCE;
        let sound = energies.iter().position(|e| !silent(e));
        let after_sound = &energies[sound.unwrap_or(energies.len())..];
        if after_sound
            .split(|e| !silent(e))
            .any(|zeros| zeros.len() >= Self::GATED_FRAMES)
        {
            self.floor = Some(0.);
            return;
        }
        for block in energies.rchunks(Self::BLOCK_FRAMES) {
            let mut heard: Vec<f32> = block.iter().copied().filter(|e| !silent(e)).collect();
            if heard.len() < Self::MIN_FRAMES {
                continue;
            }
            let tenth = (heard.len() - 1) / 10;
            let (_, &mut low, _) = heard.select_nth_unstable_by(tenth, f32::total_cmp);
            self.floor = Some(self.floor.map_or(low, |floor| floor.min(low)));
        }
    }

    /// Frames within this factor of the floor are the room, not the speaker.
    /// Pink noise at -46 dBFS keeps 99.9% of its 10 ms frames under 2x its
    /// 10th percentile; 1.5x would break most 240 ms runs of it.
    fn quiet(&self) -> f32 {
        self.floor.unwrap_or(0.) * 2.
    }
}

/// Find a completed pause anywhere in the unprocessed audio, not only at its
/// trailing edge. The returned index partitions the audio without overlap or
/// gaps. `minimum_ms` keeps on-release predecoding coarser than progressive text.
/// `floor` carries the room's noise level across the scans of one utterance.
pub fn pause_boundary(
    samples: &[f32],
    rate: u32,
    minimum_ms: u32,
    floor: &mut NoiseFloor,
) -> Option<usize> {
    if rate < 100 || samples.iter().any(|s| !s.is_finite()) {
        return None;
    }
    let frame = rate as usize / 100; // 10 ms, including non-16 kHz captures
    let energies = frame_energies(samples, frame);
    // Before the minimum check: the quiet lead-in of a dictation is the most
    // reliable view of the room, and on-release scans skip the first 8 s.
    floor.observe(&energies);
    let minimum = rate as usize * minimum_ms as usize / 1000;
    if samples.len() < minimum {
        return None;
    }
    let peak = energies.iter().copied().fold(0.0_f32, f32::max);
    // Relative to this phrase so quiet speech isn't treated as silence by a
    // fixed 0.015 RMS threshold. The ceiling prevents a loud click from making
    // ordinary speech into a pause. DC/true silence does not trigger decoding.
    if peak <= 0.00001 {
        return None;
    }
    // A room louder than the ceiling raises it; quiet rooms and microphones
    // that silence their pauses keep it exactly. Still relative to the
    // phrase: a floor that was learned from speech (no pause since the key
    // went down) cannot turn speech within 22 dB of its peak into a pause.
    let quiet = (peak * 0.08).min(floor.quiet().max(0.003));
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
            // Remember a pause made of room noise, one the fixed ceiling could
            // not have found: its threshold judges what follows until release.
            let noisy = energies[i + 1 - 24..=i].iter().filter(|&&e| e > 0.003);
            floor.pause = if noisy.count() > 12 { quiet } else { 0. };
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

/// Is this audio nothing but more of the pause found last? A pause cut at the
/// end of a scan leaves whatever the room makes until release, and recognizers
/// turn a moment of fan noise into "Yeah." or "我。". It is judged by the very
/// threshold that found that pause, which is still relative to the phrase
/// before it. Silence never needed this: after a pause of silence (a quiet
/// room) the answer is always false and the rest decodes as before. 30 ms of
/// sound above that threshold is someone speaking.
pub fn only_room(samples: &[f32], rate: u32, floor: &NoiseFloor) -> bool {
    let room = floor.pause;
    if rate < 100 || room <= 0.003 || samples.iter().any(|s| !s.is_finite()) {
        return false;
    }
    let mut sounding = 0;
    for rms in frame_energies(samples, rate as usize / 100) {
        sounding = if rms > room { sounding + 1 } else { 0 };
        if sounding >= 3 {
            return false;
        }
    }
    true
}

/// DC-free RMS of every complete frame.
fn frame_energies(samples: &[f32], frame: usize) -> Vec<f32> {
    samples
        .chunks_exact(frame)
        .map(|values| {
            let mean = values.iter().sum::<f32>() / values.len() as f32;
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
        })
        .collect()
}

/// Stable phrase boundaries must not glue English sentences together after
/// punctuation. Preserve CJK adjacency, while allowing other spaced scripts
/// (including accented Latin and Hangul) to keep their word separation.
pub fn join_separator(previous: &str, next: &str) -> &'static str {
    let (Some(left), Some(right)) = (previous.chars().last(), next.chars().next()) else {
        return "";
    };
    let cjk = |c: char| {
        matches!(c as u32,
        0x3040..=0x30ff | 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x2fa1f)
    };
    if left.is_whitespace() || right.is_whitespace() || cjk(left) || cjk(right) {
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
    use std::ops::RangeInclusive;

    fn tone(rate: u32, ms: usize, amplitude: f32) -> Vec<f32> {
        (0..rate as usize * ms / 1000)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    /// One scan with nothing learned about the room beforehand.
    fn single_scan(samples: &[f32], rate: u32, minimum_ms: u32) -> Option<usize> {
        pause_boundary(samples, rate, minimum_ms, &mut NoiseFloor::default())
    }

    #[test]
    fn catches_an_interior_pause_after_speech_has_resumed_at_multiple_rates() {
        for rate in [8000, 16000, 48000] {
            let mut audio = tone(rate, 1000, 0.1);
            audio.extend(vec![0.; rate as usize * 300 / 1000]);
            audio.extend(tone(rate, 500, 0.1));
            assert_eq!(
                single_scan(&audio, rate, 300),
                Some(rate as usize * 1240 / 1000)
            );
        }
    }

    #[test]
    fn preserves_quiet_continuous_speech_and_skips_silence_or_dc() {
        for amplitude in [0.1, 0.004, 0.0001] {
            assert_eq!(single_scan(&tone(16000, 3000, amplitude), 16000, 300), None);
        }
        assert_eq!(single_scan(&vec![0.; 32000], 16000, 300), None);
        assert_eq!(single_scan(&vec![0.1; 32000], 16000, 300), None);
        assert_eq!(single_scan(&[f32::NAN; 8000], 16000, 300), None);
        assert_eq!(single_scan(&[0.; 8000], 0, 300), None);
    }

    #[test]
    fn coarser_predecode_keeps_short_utterances_whole() {
        let mut audio = tone(16000, 5000, 0.05);
        audio.extend(vec![0.; 16000]);
        assert_eq!(single_scan(&audio, 16000, 8000), None);
        assert_eq!(single_scan(&audio, 16000, 300), Some(audio.len()));
        audio.extend(tone(16000, 3000, 0.05));
        audio.extend(vec![0.; 4000]);
        assert_eq!(single_scan(&audio, 16000, 8000), Some(audio.len()));
    }

    /// Deterministic white noise in [-1, 1).
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32 * 2. - 1.
        }
    }

    /// A fan or an office: pink noise (Paul Kellet's filter) at `dbfs` RMS.
    fn room(rate: u32, ms: usize, dbfs: f32, seed: u32) -> Vec<f32> {
        let mut white = Noise(seed);
        let mut b = [0_f32; 7];
        let mut pink: Vec<f32> = (0..rate as usize * ms / 1000)
            .map(|_| {
                let w = white.next();
                b[0] = 0.99886 * b[0] + w * 0.0555179;
                b[1] = 0.99332 * b[1] + w * 0.0750759;
                b[2] = 0.96900 * b[2] + w * 0.153852;
                b[3] = 0.86650 * b[3] + w * 0.3104856;
                b[4] = 0.55000 * b[4] + w * 0.5329522;
                b[5] = -0.7616 * b[5] - w * 0.016898;
                let out = b.iter().sum::<f32>() + w * 0.5362;
                b[6] = w * 0.115926;
                out
            })
            .collect();
        let rms = (pink.iter().map(|v| v * v).sum::<f32>() / pink.len() as f32).sqrt();
        let gain = 10_f32.powf(dbfs / 20.) / rms;
        pink.iter_mut().for_each(|v| *v *= gain);
        pink
    }

    fn vowel(rate: u32, ms: usize, amplitude: f32) -> Vec<f32> {
        (0..rate as usize * ms / 1000)
            .map(|i| amplitude * (i as f32 * 220. * std::f32::consts::TAU / rate as f32).sin())
            .collect()
    }

    fn mix(mut speech: Vec<f32>, rate: u32, seed: u32) -> Vec<f32> {
        let ms = speech.len() * 1000 / rate as usize + 1;
        for (s, n) in speech.iter_mut().zip(room(rate, ms, -46., seed)) {
            *s += n;
        }
        speech
    }

    /// The engine's cadence: nothing before the default 250 ms minimum
    /// recording, then 80 ms of new audio per scan plus one second of
    /// already-scanned context, with one floor carried through the utterance.
    fn replay(audio: &[f32], rate: u32, minimum_ms: usize) -> Vec<usize> {
        let (tick, rate) = (rate as usize * 80 / 1000, rate as usize);
        let (mut start, mut scanned, mut available) = (0_usize, 0_usize, 0_usize);
        let mut floor = NoiseFloor::default();
        let mut cuts = Vec::new();
        while available < audio.len() {
            available = (available + tick).min(audio.len());
            let scan = start.max(scanned.saturating_sub(rate));
            scanned = available;
            if start == 0 && scanned < rate / 4 {
                continue;
            }
            let remaining = (start + rate * minimum_ms / 1000).saturating_sub(scan) * 1000 / rate;
            let window = &audio[scan..available];
            if let Some(end) = pause_boundary(window, rate as u32, remaining as u32, &mut floor) {
                start = scan + end;
                scanned = start;
                cuts.push(start);
            }
        }
        cuts
    }

    /// `lead` of nothing, 400 ms of room, then five 1.7 s phrases peaking
    /// near -13 dBFS with 400 ms pauses. Also returns where each pause may be
    /// cut: once 240 ms of it has been heard.
    fn phrases_in_room(rate: u32, lead: usize) -> (Vec<f32>, Vec<RangeInclusive<usize>>) {
        let ms = |ms: usize| rate as usize * ms / 1000;
        let mut speech = vec![0.; ms(400)];
        for _ in 0..5 {
            speech.extend(vowel(rate, 1700, 0.3));
            speech.extend(vec![0.; ms(400)]);
        }
        let mut audio = vec![0.; ms(lead)];
        audio.extend(mix(speech, rate, 7));
        let pauses = (1..=5)
            .map(|n| ms(lead + 2100 * n + 240)..=ms(lead + 2100 * n + 400))
            .collect();
        (audio, pauses)
    }

    /// Progressive text finds every pause and on-release preparation the
    /// first one after 8 s, each cut inside room-only audio.
    fn assert_cuts_every_pause(audio: &[f32], pauses: &[RangeInclusive<usize>], rate: u32) {
        let cuts = replay(audio, rate, 300);
        assert_eq!(cuts.len(), 5, "{rate}: {cuts:?}");
        assert!(
            cuts.iter()
                .zip(pauses)
                .all(|(cut, pause)| pause.contains(cut)),
            "{rate}: {cuts:?}"
        );
        let cuts = replay(audio, rate, 8000);
        assert_eq!(cuts.len(), 1, "{rate}: {cuts:?}");
        assert!(pauses[3].contains(&cuts[0]), "{rate}: {cuts:?}");
    }

    #[test]
    fn finds_pauses_in_room_noise_above_the_fixed_ceiling() {
        for rate in [16000, 48000] {
            let quiet = room(rate, 1000, -46., 3);
            let mut energies = frame_energies(&quiet, rate as usize / 100);
            energies.sort_by(f32::total_cmp);
            assert!(
                energies[energies.len() / 10] > 0.003,
                "the fixed ceiling alone hears this room as continuous sound"
            );
            let (audio, pauses) = phrases_in_room(rate, 0);
            assert_cuts_every_pause(&audio, &pauses, rate);
        }
    }

    #[test]
    fn zeros_from_a_device_starting_or_dropping_out_are_not_the_room() {
        // Every press opens a new capture stream, and a device warming up (a
        // Bluetooth headset switching profiles, say) delivers zeros first; a
        // dropout does the same mid-stream. Counted as the room, 60 ms of
        // them pinned the floor at zero and no pause here was ever found.
        for rate in [16000, 48000] {
            let ms = |ms: usize| rate as usize * ms / 1000;
            for lead in [100, 300] {
                let (audio, pauses) = phrases_in_room(rate, lead);
                assert_cuts_every_pause(&audio, &pauses, rate);
            }
            // 100 ms lost inside the first phrase, then inside the first pause.
            for at in [1000, 2150] {
                let (mut audio, pauses) = phrases_in_room(rate, 0);
                audio[ms(at)..ms(at + 100)].fill(0.);
                assert_cuts_every_pause(&audio, &pauses, rate);
            }
        }
    }

    #[test]
    fn a_microphone_that_silences_its_pauses_keeps_the_fixed_ceiling() {
        // A noise gate or suppressor passes the speaker and nothing else, so
        // a floor learned from it would be the soft end of the voice: here a
        // 400 ms vowel 26 dB under the loud ones. Its pauses are zeros, which
        // the fixed ceiling finds. Before its first pause such a stream, like
        // any without a moment of room, only has the 22 dB cap.
        let rate = 16000;
        let mut audio = vec![0.; 4800];
        let mut pauses = Vec::new();
        for phrase in 0..5 {
            audio.extend(vowel(rate, 800, 0.3));
            if phrase > 0 {
                audio.extend(vowel(rate, 400, 0.015));
            }
            audio.extend(vowel(rate, 500, 0.3));
            audio.extend(vec![0.; 6400]);
            pauses.push(audio.len() - 2560..=audio.len());
        }
        assert_cuts_every_pause(&audio, &pauses, rate);
    }

    #[test]
    fn speech_like_modulated_noise_without_pauses_is_never_cut() {
        // Every third syllable stressed (peaks near -9 dBFS), the rest 24-28
        // dB under it: the phrase-relative cap (-31 dBFS) sits far above
        // twice the room (-44 dBFS), so only the floor keeps the valleys
        // around the soft ones from adding up to 240 ms. Five times it would.
        let rate = 16000;
        let mut rng = Noise(99);
        let mut speech = vec![0.; 9600];
        let mut syllables = 0;
        while speech.len() < rate as usize * 12 {
            // A syllable, noise under a raised cosine of 100–250 ms, then a
            // 20–80 ms join where only the room is heard.
            let len = 16 * (100 + (rng.next().abs() * 150.) as usize);
            let level = if syllables % 3 == 0 {
                0.6
            } else {
                0.025 + rng.next().abs() * 0.015
            };
            syllables += 1;
            let syllable: Vec<f32> = (0..len)
                .map(|i| {
                    let envelope = (std::f32::consts::PI * i as f32 / len as f32).sin().powi(2);
                    level * envelope * rng.next()
                })
                .collect();
            speech.extend(syllable);
            speech.extend(vec![0.; 16 * (20 + (rng.next().abs() * 60.) as usize)]);
        }
        let audio = mix(speech, rate, 11);
        for minimum_ms in [300, 8000] {
            assert_eq!(
                replay(&audio, rate, minimum_ms),
                Vec::<usize>::new(),
                "{minimum_ms} ms"
            );
        }
    }

    #[test]
    fn a_soft_stretch_of_speech_is_not_mistaken_for_the_room() {
        // Unbroken voicing: a second of it has no frame of room alone, so its
        // own low percentile is the soft vowel: 6 dB above the room, 26 dB
        // under the loud one. The floor heard at the start must stay the
        // reference, including for the scans after 8 s, or it is cut in two.
        let rate = 16000;
        let mut speech = vec![0.; 9600];
        for _ in 0..6 {
            speech.extend(vowel(rate, 1200, 0.3));
            speech.extend(vowel(rate, 400, 0.015));
        }
        speech.extend(vowel(rate, 1200, 0.3));
        let audio = mix(speech, rate, 5);
        for minimum_ms in [300, 8000] {
            assert_eq!(
                replay(&audio, rate, minimum_ms),
                Vec::<usize>::new(),
                "{minimum_ms} ms"
            );
        }
    }

    #[test]
    fn only_the_rest_of_a_noisy_pause_counts_as_nothing_said() {
        let rate = 16000;
        let mut floor = NoiseFloor::default();
        let rest = mix(vec![0.; 24000], rate, 17);
        assert!(!only_room(&rest, rate, &floor), "no pause found yet");
        let mut speech = vec![0.; 8000];
        speech.extend(vowel(rate, 1000, 0.3));
        speech.extend(vec![0.; 6400]);
        let heard = mix(speech, rate, 13);
        assert_eq!(
            pause_boundary(&heard, rate, 300, &mut floor),
            Some(heard.len())
        );
        assert!(only_room(&rest, rate, &floor));
        assert!(only_room(&rest[..100], rate, &floor), "under one frame");
        // A short, soft word (6 dB above the room) is someone speaking.
        let mut word = vec![0.; 8000];
        word.extend(vowel(rate, 120, 0.015));
        word.extend(vec![0.; 8000]);
        assert!(!only_room(&mix(word, rate, 17), rate, &floor));
        assert!(!only_room(&[f32::NAN; 1600], rate, &floor));
        // After a pause in a quiet room the rest decodes exactly as before.
        let mut quiet = NoiseFloor::default();
        let mut clean = vowel(rate, 1000, 0.3);
        clean.extend(vec![0.; 6400]);
        assert!(pause_boundary(&clean, rate, 300, &mut quiet).is_some());
        assert!(!only_room(&vec![0.; 16000], rate, &quiet));
        assert!(!only_room(&rest, rate, &quiet));
        // Also after silence found with a floor learned from speech: that
        // pause fell in the partial, oldest second of a long scan.
        let mut unsure = NoiseFloor::default();
        unsure.observe(&[0.02; 100]);
        let mut scan = vowel(rate, 250, 0.3);
        scan.extend(vowel(rate, 240, 0.0005));
        scan.extend(vowel(rate, 1000, 0.3));
        assert_eq!(pause_boundary(&scan, rate, 300, &mut unsure), Some(7840));
        assert!(!only_room(&vowel(rate, 500, 0.01), rate, &unsure));
    }

    #[test]
    fn a_pause_just_heard_at_the_end_of_a_long_scan_is_the_floor() {
        // One scan of 9.35 s (the first after a slow decode, say) with no
        // lead-in: the only room is the pause at its very end, which a split
        // from the oldest audio would leave in a partial second.
        let rate = 16000;
        let mut speech = vowel(rate, 8950, 0.3);
        speech.extend(vec![0.; 6400]);
        let scan = mix(speech, rate, 19);
        let mut floor = NoiseFloor::default();
        assert_eq!(
            pause_boundary(&scan, rate, 8000, &mut floor),
            Some(scan.len())
        );
        assert!(floor.floor.is_some_and(|f| f < 0.005), "{floor:?}");
        // So the soft word after it is judged against the room, not the vowel.
        let mut word = vowel(rate, 120, 0.015);
        word.extend(vec![0.; 4000]);
        assert!(!only_room(&mix(word, rate, 23), rate, &floor));
    }

    #[test]
    fn the_floor_only_falls_and_ignores_short_windows() {
        let mut floor = NoiseFloor::default();
        floor.observe(&[0.0001; 49]);
        assert_eq!(floor.floor, None, "a start-up blip is not the room");
        let mut room: Vec<f32> = (0..100).map(|i| 0.004 + i as f32 * 0.00001).collect();
        floor.observe(&room);
        assert_eq!(floor.floor, Some(room[9]));
        floor.observe(&[0.05; 100]);
        assert_eq!(floor.floor, Some(room[9]), "speech never raises it");
        room.iter_mut().for_each(|e| *e /= 2.);
        floor.observe(&room);
        assert_eq!(floor.floor, Some(room[9]));
        // A long scan is judged a second at a time: a 250 ms pause in 3 s.
        let mut long = vec![0.05; 300];
        long[150..175].fill(0.001);
        let mut fresh = NoiseFloor::default();
        fresh.observe(&long);
        assert_eq!(fresh.floor, Some(0.001));
    }

    #[test]
    fn digital_silence_is_left_out_of_the_floor_unless_it_is_a_gate() {
        let mut room: Vec<f32> = (0..100).map(|i| 0.004 + i as f32 * 0.00001).collect();
        // A stream opening with zeros, and a 100 ms dropout: judged by the
        // frames that were heard, as long as half a second of them remains.
        let mut floor = NoiseFloor::default();
        let mut opening = vec![0.; 60];
        opening.extend(&room[..40]);
        floor.observe(&opening);
        assert_eq!(floor.floor, None, "40 frames heard");
        let mut opening = vec![0.; 45];
        opening.extend(&room[..55]);
        floor.observe(&opening);
        assert_eq!(floor.floor, Some(room[5]), "the tenth of 55 heard frames");
        let mut fresh = NoiseFloor::default();
        room[30..40].fill(0.);
        fresh.observe(&room);
        assert_eq!(fresh.floor, Some(room[8]), "the tenth of 90 heard frames");
        let mut silence = NoiseFloor::default();
        silence.observe(&[0.; 300]);
        assert_eq!(silence.floor, None);
        // 150 ms of zeros after sound: a gate. Zero, and it never rises.
        room[30..45].fill(0.);
        fresh.observe(&room);
        assert_eq!(fresh.floor, Some(0.));
        fresh.observe(&[0.004; 100]);
        assert_eq!(fresh.floor, Some(0.));
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

    /// The phrase-relative cap is what stops a floor learned from speech (no
    /// pause since the key went down) from turning speech within 22 dB of its
    /// peak into a pause. Dropping it for the plain
    /// `floor.max(min(peak * 0.08, 0.003))` passes every other test here.
    #[test]
    fn a_floor_learned_from_speech_never_turns_quieter_speech_into_a_pause() {
        let mut floor = NoiseFloor::default();
        floor.observe(&[0.02; 100]);
        let mut scan = vowel(16_000, 600, 0.3);
        scan.extend(vowel(16_000, 300, 0.035));
        scan.extend(vowel(16_000, 600, 0.3));
        assert_eq!(pause_boundary(&scan, 16_000, 300, &mut floor), None);
    }
}
