use anyhow::Result;
use std::path::{Path, PathBuf};

fn ensure(p: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn config_dir() -> PathBuf {
    ensure(dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("ytmx"))
}

pub fn cache_dir() -> PathBuf {
    ensure(dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("ytmx"))
}

pub fn socket() -> PathBuf {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    base.join(format!("ytmx-{}.sock", unsafe_uid()))
}

pub fn mpv_socket() -> PathBuf {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    base.join(format!("ytmx-mpv-{}.sock", unsafe_uid()))
}

fn unsafe_uid() -> String {
    std::env::var("UID").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "user".into())
}

/// Write a file readable only by the owner (cookies live here).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}
