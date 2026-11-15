//! Shows the floating buttons for a few seconds without the app, to look
//! at them: `cargo run -p ikk-overlay --example show`. Clicks are logged.

fn main() {
    {
        ikk_overlay::show(
            std::sync::Arc::new(|| eprintln!("microphone clicked")),
            Some(std::sync::Arc::new(|| eprintln!("correction clicked"))),
            Some(std::sync::Arc::new(|audio| eprintln!("switch audio={audio}"))),
            None,
        );
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}
