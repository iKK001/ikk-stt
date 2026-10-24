//! Shared HTTP client and TLS setup.
//!
//! rustls with the pure-Rust `ring` provider and bundled Mozilla roots on
//! every target: no C toolchain for cross builds, no platform certificate
//! glue on Android/iOS. The same config serves reqwest (batch) and
//! tokio-tungstenite (streaming).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Installs `ring` as the process-wide rustls provider (idempotent) and
/// returns the shared client config with webpki roots.
pub fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            // Err = another provider already installed by the host process; fine.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .use_preconfigured_tls((*tls_config()).clone())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .user_agent(concat!("ikk-stt/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client")
    })
}
