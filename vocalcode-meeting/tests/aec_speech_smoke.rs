use std::path::Path;

use vocalcode_meeting::{
    decode_audio_file, AudioBlock, AudioSource, LinearMonoResampler, RealtimeEchoCanceller,
    TARGET_SAMPLE_RATE,
};

const FRAME_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 100;

fn decode_mono(path: &Path) -> Vec<f32> {
    let mut resampler = LinearMonoResampler::default();
    let mut output = Vec::new();
    decode_audio_file(path, |block| {
        output.extend(resampler.push(&block)?);
        Ok(())
    })
    .unwrap();
    output.extend(resampler.finish());
    output
}

fn block(samples: Vec<f32>, start_ms: u64) -> AudioBlock {
    AudioBlock {
        samples,
        sample_rate: TARGET_SAMPLE_RATE,
        channels: 1,
        start_ms,
    }
}

fn mse(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f32>()
        / left.len().min(right.len()) as f32
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

fn normalized_correlation(left: &[f32], right: &[f32]) -> f32 {
    let (dot, left_energy, right_energy) = left.iter().zip(right).fold(
        (0.0, 0.0, 0.0),
        |(dot, left_energy, right_energy), (left, right)| {
            (
                dot + left * right,
                left_energy + left * left,
                right_energy + right * right,
            )
        },
    );
    dot / (left_energy * right_energy).sqrt().max(f32::EPSILON)
}

fn frame_envelope(samples: &[f32]) -> Vec<f32> {
    samples.chunks(FRAME_SAMPLES).map(rms).collect()
}

fn write_debug_wav_if_requested(samples: &[f32]) {
    let Some(path) = std::env::var_os("VOCALCODE_AEC_OUTPUT_WAV") else {
        return;
    };
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create AEC debug wav");
    for sample in samples {
        writer
            .write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .expect("write AEC debug sample");
    }
    writer.finalize().expect("finalize AEC debug wav");
}

/// Opt-in local speech replay.  It never records hardware or writes audio.
/// Point the variables at two independent, consented local speech WAV files.
#[test]
#[ignore = "requires VOCALCODE_AEC_RENDER_WAV and VOCALCODE_AEC_NEAR_WAV"]
fn real_speech_double_talk_preserves_near_end_and_reduces_echo() {
    let render_path = std::env::var_os("VOCALCODE_AEC_RENDER_WAV").unwrap();
    let near_path = std::env::var_os("VOCALCODE_AEC_NEAR_WAV").unwrap();
    let render = decode_mono(Path::new(&render_path));
    let near = decode_mono(Path::new(&near_path));
    let samples = render.len().min(near.len());
    let samples = samples - samples % FRAME_SAMPLES;
    assert!(samples >= TARGET_SAMPLE_RATE as usize * 5);
    let render = &render[..samples];
    let near = &near[..samples];

    // A direct path plus one short reflection approximates speaker-to-mic
    // leakage while keeping an independent near-end speaker active.
    let direct_delay = TARGET_SAMPLE_RATE as usize * 70 / 1_000;
    let reflection_delay = direct_delay + TARGET_SAMPLE_RATE as usize * 19 / 1_000;
    let mut capture = near.iter().map(|sample| sample * 0.85).collect::<Vec<_>>();
    for index in direct_delay..samples {
        capture[index] += render[index - direct_delay] * 0.42;
    }
    for index in reflection_delay..samples {
        capture[index] += render[index - reflection_delay] * 0.14;
    }
    capture
        .iter_mut()
        .for_each(|sample| *sample = sample.clamp(-1.0, 1.0));

    let expected_near = near.iter().map(|sample| sample * 0.85).collect::<Vec<_>>();
    let mut aec = RealtimeEchoCanceller::new();
    let mut cleaned = Vec::with_capacity(samples);
    for (frame, (render, capture)) in render
        .chunks_exact(FRAME_SAMPLES)
        .zip(capture.chunks_exact(FRAME_SAMPLES))
        .enumerate()
    {
        aec.push(
            AudioSource::System,
            &block(render.to_vec(), frame as u64 * 10),
        )
        .unwrap();
        cleaned.extend(
            aec.push(
                AudioSource::Microphone,
                &block(capture.to_vec(), frame as u64 * 10),
            )
            .unwrap()
            .microphone_samples,
        );
    }
    cleaned.extend(aec.finish().unwrap().microphone_samples);
    assert_eq!(cleaned.len(), samples);

    let settle = (TARGET_SAMPLE_RATE as usize * 2).min(samples / 3);
    let raw_error = mse(&capture[settle..], &expected_near[settle..]);
    let cleaned_error = mse(&cleaned[settle..], &expected_near[settle..]);
    let near_rms = rms(&expected_near[settle..]);
    let cleaned_rms = rms(&cleaned[settle..]);
    let waveform_correlation = normalized_correlation(&cleaned[settle..], &expected_near[settle..]);
    let cleaned_envelope = frame_envelope(&cleaned[settle..]);
    let near_envelope = frame_envelope(&expected_near[settle..]);
    let envelope_correlation = normalized_correlation(&cleaned_envelope, &near_envelope);
    write_debug_wav_if_requested(&cleaned);
    println!(
        "raw_error={raw_error:.8} cleaned_error={cleaned_error:.8} near_rms={near_rms:.8} cleaned_rms={cleaned_rms:.8} waveform_correlation={waveform_correlation:.6} envelope_correlation={envelope_correlation:.6} stats={:?}",
        aec.stats()
    );
    // AEC3's high-pass filter and non-linear suppressor intentionally change
    // sample phase, so sample-by-sample MSE is not a useful speech-quality
    // gate.  Preserve the 10 ms speech envelope instead; the deterministic
    // echo-only unit test carries the actual echo-reduction threshold.
    assert!(
        envelope_correlation > 0.75,
        "AEC distorted the near-end speech envelope: {envelope_correlation:.6}"
    );
    assert!(cleaned_rms > near_rms * 0.45);
}

/// Independent speech pairs, cold/warm starts, quieter near-end, and echo-only
/// controls. The paths are explicit synthetic fixtures, never user recordings.
#[test]
#[ignore = "requires VOCALCODE_QA_DIALOGUE_DIR synthetic SAPI fixtures"]
fn synthetic_dialogue_acoustic_matrix() {
    let root = std::path::PathBuf::from(std::env::var_os("VOCALCODE_QA_DIALOGUE_DIR").unwrap());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let out = root.join(format!("aec-matrix-{stamp}"));
    std::fs::create_dir(&out).unwrap();
    let mut results = Vec::new();
    for (index, (near_name, far_name, near_gain, warm_seconds, delay_ms)) in [
        ("zh_negation", "en_correction", 0.85, 0, 70),
        ("en_correction", "zh_negation", 0.85, 0, 120),
        ("en_number", "zh_correction", 0.425, 0, 40),
        ("zh_negation", "en_correction", 0.85, 5, 70),
        ("zh_negation", "en_correction", 0.0, 0, 70),
        ("en_number", "zh_correction", 0.0, 0, 120),
    ]
    .into_iter()
    .enumerate()
    {
        let near_clip = decode_mono(&root.join(format!("{near_name}.wav")));
        let far_clip = decode_mono(&root.join(format!("{far_name}.wav")));
        let warm = warm_seconds * 16000;
        let len = near_clip.len().min(far_clip.len()) / 160 * 160 + warm;
        let render: Vec<_> = (0..len).map(|i| far_clip[i % far_clip.len()]).collect();
        let near: Vec<_> = (0..len)
            .map(|i| {
                if i < warm {
                    0.0
                } else {
                    near_clip[i - warm] * near_gain
                }
            })
            .collect();
        let mut capture = near.clone();
        for (delay, gain) in [(delay_ms * 16, 0.42), ((delay_ms + 19) * 16, 0.14)] {
            for i in delay..len {
                capture[i] += render[i - delay] * gain;
            }
        }
        capture.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
        let mut aec = RealtimeEchoCanceller::new();
        let mut cleaned = Vec::new();
        for start in (0..len).step_by(160) {
            aec.push(
                AudioSource::System,
                &block(render[start..start + 160].to_vec(), start as u64 / 16),
            )
            .unwrap();
            cleaned.extend(
                aec.push(
                    AudioSource::Microphone,
                    &block(capture[start..start + 160].to_vec(), start as u64 / 16),
                )
                .unwrap()
                .microphone_samples,
            );
        }
        cleaned.extend(aec.finish().unwrap().microphone_samples);
        assert_eq!(cleaned.len(), len);
        let settle = warm.max(32000);
        let correlation = normalized_correlation(
            &frame_envelope(&cleaned[settle..]),
            &frame_envelope(&near[settle..]),
        );
        let near_rms = rms(&near[settle..]);
        let clean_rms = rms(&cleaned[settle..]);
        let raw_rms = rms(&capture[settle..]);
        let pass = if near_gain > 0.0 {
            correlation > 0.75 && clean_rms > near_rms * 0.45
        } else {
            clean_rms < raw_rms * 0.45
        };
        println!("MATRIX {index} near={near_name} far={far_name} gain={near_gain} warm={warm_seconds} delay={delay_ms} correlation={correlation:.6} clean/raw={:.6} pass={pass}",clean_rms/raw_rms);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(out.join(format!("case-{index}-cleaned.wav")), spec).unwrap();
        for sample in &cleaned {
            writer
                .write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .unwrap();
        }
        writer.finalize().unwrap();
        results.push(serde_json::json!({"index":index,"near":near_name,"far":far_name,"near_gain":near_gain,"warm_seconds":warm_seconds,"delay_ms":delay_ms,
            "envelope_correlation":correlation,"near_rms":near_rms,"clean_rms":clean_rms,"raw_rms":raw_rms,
            "protected_frames":aec.stats().protected_capture_frames,"passed":pass}));
    }
    std::fs::write(
        out.join("report.json"),
        serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
    println!("MATRIX_REPORT {}", out.join("report.json").display());
    assert!(
        results.iter().all(|r| r["passed"] == true),
        "acoustic matrix failed; inspect retained report"
    );
}
