// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! STUN framing (RFC 8489, RFC 5389 wire format) for the world socket: the 20-byte header, the
//! attribute walk, Binding request/success with XOR-MAPPED-ADDRESS, and the private indications
//! the side channels ride in (contract §5.3). No I/O, no authentication: MESSAGE-INTEGRITY and
//! FINGERPRINT are carried past, never checked. Also the endpoint canonical form every module
//! compares with (an IPv4-mapped IPv6 address is the IPv4 one).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The magic cookie (RFC 8489 §5).
pub const MAGIC: u32 = 0x2112_A442;
/// Binding request (method 0x001, class request).
pub const BINDING_REQUEST: u16 = 0x0001;
/// Binding success response (method 0x001, class success).
pub const BINDING_SUCCESS: u16 = 0x0101;
/// The 96-bit transaction ID.
pub type TxId = [u8; 12];

/// Bytes before the first attribute.
pub const HEADER_LEN: usize = 20;
/// The largest datagram taken as STUN: RFC 8489 §6.1's 548 bytes for IPv4 paths without a known
/// MTU; anything longer is a game datagram, never STUN.
pub const MAX_LEN: usize = 548;

/// MAPPED-ADDRESS (RFC 8489 §14.1), read only when no XOR form is present.
const ATTR_MAPPED: u16 = 0x0001;
/// XOR-MAPPED-ADDRESS (RFC 8489 §14.2).
const ATTR_XOR_MAPPED: u16 = 0x0020;
/// The pre-RFC 5389 code point some servers still send for XOR-MAPPED-ADDRESS.
const ATTR_XOR_MAPPED_LEGACY: u16 = 0x8020;
const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;

/// What the world reads from a STUN message: its type, its transaction, and the address a
/// (XOR-)MAPPED-ADDRESS reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub kind: u16,
    pub txid: TxId,
    pub mapped: Option<SocketAddr>,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// An attribute's space on the wire: header plus value padded to 32 bits.
fn padded(value_len: usize) -> usize {
    4 + value_len.next_multiple_of(4)
}

/// The shape check: a 32-bit aligned datagram of 20..=548 bytes whose two top bits are zero,
/// whose header carries the magic cookie, and whose length field covers exactly the rest.
pub fn is_stun(data: &[u8]) -> bool {
    let size = data.len();
    (HEADER_LEN..=MAX_LEN).contains(&size)
        && size.is_multiple_of(4)
        && data[0] & 0b1100_0000 == 0
        && data[4..8] == MAGIC.to_be_bytes()
        && usize::from(u16_at(data, 2)) == size - HEADER_LEN
}

/// The attributes of a message that passed [`is_stun`], in order. An attribute whose declared
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
    if !is_stun(data) || u16_at(data, 0) != kind {
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

/// The cookie followed by the transaction: the XOR key of the address forms (RFC 8489 §14.2).
fn xor_key(txid: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..4].copy_from_slice(&MAGIC.to_be_bytes());
    key[4..].copy_from_slice(txid);
    key
}

/// A (XOR-)MAPPED-ADDRESS value: `0, family, port, address`. `key` undoes the XOR form.
fn decode_address(value: &[u8], key: Option<&[u8; 16]>) -> Option<SocketAddr> {
    if value.len() < 8 {
        return None;
    }
    let width = match value[1] {
        FAMILY_V4 => 4,
        FAMILY_V6 if value.len() >= 20 => 16,
        _ => return None,
    };
    let mut raw = [0u8; 16];
    raw[..width].copy_from_slice(&value[4..4 + width]);
    let mut port = u16_at(value, 2);
    if let Some(k) = key {
        port ^= u16_at(k, 0);
        for (b, m) in raw.iter_mut().zip(k.iter()).take(width) {
            *b ^= m;
        }
    }
    let ip = if width == 4 {
        IpAddr::V4(Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]))
    } else {
        IpAddr::V6(Ipv6Addr::from(raw))
    };
    Some(SocketAddr::new(ip, port))
}

/// Reads a message of any type. An XOR-MAPPED-ADDRESS (either code point) wins over a plain
/// MAPPED-ADDRESS; unknown attributes are skipped; a truncated attribute refuses the whole message.
pub fn parse(data: &[u8]) -> Option<Message> {
    let header = data.get(..HEADER_LEN).filter(|_| is_stun(data))?;
    let mut txid: TxId = [0; 12];
    txid.copy_from_slice(&header[8..]);
    let key = xor_key(&txid);
    let mut found = None;
    for item in attributes(data) {
        let (tag, value) = item.ok()?;
        match tag {
            ATTR_XOR_MAPPED | ATTR_XOR_MAPPED_LEGACY => {
                found = decode_address(value, Some(&key)).or(found);
            }
            ATTR_MAPPED if found.is_none() => found = decode_address(value, None),
            _ => {}
        }
    }
    Some(Message {
        kind: u16_at(header, 0),
        txid,
        mapped: found,
    })
}

/// A message of `kind` with these attributes in order, each padded with zeros to 32 bits.
pub fn indication(kind: u16, txid: &TxId, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let body: usize = attrs.iter().map(|(_, v)| padded(v.len())).sum();
    let mut out = vec![0u8; HEADER_LEN + body];
    out[0..2].copy_from_slice(&kind.to_be_bytes());
    out[2..4].copy_from_slice(&(body as u16).to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC.to_be_bytes());
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

/// A Binding request with no attributes.
pub fn binding_request(txid: &TxId) -> [u8; 20] {
    let mut out = [0u8; HEADER_LEN];
    out[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    out[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    out[8..].copy_from_slice(txid);
    out
}

/// The Binding success for `txid` telling the requester the address it was `seen` from (canonical:
/// an IPv4-mapped source is answered with the IPv4 family).
pub fn binding_success(txid: &TxId, seen: SocketAddr) -> Vec<u8> {
    let who = canonical(seen);
    let key = xor_key(txid);
    let (family, ip): (u8, Vec<u8>) = match who.ip() {
        IpAddr::V4(v4) => (FAMILY_V4, v4.octets().to_vec()),
        IpAddr::V6(v6) => (FAMILY_V6, v6.octets().to_vec()),
    };
    let mut value = Vec::with_capacity(4 + ip.len());
    value.push(0);
    value.push(family);
    value.extend_from_slice(&(who.port() ^ u16_at(&key, 0)).to_be_bytes());
    value.extend(ip.iter().zip(key.iter()).map(|(b, m)| b ^ m));
    indication(BINDING_SUCCESS, txid, &[(ATTR_XOR_MAPPED, &value)])
}

/// A fresh random transaction ID (RFC 8489 §5: uniformly random). Should the OS source fail,
/// a time-derived value still differs between calls.
pub fn new_txid() -> TxId {
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
pub fn canonical(a: SocketAddr) -> SocketAddr {
    if let SocketAddr::V6(v6) = a
        && let Some(plain) = v6.ip().to_ipv4_mapped()
    {
        return SocketAddr::from((plain, v6.port()));
    }
    a
}

/// The destination form for a socket: a dual-stack (IPv6) socket reaches an IPv4 target through
/// its IPv4-mapped address; an IPv4 socket takes it as it is.
pub fn for_socket(a: SocketAddr, socket_is_v6: bool) -> SocketAddr {
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

    const RFC5769_TXID: TxId = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// RFC 5769 §2.1, the sample request: SOFTWARE, PRIORITY, ICE-CONTROLLED, USERNAME,
    /// MESSAGE-INTEGRITY and FINGERPRINT; nothing in it is an address.
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
        assert!(is_stun(&m));
        let p = parse(&m).unwrap();
        assert_eq!(
            p,
            Message {
                kind: BINDING_REQUEST,
                txid: RFC5769_TXID,
                mapped: None
            }
        );
        assert_eq!(
            attribute(&m, BINDING_REQUEST, 0x0006),
            Some(&b"evtj:h6vY"[..])
        );
        assert_eq!(attribute(&m, BINDING_SUCCESS, 0x0006), None);
    }

    /// RFC 5769 §2.2, the sample IPv4 response with its SOFTWARE and integrity attributes.
    #[test]
    fn rfc5769_sample_ipv4_response() {
        let m = hex("0101003c2112a442b7e7a701bc34d686fa87dfae
             8022000b7465737420766563746f7220
             002000080001a147e112a643
             000800142b91f599fd9e90c38c7489f92af9ba53f06be7d7
             80280004c07d4c96");
        let p = parse(&m).unwrap();
        assert_eq!(p.kind, BINDING_SUCCESS);
        assert_eq!(p.mapped, Some("192.0.2.1:32853".parse().unwrap()));
    }

    #[test]
    fn xor_form_beats_the_plain_one_in_either_order() {
        let t = new_txid();
        let xor = binding_success(&t, "198.51.100.9:4000".parse().unwrap());
        let xor_value = attribute(&xor, BINDING_SUCCESS, ATTR_XOR_MAPPED).unwrap();
        let plain = [0, FAMILY_V4, 0x0f, 0xa0, 10, 0, 0, 1];
        let want: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        for attrs in [
            [(ATTR_MAPPED, &plain[..]), (ATTR_XOR_MAPPED, xor_value)],
            [
                (ATTR_XOR_MAPPED_LEGACY, xor_value),
                (ATTR_MAPPED, &plain[..]),
            ],
        ] {
            let m = indication(BINDING_SUCCESS, &t, &attrs);
            assert_eq!(parse(&m).unwrap().mapped, Some(want));
        }
        let only_plain = indication(BINDING_SUCCESS, &t, &[(ATTR_MAPPED, &plain[..])]);
        assert_eq!(
            parse(&only_plain).unwrap().mapped,
            Some("10.0.0.1:4000".parse().unwrap())
        );
    }

    #[test]
    fn answer_sizes_follow_the_family() {
        let t = new_txid();
        assert_eq!(binding_request(&t).len(), 20);
        assert_eq!(
            binding_success(&t, "[::ffff:192.0.2.4]:9".parse().unwrap()).len(),
            32
        );
        assert_eq!(
            binding_success(&t, "[2001:db8::4]:9".parse().unwrap()).len(),
            44
        );
    }

    #[test]
    fn shape_limits() {
        let t = new_txid();
        let big = indication(0x3e99, &t, &[(0xc0ff, &[0u8; MAX_LEN - 24])]);
        assert_eq!(big.len(), MAX_LEN);
        assert!(is_stun(&big));
        let over = indication(0x3e99, &t, &[(0xc0ff, &[0u8; MAX_LEN - 20])]);
        assert!(!is_stun(&over));
        let mut top = binding_request(&t);
        top[0] = 0x40;
        assert!(!is_stun(&top));
        // an odd value is padded on the wire, and the next attribute is read after the padding
        let m = indication(0x3e99, &t, &[(0xc001, &[1, 2, 3][..]), (0xc002, &[9][..])]);
        assert_eq!(m.len(), 36);
        assert_eq!(attribute(&m, 0x3e99, 0xc002), Some(&[9u8][..]));
    }

    #[test]
    fn short_or_unknown_address_values_are_ignored() {
        let t = new_txid();
        for value in [
            &[0u8, 1, 0, 0][..],
            &[0, 3, 0, 1, 1, 2, 3, 4][..],
            &[0, 2, 0, 1, 1, 2, 3, 4][..],
        ] {
            let m = indication(BINDING_SUCCESS, &t, &[(ATTR_XOR_MAPPED, value)]);
            assert_eq!(parse(&m).unwrap().mapped, None);
        }
    }

    #[test]
    fn canonical_and_socket_forms() {
        let v6: SocketAddr = "[2001:db8::1]:5".parse().unwrap();
        assert_eq!(canonical(v6), v6);
        assert_eq!(for_socket(v6, false), v6);
        assert_eq!(for_socket(v6, true), v6);
        let v4: SocketAddr = "192.0.2.8:7".parse().unwrap();
        assert_eq!(canonical(for_socket(v4, true)), v4);
    }
}
