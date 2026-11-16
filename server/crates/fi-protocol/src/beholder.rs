//! The beholder, the announcement directory (ANNOUNCER_API.md, CR-S54): the signed announce body
//! nodes post (§2) and the listings launchers read (§3). Wire shapes only; the field rules and the
//! signature check live in the beholder (`services/beholder`) and the node announcer
//! (`crates/fi-announce`). The TypeScript mirror is `packages/contracts/src/beholder.ts`.
//!
//! `firstSeen` / `lastSeen` are RFC 3339 UTC strings of whole seconds without a fraction
//! (`2026-10-17T14:00:00Z`), the shape the deployed beholder serves; ANNOUNCER_API §3 only says
//! "RFC 3339 UTC", so it is not the six-digit [`crate::instant`] shape of the account plane.

use serde::{Deserialize, Serialize};

/// `kind` of a server (`endpoint` is `http(s)://host[:port]`).
pub const KIND_SERVER: &str = "server";
/// `kind` of a relay (`endpoint` is `host:port`, the UDP binding port).
pub const KIND_RELAY: &str = "relay";
/// The rendezvous port a client assumes when a relay listing carries no `rendezvousPort`
/// (ANNOUNCER_API §3, MATCHMAKING_API §5).
pub const DEFAULT_RENDEZVOUS_PORT: u16 = 5101;
/// The first line of the signed message (§2).
pub const ANNOUNCE_SIGNATURE_DOMAIN: &str = "fi-beholder-announce-v1";
/// The largest announce body the beholder reads (§2).
pub const MAX_ANNOUNCE_BYTES: usize = 4096;
/// The most items one list answer carries (§3).
pub const MAX_LISTING_ITEMS: usize = 500;

/// `POST /beholder/v1/announce` (§2). `kind` stays a string so a wrong word is refused by the
/// beholder's field rule (`REQUEST.INVALID {field: "kind"}`), not by the JSON parser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnnounceRequest {
    /// [`KIND_SERVER`] or [`KIND_RELAY`].
    pub kind: String,
    /// base64 (standard, padded) of the node's 32-byte Ed25519 public key.
    pub public_key: String,
    /// Where launchers reach the node; the shape depends on `kind`.
    pub endpoint: String,
    /// 1-48 characters, no control character.
    pub name: String,
    /// 0-16 of `[A-Za-z0-9-]`; absent reads as empty.
    #[serde(default)]
    pub region: String,
    /// 1-32 of `[A-Za-z0-9.+-]`.
    pub version: String,
    /// 1-1000.
    pub capacity: u32,
    /// Unix seconds, within 120 s of the beholder's clock.
    pub issued_at: i64,
    /// base64 of the 64-byte Ed25519 signature over [`AnnounceRequest::signed_message`].
    pub signature: String,
    /// A relay's UDP rendezvous port, 1-65535 (CR-S57 follow-up); never on a server. A `u32` so
    /// an out-of-range number is the beholder's `REQUEST.INVALID {field: "rendezvousPort"}`,
    /// not a parse error. Absent: not serialized, so older bodies keep their bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous_port: Option<u32>,
}

impl AnnounceRequest {
    /// The signed message of §2: the domain and the eight fields joined by `\n`, then
    /// `\n<rendezvousPort>` only when it is present, no trailing newline (`signature` itself is
    /// not part of it). Without `rendezvousPort` the message is the original v1 text.
    pub fn signed_message(&self) -> String {
        let mut m = format!(
            "{ANNOUNCE_SIGNATURE_DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.kind,
            self.public_key,
            self.endpoint,
            self.name,
            self.region,
            self.version,
            self.capacity,
            self.issued_at
        );
        if let Some(port) = self.rendezvous_port {
            m.push('\n');
            m.push_str(&port.to_string());
        }
        m
    }
}

/// One listed node of `GET /beholder/v1/servers` or `/relays` (§3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    /// The node's identity (base64 of 32 bytes).
    pub public_key: String,
    /// The announced name.
    pub name: String,
    /// The announced endpoint.
    pub endpoint: String,
    /// The announced region (may be empty).
    pub region: String,
    /// The announced version.
    pub version: String,
    /// The announced capacity (1-1000).
    pub capacity: u32,
    /// The first announce of this key as this kind (RFC 3339 UTC, whole seconds).
    pub first_seen: String,
    /// The latest announce (RFC 3339 UTC, whole seconds).
    pub last_seen: String,
    /// A relay's announced rendezvous port; absent on servers and on relays that did not announce
    /// one (clients then use [`DEFAULT_RENDEZVOUS_PORT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous_port: Option<u16>,
}

/// The list answer of §3: newest announce first, at most [`MAX_LISTING_ITEMS`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingPage {
    /// The listed nodes.
    pub items: Vec<Listing>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_announce_round_trips_and_refuses_unknown_fields() {
        let json = r#"{"kind":"relay","publicKey":"k","endpoint":"h:1","name":"n","version":"1","capacity":3,"issuedAt":9,"signature":"s"}"#;
        let a: AnnounceRequest = serde_json::from_str(json).unwrap();
        assert_eq!(a.region, "", "region defaults to empty");
        assert_eq!(
            a.signed_message(),
            "fi-beholder-announce-v1\nrelay\nk\nh:1\nn\n\n1\n3\n9"
        );
        let extra = json.replace("\"kind\"", "\"x\":1,\"kind\"");
        assert!(serde_json::from_str::<AnnounceRequest>(&extra).is_err());
        assert_eq!(a.rendezvous_port, None);
        assert!(
            !serde_json::to_string(&a)
                .unwrap()
                .contains("rendezvousPort")
        );
    }

    #[test]
    fn a_rendezvous_port_is_the_last_signed_line() {
        let json = r#"{"kind":"relay","publicKey":"k","endpoint":"h:1","name":"n","version":"1","capacity":3,"issuedAt":9,"signature":"s","rendezvousPort":5101}"#;
        let a: AnnounceRequest = serde_json::from_str(json).unwrap();
        assert_eq!(a.rendezvous_port, Some(5101));
        assert_eq!(
            a.signed_message(),
            "fi-beholder-announce-v1\nrelay\nk\nh:1\nn\n\n1\n3\n9\n5101"
        );
        let l: Listing = serde_json::from_str(
            r#"{"publicKey":"k","name":"n","endpoint":"h:1","region":"","version":"1","capacity":3,"firstSeen":"a","lastSeen":"b"}"#,
        )
        .unwrap();
        assert_eq!(l.rendezvous_port, None, "an older listing reads as absent");
    }
}
