//! Cookie session and the SAPISIDHASH authorization the web client sends.
//!
//! Captured from music.youtube.com: every InnerTube call carries
//! `authorization: SAPISIDHASH <ts>_<sha1> SAPISID1PHASH <ts>_<sha1> SAPISID3PHASH <ts>_<sha1>`
//! where sha1 = SHA1("<ts> <cookie value> https://music.youtube.com").

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::path::PathBuf;

pub const ORIGIN: &str = "https://music.youtube.com";

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    /// Raw `Cookie:` header value for .youtube.com.
    pub cookie: String,
    #[serde(default)]
    pub authuser: u32,
    /// Where the cookies came from, for `whoami`.
    #[serde(default)]
    pub source: String,
}

impl Session {
    pub fn from_cookie_header(raw: &str, source: &str) -> Result<Self> {
        let raw = raw.trim().trim_start_matches("Cookie:").trim_start_matches("cookie:").trim();
        let s = Session { cookie: raw.to_string(), authuser: 0, source: source.into() };
        if s.get("SAPISID").is_none() && s.get("__Secure-3PAPISID").is_none() {
            bail!("cookie has no SAPISID / __Secure-3PAPISID: not a signed-in youtube.com session");
        }
        Ok(s)
    }

    /// Import YouTube cookies from a browser profile. Chromium-family profiles
    /// can be named by directory ("Profile 5"), display name or account email;
    /// the default is the browser's last-used profile.
    pub fn import_browser(browser: &str, profile: Option<&str>) -> Result<(Self, String)> {
        let domains = Some(vec!["youtube.com".to_string()]);
        let (cookies, label) = match browser {
            "firefox" => (rookie::firefox(domains), "default profile".to_string()),
            "librewolf" => (rookie::librewolf(domains), "default profile".to_string()),
            _ => {
                let root = chromium_root(browser)?;
                let (dir, label) = chromium_profile(&root, profile)?;
                let db = [dir.join("Network/Cookies"), dir.join("Cookies")]
                    .into_iter()
                    .find(|p| p.exists())
                    .ok_or_else(|| anyhow::anyhow!("no cookie database in {}", dir.display()))?;
                (rookie::any_browser(&db.to_string_lossy(), domains, None), label)
            }
        };
        let cookies = cookies.map_err(|e| anyhow::anyhow!("reading {browser} cookies: {e:#}"))?;
        // Chrome stores several copies per name (paths, partitions); keep the
        // root-path one with the latest expiry, which is what the page sends.
        let mut best: Vec<&rookie::enums::Cookie> = Vec::new();
        for c in cookies.iter().filter(|c| c.domain.trim_start_matches('.') == "youtube.com" || c.domain.ends_with(".youtube.com")) {
            let rank = |c: &rookie::enums::Cookie| (c.path == "/", c.expires.unwrap_or(0));
            match best.iter_mut().find(|b| b.name == c.name) {
                Some(b) if rank(c) > rank(b) => *b = c,
                Some(_) => {}
                None => best.push(c),
            }
        }
        let header = best.iter().map(|c| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; ");
        Ok((Self::from_cookie_header(&header, &format!("{browser}: {label}"))?, label))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookie.split(';').find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then_some(v)
        })
    }

    pub fn authorization(&self) -> String {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let hash = |v: &str| {
            let mut h = Sha1::new();
            h.update(format!("{ts} {v} {ORIGIN}"));
            format!("{ts}_{}", hex::encode(h.finalize()))
        };
        [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
            .iter()
            .filter_map(|(scheme, cookie)| self.get(cookie).map(|v| format!("{scheme} {}", hash(v))))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn path() -> PathBuf {
        crate::paths::config_dir().join("session.json")
    }

    pub fn load() -> Result<Option<Self>> {
        let p = Self::path();
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(&p)?).with_context(|| format!("parsing {}", p.display()))?))
    }

    pub fn save(&self) -> Result<()> {
        crate::paths::write_private(&Self::path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn delete() -> Result<bool> {
        let p = Self::path();
        if p.exists() {
            std::fs::remove_file(p)?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn chromium_root(browser: &str) -> Result<PathBuf> {
    let cfg = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let rel = match browser {
        "chrome" => "google-chrome",
        "chromium" => "chromium",
        "brave" => "BraveSoftware/Brave-Browser",
        "edge" => "microsoft-edge",
        "vivaldi" => "vivaldi",
        "opera" => "opera",
        other => bail!("unsupported browser {other:?} (chrome, chromium, brave, edge, vivaldi, opera, firefox, librewolf)"),
    };
    let root = cfg.join(rel);
    if !root.exists() {
        bail!("{browser} profile directory {} not found", root.display());
    }
    Ok(root)
}

/// Resolve a profile directory from `Local State`'s profile cache.
fn chromium_profile(root: &std::path::Path, want: Option<&str>) -> Result<(PathBuf, String)> {
    let state: serde_json::Value = std::fs::read(root.join("Local State")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let cache = state.pointer("/profile/info_cache").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let describe = |dir: &str| {
        let e = cache.get(dir);
        let name = e.and_then(|e| e["name"].as_str()).unwrap_or(dir);
        match e.and_then(|e| e["user_name"].as_str()).filter(|s| !s.is_empty()) {
            Some(mail) => format!("{dir} ({name}, {mail})"),
            None => format!("{dir} ({name})"),
        }
    };
    let dir = match want {
        Some(w) => {
            let wl = w.to_lowercase();
            cache
                .iter()
                .find(|(dir, e)| {
                    dir.to_lowercase() == wl
                        || ["name", "user_name", "gaia_name"].iter().any(|k| e[*k].as_str().is_some_and(|s| s.to_lowercase() == wl))
                })
                .map(|(d, _)| d.clone())
                .or_else(|| root.join(w).is_dir().then(|| w.to_string()))
                .ok_or_else(|| anyhow::anyhow!("no profile {w:?}; profiles: {}", cache.keys().map(|d| describe(d)).collect::<Vec<_>>().join(", ")))?
        }
        None => state.pointer("/profile/last_used").and_then(|v| v.as_str()).unwrap_or("Default").to_string(),
    };
    Ok((root.join(&dir), describe(&dir)))
}

/// Profiles a Chromium-family browser has, for `ytmx login --list`.
pub fn list_profiles(browser: &str) -> Result<Vec<String>> {
    let root = chromium_root(browser)?;
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("Local State"))?)?;
    let last = state.pointer("/profile/last_used").and_then(|v| v.as_str()).unwrap_or("Default");
    let cache = state.pointer("/profile/info_cache").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    Ok(cache
        .iter()
        .map(|(dir, e)| {
            let mail = e["user_name"].as_str().unwrap_or("");
            format!("{}{dir}: {} {mail}", if dir == last { "* " } else { "  " }, e["name"].as_str().unwrap_or(""))
        })
        .collect())
}
