//! Datagram shapes, all ours except the binding answer, which follows the public STUN shape
//! (RFC 5389 §6, §15.2) so any binding client can read it.
//!
//! Binding (`:3478`): a 20-byte Binding Request (type `0x0001`, magic cookie `0x2112A442`, a
//! 12-byte transaction id, attributes ignored) is answered with a Binding Success Response
//! (`0x0101`) carrying one XOR-MAPPED-ADDRESS: the sender's public endpoint.
//!
//! Rendezvous (`:5101`), every datagram `"FIR1"` ‖ type ‖ body, integers big-endian, grant ids the
//! 16 bytes of the grant's UUID:
//!
//! | Type | Direction | Body |
//! | --- | --- | --- |
//! | `0x01` hello | peer → relay | role (`1` joiner, `2` host), claims length (u16), claims, 64-byte signature, then the 16-byte cookie of the last challenge (absent on the first hello) |
//! | `0x02` peer | relay → peer | grant id, the other side's role, family (`4` or `6`), address, port (u16) |
//! | `0x03` relay | peer → relay | grant id: punching failed, forward for this grant |
//! | `0x04` relay ok | relay → peer | grant id: send `data` to the relay from now on |
//! | `0x05` data | both ways | grant id, payload (at most [`MAX_PAYLOAD`] bytes) |
//! | `0x06` relay unavailable | relay → peer | grant id: no fallback for this grant |
//! | `0x07` challenge | relay → peer | grant id, 16-byte cookie: say hello again with it |
//!
//! The challenge is return routability: a side is registered only by a hello that echoes a
//! cookie the relay sent to that very address, so a spoofed source can never be registered (and
//! never receive forwarded traffic), and the challenge is far smaller than the hello it answers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// First bytes of every rendezvous datagram.
pub const MAGIC: &[u8; 4] = b"FIR1";
/// Largest forwarded payload (fits one Ethernet frame with the headers).
pub const MAX_PAYLOAD: usize = 1200;
/// STUN magic cookie.
pub const COOKIE: u32 = 0x2112_A442;

/// Which side of a grant a peer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The player who was granted the seat.
    Joiner,
    /// The world's host relay.
    Host,
}

impl Role {
    fn byte(self) -> u8 {
        match self {
            Role::Joiner => 1,
            Role::Host => 2,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Role::Joiner),
            2 => Some(Role::Host),
            _ => None,
        }
    }

    /// The claim value of this side (`RelayGrantClaims::role`).
    pub fn claim(self) -> &'static str {
        match self {
            Role::Joiner => "joiner",
            Role::Host => "host",
        }
    }

    /// The other side.
    pub fn other(self) -> Self {
        match self {
            Role::Joiner => Role::Host,
            Role::Host => Role::Joiner,
        }
    }
}

/// A grant id (the UUID's 16 bytes).
pub type GrantId = [u8; 16];

/// A parsed rendezvous datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// `0x01`.
    Hello {
        /// The sender's side.
        role: Role,
        /// Claims bytes as signed.
        claims: &'a [u8],
        /// The signature.
        signature: [u8; 64],
        /// The cookie of the relay's challenge to this address.
        cookie: Option<[u8; 16]>,
    },
    /// `0x02`.
    Peer {
        /// The grant.
        grant: GrantId,
        /// The other side's role.
        role: Role,
        /// The other side's public endpoint.
        endpoint: SocketAddr,
    },
    /// `0x03`.
    Relay {
        /// The grant.
        grant: GrantId,
    },
    /// `0x04`.
    RelayOk {
        /// The grant.
        grant: GrantId,
    },
    /// `0x05`.
    Data {
        /// The grant.
        grant: GrantId,
        /// Opaque game bytes.
        payload: &'a [u8],
    },
    /// `0x06`.
    RelayUnavailable {
        /// The grant.
        grant: GrantId,
    },
    /// `0x07`.
    Challenge {
        /// The grant.
        grant: GrantId,
        /// Echo it in the next hello.
        cookie: [u8; 16],
    },
}

fn grant_of(b: &[u8]) -> Option<GrantId> {
    b.get(..16)?.try_into().ok()
}

impl<'a> Message<'a> {
    /// Parses a datagram (`None`: not ours or malformed, ignore it).
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let rest = data.strip_prefix(MAGIC.as_slice())?;
        let (&kind, body) = rest.split_first()?;
        match kind {
            0x01 => {
                let role = Role::from_byte(*body.first()?)?;
                let len = usize::from(u16::from_be_bytes(body.get(1..3)?.try_into().ok()?));
                let claims = body.get(3..3 + len)?;
                let rest = body.get(3 + len..)?;
                let signature: [u8; 64] = rest.get(..64)?.try_into().ok()?;
                let cookie = match rest.len() {
                    64 => None,
                    80 => Some(rest[64..].try_into().ok()?),
                    _ => return None,
                };
                Some(Message::Hello {
                    role,
                    claims,
                    signature,
                    cookie,
                })
            }
            0x02 => {
                let grant = grant_of(body)?;
                let role = Role::from_byte(*body.get(16)?)?;
                let (ip, port_at) = match *body.get(17)? {
                    4 => {
                        let o: [u8; 4] = body.get(18..22)?.try_into().ok()?;
                        (IpAddr::V4(Ipv4Addr::from(o)), 22)
                    }
                    6 => {
                        let o: [u8; 16] = body.get(18..34)?.try_into().ok()?;
                        (IpAddr::V6(Ipv6Addr::from(o)), 34)
                    }
                    _ => return None,
                };
                let port = u16::from_be_bytes(body.get(port_at..port_at + 2)?.try_into().ok()?);
                (body.len() == port_at + 2).then_some(Message::Peer {
                    grant,
                    role,
                    endpoint: SocketAddr::new(ip, port),
                })
            }
            0x03 | 0x04 | 0x06 => {
                let grant = grant_of(body).filter(|_| body.len() == 16)?;
                Some(match kind {
                    0x03 => Message::Relay { grant },
                    0x04 => Message::RelayOk { grant },
                    _ => Message::RelayUnavailable { grant },
                })
            }
            0x05 => {
                let grant = grant_of(body)?;
                let payload = &body[16..];
                (!payload.is_empty() && payload.len() <= MAX_PAYLOAD)
                    .then_some(Message::Data { grant, payload })
            }
            0x07 => {
                let grant = grant_of(body)?;
                let cookie: [u8; 16] = body.get(16..)?.try_into().ok()?;
                Some(Message::Challenge { grant, cookie })
            }
            _ => None,
        }
    }

    /// The datagram bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Hello {
                role,
                claims,
                signature,
                cookie,
            } => {
                out.push(0x01);
                out.push(role.byte());
                let len = u16::try_from(claims.len()).unwrap_or(u16::MAX);
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(&claims[..usize::from(len)]);
                out.extend_from_slice(signature);
                if let Some(c) = cookie {
                    out.extend_from_slice(c);
                }
            }
            Message::Peer {
                grant,
                role,
                endpoint,
            } => {
                out.push(0x02);
                out.extend_from_slice(grant);
                out.push(role.byte());
                match endpoint.ip() {
                    IpAddr::V4(v4) => {
                        out.push(4);
                        out.extend_from_slice(&v4.octets());
                    }
                    IpAddr::V6(v6) => {
                        out.push(6);
                        out.extend_from_slice(&v6.octets());
                    }
                }
                out.extend_from_slice(&endpoint.port().to_be_bytes());
            }
            Message::Relay { grant } => {
                out.push(0x03);
                out.extend_from_slice(grant);
            }
            Message::RelayOk { grant } => {
                out.push(0x04);
                out.extend_from_slice(grant);
            }
            Message::Data { grant, payload } => {
                out.push(0x05);
                out.extend_from_slice(grant);
                out.extend_from_slice(payload);
            }
            Message::RelayUnavailable { grant } => {
                out.push(0x06);
                out.extend_from_slice(grant);
            }
            Message::Challenge { grant, cookie } => {
                out.push(0x07);
                out.extend_from_slice(grant);
                out.extend_from_slice(cookie);
            }
        }
        out
    }
}

/// The 16 bytes of a canonical UUID string (`8-4-4-4-12` hex), else `None`.
pub fn grant_id(uuid: &str) -> Option<GrantId> {
    let b = uuid.as_bytes();
    if b.len() != 36 || [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
        return None;
    }
    let hex: Vec<u8> = b.iter().copied().filter(|&c| c != b'-').collect();
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        out[i] = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

/// The Binding Success Response to a Binding Request from `from`, else `None` (ignore).
pub fn binding_answer(request: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
    if request.len() < 20 || request[0..2] != [0x00, 0x01] {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([request[2], request[3]]));
    if u32::from_be_bytes(request[4..8].try_into().ok()?) != COOKIE
        || length % 4 != 0
        || request.len() != 20 + length
    {
        return None;
    }
    let txid = &request[8..20];
    let xport = from.port() ^ (COOKIE >> 16) as u16;
    let mut value = vec![0x00];
    match from.ip() {
        IpAddr::V4(v4) => {
            value.push(0x01);
            value.extend_from_slice(&xport.to_be_bytes());
            let x = u32::from(v4) ^ COOKIE;
            value.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(v6) => {
            value.push(0x02);
            value.extend_from_slice(&xport.to_be_bytes());
            let mut mask = COOKIE.to_be_bytes().to_vec();
            mask.extend_from_slice(txid);
            for (o, m) in v6.octets().iter().zip(mask) {
                value.push(o ^ m);
            }
        }
    }
    let mut out = Vec::with_capacity(20 + 4 + value.len());
    out.extend_from_slice(&[0x01, 0x01]);
    out.extend_from_slice(&u16::try_from(4 + value.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&COOKIE.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&[0x00, 0x20]);
    out.extend_from_slice(&u16::try_from(value.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&value);
    Some(out)
}

/// Reads the XOR-MAPPED-ADDRESS of a Binding Success Response (clients and tests).
pub fn mapped_address(response: &[u8]) -> Option<SocketAddr> {
    if response.len() < 20 || response[0..2] != [0x01, 0x01] {
        return None;
    }
    let txid = &response[8..20];
    let mut attrs = &response[20..];
    while attrs.len() >= 4 {
        let kind = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = usize::from(u16::from_be_bytes([attrs[2], attrs[3]]));
        let value = attrs.get(4..4 + len)?;
        if kind == 0x0020 && len >= 8 {
            let port = u16::from_be_bytes([value[2], value[3]]) ^ (COOKIE >> 16) as u16;
            let ip = match value[1] {
                0x01 => IpAddr::V4(Ipv4Addr::from(
                    u32::from_be_bytes(value[4..8].try_into().ok()?) ^ COOKIE,
                )),
                0x02 if len == 20 => {
                    let mut mask = COOKIE.to_be_bytes().to_vec();
                    mask.extend_from_slice(txid);
                    let mut o = [0u8; 16];
                    for (i, b) in value[4..20].iter().enumerate() {
                        o[i] = b ^ mask[i];
                    }
                    IpAddr::V6(Ipv6Addr::from(o))
                }
                _ => return None,
            };
            return Some(SocketAddr::new(ip, port));
        }
        attrs = attrs.get(4 + len.div_ceil(4) * 4..).unwrap_or(&[]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendezvous_messages_round_trip_and_garbage_is_ignored() {
        let grant = grant_id("0192f0c4-0000-7000-8000-0000000000aa").unwrap();
        let sig = [7u8; 64];
        let claims = br#"{"v":1}"#;
        for m in [
            Message::Hello {
                role: Role::Host,
                claims,
                signature: sig,
                cookie: None,
            },
            Message::Hello {
                role: Role::Joiner,
                claims,
                signature: sig,
                cookie: Some([3u8; 16]),
            },
            Message::Challenge {
                grant,
                cookie: [9u8; 16],
            },
            Message::Peer {
                grant,
                role: Role::Joiner,
                endpoint: "203.0.113.9:40000".parse().unwrap(),
            },
            Message::Peer {
                grant,
                role: Role::Host,
                endpoint: "[2001:db8::1]:5101".parse().unwrap(),
            },
            Message::Relay { grant },
            Message::RelayOk { grant },
            Message::RelayUnavailable { grant },
            Message::Data {
                grant,
                payload: b"x",
            },
        ] {
            let bytes = m.encode();
            assert_eq!(Message::parse(&bytes), Some(m));
        }
        for bad in [
            &b""[..],
            b"FIR1",
            b"FIR2\x03aaaaaaaaaaaaaaaa",
            b"FIR1\x03short",
            b"FIR1\x01\x03\x00\x00",
            b"FIR1\x05aaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(Message::parse(bad), None, "{bad:?}");
        }
        let big = Message::Data {
            grant,
            payload: &[0u8; MAX_PAYLOAD + 1],
        }
        .encode();
        assert_eq!(Message::parse(&big), None, "oversized payloads are dropped");
        assert_eq!(grant_id("not-a-uuid"), None);
        assert_eq!(grant_id("0192f0c4-0000-7000-8000-0000000000ag"), None);
    }

    #[test]
    fn binding_answers_carry_the_sender_endpoint() {
        let mut req = vec![0x00, 0x01, 0x00, 0x00];
        req.extend_from_slice(&COOKIE.to_be_bytes());
        req.extend_from_slice(&[9u8; 12]);
        for from in ["198.51.100.7:51234", "[2001:db8::42]:3478"] {
            let from: SocketAddr = from.parse().unwrap();
            let answer = binding_answer(&req, from).unwrap();
            assert_eq!(&answer[8..20], &[9u8; 12], "transaction id echoed");
            assert_eq!(mapped_address(&answer), Some(from));
        }
        let mut wrong_cookie = req.clone();
        wrong_cookie[4] ^= 1;
        assert!(binding_answer(&wrong_cookie, "127.0.0.1:1".parse().unwrap()).is_none());
        assert!(binding_answer(&req[..19], "127.0.0.1:1".parse().unwrap()).is_none());
        let mut response_type = req.clone();
        response_type[1] = 0x02;
        assert!(binding_answer(&response_type, "127.0.0.1:1".parse().unwrap()).is_none());
    }
}
