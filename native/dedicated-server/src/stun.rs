// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! STUN (RFC 5389) on the world socket (contract §5.1), and what the host learns from it:
//!   - Binding requests to two public STUN servers every 25 s: the world's public address
//!     (IPv4 and IPv6) and the NAT's kind (the same public port for both servers: a cone NAT;
//!     different ports: symmetric, which a cone client cannot punch). They also keep the NAT's
//!     mapping for the world port open.
//!   - Binding requests from a joining player's proxy (its punch) get a Binding success with the
//!     address they came from: the path works, and the player's NAT now lets us in.
//!   - Punches: a burst of Binding requests to a joining player (nat.rs).
//!   - The result is the `world.network` event's NetworkFacts (nat.rs).
//!
//! STUN packets are recognised before the NerdCore parser and never reach it.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

pub const MAGIC: u32 = 0x2112_A442;
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// The pre-RFC 5389 code some servers still send.
const ATTR_XOR_MAPPED_ADDRESS_OLD: u16 = 0x8020;
/// A STUN message is at most this long here (ours are 20 or 32 bytes).
const MAX_LEN: usize = 548;

pub type TxId = [u8; 12];

/// A STUN message as far as we read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: u16,
    pub txid: TxId,
    /// XOR-MAPPED-ADDRESS (or MAPPED-ADDRESS) of a response.
    pub mapped: Option<SocketAddr>,
}

/// Whether `data` is a STUN message: the two top bits clear, the magic cookie, and a length that
/// matches the packet. NerdCore packets never look like that (their header starts with the
/// packet id and flags, and the cookie at bytes 4..8 would be a coincidence of 2^-32 on top).
pub fn is_stun(data: &[u8]) -> bool {
    data.len() >= 20
        && data.len() <= MAX_LEN
        && data[0] & 0xC0 == 0
        && u32::from_be_bytes([data[4], data[5], data[6], data[7]]) == MAGIC
        && u16::from_be_bytes([data[2], data[3]]) as usize + 20 == data.len()
        && data.len().is_multiple_of(4)
}

pub fn parse(data: &[u8]) -> Option<Message> {
    if !is_stun(data) {
        return None;
    }
    let kind = u16::from_be_bytes([data[0], data[1]]);
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&data[8..20]);
    let mut mapped = None;
    let mut at = 20;
    while at + 4 <= data.len() {
        let t = u16::from_be_bytes([data[at], data[at + 1]]);
        let n = u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;
        let value = data.get(at + 4..at + 4 + n)?;
        match t {
            ATTR_XOR_MAPPED_ADDRESS | ATTR_XOR_MAPPED_ADDRESS_OLD => {
                if let Some(a) = address(value, Some(&txid)) {
                    mapped = Some(a);
                }
            }
            ATTR_MAPPED_ADDRESS if mapped.is_none() => mapped = address(value, None),
            _ => {}
        }
        at += 4 + n.div_ceil(4) * 4;
    }
    Some(Message { kind, txid, mapped })
}

/// A (XOR-)MAPPED-ADDRESS value: 0, family (1: IPv4, 2: IPv6), port, address.
fn address(v: &[u8], xor: Option<&TxId>) -> Option<SocketAddr> {
    if v.len() < 8 {
        return None;
    }
    let magic = MAGIC.to_be_bytes();
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor.is_some() {
        port ^= (MAGIC >> 16) as u16;
    }
    match v[1] {
        1 => {
            let mut a = [v[4], v[5], v[6], v[7]];
            if xor.is_some() {
                for i in 0..4 {
                    a[i] ^= magic[i];
                }
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a)), port))
        }
        2 if v.len() >= 20 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&v[4..20]);
            if let Some(t) = xor {
                for i in 0..16 {
                    a[i] ^= if i < 4 { magic[i] } else { t[i - 4] };
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), port))
        }
        _ => None,
    }
}

pub fn binding_request(txid: &TxId) -> [u8; 20] {
    let mut m = [0u8; 20];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..20].copy_from_slice(txid);
    m
}

/// A Binding success with XOR-MAPPED-ADDRESS = `seen` (how the request reached us).
pub fn binding_success(txid: &TxId, seen: SocketAddr) -> Vec<u8> {
    let seen = canonical(seen);
    let magic = MAGIC.to_be_bytes();
    let mut value = vec![0u8, 0];
    value.extend_from_slice(&(seen.port() ^ (MAGIC >> 16) as u16).to_be_bytes());
    match seen.ip() {
        IpAddr::V4(ip) => {
            value[1] = 1;
            value.extend(ip.octets().iter().zip(magic.iter()).map(|(a, b)| a ^ b));
        }
        IpAddr::V6(ip) => {
            value[1] = 2;
            let key: Vec<u8> = magic.iter().chain(txid.iter()).copied().collect();
            value.extend(ip.octets().iter().zip(key.iter()).map(|(a, b)| a ^ b));
        }
    }
    let mut m = Vec::with_capacity(20 + 4 + value.len());
    m.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    m.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    m.extend_from_slice(&magic);
    m.extend_from_slice(txid);
    m.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    m.extend_from_slice(&(value.len() as u16).to_be_bytes());
    m.extend_from_slice(&value);
    m
}

/// A fresh transaction id (std only: RandomState's per-process random keys, a counter, the time).
pub fn new_txid() -> TxId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = [0u8; 12];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u128(t);
        h.write_usize(i);
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`, how the dual-stack socket sees IPv4) as plain IPv4.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => a,
        },
        _ => a,
    }
}

/// `a` in the socket's family: IPv4 as `::ffff:a.b.c.d` for the dual-stack (IPv6) socket.
pub fn for_socket(a: SocketAddr, socket_is_v6: bool) -> SocketAddr {
    match (a, socket_is_v6) {
        (SocketAddr::V4(v4), true) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        _ => a,
    }
}
