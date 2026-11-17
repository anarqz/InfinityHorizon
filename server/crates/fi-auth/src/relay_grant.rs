//! Relay grants (MATCHMAKING_API.md §3, ADR 0014 §3, ADR 0017): what the matchmaking service hands
//! a joiner with a `granted` answer, and the host through `GET /mm/v1/punches`, for the relay VM
//! and the host's relay to check offline. `claims` are canonical JSON bytes
//! (`fi_protocol::matchmaking::RelayGrantClaims` in its field order, Unix seconds), signed with
//! Ed25519 over `"fi-relay-grant-v1\n"` ‖ claims by the current key of `relay-grant-signing-key`
//! (`FI_RELAY_GRANT_SIGNING_KEYS`, the [`SigningKeys`] format); at most 60 seconds
//! (`exp - iat ≤ 60`). The relay's verifier is `fi_relay::grant` (no server crate on the VM or in
//! the launcher); `crates/fi-relay/tests/signer.rs` checks it on what [`sign`] produces.
//!
//! [`PublicKeys`] is the verify-only ring of every offline verifier on the server side
//! (`kid:base64(32-byte Ed25519 public key)`): the matchmaking service's own check of a grant,
//! and, until the gate's `session.rs` lands its own, the game access ticket's verify keys
//! (`FI_TOKEN_VERIFY_KEYS`) on saves and matchmaking.

use crate::b64;
use crate::keys::{KeyError, SigningKeys};
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use fi_protocol::matchmaking::{
    GRANT_TTL_SECS, RELAY_GRANT_PREFIX, RelayGrant, RelayGrantClaims, SYNC_MAX,
};

/// Accepted clock difference for `iat` in the future.
pub const SKEW_SECS: i64 = 30;

/// Why a grant was refused (`MM.GRANT_INVALID`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// Malformed, unknown key, bad signature, wrong version or lifetime.
    #[error("invalid relay grant")]
    Invalid,
    /// Past `exp`.
    #[error("expired relay grant")]
    Expired,
    /// The claims cannot be signed (an `exp` not after `iat`, or more than 60 s later).
    #[error("relay grant claims out of bounds")]
    Bounds,
}

/// A verify-only Ed25519 ring: `(kid, public key)`, current first.
#[derive(Clone, Debug)]
pub struct PublicKeys {
    keys: Vec<(String, VerifyingKey)>,
}

impl crate::session::KeyRing for PublicKeys {
    fn public_key(&self, kid: &str) -> Option<VerifyingKey> {
        self.get(kid).copied()
    }
}

impl PublicKeys {
    /// Parses `kid:base64(32-byte public key)`, comma separated.
    pub fn parse(variable: &'static str, text: &str) -> Result<Self, KeyError> {
        let err = |reason| KeyError { variable, reason };
        let mut keys: Vec<(String, VerifyingKey)> = Vec::new();
        for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (kid, key) = item
                .split_once(':')
                .ok_or(err("each key is kid:base64(32-byte public key)"))?;
            let kid = kid.trim();
            if kid.is_empty()
                || kid.len() > 32
                || !kid
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(err("a kid is 1-32 lower-case letters, digits or dashes"));
            }
            if keys.iter().any(|(k, _)| k == kid) {
                return Err(err("a kid appears twice"));
            }
            let bytes: [u8; 32] = b64::std_decode(key.trim())
                .and_then(|k| k.try_into().ok())
                .ok_or(err("a key is the standard base64 of 32 bytes"))?;
            let key = VerifyingKey::from_bytes(&bytes).map_err(|_| err("not an Ed25519 key"))?;
            keys.push((kid.to_owned(), key));
        }
        if keys.is_empty() {
            return Err(err("no key"));
        }
        Ok(Self { keys })
    }

    /// The public halves of a signing ring (a service that signs also verifies its own).
    pub fn of(ring: &SigningKeys) -> Self {
        Self {
            keys: ring
                .public_keys()
                .into_iter()
                .filter_map(|(kid, bytes)| Some((kid, VerifyingKey::from_bytes(&bytes).ok()?)))
                .collect(),
        }
    }

    /// The key named `kid`.
    pub fn get(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, v)| v)
    }

    /// Every `(kid, public key)`, current first (what `/info`'s `trust` publishes, CR-S49).
    pub fn public_keys(&self) -> Vec<(String, [u8; 32])> {
        self.keys
            .iter()
            .map(|(k, v)| (k.clone(), v.to_bytes()))
            .collect()
    }

    /// `kid:base64` text of the ring (what a deployment's verify-keys variable holds).
    pub fn to_text(&self) -> String {
        self.keys
            .iter()
            .map(|(k, v)| format!("{k}:{}", b64::std_encode(v.as_bytes())))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Signs `claims` (its `kid` is set to the ring's current key) into the wire form.
pub fn sign(keys: &SigningKeys, mut claims: RelayGrantClaims) -> Result<RelayGrant, GrantError> {
    if claims.exp <= claims.iat
        || claims.exp - claims.iat > GRANT_TTL_SECS
        || !matches!(claims.role.as_str(), "joiner" | "host")
        || !sync_canonical(claims.sync.as_deref())
    {
        return Err(GrantError::Bounds);
    }
    let (kid, key) = keys.current();
    claims.kid = kid.to_owned();
    claims.v = 1;
    let bytes = serde_json::to_vec(&claims).map_err(|_| GrantError::Bounds)?;
    let mut message = RELAY_GRANT_PREFIX.to_vec();
    message.extend_from_slice(&bytes);
    let signature = key.sign(&message);
    Ok(RelayGrant {
        claims: b64::std_encode(&bytes),
        signature: b64::std_encode(&signature.to_bytes()),
        udp_proof: None,
    })
}

/// A `sync` the signer may emit (CR-L53, MATCHMAKING_API.md §5f): absent, empty (omitted), or
/// strictly ascending ids in 1..=255, at most [`SYNC_MAX`].
fn sync_canonical(sync: Option<&[u8]>) -> bool {
    sync.is_none_or(|ids| {
        ids.len() <= SYNC_MAX && !ids.contains(&0) && ids.windows(2).all(|w| w[0] < w[1])
    })
}

/// Verifies a grant with `keys` at `now` (Unix seconds).
pub fn verify(
    keys: &PublicKeys,
    grant: &RelayGrant,
    now: i64,
) -> Result<RelayGrantClaims, GrantError> {
    let bytes = b64::std_decode(&grant.claims).ok_or(GrantError::Invalid)?;
    let signature: [u8; 64] = b64::std_decode(&grant.signature)
        .and_then(|s| s.try_into().ok())
        .ok_or(GrantError::Invalid)?;
    let unverified: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| GrantError::Invalid)?;
    let kid = unverified
        .get("kid")
        .and_then(|k| k.as_str())
        .ok_or(GrantError::Invalid)?;
    let key = keys.get(kid).ok_or(GrantError::Invalid)?;
    let mut message = RELAY_GRANT_PREFIX.to_vec();
    message.extend_from_slice(&bytes);
    key.verify_strict(&message, &Signature::from_bytes(&signature))
        .map_err(|_| GrantError::Invalid)?;
    let claims: RelayGrantClaims =
        serde_json::from_slice(&bytes).map_err(|_| GrantError::Invalid)?;
    if claims.v != 1
        || !matches!(claims.role.as_str(), "joiner" | "host")
        || claims.exp <= claims.iat
        || claims.exp - claims.iat > GRANT_TTL_SECS
        || claims.iat > now + SKEW_SECS
    {
        return Err(GrantError::Invalid);
    }
    if claims.exp <= now {
        return Err(GrantError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_protocol::matchmaking::RelayGrantHost;

    const NOW: i64 = 1_792_245_600;

    fn ring(spec: &[(&str, u8)]) -> SigningKeys {
        let text: Vec<String> = spec
            .iter()
            .map(|(kid, b)| format!("{kid}:{}", b64::std_encode(&[*b; 32])))
            .collect();
        SigningKeys::parse("FI_RELAY_GRANT_SIGNING_KEYS", &text.join(",")).unwrap()
    }

    fn claims(iat: i64, exp: i64) -> RelayGrantClaims {
        RelayGrantClaims {
            v: 1,
            kid: String::new(),
            grant: "0192f0c4-0000-7000-8000-000000000001".into(),
            world: "0192f0c4-0000-7000-8000-0000000000aa".into(),
            sub: "0192f0c4-0000-7000-8000-0000000000bb".into(),
            xuid: "2535100000000002".into(),
            tier: "free".into(),
            host: RelayGrantHost::Xuid("2535100000000001".into()),
            role: "joiner".into(),
            iat,
            exp,
            relay: None,
            sync: None,
            voice_blocked_until: None,
        }
    }

    #[test]
    fn signs_verifies_with_the_public_key_and_refuses_expiry_and_tampering() {
        let keys = ring(&[("rg-1", 4)]);
        let grant = sign(&keys, claims(NOW, NOW + 60)).unwrap();
        let public = PublicKeys::parse("X", &PublicKeys::of(&keys).to_text()).unwrap();
        let v = verify(&public, &grant, NOW + 59).unwrap();
        assert_eq!((v.kid.as_str(), v.v), ("rg-1", 1));
        assert_eq!(verify(&public, &grant, NOW + 60), Err(GrantError::Expired));
        let bytes = b64::std_decode(&grant.claims).unwrap();
        for i in 0..bytes.len() {
            let mut t = bytes.clone();
            t[i] ^= 0x20;
            let tampered = RelayGrant {
                claims: b64::std_encode(&t),
                signature: grant.signature.clone(),
                udp_proof: None,
            };
            assert!(verify(&public, &tampered, NOW).is_err(), "byte {i}");
        }
        let other = PublicKeys::of(&ring(&[("rg-1", 5)]));
        assert_eq!(verify(&other, &grant, NOW), Err(GrantError::Invalid));
        assert_eq!(
            sign(&keys, claims(NOW, NOW + 61)).err(),
            Some(GrantError::Bounds)
        );
        assert_eq!(
            sign(&keys, claims(NOW, NOW)).err(),
            Some(GrantError::Bounds)
        );
    }

    /// CR-L53 (MATCHMAKING_API.md §3.2, §5f): the signer emits only a canonical `sync`, the
    /// verifier reads it back; a duplicate key anywhere, validly signed, refuses the grant.
    #[test]
    fn the_sync_claim_and_duplicate_keys() {
        let keys = ring(&[("rg-1", 4)]);
        let public = PublicKeys::of(&keys);
        let mut c = claims(NOW, NOW + 60);
        c.sync = Some(vec![1, 2]);
        let grant = sign(&keys, c).unwrap();
        assert_eq!(verify(&public, &grant, NOW).unwrap().sync, Some(vec![1, 2]));
        for bad in [vec![2, 1], vec![1, 1], vec![0], (1..=9).collect()] {
            let mut c = claims(NOW, NOW + 60);
            c.sync = Some(bad.clone());
            assert_eq!(sign(&keys, c).err(), Some(GrantError::Bounds), "{bad:?}");
        }
        let mut empty = claims(NOW, NOW + 60);
        empty.sync = Some(vec![]);
        let a = sign(&keys, empty).unwrap();
        let b = sign(&keys, claims(NOW, NOW + 60)).unwrap();
        assert_eq!(a.claims, b.claims, "an empty set keeps the bytes unchanged");
        let text = String::from_utf8(b64::std_decode(&b.claims).unwrap()).unwrap();
        let (kid, key) = keys.current();
        assert_eq!(kid, "rg-1");
        for dup in [
            text.replace(r#""exp":"#, r#""sync":[1],"sync":[1],"exp":"#),
            text.replace(r#""role":"joiner""#, r#""role":"joiner","role":"joiner""#),
        ] {
            let mut message = RELAY_GRANT_PREFIX.to_vec();
            message.extend_from_slice(dup.as_bytes());
            let signed = RelayGrant {
                claims: b64::std_encode(dup.as_bytes()),
                signature: b64::std_encode(&key.sign(&message).to_bytes()),
                udp_proof: None,
            };
            assert_eq!(
                verify(&public, &signed, NOW),
                Err(GrantError::Invalid),
                "{dup}"
            );
        }
    }

    #[test]
    fn rotation_and_the_public_ring_format() {
        let old = ring(&[("rg-old", 1)]);
        let grant = sign(&old, claims(NOW, NOW + 30)).unwrap();
        let rotated = PublicKeys::of(&ring(&[("rg-new", 2), ("rg-old", 1)]));
        assert!(verify(&rotated, &grant, NOW).is_ok());
        for bad in ["", "k", "K:AAAA", "k:c2hvcnQ="] {
            assert!(
                PublicKeys::parse("FI_TOKEN_VERIFY_KEYS", bad).is_err(),
                "{bad}"
            );
        }
        let text = PublicKeys::of(&old).to_text();
        assert!(
            PublicKeys::parse("X", &format!("{text},{text}")).is_err(),
            "a kid twice"
        );
        assert!(
            !text.contains(&b64::std_encode(&[1u8; 32])),
            "no seed in the public text"
        );
    }
}
