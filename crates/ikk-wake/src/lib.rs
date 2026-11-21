//! Offline voice commands that switch dictation on and off, for people who
//! cannot press a button.
//!
//! Speaker-dependent: the user records their own start command and stop
//! command — a word or a short phrase each (three takes each). Heard utterances are compared with those recordings
//! (MFCC features, dynamic time warping). Matching a person's own
//! pronunciation works for impaired speech where generic recognizers fail.
//! Everything runs on the device; no audio leaves it.
//!
//! Flow: [`Segmenter`] cuts single utterances out of the 16 kHz pipeline
//! frames with the voice activity detector; [`Spotter`] decides whether an
//! utterance is the start word, the stop word, or neither.

pub mod features;
pub mod matcher;
pub mod segment;

pub use matcher::{Command, CommandModel, Judgement, Spotter, VoiceCommands};
pub use segment::Segmenter;

/// Sample rate of the audio pipeline the features are computed at.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum WakeError {
    #[error("need at least {0} recordings")]
    TooFewTakes(usize),
    #[error("recording {0} is too short or silent; speak the command clearly")]
    TakeTooShort(usize),
    #[error("the recordings of this command differ too much; record them again more alike")]
    TakesInconsistent,
    #[error(
        "the start command and the stop command sound too alike to each other to be told apart; choose two that sound more different"
    )]
    WordsTooSimilar,
    #[error("stored voice commands are unreadable: {0}")]
    Storage(String),
}
