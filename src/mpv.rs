use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Property-change and other async events pushed by mpv over the IPC socket.
#[derive(Debug, Clone)]
pub enum Event {
    // reserved for the audio-level visualizer (astats/showvolume
    // af-metadata polling): nothing calls `observe_property` yet, so
    // this never actually fires today.
    #[allow(dead_code)]
    PropertyChange {
        name: String,
        value: Value,
    },
    EndFile,
    #[allow(dead_code)]
    Other(Value),
}

struct Pending {
    reply_tx: Sender<Result<Value, String>>,
}

/// A running mpv instance controlled over its JSON IPC socket, rather than
/// linking libmpv directly; keeps the build free of C FFI/pkg-config and
/// matches how mpv is normally driven by external controllers.
pub struct Mpv {
    child: Child,
    sock: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: Receiver<Event>,
}

impl Mpv {
    pub fn spawn() -> Result<Self> {
        let sock_path = std::env::temp_dir().join(format!("wim-mpv-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);

        let child = Command::new("mpv")
            .arg("--idle=yes")
            .arg("--no-terminal")
            // this is the actual fix for mpv opening its own native
            // window: `vid` (which track to decode, toggled per-item
            // in set_vid_for_item) and `vo` (where a decoded frame
            // gets displayed) are independent. Never setting `vo`
            // left mpv on its own default (a real gpu/x11/wayland
            // window) regardless of vid, so video played with a
            // window popping up right alongside this one even though
            // nothing ever asked for that; closing that window quits
            // mpv's whole process, breaking the ipc socket this app
            // depends on. `--vo=null` discards the frame instead of
            // displaying it anywhere; `screenshot-raw` (the embedded
            // video pane's own frame source) still works, since a
            // screenshot is taken from the decoded/filtered frame,
            // not from whatever vo would've shown it in.
            .arg("--vo=null")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning mpv; is it installed and on PATH?")?;

        // mpv creates the socket asynchronously after startup, usually
        // 30-80ms in. a flat 20ms sleep per attempt spent most of that
        // asleep past the socket actually being ready, so back off
        // instead: 1ms doubling to 20ms, same ~1s total budget.
        const CONNECT_BUDGET: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_millis(20);
        let sock = {
            let mut waited = Duration::ZERO;
            let mut backoff = Duration::from_millis(1);
            loop {
                match UnixStream::connect(&sock_path) {
                    Ok(s) => break s,
                    Err(_) if waited < CONNECT_BUDGET => {
                        thread::sleep(backoff);
                        waited += backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    Err(e) => return Err(e).context("connecting to mpv ipc socket"),
                }
            }
        };

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel();

        let reader_sock = sock.try_clone().context("cloning mpv ipc socket")?;
        let reader_pending = pending.clone();
        thread::spawn(move || {
            let mut lines = BufReader::new(reader_sock).lines();
            while let Some(Ok(line)) = lines.next() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = v.get("request_id").and_then(Value::as_u64) {
                    if let Some(p) = reader_pending.lock().unwrap().remove(&id) {
                        let result = match v.get("error").and_then(Value::as_str) {
                            Some("success") | None => {
                                Ok(v.get("data").cloned().unwrap_or(Value::Null))
                            }
                            Some(err) => Err(err.to_string()),
                        };
                        let _ = p.reply_tx.send(result);
                    }
                    continue;
                }
                match v.get("event").and_then(Value::as_str) {
                    Some("property-change") => {
                        if let (Some(name), Some(value)) = (
                            v.get("name").and_then(Value::as_str),
                            v.get("data").cloned(),
                        ) {
                            let _ = event_tx.send(Event::PropertyChange {
                                name: name.to_string(),
                                value,
                            });
                        }
                    }
                    Some("end-file") => {
                        let _ = event_tx.send(Event::EndFile);
                    }
                    Some(_) => {
                        let _ = event_tx.send(Event::Other(v));
                    }
                    None => {}
                }
            }
        });

        Ok(Mpv {
            child,
            sock: Mutex::new(sock),
            next_id: AtomicU64::new(1),
            pending,
            events: event_rx,
        })
    }

    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }

    /// Sends a raw mpv IPC command and blocks for its reply.
    pub fn command(&self, args: &[Value]) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, Pending { reply_tx: tx });

        let payload = json!({ "command": args, "request_id": id });
        let mut line = serde_json::to_vec(&payload)?;
        line.push(b'\n');
        self.sock
            .lock()
            .unwrap()
            .write_all(&line)
            .context("writing to mpv ipc socket")?;

        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("mpv command error: {e}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("timed out waiting for mpv reply")
            }
        }
    }

    pub fn get_property(&self, name: &str) -> Result<Value> {
        self.command(&[json!("get_property"), json!(name)])
    }

    pub fn set_property(&self, name: &str, value: Value) -> Result<()> {
        self.command(&[json!("set_property"), json!(name), value])?;
        Ok(())
    }

    // reserved for the audio-level visualizer; see `Event::PropertyChange`.
    #[allow(dead_code)]
    pub fn observe_property(&self, id: u64, name: &str) -> Result<()> {
        self.command(&[json!("observe_property"), json!(id), json!(name)])?;
        Ok(())
    }

    /// Replaces mpv's entire playlist and plays `path_or_url`
    /// immediately; for "play this now" (enter), not queuing.
    pub fn load_file(&self, path_or_url: &str) -> Result<()> {
        self.command(&[json!("loadfile"), json!(path_or_url), json!("replace")])?;
        Ok(())
    }

    /// Appends to mpv's own playlist (playing immediately if nothing is
    /// currently loaded, otherwise queued). mpv auto-advances through
    /// its playlist natively on end-of-file, with none of the IPC
    /// round-trip gap that reactively reloading on our end introduces;
    /// this is what actually makes queue playback gapless.
    pub fn playlist_append(&self, path_or_url: &str) -> Result<()> {
        self.command(&[json!("loadfile"), json!(path_or_url), json!("append-play")])?;
        Ok(())
    }

    /// Clears mpv's playlist except the currently-playing file (mpv's
    /// own semantics for `playlist-clear`), so the upcoming queue can be
    /// rebuilt (e.g. after a shuffle) without interrupting playback.
    pub fn playlist_clear(&self) -> Result<()> {
        self.command(&[json!("playlist-clear")])?;
        Ok(())
    }

    /// `mode`: "no" | "track" | "album". mpv reads replaygain tags
    /// embedded in the file itself (via its own demuxer/decoder) and
    /// applies the matching gain automatically; we don't need to
    /// extract gain values ourselves for local files.
    pub fn set_replaygain(&self, mode: &str) -> Result<()> {
        self.set_property("replaygain", json!(mode))
    }

    pub fn toggle_pause(&self) -> Result<()> {
        let paused = self.get_property("pause")?.as_bool().unwrap_or(false);
        self.set_property("pause", json!(!paused))
    }

    /// `"weak"`: a no-op (not an error) if there's nothing further
    /// along mpv's own playlist, since it already holds every queued
    /// track (see `playlist_append`) and knows better than we do
    /// whether there's anywhere to skip to.
    pub fn playlist_next(&self) -> Result<()> {
        self.command(&[json!("playlist-next"), json!("weak")])
            .map(|_| ())
    }

    pub fn set_volume(&self, vol: f64) -> Result<()> {
        self.set_property("volume", json!(vol))
    }

    pub fn set_speed(&self, speed: f64) -> Result<()> {
        self.set_property("speed", json!(speed))
    }

    /// `mode`: "no" | "inf" | a repeat count.
    pub fn set_loop_file(&self, mode: &str) -> Result<()> {
        self.set_property("loop-file", json!(mode))
    }

    /// Sets the `af` (audio filter graph) property directly, e.g. for
    /// the eq filter chain built by `crate::eq::build_af_filter`.
    pub fn set_af(&self, filter: &str) -> Result<()> {
        self.set_property("af", json!(filter))
    }

    /// Applies every `sub-*` style property from config/runtime state
    /// at once; best-effort per property (one unsupported/malformed
    /// value on an older mpv shouldn't block the rest from applying).
    pub fn set_subtitle_style(&self, cfg: &crate::config::SubtitleConfig) {
        let _ = self.set_property("sub-delay", json!(cfg.delay_secs));
        let _ = self.set_property("sub-font-size", json!(cfg.font_size));
        let _ = self.set_property("sub-pos", json!(cfg.position));
        let _ = self.set_property("sub-border-size", json!(cfg.outline_size));
        let _ = self.set_property("sub-blur", json!(cfg.blur));
        let _ = self.set_property("sub-color", json!(gray_color(cfg.color_level, 1.0)));
        // the background box itself always renders near-black
        // regardless of the text's own level; a box that matched a
        // bright text color would defeat its own purpose (readability
        // contrast), and there's no separate background-color config
        // field to let it drift from that.
        let _ = self.set_property(
            "sub-back-color",
            json!(gray_color(0.0, cfg.background_opacity as f32)),
        );
    }

    /// Loads an external subtitle file onto the currently playing
    /// video (e.g. one just downloaded from opensubtitles) and
    /// switches to it immediately ("select" flag), rather than just
    /// adding it to the track list unselected.
    pub fn sub_add(&self, path: &str) -> Result<()> {
        self.command(&[json!("sub-add"), json!(path), json!("select")])?;
        Ok(())
    }

    pub fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        let flag = if relative { "relative" } else { "absolute" };
        self.command(&[json!("seek"), json!(seconds), json!(flag)])?;
        Ok(())
    }

    /// One video frame as raw `bgr0` pixels, for embedding video in
    /// the workspace (see `main::VideoPane`/`main::capture_video_frame`)
    /// instead of leaving it to mpv's own native window. Round-trips
    /// over the same IPC socket as every other command, so calling
    /// this on every render tick would stall the UI on each reply;
    /// callers are expected to throttle.
    pub fn screenshot_raw(&self) -> Result<RawFrame> {
        let v = self.command(&[json!("screenshot-raw")])?;
        let width = v.get("w").and_then(Value::as_u64).context("missing w")? as u32;
        let height = v.get("h").and_then(Value::as_u64).context("missing h")? as u32;
        let stride = v
            .get("stride")
            .and_then(Value::as_u64)
            .context("missing stride")? as u32;
        let data_b64 = v
            .get("data")
            .and_then(Value::as_str)
            .context("missing data")?;
        let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
            .context("decoding screenshot-raw's base64 data")?;
        Ok(RawFrame {
            width,
            height,
            stride,
            data,
        })
    }
}

/// `bgr0` (4 bytes/pixel, blue/green/red/unused) raw frame as returned
/// by mpv's `screenshot-raw` command; `stride` is the byte width of
/// one row and may exceed `width * 4` (row padding), so it has to be
/// used to find each row's start rather than assuming a tight pack.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

impl RawFrame {
    /// `bgr0` -> `image::RgbImage`, dropping the padding byte and
    /// swapping channel order; `None` if the dimensions mpv reported
    /// don't actually fit the data it sent (never expected, but a
    /// malformed frame should just get skipped, not panic the UI).
    pub fn into_image(self) -> Option<image::RgbImage> {
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for row in 0..self.height {
            let row_start = (row * self.stride) as usize;
            for col in 0..self.width {
                let px = row_start + (col * 4) as usize;
                let pixel = self.data.get(px..px + 3)?;
                rgb.push(pixel[2]); // r
                rgb.push(pixel[1]); // g
                rgb.push(pixel[0]); // b
            }
        }
        image::RgbImage::from_raw(self.width, self.height, rgb)
    }
}

/// `#AARRGGBB`, mpv's own color option format; `level` (0..1) sets R,
/// G, and B identically so this can never produce a hue, only a shade
/// of gray, per the monochrome rule, no matter what caller passes in.
fn gray_color(level: f32, alpha: f32) -> String {
    let v = (level.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{a:02X}{v:02X}{v:02X}{v:02X}")
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.command(&[json!("quit")]);
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_color_keeps_r_g_b_identical_regardless_of_input() {
        assert_eq!(gray_color(1.0, 1.0), "#FFFFFFFF");
        assert_eq!(gray_color(0.0, 0.0), "#00000000");
        assert_eq!(gray_color(0.5, 1.0), "#FF808080");
    }

    #[test]
    fn gray_color_clamps_out_of_range_input() {
        assert_eq!(gray_color(-1.0, 2.0), "#FF000000");
    }

    #[test]
    fn raw_frame_swaps_bgr_to_rgb_and_drops_padding() {
        // 2x1 bgr0, stride padded by 4 extra bytes past width*4.
        let frame = RawFrame {
            width: 2,
            height: 1,
            stride: 12,
            data: vec![
                10, 20, 30, 0, // pixel 0: b=10 g=20 r=30
                40, 50, 60, 0, // pixel 1: b=40 g=50 r=60
                0, 0, 0, 0, // row padding, must be ignored
            ],
        };
        let img = frame.into_image().expect("valid frame");
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10]);
        assert_eq!(img.get_pixel(1, 0).0, [60, 50, 40]);
    }

    #[test]
    fn raw_frame_rejects_data_too_short_for_its_own_dimensions() {
        let frame = RawFrame {
            width: 4,
            height: 4,
            stride: 16,
            data: vec![0; 4],
        };
        assert!(frame.into_image().is_none());
    }
}
