//! Fetches nlprule's tokenizer and rule binaries for German and English
//! (once; cached by nlprule-build) and stores them gzipped next to the
//! gzipped dictionaries in OUT_DIR, where lib.rs embeds them.

use std::io::Write;

const LANGUAGES: &[&str] = &["de", "en"];
const DICTIONARIES: &[&str] = &["de_CH_frami.aff", "de_CH_frami.dic", "en_US.aff", "en_US.dic"];

fn gzip(from: &str, to: &str) {
    let data = std::fs::read(from).unwrap_or_else(|e| panic!("{from}: {e}"));
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&data).unwrap();
    std::fs::write(to, enc.finish().unwrap()).unwrap_or_else(|e| panic!("{to}: {e}"));
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dict");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    nlprule_build::BinaryBuilder::new(LANGUAGES, &out)
        .build()
        .expect("nlprule binaries (downloaded once at build time; needs network the first time)")
        .validate()
        .expect("nlprule binaries valid");
    for lang in LANGUAGES {
        for kind in ["tokenizer", "rules"] {
            let p = format!("{out}/{lang}_{kind}.bin");
            gzip(&p, &format!("{p}.gz"));
        }
    }
    let dict = format!("{}/dict", env!("CARGO_MANIFEST_DIR"));
    for f in DICTIONARIES {
        gzip(&format!("{dict}/{f}"), &format!("{out}/{f}.gz"));
    }
}
