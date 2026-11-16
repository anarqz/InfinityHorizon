//! The game access ticket (GATE_API.md §3, ADR 0014; ACCOUNTS.md §4.2 and §11 called it the
//! session token): PASETO `v4.public`, `aud = game`, claims `sid`, `sub` (profile id), `acc`
//! (account id), `xuid`, `game`, `pool`, `tier`, `mode`, `cdb`, `iat`, `exp`, `kid` in the footer;
//! minted only by the gate with the `FI_TOKEN_SIGNING_KEYS` ring, 10 minutes, never past the
//! session's `game_until`, refreshed on every heartbeat. Game surfaces verify it offline
//! (signature, `game`, `pool`, `exp`) with the public keys of `FI_TOKEN_VERIFY_KEYS`
//! ([`VerifyKeys`]) and check the session's liveness in Postgres (cached at most 60 s).
//! Built by workstream A of the local environment v0 (docs/server/LOCAL_ENV.md §6), on the same
//! primitives as [`crate::access`]; the `game` claim and the verify-only ring came with the gate
//! (platform v1). Tokens minted before the `game` claim existed parse as `fh2`.
//!
//! Verification is two steps so a game surface can answer in the contract's order
//! (GAME_PLATFORM.md §1): [`verify_signature`] (key, signature, audience, shape: anything wrong is
//! `AUTH.SESSION_EXPIRED`), then the caller compares [`Verified::game`] and [`Verified::pool`]
//! with its own ([`Verified::check_game`]; `GAME.WRONG_POOL`, before any other check), then
//! [`Verified::check_live`] (`exp`). A claim is never read before the signature over it verified.

use crate::keys::{KeyError, SigningKeys, valid_kid};
use crate::{b64, paseto};
use ed25519_dalek::VerifyingKey;
use fi_protocol::instant;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Longest lifetime of a ticket (ADR 0014: 10 minutes, renewed by the 60-second heartbeat).
pub const TTL_SECS: i64 = 600;
/// The game of a ticket minted before the `game` claim existed, and of [`issue`].
pub const DEFAULT_GAME: &str = "fh2";
/// Accepted clock difference for `iat` in the future.
pub const SKEW_SECS: i64 = 60;
/// The audience of every session token.
pub const AUDIENCE: &str = "game";

/// The claims, exactly (camel case is not used: PASETO claims are short names).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionClaims {
    /// Play session id.
    pub sid: String,
    /// Profile id.
    pub sub: String,
    /// Account id.
    pub acc: String,
    /// XUID, decimal string.
    pub xuid: String,
    /// The game the session was admitted for (ADR 0902); absent from tokens minted before the
    /// gate, which were all `fh2`.
    #[serde(default = "default_game")]
    pub game: String,
    /// The pool the session was admitted to.
    pub pool: String,
    /// The account's tier when the token was minted (`free` | `vip`).
    pub tier: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Allowed car database SHA-256 (hex), or empty when the session declared none.
    pub cdb: String,
    /// CR-S51 (GATE_API.md §3): `true` while the session holds its save line; absent means
    /// `false` (every ticket before CR-S51 and every session without a line), and `false` is not
    /// written, so such a ticket's claims are byte for byte the earlier ones.
    #[serde(default, skip_serializing_if = "is_false")]
    pub line: bool,
    /// `game`.
    pub aud: String,
    /// Issued at (wire instant).
    pub iat: String,
    /// Expires at (wire instant).
    pub exp: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_game() -> String {
    DEFAULT_GAME.to_owned()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Footer {
    kid: String,
}

/// What a token is minted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant<'a> {
    /// Play session id.
    pub session_id: &'a str,
    /// Profile id.
    pub profile_id: &'a str,
    /// Account id.
    pub account_id: &'a str,
    /// XUID.
    pub xuid: u64,
    /// Pool.
    pub pool: &'a str,
    /// Tier now.
    pub tier: &'a str,
    /// Mode.
    pub mode: &'a str,
    /// Car database digest (hex) or empty.
    pub car_db: &'a str,
    /// The session holds its save line (CR-S51, the claim `line`).
    pub line: bool,
    /// The session's game-plane end (Unix seconds): `exp` never passes it.
    pub game_until: i64,
}

/// A minted session token.
#[derive(Debug)]
pub struct Issued {
    /// The token (a bearer secret for the game plane).
    pub token: crate::Secret,
    /// `exp` in Unix seconds: `min(now + 10 min, game_until)`. At or before `now` once the game
    /// plane closed (the token is born expired, SAVES_API.md §3.2).
    pub expires_at: i64,
}

/// Why a token was refused (the game plane answers both `AUTH.SESSION_EXPIRED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// Malformed, unknown key, bad signature, wrong audience, impossible times.
    #[error("invalid session token")]
    Invalid,
    /// Past `exp`.
    #[error("expired session token")]
    Expired,
}

/// A verified ticket of another game than the surface serves (`GAME.WRONG_POOL {game}`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the ticket is for game {game}")]
pub struct WrongGame {
    /// The ticket's `game` claim (a verified claim: safe to echo).
    pub game: String,
}

/// A token whose signature verified (not yet checked against the clock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The claims.
    pub claims: SessionClaims,
    /// `iat` in Unix seconds.
    pub issued_at: i64,
    /// `exp` in Unix seconds.
    pub expires_at: i64,
}

impl Verified {
    /// The pool claim (compare it with the deployment's pool before [`Verified::check_live`]).
    pub fn pool(&self) -> &str {
        &self.claims.pool
    }

    /// The game claim (`fh2` for tokens minted before the claim existed).
    pub fn game(&self) -> &str {
        &self.claims.game
    }

    /// `Err` unless the ticket's game is one of `served` (the deployable's `FI_GAMES`).
    pub fn check_game<S: AsRef<str>>(&self, served: &[S]) -> Result<(), WrongGame> {
        if served.iter().any(|g| g.as_ref() == self.claims.game) {
            Ok(())
        } else {
            Err(WrongGame {
                game: self.claims.game.clone(),
            })
        }
    }

    /// `Expired` at or past `exp`, `Invalid` when issued in the future (beyond the skew).
    pub fn check_live(&self, now: i64) -> Result<(), SessionError> {
        if self.issued_at > now + SKEW_SECS {
            return Err(SessionError::Invalid);
        }
        if self.expires_at <= now {
            return Err(SessionError::Expired);
        }
        Ok(())
    }
}

/// Mints a token of game [`DEFAULT_GAME`] at `now` (Unix seconds) with the ring's current key;
/// the gate mints with [`issue_for_game`].
pub fn issue(keys: &SigningKeys, grant: &Grant<'_>, now: i64) -> Issued {
    issue_for_game(keys, grant, DEFAULT_GAME, now)
}

/// `exp` of a ticket minted at `now` for a session whose game plane ends at `game_until` (Unix
/// seconds): what the gate records as `play_session.ticket_exp` before it mints.
pub fn expiry(now: i64, game_until: i64) -> i64 {
    (now + TTL_SECS).min(game_until)
}

/// Mints a ticket of `game` at `now` (Unix seconds) with the ring's current key.
pub fn issue_for_game(keys: &SigningKeys, grant: &Grant<'_>, game: &str, now: i64) -> Issued {
    let (kid, key) = keys.current();
    let expires_at = expiry(now, grant.game_until);
    // A token minted past the game-plane end is born expired; `iat < exp` keeps it well formed.
    let issued_at = now.min(expires_at - 1);
    let claims = SessionClaims {
        sid: grant.session_id.to_owned(),
        sub: grant.profile_id.to_owned(),
        acc: grant.account_id.to_owned(),
        xuid: grant.xuid.to_string(),
        game: game.to_owned(),
        pool: grant.pool.to_owned(),
        tier: grant.tier.to_owned(),
        mode: grant.mode.to_owned(),
        cdb: grant.car_db.to_owned(),
        line: grant.line,
        aud: AUDIENCE.to_owned(),
        iat: instant::format_unix_secs(issued_at).unwrap_or_default(),
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

/// Public keys by `kid`: the minting ring itself ([`SigningKeys`]) or a verify-only ring
/// ([`VerifyKeys`]).
pub trait KeyRing {
    /// The public key named `kid`.
    fn public_key(&self, kid: &str) -> Option<VerifyingKey>;
}

impl KeyRing for SigningKeys {
    fn public_key(&self, kid: &str) -> Option<VerifyingKey> {
        self.verifying_key(kid)
    }
}

/// The public keys a game surface verifies tickets with (`FI_TOKEN_VERIFY_KEYS`:
/// `kid:base64(32-byte Ed25519 public key)`, comma separated; two keys during a rotation's
/// 30-day overlap). It holds no private key, so it can never mint.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyKeys {
    keys: Vec<(String, VerifyingKey)>,
}

impl fmt::Debug for VerifyKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kids: Vec<&str> = self.keys.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("VerifyKeys").field("kids", &kids).finish()
    }
}

impl VerifyKeys {
    /// The variable a game surface reads.
    pub const VARIABLE: &'static str = "FI_TOKEN_VERIFY_KEYS";

    /// Parses `kid:base64(public key),...`. The error names the variable, never a value.
    pub fn parse(variable: &'static str, text: &str) -> Result<Self, KeyError> {
        let err = |reason| KeyError { variable, reason };
        let mut keys: Vec<(String, VerifyingKey)> = Vec::new();
        for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (kid, public) = item
                .split_once(':')
                .ok_or(err("each key is kid:base64(32-byte Ed25519 public key)"))?;
            let kid = kid.trim();
            if !valid_kid(kid) {
                return Err(err("a kid is 1-32 lower-case letters, digits or dashes"));
            }
            let bytes: [u8; 32] = b64::std_decode(public.trim())
                .and_then(|b| b.try_into().ok())
                .ok_or(err("a public key is the standard base64 of 32 bytes"))?;
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|_| err("a public key is not an Ed25519 point"))?;
            if keys.iter().any(|(k, _)| k == kid) {
                return Err(err("a kid appears twice"));
            }
            keys.push((kid.to_owned(), key));
        }
        if keys.is_empty() {
            return Err(err("no key"));
        }
        Ok(Self { keys })
    }

    /// The public half of a minting ring (a deployable that mints and verifies, and tests).
    pub fn of(ring: &SigningKeys) -> Self {
        Self {
            keys: ring
                .public_keys()
                .into_iter()
                .filter_map(|(kid, b)| VerifyingKey::from_bytes(&b).ok().map(|k| (kid, k)))
                .collect(),
        }
    }

    /// `kid:base64,...`: the `FI_TOKEN_VERIFY_KEYS` value of a minting ring (public data).
    pub fn to_text(&self) -> String {
        self.keys
            .iter()
            .map(|(kid, k)| format!("{kid}:{}", b64::std_encode(k.as_bytes())))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The kids, in order.
    pub fn kids(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(k, _)| k.as_str())
    }
}

impl KeyRing for VerifyKeys {
    fn public_key(&self, kid: &str) -> Option<VerifyingKey> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, v)| *v)
    }
}

/// Verifies the signature (any key of the ring, by the footer's `kid`), the audience and the
/// shape of `token`; the clock is checked by [`Verified::check_live`].
pub fn verify_signature<R: KeyRing + ?Sized>(
    keys: &R,
    token: &str,
) -> Result<Verified, SessionError> {
    let footer = paseto::untrusted_footer(token).ok_or(SessionError::Invalid)?;
    let kid = serde_json::from_slice::<Footer>(&footer)
        .map_err(|_| SessionError::Invalid)?
        .kid;
    let key = keys.public_key(&kid).ok_or(SessionError::Invalid)?;
    let message = paseto::verify(&key, token, &footer, b"").map_err(|_| SessionError::Invalid)?;
    let claims: SessionClaims =
        serde_json::from_slice(&message).map_err(|_| SessionError::Invalid)?;
    if claims.aud != AUDIENCE {
        return Err(SessionError::Invalid);
    }
    let secs = |t: &str| instant::parse_unix_micros(t).map(|m| m.div_euclid(1_000_000));
    let (issued_at, expires_at) = (
        secs(&claims.iat).ok_or(SessionError::Invalid)?,
        secs(&claims.exp).ok_or(SessionError::Invalid)?,
    );
    if expires_at <= issued_at || expires_at - issued_at > TTL_SECS {
        return Err(SessionError::Invalid);
    }
    Ok(Verified {
        claims,
        issued_at,
        expires_at,
    })
}

/// [`verify_signature`] then [`Verified::check_live`] at `now`.
pub fn verify<R: KeyRing + ?Sized>(
    keys: &R,
    token: &str,
    now: i64,
) -> Result<Verified, SessionError> {
    let verified = verify_signature(keys, token)?;
    verified.check_live(now)?;
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{self, Audience};

    const NOW: i64 = 1_792_245_600;

    fn ring(spec: &[(&str, u8)]) -> SigningKeys {
        let text: Vec<String> = spec
            .iter()
            .map(|(kid, b)| format!("{kid}:{}", b64::std_encode(&[*b; 32])))
            .collect();
        SigningKeys::parse("FI_TOKEN_SIGNING_KEYS", &text.join(",")).unwrap()
    }

    fn grant(pool: &str, game_until: i64) -> Grant<'_> {
        Grant {
            session_id: "0192f0c4-0000-7000-8000-00000000000a",
            profile_id: "0192f0c4-0000-7000-8000-00000000000c",
            account_id: "0192f0c4-0000-7000-8000-00000000000b",
            xuid: 2_535_100_000_000_001,
            pool,
            tier: "vip",
            mode: "noob",
            car_db: "",
            line: false,
            game_until,
        }
    }

    #[test]
    fn the_line_claim_is_written_only_when_true_and_absent_reads_false() {
        let keys = ring(&[("st-1", 4)]);
        let without = issue(&keys, &grant("vip", NOW + 3_600), NOW);
        let v = verify(&keys, without.token.expose(), NOW).unwrap();
        assert!(!v.claims.line);
        let json = serde_json::to_value(&v.claims).unwrap();
        assert!(
            json.get("line").is_none(),
            "a ticket without a line is the old wire"
        );
        let with = issue(
            &keys,
            &Grant {
                line: true,
                ..grant("matchmaking", NOW + 3_600)
            },
            NOW,
        );
        let v = verify(&keys, with.token.expose(), NOW).unwrap();
        assert!(v.claims.line);
        assert_eq!(v.pool(), "matchmaking");
    }

    #[test]
    fn issue_and_verify_through_a_rotation() {
        let old = ring(&[("st-old", 1)]);
        let issued = issue(&old, &grant("vip", NOW + 3_600), NOW);
        assert_eq!(issued.expires_at, NOW + TTL_SECS);
        let rotated = ring(&[("st-new", 2), ("st-old", 1)]);
        let v = verify(&rotated, issued.token.expose(), NOW + 60).unwrap();
        assert_eq!(v.pool(), "vip");
        assert_eq!(v.claims.xuid, "2535100000000001");
        assert_eq!(v.claims.aud, "game");
        assert_eq!(v.claims.acc, "0192f0c4-0000-7000-8000-00000000000b");
    }

    #[test]
    fn never_past_game_until_and_born_expired_once_the_game_plane_closed() {
        let keys = ring(&[("st-1", 3)]);
        let short = issue(&keys, &grant("vip", NOW + 120), NOW);
        assert_eq!(short.expires_at, NOW + 120);
        assert!(verify(&keys, short.token.expose(), NOW + 119).is_ok());
        assert_eq!(
            verify(&keys, short.token.expose(), NOW + 120).err(),
            Some(SessionError::Expired)
        );
        let closed = issue(&keys, &grant("vip", NOW - 10), NOW);
        assert_eq!(closed.expires_at, NOW - 10);
        let v = verify_signature(&keys, closed.token.expose()).expect("well formed");
        assert_eq!(v.check_live(NOW), Err(SessionError::Expired));
    }

    #[test]
    fn wrong_key_wrong_audience_tampering_and_the_clock() {
        let keys = ring(&[("st-1", 3)]);
        let token = issue(&keys, &grant("free", NOW + 3_600), NOW);
        let other = ring(&[("st-1", 4)]);
        assert_eq!(
            verify(&other, token.token.expose(), NOW).err(),
            Some(SessionError::Invalid),
            "same kid, another key"
        );
        let unknown = ring(&[("st-2", 3)]);
        assert_eq!(
            verify(&unknown, token.token.expose(), NOW).err(),
            Some(SessionError::Invalid),
            "a kid the verifier does not hold"
        );
        assert_eq!(
            verify(&keys, &format!("{}x", token.token.expose()), NOW).err(),
            Some(SessionError::Invalid)
        );
        assert_eq!(
            verify(&keys, token.token.expose(), NOW + TTL_SECS).err(),
            Some(SessionError::Expired)
        );
        assert_eq!(
            verify(&keys, token.token.expose(), NOW - 3_600).err(),
            Some(SessionError::Invalid),
            "issued in the future"
        );
        // An access token of the same ring is not a session token (audience and claims differ).
        let access = access::issue(&keys, "acc", "fam", Audience::Launcher, "player", NOW);
        assert_eq!(
            verify(&keys, access.token.expose(), NOW).err(),
            Some(SessionError::Invalid)
        );
        // And a session token is not an access token.
        assert!(access::verify(&keys, token.token.expose(), Audience::Launcher, NOW).is_err());
    }

    #[test]
    fn the_pool_claim_is_read_only_after_the_signature_verified() {
        let keys = ring(&[("st-1", 3)]);
        // A forged token claiming another pool, signed by a key the game plane does not hold, is
        // refused as invalid (401), never as a wrong pool (403): no claim of an unverified token
        // is trusted, not even to choose the refusal.
        let forged = issue(&ring(&[("st-1", 9)]), &grant("free", NOW + 3_600), NOW);
        assert_eq!(
            verify_signature(&keys, forged.token.expose()).err(),
            Some(SessionError::Invalid)
        );
        // A genuine expired token of another pool: the pool is readable (the game plane answers
        // GAME.WRONG_POOL first), the clock check comes after.
        let expired = issue(&keys, &grant("free", NOW + 3_600), NOW - 2 * TTL_SECS);
        let v = verify_signature(&keys, expired.token.expose()).unwrap();
        assert_eq!(v.pool(), "free");
        assert_eq!(v.check_live(NOW), Err(SessionError::Expired));
    }

    #[test]
    fn tickets_last_ten_minutes_and_carry_their_game() {
        assert_eq!(TTL_SECS, 600);
        let keys = ring(&[("st-1", 3)]);
        let t = issue_for_game(&keys, &grant("matchmaking", NOW + 3_600), "fh2", NOW);
        assert_eq!(t.expires_at, NOW + 600);
        assert_eq!(t.expires_at, expiry(NOW, NOW + 3_600));
        let v = verify(&keys, t.token.expose(), NOW).unwrap();
        assert_eq!((v.game(), v.pool()), ("fh2", "matchmaking"));
        assert!(v.check_game(&["fh2"]).is_ok());
        // exp never passes gameUntil.
        let short = issue_for_game(&keys, &grant("vip", NOW + 30), "fh2", NOW);
        assert_eq!(short.expires_at, NOW + 30);
        // A ticket of another game is refused by a surface serving fh2, naming the game.
        let fh3 = issue_for_game(&keys, &grant("vip", NOW + 3_600), "fh3", NOW);
        let v = verify_signature(&keys, fh3.token.expose()).unwrap();
        assert_eq!(
            v.check_game(&["fh2"]),
            Err(WrongGame { game: "fh3".into() })
        );
        assert!(v.check_game(&["fh2", "fh3"]).is_ok());
        // `issue` keeps its signature and mints the default game.
        let legacy = issue(&keys, &grant("vip", NOW + 3_600), NOW);
        assert_eq!(
            verify(&keys, legacy.token.expose(), NOW).unwrap().game(),
            DEFAULT_GAME
        );
    }

    #[test]
    fn old_shape_tokens_without_a_game_still_verify_as_fh2() {
        let keys = ring(&[("st-1", 3)]);
        let (kid, key) = keys.current();
        let old = serde_json::json!({
            "sid": "0192f0c4-0000-7000-8000-00000000000a",
            "sub": "0192f0c4-0000-7000-8000-00000000000c",
            "acc": "0192f0c4-0000-7000-8000-00000000000b",
            "xuid": "2535100000000001", "pool": "vip", "tier": "vip", "mode": "noob", "cdb": "",
            "aud": "game",
            "iat": instant::format_unix_secs(NOW).unwrap(),
            "exp": instant::format_unix_secs(NOW + 300).unwrap(),
        });
        let footer = serde_json::to_vec(&serde_json::json!({ "kid": kid })).unwrap();
        let token = paseto::sign(key, &serde_json::to_vec(&old).unwrap(), &footer, b"");
        let v = verify(&keys, &token, NOW).unwrap();
        assert_eq!(v.game(), "fh2");
        // An unknown claim is still refused (the shape is exact).
        let mut extra = old.clone();
        extra["gen"] = serde_json::json!(1);
        let token = paseto::sign(key, &serde_json::to_vec(&extra).unwrap(), &footer, b"");
        assert_eq!(
            verify(&keys, &token, NOW).err(),
            Some(SessionError::Invalid)
        );
    }

    #[test]
    fn verify_only_rings_hold_public_keys_and_verify_minted_tickets() {
        let minting = ring(&[("st-new", 2), ("st-old", 1)]);
        let public = VerifyKeys::of(&minting);
        assert_eq!(public.kids().collect::<Vec<_>>(), ["st-new", "st-old"]);
        let parsed = VerifyKeys::parse(VerifyKeys::VARIABLE, &public.to_text()).unwrap();
        assert_eq!(parsed, public);
        let t = issue_for_game(&minting, &grant("vip", NOW + 3_600), "fh2", NOW);
        assert!(verify(&parsed, t.token.expose(), NOW).is_ok());
        // Another ring's public keys under the same kids refuse it.
        let other = VerifyKeys::of(&ring(&[("st-new", 7)]));
        assert_eq!(
            verify(&other, t.token.expose(), NOW).err(),
            Some(SessionError::Invalid)
        );
        assert!(!format!("{parsed:?}").contains(&public.to_text()));
        for bad in [
            "",
            "nokid",
            "ST:AAAA",
            "st-1:c2hvcnQ=",
            &format!(
                "st-1:{},st-1:{}",
                b64::std_encode(&[1; 32]),
                b64::std_encode(&[1; 32])
            ),
        ] {
            let e = VerifyKeys::parse(VerifyKeys::VARIABLE, bad).unwrap_err();
            assert_eq!(e.variable, "FI_TOKEN_VERIFY_KEYS");
        }
    }
}
