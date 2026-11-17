//! Offline verification of relay grants (MATCHMAKING_API.md §3, ADR 0014 §3): Ed25519 over
//! `"fi-relay-grant-v1\n"` ‖ the claims bytes exactly as signed, with the public keys of
//! `FI_RELAY_GRANT_VERIFY_KEYS` (`kid:base64(32-byte public key)`, comma separated, current
//! first; two during a rotation). The matchmaking service signs with `fi_auth::relay_grant`;
//! its tests verify what it signs with this module.
//!
//! The verifier never trusts a claim before the signature over it verified, except `kid`, which
//! only chooses the key.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, VerifyingKey};
use fi_protocol::matchmaking::{GRANT_TTL_SECS, RELAY_GRANT_PREFIX, RelayGrant, RelayGrantClaims};

/// Accepted clock difference for `iat` in the future.
pub const SKEW_SECS: i64 = 30;
/// Largest claims document (a real one is about 300 bytes).
pub const MAX_CLAIMS_BYTES: usize = 1024;

/// Why a grant was refused (the UDP side ignores the datagram either way).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// Not base64, not JSON, an unknown key, a bad signature, a wrong version or lifetime.
    #[error("invalid relay grant")]
    Invalid,
    /// Past `exp`.
    #[error("expired relay grant")]
    Expired,
}

/// A key configuration that cannot be used (names the variable, never a value).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{variable}: {reason}")]
pub struct KeyError {
    /// The variable.
    pub variable: &'static str,
    /// What is wrong.
    pub reason: &'static str,
}

/// The public keys a relay accepts.
#[derive(Clone, Debug)]
pub struct VerifyKeys {
    keys: Vec<(String, VerifyingKey)>,
}

impl VerifyKeys {
    /// From `(kid, public key)` pairs.
    pub fn new(keys: Vec<(String, [u8; 32])>) -> Result<Self, KeyError> {
        let err = |reason| KeyError {
            variable: "FI_RELAY_GRANT_VERIFY_KEYS",
            reason,
        };
        if keys.is_empty() {
            return Err(err("no key"));
        }
        let mut out = Vec::with_capacity(keys.len());
        for (kid, bytes) in keys {
            if kid.is_empty()
                || kid.len() > 32
                || !kid
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(err("a kid is 1-32 lower-case letters, digits or dashes"));
            }
            if out.iter().any(|(k, _)| k == &kid) {
                return Err(err("a kid appears twice"));
            }
            let key = VerifyingKey::from_bytes(&bytes).map_err(|_| err("not an Ed25519 key"))?;
            out.push((kid, key));
        }
        Ok(Self { keys: out })
    }

    /// Parses `kid:base64key,kid:base64key`.
    pub fn parse(variable: &'static str, text: &str) -> Result<Self, KeyError> {
        let mut keys = Vec::new();
        for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let bad = |reason| KeyError { variable, reason };
            let (kid, key) = item
                .split_once(':')
                .ok_or(bad("each key is kid:base64(32-byte public key)"))?;
            let key: [u8; 32] = STANDARD
                .decode(key.trim())
                .ok()
                .and_then(|k| k.try_into().ok())
                .ok_or(bad("a key is the standard base64 of 32 bytes"))?;
            keys.push((kid.trim().to_owned(), key));
        }
        Self::new(keys).map_err(|e| KeyError {
            variable,
            reason: e.reason,
        })
    }

    fn key(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, v)| v)
    }

    /// Verifies raw claims bytes and a signature at `now` (Unix seconds).
    pub fn verify_raw(
        &self,
        claims: &[u8],
        signature: &[u8; 64],
        now: i64,
    ) -> Result<RelayGrantClaims, GrantError> {
        if claims.len() > MAX_CLAIMS_BYTES {
            return Err(GrantError::Invalid);
        }
        // Only the key choice reads the unverified bytes.
        let unverified: serde_json::Value =
            serde_json::from_slice(claims).map_err(|_| GrantError::Invalid)?;
        let kid = unverified
            .get("kid")
            .and_then(|k| k.as_str())
            .ok_or(GrantError::Invalid)?;
        let key = self.key(kid).ok_or(GrantError::Invalid)?;
        let mut message = Vec::with_capacity(RELAY_GRANT_PREFIX.len() + claims.len());
        message.extend_from_slice(RELAY_GRANT_PREFIX);
        message.extend_from_slice(claims);
        key.verify_strict(&message, &Signature::from_bytes(signature))
            .map_err(|_| GrantError::Invalid)?;
        let claims: RelayGrantClaims =
            serde_json::from_slice(claims).map_err(|_| GrantError::Invalid)?;
        // OD-189 (WORLDS_OFFICIAL.md §2, §6): an official world never uses a relay, so the relay
        // never serves its grant.
        if claims.v != 1
            || claims.host.official().is_some()
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

    /// Verifies the HTTP wire form.
    pub fn verify(&self, grant: &RelayGrant, now: i64) -> Result<RelayGrantClaims, GrantError> {
        let claims = STANDARD
            .decode(&grant.claims)
            .map_err(|_| GrantError::Invalid)?;
        let signature: [u8; 64] = STANDARD
            .decode(&grant.signature)
            .ok()
            .and_then(|s| s.try_into().ok())
            .ok_or(GrantError::Invalid)?;
        self.verify_raw(&claims, &signature, now)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A signer for this crate's tests only (the real one is `fi_auth::relay_grant`).
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn keys(seed: u8) -> VerifyKeys {
        VerifyKeys::new(vec![("rg-1".into(), key(seed).verifying_key().to_bytes())]).unwrap()
    }

    pub fn claims(grant: &str, now: i64) -> RelayGrantClaims {
        claims_for(grant, "joiner", now)
    }

    pub fn claims_for(grant: &str, role: &str, now: i64) -> RelayGrantClaims {
        RelayGrantClaims {
            v: 1,
            kid: "rg-1".into(),
            grant: grant.into(),
            world: "0192f0c4-0000-7000-8000-0000000000aa".into(),
            sub: "0192f0c4-0000-7000-8000-0000000000bb".into(),
            xuid: "2535100000000002".into(),
            tier: "free".into(),
            host: fi_protocol::matchmaking::RelayGrantHost::Xuid("2535100000000001".into()),
            role: role.into(),
            iat: now,
            exp: now + 60,
            relay: None,
            sync: None,
            voice_blocked_until: None,
        }
    }

    pub fn sign(seed: u8, claims: &RelayGrantClaims) -> (Vec<u8>, [u8; 64]) {
        sign_bytes(seed, serde_json::to_vec(claims).unwrap())
    }

    /// Signs raw claims bytes (shapes the signer never emits).
    pub fn sign_bytes(seed: u8, bytes: Vec<u8>) -> (Vec<u8>, [u8; 64]) {
        let mut message = RELAY_GRANT_PREFIX.to_vec();
        message.extend_from_slice(&bytes);
        let sig = key(seed).sign(&message).to_bytes();
        (bytes, sig)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const NOW: i64 = 1_792_245_600;

    #[test]
    fn verifies_and_refuses_expired_tampered_and_unknown() {
        let keys = keys(7);
        let c = claims("0192f0c4-0000-7000-8000-000000000001", NOW);
        let (bytes, sig) = sign(7, &c);
        assert_eq!(keys.verify_raw(&bytes, &sig, NOW + 1).unwrap(), c);
        assert_eq!(
            keys.verify_raw(&bytes, &sig, NOW + 60),
            Err(GrantError::Expired)
        );
        for i in 0..bytes.len() {
            let mut t = bytes.clone();
            t[i] ^= 0x01;
            assert!(keys.verify_raw(&t, &sig, NOW).is_err(), "byte {i}");
        }
        let mut s = sig;
        s[10] ^= 0x80;
        assert_eq!(keys.verify_raw(&bytes, &s, NOW), Err(GrantError::Invalid));
        assert_eq!(
            super::testing::keys(8).verify_raw(&bytes, &sig, NOW),
            Err(GrantError::Invalid),
            "another key"
        );
        let mut long = claims("g", NOW);
        long.exp = NOW + 61;
        let (b, s) = sign(7, &long);
        assert_eq!(keys.verify_raw(&b, &s, NOW), Err(GrantError::Invalid));
        let future = claims("g", NOW + 120);
        let (b, s) = sign(7, &future);
        assert_eq!(keys.verify_raw(&b, &s, NOW), Err(GrantError::Invalid));
        // OD-189: an official world's grant, validly signed, is never a relay's.
        for role in ["joiner", "host"] {
            let mut official = claims_for("g", role, NOW);
            official.host = fi_protocol::matchmaking::RelayGrantHost::Official("local-mac".into());
            let (b, s) = sign(7, &official);
            assert_eq!(keys.verify_raw(&b, &s, NOW), Err(GrantError::Invalid));
        }
    }

    /// CR-L53 (MATCHMAKING_API.md §3.2, §5f): `sync` verifies; a malformed one reads as no
    /// sync; a duplicate key anywhere, validly signed, refuses the grant.
    #[test]
    fn the_sync_claim_and_duplicate_keys() {
        let keys = keys(7);
        let mut c = claims("g", NOW);
        c.sync = Some(vec![1, 2]);
        let (bytes, sig) = sign(7, &c);
        assert!(String::from_utf8_lossy(&bytes).contains(r#","sync":[1,2]}"#));
        assert_eq!(keys.verify_raw(&bytes, &sig, NOW).unwrap(), c);
        let text = String::from_utf8(sign(7, &claims("g", NOW)).0).unwrap();
        let body = text.strip_suffix('}').unwrap();
        for (sync, read) in [
            ("[2,1]", None),
            ("[0]", None),
            ("[]", None),
            ("[3]", Some(vec![3])),
        ] {
            let (b, s) = sign_bytes(7, format!(r#"{body},"sync":{sync}}}"#).into_bytes());
            assert_eq!(keys.verify_raw(&b, &s, NOW).unwrap().sync, read, "{sync}");
        }
        for dup in [
            format!(r#"{body},"sync":[1],"sync":[1]}}"#),
            format!(r#"{body},"sync":[{{"a":1,"a":1}}]}}"#),
            format!(r#"{body},"kid":"rg-1"}}"#),
            text.replace(
                r#""host":{"xuid":"2535100000000001"}"#,
                r#""host":{"xuid":"2535100000000001","xuid":"2535100000000001"}"#,
            ),
        ] {
            let (b, s) = sign_bytes(7, dup.clone().into_bytes());
            assert_eq!(
                keys.verify_raw(&b, &s, NOW),
                Err(GrantError::Invalid),
                "{dup}"
            );
        }
    }

    #[test]
    fn the_http_form_and_the_key_list() {
        let c = claims("g", NOW);
        let (bytes, sig) = sign(7, &c);
        let wire = RelayGrant {
            claims: STANDARD.encode(&bytes),
            signature: STANDARD.encode(sig),
            udp_proof: None,
        };
        let pk = STANDARD.encode(key(7).verifying_key().to_bytes());
        let ring = VerifyKeys::parse(
            "FI_RELAY_GRANT_VERIFY_KEYS",
            &format!(
                "rg-2:{}, rg-1:{pk}",
                STANDARD.encode(key(9).verifying_key().to_bytes())
            ),
        )
        .unwrap();
        assert_eq!(ring.verify(&wire, NOW).unwrap().grant, "g");
        for bad in [
            "",
            "rg-1",
            "RG:abc",
            &format!("rg-1:{pk},rg-1:{pk}"),
            "rg-1:c2hvcnQ=",
        ] {
            let e = VerifyKeys::parse("FI_RELAY_GRANT_VERIFY_KEYS", bad).unwrap_err();
            assert_eq!(e.variable, "FI_RELAY_GRANT_VERIFY_KEYS");
        }
    }
}
