//! Presentation-only microphone levels. No audio data, capture threshold, or
//! transcription behavior is changed by these display settings.

use crate::settings::WaveformSensitivity;

/// Most recent levels, right-aligned, normalized for any view geometry.
pub fn levels<const N: usize>(samples: &[f32], sensitivity: WaveformSensitivity) -> [f64; N] {
    // Display gain only: capture, VAD, saved audio, and ASR see the original signal.
    let gain = match sensitivity {
        WaveformSensitivity::Low => 0.65,
        WaveformSensitivity::Balanced => 1.0,
        WaveformSensitivity::High => 2.0,
    };
    let mut result = [0.0; N];
    let newest = &samples[samples.len().saturating_sub(N)..];
    let offset = N - newest.len();
    for (index, sample) in newest.iter().enumerate() {
        let level = *sample as f64 * gain;
        // Room tone below -40 dBFS stays still. Above it, preserve a broad
        // graded speech range instead of filling the bars on every syllable.
        result[offset + index] = if !level.is_finite() || level <= 0.010 {
            0.0
        } else {
            ((20.0 * level.log10() + 40.0) / 26.0)
                .clamp(0.0, 1.0)
                .powf(1.25)
        };
    }
    result
}

/// Attack/release for normalized heights. Reduce Motion uses data directly.
pub fn ease(current: f64, target: f64, reduced_motion: bool) -> f64 {
    let current = if current.is_finite() {
        current.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let target = if target.is_finite() {
        target.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if reduced_motion {
        return target;
    }
    let next = if target >= current {
        current * 0.35 + target * 0.65
    } else {
        current * 0.45 + target * 0.55
    };
    // Stop redrawing silence once its release has become imperceptible.
    if target == 0.0 && next < 0.005 {
        0.0
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BARS: usize = 10;

    #[test]
    fn balanced_room_tone_stays_near_flat() {
        for rms in [0.0, 0.002, 0.004, 0.008] {
            let bars = levels::<BARS>(&[rms; BARS], WaveformSensitivity::Balanced);
            assert!(
                bars.iter().all(|height| *height <= 0.01),
                "room tone {rms} must not look like speech: {bars:?}"
            );
        }
    }

    #[test]
    fn sensitivity_changes_real_signal_response() {
        let sample = [0.03; BARS];
        let low = levels::<BARS>(&sample, WaveformSensitivity::Low)[0];
        let balanced = levels::<BARS>(&sample, WaveformSensitivity::Balanced)[0];
        let high = levels::<BARS>(&sample, WaveformSensitivity::High)[0];
        assert!(low < balanced && balanced < high);
    }

    #[test]
    fn silence_settles_quickly_without_a_decorative_pulse() {
        let mut height = 1.0;
        for _ in 0..8 {
            height = ease(height, 0.0, false);
        }
        assert!(
            height < 0.01,
            "room tone should settle within eight UI ticks: {height}"
        );
        assert_eq!(ease(1.0, 0.0, true), 0.0);
    }

    #[test]
    fn newest_audio_samples_are_visible_and_invalid_levels_are_quiet() {
        let mut samples = vec![0.0; BARS + 4];
        *samples.last_mut().unwrap() = 0.1;
        let bars = levels::<BARS>(&samples, WaveformSensitivity::Balanced);
        assert_eq!(bars[0], 0.0);
        assert!(bars[BARS - 1] > 0.0);
        assert!(levels::<BARS>(
            &[f32::NAN, f32::INFINITY, -0.1],
            WaveformSensitivity::Balanced
        )
        .iter()
        .all(|height| *height == 0.0));
    }

    #[test]
    fn speech_has_a_strong_graded_response() {
        let mut previous = 0.0;
        for rms in [0.012, 0.025, 0.05, 0.10, 0.20] {
            let height = levels::<BARS>(&[rms; BARS], WaveformSensitivity::Balanced)[0];
            assert!(height > previous && height <= 1.0);
            previous = height;
        }
        assert!(levels::<BARS>(&[0.05; BARS], WaveformSensitivity::Balanced)[0] > 1.0 / 3.0);
    }
}
