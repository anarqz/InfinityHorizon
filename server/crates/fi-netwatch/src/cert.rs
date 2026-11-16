//! Per-start ephemeral keys certified by pinned identity keys (MATCHMAKING_API.md §5d "Keys",
//! OD-160 §4). Both sides use this module: the relay signs its certificate with its node key and
//! each request with its session key; matchmaking signs its certificate with its relay-grant key
//! and each answer with its session key. Each side pins only the other's identity key.
//!
//! - Certificate messages: the domain line, then the fields, joined by `\n`, no trailing newline.
//! - Message signatures: Ed25519 by the session key over a prefix ‖ the body bytes exactly as sent
//!   (`fi_protocol::relay_watch::*_PREFIX`), carried base64 in `FI-Relay-Signature`.
//! - Every check is strict (`verify_strict`); any failure is the same `Invalid` (the platform's
//!   401 carries no body, the relay counts a failure and treats it as no answer).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use fi_protocol::relay_watch::{
    CERT_MAX_SECS, CLOCK_SKEW_SECS, MM_SESSION_DOMAIN, MmSessionCert, RELAY_SESSION_DOMAIN,
    RelaySessionCert,
};

/// Any certificate or signature failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid;

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid relay certificate or signature")
    }
}

impl std::error::Error for Invalid {}

/// How long a new certificate is valid (26 h).
pub const CERT_SECS: i64 = 26 * 3600;
/// `notBefore` is this far before the moment of signing.
pub const BACKDATE_SECS: i64 = 60;
/// A process re-keys this often (its certificate still verifies until its end).
pub const ROTATE_SECS: i64 = 24 * 3600;

/// Standard base64.
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// 32 bytes of standard base64, `None` otherwise.
pub fn key32(text: &str) -> Option<[u8; 32]> {
    STANDARD.decode(text).ok()?.try_into().ok()
}

fn sig64(text: &str) -> Option<Signature> {
    let bytes: [u8; 64] = STANDARD.decode(text).ok()?.try_into().ok()?;
    Some(Signature::from_bytes(&bytes))
}

/// The relay certificate's signed message.
pub fn relay_cert_message(c: &RelaySessionCert) -> String {
    format!(
        "{RELAY_SESSION_DOMAIN}\n{}\n{}\n{}\n{}\n{}",
        c.node_key, c.session_key, c.start_id, c.not_before, c.not_after
    )
}

/// Matchmaking's certificate's signed message.
pub fn mm_cert_message(c: &MmSessionCert) -> String {
    format!(
        "{MM_SESSION_DOMAIN}\n{}\n{}\n{}\n{}\n{}",
        c.kid, c.session_key, c.instance, c.not_before, c.not_after
    )
}

/// A relay certificate for `session` by `node`, valid from `now - 60 s` for 26 h.
pub fn relay_cert(
    node: &SigningKey,
    session: &VerifyingKey,
    start_id: &str,
    now: i64,
) -> RelaySessionCert {
    let mut c = RelaySessionCert {
        node_key: b64(node.verifying_key().as_bytes()),
        session_key: b64(session.as_bytes()),
        start_id: start_id.to_owned(),
        not_before: now - BACKDATE_SECS,
        not_after: now - BACKDATE_SECS + CERT_SECS,
        signature: String::new(),
    };
    c.signature = b64(&node.sign(relay_cert_message(&c).as_bytes()).to_bytes());
    c
}

/// Matchmaking's certificate for `session` by the relay-grant key `kid`.
pub fn mm_cert(
    kid: &str,
    signer: &SigningKey,
    session: &VerifyingKey,
    instance: &str,
    now: i64,
) -> MmSessionCert {
    let mut c = MmSessionCert {
        kid: kid.to_owned(),
        session_key: b64(session.as_bytes()),
        instance: instance.to_owned(),
        not_before: now - BACKDATE_SECS,
        not_after: now - BACKDATE_SECS + CERT_SECS,
        signature: String::new(),
    };
    c.signature = b64(&signer.sign(mm_cert_message(&c).as_bytes()).to_bytes());
    c
}

fn window_ok(not_before: i64, not_after: i64, now: i64) -> bool {
    not_after > not_before
        && not_after - not_before <= CERT_MAX_SECS
        && now >= not_before - CLOCK_SKEW_SECS
        && now <= not_after + CLOCK_SKEW_SECS
}

/// Checks a relay certificate against the pinned node key it names (`pinned` answers whether a
/// node key is in `FI_RELAYS`): the session key it certifies.
pub fn verify_relay_cert(
    c: &RelaySessionCert,
    pinned: impl Fn(&str) -> bool,
    now: i64,
) -> Result<VerifyingKey, Invalid> {
    if !pinned(&c.node_key) {
        return Err(Invalid);
    }
    let node =
        VerifyingKey::from_bytes(&key32(&c.node_key).ok_or(Invalid)?).map_err(|_| Invalid)?;
    let sig = sig64(&c.signature).ok_or(Invalid)?;
    node.verify_strict(relay_cert_message(c).as_bytes(), &sig)
        .map_err(|_| Invalid)?;
    if !window_ok(c.not_before, c.not_after, now) {
        return Err(Invalid);
    }
    VerifyingKey::from_bytes(&key32(&c.session_key).ok_or(Invalid)?).map_err(|_| Invalid)
}

/// Checks matchmaking's certificate against the pinned relay-grant key its `kid` names: the
/// session key it certifies.
pub fn verify_mm_cert(
    c: &MmSessionCert,
    pinned: impl Fn(&str) -> Option<VerifyingKey>,
    now: i64,
) -> Result<VerifyingKey, Invalid> {
    let issuer = pinned(&c.kid).ok_or(Invalid)?;
    let sig = sig64(&c.signature).ok_or(Invalid)?;
    issuer
        .verify_strict(mm_cert_message(c).as_bytes(), &sig)
        .map_err(|_| Invalid)?;
    if !window_ok(c.not_before, c.not_after, now) {
        return Err(Invalid);
    }
    VerifyingKey::from_bytes(&key32(&c.session_key).ok_or(Invalid)?).map_err(|_| Invalid)
}

fn message(prefix: &[u8], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(prefix.len() + body.len());
    m.extend_from_slice(prefix);
    m.extend_from_slice(body);
    m
}

/// The base64 signature of `prefix ‖ body` by `key`.
pub fn sign_body(key: &SigningKey, prefix: &[u8], body: &[u8]) -> String {
    b64(&key.sign(&message(prefix, body)).to_bytes())
}

/// Checks a header signature of `prefix ‖ body` under `key`.
pub fn verify_body(
    key: &VerifyingKey,
    prefix: &[u8],
    body: &[u8],
    header: &str,
) -> Result<(), Invalid> {
    let sig = sig64(header.trim()).ok_or(Invalid)?;
    key.verify_strict(&message(prefix, body), &sig)
        .map_err(|_| Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_protocol::relay_watch::{RELAY_REPORT_PREFIX, RELAY_WATCH_PREFIX};

    const NOW: i64 = 1_793_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn a_relay_certificate_verifies_only_under_its_pinned_node_key_and_window() {
        let (node, session) = (key(1), key(2));
        let c = relay_cert(
            &node,
            &session.verifying_key(),
            "0192f0c4-0000-7000-8000-000000000001",
            NOW,
        );
        let pinned_key = c.node_key.clone();
        let pinned = |k: &str| k == pinned_key;
        assert_eq!(
            verify_relay_cert(&c, pinned, NOW).unwrap(),
            session.verifying_key()
        );
        assert_eq!(c.not_after - c.not_before, CERT_SECS);
        // Golden message layout.
        assert_eq!(
            relay_cert_message(&c),
            format!(
                "fi-relay-session-v1\n{}\n{}\n0192f0c4-0000-7000-8000-000000000001\n{}\n{}",
                c.node_key,
                c.session_key,
                NOW - 60,
                NOW - 60 + CERT_SECS
            )
        );
        // Not pinned; another key's signature; a changed field; outside the window.
        assert_eq!(verify_relay_cert(&c, |_| false, NOW), Err(Invalid));
        let mut forged = c.clone();
        forged.session_key = b64(key(3).verifying_key().as_bytes());
        assert_eq!(verify_relay_cert(&forged, pinned, NOW), Err(Invalid));
        let mut other = relay_cert(&key(4), &session.verifying_key(), "x", NOW);
        other.node_key = c.node_key.clone();
        assert_eq!(verify_relay_cert(&other, pinned, NOW), Err(Invalid));
        assert!(verify_relay_cert(&c, pinned, c.not_after + 60).is_ok());
        assert_eq!(
            verify_relay_cert(&c, pinned, c.not_after + 61),
            Err(Invalid)
        );
        assert_eq!(
            verify_relay_cert(&c, pinned, c.not_before - 61),
            Err(Invalid)
        );
        // A validity past 26 h never verifies, even signed.
        let mut long = c.clone();
        long.not_after = long.not_before + CERT_MAX_SECS + 1;
        long.signature = b64(&node.sign(relay_cert_message(&long).as_bytes()).to_bytes());
        assert_eq!(verify_relay_cert(&long, pinned, NOW), Err(Invalid));
    }

    #[test]
    fn matchmaking_certificates_and_body_signatures() {
        let (grant, session) = (key(5), key(6));
        let c = mm_cert("rg-1", &grant, &session.verifying_key(), "i", NOW);
        let pinned = |kid: &str| (kid == "rg-1").then(|| grant.verifying_key());
        assert_eq!(
            verify_mm_cert(&c, pinned, NOW).unwrap(),
            session.verifying_key()
        );
        assert!(mm_cert_message(&c).starts_with("fi-mm-session-v1\nrg-1\n"));
        let mut renamed = c.clone();
        renamed.kid = "rg-2".into();
        assert_eq!(
            verify_mm_cert(&renamed, |_| Some(grant.verifying_key()), NOW),
            Err(Invalid)
        );
        assert_eq!(verify_mm_cert(&c, |_| None, NOW), Err(Invalid));
        // A body signature verifies only with its own prefix and the exact bytes.
        let body = br#"{"seq":1}"#;
        let h = sign_body(&session, RELAY_REPORT_PREFIX, body);
        assert!(verify_body(&session.verifying_key(), RELAY_REPORT_PREFIX, body, &h).is_ok());
        assert!(verify_body(&session.verifying_key(), RELAY_WATCH_PREFIX, body, &h).is_err());
        assert!(
            verify_body(
                &session.verifying_key(),
                RELAY_REPORT_PREFIX,
                br#"{"seq":2}"#,
                &h
            )
            .is_err()
        );
        assert!(verify_body(&key(7).verifying_key(), RELAY_REPORT_PREFIX, body, &h).is_err());
        assert!(
            verify_body(
                &session.verifying_key(),
                RELAY_REPORT_PREFIX,
                body,
                "garbage"
            )
            .is_err()
        );
    }
}
