//! Header names of the client APIs and the game plane (ACCOUNTS.md §1, §11; SAVES_API.md §4.3).
//! Custom headers use the project's `X-Fi-` prefix (and Beta's `X-Fh2-` names on the game plane).

/// One UUID per user action, reused by every retry.
pub const IDEMPOTENCY_KEY: &str = "idempotency-key";
/// Echoed request id.
pub const REQUEST_ID: &str = "x-request-id";
/// The hook's activity report on a game request (OD-121, GAME_PLATFORM.md §3a).
pub const FI_ACTIVITY: &str = "x-fi-activity";
/// The game plane's answer to an activity report (OD-121, GAME_PLATFORM.md §3a).
pub const FI_ACTIVITY_STATE: &str = "x-fi-activity-state";
/// `infinity-launcher/<version> (<windows|wine|linux>; deck=<0|1>)`.
pub const FI_CLIENT: &str = "x-fi-client";
/// The play session of a save write.
pub const FI_SESSION_ID: &str = "x-fi-session-id";
/// The fencing token of a save write.
pub const FI_FENCE: &str = "x-fi-fence";
/// `upload`, `resolve_keep_local`, `import_offline`, `fresh_start` (CR-S51),
/// `seed_from_profile` (CR-S52).
pub const FI_COMMIT_KIND: &str = "x-fi-commit-kind";
/// The source profile (its decimal XUID, one of the caller's own) of a seed check or a
/// `seed_from_profile` commit (CR-S52, SAVES_API.md §4.10).
pub const FI_SEED_SOURCE: &str = "x-fi-seed-source";
/// Content digest (hex) of a bundle.
pub const FI_CONTENT_SHA256: &str = "x-fi-content-sha256";
/// The revision a parked bundle was based on.
pub const FI_BASED_ON: &str = "x-fi-based-on";
/// `true` on a replayed commit.
pub const FI_REPLAYED: &str = "x-fi-replayed";
/// RFC 9530 digest of the body: `sha-256=:<base64>:`.
pub const REPR_DIGEST: &str = "repr-digest";
/// Portal server-to-server assertion (Google ID token of `fi-portal`).
pub const FI_CLIENT_ASSERTION: &str = "x-fi-client-assertion";
/// `X-Fi-Client-Ip`: the visitor's IP as the portal saw it, trusted only with a valid assertion.
pub const FI_CLIENT_IP: &str = "x-fi-client-ip";
/// Game plane: the session token, i.e. the game access ticket of GATE_API.md (not
/// `Authorization`, which the game's own requests use).
pub const FI_SESSION: &str = "x-fi-session";
/// Web: the Cloudflare Turnstile response token of a challenged write (CORE_API.md §1.3).
pub const FI_CHALLENGE: &str = "x-fi-challenge";
/// Internal: the play dashboard's shared key (DASHBOARD_API.md §1, `FI_DASHBOARD_KEY`).
pub const FI_DASHBOARD_KEY: &str = "x-fi-dashboard-key";
/// Game plane: the hook's `MOD_VERSION`.
pub const FI_HOOK: &str = "x-fi-hook";
/// Game plane (Beta names, kept): the rewritten original URL.
pub const FH2_ORIGINAL_URL: &str = "x-fh2-original-url";
/// Game plane (Beta names, kept): the hook's request id.
pub const FH2_REQUEST_ID: &str = "x-fh2-request-id";
/// Game plane (Beta names, kept): SHA-256 of `gamedbRC.slt`.
pub const FH2_CAR_DB: &str = "x-fh2-cardb";
/// Game plane (Beta names, kept): the game's language.
pub const FH2_LANG: &str = "x-fh2-lang";
/// The bundle media type (FH2's `FH2SAVE1`, every line but FM5's).
pub const FH2SAVE1_MEDIA_TYPE: &str = "application/x-fh2save1";
/// FM5's bundle media type (`FM5SAVE1`, an FM5 Noob career's line; CR-S105, SAVES_API.md §4.12).
pub const FM5SAVE1_MEDIA_TYPE: &str = "application/x-fm5save1";

/// `Repr-Digest` value for a SHA-256 given as 32 bytes.
pub fn repr_digest(sha256: &[u8; 32]) -> String {
    format!("sha-256=:{}:", base64_standard(sha256))
}

/// Parses `Repr-Digest`; returns the SHA-256 when the header carries one (other algorithms are
/// ignored, as RFC 9530 allows).
pub fn parse_repr_digest(value: &str) -> Option<[u8; 32]> {
    value.split(',').map(str::trim).find_map(|item| {
        let b64 = item.strip_prefix("sha-256=:")?.strip_suffix(':')?;
        let bytes = base64_standard_decode(b64)?;
        <[u8; 32]>::try_from(bytes.as_slice()).ok()
    })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_standard(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_standard_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = match c {
                b'=' if i >= 2 => {
                    pad += 1;
                    0
                }
                _ if pad > 0 => return None,
                _ => B64.iter().position(|&b| b == c)? as u32,
            };
            n = n << 6 | v;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_digest_round_trips() {
        // SHA-256("abc").
        let mut d = [0u8; 32];
        let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        for (i, b) in d.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let header = repr_digest(&d);
        assert_eq!(
            header,
            "sha-256=:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=:"
        );
        assert_eq!(parse_repr_digest(&header), Some(d));
        assert_eq!(
            parse_repr_digest(&format!("sha-512=:AAAA:, {header}")),
            Some(d)
        );
        assert_eq!(parse_repr_digest("sha-256=:not base64!:"), None);
        assert_eq!(parse_repr_digest("sha-256=:YWJj:"), None, "not 32 bytes");
    }
}
