//! Version, legal texts, devices, settings and consent files.

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
    // The default utils install a `log` logger at trace level, which dumps
    // every WebSocket frame; keep info and above.
    log::set_max_level(log::LevelFilter::Info);
    ikk_ffi::engine::init_logging();
}

pub struct AboutInfo {
    pub core_version: String,
    pub build_number: u32,
    pub git_sha: String,
    pub build_date: String,
    pub target_os: String,
    pub target_arch: String,
    pub core_license: String,
    pub providers: Vec<String>,
}

#[flutter_rust_bridge::frb(sync)]
pub fn about() -> AboutInfo {
    let a = ikk_ffi::about();
    AboutInfo {
        core_version: a.core_version,
        build_number: a.build_number,
        git_sha: a.git_sha,
        build_date: a.build_date,
        target_os: a.target_os,
        target_arch: a.target_arch,
        core_license: a.core_license,
        providers: a.providers,
    }
}

#[flutter_rust_bridge::frb(sync)]
pub fn core_version() -> String {
    ikk_ffi::core_version()
}
#[flutter_rust_bridge::frb(sync)]
pub fn license_text() -> String {
    ikk_ffi::license_text().to_string()
}
#[flutter_rust_bridge::frb(sync)]
pub fn notice_text() -> String {
    ikk_ffi::notice_text().to_string()
}
#[flutter_rust_bridge::frb(sync)]
pub fn third_party_text() -> String {
    ikk_ffi::third_party_text().to_string()
}

pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub fn list_mics() -> Result<Vec<InputDevice>, String> {
    Ok(ikk_ffi::list_mics()?
        .into_iter()
        .map(|d| InputDevice {
            id: d.id,
            name: d.name,
            is_default: d.is_default,
        })
        .collect())
}

/// Desktop config directory; `None` on mobile (pass the app's documents dir).
#[flutter_rust_bridge::frb(sync)]
pub fn config_dir() -> Option<String> {
    ikk_ffi::config_dir()
}

pub fn settings_load(path: String) -> Result<String, String> {
    ikk_ffi::settings_load(&path)
}
pub fn settings_save(path: String, json: String) -> Result<(), String> {
    ikk_ffi::settings_save(&path, &json)
}
pub fn consent_load(path: String) -> Result<String, String> {
    ikk_ffi::consent_load(&path)
}
pub fn consent_save(path: String, json: String) -> Result<(), String> {
    ikk_ffi::consent_save(&path, &json)
}
