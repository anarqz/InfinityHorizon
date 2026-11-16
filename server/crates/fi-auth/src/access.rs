//! Access tokens (ACCOUNTS.md §4.2): PASETO `v4.public`, 15 minutes, verified by any fi-api
//! instance offline with the public keys of `token-signing-key` (`kid` in the footer, every key of
//! the ring accepted, so a rotation never signs anybody out). Claims: `sub` = account id, `fam` =
//! refresh family, `aud` = `launcher` | `portal`, `role`, `iat`, `exp` (PASETO's registered
//! claims are RFC 3339 instants; ours are the six-digit wire form).
//!
//! Revocation is the family's (sign-out, reuse detection, deletion, bans): fi-api checks the
//! family and the account on each request, so a revoked family's access token dies at once, and
//! an unchecked one at most 15 minutes later.

use crate::keys::SigningKeys;
use crate::paseto;
use fi_protocol::instant;
use serde::{Deserialize, Serialize};

/// Lifetime of an access token.
pub const TTL_SECS: i64 = 900;
/// Accepted clock difference for `iat` in the future.
pub const SKEW_SECS: i64 = 60;

/// Who a token is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// `/launcher/v1`.
    Launcher,
    /// `/portal/v1` (the portal server).
    Portal,
}

impl Audience {
    /// Wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Audience::Launcher => "launcher",
            Audience::Portal => "portal",
        }
    }
}

/// The claims, exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessClaims {
    /// Account id.
    pub sub: String,
    /// Refresh family id.
    pub fam: String,
    /// `launcher` | `portal`.
    pub aud: String,
    /// `player` | `moderator`.
    pub role: String,
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

/// A minted access token.
#[derive(Debug)]
pub struct Issued {
    /// The token (a bearer secret for 15 minutes).
    pub token: crate::Secret,
    /// `exp` in Unix seconds.
    pub expires_at: i64,
}

/// Why a token was refused: `AUTH.TOKEN_INVALID` or `AUTH.TOKEN_EXPIRED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// Malformed, unknown key, bad signature, wrong audience.
    #[error("invalid access token")]
    Invalid,
    /// Past `exp`.
    #[error("expired access token")]
    Expired,
}

/// What a verified token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// Account id.
    pub account_id: String,
    /// Refresh family id.
    pub family_id: String,
    /// `player` | `moderator`.
    pub role: String,
    /// `exp` in Unix seconds.
    pub expires_at: i64,
}

/// Mints a token for `account_id` in `family_id` at `now` (Unix seconds).
pub fn issue(
    keys: &SigningKeys,
    account_id: &str,
    family_id: &str,
    audience: Audience,
    role: &str,
    now: i64,
) -> Issued {
    let (kid, key) = keys.current();
    let expires_at = now + TTL_SECS;
    let claims = AccessClaims {
        sub: account_id.to_owned(),
        fam: family_id.to_owned(),
        aud: audience.as_str().to_owned(),
        role: role.to_owned(),
        iat: instant::format_unix_secs(now).unwrap_or_default(),
        exp: instant::format_unix_secs(expires_at).unwrap_or_default(),
    };
    let message = serde_json::to_vec(&claims).unwrap_or_default();
    let footer = serde_json::to_vec(&Footer {
        kid: kid.to_owned(),
    })
    .unwrap_or_default();
    Issued {
        token: crate::Secret::new(paseto::sign(key, &message, &footer, b"")),
        expires_at,
    }
}

/// Verifies `token` for `audience` at `now` (Unix seconds).
pub fn verify(
    keys: &SigningKeys,
    token: &str,
    audience: Audience,
    now: i64,
) -> Result<Verified, AccessError> {
    let footer = paseto::untrusted_footer(token).ok_or(AccessError::Invalid)?;
    let kid = serde_json::from_slice::<Footer>(&footer)
        .map_err(|_| AccessError::Invalid)?
        .kid;
    let key = keys.verifying_key(&kid).ok_or(AccessError::Invalid)?;
    let message = paseto::verify(&key, token, &footer, b"").map_err(|_| AccessError::Invalid)?;
    let claims: AccessClaims =
        serde_json::from_slice(&message).map_err(|_| AccessError::Invalid)?;
    if claims.aud != audience.as_str() {
        return Err(AccessError::Invalid);
    }
    let secs = |t: &str| instant::parse_unix_micros(t).map(|m| m.div_euclid(1_000_000));
    let (iat, exp) = (
        secs(&claims.iat).ok_or(AccessError::Invalid)?,
        secs(&claims.exp).ok_or(AccessError::Invalid)?,
    );
    if exp <= iat || exp - iat > TTL_SECS || iat > now + SKEW_SECS {
        return Err(AccessError::Invalid);
    }
    if exp <= now {
        return Err(AccessError::Expired);
    }
    Ok(Verified {
        account_id: claims.sub,
        family_id: claims.fam,
        role: claims.role,
        expires_at: exp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::b64;

    const NOW: i64 = 1_792_245_600;

    fn ring(spec: &[(&str, u8)]) -> SigningKeys {
        let text: Vec<String> = spec
            .iter()
            .map(|(kid, b)| format!("{kid}:{}", b64::std_encode(&[*b; 32])))
            .collect();
        SigningKeys::parse("FI_TOKEN_SIGNING_KEYS", &text.join(",")).unwrap()
    }

    #[test]
    fn issue_and_verify_through_a_rotation() {
        let old = ring(&[("at-old", 1)]);
        let issued = issue(&old, "acc", "fam", Audience::Launcher, "player", NOW);
        assert_eq!(issued.expires_at, NOW + 900);
        // After a rotation the new ring signs with the new key and still accepts the old one.
        let rotated = ring(&[("at-new", 2), ("at-old", 1)]);
        let v = verify(
            &rotated,
            issued.token.expose(),
            Audience::Launcher,
            NOW + 60,
        )
        .unwrap();
        assert_eq!(
            (v.account_id.as_str(), v.family_id.as_str(), v.role.as_str()),
            ("acc", "fam", "player")
        );
        let fresh = issue(&rotated, "acc", "fam", Audience::Launcher, "player", NOW);
        assert!(verify(&rotated, fresh.token.expose(), Audience::Launcher, NOW).is_ok());
        assert_eq!(
            verify(&old, fresh.token.expose(), Audience::Launcher, NOW),
            Err(AccessError::Invalid),
            "a key the verifier does not hold"
        );
    }

    #[test]
    fn expiry_audience_and_tampering() {
        let keys = ring(&[("at-1", 3)]);
        let t = issue(&keys, "acc", "fam", Audience::Launcher, "player", NOW);
        let token = t.token.expose();
        assert_eq!(
            verify(&keys, token, Audience::Launcher, NOW + 900),
            Err(AccessError::Expired)
        );
        assert_eq!(
            verify(&keys, token, Audience::Portal, NOW),
            Err(AccessError::Invalid)
        );
        assert_eq!(
            verify(&keys, token, Audience::Launcher, NOW - 3_600),
            Err(AccessError::Invalid),
            "issued in the future"
        );
        assert_eq!(
            verify(&keys, &format!("{token}x"), Audience::Launcher, NOW),
            Err(AccessError::Invalid)
        );
        assert_eq!(
            verify(&keys, "fi_rt_abc", Audience::Launcher, NOW),
            Err(AccessError::Invalid)
        );
        // A token signed by the right key with a forged footer kid naming another key.
        let other = ring(&[("at-2", 4)]);
        let forged = issue(&other, "acc", "fam", Audience::Launcher, "player", NOW);
        assert_eq!(
            verify(&keys, forged.token.expose(), Audience::Launcher, NOW),
            Err(AccessError::Invalid)
        );
    }
}
