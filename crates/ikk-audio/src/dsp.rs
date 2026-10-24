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
pub fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || input.is_empty() || from_hz == 0 || to_hz == 0 {
        return input.to_vec();
    }
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
}
