//! The desktop's output volume, so the number ytmx shows is the one the
//! system tray and media keys move. PipeWire is driven with `wpctl`,
//! PulseAudio with `pactl`; `pactl subscribe` (shipped by both) streams
//! changes made elsewhere. Without either, the player keeps mpv's own volume.

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    /// 0-100 (can exceed 100 if something else boosted the sink).
    pub volume: f64,
    pub muted: bool,
}

#[derive(Clone)]
pub struct Mixer {
    wpctl: bool,
}

async fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let fut = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let out = tokio::time::timeout(Duration::from_secs(2), fut).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

impl Mixer {
    /// A mixer for this desktop, if it has one that answers.
    pub async fn detect() -> Option<Mixer> {
        if std::env::var("YTMX_MIXER").is_ok_and(|v| v == "mpv") {
            return None;
        }
        for m in [Mixer { wpctl: true }, Mixer { wpctl: false }] {
            if on_path(if m.wpctl { "wpctl" } else { "pactl" }) && m.get().await.is_some() {
                return Some(m);
            }
        }
        None
    }

    pub async fn get(&self) -> Option<Level> {
        if self.wpctl {
            // "Volume: 0.50" or "Volume: 0.50 [MUTED]"
            let s = output("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).await?;
            let v: f64 = s.split_whitespace().nth(1)?.parse().ok()?;
            Some(Level { volume: (v * 100.0).round(), muted: s.contains("MUTED") })
        } else {
            let s = output("pactl", &["get-sink-volume", "@DEFAULT_SINK@"]).await?;
            let pct = s.split_whitespace().find_map(|w| w.strip_suffix('%')?.parse::<f64>().ok())?;
            let mute = output("pactl", &["get-sink-mute", "@DEFAULT_SINK@"]).await.is_some_and(|m| m.contains("yes"));
            Some(Level { volume: pct, muted: mute })
        }
    }

    /// Set the volume (0-100) and unmute; returns what the mixer reports afterwards.
    pub async fn set(&self, volume: f64) -> Option<Level> {
        let v = volume.clamp(0.0, 100.0).round();
        if self.wpctl {
            output("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{:.2}", v / 100.0)]).await?;
            if v > 0.0 {
                output("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]).await;
            }
        } else {
            output("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &format!("{v}%")]).await?;
            if v > 0.0 {
                output("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "0"]).await;
            }
        }
        self.get().await
    }

    pub async fn toggle_mute(&self) -> Option<Level> {
        if self.wpctl {
            output("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).await?;
        } else {
            output("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]).await?;
        }
        self.get().await
    }

    /// Send the level whenever it changes outside ytmx (tray, media keys,
    /// default device switch). Uses `pactl subscribe`, else polls every second.
    pub fn watch(&self, tx: mpsc::UnboundedSender<Level>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut last = me.get().await;
            loop {
                let child = on_path("pactl")
                    .then(|| Command::new("pactl").arg("subscribe").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().ok())
                    .flatten();
                match child {
                    Some(mut c) => {
                        let mut lines = BufReader::new(c.stdout.take().expect("piped stdout")).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            // Sink volume/mute changes, and "server" changes when the default sink switches.
                            if !(l.contains(" on sink ") || l.contains(" on server")) {
                                continue;
                            }
                            let now = me.get().await;
                            if now.is_some() && now != last {
                                last = now;
                                if tx.send(now.expect("checked")).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    None => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let now = me.get().await;
                        if now.is_some() && now != last {
                            last = now;
                            if tx.send(now.expect("checked")).is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                }
                // pactl exited (sound server restart): retry shortly.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}
