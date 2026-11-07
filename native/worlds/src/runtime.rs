// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The process around `core::Server` (contract §2.2, §8): one loop thread owns the server and
//! every world; datagrams, control requests and signals reach it through one bounded hub
//! channel, and it sleeps until the next timer deadline or the next message (no deadline: no
//! wake-up at all). Socket readers and control readers are plain blocking threads that only
//! feed the hub; no thread runs per world and no world state lives outside the loop.
//!
//! Why a hub, not a readiness poll: std has no poll over a UDP socket, a control stream and a
//! timer, and the crate stays dependency-light. A later step can swap the hub for `mio` without
//! touching `core`.

use std::{
    collections::{BTreeMap, VecDeque},
    net::{SocketAddr, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use crate::{
    cli::RunConfig,
    control::{ConnId, ControlMsg},
    core::{Config, Io, Server},
    log, log_ids, net,
};

/// Messages for the loop.
pub enum Hub {
    Control(ControlMsg),
    /// How to close a connection from the loop's side.
    Closer {
        conn: ConnId,
        closer: Box<dyn Fn() + Send>,
    },
    /// A datagram on the public socket.
    Datagram {
        from: SocketAddr,
        data: Vec<u8>,
    },
    Signal(Signal),
    /// A helper thread panicked: an internal fault outside any world (exit 70).
    Fatal(&'static str),
}

pub type HubTx = SyncSender<Hub>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Int,
}

/// The hub's bound: a flood fills it and the socket readers block, so the kernel drops instead.
pub const HUB_QUEUE: usize = 8192;
/// At most this many messages per loop turn before the timers run (commands: at most 64,
/// contract §4.6).
const TURN_MESSAGES: usize = 256;
const TURN_COMMANDS: usize = 64;
/// Datagrams longer than this are dropped in the reader (nothing legitimate is: the game's limit
/// is 1,372 bytes and the engine refuses the rest whole). The read buffer itself holds any UDP
/// payload, so a long datagram is never truncated into something else, and Windows never
/// reports it as WSAEMSGSIZE.
const DATAGRAM_MAX: usize = 2048;
const RECV_BUFFER: usize = 64 * 1024;
/// WSAEMSGSIZE: a datagram longer than the buffer (Windows; no `ErrorKind` of its own).
const WSAEMSGSIZE: i32 = 10040;
/// A controller that does not read for this long while its queue is full is disconnected.
const STALL: Duration = Duration::from_secs(30);
/// How long the loop keeps flushing replies after deciding to exit.
const EXIT_FLUSH: Duration = Duration::from_secs(2);

/// Exit codes (contract §2.1).
pub mod exit {
    pub const OK: i32 = 0;
    pub const CONFIG: i32 = 2;
    pub const CONTROL_UNAVAILABLE: i32 = 3;
    pub const UNAUTHORIZED: i32 = 4;
    pub const PROTO: i32 = 5;
    pub const START_FAILED: i32 = 6;
    /// `worlds relay` / `worlds agent`: the role is not implemented in this build yet.
    pub const ROLE_UNIMPLEMENTED: i32 = 7;
    pub const FAULT: i32 = 70;
}

/// The system side: the public socket (its reader feeds the hub) and the process's measures.
struct RtIo {
    /// The public socket and whether it is dual-stack.
    socket: (UdpSocket, bool),
}

impl RtIo {
    /// Starts the public socket's reader. It runs for the process's life (the loop never stops
    /// it: the process exits).
    fn start(hub: HubTx, socket: UdpSocket, dual: bool) -> std::io::Result<RtIo> {
        net::quiet_connreset(&socket);
        let reader = socket.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        std::thread::Builder::new()
            .name("ds-udp".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| read_socket(reader, hub.clone(), stop)));
                if r.is_err() {
                    let _ = hub.send(Hub::Fatal("udp reader"));
                }
            })?;
        Ok(RtIo {
            socket: (socket, dual),
        })
    }
}

fn read_socket(socket: UdpSocket, hub: HubTx, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; RECV_BUFFER];
    let mut errors = 0u32;
    while !stop.load(Ordering::Acquire) {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) if n > DATAGRAM_MAX => errors = 0,
            Ok((n, from)) => {
                errors = 0;
                let mut msg = Hub::Datagram {
                    from,
                    data: buf[..n].to_vec(),
                };
                // never parked on a full hub once stopped: it is the loop that drains the hub
                loop {
                    match hub.try_send(msg) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Full(back)) => {
                            if stop.load(Ordering::Acquire) {
                                return;
                            }
                            msg = back;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            // one datagram lost, never the socket: an oversize datagram (WSAEMSGSIZE) or an
            // earlier send's ICMP port unreachable (WSAECONNRESET, also switched off with
            // SIO_UDP_CONNRESET) is dropped and the next read follows at once, as the C#
            // reference's drain loop does; neither counts toward the error back-off
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.raw_os_error() == Some(WSAEMSGSIZE) => {}
            Err(_) => {
                // anything else is unexpected: never fatal, but never a busy spin either
                errors = errors.saturating_add(1);
                if errors > 8 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

impl Io for RtIo {
    fn memory_used_bytes(&self) -> u64 {
        crate::alloc::live_bytes() as u64
    }

    fn cpu_ms(&self) -> Option<u64> {
        proc_stat_cpu_ms()
    }

    fn rss_bytes(&self) -> Option<u64> {
        proc_statm_rss()
    }
}

/// utime + stime of /proc/self/stat (Linux; ticks at the usual 100 Hz).
fn proc_stat_cpu_ms() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &text[text.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) * 10)
}

fn proc_statm_rss() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

struct Conn {
    writer: SyncSender<String>,
    /// Requests taken and not yet written back: a request counts until its reply is on the
    /// writer, so replies the writer cannot take keep the reader answering `busy` (§4.6).
    inflight: Arc<AtomicUsize>,
    /// Lines the writer could not take yet: (line, whether it is a reply; else an event).
    replies: Backlog,
    closer: Option<Box<dyn Fn() + Send>>,
    stalled_since: Option<Instant>,
}

/// A connection's unwritten lines with a running count of the replies among them, so the
/// stall check each turn does not walk the queue.
#[derive(Default)]
struct Backlog {
    lines: VecDeque<(String, bool)>,
    replies: usize,
}

impl Backlog {
    fn push_back(&mut self, line: (String, bool)) {
        self.replies += usize::from(line.1);
        self.lines.push_back(line);
    }

    fn push_front(&mut self, line: (String, bool)) {
        self.replies += usize::from(line.1);
        self.lines.push_front(line);
    }

    fn pop_front(&mut self) -> Option<(String, bool)> {
        let line = self.lines.pop_front()?;
        self.replies -= usize::from(line.1);
        Some(line)
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.replies = 0;
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lines.len()
    }

    fn reply_count(&self) -> usize {
        self.replies
    }
}

impl Conn {
    fn close(&mut self) {
        if let Some(c) = &self.closer {
            c();
        }
    }
}

/// Starts the control endpoint, the public socket and the signal watcher, then runs the loop on
/// its own thread. Returns the exit code.
pub fn run(rc: RunConfig) -> i32 {
    let (hub, rx) = mpsc::sync_channel::<Hub>(HUB_QUEUE);
    let mut cfg = Config::new();
    cfg.max_worlds = rc.max_worlds;
    cfg.max_seats = rc.max_seats;
    cfg.memory_limit_mb = rc.memory_limit_mb;
    cfg.drain_seconds = rc.drain_seconds;
    cfg.stats_seconds = rc.stats_seconds;
    if let Some(k) = &rc.grant_keys {
        cfg.keys = k.clone();
    }
    let mut catalog = crate::fh2::Catalog::default();
    if let Some(db) = &rc.game_db {
        // deprecated (contract §9): the engine reads no game file; the catalogs come through
        // `catalog.set` / `catalog.parts`
        catalog.path = Some(db.clone());
        log::info(log_ids::DS_CATALOG).field("ignored", true).emit();
    }
    cfg.games = crate::registered_games(catalog);
    // the control endpoint: a Unix socket (Linux, the Worlds host); another build has none
    #[cfg(unix)]
    {
        use crate::control::unix;
        let crate::cli::Endpoint::Unix(path) = &rc.control;
        let ids = Arc::new(std::sync::atomic::AtomicU64::new(1));
        match unix::listen(path) {
            Ok(listener) => {
                if unix::serve(listener, hub.clone(), ids).is_err() {
                    log::error(log_ids::DS_CONTROL_UNAVAILABLE).emit();
                    return exit::CONTROL_UNAVAILABLE;
                }
            }
            Err(unix::ListenError::DirectoryMode(mode)) => {
                log::error(log_ids::DS_CONTROL_DIR_MODE)
                    .field("mode", format!("{mode:o}"))
                    .emit();
                return exit::CONTROL_UNAVAILABLE;
            }
            Err(unix::ListenError::DirectoryOwner) => {
                log::error(log_ids::DS_CONTROL_UNAVAILABLE)
                    .field("reason", "dir_owner")
                    .emit();
                return exit::CONTROL_UNAVAILABLE;
            }
            Err(unix::ListenError::InUse) => {
                log::error(log_ids::DS_CONTROL_UNAVAILABLE)
                    .field("reason", "in_use")
                    .emit();
                return exit::CONTROL_UNAVAILABLE;
            }
            Err(unix::ListenError::Io(e)) => {
                log::error(log_ids::DS_CONTROL_UNAVAILABLE)
                    .msg(&e.to_string())
                    .emit();
                return exit::CONTROL_UNAVAILABLE;
            }
        }
    }
    #[cfg(not(unix))]
    if matches!(rc.control, crate::cli::Endpoint::Unix(_)) {
        log::error(log_ids::DS_CONTROL_UNAVAILABLE)
            .field("reason", "unsupported")
            .emit();
        return exit::CONTROL_UNAVAILABLE;
    }
    let bound = match net::bind_public(rc.bind) {
        Ok(b) => b,
        Err(e) => {
            log::error(log_ids::DS_START_FAILED)
                .field("what", "socket")
                .field("reason", e.reason())
                .emit();
            return exit::START_FAILED;
        }
    };
    let Ok(mut io) = RtIo::start(hub.clone(), bound.socket, bound.dual) else {
        log::error(log_ids::DS_START_FAILED)
            .field("what", "socket")
            .emit();
        return exit::START_FAILED;
    };
    cfg.public_socket = true;
    #[cfg(unix)]
    if let Err(e) = signals::install(hub.clone()) {
        log::warn(log_ids::DS_SIGNAL).msg(&e.to_string()).emit();
    }
    log::info(log_ids::DS_STARTED)
        .field("profile", crate::cli::PROFILE)
        .field("version", crate::VERSION)
        .field("commit", crate::commit())
        .field("proto", crate::PROTO_MAX)
        .field("maxWorlds", rc.max_worlds)
        .field("maxSeats", rc.max_seats)
        .field("memoryLimitMb", rc.memory_limit_mb)
        .emit();
    drop(hub);
    let drain_seconds = rc.drain_seconds;
    let started = std::thread::Builder::new()
        .name("ds-loop".into())
        .spawn(move || {
            let seed = crate::core::rng::seed_from_os();
            let mut server = Server::new(cfg, seed, 0);
            run_loop(&mut server, &mut io, rx, drain_seconds)
        });
    let code = match started.map(|h| h.join()) {
        Ok(Ok(code)) => code,
        _ => {
            log::error(log_ids::DS_FAULT).field("where", "loop").emit();
            exit::FAULT
        }
    };
    log::info(log_ids::DS_STOPPED).field("code", code).emit();
    code
}

fn run_loop(
    server: &mut Server,
    io: &mut RtIo,
    rx: mpsc::Receiver<Hub>,
    drain_seconds: u64,
) -> i32 {
    let start = Instant::now();
    let mono = || start.elapsed().as_millis() as u64;
    let mut conns: BTreeMap<ConnId, Conn> = BTreeMap::new();
    let mut exit_at: Option<(i32, Instant)> = None;
    loop {
        let deadline = server.next_deadline();
        let pending_output = conns.values().any(|c| !c.replies.is_empty())
            || server
                .controller()
                .is_some_and(|_| server_has_events(server));
        let mut wait = deadline.map(|d| Duration::from_millis(d.saturating_sub(mono())));
        if pending_output || exit_at.is_some() {
            let short = Duration::from_millis(5);
            wait = Some(wait.map_or(short, |w| w.min(short)));
        }
        let first = match wait {
            Some(w) => rx.recv_timeout(w),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let now = mono();
        if let Some(d) = deadline
            && now >= d
        {
            server.note_lag(now, now - d);
        }
        let mut messages = Vec::new();
        match first {
            Ok(m) => messages.push(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // every feeder is gone (no control, no socket): nothing can reach us any more
                if exit_at.is_none() {
                    return exit::OK;
                }
            }
        }
        let mut commands = messages
            .iter()
            .filter(|m| matches!(m, Hub::Control(ControlMsg::Request { .. })))
            .count();
        while messages.len() < TURN_MESSAGES && commands < TURN_COMMANDS {
            match rx.try_recv() {
                Ok(m) => {
                    if matches!(m, Hub::Control(ControlMsg::Request { .. })) {
                        commands += 1;
                    }
                    messages.push(m);
                }
                Err(_) => break,
            }
        }
        for m in messages {
            match m {
                Hub::Control(ControlMsg::Opened {
                    conn,
                    writer,
                    inflight,
                }) => {
                    server.connect(conn);
                    conns.insert(
                        conn,
                        Conn {
                            writer,
                            inflight,
                            replies: Backlog::default(),
                            closer: None,
                            stalled_since: None,
                        },
                    );
                }
                Hub::Closer { conn, closer } => {
                    if let Some(c) = conns.get_mut(&conn) {
                        c.closer = Some(closer);
                    }
                }
                Hub::Control(ControlMsg::Request { conn, request }) => {
                    let reply = server.request(io, now, conn, &request);
                    if let Some(c) = conns.get_mut(&conn) {
                        server.note_queue_depth(c.inflight.load(Ordering::Acquire) as u64);
                        let mut line = reply.to_string();
                        line.push('\n');
                        c.replies.push_back((line, true));
                    }
                }
                Hub::Control(ControlMsg::Closed { conn }) => {
                    conns.remove(&conn);
                    server.disconnect(now, conn);
                }
                Hub::Datagram { from, data } => server.datagram(io, now, from, &data),
                Hub::Signal(s) => {
                    log::info(log_ids::DS_SIGNAL)
                        .field("signal", if s == Signal::Term { "term" } else { "int" })
                        .emit();
                    if s == Signal::Term && exit_at.is_none() {
                        server.start_drain(io, drain_seconds);
                    } else {
                        server.stop_now(io);
                    }
                }
                Hub::Fatal(what) => {
                    log::error(log_ids::DS_FAULT).field("where", what).emit();
                    return exit::FAULT;
                }
            }
        }
        server.tick(io, mono());
        for d in server.take_datagrams() {
            let (socket, dual) = &io.socket;
            let to = crate::stun::endpoint_for_socket(d.to, *dual);
            match socket.send_to(&d.data, to) {
                Ok(_) => server.note_sent(1),
                Err(_) => server.note_send_error(),
            }
        }
        for conn in server.take_closes() {
            if let Some(c) = conns.get_mut(&conn) {
                flush_conn(c, None);
                c.close();
            }
        }
        // replies first, then the controller's events
        let controller = server.controller();
        for (id, c) in conns.iter_mut() {
            let events = (Some(*id) == controller).then_some(&mut *server);
            flush_conn(c, events);
        }
        // a connection that does not read for 30 s while its queue is full is disconnected:
        // the controller's event queue, or any connection's replies (a full command queue's
        // worth waiting for the writer)
        let events_full = server.events_full();
        let mut stalled = Vec::new();
        for (id, c) in conns.iter_mut() {
            let blocked = (Some(*id) == controller && events_full) || reply_backlog_full(c);
            match (blocked, c.stalled_since) {
                (true, None) => c.stalled_since = Some(Instant::now()),
                (true, Some(t)) if t.elapsed() >= STALL => stalled.push(*id),
                (false, _) => c.stalled_since = None,
                _ => {}
            }
        }
        for id in stalled {
            if let Some(c) = conns.get_mut(&id) {
                c.close();
                c.stalled_since = None;
            }
            if Some(id) == controller {
                server.controller_stalled(mono());
            }
        }
        if let Some(code) = server.exit_code() {
            let (code, since) = *exit_at.get_or_insert((code, Instant::now()));
            let flushed =
                conns.values().all(|c| c.replies.is_empty()) && !server_has_events(server);
            if flushed || since.elapsed() >= EXIT_FLUSH {
                // give the writers a moment to put the last lines on the wire
                std::thread::sleep(Duration::from_millis(50));
                for c in conns.values_mut() {
                    c.close();
                }
                return code;
            }
        }
    }
}

fn server_has_events(server: &Server) -> bool {
    server.controller().is_some() && server.has_events()
}

/// Whether a connection's unwritten replies fill a command queue.
fn reply_backlog_full(c: &Conn) -> bool {
    c.replies.reply_count() >= crate::control::COMMAND_QUEUE
}

/// Moves queued replies (and, for the controller, events) into the connection's writer; a
/// request stops counting toward its connection's queue when its reply is on the writer.
fn flush_conn(c: &mut Conn, events: Option<&mut Server>) {
    while let Some((line, reply)) = c.replies.pop_front() {
        match c.writer.try_send(line) {
            Ok(()) => {
                if reply {
                    c.inflight.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Err(TrySendError::Full(line)) => {
                c.replies.push_front((line, reply));
                return;
            }
            Err(TrySendError::Disconnected(_)) => {
                let replies = c.replies.reply_count() + usize::from(reply);
                c.inflight.fetch_sub(
                    replies.min(c.inflight.load(Ordering::Acquire)),
                    Ordering::AcqRel,
                );
                c.replies.clear();
                return;
            }
        }
    }
    if let Some(server) = events {
        while let Some(line) = server.pop_event_line() {
            match c.writer.try_send(line) {
                Ok(()) => {}
                Err(TrySendError::Full(line)) | Err(TrySendError::Disconnected(line)) => {
                    // keep it for the next turn (it already has its seq)
                    c.replies.push_back((line, false));
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
mod signals {
    //! SIGTERM (drain) and SIGINT (stop) through a self-pipe: the handler only writes one byte
    //! (async-signal-safe); a thread turns it into a hub message.
    use std::{
        ffi::{c_int, c_void},
        io::Read,
        os::{fd::IntoRawFd, unix::net::UnixStream},
        sync::atomic::{AtomicI32, Ordering},
    };

    use super::{Hub, HubTx, Signal};

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    static FD: AtomicI32 = AtomicI32::new(-1);

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn signal(sig: c_int, handler: usize) -> usize;
        fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    }

    #[allow(unsafe_code)]
    extern "C" fn on_signal(sig: c_int) {
        let fd = FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = sig as u8;
            // SAFETY: write(2) is async-signal-safe; the buffer is a live local byte.
            unsafe {
                write(fd, &b as *const u8 as *const c_void, 1);
            }
        }
    }

    #[allow(unsafe_code)]
    pub fn install(hub: HubTx) -> std::io::Result<()> {
        let (mut rx, tx) = UnixStream::pair()?;
        FD.store(tx.into_raw_fd(), Ordering::Relaxed);
        // SAFETY: installs a handler that only does an async-signal-safe write.
        unsafe {
            signal(SIGTERM, on_signal as extern "C" fn(c_int) as usize);
            signal(SIGINT, on_signal as extern "C" fn(c_int) as usize);
        }
        std::thread::Builder::new()
            .name("ds-signals".into())
            .spawn(move || {
                let mut b = [0u8; 1];
                while rx.read_exact(&mut b).is_ok() {
                    let s = if b[0] as c_int == SIGTERM {
                        Signal::Term
                    } else {
                        Signal::Int
                    };
                    if hub.send(Hub::Signal(s)).is_err() {
                        return;
                    }
                }
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_drops_an_oversize_datagram_and_keeps_reading() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let to = socket.local_addr().unwrap();
        let (hub, rx) = mpsc::sync_channel::<Hub>(16);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let reader = std::thread::spawn(move || read_socket(socket, hub, flag));
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&[1u8; 3000], to).unwrap();
        sender.send_to(&[2u8; 10], to).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Hub::Datagram { data, .. }) => {
                assert_eq!(
                    data,
                    vec![2u8; 10],
                    "the oversize one never reaches the loop"
                );
            }
            _ => panic!("no datagram"),
        }
        stop.store(true, Ordering::Release);
        reader.join().unwrap();
    }
    #[test]
    fn a_stopped_reader_never_waits_on_a_full_hub() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let to = socket.local_addr().unwrap();
        // a hub of one, already full, that nobody drains (the loop is busy joining)
        let (hub, _full) = mpsc::sync_channel::<Hub>(1);
        hub.send(Hub::Fatal("filler")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let reader = std::thread::spawn(move || read_socket(socket, hub, flag));
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(&[1u8; 8], to)
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);
        let (done, joined) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(reader.join().is_ok());
        });
        assert_eq!(joined.recv_timeout(Duration::from_secs(5)), Ok(true));
    }

    #[test]
    fn the_public_socket_feeds_the_hub_and_a_taken_port_is_in_use() {
        let bound = net::bind_public("127.0.0.1:0".parse().unwrap()).expect("a free port");
        let to = bound.socket.local_addr().unwrap();
        assert!(!bound.dual, "a literal address is a plain bind");
        // the same port again: in use, never another port
        assert_eq!(
            net::bind_public(to).err(),
            Some(net::BindFailure::InUse),
            "the port never moves"
        );
        let (hub, rx) = mpsc::sync_channel::<Hub>(16);
        let io = RtIo::start(hub, bound.socket, bound.dual).expect("the reader starts");
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&[3u8; 12], to).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Hub::Datagram { from, data }) => {
                assert_eq!(from, sender.local_addr().unwrap());
                assert_eq!(data, vec![3u8; 12]);
            }
            _ => panic!("no datagram"),
        }
        // the loop answers on the same socket
        let (socket, dual) = &io.socket;
        let back = crate::stun::endpoint_for_socket(sender.local_addr().unwrap(), *dual);
        socket.send_to(b"ok", back).unwrap();
        sender
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = sender.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"ok"[..], to));
    }

    #[test]
    fn a_request_counts_toward_busy_until_its_reply_is_written() {
        let (writer, lines) = mpsc::sync_channel::<String>(1);
        let inflight = Arc::new(AtomicUsize::new(3));
        let mut c = Conn {
            writer,
            inflight: inflight.clone(),
            replies: Backlog::default(),
            closer: None,
            stalled_since: None,
        };
        for i in 0..3 {
            c.replies.push_back((format!("{i}\n"), true));
        }
        c.replies.push_back(("event\n".into(), false));
        flush_conn(&mut c, None);
        assert_eq!(
            inflight.load(Ordering::Acquire),
            2,
            "one reply on the writer"
        );
        assert_eq!(c.replies.len(), 3);
        assert!(!reply_backlog_full(&c));
        assert_eq!(lines.try_recv().unwrap(), "0\n");
        flush_conn(&mut c, None);
        assert_eq!(inflight.load(Ordering::Acquire), 1);
        for i in 0..crate::control::COMMAND_QUEUE {
            c.replies.push_back((format!("x{i}\n"), true));
        }
        assert!(
            reply_backlog_full(&c),
            "a full queue's worth of unwritten replies"
        );
        // the writer gone: nothing stays counted
        drop(lines);
        inflight.store(1 + crate::control::COMMAND_QUEUE, Ordering::Release);
        flush_conn(&mut c, None);
        assert!(c.replies.is_empty());
        assert_eq!(inflight.load(Ordering::Acquire), 0);
    }
}
