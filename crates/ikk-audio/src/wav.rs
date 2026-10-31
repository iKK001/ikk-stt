//! Plain 16-bit PCM WAV, read and written without a dependency: the
//! recordings the replay microphone plays and `ikk-cli record` writes.

use std::io::Write;
use std::path::Path;

use crate::AudioError;

/// Parse a 16-bit PCM WAV: (channels, rate, interleaved samples in -1..1).
pub fn parse(bytes: &[u8]) -> Result<(u16, u32, Vec<f32>), AudioError> {
    let bad = |what: &str| AudioError::Backend(format!("WAV: {what}"));
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if body + 16 <= bytes.len() => {
                if u16_at(body) != 1 {
                    return Err(bad("only 16-bit PCM is supported"));
                }
                channels = u16_at(body + 2);
                rate = u32_at(body + 4);
                bits = u16_at(body + 14);
            }
            b"data" => {
                if bits != 16 || channels == 0 || rate == 0 {
                    return Err(bad("only 16-bit PCM is supported"));
                }
                let end = (body + len).min(bytes.len());
                let samples = bytes[body..end]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
                return Ok((channels, rate, samples));
            }
            _ => {}
        }
        pos = body + len + (len & 1);
    }
    Err(bad("no data chunk"))
}

/// Read a WAV file as mono at `rate_hz` (downmixed and resampled).
pub fn read_mono(path: &Path, rate_hz: u32) -> Result<Vec<f32>, AudioError> {
    let bytes = std::fs::read(path).map_err(|e| AudioError::Backend(format!("{}: {e}", path.display())))?;
    let (channels, rate, samples) = parse(&bytes)?;
    let mono = crate::dsp::downmix_mono(&samples, channels as usize);
    Ok(crate::dsp::resample_linear(&mono, rate, rate_hz))
}

/// Write mono 16-bit PCM.
pub fn write(path: &Path, samples: &[f32], rate: u32) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * 2).to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        w.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_parse_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ikk-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let samples: Vec<f32> = (0..160).map(|i| (i as f32 / 160.0) - 0.5).collect();
        write(&path, &samples, 16_000).unwrap();
        let (channels, rate, back) = parse(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((channels, rate, back.len()), (1, 16_000, 160));
        assert!(back.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-3));
        let mono = read_mono(&path, 8_000).unwrap();
        assert_eq!(mono.len(), 80);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_other_files() {
        assert!(parse(b"ID3 not a wav").is_err());
    }
}
