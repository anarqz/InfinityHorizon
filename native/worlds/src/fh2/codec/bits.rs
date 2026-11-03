// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The NerdCore bit stream (spec §1): one little-endian bit sequence, least significant bit first
//! inside each byte, values written low bits first, at most 32 bits per call.
//!
//! The reader never panics: every read is bounds-checked and returns [`CodecError`]. The writer
//! validates widths and ranges and returns errors instead of asserting.

use super::CodecError;

/// The global string maximum (`MAX_STRING_LENGTH`, §1.6).
pub const MAX_STRING_LENGTH: usize = 255;
/// Counts with either of the top two bits of a u32 set are rejected (§1.6).
pub const COUNT_LIMIT: u32 = 1 << 30;

/// `bits(n)`: the number of bits needed to write `n` (§4.1: `while n: n >>= 1; bits++`).
/// `bits(0) == 0`, `bits(1) == 1`, `bits(2) == 2`, `bits(64) == 7`.
pub const fn bit_length(n: u32) -> u8 {
    (32 - n.leading_zeros()) as u8
}

/// Whether `v` lies in the symmetric range §1.4 recommends for senders:
/// `-(2^(n-1) - 1) ..= 2^(n-1) - 1` (any `i32` when `n == 32`).
pub fn signed_fits_strict(v: i32, n: u8) -> bool {
    if !(2..=32).contains(&n) {
        return false;
    }
    n == 32 || i64::from(v).abs() < (1i64 << (n - 1))
}

fn mask(n: u8) -> u32 {
    if n >= 32 { u32::MAX } else { (1u32 << n) - 1 }
}

/// An owned run of bits (for example an object payload, an unparsed packet tail or the voice
/// trailer, which can start at any bit offset). Bits past `len` in the last byte are zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitRun {
    bytes: Vec<u8>,
    len: usize,
}

impl BitRun {
    /// An empty run.
    pub fn new() -> BitRun {
        BitRun::default()
    }

    /// `len` bits of `bytes`, read from bit 0. `Err` when `bytes` is too short; bits past `len`
    /// are cleared.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Result<BitRun, CodecError> {
        let mut r = BitReader::new(bytes);
        r.read_run(len)
    }

    /// The run's length, in bits.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bits packed from bit 0, `ceil(len / 8)` bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// A reader over exactly these bits.
    pub fn reader(&self) -> BitReader<'_> {
        BitReader::with_len(&self.bytes, self.len)
    }
}

/// Bounds-checked reader over a byte slice (§1.2, §1.3).
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    len: usize,
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader over every bit of `data`.
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader {
            data,
            len: data.len().saturating_mul(8),
            pos: 0,
        }
    }

    /// A reader over the first `len_bits` bits of `data` (clamped to the data).
    pub fn with_len(data: &'a [u8], len_bits: usize) -> BitReader<'a> {
        BitReader {
            data,
            len: len_bits.min(data.len().saturating_mul(8)),
            pos: 0,
        }
    }

    /// Current position in bits.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Total readable bits.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits left.
    pub fn remaining(&self) -> usize {
        self.len - self.pos
    }

    /// Moves back (or forward) to a saved position (`FUN_141769290`, §1.1). `Err` past the end.
    pub fn seek(&mut self, pos: usize) -> Result<(), CodecError> {
        if pos > self.len {
            return Err(CodecError::Truncated);
        }
        self.pos = pos;
        Ok(())
    }

    /// Advances `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        if n > self.remaining() {
            return Err(CodecError::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    /// An unsigned field of `n` bits, `1 <= n <= 32` (`FUN_141767940`).
    pub fn read(&mut self, n: u8) -> Result<u32, CodecError> {
        if n == 0 || n > 32 {
            return Err(CodecError::Width(n));
        }
        let n_us = usize::from(n);
        if n_us > self.remaining() {
            return Err(CodecError::Truncated);
        }
        // LSB first: each step takes what is left of the current byte, up to the bits still due
        let mut value: u64 = 0;
        let mut filled = 0usize;
        while filled < n_us {
            let bit = self.pos + filled;
            let offset = bit % 8;
            let width = (8 - offset).min(n_us - filled);
            let chunk = u64::from(self.data[bit / 8] >> offset) & ((1u64 << width) - 1);
            value |= chunk << filled;
            filled += width;
        }
        self.pos += n_us;
        Ok(value as u32)
    }

    /// One bit (`FUN_141767100`).
    pub fn read_bool(&mut self) -> Result<bool, CodecError> {
        Ok(self.read(1)? == 1)
    }

    /// A two's complement field of `n` bits, `2 <= n <= 32`, sign-extended when `n < 32` (§1.4).
    pub fn read_signed(&mut self, n: u8) -> Result<i32, CodecError> {
        if !(2..=32).contains(&n) {
            return Err(CodecError::Width(n));
        }
        let v = self.read(n)?;
        if n < 32 && v & (1 << (n - 1)) != 0 {
            Ok((v | !mask(n)) as i32)
        } else {
            Ok(v as i32)
        }
    }

    /// A raw IEEE-754 single (§1.5), returned as its bit pattern.
    pub fn read_f32_bits(&mut self) -> Result<u32, CodecError> {
        self.read(32)
    }

    /// A 64-bit value as 8 bytes, least significant byte first, each an 8-bit field (§5).
    pub fn read_u64(&mut self) -> Result<u64, CodecError> {
        if self.remaining() < 64 {
            return Err(CodecError::Truncated);
        }
        let mut v = 0u64;
        for i in 0..8 {
            v |= u64::from(self.read(8)?) << (8 * i);
        }
        Ok(v)
    }

    /// The variable-length count (§1.6): a 2-bit size class `k`, then `8k + 6` bits.
    pub fn read_count(&mut self) -> Result<u32, CodecError> {
        let start = self.pos;
        let k = self.read(2)? as u8;
        match self.read(8 * k + 6) {
            Ok(c) => Ok(c),
            Err(e) => {
                self.pos = start;
                Err(e)
            }
        }
    }

    /// A byte run (§1.6) of at most `max` bytes. A longer count rewinds to the start of the
    /// count and fails, like the game's reader.
    pub fn read_byte_run(&mut self, max: usize) -> Result<Vec<u8>, CodecError> {
        let start = self.pos;
        let count = self.read_count()? as usize;
        if count > max {
            self.pos = start;
            return Err(CodecError::TooLong { len: count, max });
        }
        if count.saturating_mul(8) > self.remaining() {
            self.pos = start;
            return Err(CodecError::Truncated);
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.read(8)? as u8);
        }
        Ok(out)
    }

    /// A string (§1.6): a byte run with no terminator, at most `max` bytes (`max <= 255`).
    pub fn read_string(&mut self, max: usize) -> Result<Vec<u8>, CodecError> {
        self.read_byte_run(max.min(MAX_STRING_LENGTH))
    }

    /// The next `n` bits as an owned run.
    pub fn read_run(&mut self, n: usize) -> Result<BitRun, CodecError> {
        if n > self.remaining() {
            return Err(CodecError::Truncated);
        }
        let mut w = BitWriter::new();
        let mut left = n;
        while left > 0 {
            let take = left.min(32) as u8;
            let v = self.read(take)?;
            w.write(v, take)?;
            left -= usize::from(take);
        }
        Ok(w.into_run())
    }
}

/// Bit writer (§1.2, §1.3). Fails instead of panicking on a bad width or an out-of-range value.
#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    len: usize,
}

impl BitWriter {
    pub fn new() -> BitWriter {
        BitWriter::default()
    }

    /// Bits written.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes written so far; unused bits of the last byte are zero.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn into_run(self) -> BitRun {
        BitRun {
            bytes: self.bytes,
            len: self.len,
        }
    }

    /// Drops everything after bit `len` (to undo a message that did not fit).
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        self.len = len;
        self.bytes.truncate(len.div_ceil(8));
        if len & 7 != 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last &= (1u8 << (len & 7)) - 1;
        }
    }

    fn put_bits(&mut self, v: u32, n: u8) {
        let mut left = usize::from(n);
        let mut v = u64::from(v);
        while left > 0 {
            let shift = self.len & 7;
            if shift == 0 {
                self.bytes.push(0);
            }
            let take = (8 - shift).min(left);
            let part = (v & ((1u64 << take) - 1)) as u8;
            if let Some(last) = self.bytes.last_mut() {
                *last |= part << shift;
            }
            v >>= take;
            left -= take;
            self.len += take;
        }
    }

    /// An unsigned field of `n` bits, `1 <= n <= 32`; `v` must fit in `n` bits.
    pub fn write(&mut self, v: u32, n: u8) -> Result<(), CodecError> {
        if n == 0 || n > 32 {
            return Err(CodecError::Width(n));
        }
        if v & !mask(n) != 0 {
            return Err(CodecError::Range);
        }
        self.put_bits(v, n);
        Ok(())
    }

    pub fn write_bool(&mut self, b: bool) -> Result<(), CodecError> {
        self.write(u32::from(b), 1)
    }

    /// A two's complement field of `n` bits, `2 <= n <= 32` (§1.4). Any value that `n` bits of
    /// two's complement hold (`-2^(n-1) ..= 2^(n-1) - 1`) is accepted, so that a decoded value
    /// always re-encodes to the same bits; a server's own values should stay inside the
    /// symmetric `-(2^(n-1) - 1) ..= 2^(n-1) - 1` that §1.4 recommends ([`signed_fits_strict`]).
    pub fn write_signed(&mut self, v: i32, n: u8) -> Result<(), CodecError> {
        if !(2..=32).contains(&n) {
            return Err(CodecError::Width(n));
        }
        if n < 32 {
            let half = 1i64 << (n - 1);
            if !(-half..half).contains(&i64::from(v)) {
                return Err(CodecError::Range);
            }
        }
        self.put_bits((v as u32) & mask(n), n);
        Ok(())
    }

    /// A raw IEEE-754 single given as its bit pattern (§1.5).
    pub fn write_f32_bits(&mut self, bits: u32) -> Result<(), CodecError> {
        self.write(bits, 32)
    }

    /// 8 bytes, least significant first (§5).
    pub fn write_u64(&mut self, v: u64) -> Result<(), CodecError> {
        for i in 0..8 {
            self.write(((v >> (8 * i)) & 0xff) as u32, 8)?;
        }
        Ok(())
    }

    /// The variable-length count (§1.6), smallest class; `count >= 2^30` is rejected.
    pub fn write_count(&mut self, count: u32) -> Result<(), CodecError> {
        let k: u8 = match count {
            c if c < 1 << 6 => 0,
            c if c < 1 << 14 => 1,
            c if c < 1 << 22 => 2,
            c if c < COUNT_LIMIT => 3,
            _ => return Err(CodecError::Range),
        };
        self.write(u32::from(k), 2)?;
        self.write(count, 8 * k + 6)
    }

    /// A byte run (§1.6) of at most `max` bytes.
    pub fn write_byte_run(&mut self, bytes: &[u8], max: usize) -> Result<(), CodecError> {
        if bytes.len() > max {
            return Err(CodecError::TooLong {
                len: bytes.len(),
                max,
            });
        }
        let count = u32::try_from(bytes.len()).map_err(|_| CodecError::Range)?;
        self.write_count(count)?;
        for b in bytes {
            self.write(u32::from(*b), 8)?;
        }
        Ok(())
    }

    /// A string (§1.6) of at most `max` bytes (`max <= 255`), without terminator.
    pub fn write_string(&mut self, s: &[u8], max: usize) -> Result<(), CodecError> {
        self.write_byte_run(s, max.min(MAX_STRING_LENGTH))
    }

    /// Appends a run of bits as-is.
    pub fn write_run(&mut self, run: &BitRun) -> Result<(), CodecError> {
        let mut r = run.reader();
        while r.remaining() > 0 {
            let take = r.remaining().min(32) as u8;
            let v = r.read(take)?;
            self.put_bits(v, take);
        }
        Ok(())
    }

    /// Overwrites `n` bits at bit `at` (the header patch at packet close, §2.2).
    pub fn patch(&mut self, at: usize, v: u32, n: u8) -> Result<(), CodecError> {
        if n == 0 || n > 32 {
            return Err(CodecError::Width(n));
        }
        if v & !mask(n) != 0 {
            return Err(CodecError::Range);
        }
        if at + usize::from(n) > self.len {
            return Err(CodecError::Truncated);
        }
        for i in 0..usize::from(n) {
            let k = at + i;
            let bit = 1u8 << (k & 7);
            if (v >> i) & 1 == 1 {
                self.bytes[k >> 3] |= bit;
            } else {
                self.bytes[k >> 3] &= !bit;
            }
        }
        Ok(())
    }
}
