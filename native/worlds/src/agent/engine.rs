// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The engine child (`worlds serve --profile public`, the same executable) and the controller side
//! of its control channel (DEDICATED_SERVER.md §3: the engine listens on the Unix socket, the
//! controller connects and speaks first; one JSON object per line). Replies are matched to their
//! request by `id`; events go to the agent's loop.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

/// A control line's limit (§3.2).
const LINE_MAX: usize = 1024 * 1024;
/// How long a request waits for its reply.
const REPLY_WAIT: Duration = Duration::from_secs(10);

/// What the reader thread hands the agent's loop.
/// Events and closes carry their session's generation, so a late line of an ended engine never
/// reaches the next one's state.
pub enum Input {
    Event(u64, Value),
    /// EOF or a read error on the control channel.
    Closed(u64),
    Signal,
}

/// A refused command (`ok: false`), or no reply at all (`code` `"gone"`).
#[derive(Debug, Clone)]
pub struct CmdError {
    pub code: String,
    pub reason: Option<String>,
    pub retry_after: Option<Duration>,
}

impl CmdError {
    fn gone() -> CmdError {
        CmdError {
            code: "gone".into(),
            reason: None,
            retry_after: None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<u32, Sender<Value>>>>;

/// One connected control session.
pub struct Control {
    stream: Mutex<UnixStream>,
    pending: Pending,
    next: Mutex<u32>,
}

impl Control {
    /// Connects (retrying until `deadline`, while `alive` says the engine may still come up).
    pub fn connect(
        path: &Path,
        deadline: Instant,
        alive: &mut dyn FnMut() -> bool,
        generation: u64,
        events: Sender<Input>,
    ) -> Option<Control> {
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline && alive() => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(_) => return None,
            }
        };
        let reader = stream.try_clone().ok()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let routes = pending.clone();
        std::thread::Builder::new()
            .name("agent-control".into())
            .spawn(move || read_lines(reader, routes, generation, events))
            .ok()?;
        Some(Control {
            stream: Mutex::new(stream),
            pending,
            next: Mutex::new(0),
        })
    }

    /// One request; waits for its reply. `Ok` carries the reply's fields.
    pub fn request(&self, op: &str, fields: Value) -> Result<Map<String, Value>, CmdError> {
        let id = {
            let mut n = self.next.lock().map_err(|_| CmdError::gone())?;
            *n = n.wrapping_add(1).max(1);
            *n
        };
        let mut line = match fields {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        line.insert("id".into(), json!(id));
        line.insert("op".into(), json!(op));
        let (tx, rx) = mpsc::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        let mut text = Value::Object(line).to_string();
        text.push('\n');
        let written = self
            .stream
            .lock()
            .map_err(|_| ())
            .and_then(|mut s| s.write_all(text.as_bytes()).map_err(|_| ()));
        let reply = written.ok().and_then(|()| rx.recv_timeout(REPLY_WAIT).ok());
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
        let Some(Value::Object(mut reply)) = reply else {
            return Err(CmdError::gone());
        };
        if reply.get("ok").and_then(Value::as_bool) == Some(true) {
            reply.remove("id");
            reply.remove("ok");
            return Ok(reply);
        }
        let error = reply.get("error").cloned().unwrap_or(Value::Null);
        Err(CmdError {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("internal")
                .to_string(),
            reason: error
                .get("reason")
                .and_then(Value::as_str)
                .map(String::from),
            retry_after: error
                .get("retryAfterMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
        })
    }

    /// A request retried on `busy` (after its `retryAfterMs`, at most 2 s, three times): every
    /// command the agent sends is a latest-wins snapshot or idempotent, so the retry resends it.
    pub fn request_retry(&self, op: &str, fields: Value) -> Result<Map<String, Value>, CmdError> {
        let mut tries = 0;
        loop {
            match self.request(op, fields.clone()) {
                Err(e) if e.code == "busy" && tries < 3 => {
                    tries += 1;
                    std::thread::sleep(
                        e.retry_after
                            .unwrap_or(Duration::from_secs(1))
                            .min(Duration::from_secs(2)),
                    );
                }
                other => return other,
            }
        }
    }

    pub fn shutdown_write(&self) {
        if let Ok(s) = self.stream.lock() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn read_lines(stream: UnixStream, pending: Pending, generation: u64, events: Sender<Input>) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(LINE_MAX as u64 + 1)
            .read_until(b'\n', &mut buf)
            .unwrap_or_default();
        if n == 0 || (buf.len() > LINE_MAX && buf.last() != Some(&b'\n')) {
            let _ = events.send(Input::Closed(generation));
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        if value.get("ev").is_some() {
            if events.send(Input::Event(generation, value)).is_err() {
                return;
            }
        } else if let Some(id) = value.get("id").and_then(Value::as_u64)
            && let Ok(p) = pending.lock()
            && let Some(tx) = u32::try_from(id).ok().and_then(|id| p.get(&id))
        {
            let _ = tx.send(value);
        }
    }
}

/// Spawns `worlds serve --profile public` from this executable, its stdout and stderr shared with
/// the agent's (both are JSON lines to the VM's log; nothing here reads them).
pub fn spawn(
    bind: &str,
    control: &Path,
    max_worlds: usize,
    max_seats: usize,
) -> std::io::Result<Child> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("serve")
        .args(["--profile", "public", "--bind", bind])
        .arg("--control")
        .arg(format!("unix:{}", control.display()))
        .args(["--max-worlds", &max_worlds.to_string()])
        .args(["--max-seats", &max_seats.to_string()])
        .env_remove("FI_DS_CONTROL_SECRET")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

/// The receiver end the loop reads, and its sender for the reader thread and the signal watcher.
pub fn channel() -> (Sender<Input>, Receiver<Input>) {
    mpsc::channel()
}
