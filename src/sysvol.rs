//! The desktop's output volume, so the number ytmx shows is the one the
//! system tray and media keys move.
//!
//! - Linux: PipeWire via `wpctl`, PulseAudio via `pactl`; `pactl subscribe`
//!   (shipped by both) streams changes made elsewhere.
//! - macOS: `osascript` (`get volume settings` / `set volume`), polled.
//! - Windows: the default render endpoint's IAudioEndpointVolume, polled.
//!
//! Without a working backend the player keeps mpv's own volume.

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    /// 0-100 (can exceed 100 if something else boosted the output).
    pub volume: f64,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Backend {
    Wpctl,
    Pactl,
    #[cfg(target_os = "macos")]
    Osascript,
    #[cfg(windows)]
    CoreAudio,
}

#[derive(Clone)]
pub struct Mixer {
    backend: Backend,
}

async fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let mut c = Command::new(cmd);
    crate::paths::no_window(&mut c).args(args).stdin(Stdio::null()).stderr(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(2), c.output()).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Mixer {
    /// A mixer for this desktop, if it has one that answers.
    pub async fn detect() -> Option<Mixer> {
        if std::env::var("YTMX_MIXER").is_ok_and(|v| v == "mpv") {
            return None;
        }
        let mut candidates = Vec::new();
        #[cfg(windows)]
        candidates.push(Backend::CoreAudio);
        #[cfg(target_os = "macos")]
        candidates.push(Backend::Osascript);
        if crate::paths::find_on_path("wpctl").is_some() {
            candidates.push(Backend::Wpctl);
        }
        if crate::paths::find_on_path("pactl").is_some() {
            candidates.push(Backend::Pactl);
        }
        for backend in candidates {
            let m = Mixer { backend };
            if m.get().await.is_some() {
                return Some(m);
            }
        }
        None
    }

    pub async fn get(&self) -> Option<Level> {
        match self.backend {
            Backend::Wpctl => {
                // "Volume: 0.50" or "Volume: 0.50 [MUTED]"
                let s = output("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).await?;
                let v: f64 = s.split_whitespace().nth(1)?.parse().ok()?;
                Some(Level { volume: (v * 100.0).round(), muted: s.contains("MUTED") })
            }
            Backend::Pactl => {
                let s = output("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]).await?;
                let pct = s.split_whitespace().find_map(|w| w.strip_suffix('%')?.parse::<f64>().ok())?;
                let mute = output("pactl", &["get-sink-mute", "@DEFAULT_SINK@"]).await.is_some_and(|m| m.contains("yes"));
                Some(Level { volume: pct, muted: mute })
            }
            #[cfg(target_os = "macos")]
            Backend::Osascript => {
                // "50,false"
                let s = output("osascript", &["-e", "set s to get volume settings", "-e", "return (output volume of s as text) & \",\" & (output muted of s as text)"]).await?;
                let (v, m) = s.trim().split_once(',')?;
                Some(Level { volume: v.parse().ok()?, muted: m == "true" })
            }
            #[cfg(windows)]
            Backend::CoreAudio => tokio::task::spawn_blocking(win::get).await.ok()?,
        }
    }

    /// Set the volume (0-100) and unmute; returns what the mixer reports afterwards.
    pub async fn set(&self, volume: f64) -> Option<Level> {
        let v = volume.clamp(0.0, 100.0).round();
        match self.backend {
            Backend::Wpctl => {
                output("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{:.2}", v / 100.0)]).await?;
                if v > 0.0 {
                    output("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]).await;
                }
            }
            Backend::Pactl => {
                output("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &format!("{v}%")]).await?;
                if v > 0.0 {
                    output("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "0"]).await;
                }
            }
            #[cfg(target_os = "macos")]
            Backend::Osascript => {
                let unmute = if v > 0.0 { " without output muted" } else { "" };
                output("osascript", &["-e", &format!("set volume output volume {v}{unmute}")]).await?;
            }
            #[cfg(windows)]
            Backend::CoreAudio => {
                tokio::task::spawn_blocking(move || win::set(v / 100.0)).await.ok()??;
            }
        }
        self.get().await
    }

    pub async fn toggle_mute(&self) -> Option<Level> {
        match self.backend {
            Backend::Wpctl => {
                output("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).await?;
            }
            Backend::Pactl => {
                output("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]).await?;
            }
            #[cfg(target_os = "macos")]
            Backend::Osascript => {
                output("osascript", &["-e", "set volume output muted (not (output muted of (get volume settings)))"]).await?;
            }
            #[cfg(windows)]
            Backend::CoreAudio => {
                tokio::task::spawn_blocking(win::toggle_mute).await.ok()??;
            }
        }
        self.get().await
    }

    /// Send the level whenever it changes outside ytmx (tray, media keys,
    /// default device switch).
    pub fn watch(&self, tx: mpsc::UnboundedSender<Level>) {
        let me = self.clone();
        let subscribe = matches!(me.backend, Backend::Wpctl | Backend::Pactl) && crate::paths::find_on_path("pactl").is_some();
        tokio::spawn(async move {
            let mut last = me.get().await;
            let mut report = |now: Option<Level>| -> bool {
                if now.is_some() && now != last {
                    last = now;
                    return tx.send(now.expect("checked")).is_ok();
                }
                true
            };
            loop {
                if subscribe {
                    let child = Command::new("pactl").arg("subscribe").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn();
                    if let Ok(mut c) = child {
                        let mut lines = BufReader::new(c.stdout.take().expect("piped stdout")).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            // Sink volume/mute changes, and "server" changes when the default sink switches.
                            if (l.contains(" on sink ") || l.contains(" on server")) && !report(me.get().await) {
                                return;
                            }
                        }
                    }
                    // pactl exited (sound server restart): retry shortly.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if !report(me.get().await) {
                        return;
                    }
                }
            }
        });
    }
}

#[cfg(windows)]
mod win {
    use super::Level;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};

    fn endpoint() -> Option<IAudioEndpointVolume> {
        unsafe {
            // Fine to call repeatedly on a pooled thread; already-initialised is not an error here.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let d = e.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
        }
    }

    pub fn get() -> Option<Level> {
        let v = endpoint()?;
        unsafe {
            let level = v.GetMasterVolumeLevelScalar().ok()?;
            let muted = v.GetMute().ok()?.as_bool();
            Some(Level { volume: (level as f64 * 100.0).round(), muted })
        }
    }

    pub fn set(level: f64) -> Option<()> {
        let v = endpoint()?;
        unsafe {
            v.SetMasterVolumeLevelScalar(level as f32, std::ptr::null()).ok()?;
            if level > 0.0 {
                v.SetMute(false, std::ptr::null()).ok()?;
            }
        }
        Some(())
    }

    pub fn toggle_mute() -> Option<()> {
        let v = endpoint()?;
        unsafe {
            let m = v.GetMute().ok()?.as_bool();
            v.SetMute(!m, std::ptr::null()).ok()
        }
    }
}
