// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The engine's owned bit buffer and its reader: NerdCore's LSB-first bit order (NERDCORE_WIRE.md
//! §1), values up to 64 bits per call, as the engine builds and slices messages. The codec's
//! `BitReader`/`BitWriter` stop at 32 bits per call and validate spec ranges; the engine needs raw
//! 64-bit pushes (XUIDs, attempts) and exact slices, so it keeps this small pair.

/// Reading past the end (the only way a read fails).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncated;

pub type Rr<T> = Result<T, Truncated>;

/// An owned run of bits; bits past `len` in the last byte are always zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    pub fn new() -> Bits {
        Bits::default()
    }

    /// `len` bits of `bytes` from bit 0 (`bytes` must hold them).
    pub fn from_bytes(bytes: &[u8], len: usize) -> Rr<Bits> {
        Rd::with_len(bytes, len)?.take(len)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `ceil(len / 8)` bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn reader(&self) -> Rd<'_> {
        Rd {
            bytes: &self.bytes,
            end: self.len,
            pos: 0,
        }
    }

    /// Appends the low `width` bits of `value` (`width` ≤ 64).
    pub fn push(&mut self, mut value: u64, mut width: u32) -> &mut Bits {
        debug_assert!(width <= 64);
        let width_mask = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        value &= width_mask;
        self.bytes
            .resize((self.len + width as usize).div_ceil(8), 0);
        while width > 0 {
            let offset = (self.len & 7) as u32;
            let count = (8 - offset).min(width);
            let part = (value & ((1u64 << count) - 1)) as u8;
            self.bytes[self.len >> 3] |= part << offset;
            value >>= count;
            self.len += count as usize;
            width -= count;
        }
        self
    }

    pub fn boolean(&mut self, b: bool) -> &mut Bits {
        self.push(u64::from(b), 1)
    }

    /// The compact u32: a 2-bit size, then `8 × size + 6` bits (§1.6).
    pub fn compact_uint(&mut self, value: u32) -> &mut Bits {
        let size: u32 = if value & 0xffc0_0000 != 0 {
            3
        } else if value & 0x00ff_c000 != 0 {
            2
        } else if value & 0xffc0 != 0 {
            1
        } else {
            0
        };
        self.push(u64::from(size), 2)
            .push(u64::from(value), 8 * size + 6)
    }

    /// A compact-length byte string.
    pub fn byte_string(&mut self, s: &[u8]) -> &mut Bits {
        self.compact_uint(s.len() as u32);
        for b in s {
            self.push(u64::from(*b), 8);
        }
        self
    }

    pub fn append(&mut self, other: &Bits) -> &mut Bits {
        let mut r = other.reader();
        while r.left() > 0 {
            let n = r.left().min(64) as u32;
            let v = r.read(n).unwrap_or(0);
            self.push(v, n);
        }
        self
    }
}

/// A bounds-checked reader (a cheap copy: `let probe = reader;` forks it).
#[derive(Debug, Clone, Copy)]
pub struct Rd<'a> {
    bytes: &'a [u8],
    end: usize,
    pos: usize,
}

impl<'a> Rd<'a> {
    pub fn new(bytes: &'a [u8]) -> Rd<'a> {
        Rd {
            bytes,
            end: bytes.len() * 8,
            pos: 0,
        }
    }

    pub fn with_len(bytes: &'a [u8], len: usize) -> Rr<Rd<'a>> {
        if len > bytes.len() * 8 {
            return Err(Truncated);
        }
        Ok(Rd {
            bytes,
            end: len,
            pos: 0,
        })
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn left(&self) -> usize {
        self.end - self.pos
    }

    pub fn read(&mut self, width: u32) -> Rr<u64> {
        let w = width as usize;
        if width > 64 || w > self.left() {
            return Err(Truncated);
        }
        let mut value = 0u64;
        let mut got = 0usize;
        while got < w {
            let at = self.pos + got;
            let offset = at & 7;
            let count = (8 - offset).min(w - got);
            let part = (u64::from(self.bytes[at >> 3]) >> offset) & ((1u64 << count) - 1);
            value |= part << got;
            got += count;
        }
        self.pos += w;
        Ok(value)
    }

    pub fn boolean(&mut self) -> Rr<bool> {
        Ok(self.read(1)? != 0)
    }

    pub fn compact_uint(&mut self) -> Rr<u32> {
        let size = self.read(2)? as u32;
        Ok(self.read(8 * size + 6)? as u32)
    }

    /// A compact-length byte string of at most `max` bytes (`Err` above it, as the C# length
    /// limit; both end the message).
    pub fn byte_string(&mut self, max: usize) -> Rr<Vec<u8>> {
        let count = self.compact_uint()? as usize;
        if count > max {
            return Err(Truncated);
        }
        self.read_bytes(count)
    }

    pub fn read_bytes(&mut self, count: usize) -> Rr<Vec<u8>> {
        if count > self.left() / 8 {
            return Err(Truncated);
        }
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(self.read(8)? as u8);
        }
        Ok(v)
    }

    pub fn skip(&mut self, width: usize) -> Rr<()> {
        if width > self.left() {
            return Err(Truncated);
        }
        self.pos += width;
        Ok(())
    }

    pub fn take(&mut self, width: usize) -> Rr<Bits> {
        if width > self.left() {
            return Err(Truncated);
        }
        let mut out = Bits::new();
        let mut left = width;
        while left > 0 {
            let n = left.min(64);
            out.push(self.read(n as u32)?, n as u32);
            left -= n;
        }
        Ok(out)
    }
}
