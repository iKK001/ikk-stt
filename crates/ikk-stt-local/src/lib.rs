//! On-device whisper provider. Heavy native build (libclang, optional GPU
//! backends), therefore its own crate and off by default. Desktop only.

#[cfg(feature = "local-whisper")]
pub mod whisper {
    //! Local whisper-rs transcription goes here.
}
