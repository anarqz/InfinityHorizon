// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! JSON-line logs on stdout (contract §2.3): one object per line,
//! `{"ts":"<RFC 3339 UTC ms>","lvl":"info","ev":"<stable id>", ...fields}`, at most 8 KiB.
//!
//! Written off the loop: `emit` hands the line to a bounded queue (1024 lines) drained by a writer
//! thread; a full queue drops the line and counts it (`dropped()`, the stats' `logDropped`), so a
//! slow reader of stdout never stalls a world. Before `start` (argument parsing) lines are written
//! directly.
//!
//! Never log a XUID, gamertag, IP address, peer port, grant claims, `udpProof` key, control secret
//! or any game payload. Peers are their engine index, worlds their UUID, grants at most their
//! first 8 hex digits; free text goes through `anon::anonymous`.

use std::{
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

pub const QUEUE_LINES: usize = 1024;
pub const LINE_MAX: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }
}

struct Logger {
    tx: Mutex<Option<SyncSender<String>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Starts the writer thread. Later calls do nothing.
pub fn start(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
    LOGGER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<String>(QUEUE_LINES);
        let writer = std::thread::Builder::new()
            .name("ds-log".into())
            .spawn(move || {
                let stdout = std::io::stdout();
                while let Ok(first) = rx.recv() {
                    let mut out = stdout.lock();
                    let _ = out.write_all(first.as_bytes());
                    // drain what is already queued before flushing once
                    while let Ok(line) = rx.try_recv() {
                        let _ = out.write_all(line.as_bytes());
                    }
                    let _ = out.flush();
                }
            })
            .ok();
        Logger {
            tx: Mutex::new(Some(tx)),
            writer: Mutex::new(writer),
        }
    });
}

/// Stops accepting lines and waits (bounded) for the writer to finish what is queued.
pub fn shutdown(wait: Duration) {
    let Some(logger) = LOGGER.get() else {
        let _ = std::io::stdout().flush();
        return;
    };
    if let Ok(mut tx) = logger.tx.lock() {
        tx.take();
    }
    let handle = logger.writer.lock().ok().and_then(|mut w| w.take());
    if let Some(handle) = handle {
        let deadline = std::time::Instant::now() + wait;
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

/// Lines dropped because the queue was full.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Lines handed to the writer.
pub fn written() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

/// A log line under construction.
#[must_use = "a log line does nothing until emit()"]
pub struct Line {
    level: Level,
    fields: Map<String, Value>,
}

pub fn line(level: Level, ev: &str) -> Line {
    let mut fields = Map::new();
    fields.insert("ts".into(), Value::String(timestamp(SystemTime::now())));
    fields.insert("lvl".into(), Value::String(level.as_str().into()));
    fields.insert("ev".into(), Value::String(ev.into()));
    Line { level, fields }
}

pub fn error(ev: &str) -> Line {
    line(Level::Error, ev)
}
pub fn warn(ev: &str) -> Line {
    line(Level::Warn, ev)
}
pub fn info(ev: &str) -> Line {
    line(Level::Info, ev)
}
pub fn debug(ev: &str) -> Line {
    line(Level::Debug, ev)
}

impl Line {
    pub fn world(self, id: &str) -> Line {
        self.field("world", id)
    }

    pub fn field(mut self, key: &str, value: impl Into<Value>) -> Line {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Free text (an OS error, a panic message), anonymised.
    pub fn msg(self, text: &str) -> Line {
        let text = crate::anon::anonymous(text);
        self.field("msg", text)
    }

    /// The serialized line with its newline, cut to `LINE_MAX`.
    pub fn render(self) -> String {
        let mut text = Value::Object(self.fields.clone()).to_string();
        if text.len() >= LINE_MAX {
            let mut short = Map::new();
            for k in ["ts", "lvl", "ev", "world", "engine", "peer"] {
                if let Some(v) = self.fields.get(k) {
                    short.insert(k.into(), v.clone());
                }
            }
            short.insert("truncated".into(), Value::Bool(true));
            text = Value::Object(short).to_string();
        }
        text.push('\n');
        text
    }

    pub fn emit(self) {
        if !enabled(self.level) {
            return;
        }
        let text = self.render();
        match LOGGER.get() {
            None => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                WRITTEN.fetch_add(1, Ordering::Relaxed);
            }
            Some(logger) => {
                let guard = logger.tx.lock();
                let Ok(guard) = guard else { return };
                match guard.as_ref().map(|tx| tx.try_send(text)) {
                    Some(Ok(())) => {
                        WRITTEN.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(Err(TrySendError::Full(_))) | Some(Err(TrySendError::Disconnected(_))) => {
                        DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

/// RFC 3339 UTC with milliseconds: `2026-11-02T12:34:56.789Z`.
pub fn timestamp(t: SystemTime) -> String {
    let ms = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
