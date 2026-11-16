//! PASETO `v4.public` (Ed25519), as the PASETO specification defines it, written on ed25519-dalek
//! (ADR 0006: access and session tokens). It is small enough to own, and the specification's own
//! test vectors gate it, so any conforming library (patch B1 on the Beta server, a future service in
//! another language) verifies what this module signs.
//!
//! - Sign: `m2 = PAE("v4.public.", m, f, i)`, `sig = Ed25519(sk, m2)`, token
//!   `"v4.public." ‖ b64url(m ‖ sig)` and `"." ‖ b64url(f)` when the footer is not empty.
//! - Verify: the header, the footer (constant-time against the expected one), then the signature
//!   over the rebuilt PAE (strict Ed25519); only then is the message returned.
//!
//! PAE (pre-authentication encoding): `LE64(n)`, then for each piece `LE64(len) ‖ piece`, where
//! LE64 is little-endian with the top bit cleared.

use crate::{b64, ct_eq};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The header of every token this module reads or writes.
pub const HEADER: &str = "v4.public.";
const SIGNATURE_LEN: usize = 64;

/// Why a token was refused. Callers map every variant to one "invalid token" answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PasetoError {
    /// Not `v4.public.`, bad base64, or too short.
    #[error("not a v4.public token")]
    Malformed,
    /// The footer differs from the expected one.
    #[error("footer mismatch")]
    Footer,
    /// The signature does not verify.
    #[error("bad signature")]
    Signature,
}

fn le64(n: u64) -> [u8; 8] {
    (n & (u64::MAX >> 1)).to_le_bytes()
}

/// Pre-authentication encoding of `pieces`.
pub fn pae(pieces: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + pieces.iter().map(|p| 8 + p.len()).sum::<usize>());
    out.extend_from_slice(&le64(pieces.len() as u64));
    for piece in pieces {
        out.extend_from_slice(&le64(piece.len() as u64));
        out.extend_from_slice(piece);
    }
    out
}

/// Signs `message` with `footer` and implicit assertion `implicit`.
pub fn sign(key: &SigningKey, message: &[u8], footer: &[u8], implicit: &[u8]) -> String {
    let m2 = pae(&[HEADER.as_bytes(), message, footer, implicit]);
    let signature = key.sign(&m2);
    let mut body = Vec::with_capacity(message.len() + SIGNATURE_LEN);
    body.extend_from_slice(message);
    body.extend_from_slice(&signature.to_bytes());
    let mut token = format!("{HEADER}{}", b64::url_encode(&body));
    if !footer.is_empty() {
        token.push('.');
        token.push_str(&b64::url_encode(footer));
    }
    token
}

fn split(token: &str) -> Result<(&str, Option<&str>), PasetoError> {
    let rest = token.strip_prefix(HEADER).ok_or(PasetoError::Malformed)?;
    let mut parts = rest.split('.');
    let body = parts.next().ok_or(PasetoError::Malformed)?;
    let footer = parts.next();
    if parts.next().is_some() || footer == Some("") {
        return Err(PasetoError::Malformed);
    }
    Ok((body, footer))
}

/// The footer of a token, **unverified**: only to choose the key (`kid`) before [`verify`].
pub fn untrusted_footer(token: &str) -> Option<Vec<u8>> {
    let (_, footer) = split(token).ok()?;
    b64::url_decode(footer?)
}

/// Verifies `token` against `key`, the expected `footer` and the implicit assertion; returns the
/// message.
pub fn verify(
    key: &VerifyingKey,
    token: &str,
    footer: &[u8],
    implicit: &[u8],
) -> Result<Vec<u8>, PasetoError> {
    let (body, token_footer) = split(token)?;
    let token_footer = match token_footer {
        Some(f) => b64::url_decode(f).ok_or(PasetoError::Malformed)?,
        None => Vec::new(),
    };
    if !ct_eq(&token_footer, footer) {
        return Err(PasetoError::Footer);
    }
    let body = b64::url_decode(body).ok_or(PasetoError::Malformed)?;
    if body.len() < SIGNATURE_LEN {
        return Err(PasetoError::Malformed);
    }
    let (message, signature) = body.split_at(body.len() - SIGNATURE_LEN);
    let signature: [u8; SIGNATURE_LEN] =
        signature.try_into().map_err(|_| PasetoError::Malformed)?;
    let m2 = pae(&[HEADER.as_bytes(), message, footer, implicit]);
    key.verify_strict(&m2, &Signature::from_bytes(&signature))
        .map_err(|_| PasetoError::Signature)?;
    Ok(message.to_vec())
}

#[cfg(test)]
mod tests {
    //! The `v4.public` vectors of the PASETO specification's test suite (paseto-standard/
    //! test-vectors, `v4.json`): 4-S-1 to 4-S-3 must sign to the exact token and verify; 4-F-1 to
    //! 4-F-5 must be refused by a `v4.public` verifier.
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    const SEED: &str = "b4cbfb43df4ce210727d953e4a713307fa19bb7d9f85041438d9e11b942a3774";
    const PUBLIC: &str = "1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2";
    const PAYLOAD: &str =
        r#"{"data":"this is a signed message","exp":"2022-01-01T00:00:00+00:00"}"#;
    const FOOTER: &str = r#"{"kid":"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN"}"#;

    struct Vector {
        name: &'static str,
        token: &'static str,
        footer: &'static str,
        implicit: &'static str,
    }

    const VALID: [Vector; 3] = [
        Vector {
            name: "4-S-1",
            token: "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9bg_XBBzds8lTZShVlwwKSgeKpLT3yukTw6JUz3W4h_ExsQV-P0V54zemZDcAxFaSeef1QlXEFtkqxT1ciiQEDA",
            footer: "",
            implicit: "",
        },
        Vector {
            name: "4-S-2",
            token: "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9v3Jt8mx_TdM2ceTGoqwrh4yDFn0XsHvvV_D0DtwQxVrJEBMl0F2caAdgnpKlt4p7xBnx1HcO-SPo8FPp214HDw.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
            footer: FOOTER,
            implicit: "",
        },
        Vector {
            name: "4-S-3",
            token: "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9NPWciuD3d0o5eXJXG5pJy-DiVEoyPYWs1YSTwWHNJq6DZD3je5gf-0M4JR9ipdUSJbIovzmBECeaWmaqcaP0DQ.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
            footer: FOOTER,
            implicit: r#"{"test-vector":"4-S-3"}"#,
        },
    ];

    const INVALID: [Vector; 5] = [
        Vector {
            name: "4-F-1",
            token: "v4.local.vngXfCISbnKgiP6VWGuOSlYrFYU300fy9ijW33rznDYgxHNPwWluAY2Bgb0z54CUs6aYYkIJ-bOOOmJHPuX_34Agt_IPlNdGDpRdGNnBz2MpWJvB3cttheEc1uyCEYltj7wBQQYX.YXJiaXRyYXJ5LXN0cmluZy10aGF0LWlzbid0LWpzb24",
            footer: "arbitrary-string-that-isn't-json",
            implicit: r#"{"test-vector":"4-F-1"}"#,
        },
        Vector {
            name: "4-F-2",
            token: "v4.public.eyJpbnZhbGlkIjoidGhpcyBzaG91bGQgbmV2ZXIgZGVjb2RlIn22Sp4gjCaUw0c7EH84ZSm_jN_Qr41MrgLNu5LIBCzUr1pn3Z-Wukg9h3ceplWigpoHaTLcwxj0NsI1vjTh67YB.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
            footer: FOOTER,
            implicit: r#"{"test-vector":"4-F-2"}"#,
        },
        Vector {
            name: "4-F-3",
            token: "v3.local.23e_2PiqpQBPvRFKzB0zHhjmxK3sKo2grFZRRLM-U7L0a8uHxuF9RlVz3Ic6WmdUUWTxCaYycwWV1yM8gKbZB2JhygDMKvHQ7eBf8GtF0r3K0Q_gF1PXOxcOgztak1eD1dPe9rLVMSgR0nHJXeIGYVuVrVoLWQ.YXJiaXRyYXJ5LXN0cmluZy10aGF0LWlzbid0LWpzb24",
            footer: "arbitrary-string-that-isn't-json",
            implicit: r#"{"test-vector":"4-F-3"}"#,
        },
        Vector {
            name: "4-F-4",
            token: "v4.local.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQAr68PS4AXe7If_ZgesdkUMvSwscFlAl1pk5HC0e8kApeaqMfGo_7OpBnwJOAbY9V7WU6abu74MmcUE8YWAiaArVI8XJ5hOb_4v9RmDkneN0S92dx0OW4pgy7omxgf3S8c3LlQh",
            footer: "",
            implicit: "",
        },
        Vector {
            name: "4-F-5",
            token: "v4.local.32VIErrEkmY4JVILovbmfPXKW9wT1OdQepjMTC_MOtjA4kiqw7_tcaOM5GNEcnTxl60WkwMsYXw6FSNb_UdJPXjpzm0KW9ojM5f4O2mRvE2IcweP-PRdoHjd5-RHCiExR1IK6t4x-RMNXtQNbz7FvFZ_G-lFpk5RG3EOrwDL6CgDqcerSQ==.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
            footer: FOOTER,
            implicit: "",
        },
    ];

    fn keys() -> (SigningKey, VerifyingKey) {
        let seed: [u8; 32] = unhex(SEED).try_into().unwrap();
        let sk = SigningKey::from_bytes(&seed);
        let pk = VerifyingKey::from_bytes(&unhex(PUBLIC).try_into().unwrap()).unwrap();
        assert_eq!(sk.verifying_key(), pk, "the vectors' key pair");
        (sk, pk)
    }

    #[test]
    fn the_specification_vectors_sign_and_verify() {
        let (sk, pk) = keys();
        for v in &VALID {
            let token = sign(
                &sk,
                PAYLOAD.as_bytes(),
                v.footer.as_bytes(),
                v.implicit.as_bytes(),
            );
            assert_eq!(token, v.token, "{} signs to the vector's token", v.name);
            let message = verify(&pk, v.token, v.footer.as_bytes(), v.implicit.as_bytes())
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
            assert_eq!(message, PAYLOAD.as_bytes(), "{}", v.name);
            assert_eq!(
                untrusted_footer(v.token).unwrap_or_default(),
                v.footer.as_bytes()
            );
        }
    }

    #[test]
    fn the_specification_failures_are_refused() {
        let (_, pk) = keys();
        for v in &INVALID {
            assert!(
                verify(&pk, v.token, v.footer.as_bytes(), v.implicit.as_bytes()).is_err(),
                "{} must fail",
                v.name
            );
        }
    }

    #[test]
    fn any_change_is_refused() {
        let (sk, pk) = keys();
        let v = &VALID[2];
        let footer = v.footer.as_bytes();
        let implicit = v.implicit.as_bytes();
        assert_eq!(
            verify(&pk, v.token, footer, b"other"),
            Err(PasetoError::Signature),
            "implicit assertion is bound"
        );
        assert_eq!(
            verify(&pk, v.token, b"{\"kid\":\"x\"}", implicit),
            Err(PasetoError::Footer)
        );
        let other = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        assert_eq!(
            verify(&other, v.token, footer, implicit),
            Err(PasetoError::Signature)
        );
        // Flip one bit of the message inside the body.
        let token = sign(&sk, b"{\"a\":1}", footer, implicit);
        let (body, rest) = token[HEADER.len()..].split_once('.').unwrap();
        let mut bytes = b64::url_decode(body).unwrap();
        bytes[0] ^= 1;
        let tampered = format!("{HEADER}{}.{rest}", b64::url_encode(&bytes));
        assert_eq!(
            verify(&pk, &tampered, footer, implicit),
            Err(PasetoError::Signature)
        );
        assert_eq!(
            verify(&pk, &format!("{token}."), footer, implicit),
            Err(PasetoError::Malformed)
        );
        assert_eq!(
            verify(&pk, "v4.public.AAAA", b"", b""),
            Err(PasetoError::Malformed)
        );
    }

    #[test]
    fn pae_matches_the_specification_examples() {
        assert_eq!(pae(&[]), b"\x00\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(
            pae(&[b""]),
            b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
        );
        assert_eq!(
            pae(&[b"test"]),
            b"\x01\x00\x00\x00\x00\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00test"
        );
    }
}
