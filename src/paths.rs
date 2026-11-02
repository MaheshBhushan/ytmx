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

fn user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into())
}

/// Local socket name for `kind` ("player" or "mpv").
/// Unix: a socket file in the runtime dir (private to the user, mode 0600).
/// Windows: a named pipe, `\\.\pipe\ytmx-<user>-<kind>`.
pub fn ipc_name(kind: &str) -> std::io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::prelude::*;
    #[cfg(unix)]
    {
        socket_file(kind).to_fs_name::<interprocess::local_socket::GenericFilePath>()
    }
    #[cfg(windows)]
    {
        format!("ytmx-{}-{kind}", user()).to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
}

/// What mpv's `--input-ipc-server` takes for the same name.
pub fn mpv_ipc_arg() -> String {
    #[cfg(unix)]
    {
        socket_file("mpv").display().to_string()
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\ytmx-{}-mpv", user())
    }
}

#[cfg(unix)]
pub fn socket_file(kind: &str) -> PathBuf {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    let name = if kind == "player" { format!("ytmx-{}.sock", user()) } else { format!("ytmx-{kind}-{}.sock", user()) };
    base.join(name)
}

/// Remove a stale socket file (no-op for Windows pipes).
pub fn remove_socket(kind: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_file(kind));
    }
    #[cfg(windows)]
    let _ = kind;
}

/// Background helpers (mpv, yt-dlp, mixers) must not pop up console windows on Windows.
pub fn no_window(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Write a file readable only by the owner (cookies live here).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        // Windows: files under the user profile are private to the user by default ACLs.
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Is `bin` (plus `.exe` on Windows) somewhere on PATH?
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    let name = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&name)).find(|p| p.is_file())
}
