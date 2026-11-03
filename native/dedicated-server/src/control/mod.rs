// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The control channel (contract §3): framing, replies and errors, and the two transports (a Unix
//! stream socket the engine listens on, Linux; the launcher's named pipe the engine connects to,
//! Windows and Proton).

pub mod framing;
#[cfg(windows)]
pub mod pipe;
pub mod proto;
#[cfg(unix)]
pub mod unix;

use std::{
    cell::RefCell,
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
};

use serde_json::Value;

/// A control connection's id (unique for the process).
pub type ConnId = u64;

/// The command queue per connection (contract §4.6; the C# size).
pub const COMMAND_QUEUE: usize = 256;
/// Lines waiting for a connection's writer before the loop keeps them in its own queues.
pub const WRITER_QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Unix stream socket: protected by the file system, no secret.
    Unix,
    /// The launcher's named pipe: `hello.secret` must match.
    Pipe,
}

/// What a transport tells the loop.
pub enum ControlMsg {
    Opened {
        conn: ConnId,
        transport: Transport,
        writer: SyncSender<String>,
        inflight: Arc<AtomicUsize>,
    },
    /// A request line that parsed as a JSON object (framing errors are answered by the reader).
    Request {
        conn: ConnId,
        request: Value,
    },
    Closed {
        conn: ConnId,
    },
}

/// Runs a control thread's body; a panic there is an internal fault outside any world (§2.1
/// exit 70): the loop is told (`Hub::Fatal`, logged `ds.fault`) instead of leaving the
/// connection half-open.
pub fn guarded(hub: &crate::runtime::HubTx, what: &'static str, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        let _ = hub.send(crate::runtime::Hub::Fatal(what));
    }
}

/// The reader side of a connection: frames lines, answers framing errors and `busy` itself,
/// hands requests to the loop. Its own answers never block it: past a full writer queue they
/// wait in `backlog`, which the transport drains between reads (on Windows the same thread
/// reads and writes the pipe, so a blocking send there would wait on itself forever).
pub struct Feeder {
    pub conn: ConnId,
    pub writer: SyncSender<String>,
    pub inflight: Arc<AtomicUsize>,
    pub hub: crate::runtime::HubTx,
    pub backlog: RefCell<VecDeque<String>>,
}

impl Feeder {
    pub fn new(
        conn: ConnId,
        writer: SyncSender<String>,
        inflight: Arc<AtomicUsize>,
        hub: crate::runtime::HubTx,
    ) -> Feeder {
        Feeder {
            conn,
            writer,
            inflight,
            hub,
            backlog: RefCell::new(VecDeque::new()),
        }
    }

    /// The answers still waiting, oldest first (the pipe writes them itself).
    pub fn take_backlog(&self) -> Vec<String> {
        self.backlog.borrow_mut().drain(..).collect()
    }

    /// Hands the waiting answers to the writer, blocking (a transport with its own writer
    /// thread); false when the writer is gone.
    pub fn flush_backlog(&self) -> bool {
        let waiting = self.take_backlog();
        waiting
            .into_iter()
            .all(|line| self.writer.send(line).is_ok())
    }

    /// Handles one frame. False when the loop is gone.
    pub fn frame(&self, frame: framing::Frame) -> bool {
        let value = match frame {
            framing::Frame::TooLong => {
                return self.answer(proto::framing_error("line"));
            }
            framing::Frame::Line(bytes) => match proto::parse_line(&bytes) {
                Ok(v) => v,
                Err(field) => return self.answer(proto::framing_error(field)),
            },
        };
        if self.inflight.load(Ordering::Acquire) >= COMMAND_QUEUE {
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            let e = proto::CmdError::busy(proto::BUSY_RETRY_MS);
            return self.answer(proto::reply_err(&id, &e));
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        self.hub
            .send(crate::runtime::Hub::Control(ControlMsg::Request {
                conn: self.conn,
                request: value,
            }))
            .is_ok()
    }

    fn answer(&self, reply: Value) -> bool {
        let mut line = reply.to_string();
        line.push('\n');
        let mut backlog = self.backlog.borrow_mut();
        if !backlog.is_empty() {
            backlog.push_back(line);
            return true;
        }
        match self.writer.try_send(line) {
            Ok(()) => true,
            Err(TrySendError::Full(line)) => {
                backlog.push_back(line);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn closed(&self) {
        let _ = self
            .hub
            .send(crate::runtime::Hub::Control(ControlMsg::Closed {
                conn: self.conn,
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flood_of_bad_lines_never_blocks_the_reader_on_its_own_writer() {
        // nobody drains the writer queue: the pipe thread is busy reading, as on Windows
        let (writer, lines) = std::sync::mpsc::sync_channel::<String>(WRITER_QUEUE);
        let (hub, _loop) = std::sync::mpsc::sync_channel(4);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let feeder = Feeder::new(1, writer, Arc::new(AtomicUsize::new(0)), hub);
            let mut framer = framing::Framer::new();
            let flood = "not json\n".repeat(200);
            let all = framer
                .push(flood.as_bytes())
                .into_iter()
                .all(|f| feeder.frame(f));
            let _ = done.send((all, feeder.take_backlog().len()));
        });
        let (all, waiting) = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the reader blocked on its own writer queue");
        assert!(all);
        assert_eq!(waiting, 200 - WRITER_QUEUE);
        assert_eq!(lines.try_iter().count(), WRITER_QUEUE);
    }
    #[test]
    fn a_panicking_control_thread_reports_a_fault_to_the_loop() {
        let (hub, rx) = std::sync::mpsc::sync_channel(4);
        let t = std::thread::spawn(move || {
            super::guarded(&hub, "control reader", || panic!("synthetic control panic"));
        });
        t.join().unwrap();
        match rx.try_recv() {
            Ok(crate::runtime::Hub::Fatal(what)) => assert_eq!(what, "control reader"),
            _ => panic!("no fault reported"),
        }
    }
}
