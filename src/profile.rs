// Profile system. A profile bundles the values that must move together
// for stealth to stay coherent: TLS/H2 emulation pick, HTTP headers, and
// the JS-side navigator.* properties a real browser would expose. Bumping
// Chrome is one TOML edit, not a multi-file scavenger hunt.
//
// Profiles are baked into the binary via include_str! so deployment stays
// "one static file." Users can override by reading the same TOML at runtime
// from disk (loader supports both paths) — useful for niche/experimental
// profiles without rebuilding.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    pub emulation: rquest_util::Emulation,
    pub user_agent: String,
    // The next four fields are bundled by rquest_util::Emulation already —
    // we keep them in the TOML as the source-of-truth documentation of
    // what the emulation profile sends, and the planned fingerprint test
    // harness (Phase 9 follow-up) verifies the wire reality matches.
    #[allow(dead_code)]
    pub accept_language: String,
    #[allow(dead_code)]
    pub sec_ch_ua: String,
    #[allow(dead_code)]
    pub sec_ch_ua_mobile: String,
    #[allow(dead_code)]
    pub sec_ch_ua_platform: String,
    pub platform: String,
    pub languages: Vec<String>,
    pub hardware_concurrency: u32,
    pub device_memory: u32,
}

const PROFILE_CHROME_134: &str = include_str!("../profiles/chrome_134.toml");
const PROFILE_CHROME_131: &str = include_str!("../profiles/chrome_131.toml");

const BUILTIN: &[(&str, &str)] = &[
    ("chrome_134", PROFILE_CHROME_134),
    ("chrome_131", PROFILE_CHROME_131),
];

pub const DEFAULT_PROFILE: &str = "chrome_134";

impl Profile {
    pub fn load(name: &str) -> Result<Profile> {
        // 1. Built-in (baked at compile time).
        if let Some((_, src)) = BUILTIN.iter().find(|(n, _)| *n == name) {
            return toml::from_str(src).with_context(|| format!("parse builtin profile {name}"));
        }
        // 2. Filesystem fallback — useful for ad-hoc experiments without
        //    rebuilding. Looks up `profiles/<name>.toml` and `<name>` (raw
        //    path).
        let candidates = [format!("profiles/{name}.toml"), name.to_string()];
        for path in &candidates {
            if let Ok(src) = std::fs::read_to_string(path) {
                return toml::from_str(&src).with_context(|| format!("parse profile from {path}"));
            }
        }
        Err(anyhow!(
            "unknown profile '{name}'. Built-in: [{}]",
            BUILTIN
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    pub fn list_builtin() -> Vec<&'static str> {
        BUILTIN.iter().map(|(n, _)| *n).collect()
    }

    // The JS-side navigator/window patches that have to fire BEFORE any
    // page script runs. Returns a JS source string suitable for ctx.eval()
    // immediately after shims.js. Keeps the values in one place rather
    // than scattering them through shims.js + main.rs.
    pub fn js_init(&self) -> String {
        let langs = serde_json::to_string(&self.languages).unwrap_or_else(|_| "[]".into());
        let ua = serde_json::to_string(&self.user_agent).unwrap_or_else(|_| "\"\"".into());
        let plat = serde_json::to_string(&self.platform).unwrap_or_else(|_| "\"\"".into());
        format!(
            r#"(function(){{
                if (typeof navigator === 'undefined') return;
                try {{ Object.defineProperty(navigator, 'userAgent', {{ get: function(){{ return {ua}; }} }}); }} catch(e){{}}
                try {{ Object.defineProperty(navigator, 'platform', {{ get: function(){{ return {plat}; }} }}); }} catch(e){{}}
                try {{ Object.defineProperty(navigator, 'languages', {{ get: function(){{ return {langs}; }} }}); }} catch(e){{}}
                try {{ Object.defineProperty(navigator, 'language', {{ get: function(){{ return {langs}[0] || 'en-US'; }} }}); }} catch(e){{}}
                try {{ Object.defineProperty(navigator, 'hardwareConcurrency', {{ get: function(){{ return {hc}; }} }}); }} catch(e){{}}
                try {{ Object.defineProperty(navigator, 'deviceMemory', {{ get: function(){{ return {dm}; }} }}); }} catch(e){{}}
            }})();"#,
            ua = ua,
            plat = plat,
            langs = langs,
            hc = self.hardware_concurrency,
            dm = self.device_memory,
        )
    }
}
