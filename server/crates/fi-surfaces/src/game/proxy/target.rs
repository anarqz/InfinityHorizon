//! What a proxied request targets: the original URL (`X-Fh2-Original-Url`) against the rewritten
//! path `/{host}/{path}`, the transaction id of `X-TransactionId`, and the hook version policy of
//! `X-Fi-Hook` (GAME_PLATFORM.md §3a). Pure functions, no I/O.

use std::cmp::Ordering;
use uuid::Uuid;

/// The URL the game asked for, as the dispatcher reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Original {
    /// Lower case, without the port (`gameservices.fh2.forzamotorsport.net`).
    pub host: String,
    /// Starting with `/`, as sent (percent-encoding kept).
    pub path: String,
    /// Without `?`; `None` when absent or empty.
    pub query: Option<String>,
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Splits `host[:port]` and lowercases the host; `None` when it is not a plausible host name.
pub fn host_of(authority: &str) -> Option<String> {
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        Some(_) => return None,
        None => authority,
    };
    let host = host.to_ascii_lowercase();
    valid_host(&host).then_some(host)
}

/// Parses `http(s)://host[:port]/path[?query][#fragment]`. `None` for anything else.
pub fn parse_original(url: &str) -> Option<Original> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let rest = rest.split('#').next().unwrap_or_default();
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return None;
    }
    let host = host_of(authority)?;
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (tail, None),
    };
    let path = if path.is_empty() { "/" } else { path };
    Some(Original {
        host,
        path: path.to_owned(),
        query: query.filter(|q| !q.is_empty()).map(str::to_owned),
    })
}

/// The rewritten request path `/{host}/{path}` split into its host segment (as sent) and the
/// original path (starting with `/`). `None` when the first segment is not a host name (has no
/// dot): such a path is not a proxied request.
pub fn split_rewritten(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('/')?;
    let (host, tail) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    (host.contains('.') && !host.starts_with('.')).then_some((host, tail))
}

/// Percent-decodes `text` byte for byte (an invalid escape is kept as it is).
pub fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Whether the rewritten path agrees with the original URL: the same host (case and port
/// ignored) and the same path once both are percent-decoded (an empty path is `/`).
pub fn agrees(rewritten_host: &str, rewritten_path: &str, original: &Original) -> bool {
    let path = if rewritten_path.is_empty() {
        "/"
    } else {
        rewritten_path
    };
    host_of(rewritten_host).as_deref() == Some(original.host.as_str())
        && percent_decode(path) == percent_decode(&original.path)
}

/// `X-Fh2-Request-Id` of a proxied request (GAME_PLATFORM.md §3a): a UUID, or the Beta hook's
/// decimal counter of 1 to 20 ASCII digits. Returns the id as sent (trimmed); `None` for anything
/// else.
pub fn request_id(header: &str) -> Option<&str> {
    let id = header.trim();
    let counter = (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    (counter || Uuid::try_parse(id).is_ok()).then_some(id)
}

/// The event id of a fact derived from a proxied request: `UUIDv5(namespace = the play session
/// id, name = "req:" + Class.Method + ":" + the request id as sent)`. The hook's counter restarts
/// with every game process and a session is one game run, so the session scopes it: the same
/// request retried is the same fact, the same counter in a later session a new one; the method in
/// the name keeps a request id reused for another method from passing as a replay of it.
/// (`/game/v1` keeps §2.2's `UUIDv5(profile, request UUID bytes)`.)
pub fn fact_id(session: Uuid, method: &str, request_id: &str) -> Uuid {
    Uuid::new_v5(&session, format!("req:{method}:{request_id}").as_bytes())
}

/// `X-TransactionId` (32 hex digits; dashes and braces tolerated, as the Beta's key did) as the
/// 16 bytes of the guid Transaction.Reconcile names.
pub fn transaction_id(header: &str) -> Option<Uuid> {
    let digits: String = header
        .chars()
        .filter(|c| !matches!(c, '-' | '{' | '}'))
        .collect();
    if digits.len() != 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Uuid::try_parse(&digits).ok()
}

/// A hook version, `MAJOR.MINOR.PATCH[-pre][+build]` (`0.3.0+fh2online.60`), ordered by the numeric
/// triple and then by the last number of the build metadata (`fh2online.60` → 60; none → 0): the
/// fh2online build counts protocol changes, so `.59` is older than `.60` within one release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookVersion {
    core: [u64; 3],
    build: u64,
}

impl HookVersion {
    /// Parses a version; `None` when the core is not one to three dotted numbers.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (head, build) = match text.split_once('+') {
            Some((h, b)) => (h, Some(b)),
            None => (text, None),
        };
        let core_text = head.split('-').next().unwrap_or(head);
        let mut core = [0u64; 3];
        let mut parts = 0;
        for (i, part) in core_text.split('.').enumerate() {
            if i >= 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            core[i] = part.parse().ok()?;
            parts += 1;
        }
        if parts == 0 {
            return None;
        }
        let build = build
            .and_then(|b| b.rsplit('.').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        Some(Self { core, build })
    }
}

impl PartialOrd for HookVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HookVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then(self.build.cmp(&other.build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_urls_parse_with_case_ports_queries_and_fragments() {
        let o = parse_original(
            "https://gameservices.FH2.forzamotorsport.net/Services.FH2Release/o.xtsw",
        )
        .unwrap();
        assert_eq!(o.host, "gameservices.fh2.forzamotorsport.net");
        assert_eq!(o.path, "/Services.FH2Release/o.xtsw");
        assert_eq!(o.query, None);
        let o = parse_original("HTTP://Profile.XboxLive.com:443/users/me/profile/settings/people/People?settings=Gamertag,GameDisplayPicRaw#x").unwrap();
        assert_eq!(o.host, "profile.xboxlive.com");
        assert_eq!(o.path, "/users/me/profile/settings/people/People");
        assert_eq!(
            o.query.as_deref(),
            Some("settings=Gamertag,GameDisplayPicRaw")
        );
        let o = parse_original("http://gamerpic.fh2online?xuid=1").unwrap();
        assert_eq!((o.path.as_str(), o.query.as_deref()), ("/", Some("xuid=1")));
        for bad in [
            "",
            "gameservices.fh2.forzamotorsport.net/x",
            "ftp://a.b/x",
            "https:///x",
            "https://user@a.b/x",
            "https://a.b:x/y",
            "https://a b/x",
        ] {
            assert_eq!(parse_original(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_rewritten_path_must_agree_with_the_original() {
        let o = parse_original("https://userpresence.xboxlive.com/users/xuid(2535100000000001)/devices/current/titles/current").unwrap();
        let (h, p) = split_rewritten(
            "/UserPresence.XboxLive.com/users/xuid(2535100000000001)/devices/current/titles/current",
        )
        .unwrap();
        assert!(agrees(h, p, &o));
        let (h, p) =
            split_rewritten("/userpresence.xboxlive.com/users/xuid%282535100000000001%29/devices/current/titles/current").unwrap();
        assert!(agrees(h, p, &o), "percent-encoding is not a disagreement");
        let (h, p) = split_rewritten("/userpresence.xboxlive.com:443/users/xuid(2535100000000001)/devices/current/titles/current").unwrap();
        assert!(agrees(h, p, &o), "a port on the host segment");
        let (h, p) = split_rewritten(
            "/profile.xboxlive.com/users/xuid(2535100000000001)/devices/current/titles/current",
        )
        .unwrap();
        assert!(!agrees(h, p, &o), "another host");
        let (h, p) = split_rewritten("/userpresence.xboxlive.com/users/me").unwrap();
        assert!(!agrees(h, p, &o), "another path");
        let root = parse_original("http://gamerpic.fh2online?xuid=1").unwrap();
        let (h, p) = split_rewritten("/gamerpic.fh2online").unwrap();
        assert!(agrees(h, p, &root));
        assert_eq!(split_rewritten("/game/v1/session"), None);
        assert_eq!(split_rewritten("/health"), None);
        assert_eq!(split_rewritten("/.well-known/x"), None);
        assert_eq!(split_rewritten("no-slash.example"), None);
    }

    #[test]
    fn percent_decoding_keeps_invalid_escapes() {
        assert_eq!(percent_decode("a%20b%2Fc"), b"a b/c");
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
        assert_eq!(percent_decode("%41%"), b"A%");
    }

    #[test]
    fn request_ids_are_uuids_or_decimal_counters() {
        for ok in [
            "1",
            "42",
            "999",
            "12345678901234567890",
            " 7 ",
            "0192f0c4-0000-7000-8000-00000000000a",
            "0192F0C4000070008000000000000000",
        ] {
            assert_eq!(request_id(ok), Some(ok.trim()), "{ok:?}");
        }
        for bad in [
            "",
            "  ",
            "123456789012345678901",
            "-1",
            "1.0",
            "12a",
            "0x10",
            "١٢",
        ] {
            assert_eq!(request_id(bad), None, "{bad:?}");
        }
        let (s1, s2) = (Uuid::now_v7(), Uuid::now_v7());
        let all = "Forza.WebServices.OnlineProfile.UpdateAll";
        let tables = "Forza.WebServices.OnlineProfile.UpdateAllTables";
        assert_eq!(
            fact_id(s1, all, "17"),
            fact_id(s1, all, "17"),
            "a retry is the same fact"
        );
        assert_ne!(
            fact_id(s1, all, "17"),
            fact_id(s2, all, "17"),
            "a later session, a new fact"
        );
        assert_ne!(fact_id(s1, all, "17"), fact_id(s1, all, "18"));
        assert_ne!(
            fact_id(s1, all, "17"),
            fact_id(s1, tables, "17"),
            "the same id on another method, another fact"
        );
        assert_eq!(fact_id(s1, all, "17").get_version_num(), 5);
        assert_eq!(
            fact_id(s1, all, "17"),
            Uuid::new_v5(&s1, b"req:Forza.WebServices.OnlineProfile.UpdateAll:17")
        );
    }

    #[test]
    fn transaction_ids_are_32_hex_digits() {
        let id = transaction_id("0123456789ABCDEF0123456789abcdef").unwrap();
        assert_eq!(id.simple().to_string(), "0123456789abcdef0123456789abcdef");
        assert_eq!(
            transaction_id("{01234567-89ab-cdef-0123-456789abcdef}"),
            Some(id)
        );
        for bad in [
            "",
            "0123",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef00",
        ] {
            assert_eq!(transaction_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn hook_versions_order_by_core_then_fh2online_build() {
        let v = |t: &str| HookVersion::parse(t).unwrap();
        assert!(v("0.3.0+fh2online.59") < v("0.3.0+fh2online.60"));
        assert!(v("0.3.0+fh2online.60") >= v("0.3.0+fh2online.60"));
        assert!(v("0.3.1+fh2online.1") > v("0.3.0+fh2online.60"));
        assert!(v("0.4.0") > v("0.3.9+fh2online.99"));
        assert!(
            v("0.3.0") < v("0.3.0+fh2online.60"),
            "no build is the oldest build"
        );
        assert_eq!(v("0.3"), v("0.3.0"));
        assert_eq!(v("0.3.0-rc1+fh2online.60"), v("0.3.0+fh2online.60"));
        assert!(v("1.0.0") > v("0.99.99+x.1000"));
        for bad in [
            "",
            "x",
            "0.3.0.1",
            "0..1",
            "v0.3.0",
            "+fh2online.60",
            "0.3.a",
        ] {
            assert_eq!(HookVersion::parse(bad), None, "{bad}");
        }
    }
}
