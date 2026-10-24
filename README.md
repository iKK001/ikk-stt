# ikk-stt

Dictation for people with impaired speech on Windows: microphone audio →
speech-to-text API (streaming or batch) → text at the blinking cursor of the
app in front.

Developed within a [HackaHealth Go!](https://hackahealth.ch) project.

## Build (Windows)

Needs Rust (stable), Flutter (stable) and Visual Studio with the
"Desktop development with C++" workload.

```
cargo test --workspace
cd apps/reference_app
flutter run -d windows
```

## License

GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later), see
[LICENSE](LICENSE) and [NOTICE](NOTICE).

Contributions are welcome and accepted under the same license
(AGPL-3.0-or-later).
