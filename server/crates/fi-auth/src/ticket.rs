//! Launch tickets (ACCOUNTS.md §4.2, launcher ADR 0101): what the launcher hands the hook so it
//! can check, offline, that this launch was admitted. `claims` are canonical JSON bytes (the field
//! order below, snake_case, `xuid` a JSON number: the hook's fixed-width parser), signed with
//! Ed25519 over `"fi-launch-ticket-v1\n" ‖ claims` by the current key of `launch-ticket-signing-key`;
//! `kid` names the key so two can overlap for 30 days. At most 15 minutes (`exp - iat ≤ 900`).
//! The server never accepts a ticket as a credential.
//!
//! The hook's and the launcher's check is `fi_launcher_core::session::verify_ticket`; the tests
//! run it on what [`sign`] produces.

use crate::b64;
use crate::keys::SigningKeys;
use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};

/// Signed prefix.
pub const SIGN_PREFIX: &[u8] = b"fi-launch-ticket-v1\n";
/// Longest lifetime.
pub const MAX_LIFETIME_SECS: u64 = 900;

/// Version-1 claims, in canonical order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketClaims {
    /// 1.
    pub v: u32,
    /// Signing key id (set by [`sign`]).
    pub kid: String,
    /// Play session id.
    pub sid: String,
    /// Account id.
    pub account_id: String,
    /// Profile id.
    pub profile_id: String,
    /// `fh2`.
    pub game: String,
    /// The profile's XUID (a number here, below 2^53).
    pub xuid: u64,
    /// `Name#12345`.
    pub gamertag: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The pool's game-plane base URL.
    pub server: String,
    /// `vip` | `free` (internal id).
    pub tier: String,
    /// `vip` | `free` | `legacy-free`.
    pub pool: String,
    /// Hex of the launcher's 16-byte handoff nonce.
    pub nonce: String,
    /// Issued at (Unix seconds).
    pub iat: u64,
    /// Expires at (Unix seconds).
    pub exp: u64,
}

/// A signed ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTicket {
    /// The canonical claims bytes that were signed.
    pub claims: Vec<u8>,
    /// Ed25519 signature over [`SIGN_PREFIX`] ‖ `claims`.
    pub signature: [u8; 64],
}

impl SignedTicket {
    /// The wire form (standard base64 of both parts, SAVES_API.md §3.1).
    pub fn to_wire(&self) -> fi_protocol::saves::LaunchTicket {
        fi_protocol::saves::LaunchTicket {
            claims: b64::std_encode(&self.claims),
            signature: b64::std_encode(&self.signature),
        }
    }
}

/// Claims that cannot be signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    /// Not version 1, `exp ≤ iat`, longer than 15 minutes, a XUID at or above 2^53, or a nonce that
    /// is not 32 hex digits.
    #[error("launch ticket claims out of bounds")]
    Bounds,
}

/// Signs `claims` with the ring's current key (its kid replaces `claims.kid`).
pub fn sign(keys: &SigningKeys, mut claims: TicketClaims) -> Result<SignedTicket, TicketError> {
    let nonce_ok = claims.nonce.len() == 32 && claims.nonce.bytes().all(|b| b.is_ascii_hexdigit());
    if claims.v != 1
        || claims.exp <= claims.iat
        || claims.exp - claims.iat > MAX_LIFETIME_SECS
        || claims.xuid >= 1 << 53
        || !nonce_ok
    {
        return Err(TicketError::Bounds);
    }
    let (kid, key) = keys.current();
    claims.kid = kid.to_owned();
    let bytes = serde_json::to_vec(&claims).map_err(|_| TicketError::Bounds)?;
    let signature = key.sign(&[SIGN_PREFIX, &bytes].concat()).to_bytes();
    Ok(SignedTicket {
        claims: bytes,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_792_245_600;

    fn ring() -> SigningKeys {
        SigningKeys::parse(
            "FI_LAUNCH_TICKET_SIGNING_KEYS",
            &format!("lt-2026-10:{}", b64::std_encode(&[42u8; 32])),
        )
        .unwrap()
    }

    fn claims(nonce: &[u8; 16]) -> TicketClaims {
        TicketClaims {
            v: 1,
            kid: String::new(),
            sid: "0192f0c4-0000-7000-8000-00000000000a".into(),
            account_id: "0192f0c4-0000-7000-8000-00000000000b".into(),
            profile_id: "0192f0c4-0000-7000-8000-00000000000c".into(),
            game: "fh2".into(),
            xuid: 2_535_100_000_000_001,
            gamertag: "Speedy42#00731".into(),
            mode: "hardcore".into(),
            server: "http://127.0.0.1:8091".into(),
            tier: "free".into(),
            pool: "legacy-free".into(),
            nonce: crate::hex(nonce),
            iat: NOW,
            exp: NOW + 900,
        }
    }

    #[test]
    fn the_wire_form_and_the_claims_order_are_canonical() {
        let nonce = [7u8; 16];
        let keys = ring();
        let signed = sign(&keys, claims(&nonce)).unwrap();
        // The wire form decodes back to the same bytes.
        let wire = signed.to_wire();
        assert_eq!(b64::std_decode(&wire.claims).unwrap(), signed.claims);
        assert_eq!(b64::std_decode(&wire.signature).unwrap(), signed.signature);
        // Canonical order: the contract's example starts with v, kid, sid.
        let text = String::from_utf8(signed.claims).unwrap();
        assert!(
            text.starts_with(r#"{"v":1,"kid":"lt-2026-10","sid":"#),
            "{text}"
        );
        assert!(text.contains(r#""xuid":2535100000000001,"#));
    }

    #[test]
    fn out_of_bounds_claims_are_never_signed() {
        let keys = ring();
        let nonce = [1u8; 16];
        let mut long = claims(&nonce);
        long.exp = long.iat + 901;
        assert_eq!(sign(&keys, long), Err(TicketError::Bounds));
        let mut backwards = claims(&nonce);
        backwards.exp = backwards.iat;
        assert_eq!(sign(&keys, backwards), Err(TicketError::Bounds));
        let mut big = claims(&nonce);
        big.xuid = 1 << 53;
        assert_eq!(sign(&keys, big), Err(TicketError::Bounds));
        let mut nonce_bad = claims(&nonce);
        nonce_bad.nonce = "zz".into();
        assert_eq!(sign(&keys, nonce_bad), Err(TicketError::Bounds));
    }
}
