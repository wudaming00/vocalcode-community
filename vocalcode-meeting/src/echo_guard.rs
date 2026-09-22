//! Conservative double-talk safety net around the non-linear AEC suppressor.
//! If a substantial part of microphone energy is not explained by any delayed
//! render reference, aggressive attenuation must not erase it. This is not a
//! replacement echo canceller: AEC keeps adapting, and echo-dominated frames
//! still use its output. All state is bounded, local and language-independent.
use std::collections::VecDeque;

const DECIMATION: usize = 4; // 16 kHz -> 4 kHz, analysis only.
const WINDOW: usize = 640; // 160ms for robust correlation, no output buffering.
const HISTORY: usize = 4_000; // One second, accommodating callback skew.
const MAX_DELAY: usize = 1_200; // Search 0..300ms, at 1ms resolution.
                                // Require substantial independent microphone energy before overriding AEC.
                                // A more permissive 0.8 threshold reintroduced remote speech in loopback QA.
const ECHO_DOMINATED_COHERENCE: f32 = 0.55;

#[derive(Default)]
pub(super) struct DoubleTalkGuard {
    render: VecDeque<f32>,
    microphone: VecDeque<f32>,
    render_end: u64,
    capture_end: u64,
    near_end: bool,
    hold_frames: u32,
    frames: u64,
    mix: f32,
    protected_frames: u64,
}

impl DoubleTalkGuard {
    pub fn render(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(DECIMATION) {
            self.render
                .push_back(frame.iter().sum::<f32>() / DECIMATION as f32);
            self.render_end += 1;
        }
        if self.render.len() > HISTORY {
            self.render.drain(..self.render.len() - HISTORY);
        }
    }

    pub fn capture(&mut self, raw: &[f32], cleaned: &mut [f32]) {
        for frame in raw.chunks_exact(DECIMATION) {
            self.microphone
                .push_back(frame.iter().sum::<f32>() / DECIMATION as f32);
            self.capture_end += 1;
        }
        if self.microphone.len() > WINDOW {
            self.microphone.drain(..self.microphone.len() - WINDOW);
        }
        self.frames += 1;
        // Correlation scans run 25 times/sec, not in device callbacks.
        if self.frames.is_multiple_of(4) && self.microphone.len() == WINDOW {
            self.near_end = self.unexplained_microphone_energy();
        }
        let raw_energy = raw.iter().map(|s| s * s).sum::<f32>();
        let clean_energy = cleaned.iter().map(|s| s * s).sum::<f32>();
        // Only intervene on actual attenuation, not merely a weak correlation.
        // Otherwise late reflections can reappear as duplicate remote speech.
        if self.near_end && raw_energy > raw.len() as f32 * 1e-8 && clean_energy < raw_energy * 0.64
        {
            self.hold_frames = 20; // 200ms release avoids chopping word endings.
        } else {
            self.hold_frames = self.hold_frames.saturating_sub(1);
        }
        let target = if self.hold_frames > 0 { 1.0 } else { 0.0 };
        if target > 0.0 {
            self.protected_frames += 1;
        }
        let count = cleaned.len() as f32;
        for (i, (out, input)) in cleaned.iter_mut().zip(raw).enumerate() {
            let mix = self.mix + (target - self.mix) * (i + 1) as f32 / count;
            *out = (*out * (1.0 - mix) + input * mix).clamp(-1.0, 1.0);
        }
        self.mix = target;
    }

    pub fn protected_frames(&self) -> u64 {
        self.protected_frames
    }

    fn unexplained_microphone_energy(&mut self) -> bool {
        let x = self.microphone.make_contiguous();
        let sum_x = x.iter().sum::<f32>();
        let power_x = x.iter().map(|s| s * s).sum::<f32>() - sum_x * sum_x / WINDOW as f32;
        if power_x < WINDOW as f32 * 1e-8 {
            return false;
        }
        let reference_start = self.render_end.saturating_sub(self.render.len() as u64);
        let y = self.render.make_contiguous();
        let mut best = 0.0_f32;
        for delay in (0..=MAX_DELAY).step_by(4) {
            let Some(end) = self.capture_end.checked_sub(delay as u64) else {
                continue;
            };
            let Some(start) = end.checked_sub(WINDOW as u64) else {
                continue;
            };
            if start < reference_start || end > self.render_end {
                continue;
            }
            let offset = (start - reference_start) as usize;
            let reference = &y[offset..offset + WINDOW];
            let (sum_y, power_y, dot) = x
                .iter()
                .zip(reference)
                .fold((0.0, 0.0, 0.0), |(sum, power, dot), (a, b)| {
                    (sum + b, power + b * b, dot + a * b)
                });
            let power_y = power_y - sum_y * sum_y / WINDOW as f32;
            if power_y < 1e-10 {
                continue;
            }
            let dot = dot - sum_x * sum_y / WINDOW as f32;
            best = best.max((dot * dot / (power_x * power_y)).clamp(0.0, 1.0));
            if best >= ECHO_DOMINATED_COHERENCE {
                return false;
            }
        }
        // A missing/quiet/unrelated reference cannot justify removing the
        // majority of the microphone signal. Preserve uncertain speech.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_reference_cannot_erase_near_end_and_state_is_bounded() {
        let mut guard = DoubleTalkGuard::default();
        for frame in 0..500 {
            let raw: Vec<_> = (0..160)
                .map(|i| ((frame * 160 + i) as f32 * 0.11).sin() * 0.1)
                .collect();
            guard.render(&[0.0; 160]);
            let mut cleaned = [0.0; 160];
            guard.capture(&raw, &mut cleaned);
            if frame > 20 {
                assert_eq!(cleaned.as_slice(), raw.as_slice());
            }
        }
        assert_eq!(guard.render.len(), HISTORY);
        assert_eq!(guard.microphone.len(), WINDOW);
        assert!(guard.protected_frames() > 400);
    }

    #[test]
    fn correlated_echo_is_not_reintroduced() {
        let mut guard = DoubleTalkGuard::default();
        for frame in 0..500 {
            let reference: Vec<_> = (0..160)
                .map(|i| ((frame * 160 + i) as f32 * 0.073).sin() * 0.2)
                .collect();
            let raw: Vec<_> = reference.iter().map(|s| s * 0.5).collect();
            guard.render(&reference);
            let mut cleaned = [0.0; 160];
            guard.capture(&raw, &mut cleaned);
            assert_eq!(cleaned, [0.0; 160]);
        }
        assert_eq!(guard.protected_frames(), 0);
    }

    #[test]
    fn protection_releases_when_only_correlated_echo_remains() {
        let mut guard = DoubleTalkGuard::default();
        for frame in 0..150 {
            let reference: Vec<_> = (0..160)
                .map(|i| ((frame * 160 + i) as f32 * 0.073).sin() * 0.2)
                .collect();
            let raw: Vec<_> = (0..160)
                .map(|i| {
                    if frame < 80 {
                        ((frame * 160 + i) as f32 * 0.11).sin() * 0.1
                    } else {
                        reference[i] * 0.5
                    }
                })
                .collect();
            guard.render(&reference);
            let mut cleaned = [0.0; 160];
            guard.capture(&raw, &mut cleaned);
            if (40..70).contains(&frame) {
                assert_eq!(cleaned.as_slice(), raw.as_slice());
            }
            if frame > 125 {
                assert_eq!(cleaned, [0.0; 160]);
            }
        }
        assert!(guard.protected_frames() > 0);
        assert_eq!(guard.hold_frames, 0);
    }
}
