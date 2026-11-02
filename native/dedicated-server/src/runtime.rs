// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The process around `core::Server` (contract §2.2, §8): one loop thread owns the server and
//! every world; datagrams, control requests and signals reach it through one bounded hub
//! channel, and it sleeps until the next timer deadline or the next message (no deadline: no
//! wake-up at all). Socket readers and control readers are plain blocking threads that only
//! feed the hub; no thread runs per world and no world state lives outside the loop.
//!
//! Why a hub, not a readiness poll: std has no poll over a UDP socket, a control stream and a
//! timer, and the crate stays dependency-light (no C, cross-builds for Windows without a C
//! toolchain). A later step can swap the hub for `mio` without touching `core`.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::{SocketAddr, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use crate::{
    cli::{Endpoint, RunConfig},
    control::{ConnId, ControlMsg},
    core::{BoundWorld, Config, Io, PUBLIC_SOCKET, Profile, Server, SockId},
    log, log_ids,
    net::{self, BindFailure},
};

/// Messages for the loop.
pub enum Hub {
    Control(ControlMsg),
    /// How to close a connection from the loop's side.
    Closer {
        conn: ConnId,
        closer: Box<dyn Fn() + Send>,
    },
    Datagram {
        sock: SockId,
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
const DATAGRAM_MAX: usize = 2048;
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
    pub const FAULT: i32 = 70;
}

struct RtIo {
    hub: HubTx,
    sockets: HashMap<SockId, (UdpSocket, bool)>,
    stops: HashMap<SockId, Arc<AtomicBool>>,
    next_sock: SockId,
}

impl RtIo {
    fn add_socket(
        &mut self,
        sock: SockId,
        socket: UdpSocket,
        dual: bool,
        timeout: bool,
    ) -> std::io::Result<()> {
        let reader = socket.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        if timeout {
            reader.set_read_timeout(Some(Duration::from_millis(500)))?;
        }
        let hub = self.hub.clone();
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("ds-udp".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| {
                    read_socket(reader, sock, hub.clone(), flag)
                }));
                if r.is_err() {
                    let _ = hub.send(Hub::Fatal("udp reader"));
                }
            })?;
        self.sockets.insert(sock, (socket, dual));
        self.stops.insert(sock, stop);
        Ok(())
    }
}

fn read_socket(socket: UdpSocket, sock: SockId, hub: HubTx, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; DATAGRAM_MAX];
    let mut errors = 0u32;
    while !stop.load(Ordering::Acquire) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                errors = 0;
                let msg = Hub::Datagram {
                    sock,
                    from,
                    data: buf[..n].to_vec(),
                };
                if hub.send(msg).is_err() {
                    return;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                // Windows reports an earlier send's ICMP unreachable as WSAECONNRESET on recv;
                // never fatal, but never a busy spin either
                errors = errors.saturating_add(1);
                if errors > 8 {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

impl Io for RtIo {
    fn bind_world(&mut self, port: u16) -> Result<BoundWorld, BindFailure> {
        let bound = net::bind_world(port)?;
        let local = bound.socket.local_addr().map_err(|_| BindFailure::Other)?;
        self.next_sock += 1;
        let sock = self.next_sock;
        self.add_socket(sock, bound.socket, bound.dual, true)
            .map_err(|_| BindFailure::Other)?;
        Ok(BoundWorld {
            sock,
            port: local.port(),
            dual: bound.dual,
        })
    }

    fn release_world(&mut self, sock: SockId) {
        if let Some(stop) = self.stops.remove(&sock) {
            stop.store(true, Ordering::Release);
        }
        self.sockets.remove(&sock);
    }

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
    inflight: Arc<AtomicUsize>,
    replies: VecDeque<String>,
    closer: Option<Box<dyn Fn() + Send>>,
    stalled_since: Option<Instant>,
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
    let mut cfg = Config::new(rc.profile);
    cfg.max_worlds = rc.max_worlds;
    cfg.max_seats = rc.max_seats;
    cfg.memory_limit_mb = rc.memory_limit_mb;
    cfg.drain_seconds = rc.drain_seconds;
    cfg.stats_seconds = rc.stats_seconds;
    cfg.relay = rc.relay;
    cfg.control_secret = rc.secret.clone();
    if let Some(k) = &rc.grant_keys {
        cfg.keys = k.clone();
    }
    let mut catalog = crate::fh2::Catalog::default();
    if let Some(db) = &rc.game_db {
        catalog.path = Some(db.clone());
        // Step 1 reads no catalog yet (the SQLite reader comes with Road Trip and the parts
        // rule); a missing or unreadable file is never a start failure (contract §9).
        let readable = std::fs::File::open(db).is_ok();
        log::info(log_ids::DS_CATALOG)
            .field("readable", readable)
            .field("loaded", false)
            .emit();
    }
    cfg.games = crate::registered_games(catalog);
    let ids = Arc::new(AtomicU64::new(1));
    // the control endpoint
    match &rc.control {
        #[cfg(unix)]
        Endpoint::Unix(path) => {
            use crate::control::unix;
            match unix::listen(path) {
                Ok(listener) => {
                    if unix::serve(listener, hub.clone(), ids.clone()).is_err() {
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
        #[cfg(windows)]
        Endpoint::Pipe(name) => {
            use crate::control::pipe;
            let started = pipe::connect(name)
                .and_then(|f| pipe::serve(f, ids.fetch_add(1, Ordering::Relaxed), hub.clone()));
            if let Err(e) = started {
                log::error(log_ids::DS_CONTROL_UNAVAILABLE)
                    .msg(&e.to_string())
                    .emit();
                return exit::CONTROL_UNAVAILABLE;
            }
        }
        #[allow(unreachable_patterns)]
        _ => {
            log::error(log_ids::DS_CONTROL_UNAVAILABLE)
                .field("reason", "unsupported")
                .emit();
            return exit::CONTROL_UNAVAILABLE;
        }
    }
    let mut io = RtIo {
        hub: hub.clone(),
        sockets: HashMap::new(),
        stops: HashMap::new(),
        next_sock: PUBLIC_SOCKET,
    };
    if rc.profile == Profile::Public {
        let socket = match net::bind_loopback(rc.bind) {
            Ok(s) => s,
            Err(e) => {
                log::error(log_ids::DS_START_FAILED)
                    .field("what", "socket")
                    .msg(&e.to_string())
                    .emit();
                return exit::START_FAILED;
            }
        };
        if io.add_socket(PUBLIC_SOCKET, socket, false, false).is_err() {
            log::error(log_ids::DS_START_FAILED)
                .field("what", "socket")
                .emit();
            return exit::START_FAILED;
        }
        cfg.public_socket = true;
    }
    #[cfg(unix)]
    if let Err(e) = signals::install(hub.clone()) {
        log::warn(log_ids::DS_SIGNAL).msg(&e.to_string()).emit();
    }
    log::info(log_ids::DS_STARTED)
        .field("profile", rc.profile.as_str())
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
                    transport,
                    writer,
                    inflight,
                }) => {
                    server.connect(conn, transport);
                    conns.insert(
                        conn,
                        Conn {
                            writer,
                            inflight,
                            replies: VecDeque::new(),
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
                        let depth = c.inflight.fetch_sub(1, Ordering::AcqRel) as u64;
                        server.note_queue_depth(depth);
                        let mut line = reply.to_string();
                        line.push('\n');
                        c.replies.push_back(line);
                    }
                }
                Hub::Control(ControlMsg::Closed { conn }) => {
                    conns.remove(&conn);
                    server.disconnect(io, now, conn);
                }
                Hub::Datagram { sock, from, data } => server.datagram(io, now, sock, from, &data),
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
            let Some((socket, dual)) = io.sockets.get(&d.sock) else {
                continue;
            };
            let to = crate::stun::for_socket(d.to, *dual);
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
        if let Some(cid) = controller
            && let Some(c) = conns.get_mut(&cid)
        {
            let blocked = server.events_full();
            match (blocked, c.stalled_since) {
                (true, None) => c.stalled_since = Some(Instant::now()),
                (true, Some(t)) if t.elapsed() >= STALL => {
                    c.close();
                    server.controller_stalled(io, mono());
                }
                (false, _) => c.stalled_since = None,
                _ => {}
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

/// Moves queued replies (and, for the controller, events) into the connection's writer.
fn flush_conn(c: &mut Conn, events: Option<&mut Server>) {
    while let Some(line) = c.replies.pop_front() {
        match c.writer.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(line)) => {
                c.replies.push_front(line);
                return;
            }
            Err(TrySendError::Disconnected(_)) => {
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
                    c.replies.push_back(line);
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
