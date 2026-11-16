//! MCP access tokens (ADR 0018 §2, CORE_API.md §6): PASETO `v4.public`, `aud = mcp`, one hour,
//! signed with the `FI_MCP_SIGNING_KEYS` ring (`kid` in the footer, every key of the ring
//! accepted). Claims: `sub` = account id, `role` at consent (`moderator` | `admin`), `scope`
//! (space separated, ⊆ `platform:read moderation:write`), `cid` = the `mcp_connection` id, `jti`,
//! `iat`, `exp`. The claim set is exact (unknown fields refused), so neither a launcher access
//! token nor a game access ticket ever parses as one, and the reverse.
//!
//! Revocation is the connection's: `/mcp` checks the `mcp_connection` row (not revoked, role
//! unchanged, account not banned) on every call (`fi_db::mcp::live_connection_in`).

use crate::keys::SigningKeys;
use crate::paseto;
use fi_protocol::instant;
use serde::{Deserialize, Serialize};

/// Lifetime of an MCP access token.
pub const TTL_SECS: i64 = 3_600;
/// Accepted clock difference for `iat` in the future.
pub const SKEW_SECS: i64 = 60;
/// The audience of every MCP access token.
pub const AUDIENCE: &str = "mcp";
/// Every scope a connection may hold.
pub const SCOPES: [&str; 2] = ["platform:read", "moderation:write"];

/// The claims, exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpClaims {
    /// Account id.
    pub sub: String,
    /// `moderator` | `admin` (the role at consent).
    pub role: String,
    /// Space-separated scopes.
    pub scope: String,
    /// The connection (`mcp_connection.id`).
    pub cid: String,
    /// Token id.
    pub jti: String,
    /// `mcp`.
    pub aud: String,
    /// Issued at (wire instant).
    pub iat: String,
    /// Expires at (wire instant).
    pub exp: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Footer {
    kid: String,
}

/// What a token is minted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant<'a> {
    /// Account id.
    pub account_id: &'a str,
    /// `moderator` | `admin`.
    pub role: &'a str,
    /// Scopes, each one of [`SCOPES`].
    pub scopes: &'a [&'a str],
    /// The connection id.
    pub connection_id: &'a str,
    /// A fresh token id.
    pub token_id: &'a str,
}

/// A minted token.
#[derive(Debug)]
pub struct Issued {
    /// The bearer (one hour).
    pub token: crate::Secret,
    /// `exp` in Unix seconds.
    pub expires_at: i64,
}

/// Why a token was refused (the `/mcp` answer is the same 401 for both).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpTokenError {
    /// Malformed, unknown key, bad signature, wrong audience or claim set, bad scope.
    #[error("invalid MCP access token")]
    Invalid,
    /// Past `exp`.
    #[error("expired MCP access token")]
    Expired,
}

/// What a verified token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Account id.
    pub account_id: String,
    /// Role at consent.
    pub role: String,
    /// Scopes.
    pub scopes: Vec<String>,
    /// Connection id.
    pub connection_id: String,
    /// `exp` in Unix seconds.
    pub expires_at: i64,
}

fn valid_scopes<'a>(scopes: impl IntoIterator<Item = &'a str>) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for s in scopes {
        if !SCOPES.contains(&s) || out.iter().any(|o| o == s) {
            return None;
        }
        out.push(s.to_owned());
    }
    (!out.is_empty()).then_some(out)
}

/// Mints a token at `now` (Unix seconds). `None` when the grant names an unknown or repeated
/// scope, no scope, or a role other than `moderator` | `admin`.
pub fn issue(keys: &SigningKeys, grant: &Grant<'_>, now: i64) -> Option<Issued> {
    let scopes = valid_scopes(grant.scopes.iter().copied())?;
    if !matches!(grant.role, "moderator" | "admin") {
        return None;
    }
    let (kid, key) = keys.current();
    let expires_at = now + TTL_SECS;
    let claims = McpClaims {
        sub: grant.account_id.to_owned(),
        role: grant.role.to_owned(),
        scope: scopes.join(" "),
        cid: grant.connection_id.to_owned(),
        jti: grant.token_id.to_owned(),
        aud: AUDIENCE.to_owned(),
        iat: instant::format_unix_secs(now)?,
        exp: instant::format_unix_secs(expires_at)?,
    };
    let message = serde_json::to_vec(&claims).ok()?;
    let footer = serde_json::to_vec(&Footer {
        kid: kid.to_owned(),
    })
    .ok()?;
    Some(Issued {
        token: crate::Secret::new(paseto::sign(key, &message, &footer, b"")),
        expires_at,
    })
}

/// Verifies `token` at `now` (Unix seconds): key, signature, the exact claim set, `aud = mcp`,
/// scopes, role, lifetime.
pub fn verify(keys: &SigningKeys, token: &str, now: i64) -> Result<Verified, McpTokenError> {
    let footer = paseto::untrusted_footer(token).ok_or(McpTokenError::Invalid)?;
    let kid = serde_json::from_slice::<Footer>(&footer)
        .map_err(|_| McpTokenError::Invalid)?
        .kid;
    let key = keys.verifying_key(&kid).ok_or(McpTokenError::Invalid)?;
    let message = paseto::verify(&key, token, &footer, b"").map_err(|_| McpTokenError::Invalid)?;
    let claims: McpClaims = serde_json::from_slice(&message).map_err(|_| McpTokenError::Invalid)?;
    if claims.aud != AUDIENCE || !matches!(claims.role.as_str(), "moderator" | "admin") {
        return Err(McpTokenError::Invalid);
    }
    let scopes = valid_scopes(claims.scope.split(' ')).ok_or(McpTokenError::Invalid)?;
    let secs = |t: &str| instant::parse_unix_micros(t).map(|m| m.div_euclid(1_000_000));
    let (iat, exp) = (
        secs(&claims.iat).ok_or(McpTokenError::Invalid)?,
        secs(&claims.exp).ok_or(McpTokenError::Invalid)?,
    );
    if exp <= iat || exp - iat > TTL_SECS || iat > now + SKEW_SECS {
        return Err(McpTokenError::Invalid);
    }
    if exp <= now {
        return Err(McpTokenError::Expired);
    }
    Ok(Verified {
        account_id: claims.sub,
        role: claims.role,
        scopes,
        connection_id: claims.cid,
        expires_at: exp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{self, Audience};
    use crate::b64;

    const NOW: i64 = 1_792_245_600;

    fn ring(spec: &[(&str, u8)]) -> SigningKeys {
        let text: Vec<String> = spec
            .iter()
            .map(|(kid, b)| format!("{kid}:{}", b64::std_encode(&[*b; 32])))
            .collect();
        SigningKeys::parse("FI_MCP_SIGNING_KEYS", &text.join(",")).unwrap()
    }

    fn grant<'a>(scopes: &'a [&'a str], role: &'a str) -> Grant<'a> {
        Grant {
            account_id: "acc",
            role,
            scopes,
            connection_id: "conn",
            token_id: "jti-1",
        }
    }

    #[test]
    fn issue_and_verify_through_a_rotation() {
        let old = ring(&[("mcp-old", 9)]);
        let t = issue(&old, &grant(&["platform:read"], "moderator"), NOW).unwrap();
        assert_eq!(t.expires_at, NOW + 3_600);
        let rotated = ring(&[("mcp-new", 10), ("mcp-old", 9)]);
        let v = verify(&rotated, t.token.expose(), NOW + 60).unwrap();
        assert_eq!(
            (
                v.account_id.as_str(),
                v.role.as_str(),
                v.connection_id.as_str()
            ),
            ("acc", "moderator", "conn")
        );
        assert_eq!(v.scopes, ["platform:read"]);
        assert_eq!(
            verify(&rotated, t.token.expose(), NOW + 3_600),
            Err(McpTokenError::Expired)
        );
        assert_eq!(
            verify(&ring(&[("mcp-new", 10)]), t.token.expose(), NOW),
            Err(McpTokenError::Invalid),
            "a key the verifier does not hold"
        );
        let both = issue(
            &rotated,
            &grant(&["platform:read", "moderation:write"], "admin"),
            NOW,
        )
        .unwrap();
        assert_eq!(
            verify(&rotated, both.token.expose(), NOW).unwrap().scopes,
            ["platform:read", "moderation:write"]
        );
    }

    #[test]
    fn grants_outside_the_scope_set_or_roles_are_never_minted() {
        let keys = ring(&[("mcp-1", 11)]);
        assert!(issue(&keys, &grant(&[], "admin"), NOW).is_none());
        assert!(issue(&keys, &grant(&["admin:all"], "admin"), NOW).is_none());
        assert!(
            issue(
                &keys,
                &grant(&["platform:read", "platform:read"], "admin"),
                NOW
            )
            .is_none()
        );
        assert!(issue(&keys, &grant(&["platform:read"], "player"), NOW).is_none());
    }

    #[test]
    fn other_token_kinds_never_verify_as_mcp_tokens_and_the_reverse() {
        let keys = ring(&[("k-1", 12)]);
        let launcher = access::issue(&keys, "acc", "fam", Audience::Launcher, "admin", NOW);
        assert_eq!(
            verify(&keys, launcher.token.expose(), NOW),
            Err(McpTokenError::Invalid),
            "same key ring, other claim set"
        );
        let mcp = issue(&keys, &grant(&["platform:read"], "admin"), NOW).unwrap();
        assert!(access::verify(&keys, mcp.token.expose(), Audience::Launcher, NOW).is_err());
        assert!(crate::session::verify_signature(&keys, mcp.token.expose()).is_err());
        assert_eq!(
            verify(&keys, &format!("{}x", mcp.token.expose()), NOW),
            Err(McpTokenError::Invalid)
        );
        assert_eq!(verify(&keys, "garbage", NOW), Err(McpTokenError::Invalid));
    }
}
