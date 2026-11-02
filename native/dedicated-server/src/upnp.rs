// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! UPnP IGD: asks the router to forward UDP <port> to this PC, so that nobody has to open it
//! by hand. std only, in its own thread (nat.rs):
//!   1. SSDP: M-SEARCH to 239.255.255.250:1900 for an InternetGatewayDevice, WANIPConnection or
//!      WANPPPConnection; the first answer's LOCATION is the router's description.
//!   2. HTTP GET of the description (XML): the WANIPConnection (or WANPPPConnection) service's
//!      controlURL.
//!   3. SOAP AddPortMapping UDP <external> -> <this PC's LAN address>:<port>, a lease of
//!      LEASE_SECS renewed every RENEW_EVERY (a router that only takes permanent leases gets 0;
//!      an external port already taken by another PC: the next ones are tried), then
//!      GetExternalIPAddress.
//!
//! The mapping is not removed at exit (the process ends with the world): its lease ends it.
//! Everything the router sends is read with bounds (sizes, timeouts) and only as text.

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpStream, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

pub const LEASE_SECS: u32 = 1800;
pub const RENEW_EVERY: Duration = Duration::from_secs(900);
/// External ports tried after <port> when another PC has it.
const ALTERNATES: u16 = 4;
const SSDP: &str = "239.255.255.250:1900";
const SEARCH_FOR: [&str; 3] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];
const SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];
const HTTP_MAX: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(3);

/// What the router did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No UPnP router answered (UPnP off in the router, or none).
    NotFound,
    /// Forwarded: the world is reachable at external_ip:external_port.
    Mapped {
        external_ip: IpAddr,
        external_port: u16,
        lease: u32,
    },
    /// The router answered but refused, or could not be read.
    Failed(String),
}

/// The router's control point for port mappings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub control: Url,
    pub service: String,
    /// This PC's address on the router's network (the mapping's NewInternalClient).
    pub local: IpAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    /// `http://host[:port]/path` (no https: routers serve plain HTTP on the LAN).
    pub fn parse(s: &str) -> Option<Url> {
        let rest = s
            .trim()
            .strip_prefix("http://")
            .or_else(|| s.trim().strip_prefix("HTTP://"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h, p.parse().ok()?),
            _ => (authority, 80),
        };
        if host.is_empty() || host.len() > 255 {
            return None;
        }
        Some(Url {
            host: host.trim_matches(['[', ']']).to_string(),
            port,
            path: path.to_string(),
        })
    }

    /// A link from the description: absolute, or relative to `self` (or to URLBase).
    pub fn join(&self, link: &str) -> Option<Url> {
        let link = link.trim();
        if link.starts_with("http://") || link.starts_with("HTTP://") {
            return Url::parse(link);
        }
        let path = if link.starts_with('/') {
            link.to_string()
        } else {
            format!("/{link}")
        };
        Some(Url {
            host: self.host.clone(),
            port: self.port,
            path,
        })
    }
}

/// The LOCATION header of an SSDP answer (case-insensitive), if it is an HTTP 200.
pub fn ssdp_location(answer: &str) -> Option<String> {
    let mut lines = answer.lines();
    if !lines.next()?.contains(" 200") {
        return None;
    }
    lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.trim().to_string())
}

/// The text between <tag> and </tag> from `from` on, and where it ends.
fn element<'a>(xml: &'a str, tag: &str, from: usize) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.get(from..)?.find(&open)? + from + open.len();
    let end = xml.get(start..)?.find(&close)? + start;
    Some((&xml[start..end], end + close.len()))
}

/// From the router's description: (service type, control URL) of the first port mapping service
/// it has, in the order of SERVICES.
pub fn find_service(xml: &str, location: &Url) -> Option<(String, Url)> {
    let base = element(xml, "URLBase", 0)
        .and_then(|(b, _)| Url::parse(b))
        .unwrap_or_else(|| location.clone());
    let mut found: Vec<(usize, String, String)> = Vec::new();
    let mut at = 0;
    while let Some((service, end)) = element(xml, "service", at) {
        at = end;
        let Some((kind, _)) = element(service, "serviceType", 0) else {
            continue;
        };
        let Some((control, _)) = element(service, "controlURL", 0) else {
            continue;
        };
        if let Some(rank) = SERVICES
            .iter()
            .position(|s| s.eq_ignore_ascii_case(kind.trim()))
        {
            found.push((rank, kind.trim().to_string(), control.trim().to_string()));
        }
    }
    found.sort_by_key(|f| f.0);
    let (_, kind, control) = found.into_iter().next()?;
    Some((kind, base.join(&control)?))
}

/// A UPnP error code in a SOAP fault (<errorCode>718</errorCode>).
pub fn soap_error(xml: &str) -> Option<u32> {
    element(xml, "errorCode", 0).and_then(|(c, _)| c.trim().parse().ok())
}

pub fn soap_body(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut a = String::new();
    for (k, v) in args {
        a.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">{a}\
         </u:{action}></s:Body></s:Envelope>\r\n"
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The value of <name> anywhere in a SOAP answer, without a namespace prefix concern.
pub fn soap_value(xml: &str, name: &str) -> Option<String> {
    element(xml, name, 0).map(|(v, _)| v.trim().to_string())
}

/// Splits an HTTP/1.x answer into (status, body), undoing chunked encoding.
pub fn http_split(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let body = &raw[end + 4..];
    let chunked = head.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    if !chunked {
        return Some((status, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let line_end = body.get(at..)?.windows(2).position(|w| w == b"\r\n")? + at;
        let size_text = std::str::from_utf8(&body[at..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        at = line_end + 2;
        if size == 0 {
            return Some((status, out));
        }
        out.extend_from_slice(body.get(at..at + size)?);
        at += size + 2;
    }
}

/// One HTTP request to the router; the answer is bounded by HTTP_MAX and TIMEOUT. The second
/// value is our address on that connection (this PC's LAN address towards the router).
fn http(url: &Url, method: &str, extra: &str, body: &str) -> Result<(u16, String, IpAddr), String> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("endereço do roteador: {e}"))?
        .next()
        .ok_or("endereço do roteador não resolvido")?;
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)
        .map_err(|e| format!("conexão ao roteador: {e}"))?;
    let local = s.local_addr().map_err(|e| e.to_string())?.ip();
    s.set_read_timeout(Some(TIMEOUT)).ok();
    s.set_write_timeout(Some(TIMEOUT)).ok();
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {host}:{}\r\nConnection: close\r\nUser-Agent: ForzaInfinity dedicated_server\r\n\
         {extra}Content-Length: {}\r\n\r\n{body}",
        url.path,
        url.port,
        body.len()
    );
    s.write_all(request.as_bytes())
        .map_err(|e| format!("envio ao roteador: {e}"))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let started = Instant::now();
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > HTTP_MAX {
                    return Err("resposta do roteador grande demais".into());
                }
            }
            Err(_) if !raw.is_empty() => break,
            Err(e) => return Err(format!("resposta do roteador: {e}")),
        }
        if started.elapsed() > TIMEOUT * 2 {
            break;
        }
    }
    let (status, body) = http_split(&raw).ok_or("resposta do roteador ilegível")?;
    Ok((status, String::from_utf8_lossy(&body).into_owned(), local))
}

/// SSDP search: the LOCATIONs that answered within `wait` (at most 8).
fn discover(wait: Duration) -> Vec<String> {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let _ = sock.set_multicast_ttl_v4(2);
    for st in SEARCH_FOR {
        let m = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        );
        let _ = sock.send_to(m.as_bytes(), SSDP);
    }
    let mut found = Vec::new();
    let end = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    while Instant::now() < end && found.len() < 8 {
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Some(l) = ssdp_location(&String::from_utf8_lossy(&buf[..n]))
            && !found.contains(&l)
        {
            found.push(l);
        }
    }
    found
}

/// The first router whose description has a port mapping service.
pub fn find_gateway(wait: Duration) -> Option<Gateway> {
    for location in discover(wait) {
        let Some(url) = Url::parse(&location) else {
            continue;
        };
        let Ok((200, xml, local)) = http(&url, "GET", "", "") else {
            continue;
        };
        if let Some((service, control)) = find_service(&xml, &url) {
            return Some(Gateway {
                control,
                service,
                local,
            });
        }
    }
    None
}

fn soap(
    g: &Gateway,
    action: &str,
    args: &[(&str, String)],
) -> Result<String, (u16, Option<u32>, String)> {
    let body = soap_body(&g.service, action, args);
    let extra = format!(
        "Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{action}\"\r\n",
        g.service
    );
    match http(&g.control, "POST", &extra, &body) {
        Ok((200, xml, _)) => Ok(xml),
        Ok((status, xml, _)) => Err((status, soap_error(&xml), xml)),
        Err(e) => Err((0, None, e)),
    }
}

/// Forwards UDP external_port -> local:port on the gateway (alternate external ports if
/// another PC has it), then asks for the external address.
pub fn map(g: &Gateway, port: u16) -> Outcome {
    let mut last = String::new();
    for external in (0..=ALTERNATES).filter_map(|k| port.checked_add(k)) {
        for lease in [LEASE_SECS, 0] {
            let args = [
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", external.to_string()),
                ("NewProtocol", "UDP".to_string()),
                ("NewInternalPort", port.to_string()),
                ("NewInternalClient", g.local.to_string()),
                ("NewEnabled", "1".to_string()),
                (
                    "NewPortMappingDescription",
                    "ForzaInfinity world".to_string(),
                ),
                ("NewLeaseDuration", lease.to_string()),
            ];
            match soap(g, "AddPortMapping", &args) {
                Ok(_) => {
                    let external_ip = soap(g, "GetExternalIPAddress", &[])
                        .ok()
                        .and_then(|x| soap_value(&x, "NewExternalIPAddress"))
                        .and_then(|ip| ip.parse::<IpAddr>().ok());
                    return match external_ip {
                        Some(ip) => Outcome::Mapped {
                            external_ip: ip,
                            external_port: external,
                            lease,
                        },
                        None => Outcome::Failed(
                            "o roteador abriu a porta mas não disse o IP externo".into(),
                        ),
                    };
                }
                // 725 OnlyPermanentLeasesSupported: again with lease 0
                Err((_, Some(725), _)) if lease != 0 => continue,
                // 718 ConflictInMappingEntry: another PC has this external port
                Err((_, Some(718), _)) => {
                    last = format!("porta {external} já usada por outro PC");
                    break;
                }
                Err((status, code, text)) => {
                    let what = match code {
                        Some(c) => format!("erro UPnP {c}"),
                        None if status == 0 => text,
                        None => format!("HTTP {status}"),
                    };
                    return Outcome::Failed(format!("o roteador recusou: {what}"));
                }
            }
        }
    }
    Outcome::Failed(if last.is_empty() {
        "o roteador recusou".into()
    } else {
        last
    })
}

/// Whether `ip` is not on the Internet itself: the router's "external" address is then behind
/// another NAT (double NAT, or the operator's CGNAT in 100.64.0.0/10).
pub fn behind_another_nat(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        IpAddr::V6(_) => false,
    }
}

/// The router as nat.rs reaches it: found once, then mapped and renewed.
pub fn run(port: u16, report: impl Fn(Outcome)) {
    loop {
        let Some(g) = find_gateway(Duration::from_secs(3)) else {
            report(Outcome::NotFound);
            std::thread::sleep(Duration::from_secs(300));
            continue;
        };
        loop {
            let o = map(&g, port);
            let ok = matches!(o, Outcome::Mapped { .. });
            report(o);
            std::thread::sleep(if ok {
                RENEW_EVERY
            } else {
                Duration::from_secs(300)
            });
            if !ok {
                break;
            }
        }
    }
}

/// For tests: an address on loopback that answers like a router (see the tests).
pub fn gateway_at(control: Url, service: &str, local: IpAddr) -> Gateway {
    Gateway {
        control,
        service: service.to_string(),
        local,
    }
}
