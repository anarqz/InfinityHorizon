// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The world agent's request signature (WORLDS_OFFICIAL.md §4.1, vectors §4.2):
//! `FI-World-Agent: v1 <vmId> <unix seconds> <base64 Ed25519 signature>` over
//! `"fi-world-agent-v1\n" ‖ method ‖ "\n" ‖ path-and-query ‖ "\n" ‖ seconds ‖ "\n" ‖
//! hex(SHA-256(body))`, no trailing newline, the signature standard base64 with padding.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::core::keys::encode_base64;

/// The header's name.
pub const HEADER: &str = "FI-World-Agent";

/// The signed message's first line (with its newline).
pub const DOMAIN: &str = "fi-world-agent-v1\n";

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

/// The exact bytes signed for one request. `target` is the path and query as sent (`/mm/v1/…`).
pub fn message(method: &str, target: &str, seconds: u64, body: &[u8]) -> Vec<u8> {
    let digest = hex(&Sha256::digest(body));
    format!("{DOMAIN}{method}\n{target}\n{seconds}\n{digest}").into_bytes()
}

/// The header value for one request.
pub fn header(
    key: &SigningKey,
    vm_id: &str,
    method: &str,
    target: &str,
    seconds: u64,
    body: &[u8],
) -> String {
    let signature = key.sign(&message(method, target, seconds, body));
    format!(
        "v1 {vm_id} {seconds} {}",
        encode_base64(&signature.to_bytes())
    )
}

/// The node key's public half as `FI_WORLDS.publicKey` lists it (standard base64, padded).
pub fn public_key(key: &SigningKey) -> String {
    encode_base64(key.verifying_key().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector_key() -> SigningKey {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn the_worlds_official_4_2_vectors() {
        let key = vector_key();
        assert_eq!(
            public_key(&key),
            "A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg="
        );
        // V1
        let m = message("GET", "/mm/v1/official/hello", 1_793_750_400, b"");
        assert_eq!(
            hex(&m),
            concat!(
                "66692d776f726c642d6167656e742d76310a4745540a2f6d6d2f76312f6f6666696369616c2f68656c6c6f0a313739333735303430300a",
                "65336230633434323938666331633134396166626634633839393666623932343237616534316534363439623933346361343935393931",
                "623738353262383535"
            )
        );
        assert_eq!(
            header(
                &key,
                "local-mac",
                "GET",
                "/mm/v1/official/hello",
                1_793_750_400,
                b""
            ),
            "v1 local-mac 1793750400 mAV+0LE5h39r4oF4WEhXo9Cpwv0L784vSs05hM2/vc9CuE/MBIrkFpzlH6xd04nUOaIrplacH2/sv5P+L547Cg=="
        );
        // V2
        let target = "/mm/v1/official/worlds/01900000-0000-7000-8000-000000000001";
        let body = br#"{"game":"fh2","mode":"noob","activity":"freeroam","maxPlayers":12}"#;
        assert_eq!(
            hex(&Sha256::digest(body)),
            "7698f80a5977dce20df7fdef24eb64b0c070fb170694fb009f437c28412125f6"
        );
        assert_eq!(
            hex(&message("PUT", target, 1_793_750_415, body)),
            concat!(
                "66692d776f726c642d6167656e742d76310a5055540a2f6d6d2f76312f6f6666696369616c2f776f726c64732f30313930303030302d3030",
                "30302d373030302d383030302d3030303030303030303030310a313739333735303431350a3736393866383061353937376463653230646637",
                "6664656632346562363462306330373066623137303639346662303039663433376332383431323132356636"
            )
        );
        assert_eq!(
            header(&key, "local-mac", "PUT", target, 1_793_750_415, body),
            "v1 local-mac 1793750415 OR7d/J4aZ1ksGXMhO2+rikYTJ360YjooL5e/q7ZC91Fd+/lvVTuUVhiighFKYiIWcZsoGy7lt5lcsnhlXxBBBw=="
        );
    }

    #[test]
    fn the_signature_verifies_strictly_and_binds_every_field() {
        let key = vector_key();
        let public = key.verifying_key();
        let h = header(
            &key,
            "local-mac",
            "DELETE",
            "/mm/v1/x?a=1",
            1_793_750_400,
            b"",
        );
        let sig_text = h.split(' ').nth(3).unwrap();
        assert_eq!(sig_text.len(), 88);
        let raw = crate::core::keys::decode_base64(sig_text).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&raw).unwrap();
        let signed = message("DELETE", "/mm/v1/x?a=1", 1_793_750_400, b"");
        assert!(public.verify_strict(&signed, &sig).is_ok());
        for other in [
            message("GET", "/mm/v1/x?a=1", 1_793_750_400, b""),
            message("DELETE", "/mm/v1/x", 1_793_750_400, b""),
            message("DELETE", "/mm/v1/x?a=1", 1_793_750_401, b""),
            message("DELETE", "/mm/v1/x?a=1", 1_793_750_400, b"{}"),
        ] {
            assert!(public.verify_strict(&other, &sig).is_err());
        }
    }
}
