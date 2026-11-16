//! The relay as the anti-cheat detector (MATCHMAKING_API.md §5d, OD-159/OD-160, register CR-S80):
//! the relay → platform bodies of `POST /mm/v1/relay/watch` and `POST /mm/v1/relay/reports`, the
//! platform's answers and both sides' per-start certificates. Every body is signed as sent (header
//! [`SIGNATURE_HEADER`] over a prefix ‖ the body bytes); `fi_netwatch::cert` builds and checks the
//! signatures. Mirror: `packages/contracts/src/relay-watch.ts`.

use crate::matchmaking::WatchRules;
use serde::{Deserialize, Serialize};

/// The header carrying a relay request's or a platform answer's Ed25519 signature (base64).
pub const SIGNATURE_HEADER: &str = "FI-Relay-Signature";
/// First line of a relay's per-start certificate message.
pub const RELAY_SESSION_DOMAIN: &str = "fi-relay-session-v1";
/// First line of matchmaking's per-instance certificate message.
pub const MM_SESSION_DOMAIN: &str = "fi-mm-session-v1";
/// Prefix of a report batch's signature (relay session key).
pub const RELAY_REPORT_PREFIX: &[u8] = b"fi-relay-report-v1\n";
/// Prefix of a rules fetch's signature (relay session key).
pub const RELAY_WATCH_PREFIX: &[u8] = b"fi-relay-watch-v1\n";
/// Prefix of a report answer's signature (matchmaking session key).
pub const MM_REPORT_ANSWER_PREFIX: &[u8] = b"fi-mm-report-answer-v1\n";
/// Prefix of a rules answer's signature (matchmaking session key).
pub const MM_WATCH_ANSWER_PREFIX: &[u8] = b"fi-mm-watch-answer-v1\n";
/// Longest certificate validity, seconds (26 h).
pub const CERT_MAX_SECS: i64 = 93_600;
/// Accepted clock difference around a certificate's window and a request's `iat`, seconds.
pub const CLOCK_SKEW_SECS: i64 = 60;
/// Largest request body, bytes (413 past it).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Most reports in one batch.
pub const MAX_REPORTS: usize = 64;
/// The report types.
pub const REPORT_TYPES: [&str; 2] = ["violation", "forgery"];
/// The report kinds (`forgery`: speed or climb only).
pub const REPORT_KINDS: [&str; 3] = ["speed", "climb", "jump"];
/// What the platform stored a report as (`relay_report.outcome`).
pub const REPORT_OUTCOMES: [&str; 6] = [
    "rejected",
    "recorded",
    "would_flag",
    "would_flag_host",
    "flagged",
    "flagged_host",
];

/// The relay's per-start certificate, signed by its node key (the `publicKey` `FI_RELAYS` pins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelaySessionCert {
    /// The relay's node key, standard base64 of 32 bytes.
    pub node_key: String,
    /// The per-start ephemeral Ed25519 public key, base64 of 32 bytes.
    pub session_key: String,
    /// Minted at process start (UUID).
    pub start_id: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds; `not_after - not_before ≤` [`CERT_MAX_SECS`].
    pub not_after: i64,
    /// Base64 of 64 bytes, by the node key.
    pub signature: String,
}

/// Matchmaking's per-instance certificate, signed by its relay-grant key (`kid`, which the relay
/// pins in `FI_RELAY_GRANT_VERIFY_KEYS`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MmSessionCert {
    /// The relay-grant key id.
    pub kid: String,
    /// The per-instance ephemeral Ed25519 public key, base64 of 32 bytes.
    pub session_key: String,
    /// The instance (UUID).
    pub instance: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
    /// Base64 of 64 bytes, by the relay-grant key.
    pub signature: String,
}

/// `POST /mm/v1/relay/watch` body (header [`SIGNATURE_HEADER`] over [`RELAY_WATCH_PREFIX`] ‖ body).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayWatchRequest {
    /// `= cert.nodeKey`.
    pub relay: String,
    /// The relay's certificate.
    pub cert: RelaySessionCert,
    /// `= cert.startId`.
    pub start_id: String,
    /// Unix seconds.
    pub iat: i64,
    /// A fresh UUID; the answer echoes it.
    pub nonce: String,
}

/// → 200 (header over [`MM_WATCH_ANSWER_PREFIX`] ‖ body): the current rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayWatch {
    /// The request's.
    pub nonce: String,
    /// The same object `GET /mm/v1/watch` returns.
    pub rules: WatchRules,
    /// Matchmaking's certificate.
    pub cert: MmSessionCert,
}

/// One report (§5d "Reports").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayReport {
    /// Relay-minted, kept across retries: the idempotency key.
    pub id: String,
    /// Relay-minted once per episode; repeats share it.
    pub episode: String,
    /// One of [`REPORT_TYPES`].
    #[serde(rename = "type")]
    pub kind_type: String,
    /// One of [`REPORT_KINDS`].
    pub kind: String,
    /// The world (the grant's).
    pub world: String,
    /// Violation: the measured player; forgery: the player whose data was forged (decimal).
    pub xuid: String,
    /// The grant's host (decimal).
    pub host_xuid: String,
    /// The episode's first second, Unix ms (relay clock).
    pub from: i64,
    /// Its last second, Unix ms.
    pub to: i64,
    /// The rules version evaluated against.
    pub rules_version: i64,
    /// Speed (NR-5): the measured car's ordinal, when the relay decodes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car_id: Option<i64>,
    /// Violation: own peak; forgery: forwarded peak; jump: 0 (km/h).
    pub peak_kmh: f64,
    /// The run's length × 1000 (a lower bound).
    pub over_limit_ms: i64,
    /// Forgery: own maximum over `[from - 3 s, to + 3 s]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_peak_kmh: Option<f64>,
    /// Forgery: host → joiner legs that carried the forwarded run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legs: Option<i64>,
    /// Jump: the largest move, metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_jump_m: Option<f64>,
}

/// The relay's counters: totals since its start (`ringsInUse` a gauge); the platform keeps the
/// largest per start, so a retried or replayed batch never counts twice.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayCounters {
    /// Cars decoded.
    pub decoded: i64,
    /// Packets whose read stopped, and impossible positions.
    pub undecodable: i64,
    /// Foreign handles on a joiner's own leg.
    pub foreign_owner: i64,
    /// A joiner's own PlayerData naming another XUID, or an XUID claimed twice on a host leg.
    pub xuid_mismatch: i64,
    /// Subjects or grants without a ring.
    pub witness_full: i64,
    /// Evaluation steps without rules.
    pub rules_missing: i64,
    /// Kinds not evaluated (a sustain past 48 s).
    pub rule_unevaluable: i64,
    /// Reports dropped (a full outbox, a 400, too old).
    pub report_dropped: i64,
    /// Rings in use now.
    pub rings_in_use: i64,
}

/// `POST /mm/v1/relay/reports` body (header over [`RELAY_REPORT_PREFIX`] ‖ body).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayReportBatch {
    /// `= cert.nodeKey`.
    pub relay: String,
    /// The relay's certificate.
    pub cert: RelaySessionCert,
    /// `= cert.startId`.
    pub start_id: String,
    /// Batch counter since start (a retry gets a new one).
    pub seq: i64,
    /// Unix seconds.
    pub iat: i64,
    /// The rules version held (`null`: none yet).
    pub rules_version: Option<i64>,
    /// 0 to [`MAX_REPORTS`]; an empty batch carries counters only.
    pub reports: Vec<RelayReport>,
    /// Totals since this start.
    pub counters: RelayCounters,
}

/// → 200 (header over [`MM_REPORT_ANSWER_PREFIX`] ‖ body).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayReportAnswer {
    /// The batch's.
    pub seq: i64,
    /// The batch's `startId` (an answer from another start never verifies).
    pub start_id: String,
    /// Every report id now stored (including ids stored before).
    pub accepted: Vec<String>,
    /// The platform's current rules version.
    pub rules_version: i64,
    /// Matchmaking's certificate.
    pub cert: MmSessionCert,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> RelayReport {
        RelayReport {
            id: "i".into(),
            episode: "e".into(),
            kind_type: "forgery".into(),
            kind: "speed".into(),
            world: "w".into(),
            xuid: "1".into(),
            host_xuid: "2".into(),
            from: 1,
            to: 2,
            rules_version: 1,
            car_id: Some(2176),
            peak_kmh: 500.0,
            over_limit_ms: 3000,
            own_peak_kmh: Some(100.0),
            legs: Some(1),
            max_jump_m: Some(1.0),
        }
    }

    #[test]
    fn the_type_field_is_named_type_and_bodies_are_strict() {
        let json = serde_json::to_value(report()).unwrap();
        assert_eq!(json["type"], "forgery");
        assert_eq!(json["hostXuid"], "2");
        let mut r = report();
        r.own_peak_kmh = None;
        r.legs = None;
        r.max_jump_m = None;
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("ownPeakKmh").is_none() && json.get("legs").is_none());
        let mut v = serde_json::to_value(report()).unwrap();
        v["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<RelayReport>(v).is_err());
        // The six prefixes and the grant's all differ.
        let all: [&[u8]; 5] = [
            RELAY_REPORT_PREFIX,
            RELAY_WATCH_PREFIX,
            MM_REPORT_ANSWER_PREFIX,
            MM_WATCH_ANSWER_PREFIX,
            crate::matchmaking::RELAY_GRANT_PREFIX,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(!a.starts_with(b) && !b.starts_with(a));
            }
        }
    }
}
