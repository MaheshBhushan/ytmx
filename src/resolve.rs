//! Audio stream resolution. YouTube's stream URLs are signature-ciphered and
//! need a PO token (seen in the captured /player request), so this delegates
//! to yt-dlp and caches the direct googlevideo URLs until they expire. The
//! player resolves upcoming tracks ahead of time so skips are instant.

use anyhow::{Context, Result, anyhow, bail};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Stream {
    pub url: String,
    pub user_agent: Option<String>,
    pub expires: u64,
}

type Pending = Shared<BoxFuture<'static, Result<Stream, String>>>;

pub struct Resolver {
    ytdlp: PathBuf,
    cookies: Option<PathBuf>,
    cache: Mutex<HashMap<String, Stream>>,
    inflight: Mutex<HashMap<String, Pending>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn find_ytdlp() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("YTMX_YTDLP") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    let mut cands: Vec<PathBuf> = std::env::split_paths(&path).map(|d| d.join("yt-dlp")).collect();
    if let Some(h) = dirs::home_dir() {
        cands.push(h.join(".local/bin/yt-dlp"));
    }
    cands.into_iter().find(|p| p.is_file())
}

/// Netscape cookie jar for yt-dlp, used only when a track needs sign-in.
fn write_cookie_jar(session: &crate::auth::Session) -> Option<PathBuf> {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for kv in session.cookie.split(';') {
        let (k, v) = kv.trim().split_once('=')?;
        let secure = if k.starts_with("__Secure") || k.starts_with("__Host") { "TRUE" } else { "FALSE" };
        out.push_str(&format!(".youtube.com\tTRUE\t/\t{secure}\t{}\t{k}\t{v}\n", now() + 86_400 * 30));
    }
    let p = crate::paths::cache_dir().join("cookies.txt");
    crate::paths::write_private(&p, out.as_bytes()).ok()?;
    Some(p)
}

impl Resolver {
    pub fn new(session: Option<&crate::auth::Session>) -> Result<Arc<Self>> {
        let ytdlp = find_ytdlp().ok_or_else(|| anyhow!("yt-dlp not found: install it (`uv tool install yt-dlp` or `pipx install yt-dlp`) or set YTMX_YTDLP"))?;
        Ok(Arc::new(Resolver { ytdlp, cookies: session.and_then(write_cookie_jar), cache: Mutex::default(), inflight: Mutex::default() }))
    }

    pub fn cached(&self, id: &str) -> Option<Stream> {
        let c = self.cache.lock().unwrap();
        c.get(id).filter(|s| s.expires > now() + 120).cloned()
    }

    pub fn forget(&self, id: &str) {
        self.cache.lock().unwrap().remove(id);
    }

    pub async fn resolve(self: &Arc<Self>, id: &str) -> Result<Stream> {
        if let Some(s) = self.cached(id) {
            return Ok(s);
        }
        let fut = {
            let mut inflight = self.inflight.lock().unwrap();
            inflight
                .entry(id.to_string())
                .or_insert_with(|| {
                    let me = self.clone();
                    let id = id.to_string();
                    async move {
                        let r = match me.run(&id, false).await {
                            Err(e) if me.cookies.is_some() => me.run(&id, true).await.map_err(|e2| anyhow!("{e:#}; with cookies: {e2:#}")),
                            r => r,
                        };
                        me.inflight.lock().unwrap().remove(&id);
                        match r {
                            Ok(s) => {
                                me.cache.lock().unwrap().insert(id, s.clone());
                                Ok(s)
                            }
                            Err(e) => Err(format!("{e:#}")),
                        }
                    }
                    .boxed()
                    .shared()
                })
                .clone()
        };
        fut.await.map_err(|e| anyhow!(e))
    }

    async fn run(&self, id: &str, with_cookies: bool) -> Result<Stream> {
        let mut cmd = tokio::process::Command::new(&self.ytdlp);
        cmd.args(["-f", "bestaudio[acodec=opus]/bestaudio/best", "--no-playlist", "--no-warnings", "-q", "--print", "%(url)s", "--print", "%(http_headers.User-Agent|)s"]);
        if with_cookies
            && let Some(c) = &self.cookies {
                cmd.arg("--cookies").arg(c);
            }
        cmd.arg(format!("https://music.youtube.com/watch?v={id}"));
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(std::time::Duration::from_secs(45), cmd.output()).await.context("yt-dlp timed out")?.context("running yt-dlp")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("{}", err.lines().rev().find(|l| l.contains("ERROR")).unwrap_or(err.trim()).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let url = lines.next().filter(|u| u.starts_with("http")).ok_or_else(|| anyhow!("yt-dlp printed no URL"))?.to_string();
        let ua = lines.next().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let expires = url.split(['?', '&']).find_map(|kv| kv.strip_prefix("expire=")).and_then(|s| s.parse().ok()).unwrap_or(now() + 3600);
        Ok(Stream { url, user_agent: ua, expires })
    }
}
