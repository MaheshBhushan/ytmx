//! `ytmx update`: replace this binary with a release build from GitHub.
//!
//! Same artifacts and checks as install.sh / install.ps1: the archive for
//! this OS and CPU, verified against its published SHA-256, then swapped in
//! where the running binary lives.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "MaheshBhushan/ytmx";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The release asset name for the platform this binary was built for.
fn asset_name() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("no release builds for {other} CPUs; build from source"),
    };
    Ok(match std::env::consts::OS {
        "linux" => format!("ytmx-{arch}-unknown-linux-gnu.tar.gz"),
        "macos" => format!("ytmx-{arch}-apple-darwin.tar.gz"),
        // ARM64 Windows runs the x64 build under emulation.
        "windows" => "ytmx-x86_64-pc-windows-msvc.zip".to_string(),
        other => bail!("no release builds for {other}; build from source"),
    })
}

/// "v0.3.1" / "0.3.1" → (0, 3, 1).
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u64>());
    Some((it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?))
}

pub fn is_newer(candidate: &str, than: &str) -> bool {
    matches!((parse(candidate), parse(than)), (Some(a), Some(b)) if a > b)
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(format!("ytmx/{CURRENT}")).timeout(Duration::from_secs(120)).build()?)
}

/// Tag of the latest published release, e.g. "v0.3.1".
pub async fn latest() -> Result<String> {
    let r = http()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("asking GitHub for the latest release")?;
    if !r.status().is_success() {
        bail!("GitHub answered HTTP {} for the latest release", r.status());
    }
    let v: serde_json::Value = r.json().await?;
    v["tag_name"].as_str().map(String::from).ok_or_else(|| anyhow!("no release tag in GitHub's answer"))
}

/// The real binary path (through symlinks), and whether it can be replaced in place.
fn install_target() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().ok_or_else(|| anyhow!("can't locate the ytmx binary"))?;
    let s = exe.to_string_lossy();
    if s.contains("/Cellar/") || s.contains("/nix/store/") || s.starts_with("/usr/bin/") || s.starts_with("/usr/sbin/") {
        bail!("ytmx at {} is managed by a package manager; update it there", exe.display());
    }
    if s.contains("/.cargo/bin/") {
        bail!("ytmx was installed with cargo; update with `cargo install --git https://github.com/{REPO} --force`, or switch to the installer in the README");
    }
    // Probe writability with a temp file next to the binary.
    let probe = dir.join(format!(".ytmx-update-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(e) => bail!("can't write to {} ({e}); re-run the install command, or update with the rights to that folder", dir.display()),
    }
    Ok(exe)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let r = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    if !r.status().is_success() {
        bail!("HTTP {} for {url}", r.status());
    }
    Ok(r.bytes().await?.to_vec())
}

/// Unpack `archive` (tar.gz or zip) into `dir` with the system tar
/// (bsdtar on macOS/Windows reads zip too).
fn extract(archive: &Path, dir: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new("tar");
    cmd.arg(if archive.to_string_lossy().ends_with(".zip") { "-xf" } else { "-xzf" }).arg(archive).arg("-C").arg(dir);
    let out = cmd.output().context("running tar to unpack the update")?;
    if !out.status.success() {
        bail!("unpacking the update failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Leftover from a previous Windows update (the old binary can only be renamed while running).
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

/// Swap `new` in for `exe`. Unix renames over the running binary (atomic, the
/// running process keeps its inode). Windows can't overwrite a running .exe but
/// can rename it, so the old one moves aside first.
fn replace(exe: &Path, new: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(new, exe).with_context(|| format!("replacing {}", exe.display()))?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).with_context(|| format!("moving the running {} aside", exe.display()))?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e).with_context(|| format!("installing the new {}", exe.display()));
        }
    }
    Ok(())
}

/// Download and install `tag` (default: latest). Returns the version installed, or None if already current.
pub async fn run(check_only: bool, to: Option<String>) -> Result<()> {
    let (tag, pinned) = match to {
        Some(t) => (if t.starts_with('v') { t } else { format!("v{t}") }, true),
        None => (latest().await?, false),
    };
    if !pinned && !is_newer(&tag, CURRENT) {
        println!("ytmx {CURRENT} is the latest version");
        return Ok(());
    }
    if check_only {
        println!("ytmx {} is available (you have {CURRENT}); run `ytmx update`", tag.trim_start_matches('v'));
        return Ok(());
    }
    if pinned && parse(&tag) == parse(CURRENT) {
        println!("ytmx {CURRENT} is already installed");
        return Ok(());
    }
    let exe = install_target()?;
    let asset = asset_name()?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let client = http()?;
    println!("Downloading ytmx {} ({asset})...", tag.trim_start_matches('v'));
    let data = download(&client, &format!("{base}/{asset}")).await.with_context(|| format!("no {asset} in release {tag}"))?;
    let want = String::from_utf8(download(&client, &format!("{base}/{asset}.sha256")).await?)?.trim().to_lowercase();
    let got = hex::encode(Sha256::digest(&data));
    if got != want {
        bail!("checksum mismatch for {asset}: refusing to install");
    }

    // Stage next to the binary so the final rename stays on one filesystem.
    let dir = exe.parent().expect("binary has a parent dir");
    let stage = dir.join(format!(".ytmx-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let result = async {
        let archive = stage.join(&asset);
        std::fs::write(&archive, &data)?;
        extract(&archive, &stage)?;
        let new = stage.join(format!("ytmx{}", std::env::consts::EXE_SUFFIX));
        if !new.is_file() {
            bail!("the release archive has no ytmx binary");
        }
        replace(&exe, &new)
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result?;
    println!("Updated ytmx {CURRENT} → {} at {}", tag.trim_start_matches('v'), exe.display());

    // The background player is still the old binary; restart it on the new one.
    if let Some(mut c) = crate::ipc::Client::existing().await {
        let _ = c.request(&crate::ipc::Request::Quit).await;
        println!("Stopped the old background player; the next `ytmx` starts the new one.");
    }
    Ok(())
}

/// Once a day at most: the newer release tag, if any. Silent on errors.
pub async fn notice() -> Option<String> {
    if std::env::var_os("YTMX_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let path = crate::paths::cache_dir().join("update.json");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let cached: Option<serde_json::Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let fresh = cached.as_ref().and_then(|c| c["at"].as_u64()).is_some_and(|at| now.saturating_sub(at) < 86_400);
    let tag = if fresh {
        cached?["latest"].as_str()?.to_string()
    } else {
        let tag = latest().await.ok()?;
        let _ = std::fs::write(&path, serde_json::json!({ "at": now, "latest": tag }).to_string());
        tag
    };
    is_newer(&tag, CURRENT).then_some(tag)
}
