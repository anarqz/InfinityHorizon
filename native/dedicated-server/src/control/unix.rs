// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The Unix stream socket transport (contract §3.1, Linux/VM): the engine listens; the directory
//! is `0700` and the socket `0600`, owned by the engine's user, so no secret is needed. Any number
//! of connections up to `CONNECTIONS_MAX` (one controller session, plus `--health` probes); the
//! engine (`core::Server`) enforces the session rules.

use std::{
    fs,
    io::{self, Write},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

use super::{ControlMsg, Feeder, Transport, WRITER_QUEUE, framing};
use crate::runtime::Hub;

pub const CONNECTIONS_MAX: usize = 8;

/// Why the endpoint could not be created.
#[derive(Debug)]
pub enum ListenError {
    /// The directory exists with group/other access.
    DirectoryMode(u32),
    /// The directory belongs to another user than the engine's.
    DirectoryOwner,
    /// Another engine answers on that socket.
    InUse,
    Io(io::Error),
}

/// Creates the parent directory (`0700`) when missing, refuses one that others can enter,
/// removes a stale socket nobody answers on, binds and sets the socket to `0600`, and refuses a
/// directory another user owns (its owner could replace the socket): the socket just bound
/// belongs to the engine's effective user, so its owner is compared with the directory's.
pub fn listen(path: &Path) -> Result<UnixListener, ListenError> {
    let dir = path.parent().ok_or_else(|| {
        ListenError::Io(io::Error::new(io::ErrorKind::InvalidInput, "no directory"))
    })?;
    match fs::metadata(dir) {
        Ok(m) => {
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(ListenError::DirectoryMode(mode));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(ListenError::Io)?,
        Err(e) => return Err(ListenError::Io(e)),
    }
    if let Ok(m) = fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(ListenError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "not a socket",
            )));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(ListenError::InUse);
        }
        fs::remove_file(path).map_err(ListenError::Io)?;
    }
    let listener = UnixListener::bind(path).map_err(ListenError::Io)?;
    let owners = fs::symlink_metadata(path)
        .and_then(|socket| Ok((socket.uid(), fs::metadata(dir)?.uid())))
        .map_err(ListenError::Io)?;
    if owners.0 != owners.1 {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(ListenError::DirectoryOwner);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(ListenError::Io)?;
    Ok(listener)
}

/// Accepts connections on a thread of its own, each with a reader and a writer thread.
pub fn serve(
    listener: UnixListener,
    hub: crate::runtime::HubTx,
    ids: Arc<AtomicU64>,
) -> io::Result<()> {
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("ds-control-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if open.load(Ordering::Acquire) >= CONNECTIONS_MAX {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                open.fetch_add(1, Ordering::AcqRel);
                let conn = ids.fetch_add(1, Ordering::Relaxed);
                if start_connection(stream, conn, hub.clone(), open.clone()).is_err() {
                    open.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })?;
    Ok(())
}

fn start_connection(
    stream: UnixStream,
    conn: u64,
    hub: crate::runtime::HubTx,
    open: Arc<AtomicUsize>,
) -> io::Result<()> {
    let mut write_half = stream.try_clone()?;
    let closer_half = stream.try_clone()?;
    let (writer, lines) = mpsc::sync_channel::<String>(WRITER_QUEUE);
    let inflight = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("ds-control-write".into())
        .spawn(move || {
            for line in lines {
                if write_half.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            let _ = write_half.shutdown(std::net::Shutdown::Both);
        })?;
    let closer: Box<dyn Fn() + Send> = Box::new(move || {
        let _ = closer_half.shutdown(std::net::Shutdown::Both);
    });
    hub.send(Hub::Control(ControlMsg::Opened {
        conn,
        transport: Transport::Unix,
        writer: writer.clone(),
        inflight: inflight.clone(),
    }))
    .map_err(|_| io::Error::other("loop gone"))?;
    hub.send(Hub::Closer { conn, closer })
        .map_err(|_| io::Error::other("loop gone"))?;
    let feeder = Feeder {
        conn,
        writer,
        inflight,
        hub,
    };
    std::thread::Builder::new()
        .name("ds-control-read".into())
        .spawn(move || {
            let _ = framing::read_frames(&stream, |f| feeder.frame(f));
            feeder.closed();
            open.fetch_sub(1, Ordering::AcqRel);
        })?;
    Ok(())
}
