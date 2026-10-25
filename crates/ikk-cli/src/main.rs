use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use ikk_audio::vad::{VadEvent, VadMode, VoiceActivityDetector};
use ikk_audio::{AudioFrame, CaptureConfig, PIPELINE_SAMPLE_RATE_HZ};
use ikk_engine::{Engine, EngineCommand, EngineEvent};
use ikk_output::{AccessLevel, OutputError, TargetStatus, TextSink};
use ikk_stt::{ProviderId, ProviderRegistry, Secret, TranscriptEvent};

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
    /// Dictate from the microphone to stdout: live interim line, finals appended.
    Dictate {
        #[arg(long, default_value = "deepgram")]
        provider: String,
        /// Environment variable holding the API key (default: <PROVIDER>_API_KEY).
        #[arg(long)]
        key_env: Option<String>,
        /// ISO-639-3 language, empty = auto-detect.
        #[arg(long, default_value = "deu")]
        language: String,
        #[arg(long)]
        model: Option<String>,
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
            write_wav(&out, &samples, PIPELINE_SAMPLE_RATE_HZ)?;
            println!(
                "\nwrote {} ({:.2} s)",
                out.display(),
                samples.len() as f64 / PIPELINE_SAMPLE_RATE_HZ as f64
            );
            Ok(())
        }
        Cmd::Dictate {
            provider,
            key_env,
            language,
            model,
            batch,
            seconds,
            mic,
            denoise,
            agc,
            to_cursor,
        } => {
            // Windows' own recognizer needs no key.
            let keyless = provider == "windows";
            let env_name = key_env.unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()));
            let mut key = if keyless {
                String::new()
            } else {
                std::env::var(&env_name).map_err(|_| anyhow::anyhow!("set {env_name} to your {provider} API key"))?
            };
            if provider == "voiceitt" {
                // Voiceitt needs App ID + API key; the credential is "APP_ID:API_KEY".
                let app_id =
                    std::env::var("VOICEITT_APP_ID").map_err(|_| anyhow::anyhow!("set VOICEITT_APP_ID as well"))?;
                key = format!("{app_id}:{key}");
            }
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
            settings.sound.denoise = if denoise {
                ikk_settings::Denoise::RnNoise
            } else {
                ikk_settings::Denoise::Off
            };
            let mut opts = settings.active_options();
            opts.model = model;
            opts.transfer = if batch {
                ikk_settings::TransferMode::Batch
            } else {
                ikk_settings::TransferMode::Streaming
            };
            settings.providers.options.insert(provider.clone(), opts);
            let mut start = ikk_engine::config::start_config(&settings);
            start.capture.agc = agc;
            start.flow_ms = if to_cursor { 10 } else { 0 };

            let sink: Box<dyn TextSink> = if to_cursor {
                ikk_output::default_sink()
            } else {
                Box::new(StdoutSink)
            };
            let mut engine = Engine::spawn(reg, sink);
            if !keyless {
                engine
                    .send(EngineCommand::SetCredential(pid.clone(), Secret::new(key)))
                    .await;
            }
            // Choosing a provider here (with its key, if it needs one) is
            // consent for the CLI; record it like the app would.
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
fn write_wav(path: &PathBuf, samples: &[f32], rate: u32) -> anyhow::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut w = std::fs::File::create(path)?;
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
    Ok(())
}

/// Sink that prints finals to stdout (the CLI's "cursor"): a line per final,
/// the interim preview is cleared first by the event loop.
struct StdoutSink;

impl TextSink for StdoutSink {
    fn access_level(&self) -> AccessLevel {
        AccessLevel::BasicTyping
    }
    fn probe(&mut self) -> TargetStatus {
        TargetStatus::Injectable(AccessLevel::BasicTyping)
    }
    fn type_text(&mut self, text: &str) -> Result<(), OutputError> {
        print!("\r{:60}\r{}\n", "", text.trim_start());
        Ok(())
    }
    fn backspace(&mut self, _n: usize) -> Result<(), OutputError> {
        Ok(())
    }
}
