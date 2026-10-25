//! Synthetic "words": sequences of voiced sounds with their own pitch and
//! formant colour. Takes vary in speed, loudness and noise like a person
//! repeating a word.

use ikk_wake::{Command, Segmenter, Spotter, VoiceCommands, WakeError, SAMPLE_RATE_HZ};

/// (pitch Hz, formant 1 Hz, formant 2 Hz, duration s) per sound.
type Word = &'static [(f32, f32, f32, f32)];

const START: Word = &[
    (120.0, 300.0, 2300.0, 0.18),
    (130.0, 700.0, 1200.0, 0.22),
    (110.0, 500.0, 1800.0, 0.15),
];
const STOP: Word = &[
    (150.0, 600.0, 1000.0, 0.12),
    (140.0, 350.0, 900.0, 0.25),
    (160.0, 750.0, 1700.0, 0.12),
];
const OTHER: Word = &[(200.0, 400.0, 2600.0, 0.3), (210.0, 450.0, 2500.0, 0.3)];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 10_000) as f32 / 5_000.0 - 1.0
    }
}

fn speak(word: Word, speed: f32, gain: f32, noise: f32, seed: u64) -> Vec<f32> {
    let sr = SAMPLE_RATE_HZ as f32;
    let mut rng = Rng(seed | 1);
    let mut out = Vec::new();
    let mut phase = 0f32;
    for &(f0, f1, f2, dur) in word {
        let n = (dur * speed * sr) as usize;
        for i in 0..n {
            let env = (std::f32::consts::PI * i as f32 / n as f32).sin().max(0.0);
            phase += f0 / sr;
            // Harmonics weighted by two formant peaks: a crude vowel.
            let mut v = 0f32;
            for h in 1..40 {
                let fh = f0 * h as f32;
                if fh > 7000.0 {
                    break;
                }
                let w = 1.0 / (1.0 + ((fh - f1) / 90.0).powi(2)) + 0.6 / (1.0 + ((fh - f2) / 120.0).powi(2));
                v += w * (2.0 * std::f32::consts::PI * h as f32 * phase).sin();
            }
            out.push(gain * 0.1 * env * v + noise * rng.next());
        }
    }
    out
}

fn takes(word: Word) -> Vec<Vec<f32>> {
    vec![
        speak(word, 1.0, 1.0, 0.002, 1),
        speak(word, 0.9, 0.7, 0.003, 2),
        speak(word, 1.12, 1.3, 0.002, 3),
    ]
}

fn trained() -> Spotter {
    Spotter::new(VoiceCommands::train(&takes(START), &takes(STOP)).expect("train"))
}

#[test]
fn recognises_own_words_at_other_speed_and_loudness() {
    let s = trained();
    assert_eq!(s.detect(&speak(START, 1.05, 0.5, 0.004, 11)), Some(Command::Start));
    assert_eq!(s.detect(&speak(START, 0.88, 1.6, 0.004, 12)), Some(Command::Start));
    assert_eq!(s.detect(&speak(STOP, 1.08, 0.6, 0.004, 13)), Some(Command::Stop));
    assert_eq!(s.detect(&speak(STOP, 0.92, 1.4, 0.004, 14)), Some(Command::Stop));
}

#[test]
fn rejects_other_words_and_noise() {
    let s = trained();
    assert_eq!(s.detect(&speak(OTHER, 1.0, 1.0, 0.003, 21)), None);
    let mut rng = Rng(99);
    let noise: Vec<f32> = (0..SAMPLE_RATE_HZ as usize / 2).map(|_| 0.05 * rng.next()).collect();
    assert_eq!(s.detect(&noise), None);
    // The start word spoken far too slowly is not taken as the word.
    assert_eq!(s.detect(&speak(START, 2.6, 1.0, 0.003, 22)), None);
}

#[test]
fn refuses_identical_words_and_too_few_takes() {
    assert!(matches!(
        VoiceCommands::train(&takes(START), &takes(START)),
        Err(WakeError::WordsTooSimilar)
    ));
    assert!(matches!(
        VoiceCommands::train(&takes(START)[..2], &takes(STOP)),
        Err(WakeError::TooFewTakes(3))
    ));
}

#[test]
fn stores_and_loads() {
    let c = VoiceCommands::train(&takes(START), &takes(STOP)).unwrap();
    let back = VoiceCommands::from_json(&c.to_json()).unwrap();
    assert_eq!(
        Spotter::new(back).detect(&speak(STOP, 1.0, 1.0, 0.003, 31)),
        Some(Command::Stop)
    );
}

#[test]
fn segmenter_cuts_a_word_out_of_silence() {
    let mut seg = Segmenter::new();
    let mut rng = Rng(7);
    let mut stream: Vec<f32> = (0..SAMPLE_RATE_HZ as usize).map(|_| 0.001 * rng.next()).collect();
    let word = speak(STOP, 1.0, 1.0, 0.001, 41);
    stream.extend_from_slice(&word);
    stream.extend((0..SAMPLE_RATE_HZ as usize).map(|_| 0.001 * rng.next()));
    let mut got = Vec::new();
    for chunk in stream.chunks(160) {
        if let Some(u) = seg.push(chunk) {
            got.push(u);
        }
    }
    assert_eq!(got.len(), 1, "exactly one utterance");
    let len = got[0].len() as f32 / word.len() as f32;
    assert!((0.8..2.5).contains(&len), "utterance length ratio {len}");
    assert_eq!(trained().detect(&got[0]), Some(Command::Stop));
}
