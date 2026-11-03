// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Line framing (contract §3.2): UTF-8 JSON, one object per `\n`-terminated line, at most 1 MiB.
//! A longer line is discarded up to its newline and reported as `TooLong`; nothing closes the
//! channel but EOF.

use std::io::{self, Read};

pub const LINE_MAX: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Line(Vec<u8>),
    TooLong,
}

/// Splits bytes into frames; feed it whatever the transport read.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
    discarding: bool,
}

impl Framer {
    pub fn new() -> Framer {
        Framer::default()
    }

    /// Adds bytes and returns the complete frames they finish.
    pub fn push(&mut self, mut data: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        while !data.is_empty() {
            match data.iter().position(|b| *b == b'\n') {
                Some(nl) => {
                    let (part, rest) = (&data[..nl], &data[nl + 1..]);
                    if self.discarding || self.buf.len() + part.len() > LINE_MAX {
                        out.push(Frame::TooLong);
                    } else {
                        self.buf.extend_from_slice(part);
                        out.push(Frame::Line(std::mem::take(&mut self.buf)));
                    }
                    self.buf.clear();
                    self.discarding = false;
                    data = rest;
                }
                None => {
                    if !self.discarding {
                        if self.buf.len() + data.len() > LINE_MAX {
                            self.discarding = true;
                            self.buf = Vec::new();
                        } else {
                            self.buf.extend_from_slice(data);
                        }
                    }
                    data = &[];
                }
            }
        }
        out
    }
}

/// Reads frames from a blocking reader until EOF or an error; `on_frame` returning false stops.
pub fn read_frames(mut r: impl Read, mut on_frame: impl FnMut(Frame) -> bool) -> io::Result<()> {
    let mut framer = Framer::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for f in framer.push(&chunk[..n]) {
            if !on_frame(f) {
                return Ok(());
            }
        }
    }
}
