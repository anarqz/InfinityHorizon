//! The Forza RPC step of the dispatch (GAME_PLATFORM.md §3a (2)): `X-ClassName` and `X-Method`
//! looked up in the game's schema (`fi_xtsw::Schema::fh2`, the assembly form accepted), the body
//! decoded as the method's `_Parameters`, the reply built ([`super::forza`]) and encoded as its
//! `_Results` in the 12-byte envelope, HTTP 200 `bin/xtsw` for success and Xls errors alike (the
//! Beta's `forza_answer`, `deprecated/beta/server/src/main.rs:834-909`).
//!
//! Refusals: a class or method the schema lacks, or one not served: 404 `GAME.NOT_SERVED` (decided
//! from the headers by [`lookup`], before the body is read); a body over the method's
//! [`body_cap`]: 413 `REQUEST.TOO_LARGE`, before it is decoded; a body that does not decode: 400
//! `REQUEST.INVALID {field: "body"}`. A reply that does not encode is a server bug: logged, 503
//! (every builder is encoded in a unit test).
//!
//! A stored method runs in one transaction with its progression fact (event id
//! `UUIDv5(session, "req:" + Class.Method + ":" + X-Fh2-Request-Id)`, [`target::fact_id`]), and
//! its write applies only when that fact is new: a replayed request id answers the same reply and
//! changes nothing. A request of a method the game reconciles ([`forza::reconcilable`]) with an
//! `X-TransactionId` keeps its reply (`fi_db::gameplane::keep_reply_in`, a ring per profile), so
//! Reconcile hands back exactly what was answered. Pure methods touch no database. The profile sync
//! (OnlineProfile.UpdateAll, UpdateAllTables) is stored for every ticket, free, noob and lapsed
//! included (OD-128 replaced OD-24's acknowledgement: the drivatars need everyone's level and
//! garage).
//!
//! The `features` every function here takes are the request's ([`Features::for_ticket`]): the
//! ticket's `tier` and `mode` already narrowed them, so the universe of a ticket that keeps no
//! cloud data was refused from the headers (OD-51, OD-52).

use super::forza::{self, Lang, Player, Reply, Served, XlsError};
use super::{Features, not_served, target};
use crate::errors::ApiError;
use crate::game::request::GameRequest;
use crate::state::AppState;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_db::game::{Fact, FactError, record_progression_in};
use fi_db::gameplane::{self, Owner};
use fi_db::visibility::Viewer;
use fi_db::{integrity, seeds};
use fi_gamedb::Catalog;
use fi_gamedb::gates::{self, Refusal};
use fi_guard::Assessment;
use fi_protocol::ErrorCode;
use fi_storage::Blobs;
use fi_xtsw::{Schema, decode_params, encode_results, error_envelope, ok_envelope};
use serde_json::{Map, Value as Json, json};
use sqlx::{Connection, PgConnection};

/// The `bin/xtsw` media type of requests and replies.
pub const XTSW: &str = "bin/xtsw";

/// The largest body of a served method, unless [`LARGE_BODIES`] says otherwise. A decoded body is
/// held as a typed tree and then a JSON tree, up to ~45 times its size for an array of small
/// numbers, so the cap bounds a request's memory; the game's bodies of the served methods are
/// under 2 KiB (Beta captures), and a full garage upload stays a few hundred KiB.
pub const RPC_BODY: usize = 1 << 20;

const _: () = assert!(super::JSON_BODY < RPC_BODY && RPC_BODY <= super::MAX_BODY);

/// Methods whose bodies have their own cap instead of [`RPC_BODY`] (larger or smaller), `(class
/// key, method, cap)`: the blob uploads (liveries, ghosts, thumbnails, files) get theirs here when
/// they are served (SER-M3 steps 6 to 9), at most [`super::MAX_BODY`]. A ghost's `data` is a `blob`
/// on the wire (raw bytes; hex only in the decoded JSON): the Beta's captures show 20 bytes around
/// it (the scoreId and the length), so `UploadFile` is capped just above the largest ghost kept.
const LARGE_BODIES: &[(&str, &str, usize)] = &[
    (
        "Forza.WebServices.Scoreboard",
        "UploadFile",
        super::universe::ghosts::UPLOAD_BODY,
    ),
    (
        "Forza.WebServices.OnlineProfile",
        "UpdateLivery",
        super::liveries::UPDATE_BODY,
    ),
    // A shared design carries its preview (up to 400 KiB) and an admin texture beside the header
    // (OD-138, CR-S74).
    ("Forza.WebServices.StorefrontLivery", "Create", 2 * RPC_BODY),
];

/// `Forza.WebServices.User, Forza.WebServices` → `Forza.WebServices.User`.
pub fn class_key(class: &str) -> &str {
    class.split(',').next().unwrap_or(class).trim()
}

/// A served Forza RPC, from its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rpc<'a> {
    /// The class without its assembly (`Forza.WebServices.User`).
    pub key: &'a str,
    /// The method.
    pub method: &'a str,
    /// An `Xls.` class.
    pub xls: bool,
    /// The class's last segment (`User`).
    pub short: &'a str,
    /// How it is answered.
    pub served: Served,
}

/// The served RPC that `X-ClassName` and `X-Method` name; `None` when the schema lacks it or a
/// deployment serving `features` does not serve it ([`forza::served`]; `GAME.NOT_SERVED`).
pub fn lookup<'a>(features: Features, class: &'a str, method: &'a str) -> Option<Rpc<'a>> {
    let key = class_key(class);
    let short = key.rsplit('.').next().unwrap_or(key);
    let xls = key.starts_with("Xls.");
    Schema::fh2().method(key, method)?;
    let served = forza::served(features, xls, short, method)?;
    Some(Rpc {
        key,
        method,
        xls,
        short,
        served,
    })
}

/// The largest body accepted for `key.method` ([`RPC_BODY`] unless [`LARGE_BODIES`] lists it).
pub fn body_cap(key: &str, method: &str) -> usize {
    LARGE_BODIES
        .iter()
        .find(|(c, m, _)| *c == key && *m == method)
        .map_or(RPC_BODY, |(_, _, cap)| (*cap).min(super::MAX_BODY))
}

/// What a stored handler sees besides its parameters (here and in [`super::universe`]).
pub(crate) struct Ctx<'a> {
    pub(crate) req: &'a GameRequest<'a>,
    /// `Class.Method`, the fact id's scope.
    pub(crate) label: &'a str,
    /// `X-Fh2-Request-Id` as sent (a UUID or the hook's counter).
    pub(crate) request_id: &'a str,
    pub(crate) lang: Lang,
    /// What this request is served ([`Features::for_ticket`]).
    pub(crate) features: Features,
    /// The cloud-saves bucket of the deployment (`PlatformConfig::blobs`), where ghost recordings
    /// live; reached only outside a transaction ([`super::universe::ghosts::with_bucket`]).
    pub(crate) blobs: Option<&'a Blobs>,
    /// The guardrails' enforcement switch ([`Deployment::guard_enforce`]).
    pub(crate) guard_enforce: bool,
}

impl Ctx<'_> {
    pub(crate) fn owner(&self) -> Owner<'_> {
        Owner {
            game: self.req.ticket.game(),
            profile_id: self.req.ticket.live.profile_id,
            session_id: self.req.ticket.live.session_id,
        }
    }

    /// The ticket's profile as a universe read sees it ([`fi_db::visibility`]).
    pub(crate) fn viewer(&self) -> Viewer<'_> {
        let t = &self.req.ticket;
        Viewer {
            game: t.game(),
            mode: &t.claims.mode,
            profile_id: t.live.profile_id,
            account_id: t.live.account_id,
            xuid: t.xuid,
            session_id: t.live.session_id,
        }
    }

    /// The request's fact id (GAME_PLATFORM.md §3a): stable across a replay of the request, so
    /// an id derived from it (a `scoreId`) is answered the same.
    pub(crate) fn fact_id(&self) -> uuid::Uuid {
        target::fact_id(self.req.ticket.live.session_id, self.label, self.request_id)
    }

    /// `GAME.NOT_SERVED` for this request.
    pub(crate) fn not_served(&self, short: &str, method: &str) -> ApiError {
        not_served(self.req.host, Some(short), Some(method))
    }
}

fn fact_error(e: FactError) -> ApiError {
    match e {
        FactError::Db(e) => e.into(),
        other => {
            tracing::error!(error = %other, "progression fact refused");
            ApiError::unavailable()
        }
    }
}

fn too_large() -> ApiError {
    ApiError::new(ErrorCode::RequestTooLarge, "the body is too large")
}

/// What a stored method needs of the deployment ([`crate::state::Platform`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Deployment<'a> {
    /// The cloud-saves bucket, if the deployment has one (ghost recordings).
    pub blobs: Option<&'a Blobs>,
    /// `FI_GUARD_ENFORCE` (`PlatformConfig::guard_enforce`): the guardrails refuse a score of 60
    /// or more; off, they only score (shadow). Ceilings refuse either way.
    pub guard_enforce: bool,
}

/// Answers one Forza RPC served `features` (the request's, [`Features::for_ticket`]; the caller
/// checked the ticket, the headers, what is served and the body against [`body_cap`]) on
/// `deployment`.
#[allow(clippy::too_many_arguments)]
pub async fn answer(
    state: &AppState,
    req: &GameRequest<'_>,
    features: Features,
    class: &str,
    method: &str,
    request_id: &str,
    lang: Lang,
    deployment: Deployment<'_>,
) -> Result<Response, ApiError> {
    let rpc = lookup(features, class, method)
        .ok_or_else(|| not_served(req.host, Some(class_key(class)), Some(method)))?;
    let Rpc {
        key,
        xls,
        short,
        served,
        ..
    } = rpc;
    if req.body.len() > body_cap(key, method) {
        return Err(too_large());
    }
    let schema = Schema::fh2();
    let params = decode_params(schema, key, method, req.body)
        .map_err(|e| {
            ApiError::invalid(
                "body",
                format!("the body does not decode as {key}.{method}: {e}"),
            )
        })?
        .into_json();
    // Kept only where the game reconciles: any other method's transaction id is not stored.
    let keep = req
        .headers
        .get("x-transactionid")
        .and_then(|v| v.to_str().ok())
        .and_then(target::transaction_id)
        .filter(|_| forza::reconcilable(xls, short, method));
    let label = format!("{key}.{method}");
    let ctx = Ctx {
        req,
        label: &label,
        request_id,
        lang,
        features,
        blobs: deployment.blobs,
        guard_enforce: deployment.guard_enforce,
    };
    // The ghost recordings reach the bucket, so these run their own steps around it: no
    // transaction spans a network call (services/AGENTS.md).
    if super::universe::ghosts::uses_bucket(xls, short, method) {
        let reply = super::universe::ghosts::with_bucket(state, &ctx, method, params).await?;
        return Ok(xtsw_response(envelope(schema, key, method, reply)?));
    }
    // A universe method that reads and writes nothing: no transaction.
    if let Some(reply) = super::universe::boards::without_db(xls, short, method, lang, &params) {
        return Ok(xtsw_response(envelope(schema, key, method, reply)?));
    }
    let body = match (served, keep) {
        (Served::Pure, None) => {
            let reply =
                forza::pure(xls, short, method, &params).ok_or_else(ApiError::unavailable)?;
            envelope(schema, key, method, Ok(reply))?
        }
        (served, keep) => {
            let mut tx = state.pool.begin().await?;
            let reply =
                match served {
                    Served::Pure => Ok(forza::pure(xls, short, method, &params)
                        .ok_or_else(ApiError::unavailable)?),
                    Served::Store => stored(&mut tx, &ctx, xls, short, method, params).await?,
                };
            let body = envelope(schema, key, method, reply)?;
            if let Some(id) = keep {
                gameplane::keep_reply_in(&mut tx, ctx.owner(), id, &label, &body)
                    .await
                    .map_err(ApiError::from_stored)?;
            }
            tx.commit().await?;
            body
        }
    };
    Ok(xtsw_response(body))
}

/// HTTP 200 `bin/xtsw` with an envelope (success and Xls errors alike).
pub(super) fn xtsw_response(body: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(XTSW));
    response
}

/// The envelope of a reply: `_Results` encoded behind a success header, or an Xls error.
pub(super) fn envelope(
    schema: &Schema,
    class: &str,
    method: &str,
    reply: Reply,
) -> Result<Vec<u8>, ApiError> {
    match reply {
        Ok(results) => match encode_results(schema, class, method, &results) {
            Ok(bytes) => Ok(ok_envelope(&bytes)),
            Err(e) => {
                tracing::error!(class, method, error = %e, "reply does not encode");
                Err(ApiError::unavailable())
            }
        },
        Err(XlsError { code, message }) => Ok(error_envelope(code, message)),
    }
}

/// The player as the replies show them, from their stored logon state.
pub(crate) fn player_of(row: gameplane::Player) -> Player {
    let xuid = u64::try_from(row.xuid).unwrap_or(0);
    let gamertag = match row.name.as_deref() {
        Some(name) => forza::game_tag(name),
        None => forza::generated_gamertag(xuid),
    };
    Player {
        xuid,
        gamertag,
        card: row.card,
    }
}

async fn player(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<gameplane::Player, ApiError> {
    gameplane::player_in(conn, ctx.req.ticket.game(), ctx.req.ticket.live.profile_id)
        .await?
        .ok_or_else(ApiError::unavailable)
}

/// Records the request's fact of `kind`; `false` when it was already recorded (a replayed
/// request id), in which case the caller writes nothing.
pub(crate) async fn record(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    kind: &str,
    data: Map<String, Json>,
) -> Result<bool, ApiError> {
    let live = &ctx.req.ticket.live;
    let done = record_progression_in(
        conn,
        &Fact {
            event_id: target::fact_id(live.session_id, ctx.label, ctx.request_id),
            account_id: live.account_id,
            profile_id: live.profile_id,
            session_id: Some(live.session_id),
            kind: kind.to_owned(),
            data,
        },
    )
    .await;
    match done {
        Ok(done) => Ok(done.recorded),
        // The game's own request still answers: a fact whose numbers are out of range is not
        // stored (it would poison the read models) and its once-only writes are skipped.
        Err(FactError::Data(field, _)) => {
            tracing::warn!(kind, field, session = %live.session_id, "progression fact out of range, not stored");
            Ok(false)
        }
        Err(e) => Err(fact_error(e)),
    }
}

const HARDCORE: &str = "hardcore";

/// The ticket's mode as the gates hold it: `noob`, else `hardcore` (any other value is gated as
/// Hardcore).
fn gate_mode(ctx: &Ctx<'_>) -> &'static str {
    if ctx.req.ticket.claims.mode == "noob" {
        "noob"
    } else {
        HARDCORE
    }
}

/// OD-178: whether a gated ticket's creation (`method`, for car `car`) is refused: a car the game
/// cannot have or a traffic car ([`gates::creation_car`]). A refusal is recorded once as a
/// `creation` fact (`method`, `carId`, `accepted: false`, `refusal`; EVENTS.md §3.3), never struck
/// (no creation rule strikes); the caller answers as its moderation block does. `false` on a noob
/// ticket (OD-52) and for an accepted car.
pub(crate) async fn creation_refused(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    method: &str,
    car: i64,
) -> Result<bool, ApiError> {
    if !gated(ctx) {
        return Ok(false);
    }
    let Err(r) = gates::creation_car(Catalog::fh2(), car) else {
        return Ok(false);
    };
    refuse_creation(conn, ctx, method, car, &r).await?;
    Ok(true)
}

/// Records a gated creation's refusal `r` once as a `creation` fact (`method`, `carId`,
/// `accepted: false`, `refusal`; EVENTS.md §3.3), never struck (OD-178).
pub(crate) async fn refuse_creation(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    method: &str,
    car: i64,
    r: &Refusal,
) -> Result<(), ApiError> {
    let mut fact = Map::new();
    fact.insert("method".into(), method.into());
    fact.insert("carId".into(), car.into());
    fact.insert("accepted".into(), false.into());
    fact.insert("refusal".into(), r.rule.into());
    record(conn, ctx, "creation", fact).await?;
    tracing::info!(
        session = %ctx.req.ticket.live.session_id,
        method,
        rule = r.rule,
        "a creation refused (OD-178)"
    );
    Ok(())
}

/// The player card as kept: on a gated ticket ([`gated`]) its career level is at most the game's
/// ([`gates::card_level`], OD-177) and a badge or title id the game does not list is its fallback,
/// UnlockId 1 ([`gates::card_ids`], OD-178 item 4); a noob ticket's is kept as sent (OD-52).
fn held_card(ctx: &Ctx<'_>, mut card: gameplane::Card) -> gameplane::Card {
    if gated(ctx) {
        card.career_level = gates::card_level(card.career_level);
        (card.badge_id, card.player_title_id) =
            gates::card_ids(card.badge_id, card.player_title_id);
    }
    card
}

/// Whether the integrity gates apply to the ticket's profile ([`gate_mode`] is hardcore): the one
/// rule of the profile sync's gates and the universe's (scores, XP).
pub(crate) fn gated(ctx: &Ctx<'_>) -> bool {
    gate_mode(ctx) == HARDCORE
}

/// The ticket's profile in `mode`, whose baseline the gates read and move.
fn subject<'a>(ctx: &Ctx<'a>, mode: &'a str) -> integrity::Subject<'a> {
    integrity::Subject {
        game: ctx.req.ticket.game(),
        profile_id: ctx.req.ticket.live.profile_id,
        mode,
    }
}

/// A gate's refusal, with the guardrails' assessment that made it when there is one.
type Refused = (Refusal, Option<Box<Assessment>>);

/// The profile gates' verdict on a merged profile: Noob, technical soundness only
/// ([`gates::progress_numbers`], no assessment); Hardcore, the game's limits too
/// ([`gates::progress`]) and the guardrails' assessment of the growth since the baseline
/// ([`gates::assess_profile`]; the caller made sure a Hardcore baseline exists,
/// [`baseline_from_seed`]): a ceiling refuses, and a score of 60 or more with `enforce`.
fn profile_verdict(
    mode: &str,
    merged: &Json,
    base: &integrity::Baselines,
    enforce: bool,
) -> Result<(gates::Progress, Option<Assessment>), Refused> {
    if mode != HARDCORE {
        return gates::progress_numbers(merged)
            .map(|p| (p, None))
            .map_err(|r| (r, None));
    }
    let progress = gates::progress(merged).map_err(|r| (r, None))?;
    let previous = base.profile.map(|b| gates::Baseline {
        credits: b.numbers.credits,
        xp: b.numbers.xp,
        level: b.numbers.level,
        winnings: b.numbers.winnings,
        synced_ms: b.synced_at.millis(),
        rates: b.rates,
        level_high: b.level_high,
        judged: b.judged,
    });
    let assessment = gates::assess_profile(
        previous.as_ref(),
        &progress,
        gates::Sections::of(merged),
        base.now.millis(),
    );
    match assessment.refusal(enforce) {
        Some(r) => Err((r.into(), Some(Box::new(assessment)))),
        None => Ok((progress, Some(assessment))),
    }
}

fn profile_numbers(p: &gates::Progress) -> integrity::ProfileNumbers {
    integrity::ProfileNumbers {
        credits: p.credits,
        xp: p.xp,
        level: p.level,
        winnings: p.winnings,
        top_speed_mps: p.top_speed,
    }
}

/// The two synced halves of a profile, as the gates, their facts, strikes and assessments name
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Synced {
    /// `OnlineProfile.UpdateAll`.
    Profile,
    /// `OnlineProfile.UpdateAllTables`.
    Tables,
}

impl Synced {
    /// The progression fact's kind (EVENTS.md §3.3).
    fn kind(self) -> &'static str {
        match self {
            Synced::Profile => "profile_sync",
            Synced::Tables => "tables_sync",
        }
    }

    /// The strike's source (`integrity_strike.source`).
    fn strike_source(self) -> &'static str {
        match self {
            Synced::Profile => "profile_gate",
            Synced::Tables => "garage",
        }
    }

    /// The baseline's half (its refusal episodes).
    fn half(self) -> integrity::Half {
        match self {
            Synced::Profile => integrity::Half::Profile,
            Synced::Tables => integrity::Half::Garage,
        }
    }

    /// The assessment's source.
    fn assessed(self) -> integrity::AssessmentSource {
        match self {
            Synced::Profile => integrity::AssessmentSource::ProfileSync,
            Synced::Tables => integrity::AssessmentSource::TablesSync,
        }
    }
}

/// Records the guardrails' assessment of an accepted sync (no refusal) or of a refused one, on
/// the request's transaction (OD-62, `integrity_assessment`); the caller records only when the
/// request's fact is new, so a replay records nothing.
async fn record_assessment(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    synced: Synced,
    assessment: &Assessment,
    refusal: Option<&Refusal>,
) -> Result<(), ApiError> {
    let code = refusal.map(|r| r.rule.to_ascii_uppercase());
    integrity::record_assessment_in(
        conn,
        subject(ctx, gate_mode(ctx)),
        ctx.req.ticket.live.session_id,
        synced.assessed(),
        assessment,
        code.as_deref(),
    )
    .await?;
    if assessment.would_block() || refusal.is_some() {
        tracing::info!(
            session = %ctx.req.ticket.live.session_id,
            kind = synced.kind(),
            score = assessment.score,
            reasons = ?assessment.reasons.iter().map(|r| r.code()).collect::<Vec<_>>(),
            refused = refusal.map(|r| r.rule),
            "a Hardcore sync assessed at or above the block score, or refused"
        );
    }
    Ok(())
}

/// The missing `half` of a Hardcore profile's baseline from the game's active seed, with no recent
/// growth (rates 0; OD-60: never from a sync; `fi_db::seeds`), on the request's transaction under
/// [`integrity::lock_in`]. `false` when the half is missing and the game has no seed: the caller
/// stores nothing and acknowledges the sync as the free behaviour does (logged once per process).
async fn baseline_from_seed(
    conn: &mut PgConnection,
    me: integrity::Subject<'_>,
    half: integrity::Half,
) -> Result<bool, ApiError> {
    let base = integrity::baselines_in(conn, me).await?;
    let missing = match half {
        integrity::Half::Profile => base.profile.is_none(),
        integrity::Half::Garage => base.garage.is_none(),
    };
    if !missing {
        return Ok(true);
    }
    let seed = match seeds::active_in(conn, me.game, seeds::HARDCORE).await {
        Ok(seed) => seed,
        Err(seeds::SeedError::Db(e)) => return Err(e.into()),
        Err(e) => {
            tracing::error!(error = %e, "the active seed cannot be read");
            return Err(ApiError::unavailable());
        }
    };
    let Some(seed) = seed else {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                game = me.game,
                "no Hardcore seed: an unseeded Hardcore profile's syncs are acknowledged and not \
                 stored (fi-cli seed-import)"
            );
        }
        return Ok(false);
    };
    match half {
        integrity::Half::Profile => seeds::profile_baseline_from_in(conn, me, &seed).await?,
        integrity::Half::Garage => seeds::garage_baseline_from_in(conn, me, &seed).await?,
    };
    tracing::info!(profile = %me.profile_id, ?half, "a baseline half from the seed");
    Ok(true)
}

/// A new top speed record (above the last accepted one by more than
/// [`gates::TOP_SPEED_RECORD_STEP`]) beyond the stored garage's stock cars: logged as a signal,
/// never refused or struck (the Beta's hotfix 2026-10-18; CLOUD_SAVES.md §7.1 rule 5).
async fn top_speed_signal(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    base: &integrity::Baselines,
    progress: &gates::Progress,
) -> Result<(), ApiError> {
    let record = base.profile.map_or(0.0, |b| b.numbers.top_speed_mps);
    if progress.top_speed <= record + gates::TOP_SPEED_RECORD_STEP {
        return Ok(());
    }
    let owner = ctx.owner();
    let cars = integrity::garage_car_ids_in(conn, owner.game, owner.profile_id).await?;
    if let Err(signal) = gates::check_top_speed(Catalog::fh2(), &cars, progress.top_speed) {
        tracing::warn!(
            session = %owner.session_id,
            rule = signal.rule,
            "a top speed record above the garage's stock cars (a signal, not refused)"
        );
    }
    Ok(())
}

/// The keys of the garage rows an UpdateAllTables brings and of the ones it deletes; `None` when
/// it carries no garage (no `garageCars` row and no `deleteGarageCars` key).
/// The rule id of a `garageCars` merge refused by OD-175's structural cap (the `tables_sync`
/// fact's `garageRefusal`).
pub const GARAGE_KEYS_RULE: &str = "garage_keys";

/// OD-175: keeps only the cars the game can have (car packs included) in an upload's `garageCars`
/// rows; a garage table left with
/// neither rows nor deletions is dropped (nothing to merge or measure). Returns the rows dropped.
fn keep_known_garage(tables: &mut Vec<forza::TableUpdate>) -> usize {
    let mut dropped = 0;
    for t in tables.iter_mut() {
        if t.name == integrity::GARAGE_TABLE {
            dropped += gates::keep_known_cars(Catalog::fh2(), &mut t.rows);
        }
    }
    tables.retain(|t| {
        t.name != integrity::GARAGE_TABLE || !t.rows.is_empty() || !t.deletes.is_empty()
    });
    dropped
}

/// What [`keep_plausible_rows`] dropped, per table (the `tables_sync` fact's counts).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Plausible {
    parts: usize,
    track_stats: usize,
    drivatar_track_stats: usize,
    race_feats: usize,
}

impl Plausible {
    fn fields(self) -> [(&'static str, usize); 4] {
        [
            ("droppedParts", self.parts),
            ("droppedTrackStats", self.track_stats),
            ("droppedDrivatarTrackStats", self.drivatar_track_stats),
            ("droppedRaceFeats", self.race_feats),
        ]
    }
}

/// OD-178: keeps the plausible rows of a gated UpdateAllTables' `purchasedParts`
/// ([`gates::keep_known_parts`], the garage cars from the upload's `garageCars` over the stored
/// garage), `trackStats` ([`gates::keep_plausible_track_stats`]), `drivatarTrackStats` and
/// `raceFeats` ([`gates::keep_non_negative`]); a table this step left with neither rows nor
/// deletions is dropped, any other table (one sent empty included) is kept as it came.
async fn keep_plausible_rows(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    tables: &mut Vec<forza::TableUpdate>,
) -> Result<Plausible, ApiError> {
    let catalog = Catalog::fh2();
    let mut out = Plausible::default();
    let mut garage: std::collections::BTreeMap<i64, i64> = tables
        .iter()
        .filter(|t| t.name == integrity::GARAGE_TABLE)
        .flat_map(|t| t.rows.values())
        .filter_map(|r| Some((r["id"].as_i64()?, r["carId"].as_i64()?)))
        .collect();
    let wanted: Vec<String> = tables
        .iter()
        .filter(|t| t.name == PARTS_TABLE)
        .flat_map(|t| t.rows.values())
        .filter_map(|r| r["garageId"].as_i64())
        .filter(|g| !garage.contains_key(g))
        .map(|g| g.to_string())
        .collect();
    if !wanted.is_empty() {
        let owner = ctx.owner();
        for (key, car) in
            integrity::garage_cars_of_in(conn, owner.game, owner.profile_id, &wanted).await?
        {
            if let Ok(id) = key.parse::<i64>() {
                garage.entry(id).or_insert(car);
            }
        }
    }
    // Only a table this step emptied is left out (OD-178 follow-up): a table the game sent empty, a
    // `garageCars` above all, is applied and measured exactly as before.
    let mut emptied: Vec<String> = Vec::new();
    for t in tables.iter_mut() {
        let had_rows = !t.rows.is_empty();
        match t.name.as_str() {
            PARTS_TABLE => {
                out.parts += gates::keep_known_parts(
                    catalog,
                    fi_gamedb::parts::PartPrices::fh2(),
                    &garage,
                    &mut t.rows,
                )
            }
            "trackStats" => {
                out.track_stats += gates::keep_plausible_track_stats(catalog, &mut t.rows)
            }
            "drivatarTrackStats" => {
                out.drivatar_track_stats += gates::keep_non_negative(&mut t.rows)
            }
            "raceFeats" => out.race_feats += gates::keep_non_negative(&mut t.rows),
            _ => {}
        }
        if had_rows && t.rows.is_empty() && t.deletes.is_empty() {
            emptied.push(t.name.clone());
        }
    }
    tables.retain(|t| !emptied.contains(&t.name));
    if out != Plausible::default() {
        tracing::info!(
            session = %ctx.req.ticket.live.session_id,
            parts = out.parts,
            track_stats = out.track_stats,
            drivatar_track_stats = out.drivatar_track_stats,
            race_feats = out.race_feats,
            "implausible table rows dropped (OD-178)"
        );
    }
    Ok(out)
}

/// The UpdateAllTables table of purchased parts.
const PARTS_TABLE: &str = "purchasedParts";

fn garage_keys(tables: &[forza::TableUpdate]) -> Option<(Vec<String>, Vec<String>)> {
    tables
        .iter()
        .find(|t| t.name == integrity::GARAGE_TABLE)
        .map(|t| (t.rows.keys().cloned().collect(), t.deletes.clone()))
}

/// A sync a gate refused, which the caller answers exactly as an accepted one (the game cannot
/// tell) and of which nothing is stored: its fact is recorded with `accepted: false` and the rule
/// in `refusal` (so a replayed request id stays a replay), with the guardrails' assessment that
/// refused it, if any. A Hardcore profile is struck (the half's strike source) once per refusal
/// episode of the half: only when this fact is new and this refusal opens the episode (the half's
/// previous sync was accepted, or there was none); later refusals of the episode are recorded and
/// not struck, until an accepted sync of the half ends it. A Noob profile is never struck.
async fn refuse(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    synced: Synced,
    mut fact: Map<String, Json>,
    (refusal, assessment): &Refused,
) -> Result<(), ApiError> {
    let kind = synced.kind();
    fact.insert("accepted".into(), false.into());
    fact.insert("refusal".into(), refusal.rule.into());
    if !record(conn, ctx, kind, fact).await? {
        return Ok(());
    }
    if let Some(a) = assessment {
        record_assessment(conn, ctx, synced, a, Some(refusal)).await?;
    }
    let mode = gate_mode(ctx);
    let noob = mode != HARDCORE;
    let struck =
        !noob && integrity::open_episode_in(conn, subject(ctx, mode), synced.half()).await?;
    // A refusal by score judged its fact checks' values: kept with the episode, so the same value
    // is not refused again against the same accepted state (CLOUD_SAVES.md §7.1 rule 4).
    if let (false, Synced::Profile, Some(a)) = (noob, synced, assessment)
        && refusal.rule == fi_guard::ANOMALY_RULE
    {
        let me = subject(ctx, mode);
        let before = integrity::baselines_in(conn, me)
            .await?
            .profile
            .map(|b| b.judged)
            .unwrap_or_default();
        integrity::judge_in(conn, me, &a.judged_after_refusal(before)).await?;
    }
    if struck {
        integrity::strike_rpc_in(
            conn,
            ctx.req.ticket.live.profile_id,
            synced.strike_source(),
            &refusal.reason,
        )
        .await?;
    }
    tracing::info!(
        session = %ctx.req.ticket.live.session_id,
        kind,
        rule = refusal.rule,
        noob,
        struck,
        "a sync refused by an integrity gate"
    );
    Ok(())
}

/// The stored methods, on the request's transaction.
async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    xls: bool,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    if super::liveries::is_livery(xls, short, method) {
        return super::liveries::stored(conn, ctx, method, params).await;
    }
    let owner = ctx.owner();
    Ok(match (xls, short, method) {
        // The ticket's profile is who logs on, whatever XUID and name the game reports.
        (false, "User", "Logon") => {
            let card = held_card(ctx, forza::logon_card(&params));
            let tables =
                gameplane::record_logon_in(conn, owner, card, forza::logon_title_version(&params))
                    .await?;
            // Every ticket's tables are stored (OD-128), so every ticket gets its checkpoint.
            let p = player_of(player(conn, ctx).await?);
            let badge = super::universe::clubs::own_badge(conn, ctx).await?;
            Ok(forza::logon(&p, &badge, tables, &params, ctx.lang))
        }
        (true, "User", "GetUserData") => {
            let p = player_of(player(conn, ctx).await?);
            let badge = super::universe::clubs::own_badge(conn, ctx).await?;
            Ok(forza::get_user_data(&p, &badge))
        }
        (false, "User", "SetPlayerCardInfo") => {
            gameplane::set_card_in(conn, owner, held_card(ctx, forza::card_of(&params))).await?;
            Ok(json!({}))
        }
        // The profile gates (GAME_PLATFORM.md §3a "Supporter universe", RPC gates): the merged
        // profile's numbers, technically sound and (Hardcore) within the game's limits, then the
        // guardrails' assessment of the growth since the baseline (OD-62), which for Hardcore
        // always exists: the seed's numbers when the profile had none ([`baseline_from_seed`],
        // OD-60), never the sync's. Refused (a ceiling; a score of 60 or more with the switch
        // on): answered the same, nothing stored ([`refuse`]). Every assessment is recorded.
        (false, "OnlineProfile", "UpdateAll") => {
            let update = match params {
                Json::Object(map) => map,
                _ => Map::new(),
            };
            let mode = gate_mode(ctx);
            let me = subject(ctx, mode);
            // Concurrent syncs of the profile are measured one after the other. On the request's
            // transaction, not the savepoint below: a rolled-back merge must not release it.
            integrity::lock_in(conn, owner.game, owner.profile_id).await?;
            // OD-60: a Hardcore profile's baseline never comes from a sync. A missing half is the
            // game's seed (on the request's transaction, so it stays when this sync is refused);
            // without a seed nothing is stored, acknowledged as the free behaviour is.
            if mode == HARDCORE && !baseline_from_seed(conn, me, integrity::Half::Profile).await? {
                return Ok(Ok(forza::updated()));
            }
            // The gates and the fact read the merged profile (a section the game marks unchanged
            // keeps the stored one), so the merge runs first, in a savepoint rolled back when the
            // sync is refused or its fact already existed: a replay writes nothing.
            let mut once = conn.begin().await?;
            let merged = gameplane::merge_profile_in(&mut once, owner, &update)
                .await
                .map_err(ApiError::from_stored)?;
            let base = integrity::baselines_in(&mut once, me).await?;
            match profile_verdict(mode, &merged, &base, ctx.guard_enforce) {
                Ok((progress, assessment)) => {
                    if record(
                        &mut once,
                        ctx,
                        Synced::Profile.kind(),
                        forza::profile_sync_fact(&merged),
                    )
                    .await?
                    {
                        if mode == HARDCORE {
                            top_speed_signal(&mut once, ctx, &base, &progress).await?;
                        }
                        if let Some(a) = &assessment {
                            record_assessment(&mut once, ctx, Synced::Profile, a, None).await?;
                        }
                        // A section the game has not sent yet would read as 0: only a complete
                        // profile seeds or moves the baseline (with the rates after it). Either
                        // way the accepted sync ends the profile half's refusal episode.
                        if gates::complete(&merged) {
                            let rates = assessment.as_ref().map(|a| a.rates).unwrap_or_default();
                            let level_high = assessment.as_ref().and_then(|a| a.level_high);
                            integrity::set_profile_baseline_in(
                                &mut once,
                                me,
                                &profile_numbers(&progress),
                                &rates,
                                level_high,
                            )
                            .await?;
                        } else {
                            integrity::close_episode_in(&mut once, me, integrity::Half::Profile)
                                .await?;
                        }
                        once.commit().await?;
                    } else {
                        once.rollback().await?;
                    }
                }
                Err(refused) => {
                    once.rollback().await?;
                    let fact = forza::profile_sync_fact(&merged);
                    refuse(conn, ctx, Synced::Profile, fact, &refused).await?;
                }
            }
            // OD-239: an Azimuth (no-line) career has no save commit to take its numbers from,
            // so its stored sync fills `profile_progress` (as FM5's does); a career with a line
            // keeps the save commits' numbers.
            if !(ctx.req.ticket.claims.line && ctx.req.ticket.live.cloud) {
                fi_db::fm5::sync_progress_of_in(conn, owner.game, owner.profile_id).await?;
            }
            Ok(forza::updated())
        }
        // The garage gates: real cars only (every mode, the first sync included) and, Hardcore,
        // the guardrails' assessment of the cars added since the garage half of the baseline (the
        // seed's when missing, [`baseline_from_seed`], OD-60; cars only score, OD-62); only an
        // upload that carries a garage is measured. Refused: answered the same, no table applied,
        // no checkpoint ([`refuse`]). Then OD-175's structural rule, every mode: only cars the
        // game can have are stored ([`gates::keep_known_cars`], car packs kept; the request's
        // duplicate keys are already one row each), and a merge that would leave more than
        // [`gates::MAX_GARAGE_KEYS`] keys is refused for `garageCars` alone: the other tables and
        // the checkpoint are applied, the reply is the same, the fact says so, no strike.
        (false, "OnlineProfile", "UpdateAllTables") => {
            let checkpoint = params["transactionId"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok());
            let mode = gate_mode(ctx);
            let me = subject(ctx, mode);
            // Concurrent uploads of the profile are measured one after the other, the first one
            // included (no garage row to lock yet).
            integrity::lock_in(conn, owner.game, owner.profile_id).await?;
            // OD-60, as UpdateAll: a missing garage half is the seed's, never this upload's.
            if mode == HARDCORE && !baseline_from_seed(conn, me, integrity::Half::Garage).await? {
                return Ok(Ok(forza::updated()));
            }
            let cars = gates::check_garage_cars(Catalog::fh2(), &params);
            let mut tables = forza::table_updates(params);
            // The fact carries the upload's tables and rows (EVENTS.md §3.3), then what the
            // structural rule did.
            let mut fact = forza::tables_sync_fact(&tables);
            let dropped = keep_known_garage(&mut tables);
            fact.insert("droppedCars".into(), dropped.into());
            // OD-178, gated tickets: implausible rows of the parts and track tables are dropped
            // (the rest applied, the reply unchanged, no strike); the fact counts them.
            let plausible = if gated(ctx) {
                keep_plausible_rows(conn, ctx, &mut tables).await?
            } else {
                Plausible::default()
            };
            for (field, n) in plausible.fields() {
                fact.insert(field.into(), n.into());
            }
            fact.insert("garageRefusal".into(), Json::Null);
            // The garage's size after the upload with its assessment (Hardcore); `None`: no
            // garage in the upload, nothing measured and the garage's baseline left as it is.
            type Garage = Option<(i64, Option<Assessment>)>;
            // A garage past the structural cap: its size, the table left out.
            let mut capped: Option<i64> = None;
            let verdict: Result<Garage, Refused> = match (cars, garage_keys(&tables)) {
                (Err(refusal), _) => Err((refusal, None)),
                (Ok(()), None) => Ok(None),
                (Ok(()), Some((rows, deletes))) => {
                    let after = integrity::garage_size_after_in(
                        conn,
                        owner.game,
                        owner.profile_id,
                        &rows,
                        &deletes,
                    )
                    .await?;
                    if after > gates::MAX_GARAGE_KEYS {
                        capped = Some(after);
                        // Not a sync of the garage half: nothing measured, the baseline stays.
                        Ok(None)
                    } else {
                        let base = integrity::baselines_in(conn, me).await?;
                        if mode == HARDCORE {
                            let previous = base.garage.map(|g| gates::GarageBaseline {
                                cars: g.cars,
                                synced_ms: g.synced_at.millis(),
                                rate: g.rate,
                            });
                            let a =
                                gates::assess_garage(previous.as_ref(), after, base.now.millis());
                            match a.refusal(ctx.guard_enforce) {
                                Some(r) => Err((r.into(), Some(Box::new(a)))),
                                None => Ok(Some((after, Some(a)))),
                            }
                        } else {
                            Ok(Some((after, None)))
                        }
                    }
                }
            };
            if let Some(after) = capped {
                tables.retain(|t| t.name != integrity::GARAGE_TABLE);
                fact.insert("garageRefusal".into(), GARAGE_KEYS_RULE.into());
                fact.insert("garageKeys".into(), after.into());
                tracing::warn!(
                    session = %ctx.req.ticket.live.session_id,
                    keys = after,
                    "garageCars merge refused: past the structural cap (OD-175)"
                );
            }
            match verdict {
                // Recorded first: a replayed request id writes nothing.
                Ok(garage) => {
                    if record(conn, ctx, Synced::Tables.kind(), fact).await? {
                        for t in &tables {
                            gameplane::apply_table_in(conn, owner, &t.name, &t.rows, &t.deletes)
                                .await
                                .map_err(ApiError::from_stored)?;
                        }
                        if let Some(id) = checkpoint {
                            gameplane::set_tables_checkpoint_in(conn, owner, id).await?;
                        }
                        // Only an upload that carries a garage is a sync of the garage half: it
                        // moves the half and ends its refusal episode; one without leaves both.
                        if let Some((after, assessment)) = garage {
                            if let Some(a) = &assessment {
                                record_assessment(conn, ctx, Synced::Tables, a, None).await?;
                            }
                            let rate = assessment.map_or(0.0, |a| a.rates.cars);
                            integrity::set_garage_baseline_in(conn, me, after, rate).await?;
                        }
                    }
                }
                Err(refused) => {
                    refuse(conn, ctx, Synced::Tables, fact, &refused).await?;
                }
            }
            // OD-239: an Azimuth (no-line) career has no save commit to take its numbers from,
            // so its stored sync fills `profile_progress` (as FM5's does); a career with a line
            // keeps the save commits' numbers.
            if !(ctx.req.ticket.claims.line && ctx.req.ticket.live.cloud) {
                fi_db::fm5::sync_progress_of_in(conn, owner.game, owner.profile_id).await?;
            }
            Ok(forza::updated())
        }
        (true, "Transaction", "Reconcile") => {
            let kept = match params["transactionId"]
                .as_str()
                .and_then(target::transaction_id)
            {
                Some(id) => gameplane::reply_in(conn, owner.game, owner.profile_id, id).await?,
                None => None,
            };
            forza::reconcile(kept.as_deref())
        }
        // The Supporters' universe (SER-M3 step 6): only reached on saves, where
        // `forza::served` lists it.
        _ => return super::universe::stored(conn, ctx, xls, short, method, params).await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_reads_the_headers_and_the_served_table() {
        for features in [Features::Supporter, Features::Free] {
            let r = lookup(
                features,
                "Forza.WebServices.User, Forza.WebServices",
                "Logon",
            )
            .unwrap();
            assert_eq!(
                (r.key, r.short, r.xls, r.served),
                ("Forza.WebServices.User", "User", false, Served::Store)
            );
            let r = lookup(features, "Xls.WebServices.Notifications", "RetrieveForUser").unwrap();
            // The Supporters' inbox is stored (SER-M3 step 7); the free one stays empty.
            let inbox = match features {
                Features::Supporter => Served::Store,
                Features::Free => Served::Pure,
            };
            assert_eq!((r.xls, r.served), (true, inbox));
            assert_eq!(
                lookup(features, "Forza.WebServices.User", "NoSuchMethod"),
                None
            );
            assert_eq!(
                lookup(features, "Forza.WebServices.Scoreboard", "GetRollupRows"),
                None,
                "not served"
            );
            assert_eq!(lookup(features, "No.Such.Class", "Logon"), None);
        }
        // The boards (SER-M3 step 6): stored on saves, never served on matchmaking.
        let scoreboard = "Forza.WebServices.Scoreboard, Forza.WebServices";
        for method in ["GetRows", "SubmitScore", "UploadFile", "GetRival"] {
            assert_eq!(
                lookup(Features::Supporter, scoreboard, method).map(|r| r.served),
                Some(Served::Store),
                "{method}"
            );
            assert_eq!(lookup(Features::Free, scoreboard, method), None, "{method}");
        }
        // The profile sync and the drivatars: stored for every ticket (OD-128).
        let profile = "Forza.WebServices.OnlineProfile, Forza.WebServices";
        let drivatar = "Forza.WebServices.Drivatar, Forza.WebServices";
        for (class, method) in [
            (profile, "UpdateAll"),
            (profile, "UpdateAllTables"),
            (drivatar, "GetDrivatarsForCache"),
            (drivatar, "GetDrivatarInformationForRace"),
            (drivatar, "SaveDrivatarInformation"),
        ] {
            for features in [Features::Supporter, Features::Free] {
                assert_eq!(
                    lookup(features, class, method).map(|r| r.served),
                    Some(Served::Store),
                    "{method}"
                );
            }
        }
    }

    /// OD-129 replaced OD-52's narrowing: what a ticket is served is its deployment's features, so
    /// on saves every ticket (a noob, a free or a lapsed one) gets the universe and its stored
    /// data; only the free deployment (matchmaking) keeps the free allowlist's own answers.
    #[test]
    fn the_universe_follows_the_deployment_whatever_the_ticket() {
        let claims = |tier: &str, mode: &str| {
            let mut c = crate::game::ticket::tests::claims("vip", Some("fh2"), 1_792_245_600);
            c.tier = tier.into();
            c.mode = mode.into();
            // What the gate mints (OD-239: Hardcore only, whatever the tier).
            c.line = mode == "hardcore";
            c
        };
        for (tier, mode, universe) in [
            ("vip", "hardcore", true),
            ("vip", "noob", true),
            ("free", "hardcore", true),
            ("free", "noob", true),
            ("", "hardcore", true),
        ] {
            let features = Features::Supporter.for_ticket(&claims(tier, mode), true);
            for (class, method) in [
                ("Forza.WebServices.Scoreboard", "GetRows"),
                ("Forza.WebServices.Scoreboard", "UploadFile"),
                ("Xls.WebServices.Notifications", "SendTextNotification"),
            ] {
                assert_eq!(
                    lookup(features, class, method).is_some(),
                    universe,
                    "{tier} {mode} {method}"
                );
            }
            assert!(lookup(features, "Forza.WebServices.User", "Logon").is_some());
            // The drivatars, whatever the tier and mode (OD-128).
            assert!(
                lookup(
                    features,
                    "Forza.WebServices.Drivatar",
                    "GetDrivatarsForCache"
                )
                .is_some()
            );
            // The methods both sets serve: the stored data for every ticket on saves (OD-129).
            for (class, method) in [
                ("Forza.WebServices.User", "GetCompareStatsForRelatedUsers"),
                ("Forza.WebServices.User", "GetCompareStatsByBucket"),
                ("Xls.WebServices.Notifications", "RetrieveForUser"),
            ] {
                assert_eq!(
                    lookup(features, class, method).map(|r| r.served),
                    Some(if universe {
                        Served::Store
                    } else {
                        Served::Pure
                    }),
                    "{tier} {mode} {method}"
                );
            }
        }
        // The free deployment answers the compare stats itself (empty), whatever the ticket.
        for method in ["GetCompareStatsForRelatedUsers", "GetCompareStatsByBucket"] {
            assert_eq!(
                Features::Free.for_ticket(&claims("vip", "hardcore"), true),
                Features::Free
            );
            assert_eq!(
                lookup(Features::Free, "Forza.WebServices.User", method).map(|r| r.served),
                Some(Served::Pure)
            );
        }
    }

    #[test]
    fn bodies_are_capped_per_method() {
        assert_eq!(body_cap("Forza.WebServices.User", "Logon"), RPC_BODY);
        assert_eq!(
            body_cap("Forza.WebServices.OnlineProfile", "UpdateAllTables"),
            RPC_BODY
        );
        for (key, method, cap) in LARGE_BODIES {
            assert_eq!(body_cap(key, method), (*cap).min(super::super::MAX_BODY));
        }
        // A ghost upload: the largest ghost kept and its scoreId and length.
        let ghost = body_cap("Forza.WebServices.Scoreboard", "UploadFile");
        assert!(ghost > fi_db::scores::MAX_GHOST + 20 && ghost < RPC_BODY);
    }
}
