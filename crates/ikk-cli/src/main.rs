use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use ikk_audio::chain::{CaptureChain, ChainConfig};
use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};
use ikk_audio::{AudioFrame, CaptureConfig, PIPELINE_SAMPLE_RATE_HZ};
use ikk_engine::{Engine, EngineCommand, EngineEvent};
use ikk_output::{AccessLevel, OutputError, TargetStatus, TextSink};
use ikk_stt::{ProviderId, ProviderRegistry, Secret, SessionConfig, TranscriptEvent};

#[derive(Parser)]
#[command(name = "ikk-cli", about = "ikk-stt headless reference")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print core version, platform and compiled-in providers.
    Doctor,
    /// List input devices (microphones) as the platform backend reports them.
    Mics,
    /// Version, build, license and third-party notices (what an About screen shows).
    About {
        /// Print the full license, notice and third-party texts too.
        #[arg(long)]
        full: bool,
    },
    /// Live level meter from the microphone, with VAD speech start/end.
    Monitor {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Device id as listed by `mics`; default device if omitted.
        #[arg(long)]
        mic: Option<String>,
        #[arg(long)]
        denoise: bool,
        #[arg(long)]
        agc: bool,
        #[arg(long, default_value_t = 0.0)]
        preamp_db: f32,
    },
    /// Record the processed 16 kHz mono pipeline output to a WAV file.
    Record {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long)]
        mic: Option<String>,
        #[arg(long)]
        denoise: bool,
        #[arg(long)]
        agc: bool,
        #[arg(long, default_value_t = 0.0)]
        preamp_db: f32,
        #[arg(long, default_value = "ikk-record.wav")]
        out: PathBuf,
    },
    /// Training mode: say a word a few times; what the recogniser hears for
    /// it is stored as its heard forms in the settings file, and from then
    /// on written as the word. Say it after each "Say «word» …" prompt.
    Train {
        /// The word (or short phrase) as it must be written.
        word: String,
        #[arg(long, default_value_t = 5)]
        takes: u32,
        #[arg(long, default_value = "elevenlabs")]
        provider: String,
        #[arg(long)]
        key_env: Option<String>,
        /// Use the key the app saved for this provider (macOS).
        #[arg(long)]
        key_from_app: bool,
        /// Microphone id (default: the settings file); `file:<wav>` replays a
        /// recording as one take.
        #[arg(long)]
        mic: Option<String>,
        /// Seconds to wait for each take.
        #[arg(long, default_value_t = 10)]
        timeout: u32,
        /// Teach a heard form without recording (what the app's "Teach from
        /// the last text" does); repeatable. With it, no takes are recorded.
        #[arg(long)]
        heard: Vec<String>,
        /// Mark the word "always replace": a one-word heard form is swapped
        /// even when every reading agrees on it.
        #[arg(long)]
        always: bool,
    },
    /// Transcribe a WAV recording the way the app would: through the capture
    /// chain, cut at gaps and pauses, chunks uploaded in parallel and printed
    /// in order. For comparing providers and settings on the same sample.
    /// RUST_LOG=ikk_stt=debug shows every cut and why it was made.
    Transcribe {
        /// 16-bit PCM WAV, mono or stereo, any rate.
        file: PathBuf,
        #[arg(long, default_value = "deepgram")]
        provider: String,
        /// Environment variable holding the API key (default: <PROVIDER>_API_KEY).
        #[arg(long)]
        key_env: Option<String>,
        /// Use the key the app saved for this provider (macOS login keychain
        /// of a debug build) instead of an environment variable.
        #[arg(long)]
        key_from_app: bool,
        /// ISO-639-3 language, empty = auto-detect.
        #[arg(long, default_value = "deu")]
        language: String,
        #[arg(long)]
        model: Option<String>,
        /// Send the whole file as one request instead of chunks.
        #[arg(long)]
        whole: bool,
        /// Stream it live (provider's own segmentation), paced in real time
        /// as a microphone would deliver it.
        #[arg(long)]
        live: bool,
        #[arg(long)]
        denoise: bool,
        #[arg(long)]
        agc: bool,
        /// Turn the pauses between words down (breathing, ventilator).
        #[arg(long)]
        gate: bool,
        /// High-pass cutoff in Hz (portable tone shaping).
        #[arg(long, default_value_t = 0.0)]
        highpass: f32,
        /// Presence lift at 3 kHz in dB.
        #[arg(long, default_value_t = 0.0)]
        presence: f32,
        /// Fast compressor (flattens a rolled R's flutter).
        #[arg(long)]
        compress: bool,
        /// Trill filler (lifts the closures of a rolled R).
        #[arg(long)]
        trill: bool,
        /// Learnt steady-noise profile subtracted (ventilator, fan).
        #[arg(long)]
        profile: bool,
        /// Microphone-path equaliser: 18 third-octave gains in dB, comma-separated.
        #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
        path_eq: Vec<f32>,
        /// After a pause, transcribe the whole thought again in one piece
        /// and replace the chunk texts.
        #[arg(long)]
        refine: bool,
        /// Agreed words only: transcribe the thought so far every this many
        /// ms and type only words the last --agree results agree on.
        #[arg(long)]
        agreement_ms: Option<u32>,
        #[arg(long, default_value_t = 3)]
        agree: usize,
        /// Earliest cut at a gap between words, ms (default: the settings file).
        #[arg(long)]
        chunk_ms: Option<u32>,
        /// Pause that cuts a chunk, ms (default: the settings file).
        #[arg(long)]
        pause_ms: Option<u32>,
        /// Words to bias the recogniser towards (comma-separated).
        #[arg(long, value_delimiter = ',')]
        keywords: Vec<String>,
    },
    /// Dictate from the microphone to stdout: live interim line, finals appended.
    /// `--mic file:/path/to.wav` replays a recording as the microphone.
    Dictate {
        #[arg(long, default_value = "deepgram")]
        provider: String,
        /// Environment variable holding the API key (default: <PROVIDER>_API_KEY).
        #[arg(long)]
        key_env: Option<String>,
        /// Use the key the app saved for this provider (macOS).
        #[arg(long)]
        key_from_app: bool,
        /// ISO-639-3 language, empty = auto-detect.
        #[arg(long, default_value = "deu")]
        language: String,
        #[arg(long)]
        model: Option<String>,
        /// Earliest cut at a gap between words, ms (default: the settings file).
        #[arg(long)]
        chunk_ms: Option<u32>,
        /// Pause that cuts a chunk, ms (default: the settings file).
        #[arg(long)]
        pause_ms: Option<u32>,
        /// Turn the pauses between words down (breathing, ventilator).
        #[arg(long)]
        gate: bool,
        /// High-pass cutoff in Hz (portable tone shaping).
        #[arg(long, default_value_t = 0.0)]
        highpass: f32,
        /// Presence lift at 3 kHz in dB.
        #[arg(long, default_value_t = 0.0)]
        presence: f32,
        /// Fast compressor (flattens a rolled R's flutter).
        #[arg(long)]
        compress: bool,
        /// Trill filler (lifts the closures of a rolled R).
        #[arg(long)]
        trill: bool,
        /// Learnt steady-noise profile subtracted (ventilator, fan).
        #[arg(long)]
        profile: bool,
        /// Microphone-path equaliser: 18 third-octave gains in dB, comma-separated.
        #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
        path_eq: Vec<f32>,
        /// After a pause, transcribe the whole thought again in one piece.
        #[arg(long)]
        refine: bool,
        /// Agreed words only (see `transcribe --agreement-ms`); 0 turns the
        /// settings file's agreement mode off.
        #[arg(long)]
        agreement_ms: Option<u32>,
        /// Words and phrases to bias the recogniser towards (comma-separated).
        #[arg(long, value_delimiter = ',')]
        keywords: Vec<String>,
        /// Chunked batch upload instead of live streaming.
        #[arg(long)]
        batch: bool,
        /// Stop after this many seconds (0 = until Ctrl-C).
        #[arg(long, default_value_t = 0)]
        seconds: u64,
        #[arg(long)]
        mic: Option<String>,
        #[arg(long)]
        denoise: bool,
        #[arg(long)]
        agc: bool,
        /// Type into the focused app (real desktop injection) instead of stdout.
        #[arg(long)]
        to_cursor: bool,
    },
    /// Show or edit the persisted settings (settings.json).
    Settings {
        #[command(subcommand)]
        cmd: SettingsCmd,
    },
    /// Show or change persisted consent decisions (consent.json).
    Consent {
        #[command(subcommand)]
        cmd: ConsentCmd,
    },
    /// Real-machine typing proof: waits 3 s, then types a fixed test string
    /// (umlauts, guillemets, > 256 chars) into whatever window has focus.
    TypeTest {
        #[arg(long, default_value_t = 3)]
        wait_seconds: u64,
        /// Pause per character in milliseconds (default: the sink's minimum).
        #[arg(long)]
        delay_ms: Option<u32>,
        /// Wait for the target to process each character before the next.
        #[arg(long)]
        sync: bool,
        /// Insert through the clipboard instead of keystrokes.
        #[arg(long)]
        paste: bool,
        /// Keystrokes without waiting for each character to appear.
        #[arg(long, conflicts_with = "paste")]
        blind: bool,
    },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Print the settings file path and contents.
    Show,
    /// Set the active provider, e.g. `settings provider openai`.
    Provider { id: String },
    /// Set the language (ISO-639-3), e.g. `settings language eng`.
    Language { code: String },
    /// Set the microphone device id (from `mics`), or `default`.
    Mic { id: String },
}

#[derive(Subcommand)]
enum ConsentCmd {
    /// List recorded decisions.
    List,
    /// Record consent for a provider.
    Grant { provider: String },
    /// Withdraw consent for a provider.
    Revoke { provider: String },
}

fn settings_path() -> anyhow::Result<std::path::PathBuf> {
    ikk_settings::Settings::default_path().ok_or_else(|| anyhow::anyhow!("no config directory on this platform"))
}
fn consent_path() -> anyhow::Result<std::path::PathBuf> {
    ikk_consent::ConsentState::default_path().ok_or_else(|| anyhow::anyhow!("no config directory on this platform"))
}

/// Fixed de-CH diagnostic string: umlauts, accents, guillemets, punctuation
/// and a run > 256 chars (several injection chunks).
const TYPE_TEST_STRING: &str = "das ist ein Test — äöü ÄÖÜ éèàô «Zürich» ß, . ? ! 20 % · \
Schnürlischrift funktioniert. Der schnelle braune Fuchs springt über den faulen Hund. \
The quick brown fox jumps over the lazy dog 0123456789 (de-CH Layout-Härte-Probe).";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log messages (RUST_LOG, default info) for diagnosing providers.
    ikk_ffi::engine::init_logging();
    match Cli::parse().cmd {
        Cmd::Doctor => {
            println!("ikk core {}", ikk_ffi::core_version());
            println!("providers: {}", ikk_ffi::provider_ids().join(", "));
            println!("output: {:?}", ikk_output::default_sink().access_level());
            Ok(())
        }
        Cmd::About { full } => {
            let a = ikk_ffi::about();
            println!(
                "ikk core {} (build {}, {}, {}) {}-{}",
                a.core_version, a.build_number, a.git_sha, a.build_date, a.target_os, a.target_arch
            );
            println!("core license: {}", a.core_license);
            println!("providers: {}", a.providers.join(", "));
            if full {
                println!("\n===== LICENSE =====\n{}", ikk_ffi::license_text());
                println!("\n===== NOTICE =====\n{}", ikk_ffi::notice_text());
                println!("\n===== THIRD PARTY =====\n{}", ikk_ffi::third_party_text());
            }
            Ok(())
        }
        Cmd::Settings { cmd } => {
            let path = settings_path()?;
            let mut s = ikk_settings::Settings::load(&path)?;
            match cmd {
                SettingsCmd::Show => {
                    println!("{}\n{}", path.display(), s.to_json()?);
                    return Ok(());
                }
                SettingsCmd::Provider { id } => s.providers.active = id,
                SettingsCmd::Language { code } => s.providers.language = code,
                SettingsCmd::Mic { id } => s.microphone.device_id = (id != "default").then_some(id),
            }
            s.save(&path)?;
            println!("saved {}", path.display());
            Ok(())
        }
        Cmd::Consent { cmd } => {
            let path = consent_path()?;
            let mut c = ikk_consent::ConsentState::load(&path)?;
            match cmd {
                ConsentCmd::List => {
                    println!("{}", path.display());
                    for (id, r) in &c.records {
                        println!("{id:<12} {:?}  at {}  {}", r.decision, r.at, r.recipient.privacy_url);
                    }
                    return Ok(());
                }
                ConsentCmd::Grant { provider } => c.allow(ikk_consent::recipient_for(&provider, &provider)),
                ConsentCmd::Revoke { provider } => c.revoke(&provider),
            }
            c.save(&path)?;
            println!("saved {}", path.display());
            Ok(())
        }
        Cmd::TypeTest {
            wait_seconds,
            delay_ms,
            sync,
            paste,
            blind,
        } => {
            let mut sink: Box<dyn TextSink> = {
                let mut s = ikk_output::desktop::DesktopSink::new().with_sync_typing(sync);
                if paste {
                    s = s.with_paste_mode(true);
                }
                if blind {
                    s = s.with_confirmed_typing(false);
                }
                if let Some(ms) = delay_ms {
                    s = s.with_typing_delay(ms);
                }
                Box::new(s)
            };
            eprintln!("focus the target window now … typing in {wait_seconds} s");
            tokio::time::sleep(Duration::from_secs(wait_seconds)).await;
            let status = sink.probe();
            eprintln!("probe: {status:?}");
            let started = Instant::now();
            sink.type_text(TYPE_TEST_STRING)?;
            let took = started.elapsed();
            tokio::time::sleep(Duration::from_millis(300)).await;
            eprintln!(
                "typed {} chars in {:.1} s; read-back: {:?}",
                TYPE_TEST_STRING.chars().count(),
                took.as_secs_f32(),
                sink.verify(TYPE_TEST_STRING)
            );
            eprintln!("expected:\n{TYPE_TEST_STRING}");
            Ok(())
        }
        Cmd::Mics => {
            let devices = ikk_audio::list_input_devices()?;
            if devices.is_empty() {
                println!("no input devices found");
            }
            for d in devices {
                println!("{}{}    [id: {}]", if d.is_default { "* " } else { "  " }, d.name, d.id);
            }
            Ok(())
        }
        Cmd::Monitor {
            seconds,
            mic,
            denoise,
            agc,
            preamp_db,
        } => {
            let cfg = CaptureConfig {
                device_id: mic,
                preamp_gain_db: preamp_db,
                denoise,
                agc,
                ..Default::default()
            };
            let frames = capture_for(seconds, &cfg, true).await?;
            let total: usize = frames.iter().map(|f| f.samples.len()).sum();
            println!(
                "\n{} frames, {:.2} s of audio",
                frames.len(),
                total as f64 / PIPELINE_SAMPLE_RATE_HZ as f64
            );
            Ok(())
        }
        Cmd::Train {
            word,
            takes,
            provider,
            key_env,
            key_from_app,
            mic,
            timeout,
            heard,
            always,
        } => {
            if !heard.is_empty() || always {
                let path = settings_path()?;
                let mut stored = ikk_settings::Settings::load(&path).unwrap_or_default();
                for h in &heard {
                    stored.text.training.learn(&word, h);
                }
                if always {
                    stored.text.training.learn(&word, "");
                    if let Some(e) = stored.text.training.words.iter_mut().find(|e| e.word == word) {
                        e.always = true;
                    }
                }
                stored.save(&path)?;
                if let Some(e) = stored.text.training.words.iter().find(|e| e.word == word) {
                    let forms: Vec<String> = e.heard.iter().map(|h| format!("{} ×{}", h.text, h.count)).collect();
                    println!("«{word}»: heard as {}", forms.join(", "));
                }
                return Ok(());
            }
            let key = if key_from_app {
                credential_from_app(&provider)?
            } else {
                credential_from_env(&provider, key_env.as_deref())?
            };
            let path = settings_path()?;
            let mut settings = ikk_settings::Settings::load(&path).unwrap_or_default();
            settings.providers.active = provider.clone();
            if let Some(m) = mic {
                settings.microphone.device_id = Some(m);
            }
            let mut reg = ProviderRegistry::new();
            ikk_providers::register_all(&mut reg);
            let Some(prov) = reg.get(&ProviderId(provider.clone())) else {
                anyhow::bail!("unknown provider {provider}");
            };
            for n in 1..=takes {
                println!("Say «{word}» … ({n} of {takes})");
                match ikk_engine::training::take(
                    prov.clone(),
                    &settings,
                    Secret::new(key.clone()),
                    Duration::from_secs(timeout as u64),
                )
                .await
                {
                    Ok(heard) if heard.is_empty() => println!("  heard nothing in it"),
                    Ok(heard) => {
                        println!("  heard: {heard}");
                        settings.text.training.learn(&word, &heard);
                    }
                    Err(e) => println!("  {e}"),
                }
            }
            // Only the learned words are written back; provider and
            // microphone chosen for the takes stay as the app has them.
            let mut stored = ikk_settings::Settings::load(&path).unwrap_or_default();
            stored.text.training = settings.text.training.clone();
            stored.save(&path)?;
            if let Some(e) = settings.text.training.words.iter().find(|e| e.word == word) {
                let forms: Vec<String> = e.heard.iter().map(|h| format!("{} ×{}", h.text, h.count)).collect();
                println!(
                    "«{word}»: heard as {}",
                    if forms.is_empty() {
                        "— (always recognised)".to_string()
                    } else {
                        forms.join(", ")
                    }
                );
            }
            println!("saved to {}", path.display());
            Ok(())
        }
        Cmd::Record {
            seconds,
            mic,
            denoise,
            agc,
            preamp_db,
            out,
        } => {
            let cfg = CaptureConfig {
                device_id: mic,
                preamp_gain_db: preamp_db,
                denoise,
                agc,
                ..Default::default()
            };
            let frames = capture_for(seconds, &cfg, true).await?;
            let samples: Vec<f32> = frames.iter().flat_map(|f| f.samples.iter().copied()).collect();
            ikk_audio::wav::write(&out, &samples, PIPELINE_SAMPLE_RATE_HZ)?;
            println!(
                "\nwrote {} ({:.2} s)",
                out.display(),
                samples.len() as f64 / PIPELINE_SAMPLE_RATE_HZ as f64
            );
            Ok(())
        }
        Cmd::Transcribe {
            file,
            provider,
            key_env,
            key_from_app,
            language,
            model,
            whole,
            live,
            denoise,
            agc,
            gate,
            highpass,
            presence,
            compress,
            trill,
            profile,
            path_eq,
            refine,
            agreement_ms,
            agree,
            chunk_ms,
            pause_ms,
            keywords,
        } => {
            let key = if key_from_app {
                credential_from_app(&provider)?
            } else {
                credential_from_env(&provider, key_env.as_deref())?
            };
            let mut reg = ProviderRegistry::new();
            ikk_providers::register_all(&mut reg);
            let pid = ProviderId(provider.clone());
            let Some(prov) = reg.get(&pid) else {
                anyhow::bail!("unknown provider {provider}");
            };
            let settings = settings_path()
                .and_then(|p| Ok(ikk_settings::Settings::load(&p)?))
                .unwrap_or_default();
            let mut cfg = SessionConfig::new(provider.clone());
            cfg.credential = Some(Secret::new(key));
            cfg.language = language;
            cfg.model = model;
            cfg.transfer = if live {
                ikk_stt::TransferMode::Streaming
            } else {
                ikk_stt::TransferMode::Batch
            };
            cfg.refine = refine;
            cfg.agreement = agreement_ms.map(|ms| ikk_stt::Agreement {
                step: Duration::from_millis(ms as u64),
                windows: agree,
            });
            cfg.keywords = keywords.into_iter().filter(|k| !k.is_empty()).map(|k| (k, 5)).collect();
            cfg.learned = settings.text.training.learned_words();
            cfg.chunk_interval = Duration::from_millis(chunk_ms.unwrap_or(settings.sound.vad.chunk_interval_ms) as u64);
            let pause = pause_ms.unwrap_or(settings.sound.vad.silence_duration_ms);
            cfg.endpointing = Duration::from_millis(pause as u64);

            let frames = wav_frames(
                &file,
                denoise,
                agc,
                gate,
                Shaping {
                    highpass,
                    presence,
                    compress,
                    trill,
                    profile,
                    path_eq: path_eq.clone(),
                },
            )?;
            let total: f64 = frames.iter().map(|f| f.duration().as_secs_f64()).sum();
            eprintln!(
                "{} ({total:.1} s) → {provider}, {}{}{}{}",
                file.display(),
                if whole {
                    "whole file".to_string()
                } else if live {
                    format!("live stream, pause {pause} ms")
                } else {
                    format!("chunks ≥ {} ms, pause {pause} ms", cfg.chunk_interval.as_millis())
                },
                if denoise { ", denoise" } else { "" },
                if agc { ", agc" } else { "" },
                if gate { ", gate" } else { "" },
            );
            let started = Instant::now();
            if whole {
                let t = prov.transcribe(ikk_stt::AudioClip { frames }, &cfg).await?;
                println!("{}", t.text.trim());
                eprintln!("done in {:.1} s", started.elapsed().as_secs_f32());
                return Ok(());
            }
            let mut session = ikk_stt::open_session(prov, &cfg).await?;
            let mut vad = VoiceActivityDetector::new(VadMode::Normal, PIPELINE_SAMPLE_RATE_HZ);
            vad.set_pause_duration_ms(pause.max(100));
            let print = |ev: TranscriptEvent, at: Duration| match ev {
                TranscriptEvent::Final(t) => println!("[{:5.1}s] {t}", at.as_secs_f32()),
                TranscriptEvent::Refined { segments, text } => {
                    println!("[{:5.1}s] ⟲ replaces last {segments}: {text}", at.as_secs_f32())
                }
                TranscriptEvent::Error(e) => eprintln!("[provider error] {e:?}"),
                TranscriptEvent::Interim(_) | TranscriptEvent::SegmentRefine(_) => {}
                other => eprintln!("[{other:?}]"),
            };
            let mut clock = started;
            for frame in frames {
                if live {
                    // Real-time pacing: the provider segments by its own clock.
                    clock += frame.duration();
                    tokio::time::sleep_until(clock.into()).await;
                }
                if vad.process(&frame.samples) == Some(VadEvent::SpeechEnded) {
                    session.cut_segment().await?;
                }
                session.push(frame).await?;
                while let Ok(ev) = session.events().try_recv() {
                    print(ev, started.elapsed());
                }
            }
            session.finish().await?;
            while let Some(ev) = session.events().recv().await {
                let closed = matches!(ev, TranscriptEvent::Closed);
                print(ev, started.elapsed());
                if closed {
                    break;
                }
            }
            eprintln!("done in {:.1} s", started.elapsed().as_secs_f32());
            Ok(())
        }
        Cmd::Dictate {
            provider,
            key_env,
            key_from_app,
            language,
            model,
            chunk_ms,
            pause_ms,
            gate,
            highpass,
            presence,
            compress,
            trill,
            profile,
            path_eq,
            refine,
            agreement_ms,
            keywords,
            batch,
            seconds,
            mic,
            denoise,
            agc,
            to_cursor,
        } => {
            // Windows' own recognizer (test only) needs no key.
            let keyless = provider == "windows";
            let key = if keyless {
                String::new()
            } else if key_from_app {
                credential_from_app(&provider)?
            } else {
                credential_from_env(&provider, key_env.as_deref())?
            };
            let mut reg = ProviderRegistry::new();
            ikk_providers::register_all(&mut reg);
            let pid = ProviderId(provider.clone());
            if reg.get(&pid).is_none() {
                anyhow::bail!("unknown provider {provider}");
            }

            // Persisted settings (if any) → start config, then the CLI's own overrides.
            let mut settings = settings_path()
                .and_then(|p| Ok(ikk_settings::Settings::load(&p)?))
                .unwrap_or_default();
            settings.providers.active = provider.clone();
            settings.providers.language = language;
            settings.microphone.device_id = mic;
            if let Some(ms) = chunk_ms {
                settings.sound.vad.chunk_interval_ms = ms;
            }
            if let Some(ms) = pause_ms {
                settings.sound.vad.silence_duration_ms = ms;
            }
            if refine {
                settings.sound.vad.refine = true;
                settings.sound.vad.agreement = false;
            }
            if let Some(ms) = agreement_ms {
                settings.sound.vad.agreement = ms > 0;
                if ms > 0 {
                    settings.sound.vad.agreement_step_ms = ms;
                }
            }
            settings.sound.denoise = if denoise {
                ikk_settings::Denoise::RnNoise
            } else {
                ikk_settings::Denoise::Off
            };
            let mut opts = settings.active_options();
            opts.model = model;
            if !keywords.is_empty() {
                opts.keywords = keywords
                    .into_iter()
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .collect();
            }
            opts.transfer = if batch {
                ikk_settings::TransferMode::Batch
            } else {
                ikk_settings::TransferMode::Streaming
            };
            settings.providers.options.insert(provider.clone(), opts);
            let mut start = ikk_engine::config::start_config(&settings);
            start.capture.agc = agc;
            start.capture.gate = gate;
            start.capture.highpass_hz = highpass;
            start.capture.presence_db = presence;
            start.capture.compressor = compress;
            start.capture.trill_filler = trill;
            start.capture.noise_profile = profile;
            start.capture.path_eq_db = path_eq;
            start.flow_ms = if to_cursor { 10 } else { 0 };

            let sink: Box<dyn TextSink> = if to_cursor {
                ikk_output::default_sink()
            } else {
                Box::new(StdoutSink {
                    started: Instant::now(),
                })
            };
            let mut engine = Engine::spawn(reg, sink);
            if !keyless {
                engine
                    .send(EngineCommand::SetCredential(pid.clone(), Secret::new(key)))
                    .await;
            }
            // Choosing a provider here with its key is consent for the CLI;
            // record it like the app would.
            eprintln!("consent recorded for {provider}: audio is sent to this provider");
            let consent_file = consent_path().ok();
            if let Some(p) = &consent_file {
                if let Ok(c) = ikk_consent::ConsentState::load(p) {
                    engine.send(EngineCommand::LoadConsent(c)).await;
                }
            }
            engine.send(EngineCommand::GrantConsent(provider.clone())).await;
            engine.send(EngineCommand::Start(Box::new(start))).await;
            eprintln!(
                "listening ({provider}, {}) … Ctrl-C or {seconds}s to stop",
                if batch { "batch" } else { "streaming" }
            );

            let deadline = if seconds == 0 {
                None
            } else {
                Some(Instant::now() + Duration::from_secs(seconds))
            };
            let mut out = std::io::stdout();
            let mut interim_len = 0usize;
            let mut stopping = false;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c(), if !stopping => {
                        stopping = true;
                        eprintln!("\nfinishing …");
                        engine.send(EngineCommand::Stop).await;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)), if !stopping => {
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            stopping = true;
                            eprintln!("\nfinishing …");
                            engine.send(EngineCommand::Stop).await;
                        }
                    }
                    ev = engine.events.recv() => match ev {
                        Some(EngineEvent::Transcript(TranscriptEvent::Interim(t))) => {
                            print!("\r{:width$}\r{t}", "", width = interim_len);
                            interim_len = t.chars().count();
                            let _ = out.flush();
                        }
                        Some(EngineEvent::Typed(_)) => interim_len = 0,
                        Some(EngineEvent::ConsentChanged(c)) => {
                            if let Some(p) = &consent_file {
                                let _ = c.save(p);
                            }
                        }
                        Some(EngineEvent::Transcript(TranscriptEvent::Error(e))) => eprintln!("\n[provider error] {e:?}"),
                        Some(EngineEvent::Error(e)) => eprintln!("\n[error] {e}"),
                        Some(EngineEvent::SetupRequired { message, settings_url, .. }) => {
                            eprintln!("\n[setup needed] {message}");
                            if !settings_url.is_empty() {
                                eprintln!("open it with:  start {settings_url}");
                            }
                            break;
                        }
                        Some(EngineEvent::Command(c)) => eprintln!("\n[command] {c:?}"),
                        Some(EngineEvent::FallbackRequired { reason, .. }) => eprintln!("\n[fallback] {reason:?}"),
                        Some(EngineEvent::State(state)) if !stopping || state != ikk_engine::EngineState::Idle => {
                            eprintln!("\n[state] {state:?}");
                            if state == ikk_engine::EngineState::Error {
                                break;
                            }
                        }
                        Some(EngineEvent::State(ikk_engine::EngineState::Idle)) if stopping => break,
                        Some(EngineEvent::State(ikk_engine::EngineState::Error)) => break,
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            engine.shutdown().await;
            Ok(())
        }
    }
}

/// Runs the capture for `seconds`, prints a level bar and VAD events.
async fn capture_for(seconds: u64, cfg: &CaptureConfig, show_meter: bool) -> anyhow::Result<Vec<AudioFrame>> {
    let mut cap = ikk_audio::open_capture(cfg)?;
    let mut vad = VoiceActivityDetector::new(VadMode::Normal, PIPELINE_SAMPLE_RATE_HZ);
    vad.set_pause_duration_ms(900);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let mut frames = Vec::new();
    let mut last_meter = Instant::now();
    let mut out = std::io::stdout();
    while Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_millis(100), cap.frames().recv()).await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => continue,
        };
        if let Some(ev) = vad.process(&frame.samples) {
            let t = start.elapsed().as_secs_f32();
            match ev {
                VadEvent::SpeechStarted => print!("\n[{t:5.2}s] speech started"),
                VadEvent::SpeechEnded => print!("\n[{t:5.2}s] speech ended"),
            }
            println!();
        }
        if show_meter && last_meter.elapsed() >= Duration::from_millis(100) {
            last_meter = Instant::now();
            let dbfs = frame.level_dbfs();
            let bars = (((dbfs + 60.0) / 60.0).clamp(0.0, 1.0) * 40.0) as usize;
            print!(
                "\r{:>6.1} dBFS |{:<40}| {}",
                dbfs,
                "#".repeat(bars),
                if vad.is_speaking() { "SPEECH" } else { "      " }
            );
            let _ = out.flush();
        }
        frames.push(frame);
    }
    cap.stop();
    Ok(frames)
}

/// Minimal 16-bit PCM WAV writer (no dependency).
/// The API key from `<PROVIDER>_API_KEY` (or `key_env`), completed with the
/// second part some providers need: Azure's region, Voiceitt's App ID.
fn credential_from_env(provider: &str, key_env: Option<&str>) -> anyhow::Result<String> {
    let env_name = key_env
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()));
    let mut key = std::env::var(&env_name).map_err(|_| anyhow::anyhow!("set {env_name} to your {provider} API key"))?;
    if provider == "azure" && !key.contains(':') {
        let region =
            std::env::var("AZURE_REGION").map_err(|_| anyhow::anyhow!("set AZURE_REGION as well, e.g. westeurope"))?;
        key = format!("{region}:{key}");
    }
    if provider == "voiceitt" {
        let app_id = std::env::var("VOICEITT_APP_ID").map_err(|_| anyhow::anyhow!("set VOICEITT_APP_ID as well"))?;
        key = format!("{app_id}:{key}");
    }
    Ok(key)
}

/// The credential the app saved for `provider` (`api_key:<provider>` in
/// flutter_secure_storage, already in the provider's own format such as
/// REGION:KEY). macOS debug builds keep it in the login keychain, which the
/// `security` tool can read; the key never touches the terminal.
fn credential_from_app(provider: &str) -> anyhow::Result<String> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!(
            "--key-from-app is only available on macOS; set {}_API_KEY",
            provider.to_uppercase()
        );
    }
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-a", &format!("api_key:{provider}"), "-w"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("no key for {provider} in the app's keychain (save one in the app first)");
    }
    let key = String::from_utf8(out.stdout)?.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("the app's key for {provider} is empty");
    }
    Ok(key)
}

/// A WAV file as the capture would deliver it: mono 16 kHz, through the
/// capture chain (noise model listening, so chunks cut at real gaps), in
/// 20 ms frames.
/// Tone shaping flags shared by the file replay commands.
struct Shaping {
    highpass: f32,
    presence: f32,
    compress: bool,
    trill: bool,
    profile: bool,
    path_eq: Vec<f32>,
}

fn wav_frames(
    path: &std::path::Path,
    denoise: bool,
    agc: bool,
    gate: bool,
    shape: Shaping,
) -> anyhow::Result<Vec<AudioFrame>> {
    let pcm = ikk_audio::wav::read_mono(path, PIPELINE_SAMPLE_RATE_HZ)?;
    let mut chain = CaptureChain::new(ChainConfig {
        denoise,
        listen: true,
        gate,
        highpass_hz: shape.highpass,
        presence_db: shape.presence,
        compressor: shape.compress,
        trill_filler: shape.trill,
        noise_profile: shape.profile,
        path_eq_db: shape.path_eq,
        agc,
        agc_target_dbfs: -16.0,
        sample_rate_hz: PIPELINE_SAMPLE_RATE_HZ,
        ..Default::default()
    });
    let block = PIPELINE_SAMPLE_RATE_HZ as usize / 50;
    let mut frames = Vec::with_capacity(pcm.len() / block + 1);
    let mut sent = 0usize;
    for chunk in pcm.chunks(block) {
        let processed = chain.process(chunk).into_owned();
        if processed.is_empty() {
            continue;
        }
        let offset = Duration::from_secs_f64(sent as f64 / PIPELINE_SAMPLE_RATE_HZ as f64);
        sent += processed.len();
        frames.push(AudioFrame {
            samples: processed,
            sample_rate_hz: PIPELINE_SAMPLE_RATE_HZ,
            offset,
            speech: chain.speech_probability(),
        });
    }
    Ok(frames)
}

/// Sink that prints finals to stdout (the CLI's "cursor"): a line per final,
/// the interim preview is cleared first by the event loop.
struct StdoutSink {
    started: Instant,
}

impl TextSink for StdoutSink {
    fn access_level(&self) -> AccessLevel {
        AccessLevel::BasicTyping
    }
    fn probe(&mut self) -> TargetStatus {
        TargetStatus::Injectable(AccessLevel::BasicTyping)
    }
    fn type_text(&mut self, text: &str) -> Result<(), OutputError> {
        // Seconds since the sink was made: when each word reached the "cursor".
        print!(
            "\r{:60}\r[{:5.1}s] {}\n",
            "",
            self.started.elapsed().as_secs_f32(),
            text.trim_start()
        );
        Ok(())
    }
    fn backspace(&mut self, _n: usize) -> Result<(), OutputError> {
        Ok(())
    }
}
