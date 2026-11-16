//! In-game free roam (migration `20261019000500_freeroam.sql`; docs/contracts/MATCHMAKING_API.md
//! §4a): the rows behind the game's sessiondirectory (MPSD sessions) and momatch (SmartMatch
//! tickets), and the routing decision `world_for` over matchmaking's `world` and `mm_grant`. The
//! game-plane library (`fi_surfaces::game::freeroam`) shapes the documents; this module stores them, so
//! any instance answers any request.
//!
//! Sessions and tickets belong to the profile that created them while they live: for anybody else
//! a live row does not exist. Expiry is a filter on `expires_at`; ending a row sets it to now
//! (fi_runtime holds no DELETE); [`purge`] (fi_maintenance) removes long-expired rows.
//!
//! The caller's world ([`world_for_in`]), the first that applies: the world the caller hosts
//! (`127.0.0.1:<port>`); the world of the caller's live grant (`issued` and unexpired, or `used`)
//! on an open world; with [`RouteOptions::auto`] the best open world the caller could be granted;
//! then [`RouteOptions::fallback`]. Reads only: taking a grant for an automatic choice is the
//! caller's (`fi_db::matchmaking::join`, its own transaction).

use crate::identity::IdentityError;
use crate::matchmaking;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use uuid::Uuid;

/// Seconds a session lives after its last read or write.
pub const SESSION_TTL_SECS: i64 = 30 * 60;
/// Seconds a ticket lives after its creation.
pub const TICKET_TTL_SECS: i64 = 30 * 60;
/// Live sessions (and, separately, tickets) one profile keeps; a new one expires the oldest.
pub const MAX_LIVE_PER_OWNER: i64 = 16;
/// Longest key part (scid, template, name, hopper).
pub const MAX_KEY_PART: usize = 128;
/// Rows [`purge`] deletes per statement.
pub const PURGE_BATCH: i64 = 5_000;

/// Who asks: the ticket's profile and the request's address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The ticket's game.
    pub game: String,
    /// Profile.
    pub profile_id: Uuid,
    /// Account.
    pub account_id: Uuid,
    /// Play session.
    pub session_id: Uuid,
    /// XUID.
    pub xuid: i64,
    /// `free` | `vip`.
    pub tier: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The caller's address as the platform sees it (canonical: an IPv4-mapped IPv6 is IPv4).
    pub ip: Option<IpAddr>,
}

impl Caller {
    /// The same caller for matchmaking's join.
    pub fn mm(&self) -> matchmaking::Caller {
        matchmaking::Caller {
            game: self.game.clone(),
            profile_id: self.profile_id,
            account_id: self.account_id,
            session_id: self.session_id,
            xuid: self.xuid,
            tier: self.tier.clone(),
            mode: self.mode.clone(),
        }
    }
}

/// How a world was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The caller hosts it.
    Hosting,
    /// The caller holds a live grant for it (the launcher's join; the host reports the caller in).
    Grant,
    /// The best open world for the caller (opt-in).
    Auto,
    /// The configured fallback world.
    Fallback,
}

impl Via {
    /// Stored form (`freeroam_ticket.via`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Via::Hosting => "hosting",
            Via::Grant => "grant",
            Via::Auto => "auto",
            Via::Fallback => "fallback",
        }
    }
}

/// Where the caller's game connects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// The relay's socket address as this caller reaches it.
    pub addr: SocketAddr,
    /// The world (`None` for the fallback).
    pub world_id: Option<Uuid>,
    /// How it was chosen.
    pub via: Via,
}

/// The deployment's routing options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteOptions {
    /// `FI_FREEROAM_AUTO`: consider the best open world (off by default).
    pub auto: bool,
    /// `FI_FREEROAM_WORLD`: the world for a caller with no other (unset by default).
    pub fallback: Option<SocketAddr>,
}

/// The address a joiner is sent to: the host's public IP, or its first LAN address when the
/// joiner shares that public IP (or the host was seen on loopback). Both IPs canonical.
pub fn joiner_address(
    host_ip: IpAddr,
    port: u16,
    lan: &[IpAddr],
    caller_ip: Option<IpAddr>,
) -> SocketAddr {
    joiner_address_with_ports(host_ip, port, port, lan, caller_ip)
}

/// Selects a routing hint using distinct public/mapped and local bind ports. Same-address/LAN
/// selection never authorizes a peer or bypasses its grant-scoped UDP socket proof.
pub fn joiner_address_with_ports(
    host_ip: IpAddr,
    public_port: u16,
    local_port: u16,
    lan: &[IpAddr],
    caller_ip: Option<IpAddr>,
) -> SocketAddr {
    let host_ip = host_ip.to_canonical();
    let same = caller_ip.map(|ip| ip.to_canonical()) == Some(host_ip);
    if (same || host_ip.is_loopback())
        && let Some(ip) = lan.first()
    {
        return SocketAddr::new(*ip, local_port);
    }
    // A shared public IP alone does not make that destination local: NAT/CGNAT still needs
    // the public mapping when no LAN address was announced.
    SocketAddr::new(
        host_ip,
        if host_ip.is_loopback() {
            local_port
        } else {
            public_port
        },
    )
}

/// A private address a host may announce as `endpoint.lan`: RFC 1918 and link-local IPv4, unique
/// local (`fc00::/7`) and link-local (`fe80::/10`) IPv6.
pub fn is_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The SQL of the grant step (tests EXPLAIN it): the caller's live seat in an open world, whatever
/// either's mod set (OD-181 retired OD-112's equal-key rule); an official world (OD-188/189) has no
/// host session to check.
pub const GRANT_WORLD_SQL: &str = "SELECT w.id, host(w.endpoint_ip) AS ip, w.endpoint_port AS port, coalesce(w.endpoint_local_port, w.endpoint_port) AS local_port, \
     ARRAY(SELECT host(a) FROM unnest(w.endpoint_lan) WITH ORDINALITY AS t(a, n) ORDER BY n) AS lan \
     FROM mm_grant g JOIN world w ON w.id = g.world_id \
       JOIN play_session ps ON ps.id = g.session_id JOIN profile pp ON pp.id = g.profile_id \
       LEFT JOIN play_session hs ON hs.id = w.host_session_id LEFT JOIN profile hp ON hp.id = w.host_profile_id \
     WHERE g.game = $1 AND g.profile_id = $2 AND g.session_id = $5 AND g.account_id = $6 \
       AND ((g.state = 'issued' AND g.expires_at > clock_timestamp()) OR g.state = 'used') \
       AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
       AND (w.mode = $3 OR w.any_mode) AND (w.audience <> 'supporters' OR $4) \
       AND ps.profile_id = g.profile_id AND ps.account_id = g.account_id AND ps.state = 'active' AND ps.game_until > clock_timestamp() \
       AND NOT ps.dedicated \
       AND (pp.mode = w.mode OR w.any_mode) AND pp.game = w.game \
       AND (w.host_kind = 'official' OR (hp.mode = w.mode AND hp.game = w.game \
         AND hs.profile_id = w.host_profile_id AND hs.account_id = w.host_account_id \
         AND hs.state = 'active' AND hs.game_until > clock_timestamp())) \
     ORDER BY g.issued_at DESC LIMIT 1";

/// The SQL of the automatic step: the best open world the caller could be granted, modded or not
/// (T-1110-NOMODMATCH: mods never decide a world), on the caller's side of the pineapple pool (OD-149: a
/// flagged Hardcore caller only pineapple worlds, a clean one none).
pub const AUTO_WORLD_SQL: &str = "SELECT w.id, host(w.endpoint_ip) AS ip, w.endpoint_port AS port, coalesce(w.endpoint_local_port, w.endpoint_port) AS local_port, \
     ARRAY(SELECT host(a) FROM unnest(w.endpoint_lan) WITH ORDINALITY AS t(a, n) ORDER BY n) AS lan \
     FROM world w JOIN profile p ON p.id = w.host_profile_id JOIN play_session hs ON hs.id = w.host_session_id \
     WHERE w.game = $1 AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
       AND hs.profile_id = w.host_profile_id AND hs.account_id = w.host_account_id \
       AND hs.state = 'active' AND hs.game_until > clock_timestamp() AND p.mode = w.mode AND p.game = w.game \
       AND EXISTS (SELECT 1 FROM play_session ps JOIN profile pp ON pp.id = ps.profile_id \
         WHERE ps.id = $7 AND ps.profile_id = $2 AND ps.account_id = $8 AND NOT ps.dedicated \
           AND ps.state = 'active' AND ps.game_until > clock_timestamp() AND pp.mode = $3 AND pp.game = $1) \
       AND (w.audience = 'everyone' OR (w.audience = 'supporters' AND $4)) AND (w.mode = $3 OR w.any_mode) \
       AND w.activity = 'freeroam' \
       AND w.host_profile_id <> $2 \
       AND w.pool = EXISTS (SELECT 1 FROM cheat_flag cf WHERE cf.profile_id = $2 AND cf.until > clock_timestamp() AND $3 = 'hardcore') \
       AND (w.allow IS NULL OR $5 = ANY(w.allow)) \
       AND NOT EXISTS (SELECT 1 FROM world_ban b WHERE b.game = w.game AND b.host_profile_id = w.host_profile_id \
             AND b.target_profile_id = $2 AND (b.until IS NULL OR b.until > clock_timestamp())) \
       AND NOT EXISTS (SELECT 1 FROM mm_grant k WHERE k.world_id = w.id AND k.profile_id = $2 AND k.state = 'kicked') \
       AND (CASE WHEN w.dedicated THEN 0 ELSE 1 END) + (SELECT count(*) FROM mm_grant g WHERE g.world_id = w.id AND (g.state = 'used' \
             OR (g.state = 'issued' AND g.expires_at > clock_timestamp()))) \
           + (SELECT count(*) FROM world_invite i WHERE i.world_id = w.id AND i.state = 'pending' \
             AND i.expires_at > clock_timestamp()) < w.max_players \
     ORDER BY (w.mode = $3) DESC, coalesce(w.endpoint_ip = $6::text::inet, false) DESC, \
       (SELECT count(*) FROM mm_grant s WHERE s.world_id = w.id AND s.state = 'used') DESC, w.announced_at DESC, p.xuid ASC \
     LIMIT 1";

fn joined_route(
    r: &sqlx::postgres::PgRow,
    caller: &Caller,
    via: Via,
) -> Result<Option<Route>, IdentityError> {
    let ip: String = r.try_get("ip")?;
    let Ok(host_ip) = ip.parse::<IpAddr>() else {
        return Ok(None);
    };
    let port: i32 = r.try_get("port")?;
    let Ok(port) = u16::try_from(port) else {
        return Ok(None);
    };
    let local_port: i32 = r.try_get("local_port")?;
    let Ok(local_port) = u16::try_from(local_port) else {
        return Ok(None);
    };
    let lan: Vec<String> = r.try_get("lan")?;
    let lan: Vec<IpAddr> = lan.iter().filter_map(|a| a.parse().ok()).collect();
    Ok(Some(Route {
        addr: joiner_address_with_ports(host_ip, port, local_port, &lan, caller.ip),
        world_id: Some(r.try_get("id")?),
        via,
    }))
}

/// The caller's world in the contract's order (module docs), on the caller's connection.
pub async fn world_for_in(
    conn: &mut PgConnection,
    caller: &Caller,
    options: &RouteOptions,
) -> Result<Option<Route>, IdentityError> {
    // The caller's live session, and whether it is flagged (OD-149).
    let live_caller: Option<(bool,)> = sqlx::query_as(
        "SELECT p.mode = 'hardcore' AND EXISTS (SELECT 1 FROM cheat_flag cf \
           WHERE cf.profile_id = p.id AND cf.until > clock_timestamp()) \
         FROM play_session s JOIN profile p ON p.id = s.profile_id \
         WHERE s.id = $1 AND s.profile_id = $2 AND s.account_id = $3 AND NOT s.dedicated \
           AND s.state = 'active' AND s.game_until > clock_timestamp() AND p.mode = $4 AND p.game = $5",
    )
    .bind(caller.session_id).bind(caller.profile_id).bind(caller.account_id).bind(&caller.mode).bind(&caller.game)
    .fetch_optional(&mut *conn).await?;
    let Some((flagged,)) = live_caller else {
        return Ok(None);
    };
    let hosted: Option<(Uuid, i32)> = sqlx::query_as(
        "SELECT w.id, coalesce(w.endpoint_local_port, w.endpoint_port) FROM world w \
           JOIN play_session s ON s.id = w.host_session_id JOIN profile p ON p.id = w.host_profile_id \
         WHERE w.game = $1 AND w.host_profile_id = $2 AND w.mode = $3 AND w.host_session_id = $4 \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() \
           AND s.profile_id = w.host_profile_id AND s.account_id = $5 AND s.account_id = w.host_account_id \
           AND s.state = 'active' AND s.game_until > clock_timestamp() AND p.mode = w.mode AND p.game = w.game",
    )
    .bind(&caller.game)
    .bind(caller.profile_id)
    .bind(&caller.mode)
    .bind(caller.session_id)
    .bind(caller.account_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((id, port)) = hosted
        && let Ok(port) = u16::try_from(port)
    {
        return Ok(Some(Route {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            world_id: Some(id),
            via: Via::Hosting,
        }));
    }
    let granted = sqlx::query(GRANT_WORLD_SQL)
        .bind(&caller.game)
        .bind(caller.profile_id)
        .bind(&caller.mode)
        .bind(true) // OD-129: `supporters` worlds are everyone's
        .bind(caller.session_id)
        .bind(caller.account_id)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(r) = granted
        && let Some(route) = joined_route(&r, caller, Via::Grant)?
    {
        return Ok(Some(route));
    }
    if options.auto {
        let best = sqlx::query(AUTO_WORLD_SQL)
            .bind(&caller.game)
            .bind(caller.profile_id)
            .bind(&caller.mode)
            .bind(true) // OD-129: `supporters` worlds are everyone's
            .bind(caller.xuid)
            .bind(caller.ip.map(|ip| ip.to_canonical().to_string()))
            .bind(caller.session_id)
            .bind(caller.account_id)
            .fetch_optional(&mut *conn)
            .await?;
        if let Some(r) = best
            && let Some(route) = joined_route(&r, caller, Via::Auto)?
        {
            return Ok(Some(route));
        }
    }
    // OD-149: a flagged session gets no fallback (the fixed world is no pineapple world). Mods
    // never decide it (T-1110-NOMODMATCH).
    if flagged {
        return Ok(None);
    }
    Ok(options.fallback.map(|addr| Route {
        addr,
        world_id: None,
        via: Via::Fallback,
    }))
}

/// [`world_for_in`] on a connection of its own (released before it returns).
pub async fn world_for(
    db: &PgPool,
    caller: &Caller,
    options: &RouteOptions,
) -> Result<Option<Route>, IdentityError> {
    let mut conn = db.acquire().await?;
    world_for_in(&mut conn, caller, options).await
}

/// The caller's gamertag as the platform shows it (the plain name, OD-120), `None` for an unknown profile.
pub async fn gamertag_in(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<Option<String>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT name FROM profile WHERE id = $1")
        .bind(profile)
        .fetch_optional(&mut *conn)
        .await?)
}

/// A session's key, lower case.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// Game.
    pub game: String,
    /// Service configuration id.
    pub scid: String,
    /// Session template.
    pub template: String,
    /// Session name.
    pub name: String,
}

/// A key part: 1 to [`MAX_KEY_PART`] letters, digits, `.`, `_` or `-`, lower-cased.
pub fn key_part(text: &str) -> Option<String> {
    let ok = (1..=MAX_KEY_PART).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ok.then(|| text.to_ascii_lowercase())
}

impl SessionKey {
    /// The key of `scid`/`template`/`name` in `game`; `None` when a part is malformed.
    pub fn new(game: &str, scid: &str, template: &str, name: &str) -> Option<Self> {
        Some(Self {
            game: game.to_owned(),
            scid: key_part(scid)?,
            template: key_part(template)?,
            name: key_part(name)?,
        })
    }
}

/// A session row as a write found it (locked).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSession {
    /// Who created it (meaningful only while live).
    pub owner_profile_id: Uuid,
    /// The document (`{}` for a row that never lived).
    pub doc: Value,
    /// Its revision.
    pub revision: i64,
    /// Inside its life: a dead row is a session that does not exist.
    pub live: bool,
    /// The private official world it is linked to (CR-S96).
    pub private_world_id: Option<Uuid>,
}

/// Locks the row of `key` for a write, inserting a dead placeholder when there is none, so two
/// first writers of one name serialize on the row (the second then sees the first's session).
/// The placeholder disappears with the transaction unless [`save_session_in`] writes it.
pub async fn lock_session_in(
    conn: &mut PgConnection,
    key: &SessionKey,
    writer: Uuid,
) -> Result<StoredSession, IdentityError> {
    // Twice at most: a purge may delete a long-dead row between the insert that found it and the
    // lock; the second insert then creates the placeholder.
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO freeroam_session (game, scid, template, name, owner_profile_id, doc, revision, expires_at) \
             VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, 0, clock_timestamp()) ON CONFLICT DO NOTHING",
        )
        .bind(&key.game)
        .bind(&key.scid)
        .bind(&key.template)
        .bind(&key.name)
        .bind(writer)
        .execute(&mut *conn)
        .await?;
        let row = sqlx::query(
            "SELECT owner_profile_id, doc, revision, expires_at > clock_timestamp() AS live, private_world_id FROM freeroam_session \
             WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4 FOR UPDATE",
        )
        .bind(&key.game)
        .bind(&key.scid)
        .bind(&key.template)
        .bind(&key.name)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(r) = row {
            return Ok(StoredSession {
                owner_profile_id: r.try_get("owner_profile_id")?,
                doc: r.try_get("doc")?,
                revision: r.try_get("revision")?,
                live: r.try_get("live")?,
                private_world_id: r.try_get("private_world_id")?,
            });
        }
    }
    Err(IdentityError::Busy)
}

/// Whether `owner` has a live session at `key` (no lock, no touch): the check a request makes
/// before anything with a side effect.
pub async fn is_live_session_of(
    db: &PgPool,
    key: &SessionKey,
    owner: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM freeroam_session WHERE game = $1 AND scid = $2 AND template = $3 \
           AND name = $4 AND owner_profile_id = $5 AND expires_at > clock_timestamp())",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(owner)
    .fetch_one(db)
    .await?)
}

/// Writes the locked row of `key`: a new life (`created`: owner, creation time, revision 1, and
/// the owner's oldest live sessions past [`MAX_LIVE_PER_OWNER`] expire) or the next revision.
/// The session lives [`SESSION_TTL_SECS`] from now. Returns the revision.
pub async fn save_session_in(
    conn: &mut PgConnection,
    key: &SessionKey,
    owner: Uuid,
    doc: &Value,
    created: bool,
) -> Result<i64, IdentityError> {
    let revision: i64 = sqlx::query_scalar(
        "UPDATE freeroam_session SET doc = $5, owner_profile_id = $6, \
           revision = CASE WHEN $7 THEN 1 ELSE revision + 1 END, \
           created_at = CASE WHEN $7 THEN clock_timestamp() ELSE created_at END, \
           updated_at = clock_timestamp(), expires_at = clock_timestamp() + make_interval(secs => $8) \
         WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4 RETURNING revision",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(doc)
    .bind(owner)
    .bind(created)
    .bind(SESSION_TTL_SECS as f64)
    .fetch_one(&mut *conn)
    .await?;
    if created {
        sqlx::query(
            "UPDATE freeroam_session SET expires_at = clock_timestamp() WHERE (game, scid, template, name) IN ( \
               SELECT game, scid, template, name FROM freeroam_session \
               WHERE game = $1 AND owner_profile_id = $2 AND expires_at > clock_timestamp() \
                 AND private_world_id IS NULL \
               ORDER BY updated_at DESC, name DESC OFFSET $3 FOR UPDATE SKIP LOCKED)",
        )
        .bind(&key.game)
        .bind(owner)
        .bind(MAX_LIVE_PER_OWNER)
        .execute(&mut *conn)
        .await?;
    }
    Ok(revision)
}

/// The owner's live session of `key` (its life extended, as a read touches it), with its
/// revision; `None` when missing, dead or another profile's.
pub async fn read_session(
    db: &PgPool,
    key: &SessionKey,
    owner: Uuid,
) -> Result<Option<(Value, i64)>, IdentityError> {
    let row: Option<(Value, i64)> = sqlx::query_as(
        "UPDATE freeroam_session SET updated_at = clock_timestamp(), \
           expires_at = clock_timestamp() + make_interval(secs => $6) \
         WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4 AND owner_profile_id = $5 \
           AND expires_at > clock_timestamp() RETURNING doc, revision",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(owner)
    .bind(SESSION_TTL_SECS as f64)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The name prefix of every car meet invite session (MATCHMAKING_API.md §3.8).
pub const INVITE_PREFIX: &str = "fh2inf-invite-";

/// The name prefix of one world's invite sessions.
pub fn invite_prefix(world: Uuid) -> String {
    format!("{INVITE_PREFIX}{}-", world.simple())
}

/// The world an invite session's name was made for ([`invite_prefix`]); `None` for any other name.
pub fn invite_world(name: &str) -> Option<Uuid> {
    let rest = name.strip_prefix(INVITE_PREFIX)?;
    let (world, _) = rest.split_once('-')?;
    if world.len() != 32 {
        return None;
    }
    Uuid::try_parse(world).ok()
}

// ---- Private official worlds (OD-204, CR-S96; WORLDS_OFFICIAL.md §12.4) ---------------------------

/// The private world the caller owns and holds a live seat of this session in (`None`: no link).
/// Never allocates: only the launcher's create route does (§12.2 item 4).
pub async fn owned_private_seat_in(
    conn: &mut PgConnection,
    caller: &Caller,
) -> Result<Option<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT w.id FROM mm_grant g JOIN world w ON w.id = g.world_id \
         WHERE g.profile_id = $1 AND g.session_id = $2 AND w.official_owner_profile_id = $1 \
           AND (g.state = 'used' OR (g.state = 'issued' AND g.expires_at > clock_timestamp())) \
           AND w.closed_at IS NULL AND w.expires_at > clock_timestamp() LIMIT 1",
    )
    .bind(caller.profile_id)
    .bind(caller.session_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Sets the link of the locked row of `key` (a new life clears an earlier one's).
pub async fn set_private_world_in(
    conn: &mut PgConnection,
    key: &SessionKey,
    world: Option<Uuid>,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE freeroam_session SET private_world_id = $5 \
         WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(world)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What `account` holds of the live private session of `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateReader {
    /// A `world_access` row: it reads, and becomes a member.
    Access,
    /// A live invite: it reads only.
    Invited,
}

/// Whether `account` may read the live private session of `key`: `None` for anything else (404),
/// and for a `profile` kicked from its world.
pub async fn private_reader_in(
    conn: &mut PgConnection,
    key: &SessionKey,
    account: Uuid,
    profile: Uuid,
) -> Result<Option<PrivateReader>, IdentityError> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM world_access a WHERE a.world_id = f.private_world_id AND a.account_id = $5), \
           EXISTS (SELECT 1 FROM world_invite i WHERE i.world_id = f.private_world_id AND i.to_account_id = $5 \
             AND i.kind = 'invite' AND i.state = 'pending' AND i.expires_at > clock_timestamp()) \
         FROM freeroam_session f JOIN world w ON w.id = f.private_world_id \
         WHERE f.game = $1 AND f.scid = $2 AND f.template = $3 AND f.name = $4 \
           AND f.expires_at > clock_timestamp() AND w.closed_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM mm_grant k WHERE k.world_id = w.id AND k.profile_id = $6 AND k.state = 'kicked')",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(account)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some((true, _)) => Some(PrivateReader::Access),
        Some((false, true)) => Some(PrivateReader::Invited),
        _ => None,
    })
}

/// A world's announce keeps its linked session alive (§12.4).
pub async fn extend_private_sessions_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE freeroam_session SET expires_at = greatest(expires_at, clock_timestamp() + make_interval(secs => $2)) \
         WHERE private_world_id = $1 AND expires_at > clock_timestamp()",
    )
    .bind(world)
    .bind(SESSION_TTL_SECS as f64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A closing world ends its linked session.
pub async fn end_private_sessions_in(
    conn: &mut PgConnection,
    world: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE freeroam_session SET expires_at = clock_timestamp() \
         WHERE private_world_id = $1 AND expires_at > clock_timestamp()",
    )
    .bind(world)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The name prefix of the shared car meet and Road Trip sessions the server once made (before
/// 2026-11-02 and 2026-10-30; both hoppers now get one target per player, MATCHMAKING_API.md
/// §4a). Kept so a session of that name still in the directory stays readable by its members
/// until it expires.
pub const MEET_PREFIX: &str = "fh2inf-meet-";

/// The name `world`'s shared Online Road Trip session had before 2026-10-30 (no longer made).
pub fn roadtrip_name(world: Uuid) -> String {
    format!("{MEET_PREFIX}{}-roadtrip", world.simple())
}

/// Whether `name` is a session several players share (a car meet invite's, or a legacy shared
/// meet session's): its members read it and write their own entry.
pub fn is_invite_name(name: &str) -> bool {
    name.starts_with(INVITE_PREFIX) || name.starts_with(MEET_PREFIX)
}

/// The live invite session of `key` (§3.8) when `xuid` is one of its members: `(doc, revision)`;
/// `None` for any other session or a non-member. A member's read does not extend its life (only
/// the owner's reads and anyone's writes do), so members cannot keep an old invite alive.
pub async fn read_invite_session(
    db: &PgPool,
    key: &SessionKey,
    xuid: i64,
) -> Result<Option<(Value, i64)>, IdentityError> {
    if !is_invite_name(&key.name) {
        return Ok(None);
    }
    let row: Option<(Value, i64)> = sqlx::query_as(
        "SELECT doc, revision FROM freeroam_session \
         WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4 \
           AND expires_at > clock_timestamp() \
           AND EXISTS (SELECT 1 FROM jsonb_each(doc -> 'members') m \
                       WHERE m.value #>> '{constants,system,xuid}' = $5)",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(xuid.to_string())
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Ends the owner's live sessions whose name starts with `prefix` (a world's earlier invite
/// sessions, §3.8). Returns how many.
pub async fn end_sessions_with_prefix_in(
    conn: &mut PgConnection,
    game: &str,
    owner: Uuid,
    prefix: &str,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE freeroam_session SET expires_at = clock_timestamp() \
         WHERE game = $1 AND owner_profile_id = $2 AND left(name, length($3)) = $3 \
           AND expires_at > clock_timestamp()",
    )
    .bind(game)
    .bind(owner)
    .bind(prefix)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Ends world `world`'s live sessions whose name starts with `prefix`, whoever owns them: its
/// earlier invite sessions, owned by players who held a seat there (an official world's, OD-190,
/// MATCHMAKING_API.md §2.9) or by its host (a player world's, CR-S92). Returns how many.
pub async fn end_world_sessions_with_prefix_in(
    conn: &mut PgConnection,
    game: &str,
    world: Uuid,
    prefix: &str,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE freeroam_session SET expires_at = clock_timestamp() \
         WHERE game = $1 AND (owner_profile_id IN (SELECT DISTINCT g.profile_id FROM mm_grant g WHERE g.world_id = $2) \
             OR owner_profile_id = (SELECT w.host_profile_id FROM world w WHERE w.id = $2)) \
           AND left(name, length($3)) = $3 AND expires_at > clock_timestamp()",
    )
    .bind(game)
    .bind(world)
    .bind(prefix)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// The owner's live session of `key` (any other answer: `None`), locked for a write.
pub async fn lock_live_session_in(
    conn: &mut PgConnection,
    key: &SessionKey,
    owner: Uuid,
) -> Result<Option<(Value, i64)>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT doc, revision FROM freeroam_session \
         WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4 AND owner_profile_id = $5 \
           AND expires_at > clock_timestamp() FOR UPDATE",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .bind(owner)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The owner's most recent live session named `name` (any scid and template), locked: the
/// session a cluster request names by its last segment.
pub async fn lock_own_session_named_in(
    conn: &mut PgConnection,
    game: &str,
    owner: Uuid,
    name: &str,
) -> Result<Option<(SessionKey, Value, i64)>, IdentityError> {
    let row = sqlx::query(
        "SELECT scid, template, doc, revision FROM freeroam_session \
         WHERE game = $1 AND owner_profile_id = $2 AND name = $3 AND expires_at > clock_timestamp() \
         ORDER BY updated_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(game)
    .bind(owner)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok((
            SessionKey {
                game: game.to_owned(),
                scid: r.try_get("scid")?,
                template: r.try_get("template")?,
                name: name.to_owned(),
            },
            r.try_get("doc")?,
            r.try_get("revision")?,
        ))
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// What ending a session did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The owner's live session ended.
    Ended,
    /// No live session by that key.
    Missing,
    /// Another profile's live session: untouched.
    Foreign,
}

/// Ends the owner's live session of `key` (its row stays, dead, until [`purge`]).
pub async fn end_session(
    db: &PgPool,
    key: &SessionKey,
    owner: Uuid,
) -> Result<Ended, IdentityError> {
    let mut tx = db.begin().await?;
    let live: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT f.owner_profile_id, EXISTS (SELECT 1 FROM world w WHERE w.id = f.private_world_id AND w.closed_at IS NULL) \
         FROM freeroam_session f \
         WHERE f.game = $1 AND f.scid = $2 AND f.template = $3 AND f.name = $4 AND f.expires_at > clock_timestamp() \
         FOR UPDATE OF f",
    )
    .bind(&key.game)
    .bind(&key.scid)
    .bind(&key.template)
    .bind(&key.name)
    .fetch_optional(&mut *tx)
    .await?;
    let ended = match live {
        None => Ended::Missing,
        Some((o, _)) if o != owner => Ended::Foreign,
        // A linked session lives with its private world: the owner leaving doesn't end it (§12.4).
        Some((_, true)) => Ended::Ended,
        Some((_, false)) => {
            sqlx::query(
                "UPDATE freeroam_session SET expires_at = clock_timestamp(), updated_at = clock_timestamp() \
                 WHERE game = $1 AND scid = $2 AND template = $3 AND name = $4",
            )
            .bind(&key.game)
            .bind(&key.scid)
            .bind(&key.template)
            .bind(&key.name)
            .execute(&mut *tx)
            .await?;
            Ended::Ended
        }
    };
    tx.commit().await?;
    Ok(ended)
}

/// A ticket to store.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTicket {
    /// Ticket id (server-minted).
    pub id: Uuid,
    /// Game.
    pub game: String,
    /// The caller.
    pub owner_profile_id: Uuid,
    /// The hopper, lower case.
    pub hopper: String,
    /// What `GET …/tickets/{id}` answers.
    pub details: Value,
    /// Where the caller was sent.
    pub route: Route,
}

/// Stores a found ticket (alive [`TICKET_TTL_SECS`]); the owner's oldest live tickets past
/// [`MAX_LIVE_PER_OWNER`] expire.
pub async fn insert_ticket_in(conn: &mut PgConnection, t: &NewTicket) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO freeroam_ticket (id, game, owner_profile_id, hopper, details, world_id, via, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, clock_timestamp() + make_interval(secs => $8))",
    )
    .bind(t.id)
    .bind(&t.game)
    .bind(t.owner_profile_id)
    .bind(&t.hopper)
    .bind(&t.details)
    .bind(t.route.world_id)
    .bind(t.route.via.as_str())
    .bind(TICKET_TTL_SECS as f64)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE freeroam_ticket SET expires_at = clock_timestamp() WHERE id IN ( \
           SELECT id FROM freeroam_ticket WHERE game = $1 AND owner_profile_id = $2 AND expires_at > clock_timestamp() \
           ORDER BY created_at DESC, id DESC OFFSET $3 FOR UPDATE SKIP LOCKED)",
    )
    .bind(&t.game)
    .bind(t.owner_profile_id)
    .bind(MAX_LIVE_PER_OWNER)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The details of the owner's live ticket `id`; `None` when missing, expired or another's.
pub async fn ticket_details(
    db: &PgPool,
    game: &str,
    id: Uuid,
    owner: Uuid,
) -> Result<Option<Value>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT details FROM freeroam_ticket WHERE id = $1 AND game = $2 AND owner_profile_id = $3 \
           AND expires_at > clock_timestamp()",
    )
    .bind(id)
    .bind(game)
    .bind(owner)
    .fetch_optional(db)
    .await?)
}

/// Ends the owner's live ticket `id` (anything else: nothing). Whether one ended.
pub async fn end_ticket(
    db: &PgPool,
    game: &str,
    id: Uuid,
    owner: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query(
        "UPDATE freeroam_ticket SET expires_at = clock_timestamp() WHERE id = $1 AND game = $2 \
           AND owner_profile_id = $3 AND expires_at > clock_timestamp()",
    )
    .bind(id)
    .bind(game)
    .bind(owner)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

/// Live tickets of `hopper` (lower case) in `game`.
pub async fn hopper_population(
    db: &PgPool,
    game: &str,
    hopper: &str,
) -> Result<i64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM freeroam_ticket WHERE game = $1 AND hopper = $2 AND expires_at > clock_timestamp()",
    )
    .bind(game)
    .bind(hopper)
    .fetch_one(db)
    .await?)
}

/// What a purge removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    /// Session rows.
    pub sessions: u64,
    /// Ticket rows.
    pub tickets: u64,
}

/// Deletes sessions and tickets dead for more than `older_than_secs`, [`PURGE_BATCH`] rows per
/// statement (a `fi_maintenance` login: `fi-cli freeroam-purge`). Safe beside live traffic: a
/// dead row is a session that does not exist, and its key's next write recreates it.
pub async fn purge(db: &PgPool, older_than_secs: i64) -> Result<Purged, IdentityError> {
    let mut out = Purged::default();
    for (table, count) in [
        ("freeroam_session", &mut out.sessions),
        ("freeroam_ticket", &mut out.tickets),
    ] {
        // No row lock in the subquery (it would need UPDATE, which fi_maintenance lacks); the
        // outer predicate is re-checked on a row a writer revived meanwhile, so it stays.
        let sql = format!(
            "DELETE FROM {table} WHERE ctid IN (SELECT ctid FROM {table} \
               WHERE expires_at < clock_timestamp() - make_interval(secs => $1) LIMIT $2) \
             AND expires_at < clock_timestamp() - make_interval(secs => $1)"
        );
        loop {
            let n = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(older_than_secs as f64)
                .bind(PURGE_BATCH)
                .execute(db)
                .await?
                .rows_affected();
            *count += n;
            if n < PURGE_BATCH as u64 {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joiners_on_the_hosts_network_get_its_lan_address() {
        let public: IpAddr = "203.0.113.5".parse().unwrap();
        let lan: Vec<IpAddr> = vec!["192.168.1.40".parse().unwrap()];
        let away: IpAddr = "198.51.100.9".parse().unwrap();
        assert_eq!(
            joiner_address(public, 5101, &lan, Some(away)),
            "203.0.113.5:5101".parse().unwrap()
        );
        assert_eq!(
            joiner_address(public, 5101, &lan, Some(public)),
            "192.168.1.40:5101".parse().unwrap()
        );
        let mapped: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        assert_eq!(
            joiner_address(public, 5101, &lan, Some(mapped)),
            "192.168.1.40:5101".parse().unwrap(),
            "an IPv4-mapped caller is the same network"
        );
        assert_eq!(
            joiner_address(public, 5101, &[], Some(public)),
            "203.0.113.5:5101".parse().unwrap(),
            "no LAN address announced: the public one"
        );
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            joiner_address(loopback, 5101, &lan, Some(away)),
            "192.168.1.40:5101".parse().unwrap(),
            "a host seen on loopback is reached on its LAN address"
        );
        assert_eq!(joiner_address(public, 5101, &lan, None).ip(), public);
    }

    #[test]
    fn mapped_public_ports_follow_the_selected_destination() {
        let public: IpAddr = "203.0.113.5".parse().unwrap();
        let mapped: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        let lan: Vec<IpAddr> = vec!["192.168.1.40".parse().unwrap()];
        assert_eq!(
            joiner_address_with_ports(public, 62001, 5101, &lan, None),
            "203.0.113.5:62001".parse().unwrap(),
            "an unknown caller gets the public endpoint"
        );
        assert_eq!(
            joiner_address_with_ports(public, 62001, 5101, &lan, Some(mapped)),
            "192.168.1.40:5101".parse().unwrap()
        );
        assert_eq!(
            joiner_address_with_ports(public, 62001, 5101, &[], Some(public)),
            "203.0.113.5:62001".parse().unwrap(),
            "a shared public IP without a LAN hint still needs the public mapping"
        );
        assert_eq!(
            joiner_address_with_ports("127.0.0.1".parse().unwrap(), 62001, 5101, &[], None),
            "127.0.0.1:5101".parse().unwrap(),
            "an actual loopback destination uses the local bind"
        );
    }

    #[test]
    fn lan_addresses_and_key_parts() {
        for ok in [
            "10.0.0.1",
            "172.16.4.2",
            "192.168.1.9",
            "169.254.3.3",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_lan(&ok.parse().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "127.0.0.1", "2001:db8::1", "::1", "0.0.0.0"] {
            assert!(!is_lan(&bad.parse().unwrap()), "{bad}");
        }
        assert_eq!(
            key_part("defaultTemplate_qos").as_deref(),
            Some("defaulttemplate_qos")
        );
        assert_eq!(key_part("a.b-c_D9").as_deref(), Some("a.b-c_d9"));
        for bad in ["", "a/b", "a b", "ä", &"x".repeat(MAX_KEY_PART + 1)] {
            assert_eq!(key_part(bad), None, "{bad}");
        }
        assert!(SessionKey::new("fh2", "S", "T", "N").is_some());
        assert!(SessionKey::new("fh2", "S", "T", "").is_none());
        assert_eq!(Via::Fallback.as_str(), "fallback");
    }
}
