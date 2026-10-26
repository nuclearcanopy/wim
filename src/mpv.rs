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
    PropertyChange { name: String, value: Value },
    EndFile,
    Other(Value),
}

struct Pending {
    reply_tx: Sender<Result<Value, String>>,
}

/// A running mpv instance controlled over its JSON IPC socket, rather than
/// linking libmpv directly — keeps the build free of C FFI/pkg-config and
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
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning mpv; is it installed and on PATH?")?;

        // mpv creates the socket asynchronously after startup.
        let sock = {
            let mut attempt = 0;
            loop {
                match UnixStream::connect(&sock_path) {
                    Ok(s) => break s,
                    Err(e) if attempt < 50 => {
                        attempt += 1;
                        thread::sleep(Duration::from_millis(20));
                        let _ = e;
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
                            Some("success") | None => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
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
        self.pending.lock().unwrap().insert(id, Pending { reply_tx: tx });

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

    pub fn observe_property(&self, id: u64, name: &str) -> Result<()> {
        self.command(&[json!("observe_property"), json!(id), json!(name)])?;
        Ok(())
    }

    pub fn load_file(&self, path_or_url: &str) -> Result<()> {
        self.command(&[json!("loadfile"), json!(path_or_url)])?;
        Ok(())
    }

    pub fn play(&self) -> Result<()> {
        self.set_property("pause", json!(false))
    }

    pub fn pause(&self) -> Result<()> {
        self.set_property("pause", json!(true))
    }

    pub fn toggle_pause(&self) -> Result<()> {
        let paused = self.get_property("pause")?.as_bool().unwrap_or(false);
        self.set_property("pause", json!(!paused))
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

    pub fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        let flag = if relative { "relative" } else { "absolute" };
        self.command(&[json!("seek"), json!(seconds), json!(flag)])?;
        Ok(())
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.command(&[json!("quit")]);
        let _ = self.child.wait();
    }
}
