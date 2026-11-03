// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The named-pipe client (contract §3.1, Windows and Proton): the launcher creates
//! `\\.\pipe\fi-ds-<32 hex>` before spawning the engine, which connects as the client, retrying
//! while the pipe is not created yet or busy for up to 5 s (then exit 3). The client asks for
//! identification-only impersonation (`SECURITY_IDENTIFICATION`), so a squatting server cannot act
//! as the engine's user. The secret check is the engine's (`core::Server`, `hello.secret`).
//!
//! One thread does both directions: a synchronous pipe handle serialises its I/O, so a blocked
//! read would stall every write. The thread writes what is queued, reads only what
//! `PeekNamedPipe` says is there, and sleeps briefly (1 to 10 ms) when idle.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, TryRecvError},
    },
    time::{Duration, Instant},
};

use super::{ControlMsg, Feeder, Transport, WRITER_QUEUE, framing};
use crate::runtime::Hub;

pub const CONNECT_WAIT: Duration = Duration::from_secs(5);
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_NO_DATA: i32 = 232;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;

#[allow(unsafe_code)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn PeekNamedPipe(
        pipe: *mut core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
}

/// Whether `name` is `\\.\pipe\fi-ds-<32 lowercase hex>`.
pub fn valid_name(name: &str) -> bool {
    name.strip_prefix(r"\\.\pipe\fi-ds-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Connects, retrying "not created yet" and "busy" for up to `CONNECT_WAIT`.
pub fn connect(name: &str) -> io::Result<File> {
    let deadline = Instant::now() + CONNECT_WAIT;
    loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
            .open(name)
        {
            Ok(f) => return Ok(f),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PIPE_BUSY)
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

#[allow(unsafe_code)]
fn available(file: &File) -> io::Result<u32> {
    let mut avail: u32 = 0;
    // SAFETY: the handle is the live pipe handle owned by `file`; null buffer with size 0 only
    // queries the byte count, written to a local.
    let ok = unsafe {
        PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut avail,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(avail)
    }
}

fn is_eof(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_BROKEN_PIPE) | Some(ERROR_NO_DATA) | Some(ERROR_PIPE_NOT_CONNECTED)
    )
}

/// Runs the connection on a thread of its own.
pub fn serve(file: File, conn: u64, hub: crate::runtime::HubTx) -> io::Result<()> {
    let (writer, lines) = mpsc::sync_channel::<String>(WRITER_QUEUE);
    let inflight = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let close_flag = closed.clone();
    let closer: Box<dyn Fn() + Send> = Box::new(move || close_flag.store(true, Ordering::Release));
    hub.send(Hub::Control(ControlMsg::Opened {
        conn,
        transport: Transport::Pipe,
        writer: writer.clone(),
        inflight: inflight.clone(),
    }))
    .map_err(|_| io::Error::other("loop gone"))?;
    hub.send(Hub::Closer { conn, closer })
        .map_err(|_| io::Error::other("loop gone"))?;
    let fault = hub.clone();
    let feeder = Feeder::new(conn, writer, inflight, hub);
    std::thread::Builder::new()
        .name("ds-control-pipe".into())
        .spawn(move || {
            super::guarded(&fault, "control pipe", move || {
                run(file, lines, closed, feeder)
            })
        })?;
    Ok(())
}

fn run(mut file: File, lines: mpsc::Receiver<String>, closed: Arc<AtomicBool>, feeder: Feeder) {
    let mut framer = framing::Framer::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut idle_ms = 1u64;
    'run: loop {
        let mut busy = false;
        // writes first: replies and events waiting for the launcher
        loop {
            match lines.try_recv() {
                Ok(line) => {
                    busy = true;
                    if file.write_all(line.as_bytes()).is_err() {
                        break 'run;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'run,
            }
        }
        if closed.load(Ordering::Acquire) {
            // the loop closes after its last reply (an `unauthorized`): that reply still goes out
            while let Ok(line) = lines.try_recv() {
                if file.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            let _ = file.flush();
            break;
        }
        match available(&file) {
            Ok(0) => {}
            Ok(n) => {
                busy = true;
                let want = (n as usize).min(chunk.len());
                match file.read(&mut chunk[..want]) {
                    Ok(0) => break,
                    Ok(got) => {
                        for f in framer.push(&chunk[..got]) {
                            if !feeder.frame(f) {
                                break 'run;
                            }
                        }
                        // this thread is the pipe's writer too: its own answers go out here
                        for line in feeder.take_backlog() {
                            if file.write_all(line.as_bytes()).is_err() {
                                break 'run;
                            }
                        }
                    }
                    Err(e) if is_eof(&e) => break,
                    Err(_) => break,
                }
            }
            Err(e) if is_eof(&e) => break,
            Err(_) => break,
        }
        if busy {
            idle_ms = 1;
        } else {
            std::thread::sleep(Duration::from_millis(idle_ms));
            idle_ms = (idle_ms * 2).min(10);
        }
    }
    feeder.closed();
}
