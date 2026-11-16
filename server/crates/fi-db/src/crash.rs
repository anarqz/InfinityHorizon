//! OD-78 metadata-only intake, diagnostic triage, and separately authorized HUMAN moderation.
//! No attachment storage, causal attribution, strike counter, or automatic enforcement path.
use fi_crash::Validated;
use fi_protocol::crash::{CrashAccepted, CrashView};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Safe refusals; never format the inner SQL error into logs/client replies.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Unavailable storage.
    #[error("crash storage unavailable")]
    Db(#[from] sqlx::Error),
    /// Changed id/key/body.
    #[error("idempotency conflict")]
    Conflict,
    /// Invalid session/membership/moderation context.
    #[error("invalid context")]
    Context,
    /// Missing/foreign report.
    #[error("report not found")]
    NotFound,
    /// Currently verified identity required.
    #[error("verified identity required")]
    Unverified,
    /// Quota and conservative retry delay.
    #[error("request quota exceeded")]
    Rate(u64),
    /// Global retained-report cap.
    #[error("crash capacity exhausted")]
    Capacity,
}
async fn lock(conn: &mut PgConnection) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(753182401)")
        .execute(conn)
        .await?;
    Ok(())
}
fn wire(t: OffsetDateTime) -> String {
    fi_protocol::instant::format_unix_micros((t.unix_timestamp_nanos() / 1000) as i64)
        .unwrap_or_default()
}
/// Every API route/replay checks current verified mapping.
pub async fn verified_in(conn: &mut PgConnection, owner: Uuid) -> Result<(), Error> {
    let verified: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM account_identity WHERE account_id=$1 AND email_verified)",
    )
    .bind(owner)
    .fetch_one(conn)
    .await?;
    if verified {
        Ok(())
    } else {
        Err(Error::Unverified)
    }
}
/// Exact shared attempt budgets also cover malformed/retried/disabled calls.
pub async fn request_limit(pool: &PgPool, owner: Uuid, kind: &str) -> Result<(), Error> {
    let (max, window_s) = match kind {
        "read" => (60, 60),
        "upload" => (10, 3600),
        _ => (120, 3600),
    };
    let limit = crate::limits::Limit {
        name: "crash.attempt",
        max,
        window_s,
    };
    let key = format!(
        "crash.attempt:{}",
        fi_crash::digest(format!("crash:{owner}:{kind}").as_bytes())
    );
    let mut conn = pool.acquire().await?;
    verified_in(&mut conn, owner).await?;
    if let crate::limits::Hit::Limited { retry_after_secs } =
        crate::limits::hit_in(&mut conn, &key, &limit)
            .await
            .map_err(|_| Error::Capacity)?
    {
        return Err(Error::Rate(retry_after_secs.max(1) as u64));
    }
    Ok(())
}
/// Durable authenticated metadata accept. No 202 can advertise an attachment capability.
pub async fn accept_in(
    conn: &mut PgConnection,
    owner: Uuid,
    key: Uuid,
    r: &Validated,
) -> Result<CrashAccepted, Error> {
    lock(conn).await?;
    verified_in(conn, owner).await?;
    let old=sqlx::query("SELECT id,reporter,idempotency_key,request_hash FROM crash_report WHERE id=$1 OR (reporter=$2 AND idempotency_key=$3)").bind(r.id).bind(owner).bind(key).fetch_all(&mut *conn).await?;
    if !old.is_empty() {
        if old.len() != 1
            || old[0].try_get::<Uuid, _>("id")? != r.id
            || old[0].try_get::<Uuid, _>("reporter")? != owner
            || old[0].try_get::<Uuid, _>("idempotency_key")? != key
            || old[0].try_get::<Vec<u8>, _>("request_hash")? != r.request_hash
        {
            return Err(Error::Conflict);
        }
        return Ok(accepted(r.id));
    }
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(r.occurred_ms) * 1_000_000)
        .map_err(|_| Error::Context)?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM play_session s WHERE s.id=$1 AND s.account_id=$2 AND s.game='fh2' AND s.admitted_at<=$3 AND $3<=least(coalesce(s.ended_at,clock_timestamp()),s.expires_at,s.max_until) AND $3>=clock_timestamp()-interval '24 hours')").bind(r.session).bind(owner).bind(at).fetch_one(&mut *conn).await?;
    if !valid {
        return Err(Error::Context);
    }
    let (hour,day):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE created_at>clock_timestamp()-interval '1 hour'),count(*) FROM crash_report WHERE reporter=$1 AND created_at>clock_timestamp()-interval '1 day'").bind(owner).fetch_one(&mut *conn).await?;
    if hour >= 5 || day >= 20 {
        return Err(Error::Rate(if hour >= 5 { 3600 } else { 86400 }));
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM crash_report")
        .fetch_one(&mut *conn)
        .await?;
    if total >= 5000 {
        return Err(Error::Capacity);
    }
    let participants: Vec<(Uuid, i64)> = if let Some(world) = r.world {
        sqlx::query_as("WITH w AS (SELECT * FROM world WHERE id=$1 AND game='fh2' AND opened_at<=$2 AND $2<=least(coalesce(closed_at,clock_timestamp()),expires_at)), members AS (SELECT host_account_id AS account_id,p.xuid FROM w JOIN profile p ON p.id=w.host_profile_id UNION SELECT g.account_id,g.xuid FROM mm_grant g JOIN w ON w.id=g.world_id WHERE g.used_at<=$2 AND $2<=coalesce(g.ended_at,clock_timestamp())) SELECT account_id,xuid FROM members").bind(world).bind(at).fetch_all(&mut *conn).await?
    } else {
        Vec::new()
    };
    if r.world.is_some() && !participants.iter().any(|(a, _)| *a == owner) {
        return Err(Error::Context);
    }
    sqlx::query("INSERT INTO crash_report(id,reporter,session_id,world_id,occurred_at,idempotency_key,request_hash,signature,metadata) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(r.id).bind(owner).bind(r.session).bind(r.world).bind(at).bind(key).bind(&r.request_hash).bind(&r.signature).bind(&r.metadata).execute(&mut *conn).await?;
    for (account, xuid) in participants {
        sqlx::query(
            "INSERT INTO crash_participant(report_id,account_id,xuid,hinted) VALUES($1,$2,$3,$4)",
        )
        .bind(r.id)
        .bind(account)
        .bind(xuid)
        .bind(r.observations.contains(&xuid))
        .execute(&mut *conn)
        .await?;
    }
    Ok(accepted(r.id))
}
fn accepted(id: Uuid) -> CrashAccepted {
    CrashAccepted {
        report_id: id.to_string(),
        state: "queued".into(),
        dump_upload: None,
    }
}
/// Private owner workflow view. No causal verdict or enforcement details.
pub async fn view(conn: &mut PgConnection, owner: Uuid, id: Uuid) -> Result<CrashView, Error> {
    verified_in(conn, owner).await?;
    let r=sqlx::query("SELECT state,classification,expires_at FROM crash_report WHERE id=$1 AND reporter=$2 AND expires_at>clock_timestamp()").bind(id).bind(owner).fetch_optional(conn).await?.ok_or(Error::NotFound)?;
    Ok(CrashView {
        report_id: id.to_string(),
        state: r.try_get("state")?,
        classification: r.try_get("classification")?,
        dump_status: "disabled".into(),
        retained_until: wire(r.try_get("expires_at")?),
    })
}
/// Bounded diagnostic worker. Does not call a moderation writer or count victims for sanctions.
pub async fn analyze_in(conn: &mut PgConnection) -> Result<u64, Error> {
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM crash_report WHERE state='queued' AND expires_at>clock_timestamp() ORDER BY created_at LIMIT 100 FOR UPDATE SKIP LOCKED").fetch_all(&mut *conn).await?;
    for id in &ids {
        sqlx::query("UPDATE crash_report r SET state='analyzed',classification=CASE WHEN EXISTS(SELECT 1 FROM crash_review v WHERE v.report_id=r.id) THEN 'reviewed' WHEN EXISTS(SELECT 1 FROM crash_participant p WHERE p.report_id=r.id AND p.hinted AND p.account_id<>r.reporter) THEN 'needs_review' ELSE 'unknown' END WHERE id=$1").bind(id).execute(&mut *conn).await?;
        notify_in(conn, *id).await?;
    }
    Ok(ids.len() as u64)
}
async fn notify_in(conn: &mut PgConnection, id: Uuid) -> Result<(), Error> {
    sqlx::query("INSERT INTO crash_notification(report_id,classification,signature,human_review) SELECT id,classification,signature,classification='reviewed' FROM crash_report WHERE id=$1 ON CONFLICT(report_id) DO UPDATE SET classification=EXCLUDED.classification,human_review=EXCLUDED.human_review,generation=crash_notification.generation+1,attempts=0,next_at=clock_timestamp() WHERE (crash_notification.classification,crash_notification.human_review) IS DISTINCT FROM (EXCLUDED.classification,EXCLUDED.human_review)").bind(id).execute(conn).await?;
    Ok(())
}
/// Explicit human decision; private case digest documents the moderator's evidence, not an AI
/// or client attestation. A missing duration means diagnostic review with NO action.
pub struct ManualReview<'a> {
    /// Operator-minted action id, reused by retries.
    pub id: Uuid,
    /// Consented report/context.
    pub report: Uuid,
    /// Human actor, currently authorized moderator/admin.
    pub moderator: Uuid,
    /// Optional subject, server-bound to this incident; never inferred by the analyzer.
    pub subject: Option<Uuid>,
    /// Private case reference digest, never exposed in notices.
    pub case_digest: &'a [u8],
    /// Explicit restriction duration, 1..86400 seconds; None = no restriction.
    pub duration_s: Option<i32>,
}
async fn moderator_in(conn: &mut PgConnection, actor: Uuid) -> Result<String, Error> {
    sqlx::query_scalar("SELECT role FROM account WHERE id=$1 AND role IN ('moderator','admin') AND status='active' AND (banned_at IS NULL OR banned_until<=clock_timestamp())").bind(actor).fetch_optional(conn).await?.ok_or(Error::Context)
}
/// The sole writer of online restrictions. Maintenance DB credential AND human actor required.
pub async fn review_in(
    conn: &mut PgConnection,
    r: &ManualReview<'_>,
) -> Result<Option<Uuid>, Error> {
    lock(conn).await?;
    let role = moderator_in(conn, r.moderator).await?;
    if r.id.is_nil()
        || r.case_digest.len() != 32
        || r.duration_s.is_some_and(|s| !(1..=86400).contains(&s))
        || (r.duration_s.is_some() && r.subject.is_none())
        || r.subject == Some(r.moderator)
    {
        return Err(Error::Context);
    }
    let old=sqlx::query("SELECT report_id,moderator,subject,private_case_digest,duration_s FROM crash_review WHERE id=$1").bind(r.id).fetch_optional(&mut *conn).await?;
    if let Some(old) = old {
        if old.try_get::<Option<Uuid>, _>("report_id")? != Some(r.report)
            || old.try_get::<Uuid, _>("moderator")? != r.moderator
            || old.try_get::<Option<Uuid>, _>("subject")? != r.subject
            || old.try_get::<Vec<u8>, _>("private_case_digest")? != r.case_digest
            || old.try_get::<Option<i32>, _>("duration_s")? != r.duration_s
        {
            return Err(Error::Conflict);
        }
        return Ok(r.duration_s.map(|_| r.id));
    }
    let reporter: Uuid = sqlx::query_scalar(
        "SELECT reporter FROM crash_report WHERE id=$1 AND expires_at>clock_timestamp()",
    )
    .bind(r.report)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(Error::NotFound)?;
    if reporter == r.moderator {
        return Err(Error::Context);
    }
    if let Some(subject) = r.subject {
        let related:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account a WHERE a.id=$2 AND a.status<>'deleted' AND (a.id=$3 OR EXISTS(SELECT 1 FROM crash_participant p WHERE p.report_id=$1 AND p.account_id=a.id)))").bind(r.report).bind(subject).bind(reporter).fetch_one(&mut *conn).await?;
        if !related {
            return Err(Error::Context);
        }
    }
    sqlx::query("INSERT INTO crash_review(id,report_id,moderator,moderator_role,subject,private_case_digest,duration_s) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(r.id).bind(r.report).bind(r.moderator).bind(role).bind(r.subject).bind(r.case_digest).bind(r.duration_s).execute(&mut *conn).await?;
    if let (Some(account), Some(seconds)) = (r.subject, r.duration_s) {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,753182402))")
            .bind(account.to_string())
            .execute(&mut *conn)
            .await?;
        sqlx::query("INSERT INTO crash_restriction(id,account_id,until_at) VALUES($1,$2,clock_timestamp()+make_interval(secs=>$3))").bind(r.id).bind(account).bind(seconds as f64).execute(&mut *conn).await?;
    }
    sqlx::query("UPDATE crash_report SET state='analyzed',classification='reviewed' WHERE id=$1")
        .bind(r.report)
        .execute(&mut *conn)
        .await?;
    notify_in(conn, r.report).await?;
    Ok(r.duration_s.map(|_| r.id))
}
/// Effective HUMAN-imposed online restriction, serialized with gate decisions until commit.
pub async fn frozen_until_in(
    conn: &mut PgConnection,
    account: Uuid,
) -> Result<Option<fi_domain::UnixMs>, sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,753182402))")
        .bind(account.to_string())
        .execute(&mut *conn)
        .await?;
    let until:Option<OffsetDateTime>=sqlx::query_scalar("SELECT max(until_at) FROM crash_restriction WHERE account_id=$1 AND released_at IS NULL AND until_at>clock_timestamp()").bind(account).fetch_one(conn).await?;
    Ok(until.map(crate::identity::ms))
}
/// Human reversal of one exact action, without touching another restriction or a manual ban.
pub async fn release_in(conn: &mut PgConnection, id: Uuid, actor: Uuid) -> Result<(), Error> {
    lock(conn).await?;
    moderator_in(conn, actor).await?;
    let account: Uuid = sqlx::query_scalar("SELECT account_id FROM crash_restriction WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(Error::NotFound)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,753182402))")
        .bind(account.to_string())
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE crash_restriction SET released_at=coalesce(released_at,clock_timestamp()),released_by=coalesce(released_by,$2) WHERE id=$1").bind(id).bind(actor).execute(conn).await?;
    Ok(())
}
/// Bounded metadata/context erasure (30d), separate immutable moderation audit retention (90d).
pub async fn retain_in(conn: &mut PgConnection) -> Result<(), Error> {
    lock(conn).await?;
    sqlx::query("DELETE FROM crash_report WHERE id IN (SELECT r.id FROM crash_report r JOIN account a ON a.id=r.reporter WHERE r.expires_at<=clock_timestamp() OR a.status='deleted' OR EXISTS(SELECT 1 FROM crash_participant p JOIN account x ON x.id=p.account_id WHERE p.report_id=r.id AND x.status='deleted') LIMIT 100)").execute(&mut *conn).await?;
    sqlx::query("DELETE FROM crash_review WHERE id IN (SELECT id FROM crash_review WHERE created_at<clock_timestamp()-interval '90 days' LIMIT 100)").execute(conn).await?;
    Ok(())
}
/// Durable at-least-once notification lease; payload has only server allowlisted diagnostics.
pub async fn notification(pool: &PgPool) -> Result<Option<(Uuid, i32, Value)>, Error> {
    let row=sqlx::query("UPDATE crash_notification SET attempts=attempts+1,lease_until=clock_timestamp()+interval '30 seconds' WHERE id=(SELECT n.id FROM crash_notification n JOIN crash_report r ON r.id=n.report_id JOIN account a ON a.id=r.reporter WHERE n.sent_generation<n.generation AND n.attempts<8 AND n.next_at<=clock_timestamp() AND (n.lease_until IS NULL OR n.lease_until<=clock_timestamp()) AND r.expires_at>clock_timestamp() AND a.status<>'deleted' AND NOT EXISTS(SELECT 1 FROM crash_participant p JOIN account x ON x.id=p.account_id WHERE p.report_id=r.id AND x.status='deleted') ORDER BY n.next_at LIMIT 1 FOR UPDATE OF n SKIP LOCKED) RETURNING id,report_id,classification,signature,human_review,generation").fetch_optional(pool).await?;
    row.map(|r|Ok((r.try_get("id")?,r.try_get("generation")?,json!({"allowed_mentions":{"parse":[]},"content":format!("Crash metadata {} | triage={} | diagnostic_group={} | human_review={} | notification={}",r.try_get::<Uuid,_>("report_id")?,r.try_get::<String,_>("classification")?,r.try_get::<String,_>("signature")?,r.try_get::<bool,_>("human_review")?,r.try_get::<Uuid,_>("id")?)})))).transpose()
}
/// Generation-aware acknowledgement; no network call holds a transaction.
pub async fn notification_done(
    pool: &PgPool,
    id: Uuid,
    generation: i32,
    ok: bool,
) -> Result<(), Error> {
    sqlx::query("UPDATE crash_notification SET sent_generation=CASE WHEN $3 THEN $2 ELSE sent_generation END,lease_until=NULL,next_at=clock_timestamp()+interval '1 minute' WHERE id=$1 AND generation=$2").bind(id).bind(generation).bind(ok).execute(pool).await?;
    Ok(())
}
