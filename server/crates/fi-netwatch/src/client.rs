//! The relay's side of the two platform calls (MATCHMAKING_API.md §5d "Reports", "The rules"),
//! without I/O: the per-start [`Session`] (an ephemeral key certified by the node key), signed
//! request bodies, and the checks of the platform's signed answers. fi-relay-vm sends the bytes;
//! matchmaking's tests build their requests with the same code.

use crate::cert::{self, Invalid};
use crate::detect::{Counters, Finding, Rules};
use ed25519_dalek::{SigningKey, VerifyingKey};
use fi_protocol::matchmaking::WatchRules;
use fi_protocol::relay_watch::{
    MM_REPORT_ANSWER_PREFIX, MM_WATCH_ANSWER_PREFIX, RELAY_REPORT_PREFIX, RELAY_WATCH_PREFIX,
    RelayCounters, RelayReport, RelayReportAnswer, RelayReportBatch, RelaySessionCert, RelayWatch,
    RelayWatchRequest,
};

/// A UUID's canonical text.
pub fn uuid_text(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// 16 random bytes as a version-4 UUID.
pub fn uuid4(mut random: [u8; 16]) -> [u8; 16] {
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    random
}

/// The relay's per-start session: its ephemeral key, its certificate and the start id.
pub struct Session {
    key: SigningKey,
    /// The certificate (node key, session key, start id, window).
    pub cert: RelaySessionCert,
    /// When it was made (Unix seconds); re-key after [`cert::ROTATE_SECS`].
    pub made_at: i64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("start_id", &self.cert.start_id)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// A session for `start_id` with the ephemeral seed `seed` (random, never stored), certified
    /// by `node`.
    pub fn new(node: &SigningKey, seed: [u8; 32], start_id: &str, now: i64) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let cert = cert::relay_cert(node, &key.verifying_key(), start_id, now);
        Self {
            key,
            cert,
            made_at: now,
        }
    }

    /// Whether it is time to re-key (the start id stays).
    pub fn due(&self, now: i64) -> bool {
        now - self.made_at >= cert::ROTATE_SECS
    }

    /// A report batch's body and its `FI-Relay-Signature`.
    pub fn batch(
        &self,
        seq: i64,
        iat: i64,
        rules_version: Option<i64>,
        reports: Vec<RelayReport>,
        counters: RelayCounters,
    ) -> (Vec<u8>, String) {
        let body = RelayReportBatch {
            relay: self.cert.node_key.clone(),
            cert: self.cert.clone(),
            start_id: self.cert.start_id.clone(),
            seq,
            iat,
            rules_version,
            reports,
            counters,
        };
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        let sig = cert::sign_body(&self.key, RELAY_REPORT_PREFIX, &bytes);
        (bytes, sig)
    }

    /// A rules fetch's body and its signature.
    pub fn watch(&self, iat: i64, nonce: &str) -> (Vec<u8>, String) {
        let body = RelayWatchRequest {
            relay: self.cert.node_key.clone(),
            cert: self.cert.clone(),
            start_id: self.cert.start_id.clone(),
            iat,
            nonce: nonce.to_owned(),
        };
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        let sig = cert::sign_body(&self.key, RELAY_WATCH_PREFIX, &bytes);
        (bytes, sig)
    }
}

/// A finding on the wire.
pub fn report(f: &Finding) -> RelayReport {
    RelayReport {
        id: uuid_text(&f.id),
        episode: uuid_text(&f.episode),
        kind_type: f.kind_type.as_str().to_owned(),
        kind: f.kind.as_str().to_owned(),
        world: uuid_text(&f.world),
        xuid: f.xuid.to_string(),
        host_xuid: f.host_xuid.to_string(),
        from: f.from_ms as i64,
        to: f.to_ms as i64,
        rules_version: f.rules_version,
        // NR-5: the detector does not decode the car yet.
        car_id: None,
        peak_kmh: (f.peak_kmh * 10.0).round() / 10.0,
        over_limit_ms: f.over_limit_ms as i64,
        own_peak_kmh: f.own_peak_kmh.map(|v| (v * 10.0).round() / 10.0),
        legs: f.legs.map(i64::from),
        max_jump_m: f.max_jump_m.map(|v| (v * 10.0).round() / 10.0),
    }
}

/// The counters since `base` (the last accepted batch's), with `dropped` reports.
pub fn counters(now: &Counters, base: &Counters, dropped: u64) -> RelayCounters {
    let d = |a: u64, b: u64| a.saturating_sub(b).min(i64::MAX as u64) as i64;
    RelayCounters {
        decoded: d(now.decoded, base.decoded),
        undecodable: d(now.undecodable, base.undecodable),
        foreign_owner: d(now.foreign_owner, base.foreign_owner),
        xuid_mismatch: d(now.xuid_mismatch, base.xuid_mismatch),
        witness_full: d(now.witness_full, base.witness_full),
        rules_missing: d(now.rules_missing, base.rules_missing),
        rule_unevaluable: d(now.rule_unevaluable, base.rule_unevaluable),
        report_dropped: dropped.min(i64::MAX as u64) as i64,
        rings_in_use: now.rings_in_use.min(i64::MAX as u64) as i64,
    }
}

/// `WatchRules` as the detector holds them.
pub fn rules(w: &WatchRules) -> Rules {
    Rules {
        version: w.version,
        speed: crate::detect::Limit {
            max_kmh: w.speed.max_kmh,
            sustain_ms: w.speed.sustain_ms,
        },
        climb: crate::detect::Limit {
            max_kmh: w.climb.max_kmh,
            sustain_ms: w.climb.sustain_ms,
        },
        jump_m: w.jump.max_meters,
    }
}

/// Checks a report answer: the certificate under a pinned relay-grant key, the header under its
/// session key, the batch's `seq` and `startId`.
pub fn verify_report_answer(
    body: &[u8],
    header: &str,
    seq: i64,
    start_id: &str,
    pinned: impl Fn(&str) -> Option<VerifyingKey>,
    now: i64,
) -> Result<RelayReportAnswer, Invalid> {
    let answer: RelayReportAnswer = serde_json::from_slice(body).map_err(|_| Invalid)?;
    let session = cert::verify_mm_cert(&answer.cert, pinned, now)?;
    cert::verify_body(&session, MM_REPORT_ANSWER_PREFIX, body, header)?;
    if answer.seq != seq || answer.start_id != start_id {
        return Err(Invalid);
    }
    Ok(answer)
}

/// Checks a rules answer: as [`verify_report_answer`], with the request's `nonce`.
pub fn verify_watch_answer(
    body: &[u8],
    header: &str,
    nonce: &str,
    pinned: impl Fn(&str) -> Option<VerifyingKey>,
    now: i64,
) -> Result<RelayWatch, Invalid> {
    let answer: RelayWatch = serde_json::from_slice(body).map_err(|_| Invalid)?;
    let session = cert::verify_mm_cert(&answer.cert, pinned, now)?;
    cert::verify_body(&session, MM_WATCH_ANSWER_PREFIX, body, header)?;
    if answer.nonce != nonce {
        return Err(Invalid);
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{FindingType, Kind};
    use fi_protocol::matchmaking::{WatchJump, WatchLimit, WatchSpeed};
    use fi_protocol::relay_watch::MmSessionCert;

    const NOW: i64 = 1_793_000_000;
    const START: &str = "0192f0c4-0000-4000-8000-000000000001";

    fn mm(seed: u8) -> (SigningKey, SigningKey, MmSessionCert) {
        let (grant, session) = (
            SigningKey::from_bytes(&[seed; 32]),
            SigningKey::from_bytes(&[seed + 1; 32]),
        );
        let c = cert::mm_cert("rg-1", &grant, &session.verifying_key(), "inst", NOW);
        (grant, session, c)
    }

    #[test]
    fn uuids_are_canonical_v4() {
        let u = uuid4([0xab; 16]);
        assert_eq!(uuid_text(&u), "abababab-abab-4bab-abab-abababababab");
    }

    #[test]
    fn a_batch_is_signed_as_sent_and_an_answer_is_checked() {
        let node = SigningKey::from_bytes(&[1; 32]);
        let s = Session::new(&node, [2; 32], "0192f0c4-0000-4000-8000-000000000001", NOW);
        let f = Finding {
            id: [1; 16],
            episode: [2; 16],
            kind_type: FindingType::Forgery,
            kind: Kind::Speed,
            world: [3; 16],
            xuid: 11,
            host_xuid: 22,
            from_ms: 1,
            to_ms: 2,
            rules_version: 1,
            peak_kmh: 499.96,
            over_limit_ms: 3000,
            own_peak_kmh: Some(100.04),
            legs: Some(1),
            max_jump_m: None,
        };
        let (body, sig) = s.batch(7, NOW, Some(1), vec![report(&f)], RelayCounters::default());
        let parsed: RelayReportBatch = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.reports[0].peak_kmh, 500.0);
        assert_eq!(parsed.reports[0].kind_type, "forgery");
        let pinned_node = parsed.cert.node_key.clone();
        let key = cert::verify_relay_cert(&parsed.cert, |k| k == pinned_node, NOW).unwrap();
        cert::verify_body(&key, RELAY_REPORT_PREFIX, &body, &sig).unwrap();
        // The answer.
        let (grant, session, c) = mm(5);
        let answer = RelayReportAnswer {
            seq: 7,
            start_id: START.into(),
            accepted: vec![report(&f).id],
            rules_version: 1,
            cert: c,
        };
        let bytes = serde_json::to_vec(&answer).unwrap();
        let h = cert::sign_body(&session, MM_REPORT_ANSWER_PREFIX, &bytes);
        let pinned = |kid: &str| (kid == "rg-1").then(|| grant.verifying_key());
        assert_eq!(
            verify_report_answer(&bytes, &h, 7, START, pinned, NOW).unwrap(),
            answer
        );
        assert!(
            verify_report_answer(&bytes, &h, 7, "another-start", pinned, NOW).is_err(),
            "another start"
        );
        assert!(
            verify_report_answer(&bytes, &h, 8, START, pinned, NOW).is_err(),
            "another seq"
        );
        assert!(
            verify_report_answer(&bytes, &h, 7, START, |_| None, NOW).is_err(),
            "kid not pinned"
        );
        let wrong = cert::sign_body(&session, MM_WATCH_ANSWER_PREFIX, &bytes);
        assert!(
            verify_report_answer(&bytes, &wrong, 7, START, pinned, NOW).is_err(),
            "another prefix"
        );
        // A rules answer with its nonce.
        let (_, wsig) = s.watch(NOW, "n-1");
        assert!(!wsig.is_empty());
        let w = RelayWatch {
            nonce: "n-1".into(),
            rules: WatchRules {
                version: 1,
                speed: WatchSpeed {
                    max_kmh: 451.0,
                    sustain_ms: 3000,
                    per_car_floor_kmh: 311.3,
                },
                climb: WatchLimit {
                    max_kmh: 150.0,
                    sustain_ms: 3000,
                },
                jump: WatchJump { max_meters: 200.0 },
            },
            cert: answer.cert.clone(),
        };
        let bytes = serde_json::to_vec(&w).unwrap();
        let h = cert::sign_body(&session, MM_WATCH_ANSWER_PREFIX, &bytes);
        let got = verify_watch_answer(&bytes, &h, "n-1", pinned, NOW).unwrap();
        assert_eq!(rules(&got.rules).speed.max_kmh, 451.0);
        assert!(verify_watch_answer(&bytes, &h, "n-2", pinned, NOW).is_err());
    }

    #[test]
    fn counters_are_differences_since_the_last_accepted_batch() {
        let base = Counters {
            decoded: 10,
            ..Counters::default()
        };
        let now = Counters {
            decoded: 15,
            rings_in_use: 3,
            ..Counters::default()
        };
        let c = counters(&now, &base, 2);
        assert_eq!((c.decoded, c.rings_in_use, c.report_dropped), (5, 3, 2));
    }
}
