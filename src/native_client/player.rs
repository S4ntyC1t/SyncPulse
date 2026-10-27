//! Audio playback through a child `mpv` process driven over JSON IPC.
//!
//! Using mpv keeps the native client free of codec/audio-backend code and
//! reuses whatever the user's system already plays well. It also means a
//! crashed UI never takes the audio pipeline down with it.
//!
//! Everything here is blocking IPC, so callers must run off the GTK main
//! thread — the UI polls through [`watch`], which owns its own thread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

/// Snapshot of mpv's playback state, polled on a worker thread.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlaybackState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub idle: bool,
}

pub(crate) struct Mpv {
    socket: PathBuf,
    child: Child,
    quit: AtomicBool,
}

impl Mpv {
    /// Start an idle mpv and wait until its IPC socket is ready.
    pub(crate) fn spawn() -> Result<Self, String> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let socket = runtime_dir.join(format!("syncpulse-client-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);

        let mut command = Command::new("mpv");
        command
            .arg("--idle=yes")
            .arg("--no-video")
            .arg("--no-terminal")
            .arg("--really-quiet")
            // Navidrome runs on loopback, so the demuxer cache is pure overhead.
            .arg("--cache=no")
            .arg("--audio-buffer=0.2")
            // Gapless playback matters for albums split into many tracks.
            .arg("--gapless-audio=yes")
            .arg("--audio-client-name=SyncPulse")
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // If the client dies, mpv must not survive it: an orphaned player would
        // keep the audio device busy and nothing would ever stop it.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = command
            .spawn()
            .map_err(|error| format!("Unable to start mpv: {error}"))?;

        for _ in 0..60 {
            if socket.exists() {
                return Ok(Self {
                    socket,
                    child,
                    quit: AtomicBool::new(false),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("mpv did not create its IPC socket".to_owned())
    }

    /// Send one command and return mpv's reply, skipping asynchronous event
    /// lines until the matching `request_id` shows up.
    fn command(&self, args: serde_json::Value) -> Option<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .ok();
        const REQUEST_ID: i64 = 1;
        let payload = json!({ "command": args, "request_id": REQUEST_ID });
        {
            let mut writer = stream.try_clone().ok()?;
            writer
                .write_all(payload.to_string().as_bytes())
                .and_then(|_| writer.write_all(b"\n"))
                .and_then(|_| writer.flush())
                .ok()?;
        }
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if value.get("request_id").and_then(|id| id.as_i64()) == Some(REQUEST_ID) {
                return Some(value);
            }
        }
    }

    fn property(&self, name: &str) -> Option<serde_json::Value> {
        let reply = self.command(json!(["get_property", name]))?;
        if reply.get("error").and_then(|value| value.as_str()) != Some("success") {
            return None;
        }
        reply.get("data").cloned()
    }

    /// Load a URL, replacing whatever was playing.
    pub(crate) fn play(&self, url: &str) -> Result<(), String> {
        let reply = self
            .command(json!(["loadfile", url, "replace"]))
            .ok_or_else(|| "mpv is not responding".to_owned())?;
        match reply.get("error").and_then(|value| value.as_str()) {
            Some("success") => Ok(()),
            Some(other) => Err(format!("mpv rejected the track: {other}")),
            None => Ok(()),
        }
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.command(json!(["set_property", "pause", paused]));
    }

    pub(crate) fn paused(&self) -> bool {
        self.property("pause")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Seek to an absolute position in seconds.
    pub(crate) fn seek(&self, seconds: f64) {
        self.command(json!(["seek", seconds, "absolute"]));
    }

    pub(crate) fn set_volume(&self, volume: f64) {
        self.command(json!(["set_property", "volume", volume]));
    }

    pub(crate) fn position(&self) -> f64 {
        self.property("time-pos")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    }

    pub(crate) fn duration(&self) -> f64 {
        self.property("duration")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    }

    /// True once the current file finished and mpv fell back to idle.
    pub(crate) fn finished(&self) -> bool {
        self.property("idle-active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Ask mpv to exit, then make sure the process is really gone.
    pub(crate) fn quit(&self) {
        if self.quit.swap(true, Ordering::SeqCst) {
            return;
        }
        self.command(json!(["quit"]));
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        self.quit();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Poll `player` on a worker thread, reporting playback state through `report`.
///
/// The thread holds only a [`Weak`] reference, so it stops (and lets mpv be
/// shut down) as soon as the UI drops its handle. It also stops if the receiver
/// is gone.
pub(crate) fn watch<F>(player: &Arc<Mpv>, interval: Duration, report: F)
where
    F: Fn(PlaybackState) -> bool + Send + 'static,
{
    let weak = Arc::downgrade(player);
    std::thread::spawn(move || {
        while let Some(player) = weak.upgrade() {
            let state = PlaybackState {
                position: player.position(),
                duration: player.duration(),
                paused: player.paused(),
                idle: player.finished(),
            };
            if !report(state) {
                break;
            }
            std::thread::sleep(interval);
        }
    });
}
