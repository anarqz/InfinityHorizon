//! Metadata-only crash diagnostics (OD-78). No dump parser, causal attribution or sanction rule.
use fi_protocol::crash::CrashCreate;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Maximum metadata wire size.
pub const METADATA_MAX: usize = 16 * 1024;
/// Maximum untrusted diagnostic hints.
pub const OBSERVATIONS_MAX: usize = 16;

/// Bounded normalized metadata for server context checks, never a causal verdict.
#[derive(Debug)]
pub struct Validated {
    /// Client incident id.
    pub id: Uuid,
    /// Owned admitted session.
    pub session: Uuid,
    /// Optional server world.
    pub world: Option<Uuid>,
    /// Claimed UTC incident time; eligibility still checked against server history.
    pub occurred_ms: i64,
    /// Canonical typed body hash.
    pub request_hash: Vec<u8>,
    /// Diagnostic grouping only; never identifies an offender.
    pub signature: String,
    /// Bounded private JSON, without attachments.
    pub metadata: serde_json::Value,
    /// Parsed hints, never proof.
    pub observations: Vec<i64>,
}

/// Lowercase SHA-256 for normalized grouping/idempotency, not remote attestation.
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Strict bounded field validation. `dump_disabled` maps to CRASH.DUMPS_DISABLED, not a 202
/// with silently reduced consent. There is deliberately no attachment storage/validation path.
pub fn validate(r: &CrashCreate) -> Result<Validated, &'static str> {
    if r.schema_version != 1 || !r.consent.metadata {
        return Err("consent");
    }
    if r.consent.minidump || r.dump.is_some() {
        return Err("dump_disabled");
    }
    let id = |s: &str| Uuid::parse_str(s).ok().filter(|id| !id.is_nil());
    let report = id(&r.report_id).ok_or("reportId")?;
    let session = id(&r.incident.session_id).ok_or("incident.sessionId")?;
    let world = r
        .incident
        .world_id
        .as_deref()
        .map(|s| id(s).ok_or("incident.worldId"))
        .transpose()?;
    let occurred_ms = fi_protocol::instant::parse_unix_micros(&r.incident.occurred_at)
        .ok_or("incident.occurredAt")?
        / 1_000;
    if r.build.game != "fh2" {
        return Err("build.game");
    }
    for s in [
        &r.build.game_build,
        &r.build.launcher_version,
        &r.build.hook_version,
    ] {
        if !(1..=32).contains(&s.len())
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_+".contains(&b))
        {
            return Err("build");
        }
    }
    if r.crash.fault_offset.as_deref().is_some_and(|s| !hex(s, 16)) {
        return Err("crash.faultOffset");
    }
    if r.observations.len() > OBSERVATIONS_MAX {
        return Err("observations");
    }
    let mut observations = Vec::new();
    for o in &r.observations {
        if o.participant_xuid.is_empty() || !o.participant_xuid.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("observations.participantXuid");
        }
        let xuid = o
            .participant_xuid
            .parse::<i64>()
            .ok()
            .filter(|x| *x > 0)
            .ok_or("observations.participantXuid")?;
        if o.content_sha256.as_deref().is_some_and(|s| !hex(s, 64)) {
            return Err("observations.contentSha256");
        }
        observations.push(xuid);
    }
    let metadata = serde_json::to_value(r).map_err(|_| "body")?;
    let bytes = serde_json::to_vec(&metadata).map_err(|_| "body")?;
    if bytes.len() > METADATA_MAX {
        return Err("body");
    }
    let group = serde_json::json!({"build":r.build,"crash":r.crash});
    Ok(Validated {
        id: report,
        session,
        world,
        occurred_ms,
        request_hash: Sha256::digest(&bytes).to_vec(),
        signature: digest(&serde_json::to_vec(&group).map_err(|_| "body")?),
        metadata,
        observations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> serde_json::Value {
        serde_json::json!({"schemaVersion":1,"reportId":Uuid::now_v7().to_string(),"consent":{"metadata":true,"minidump":false},"incident":{"occurredAt":"2026-10-20T20:00:00.000000Z","sessionId":Uuid::now_v7().to_string()},"build":{"game":"fh2","gameBuild":"0.9.10","launcherVersion":"0.1.0","hookVersion":"0.3.0"},"crash":{"kind":"abnormal_exit","faultModule":"unknown"},"observations":[]})
    }
    #[test]
    fn nested_schema_refuses_memory_verdicts_and_commands() {
        let clean = request();
        assert!(validate(&serde_json::from_value(clean.clone()).unwrap()).is_ok());
        for parent in ["", "consent", "incident", "build", "crash"] {
            let mut b = clean.clone();
            let object = if parent.is_empty() {
                b.as_object_mut().unwrap()
            } else {
                b[parent].as_object_mut().unwrap()
            };
            object.insert(
                "rawMemoryOrVerdict".to_owned(),
                serde_json::json!("forbidden"),
            );
            assert!(serde_json::from_value::<CrashCreate>(b).is_err());
        }
    }
    #[test]
    fn any_attachment_consent_or_descriptor_is_explicitly_disabled() {
        for (consent, descriptor) in [(true, false), (false, true), (true, true)] {
            let mut r = request();
            r["consent"]["minidump"] = serde_json::json!(consent);
            if descriptor {
                r["dump"] = serde_json::json!({"byteLength":48,"sha256":"0".repeat(64)});
            }
            assert_eq!(
                validate(&serde_json::from_value(r).unwrap()).unwrap_err(),
                "dump_disabled"
            );
        }
    }
    #[test]
    fn consent_and_observation_bounds() {
        let mut b = request();
        b["consent"]["metadata"] = serde_json::json!(false);
        assert!(validate(&serde_json::from_value(b).unwrap()).is_err());
        let mut b = request();
        b["observations"] = serde_json::json!(vec![
            serde_json::json!({"participantXuid":"1","kind":"roster"});
            17
        ]);
        assert!(validate(&serde_json::from_value(b).unwrap()).is_err());
        for bad in ["../private/path", "@everyone", ""] {
            let mut b = request();
            b["build"]["hookVersion"] = serde_json::json!(bad);
            assert!(validate(&serde_json::from_value(b).unwrap()).is_err());
        }
    }
    #[test]
    fn diagnostic_cluster_never_depends_on_accused_identity() {
        let a = request();
        let mut b = a.clone();
        b["reportId"] = serde_json::json!(Uuid::now_v7().to_string());
        b["observations"] = serde_json::json!([{"participantXuid":"123","kind":"last_packet"}]);
        let x = validate(&serde_json::from_value(a).unwrap()).unwrap();
        let y = validate(&serde_json::from_value(b).unwrap()).unwrap();
        assert_eq!(x.signature, y.signature);
        assert_ne!(x.request_hash, y.request_hash);
    }
}
