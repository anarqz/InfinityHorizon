// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! UPnP port mapping for a private world's socket (contract §5.1): SSDP M-SEARCH discovery of an
//! Internet Gateway Device (UPnP Device Architecture 1.1 §1.3), its description's
//! WANIPConnection:2/:1 or WANPPPConnection:1 control URL, then the SOAP actions AddPortMapping
//! (UDP, lease 1800 s, falling back to a permanent lease on error 725 and to the next of four
//! external ports on 718), GetExternalIPAddress, and DeletePortMapping when the world stops.
//! Blocking std I/O with hard limits (3 s per request, 256 KiB per answer, router addresses
//! only); it runs on its own thread, never on the world loop. The XML reading is a small
//! tolerant scanner: no DTD, no entity beyond the five predefined ones and character references.

use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// The lease asked for a mapping, in seconds.
pub const LEASE_DURATION_SECS: u32 = 1800;
/// A live mapping is asked for again this often (half the lease).
pub const RENEW_INTERVAL: Duration = Duration::from_secs(900);

/// The largest router answer read.
const HTTP_LIMIT: usize = 256 * 1024;
/// One HTTP exchange's deadline, connect to last byte.
const HTTP_DEADLINE: Duration = Duration::from_secs(3);
/// After a failure or a router not found, the next attempt.
const RETRY_AFTER: Duration = Duration::from_secs(300);
/// How long discovery listens for SSDP answers in [`hold_mapping`].
const SEARCH_WAIT: Duration = Duration::from_secs(3);
/// Distinct description URLs taken from one search.
const LOCATIONS_MAX: usize = 8;
/// The external ports tried: the world's port and the next four.
const PORT_TRIES: u16 = 5;
/// Granularity of the waits in [`hold_mapping`], so a stop is seen quickly.
const STOP_POLL: Duration = Duration::from_millis(250);

const SSDP_GROUP: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
const IGD_DEVICE: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
const WAN_IP_2: &str = "urn:schemas-upnp-org:service:WANIPConnection:2";
const WAN_IP_1: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
const WAN_PPP_1: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";
/// Services in order of preference.
const SERVICES: [&str; 3] = [WAN_IP_2, WAN_IP_1, WAN_PPP_1];
/// What M-SEARCH asks for (one request each).
const SEARCH_TARGETS: [&str; 3] = [IGD_DEVICE, WAN_IP_1, WAN_PPP_1];

/// UPnP errorCode values (WANIPConnection:2 §2.4.16).
const ERR_CONFLICT: u32 = 718;
const ERR_ONLY_PERMANENT_LEASES: u32 = 725;

/// What a mapping attempt ended with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapResult {
    Mapped {
        public_ip: IpAddr,
        public_port: u16,
        lease_secs: u32,
    },
    /// No gateway answered the search.
    NotFound,
    /// A gateway answered, but no mapping came of it.
    Failed,
}

/// An http:// URL as routers write them: host (no brackets), port, path with query.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Location {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Text safe inside a request line or header: no whitespace, no control bytes.
fn header_safe(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_graphic())
}

impl Location {
    /// `http://host[:port][/path]`; `None` for another scheme, user info, a fragment, an empty
    /// host, a port outside 1..=65535 or bytes that do not belong in a request line.
    pub fn from_text(s: &str) -> Option<Location> {
        let s = s.trim();
        let scheme = s.get(..7)?;
        if !scheme.eq_ignore_ascii_case("http://") || !header_safe(s) || s.contains('#') {
            return None;
        }
        let rest = &s[7..];
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        if authority.contains('@') {
            return None;
        }
        let (host, port_text) = if let Some(inner) = authority.strip_prefix('[') {
            let (h, after) = inner.split_once(']')?;
            match after {
                "" => (h, None),
                p => (h, Some(p.strip_prefix(':')?)),
            }
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        let port = match port_text {
            None => 80,
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        };
        if host.is_empty() {
            return None;
        }
        Some(Location {
            host: host.to_string(),
            port,
            path,
        })
    }

    /// A link from a description: an absolute URL replaces this one; anything else is a path
    /// from this URL's root (routers write `ctl/IPConn` meaning `/ctl/IPConn`).
    pub fn resolve(&self, link: &str) -> Option<Location> {
        let text = link.trim();
        let absolute = text
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"));
        if absolute {
            Location::from_text(text)
        } else if text.contains("://") || text.contains('#') || !header_safe(text) {
            None
        } else {
            let path = format!("/{}", text.trim_start_matches('/'));
            Some(Location {
                path,
                ..self.clone()
            })
        }
    }

    /// The Host header's value.
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A gateway's control endpoint, the service type it speaks, and the local address this machine
/// reaches it from (the mapping's internal client).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Igd {
    pub control_url: Location,
    pub service_type: String,
    pub client_ip: IpAddr,
}

impl Igd {
    /// A gateway known without discovery.
    pub fn known(control_url: Location, service_type: &str, client_ip: IpAddr) -> Igd {
        Igd {
            control_url,
            service_type: service_type.to_owned(),
            client_ip,
        }
    }
}

/// The LOCATION of an SSDP search answer (`HTTP/1.x 200`); notifications and other statuses
/// carry none.
pub fn location_header(answer: &str) -> Option<String> {
    let mut rows = answer.lines();
    let status = rows.next()?;
    let is_ok = status.len() >= 7
        && status[..7].eq_ignore_ascii_case("HTTP/1.")
        && status.split_whitespace().nth(1) == Some("200");
    if !is_ok {
        return None;
    }
    rows.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| value.trim().to_string())
    })
}

// ---------------------------------------------------------------- the XML scanner

/// One tag found by [`next_tag`]: its local name (prefix dropped), whether it closes, whether it
/// is self-closing, and the byte offsets of `<` and just past `>`.
struct Tag<'a> {
    name: &'a str,
    closing: bool,
    empty: bool,
    start: usize,
    end: usize,
}

/// The next start or end tag at or after `from`; comments, processing instructions and
/// declarations are skipped.
fn next_tag(xml: &str, mut from: usize) -> Option<Tag<'_>> {
    loop {
        let start = from + xml.get(from..)?.find('<')?;
        if xml[start..].starts_with("<!--") {
            from = start + 4 + xml[start + 4..].find("-->")? + 3;
            continue;
        }
        let body_from = start + 1;
        let close = body_from + xml[body_from..].find('>')?;
        let inner = &xml[body_from..close];
        from = close + 1;
        if inner.starts_with(['?', '!']) {
            continue;
        }
        let closing = inner.starts_with('/');
        let inner = inner.trim_start_matches('/');
        let empty = inner.ends_with('/');
        let raw_name = inner
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        return Some(Tag {
            name,
            closing,
            empty,
            start,
            end: from,
        });
    }
}

/// The raw inner text of every `name` element (by local name, outermost only), in order. An
/// element with no end tag ends the list.
fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut inners: Vec<&'a str> = Vec::new();
    let mut cursor = 0;
    while let Some(open) = next_tag(xml, cursor) {
        cursor = open.end;
        if open.closing || open.name != name {
            continue;
        }
        if open.empty {
            inners.push("");
            continue;
        }
        let mut depth = 1usize;
        let mut scan = open.end;
        let mut finished = None;
        while let Some(t) = next_tag(xml, scan) {
            scan = t.end;
            if t.name == name && !t.empty {
                if t.closing {
                    depth -= 1;
                    if depth == 0 {
                        finished = Some(t.start);
                        break;
                    }
                } else {
                    depth += 1;
                }
            }
        }
        let Some(stop) = finished else { break };
        inners.push(&xml[open.end..stop]);
        cursor = scan;
    }
    inners
}

/// The five predefined entities and numeric character references; anything else stays as it is.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The trimmed text of the first `name` element (by local name). Tolerates fragments: only the
/// element itself has to be closed.
pub fn element_text(xml: &str, name: &str) -> Option<String> {
    elements(xml, name)
        .first()
        .map(|inner| unescape(inner).trim().to_string())
}

/// The UPnP errorCode of a SOAP fault.
pub fn upnp_error_code(xml: &str) -> Option<u32> {
    element_text(xml, "errorCode")?.parse().ok()
}

/// The service to control and its control URL from a device description: the preferred service
/// type found anywhere in the device tree, its controlURL resolved against URLBase (when valid)
/// or the description's own URL.
pub fn wan_service(xml: &str, location: &Location) -> Option<(String, Location)> {
    let declared = element_text(xml, "URLBase").and_then(|b| Location::from_text(&b));
    let base = match declared {
        Some(b) => b,
        None => location.clone(),
    };
    let offered: Vec<(String, String)> = elements(xml, "service")
        .into_iter()
        .filter_map(|block| {
            Some((
                element_text(block, "serviceType")?,
                element_text(block, "controlURL")?,
            ))
        })
        .filter(|(_, control)| !control.is_empty())
        .collect();
    SERVICES.iter().find_map(|wanted| {
        offered
            .iter()
            .filter(|(kind, _)| kind.eq_ignore_ascii_case(wanted))
            .find_map(|(_, control)| base.resolve(control))
            .map(|url| (wanted.to_string(), url))
    })
}

/// A SOAP 1.1 envelope calling `action` of `service` with these arguments, values escaped.
pub fn soap_envelope(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut call = String::new();
    for (name, value) in args {
        call.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{}\">{call}</u:{action}></s:Body></s:Envelope>",
        escape(service)
    )
}

// ---------------------------------------------------------------- HTTP

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The body of a chunked transfer (RFC 9112 §7.1), its last chunk followed directly by CRLF.
fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut decoded: Vec<u8> = Vec::new();
    loop {
        let line_end = find_bytes(body, b"\r\n")?;
        let line = std::str::from_utf8(&body[..line_end]).ok()?;
        let hex = line.split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex, 16).ok()?;
        body = &body[line_end + 2..];
        if chunk_len == 0 {
            return body.starts_with(b"\r\n").then_some(decoded);
        }
        let room = HTTP_LIMIT - decoded.len();
        let (data, rest) = body
            .split_at_checked(chunk_len)
            .filter(|_| chunk_len <= room)?;
        body = rest.strip_prefix(b"\r\n")?;
        decoded.extend_from_slice(data);
    }
}

/// A complete HTTP/1.x response: its status and body (de-chunked, or cut to Content-Length).
/// `None` for anything malformed, incomplete or over the limit.
pub fn split_http_response(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    if raw.len() > HTTP_LIMIT {
        return None;
    }
    let head_end = find_bytes(raw, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;
    let body = &raw[head_end + 4..];
    let mut lines = head.split("\r\n");
    let mut status_line = lines.next()?.split(' ');
    if !status_line.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let status: u16 = status_line.next()?.parse().ok()?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") {
                return None;
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            let n: usize = value.parse().ok().filter(|n| *n <= HTTP_LIMIT)?;
            if length.is_some_and(|l| l != n) {
                return None;
            }
            length = Some(n);
        }
    }
    if chunked {
        return dechunk(body).map(|b| (status, b));
    }
    match length {
        Some(n) => body.get(..n).map(|b| (status, b.to_vec())),
        None => Some((status, body.to_vec())),
    }
}

/// An address a router may have: private or shared IPv4 (and loopback), IPv6 link-local, ULA or
/// loopback. Discovery and control never leave the local network.
fn local_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && unroutable_external(ip),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => local_network(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                v6.is_loopback() || first & 0xffc0 == 0xfe80 || first & 0xfe00 == 0xfc00
            }
        },
    }
}

/// Whether the response so far declares its framing (then a complete frame ends the read even if
/// the router keeps the connection open).
fn declares_framing(raw: &[u8]) -> bool {
    let Some(end) = find_bytes(raw, b"\r\n\r\n") else {
        return false;
    };
    String::from_utf8_lossy(&raw[..end]).split("\r\n").any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("content-length:") || l.starts_with("transfer-encoding:")
    })
}

struct Reply {
    status: u16,
    body: String,
    local: IpAddr,
}

/// One HTTP/1.1 exchange with a router, under [`HTTP_DEADLINE`].
fn exchange(url: &Location, method: &str, extra_headers: &str, body: &str) -> Option<Reply> {
    let deadline = Instant::now() + HTTP_DEADLINE;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    };
    let mut candidates = ToSocketAddrs::to_socket_addrs(&(url.host.as_str(), url.port)).ok()?;
    let target = candidates.find(|a| local_network(a.ip()))?;
    let mut stream = TcpStream::connect_timeout(&target, left()?).ok()?;
    let local = stream.local_addr().ok()?.ip();
    let mut request = String::with_capacity(256 + body.len());
    for part in [
        method,
        " ",
        &url.path,
        " HTTP/1.1\r\nHost: ",
        &url.authority(),
        "\r\nConnection: close\r\nUser-Agent: Infinity\r\n",
        extra_headers,
        "Content-Length: ",
        &body.len().to_string(),
        "\r\n\r\n",
        body,
    ] {
        request.push_str(part);
    }
    stream.set_write_timeout(left()).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut got: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        stream.set_read_timeout(Some(left()?)).ok()?;
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        if got.len() + n > HTTP_LIMIT {
            return None;
        }
        got.extend_from_slice(&chunk[..n]);
        if declares_framing(&got) && split_http_response(&got).is_some() {
            break;
        }
    }
    let (status, body) = split_http_response(&got)?;
    Some(Reply {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
        local,
    })
}

fn soap(g: &Igd, action: &str, args: &[(&str, String)]) -> Option<Reply> {
    let headers = format!(
        "Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{}\"\r\n",
        g.service_type, action
    );
    exchange(
        &g.control_url,
        "POST",
        &headers,
        &soap_envelope(&g.service_type, action, args),
    )
}

// ---------------------------------------------------------------- discovery and mapping

/// Searches the LAN for a gateway for at most `wait`, then reads the descriptions found (up to
/// eight) until one offers a WAN connection service.
pub fn discover_igd(wait: Duration) -> Option<Igd> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_multicast_ttl_v4(2).ok()?;
    for target in SEARCH_TARGETS {
        let search = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {target}\r\n\r\n"
        );
        let _ = socket.send_to(search.as_bytes(), SSDP_GROUP);
    }
    let until = Instant::now() + wait;
    let mut locations: Vec<String> = Vec::new();
    let mut inbox = vec![0u8; 2048];
    while locations.len() < LOCATIONS_MAX {
        let Some(left) = until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
        else {
            break;
        };
        if socket.set_read_timeout(Some(left)).is_err() {
            break;
        }
        let Ok((n, from)) = socket.recv_from(&mut inbox) else {
            break;
        };
        if !local_network(from.ip()) {
            continue;
        }
        if let Some(l) = location_header(&String::from_utf8_lossy(&inbox[..n]))
            && !locations.contains(&l)
        {
            locations.push(l);
        }
    }
    locations.iter().find_map(|l| {
        let url = Location::from_text(l)?;
        let reply = exchange(&url, "GET", "", "")?;
        if reply.status != 200 {
            return None;
        }
        let (service, control) = wan_service(&reply.body, &url)?;
        Some(Igd {
            control_url: control,
            service_type: service,
            client_ip: reply.local,
        })
    })
}

/// The arguments that name a UDP mapping (any remote host, this external port): the head of
/// both AddPortMapping and DeletePortMapping, in the order the service table lists them.
fn mapping_key(outside: u16) -> Vec<(&'static str, String)> {
    [
        ("NewRemoteHost", ""),
        ("NewExternalPort", &outside.to_string()),
        ("NewProtocol", "UDP"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

fn add_mapping(g: &Igd, inside: u16, outside: u16, lease: u32) -> Option<Reply> {
    let mut args = mapping_key(outside);
    args.push(("NewInternalPort", inside.to_string()));
    args.push(("NewInternalClient", g.client_ip.to_string()));
    args.push(("NewEnabled", String::from("1")));
    args.push(("NewPortMappingDescription", String::from("Infinity world")));
    args.push(("NewLeaseDuration", lease.to_string()));
    soap(g, "AddPortMapping", &args)
}

/// Asks the router to drop a mapping this process made (best effort).
fn delete_mapping(g: &Igd, outside: u16) {
    let _ = soap(g, "DeletePortMapping", &mapping_key(outside));
}

fn external_ip(g: &Igd) -> Option<IpAddr> {
    let reply = soap(g, "GetExternalIPAddress", &[])?;
    if reply.status != 200 {
        return None;
    }
    element_text(&reply.body, "NewExternalIPAddress")?
        .parse()
        .ok()
}

/// Maps UDP `port` on the gateway: the same external port first, then up to four after it when
/// the router reports a conflict; a router that only takes permanent leases gets lease 0.
pub fn map_port(g: &Igd, port: u16) -> MapResult {
    let candidates = (0..PORT_TRIES).map_while(|step| port.checked_add(step));
    for outside in candidates {
        let mut lease = LEASE_DURATION_SECS;
        loop {
            let Some(reply) = add_mapping(g, port, outside, lease) else {
                return MapResult::Failed;
            };
            if reply.status == 200 {
                return external_ip(g).map_or(MapResult::Failed, |public_ip| MapResult::Mapped {
                    public_ip,
                    public_port: outside,
                    lease_secs: lease,
                });
            }
            match upnp_error_code(&reply.body) {
                Some(ERR_ONLY_PERMANENT_LEASES) if lease != 0 => lease = 0,
                Some(ERR_CONFLICT) => break,
                _ => return MapResult::Failed,
            }
        }
    }
    MapResult::Failed
}

/// Is `ip` an address no Internet host can reach directly: private, shared (CGNAT 100.64/10),
/// link-local, loopback or unspecified IPv4? A UPnP "external" address like that means another
/// NAT sits between the router and the Internet.
pub fn unroutable_external(ip: IpAddr) -> bool {
    let octets = match ip {
        IpAddr::V4(v4) => v4.octets(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(mapped) => mapped.octets(),
            None => return false,
        },
    };
    match octets {
        [0, 0, 0, 0] | [10 | 127, ..] | [169, 254, ..] | [192, 168, ..] => true,
        [172, second, ..] => (16..=31).contains(&second),
        [100, second, ..] => (64..=127).contains(&second),
        _ => false,
    }
}

/// Sleeps `total` in short slices; false when `keep_going` turned false meanwhile.
fn pause(total: Duration, keep_going: &dyn Fn() -> bool) -> bool {
    let until = Instant::now() + total;
    while let Some(left) = until
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
    {
        if !keep_going() {
            return false;
        }
        std::thread::sleep(left.min(STOP_POLL));
    }
    keep_going()
}

/// The mapping loop for `port` while `keep_going` holds: discover, map, report each result,
/// renew every [`RENEW_INTERVAL`] while mapped, retry after five minutes otherwise; a mapping
/// still held when it stops is deleted.
pub fn hold_mapping(port: u16, report: impl Fn(MapResult), keep_going: impl Fn() -> bool) {
    while keep_going() {
        match discover_igd(SEARCH_WAIT) {
            None => report(MapResult::NotFound),
            Some(gateway) => {
                let mut held: Option<u16> = None;
                loop {
                    if !keep_going() {
                        break;
                    }
                    let outcome = map_port(&gateway, port);
                    let mapped = match &outcome {
                        MapResult::Mapped { public_port, .. } => Some(*public_port),
                        _ => None,
                    };
                    report(outcome);
                    match mapped {
                        Some(p) => held = Some(p),
                        None => break,
                    }
                    if !pause(RENEW_INTERVAL, &keep_going) {
                        break;
                    }
                }
                if let Some(p) = held.filter(|_| !keep_going()) {
                    delete_mapping(&gateway, p);
                    return;
                }
            }
        }
        if !pause(RETRY_AFTER, &keep_going) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_with_brackets_queries_and_bad_bytes() {
        let u = Location::from_text("HTTP://[fe80::1]:49152/desc?x=1").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("fe80::1", 49152, "/desc?x=1")
        );
        assert_eq!(u.authority(), "[fe80::1]:49152");
        assert_eq!(
            Location::from_text("http://10.0.0.1?q").unwrap().path,
            "/?q"
        );
        assert!(Location::from_text("http://user@10.0.0.1/").is_none());
        assert!(Location::from_text("http://10.0.0.1/a#b").is_none());
        assert!(Location::from_text("http://10.0.0.1/a b").is_none());
        assert!(Location::from_text("http://10.0.0.1:0/").is_none());
        assert!(Location::from_text("http://10.0.0.1:/").is_none());
        let base = Location::from_text("http://10.0.0.1:80/x/y.xml").unwrap();
        assert_eq!(base.resolve("/abs").unwrap().path, "/abs");
        assert!(base.resolve("ftp://10.0.0.1/").is_none());
        assert!(base.resolve("a\r\nb").is_none());
    }

    #[test]
    fn scanner_handles_prefixes_nesting_comments_and_entities() {
        let xml = "<?xml version=\"1.0\"?><!-- <errorCode>1</errorCode> -->\
                   <s:Body><m:errorCode> 402 </m:errorCode><a><a>in</a></a><e/></s:Body>";
        assert_eq!(upnp_error_code(xml), Some(402));
        assert_eq!(elements(xml, "a"), vec!["<a>in</a>"]);
        assert_eq!(element_text(xml, "e").as_deref(), Some(""));
        assert_eq!(unescape("a&lt;&#65;&#x42;&bogus;&"), "a<AB&bogus;&");
        assert_eq!(element_text("<x>open", "x"), None);
    }

    #[test]
    fn description_with_only_ppp_and_a_relative_url_base_falls_back() {
        let xml = "<root><URLBase>not a url</URLBase><serviceList><service>\
                   <serviceType>URN:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
                   <controlURL>/ppp</controlURL></service></serviceList></root>";
        let loc = Location::from_text("http://192.168.0.1:5431/d.xml").unwrap();
        let (kind, url) = wan_service(xml, &loc).unwrap();
        assert_eq!(kind, WAN_PPP_1);
        assert_eq!(
            (url.host.as_str(), url.port, url.path.as_str()),
            ("192.168.0.1", 5431, "/ppp")
        );
    }

    #[test]
    fn soap_body_shape() {
        let b = soap_envelope(WAN_IP_1, "GetExternalIPAddress", &[]);
        assert!(b.starts_with("<?xml version=\"1.0\"?><s:Envelope "));
        assert!(b.contains("<s:Body><u:GetExternalIPAddress xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\"></u:GetExternalIPAddress></s:Body></s:Envelope>"));
    }

    #[test]
    fn http_framing_rules() {
        assert_eq!(
            split_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef"),
            Some((200, b"abc".to_vec()))
        );
        assert_eq!(
            split_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc"),
            None
        );
        assert_eq!(
            split_http_response(
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd"
            ),
            None
        );
        assert_eq!(
            split_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n"),
            None
        );
        assert_eq!(
            split_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab"),
            None
        );
        assert_eq!(split_http_response(b"ICY 200 OK\r\n\r\n"), None);
        assert!(declares_framing(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"
        ));
        assert!(!declares_framing(b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\n"));
    }

    #[test]
    fn router_addresses_stay_local() {
        for ip in [
            "192.168.1.1",
            "10.0.0.1",
            "127.0.0.1",
            "fe80::1",
            "fd00::1",
            "::1",
        ] {
            assert!(local_network(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["0.0.0.0", "8.8.8.8", "2001:db8::1", "::"] {
            assert!(!local_network(ip.parse().unwrap()), "{ip}");
        }
        assert!(unroutable_external("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!unroutable_external("fd00::1".parse().unwrap()));
    }

    #[test]
    fn a_stopped_loop_returns_without_reporting() {
        let reported = std::cell::Cell::new(0);
        hold_mapping(5101, |_| reported.set(reported.get() + 1), || false);
        assert_eq!(reported.get(), 0);
    }
}
