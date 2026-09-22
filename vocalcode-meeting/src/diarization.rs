use crate::{MeetingError, Result, TARGET_SAMPLE_RATE};

const WINDOW_SAMPLES: usize = 400; // 25 ms
const WINDOW_STEP: usize = 320; // 20 ms
const BAND_COUNT: usize = 24;
const MAX_VOICEPRINT_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 30;
const MIN_VOICEPRINT_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 2;
const MAX_CLUSTERS: usize = 16;

/// Compact, normalized acoustic signature used for immediate local speaker
/// labels.  It is deliberately not biometric identity: it is scoped to one
/// meeting and never persisted outside that meeting's speaker assignment.
#[derive(Debug, Clone, PartialEq)]
pub struct Voiceprint {
    features: Vec<f32>,
}

impl Voiceprint {
    pub fn from_samples(samples: &[f32]) -> Result<Self> {
        if samples.len() < MIN_VOICEPRINT_SAMPLES {
            return Err(MeetingError::Invalid(
                "speaker segment is shorter than 500 ms".to_string(),
            ));
        }
        let samples = &samples[..samples.len().min(MAX_VOICEPRINT_SAMPLES)];
        let frequencies = log_spaced_frequencies();
        let mut means = [0.0f32; BAND_COUNT];
        let mut squares = [0.0f32; BAND_COUNT];
        let mut frame_count = 0usize;
        let mut rms_sum = 0.0f32;
        let mut crossing_sum = 0.0f32;
        for frame in samples.windows(WINDOW_SAMPLES).step_by(WINDOW_STEP) {
            let rms = (frame.iter().map(|sample| sample * sample).sum::<f32>()
                / frame.len() as f32)
                .sqrt();
            // Skip near-silent windows so room noise does not become a speaker.
            if rms < 0.004 {
                continue;
            }
            rms_sum += rms.ln_1p();
            crossing_sum += frame
                .windows(2)
                .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
                .count() as f32
                / frame.len() as f32;
            for (index, frequency) in frequencies.iter().enumerate() {
                let energy = goertzel_energy(frame, *frequency).ln_1p();
                means[index] += energy;
                squares[index] += energy * energy;
            }
            frame_count += 1;
        }
        if frame_count < 5 {
            return Err(MeetingError::Invalid(
                "speaker segment contains too little voiced audio".to_string(),
            ));
        }
        let denominator = frame_count as f32;
        let mut features = Vec::with_capacity(BAND_COUNT * 2 + 2);
        features.extend(means.iter().map(|sum| sum / denominator));
        for (sum, square_sum) in means.iter().zip(&squares) {
            let mean = sum / denominator;
            features.push((square_sum / denominator - mean * mean).max(0.0).sqrt());
        }
        features.push(rms_sum / denominator);
        features.push(crossing_sum / denominator);
        normalize(&mut features)?;
        Ok(Self { features })
    }

    pub fn similarity(&self, other: &Self) -> f32 {
        if self.features.len() != other.features.len() {
            return -1.0;
        }
        self.features
            .iter()
            .zip(&other.features)
            .map(|(left, right)| left * right)
            .sum::<f32>()
            .clamp(-1.0, 1.0)
    }
}

fn normalize(features: &mut [f32]) -> Result<()> {
    if features.iter().any(|feature| !feature.is_finite()) {
        return Err(MeetingError::Invalid(
            "speaker features are not finite".to_string(),
        ));
    }
    let norm = features
        .iter()
        .map(|feature| feature * feature)
        .sum::<f32>()
        .sqrt();
    if norm <= f32::EPSILON {
        return Err(MeetingError::Invalid(
            "speaker features have no energy".to_string(),
        ));
    }
    for feature in features {
        *feature /= norm;
    }
    Ok(())
}

fn log_spaced_frequencies() -> [f32; BAND_COUNT] {
    let low = 120.0f32.ln();
    let high = 7_200.0f32.ln();
    std::array::from_fn(|index| {
        let ratio = index as f32 / (BAND_COUNT - 1) as f32;
        (low + (high - low) * ratio).exp()
    })
}

fn goertzel_energy(samples: &[f32], frequency: f32) -> f32 {
    let omega = std::f32::consts::TAU * frequency / TARGET_SAMPLE_RATE as f32;
    let coefficient = 2.0 * omega.cos();
    let mut previous = 0.0f32;
    let mut before_previous = 0.0f32;
    for (index, sample) in samples.iter().enumerate() {
        // Hann window limits leakage enough for a compact voice signature.
        let window =
            0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / (samples.len() - 1) as f32).cos();
        let current = sample * window + coefficient * previous - before_previous;
        before_previous = previous;
        previous = current;
    }
    (previous * previous + before_previous * before_previous
        - coefficient * previous * before_previous)
        .max(0.0)
        / (samples.len() * samples.len()) as f32
}

#[derive(Debug, Clone)]
struct Cluster {
    centroid: Vec<f32>,
    count: u64,
}

/// Incremental meeting-scoped clustering for live labels.  A higher-quality
/// offline sherpa-onnx diarization pass may relabel the same segments later.
#[derive(Debug, Clone)]
pub struct OnlineSpeakerClusterer {
    clusters: Vec<Cluster>,
    similarity_threshold: f32,
    max_clusters: usize,
}

impl Default for OnlineSpeakerClusterer {
    fn default() -> Self {
        Self::new(0.92, 8).expect("valid built-in speaker clustering settings")
    }
}

impl OnlineSpeakerClusterer {
    pub fn new(similarity_threshold: f32, max_clusters: usize) -> Result<Self> {
        if !(-1.0..=1.0).contains(&similarity_threshold)
            || max_clusters == 0
            || max_clusters > MAX_CLUSTERS
        {
            return Err(MeetingError::Invalid(
                "invalid speaker clustering settings".to_string(),
            ));
        }
        Ok(Self {
            clusters: Vec::new(),
            similarity_threshold,
            max_clusters,
        })
    }

    /// Return a zero-based meeting-local speaker index.
    pub fn assign(&mut self, voiceprint: &Voiceprint) -> usize {
        let best = self
            .clusters
            .iter()
            .enumerate()
            .map(|(index, cluster)| {
                let similarity = cluster
                    .centroid
                    .iter()
                    .zip(&voiceprint.features)
                    .map(|(left, right)| left * right)
                    .sum::<f32>();
                (index, similarity)
            })
            .max_by(|left, right| left.1.total_cmp(&right.1));
        let index = match best {
            Some((index, similarity))
                if similarity >= self.similarity_threshold
                    || self.clusters.len() >= self.max_clusters =>
            {
                index
            }
            _ => {
                self.clusters.push(Cluster {
                    centroid: voiceprint.features.clone(),
                    count: 0,
                });
                self.clusters.len() - 1
            }
        };
        let cluster = &mut self.clusters[index];
        let old_count = cluster.count as f32;
        cluster.count = cluster.count.saturating_add(1);
        for (centroid, feature) in cluster.centroid.iter_mut().zip(&voiceprint.features) {
            *centroid = (*centroid * old_count + feature) / cluster.count as f32;
        }
        let _ = normalize(&mut cluster.centroid);
        index
    }

    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voiced(frequency: f32, harmonic: f32) -> Vec<f32> {
        (0..TARGET_SAMPLE_RATE as usize * 2)
            .map(|index| {
                let time = index as f32 / TARGET_SAMPLE_RATE as f32;
                ((std::f32::consts::TAU * frequency * time).sin() * 0.16
                    + (std::f32::consts::TAU * frequency * 2.0 * time).sin() * harmonic)
                    * (0.8 + 0.2 * (std::f32::consts::TAU * 3.0 * time).sin())
            })
            .collect()
    }

    #[test]
    fn same_acoustic_source_stays_in_one_cluster() {
        let a = Voiceprint::from_samples(&voiced(180.0, 0.05)).unwrap();
        let b = Voiceprint::from_samples(&voiced(182.0, 0.05)).unwrap();
        let mut clusterer = OnlineSpeakerClusterer::new(0.90, 8).unwrap();
        assert_eq!(clusterer.assign(&a), 0);
        assert_eq!(clusterer.assign(&b), 0);
        assert_eq!(clusterer.cluster_count(), 1);
    }

    #[test]
    fn distinct_spectra_get_distinct_live_labels() {
        let low = Voiceprint::from_samples(&voiced(150.0, 0.08)).unwrap();
        let high = Voiceprint::from_samples(&voiced(520.0, 0.01)).unwrap();
        let mut clusterer = OnlineSpeakerClusterer::new(0.97, 8).unwrap();
        assert_eq!(clusterer.assign(&low), 0);
        assert_eq!(clusterer.assign(&high), 1);
        assert_eq!(clusterer.cluster_count(), 2);
    }

    #[test]
    fn silence_is_not_a_voiceprint() {
        assert!(Voiceprint::from_samples(&vec![0.0; TARGET_SAMPLE_RATE as usize]).is_err());
    }
}
