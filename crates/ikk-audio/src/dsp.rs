//! Small pure DSP helpers shared by the pipeline (downmix, resample, rms,
//! dBFS). No state, no I/O.

/// Interleaved multi-channel → mono by averaging.
pub fn downmix_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Linear-interpolation resampler. Good enough for speech to 16 kHz; no
/// anti-alias filter, so only use for downsampling from ≤ 48 kHz voice input.
/// Resample by linear interpolation. Going down (48 kHz microphones to
/// the 16 kHz pipeline) the input is low-pass filtered first; without
/// that, everything above the new Nyquist frequency folds back into the
/// speech band as noise on exactly the sharp consonants a recogniser
/// listens for.
pub fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() || from_hz == 0 || to_hz == 0 {
        return input.to_vec();
    }
    let filtered;
    let input = if to_hz < from_hz {
        filtered = anti_alias(input, from_hz, to_hz);
        &filtered[..]
    } else {
        input
    };
    let ratio = to_hz as f64 / from_hz as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let i0 = src.floor() as usize;
        let frac = (src - i0 as f64) as f32;
        let s0 = input[i0.min(last)];
        let s1 = input[(i0 + 1).min(last)];
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// Windowed-sinc low-pass at 0.45 × the new sample rate (31 taps,
/// Blackman window): cheap, linear phase, about −60 dB in the stop band.
fn anti_alias(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    const TAPS: usize = 31;
    let cutoff = 0.45 * to_hz as f64 / from_hz as f64; // cycles per input sample
    let mid = (TAPS / 2) as f64;
    let mut taps = [0f32; TAPS];
    let mut sum = 0f64;
    for (i, tap) in taps.iter_mut().enumerate() {
        let n = i as f64 - mid;
        let sinc = if n == 0.0 {
            2.0 * cutoff
        } else {
            (2.0 * std::f64::consts::PI * cutoff * n).sin() / (std::f64::consts::PI * n)
        };
        let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos()
            + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos();
        *tap = (sinc * w) as f32;
        sum += sinc * w;
    }
    for tap in taps.iter_mut() {
        *tap /= sum as f32;
    }
    let half = TAPS / 2;
    let last = input.len() - 1;
    (0..input.len())
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(k, t)| t * input[(i + k).saturating_sub(half).min(last)])
                .sum()
        })
        .collect()
}

/// Root mean square of a block (linear amplitude).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

const FLOOR_RMS: f32 = 1e-5;

/// Linear RMS → dBFS, floored at −100 dBFS.
pub fn rms_to_dbfs(rms: f32) -> f32 {
    20.0 * rms.max(FLOOR_RMS).log10()
}

/// dB gain → linear factor.
pub fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// Apply a fixed gain with hard clamp to [-1, 1].
pub fn apply_gain(samples: &mut [f32], linear_gain: f32) {
    if (linear_gain - 1.0).abs() < 1e-6 {
        return;
    }
    for s in samples {
        *s = (*s * linear_gain).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_channels() {
        assert_eq!(downmix_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(downmix_mono(&[0.3, 0.3], 1), vec![0.3, 0.3]);
    }

    #[test]
    fn resample_lengths() {
        let x: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin()).collect();
        assert_eq!(resample_linear(&x, 48_000, 16_000).len(), 160);
        assert_eq!(resample_linear(&x, 16_000, 16_000).len(), 480);
        assert_eq!(resample_linear(&x, 44_100, 16_000).len(), 174);
    }

    #[test]
    fn dbfs_reference_points() {
        assert!((rms_to_dbfs(1.0)).abs() < 1e-3);
        assert!((rms_to_dbfs(0.1) + 20.0).abs() < 1e-3);
        assert!(rms_to_dbfs(0.0) > -101.0 && rms_to_dbfs(0.0) < -99.0);
    }

    #[test]
    fn gain_clamps() {
        let mut s = vec![0.5, -0.9];
        apply_gain(&mut s, db_to_linear(12.0));
        assert_eq!(s, vec![1.0, -1.0]);
    }

    #[test]
    fn downsampling_removes_what_would_alias() {
        // A 20 kHz tone at 48 kHz has no place in a 16 kHz signal; after
        // resampling it must be nearly gone. A 1 kHz tone must survive.
        let n = 4800;
        let tone = |f: f32| -> Vec<f32> {
            (0..n)
                .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / 48_000.0).sin())
                .collect()
        };
        let high = resample_linear(&tone(20_000.0), 48_000, 16_000);
        let low = resample_linear(&tone(1_000.0), 48_000, 16_000);
        let core = |v: &[f32]| rms(&v[v.len() / 4..v.len() * 3 / 4]);
        assert!(core(&high) < 0.02, "alias left: {}", core(&high));
        assert!(core(&low) > 0.6, "speech band damaged: {}", core(&low));
        assert_eq!(low.len(), 1600);
    }
}
