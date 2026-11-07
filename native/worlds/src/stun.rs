// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! STUN framing (RFC 8489, RFC 5389 wire format) as the side channels use it (contract §5.3):
//! the 20-byte header, the attribute walk and the private indications the hook's car state,
//! voice and roster ride in. No I/O, no authentication, no Binding: an official world runs no
//! STUN to a host and no NAT discovery (WORLDS_OFFICIAL.md §6). Also the endpoint canonical form
//! every module compares with (an IPv4-mapped IPv6 address is the IPv4 one).

use std::net::SocketAddr;

/// The magic cookie (RFC 8489 §5).
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// The 96-bit transaction ID.
pub type TxId = [u8; 12];

/// Bytes before the first attribute.
pub const HEADER_LEN: usize = 20;
/// The largest datagram taken as STUN: RFC 8489 §6.1's 548 bytes for IPv4 paths without a known
/// MTU; anything longer is a game datagram, never STUN.
pub const MAX_LEN: usize = 548;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// An attribute's space on the wire: header plus value padded to 32 bits.
fn padded(value_len: usize) -> usize {
    4 + value_len.next_multiple_of(4)
}

/// The shape check: a 32-bit aligned datagram of 20..=548 bytes whose two top bits are zero,
/// whose header carries the magic cookie, and whose length field covers exactly the rest.
pub fn is_framed(data: &[u8]) -> bool {
    let size = data.len();
    (HEADER_LEN..=MAX_LEN).contains(&size)
        && size.is_multiple_of(4)
        && data[0] & 0b1100_0000 == 0
        && data[4..8] == MAGIC_COOKIE.to_be_bytes()
        && usize::from(u16_at(data, 2)) == size - HEADER_LEN
}

/// The attributes of a message that passed [`is_framed`], in order. An attribute whose declared
/// length runs past the datagram yields one `Err` and ends the walk.
struct Attributes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Result<(u16, &'a [u8]), ()>;

    fn next(&mut self) -> Option<Self::Item> {
        let left = self.data.len().checked_sub(self.at)?;
        if left < 4 {
            return None;
        }
        let tag = u16_at(self.data, self.at);
        let len = usize::from(u16_at(self.data, self.at + 2));
        if len > left - 4 {
            self.at = self.data.len();
            return Some(Err(()));
        }
        let start = self.at + 4;
        self.at += padded(len);
        Some(Ok((tag, &self.data[start..start + len])))
    }
}

fn attributes(data: &[u8]) -> Attributes<'_> {
    Attributes {
        data,
        at: HEADER_LEN,
    }
}

/// The value of the first `attribute` of a STUN message of type `kind`; `None` when the message
/// is not that, lacks it, or an attribute before it is truncated.
pub fn attribute(data: &[u8], kind: u16, attribute: u16) -> Option<&[u8]> {
    if !is_framed(data) || u16_at(data, 0) != kind {
        return None;
    }
    for item in attributes(data) {
        match item {
            Ok((tag, value)) if tag == attribute => return Some(value),
            Ok(_) => {}
            Err(()) => return None,
        }
    }
    None
}

/// Like [`attribute`], but the whole message must walk: an attribute running past the end
/// refuses the message even when it comes after the one asked for (the side channels' rule).
pub fn attribute_strict(data: &[u8], kind: u16, wanted: u16) -> Option<&[u8]> {
    if !is_framed(data) || u16_at(data, 0) != kind {
        return None;
    }
    let mut first = None;
    for item in attributes(data) {
        let (tag, value) = item.ok()?;
        if tag == wanted && first.is_none() {
            first = Some(value);
        }
    }
    first
}

/// A message of `kind` with these attributes in order, each padded with zeros to 32 bits.
pub fn indication(kind: u16, txid: &TxId, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let body: usize = attrs.iter().map(|(_, v)| padded(v.len())).sum();
    let mut out = vec![0u8; HEADER_LEN + body];
    out[0..2].copy_from_slice(&kind.to_be_bytes());
    out[2..4].copy_from_slice(&(body as u16).to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..HEADER_LEN].copy_from_slice(txid);
    let mut at = HEADER_LEN;
    for (tag, value) in attrs {
        out[at..at + 2].copy_from_slice(&tag.to_be_bytes());
        out[at + 2..at + 4].copy_from_slice(&(value.len() as u16).to_be_bytes());
        out[at + 4..at + 4 + value.len()].copy_from_slice(value);
        at += padded(value.len());
    }
    out
}

/// A fresh random transaction ID (RFC 8489 §5: uniformly random). Should the OS source fail,
/// a time-derived value still differs between calls.
pub fn random_txid() -> TxId {
    let mut t: TxId = [0; 12];
    if getrandom::fill(&mut t).is_err() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SPARE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let bump = SPARE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        t[..8].copy_from_slice(&(nanos ^ bump).to_le_bytes());
        t[8..].copy_from_slice(&(bump as u32).to_le_bytes());
    }
    t
}

/// The comparable form of an endpoint: `[::ffff:a.b.c.d]:p` becomes `a.b.c.d:p`.
pub fn plain_endpoint(a: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = a
        && let Some(plain) = v6.ip().to_ipv4_mapped()
    {
        return SocketAddr::from((plain, v6.port()));
    }
    a
}

/// The destination form for a socket: a dual-stack (IPv6) socket reaches an IPv4 target through
/// its IPv4-mapped address; an IPv4 socket takes it as it is.
pub fn endpoint_for_socket(a: SocketAddr, socket_is_v6: bool) -> SocketAddr {
    if let (true, SocketAddr::V4(v4)) = (socket_is_v6, a) {
        return SocketAddr::from((v4.ip().to_ipv6_mapped(), v4.port()));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }

    /// RFC 5769 §2.1, the sample request: SOFTWARE, PRIORITY, ICE-CONTROLLED, USERNAME,
    /// MESSAGE-INTEGRITY and FINGERPRINT, walked whole (the side channels' attribute walk on a
    /// real message).
    #[test]
    fn rfc5769_sample_request_walks_whole() {
        let m = hex("000100582112a442b7e7a701bc34d686fa87dfae
             802200105354554e207465737420636c69656e74
             002400046e0001ff
             80290008932ff9b151263b36
             000600096576746a3a68367659202020
             000800149aeaa70cbfd8cb56781ef2b5b2d3f249c1b571a2
             80280004e57a3bcf");
        assert_eq!(m.len(), 108);
        assert!(is_framed(&m));
        assert_eq!(attribute(&m, 0x0001, 0x0006), Some(&b"evtj:h6vY"[..]));
        assert_eq!(
            attribute_strict(&m, 0x0001, 0x0006),
            Some(&b"evtj:h6vY"[..])
        );
        assert_eq!(attribute(&m, 0x0101, 0x0006), None, "another message type");
        assert_eq!(attribute(&m, 0x0001, 0x7fff), None, "no such attribute");
    }

    /// RFC 5769 §2.2, the sample IPv4 response: its attributes walk, the address value as sent.
    #[test]
    fn rfc5769_sample_ipv4_response() {
        let m = hex("0101003c2112a442b7e7a701bc34d686fa87dfae
             8022000b7465737420766563746f7220
             002000080001a147e112a643
             000800142b91f599fd9e90c38c7489f92af9ba53f06be7d7
             80280004c07d4c96");
        assert!(is_framed(&m));
        assert_eq!(
            attribute(&m, 0x0101, 0x0020),
            Some(&hex("0001a147e112a643")[..])
        );
        // SOFTWARE is 11 bytes, padded to 12 on the wire: the walk reads past the padding
        assert_eq!(attribute(&m, 0x0101, 0x8022), Some(&b"test vector"[..]));
    }

    #[test]
    fn shape_limits() {
        let t = random_txid();
        assert_ne!(t, random_txid());
        let big = indication(0x3e99, &t, &[(0xc0ff, &[0u8; MAX_LEN - 24])]);
        assert_eq!(big.len(), MAX_LEN);
        assert!(is_framed(&big));
        let over = indication(0x3e99, &t, &[(0xc0ff, &[0u8; MAX_LEN - 20])]);
        assert!(!is_framed(&over));
        let mut top = indication(0x0001, &t, &[]);
        assert!(is_framed(&top));
        top[0] = 0x40;
        assert!(!is_framed(&top));
        let mut cookie = indication(0x0001, &t, &[]);
        cookie[4] ^= 1;
        assert!(!is_framed(&cookie));
        let mut long = indication(0x0001, &t, &[]);
        long.push(0);
        assert!(!is_framed(&long));
        // an odd value is padded on the wire, and the next attribute is read after the padding
        let m = indication(0x3e99, &t, &[(0xc001, &[1, 2, 3][..]), (0xc002, &[9][..])]);
        assert_eq!(m.len(), 36);
        assert_eq!(attribute(&m, 0x3e99, 0xc002), Some(&[9u8][..]));
    }

    #[test]
    fn a_truncated_attribute_ends_the_walk() {
        let t = random_txid();
        let mut m = indication(0x3e99, &t, &[(0xc001, &[1, 2, 3, 4][..])]);
        // a second attribute declaring 64 bytes it does not have
        m.extend_from_slice(&[0xc0, 0x02, 0x00, 0x40]);
        let body = (m.len() - HEADER_LEN) as u16;
        m[2..4].copy_from_slice(&body.to_be_bytes());
        assert!(is_framed(&m));
        assert_eq!(attribute(&m, 0x3e99, 0xc001), Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(attribute(&m, 0x3e99, 0xc002), None);
        assert_eq!(
            attribute_strict(&m, 0x3e99, 0xc001),
            None,
            "the strict walk refuses the whole message"
        );
        // garbage of every length never panics
        for n in 0..64 {
            let junk: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let _ = attribute_strict(&junk, 0x3e99, 0xc001);
        }
    }

    #[test]
    fn canonical_and_socket_forms() {
        let v6: SocketAddr = "[2001:db8::1]:5".parse().unwrap();
        assert_eq!(plain_endpoint(v6), v6);
        assert_eq!(endpoint_for_socket(v6, false), v6);
        assert_eq!(endpoint_for_socket(v6, true), v6);
        let v4: SocketAddr = "192.0.2.8:7".parse().unwrap();
        assert_eq!(plain_endpoint(endpoint_for_socket(v4, true)), v4);
    }
}
