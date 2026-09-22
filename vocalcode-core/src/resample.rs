//! Bounded streaming mono resampling. A windowed-sinc low-pass is applied before
//! downsampling; integer output positions prevent cumulative timing drift.
//! Centered-filter lookahead is flushed at finish, not lost from either edge.

#[derive(Debug)]
pub struct MonoResampler {
    input_rate: u32,
    output_rate: u32,
    coefficients: Vec<f64>,
    history: Vec<f32>,
    cursor: usize,
    half: usize,
    input_frames: u64,
    processed_frames: u64,
    next_output: u64,
    previous: Option<f32>,
    last_input: f32,
    finished: bool,
}

impl MonoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Option<Self> {
        if !(1000..=384_000).contains(&input_rate) || !(1000..=384_000).contains(&output_rate) {
            return None;
        }
        let half = if input_rate > output_rate {
            (input_rate.div_ceil(output_rate) as usize * 32).min(768)
        } else {
            0
        };
        let mut coefficients = Vec::new();
        if half > 0 {
            // 90% of destination Nyquist leaves an anti-alias transition band.
            let cutoff = 0.45 * output_rate as f64 / input_rate as f64;
            for i in 0..=half * 2 {
                let x = i as f64 - half as f64;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let angle = std::f64::consts::PI * i as f64 / half as f64;
                let window = 0.42 - 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
                coefficients.push(sinc * window);
            }
            let gain: f64 = coefficients.iter().sum();
            for coefficient in &mut coefficients {
                *coefficient /= gain;
            }
        }
        Some(Self {
            input_rate,
            output_rate,
            history: vec![0.0; coefficients.len()],
            coefficients,
            cursor: 0,
            half,
            input_frames: 0,
            processed_frames: 0,
            next_output: 0,
            previous: None,
            last_input: 0.0,
            finished: false,
        })
    }

    pub fn push(&mut self, sample: f32, mut output: impl FnMut(f32)) -> bool {
        if self.finished || !sample.is_finite() {
            return false;
        }
        if self.input_frames == 0 {
            self.history.fill(sample);
        }
        self.last_input = sample;
        self.input_frames += 1;
        self.process(sample, &mut output);
        true
    }

    fn process(&mut self, sample: f32, output: &mut impl FnMut(f32)) {
        let filtered = if self.half == 0 {
            sample
        } else {
            self.history[self.cursor] = sample;
            let mut value = 0.0;
            let mut index = self.cursor;
            for coefficient in &self.coefficients {
                value += coefficient * self.history[index] as f64;
                index = if index == 0 {
                    self.history.len() - 1
                } else {
                    index - 1
                };
            }
            self.cursor = (self.cursor + 1) % self.history.len();
            value as f32
        };
        let index = self.processed_frames;
        self.processed_frames += 1;
        if index < self.half as u64 {
            return;
        }
        let current = index - self.half as u64;
        if current == 0 {
            output(filtered);
            self.next_output = 1;
            self.previous = Some(filtered);
            return;
        }
        let previous = self.previous.unwrap_or(filtered);
        let output_rate = self.output_rate as u64;
        let maximum = if self.finished {
            ((self.input_frames * output_rate + self.input_rate as u64 / 2)
                / self.input_rate as u64)
                .max(1)
        } else {
            u64::MAX
        };
        while self.next_output < maximum
            && self.next_output * self.input_rate as u64 <= current * output_rate
        {
            let offset = self.next_output * self.input_rate as u64 - (current - 1) * output_rate;
            output(if offset == output_rate {
                filtered
            } else {
                previous + (filtered - previous) * (offset as f32 / output_rate as f32)
            });
            self.next_output += 1;
        }
        self.previous = Some(filtered);
    }

    pub fn finish(&mut self, mut output: impl FnMut(f32)) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.input_frames == 0 {
            return;
        }
        for _ in 0..self.half {
            self.process(self.last_input, &mut output);
        }
        let expected = (self.input_frames * self.output_rate as u64 + self.input_rate as u64 / 2)
            / self.input_rate as u64;
        while self.next_output < expected {
            output(self.previous.unwrap_or(0.0));
            self.next_output += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sine(rate: u32, hz: f64) -> Vec<f32> {
        (0..rate)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / rate as f64).sin() as f32 * 0.1)
            .collect()
    }
    fn convert(input: &[f32], rate: u32) -> Vec<f32> {
        let mut r = MonoResampler::new(rate, 16000).unwrap();
        let mut output = Vec::new();
        for &s in input {
            assert!(r.push(s, |s| output.push(s)));
        }
        r.finish(|s| output.push(s));
        r.finish(|_| panic!("finish twice"));
        output
    }
    fn rms(values: &[f32]) -> f64 {
        (values.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
    }
    #[test]
    fn downsampling_preserves_voice_band_and_rejects_aliases() {
        for rate in [44100, 48000, 96000] {
            let speech = convert(&sine(rate, 1000.0), rate);
            let alias = convert(&sine(rate, 12000.0), rate);
            assert_eq!(speech.len(), 16000);
            assert!((rms(&speech[200..15800]) - 0.07071).abs() < 0.002, "{rate}");
            assert!(
                rms(&alias[200..15800]) < 0.0007,
                "alias at {rate}: {}",
                rms(&alias[200..15800])
            );
        }
    }
    #[test]
    fn passthrough_is_bit_exact_and_dc_is_preserved() {
        let input = sine(16000, 1234.0);
        assert!(convert(&input, 16000) == input, "same-rate samples changed");
        for rate in [8000, 44100, 48000] {
            let output = convert(&vec![0.3; rate as usize], rate);
            assert_eq!(output.len(), 16000);
            assert!(output.iter().all(|&s| (s - 0.3).abs() < 1e-6));
        }
    }
    #[test]
    fn tiny_recordings_flush_lookahead_and_invalid_input_is_refused() {
        for count in [1, 2, 3, 7, 30, 100, 999] {
            let output = convert(&vec![0.2; count], 48000);
            // The old converter emitted one sample for any nonempty input.
            assert_eq!(output.len(), ((count * 16000 + 24000) / 48000).max(1));
            assert!(output.iter().all(|&s| (s - 0.2).abs() < 1e-6));
        }
        assert!(MonoResampler::new(0, 16000).is_none());
        let mut r = MonoResampler::new(48000, 16000).unwrap();
        assert!(!r.push(f32::NAN, |_| panic!()));
        r.finish(|_| panic!());
        assert!(!r.push(0.1, |_| panic!()));
    }
}
