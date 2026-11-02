// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Derived in part from fh2-mods (https://github.com/conner-mcvay/fh2-mods), MIT, Copyright (c) 2026 conner-mcvay and
// contributors; see licenses/fh2-mods-LICENSE.txt.
//! NerdCore's bit stream (Engine\NetV2\NerdCore BitStream): values are packed LSB first, one
//! after another, with no alignment between them; a packet is padded with zero bits to a byte
//! at the end only.
//!
//! - Plain fields: n bits (BitStream::WriteBits, 14176B380).
//! - Compact uint (14176AA80): a 2-bit size s, then 8*s+6 bits (so at most 30 bits).
//! - String (14176A900): a compact length, then 8-bit chars.
//!
//! Every read is bounds-checked: packets come from the network, and the game's messages carry no
//! length, so a truncated or hostile packet must end the parse, not the server.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitError {
    /// The read runs past the end of the data.
    Short,
    /// A length (string, payload) is over the field's limit.
    TooLong,
}

impl fmt::Display for BitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitError::Short => f.write_str("truncated"),
            BitError::TooLong => f.write_str("length over the limit"),
        }
    }
}

pub type BitResult<T> = Result<T, BitError>;

fn mask(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// A bit string being written (a message, a payload, a packet body).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Bits {
    buf: Vec<u8>,
    len: usize,
}

impl fmt::Debug for Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bits({} bits: ", self.len)?;
        for b in &self.buf {
            write!(f, "{b:02x}")?;
        }
        f.write_str(")")
    }
}

impl Bits {
    pub fn new() -> Self {
        Self::default()
    }

    /// Length in bits.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes, the last one zero-padded.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// The low `n` bits of `v` (n <= 64); higher bits of `v` are ignored.
    pub fn push(&mut self, v: u64, n: u32) -> &mut Self {
        debug_assert!(n <= 64);
        let mut v = v & mask(n);
        let mut n = n as usize;
        while n > 0 {
            let off = self.len & 7;
            if off == 0 {
                self.buf.push(0);
            }
            let k = (8 - off).min(n);
            let last = self.buf.len() - 1;
            self.buf[last] |= ((v & mask(k as u32)) << off) as u8;
            v >>= k;
            n -= k;
            self.len += k;
        }
        self
    }

    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.push(b as u64, 1)
    }

    /// Compact uint (14176AA80). Values of 2^30 and over do not fit; they are truncated to 30
    /// bits, as the game's writer does.
    pub fn cuint(&mut self, v: u32) -> &mut Self {
        let s: u32 = if v & 0xFFC0_0000 != 0 {
            3
        } else if v & 0x00FF_C000 != 0 {
            2
        } else if v & 0xFFC0 != 0 {
            1
        } else {
            0
        };
        self.push(s as u64, 2).push(v as u64, 8 * s + 6)
    }

    /// A string (14176A900): compact length, then 8-bit chars.
    pub fn string(&mut self, s: &[u8]) -> &mut Self {
        self.cuint(s.len() as u32);
        for &c in s {
            self.push(c as u64, 8);
        }
        self
    }

    /// One bit, by index.
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.buf[i >> 3] >> (i & 7)) & 1 == 1
    }

    /// `other`'s bits after ours (messages follow each other with nothing in between).
    pub fn append(&mut self, other: &Bits) -> &mut Self {
        let mut r = Reader::new(&other.buf);
        let mut left = other.len;
        while left > 0 {
            let k = left.min(64) as u32;
            let v = r.bits(k).expect("in bounds");
            self.push(v, k);
            left -= k as usize;
        }
        self
    }
}

/// A bounds-checked reader over a byte slice, from a bit position.
#[derive(Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            end: data.len() * 8,
        }
    }

    /// From bit `pos` (the first message is at bit 88, after the header).
    pub fn at(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos: pos.min(data.len() * 8),
            end: data.len() * 8,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Bits left (including the zero padding of the last byte).
    pub fn left(&self) -> usize {
        self.end - self.pos
    }

    /// `n` bits (n <= 64).
    pub fn bits(&mut self, n: u32) -> BitResult<u64> {
        debug_assert!(n <= 64);
        let n = n as usize;
        if n > self.left() {
            return Err(BitError::Short);
        }
        let mut v = 0u64;
        let mut got = 0usize;
        while got < n {
            let p = self.pos + got;
            let off = p & 7;
            let k = (8 - off).min(n - got);
            let byte = (self.data[p >> 3] >> off) as u64 & mask(k as u32);
            v |= byte << got;
            got += k;
        }
        self.pos += n;
        Ok(v)
    }

    pub fn u8(&mut self, n: u32) -> BitResult<u8> {
        debug_assert!(n <= 8);
        Ok(self.bits(n)? as u8)
    }

    pub fn u32(&mut self, n: u32) -> BitResult<u32> {
        debug_assert!(n <= 32);
        Ok(self.bits(n)? as u32)
    }

    pub fn bool(&mut self) -> BitResult<bool> {
        Ok(self.bits(1)? == 1)
    }

    pub fn cuint(&mut self) -> BitResult<u32> {
        let s = self.bits(2)? as u32;
        self.u32(8 * s + 6)
    }

    /// A string of at most `max` chars (the game's limits: version 38, game 16).
    pub fn string(&mut self, max: usize) -> BitResult<Vec<u8>> {
        let n = self.cuint()? as usize;
        if n > max {
            return Err(BitError::TooLong);
        }
        if n * 8 > self.left() {
            return Err(BitError::Short);
        }
        (0..n).map(|_| self.u8(8)).collect()
    }

    /// The next `n` bits, copied out as they are.
    pub fn take(&mut self, n: usize) -> BitResult<Bits> {
        if n > self.left() {
            return Err(BitError::Short);
        }
        let mut out = Bits::new();
        let mut left = n;
        while left > 0 {
            let k = left.min(64) as u32;
            out.push(self.bits(k)?, k);
            left -= k as usize;
        }
        Ok(out)
    }

    /// The next `n` bits without consuming them.
    pub fn peek(&self, n: u32) -> BitResult<u64> {
        self.clone().bits(n)
    }
}
