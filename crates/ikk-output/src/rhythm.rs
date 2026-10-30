//! Typing rhythm: the pause before each character, chosen so that typing
//! looks like a person at the keyboard but never falls behind the speaker.
//!
//! A person types unevenly: a little faster or slower from key to key, a
//! short breath after a word, a longer one after a sentence, and now and
//! then a moment of hesitation. The planner reproduces that with a
//! deterministic pseudo-random sequence (seeded, so tests can replay it).
//!
//! Latency comes first: when text piles up behind the writer, the pauses
//! shrink so that everything waiting is typed within [`RhythmConfig::lag_budget_ms`].
//! As the backlog drains the pace relaxes again, which also looks natural
//! (a typist speeds up to catch up, then settles). The platform floor
//! ([`RhythmConfig::floor_ms`]) is never undercut: below it Windows loses
//! characters.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RhythmConfig {
    /// Typical pause between two letters of a word, before variation.
    pub base_ms: u32,
    /// Never below this (keystroke injection loses characters otherwise).
    pub floor_ms: u32,
    /// Everything that is waiting must be typed within this time.
    pub lag_budget_ms: u32,
}

impl Default for RhythmConfig {
    /// About 22 characters per second when relaxed: a fast typist.
    fn default() -> Self {
        Self {
            base_ms: 45,
            floor_ms: 5,
            lag_budget_ms: 1200,
        }
    }
}

/// Multipliers on the base pause.
const AFTER_SPACE: f32 = 1.5;
const AFTER_SENTENCE: f32 = 2.4;
const AFTER_CLAUSE: f32 = 1.7;
const HESITATION: f32 = 2.0;
/// Random variation from key to key: 0.75 … 1.35 of the base.
const JITTER_MIN: f32 = 0.75;
const JITTER_SPAN: f32 = 0.6;
/// A hesitation every 10 … 24 characters.
const HESITATION_MIN: u32 = 10;
const HESITATION_SPAN: u32 = 15;
/// A pause is never longer than this many base pauses, whatever the text.
const MAX_FACTOR: f32 = 4.0;

#[derive(Debug, Clone)]
pub struct Rhythm {
    cfg: RhythmConfig,
    state: u64,
    until_hesitation: u32,
}

impl Default for Rhythm {
    fn default() -> Self {
        Self::new(RhythmConfig::default(), 0x9E37_79B9_7F4A_7C15)
    }
}

impl Rhythm {
    pub fn new(cfg: RhythmConfig, seed: u64) -> Self {
        let mut r = Self {
            cfg,
            state: seed.max(1),
            until_hesitation: 0,
        };
        r.until_hesitation = r.next_hesitation();
        r
    }

    pub fn config(&self) -> RhythmConfig {
        self.cfg
    }

    /// xorshift64*: small, deterministic, good enough for timing variation.
    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    fn next_unit(&mut self) -> f32 {
        self.next_u32() as f32 / u32::MAX as f32
    }

    fn next_hesitation(&mut self) -> u32 {
        HESITATION_MIN + self.next_u32() % HESITATION_SPAN
    }

    /// The pause after `typed` was typed, with `pending` characters still
    /// waiting behind it.
    pub fn delay_after(&mut self, typed: char, pending: usize) -> Duration {
        let mut factor = match typed {
            ' ' | '\n' => AFTER_SPACE,
            '.' | '!' | '?' => AFTER_SENTENCE,
            ',' | ';' | ':' => AFTER_CLAUSE,
            _ => 1.0,
        };
        factor *= JITTER_MIN + JITTER_SPAN * self.next_unit();
        if self.until_hesitation == 0 {
            factor *= HESITATION;
            self.until_hesitation = self.next_hesitation();
        } else {
            self.until_hesitation -= 1;
        }
        let base = self.cfg.base_ms as f32;
        let human = (base * factor).min(base * MAX_FACTOR);
        // Whatever waits must be out within the budget.
        let cap = if pending == 0 {
            human
        } else {
            self.cfg.lag_budget_ms as f32 / pending as f32
        };
        let ms = human.min(cap).max(self.cfg.floor_ms as f32);
        Duration::from_micros((ms * 1000.0) as u64)
    }

    /// The pauses for typing `text` in one go (the host types it itself):
    /// one entry per character, the pause *after* that character.
    pub fn plan(&mut self, text: &str) -> Vec<Duration> {
        let n = text.chars().count();
        text.chars()
            .enumerate()
            .map(|(i, c)| self.delay_after(c, n - i - 1))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(d: Duration) -> f32 {
        d.as_secs_f32() * 1000.0
    }

    #[test]
    fn same_seed_same_rhythm_and_it_varies() {
        let mut a = Rhythm::new(RhythmConfig::default(), 7);
        let mut b = Rhythm::new(RhythmConfig::default(), 7);
        let pa = a.plan("das ist ein test");
        let pb = b.plan("das ist ein test");
        assert_eq!(pa, pb);
        let distinct: std::collections::HashSet<u64> = pa.iter().map(|d| d.as_micros() as u64).collect();
        assert!(distinct.len() > pa.len() / 2, "hardly any variation: {pa:?}");
    }

    #[test]
    fn breathes_after_words_and_sentences() {
        let cfg = RhythmConfig::default();
        let letters: f32 = (0..200)
            .map(|i| ms(Rhythm::new(cfg, i).delay_after('a', 0)))
            .sum::<f32>()
            / 200.0;
        let spaces: f32 = (0..200)
            .map(|i| ms(Rhythm::new(cfg, i).delay_after(' ', 0)))
            .sum::<f32>()
            / 200.0;
        let stops: f32 = (0..200)
            .map(|i| ms(Rhythm::new(cfg, i).delay_after('.', 0)))
            .sum::<f32>()
            / 200.0;
        assert!(letters < spaces && spaces < stops, "{letters} {spaces} {stops}");
        assert!((40.0..60.0).contains(&letters), "{letters}");
    }

    #[test]
    fn backlog_shrinks_the_pauses_but_never_below_the_floor() {
        let cfg = RhythmConfig::default();
        let mut r = Rhythm::new(cfg, 3);
        // 300 characters waiting: 1200 ms / 300 = 4 ms, clamped to the floor.
        let d = r.delay_after('a', 300);
        assert_eq!(ms(d), cfg.floor_ms as f32);
        // 40 waiting: at most 30 ms each, whatever the rhythm wanted.
        for _ in 0..50 {
            assert!(ms(r.delay_after('.', 40)) <= 30.01);
        }
        // Nothing waiting: the full relaxed pause.
        assert!(ms(r.delay_after('a', 0)) >= 30.0);
    }

    #[test]
    fn a_plan_types_a_whole_text_within_the_budget_plus_one_relaxed_tail() {
        let cfg = RhythmConfig::default();
        let text = "Dies ist ein langer Satz, der sofort am Cursor erscheinen soll. Und noch einer.";
        let total: f32 = Rhythm::new(cfg, 11).plan(text).iter().map(|d| ms(*d)).sum();
        let n = text.chars().count() as f32;
        // Harmonic sum of the budget over the backlog, plus the last pause.
        let bound = cfg.lag_budget_ms as f32 * (1.0 + n.ln()) + cfg.base_ms as f32 * MAX_FACTOR;
        assert!(total < bound, "{total} ms for {n} chars");
        assert!(
            total > cfg.lag_budget_ms as f32 * 0.8,
            "{total} ms: faster than a person could"
        );
    }
}
