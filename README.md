# ikk-stt

Dictation for people with impaired speech on Windows: microphone audio →
speech-to-text API (streaming or batch) → text at the blinking cursor of the
app in front.

Developed within a [HackaHealth Go!](https://hackahealth.ch) project.

## Install (Windows)

In PowerShell (no administrator rights needed):

```
irm https://raw.githubusercontent.com/iKK001/ikk-stt/main/install.ps1 | iex
```

This downloads the newest [release](https://github.com/iKK001/ikk-stt/releases),
checks its SHA-256, unpacks it into `%LOCALAPPDATA%\Programs\ikk-stt` and
adds a Start menu entry. Run the same line again to update. To remove it:

```
irm https://raw.githubusercontent.com/iKK001/ikk-stt/main/uninstall.ps1 | iex
```

The program is not code-signed: Windows may ask once whether to run it.
Without PowerShell, download `ikk-stt-windows-x64.zip` from the releases
page, unpack it anywhere and start `ikk-stt.exe`.

Releases are made by hand: Actions tab > release > "Run workflow".

## Build (Windows)

Needs Rust (stable), Flutter (stable) and Visual Studio with the
"Desktop development with C++" workload plus the individual component
"C++ ATL for latest build tools (x86 & x64)" (the key storage plugin needs
ATL; without it the build fails with `atlstr.h` not found).

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
