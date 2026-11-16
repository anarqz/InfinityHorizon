//! Car meet invite sessions (MATCHMAKING_API.md §3.8, S9; other projects used as reference): the free-roam session a
//! car meet invite's joiners go to, made on the host world server's request. The host owns it;
//! every invited player is an inactive member (the game looks for itself in it and marks itself
//! active), who may read it and write its own entry ([`super::mpsd`]).

use super::doc::{self, Me};
use super::{check_storable, stored};
use crate::errors::ApiError;
use fi_db::PgPool;
use fi_db::freeroam::{self, Caller, SessionKey};
use serde_json::json;
use uuid::Uuid;

/// The game's service config id (title 0x1A96545B).
pub const SCID: &str = "788f0100-a4bc-46c3-b19b-d1001a96545b";
/// The template of an invite session.
pub const TEMPLATE: &str = "defaulttemplate";

/// The longest session name the game's car meet field carries (`MsgCarMeetSessionReady`, the engine's
/// `CAR_MEET_NAME_MAX`).
pub const INVITE_NAME_MAX: usize = 64;

/// One invited player: XUID and the gamertag the game shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invited {
    /// XUID.
    pub xuid: i64,
    /// The plain gamertag (cut to the game's 15 characters here).
    pub gamertag: Option<String>,
}

/// Creates world `world`'s invite session for `members`, owned by `host`: the invitation's starter
/// (the world's host, or a player seated in it: CR-S92), and ends the world's earlier ones. Returns
/// its key.
pub async fn create(
    pool: &PgPool,
    host: &Caller,
    world: Uuid,
    members: &[Invited],
) -> Result<SessionKey, ApiError> {
    create_owned(pool, host, world, members, false).await
}

/// [`create`] for an official world (OD-190, MATCHMAKING_API.md §2.9): `owner` is a player seated
/// in it (the one who started the meet invite), and the world's earlier invite sessions end
/// whoever owned them.
pub async fn create_official(
    pool: &PgPool,
    owner: &Caller,
    world: Uuid,
    members: &[Invited],
) -> Result<SessionKey, ApiError> {
    create_owned(pool, owner, world, members, true).await
}

async fn create_owned(
    pool: &PgPool,
    host: &Caller,
    world: Uuid,
    members: &[Invited],
    official: bool,
) -> Result<SessionKey, ApiError> {
    let prefix = freeroam::invite_prefix(world);
    // The game's car meet session-name field holds 64 characters (the engine refuses longer):
    // the prefix and world id take 47, so 16 random hex characters end it at 63.
    // (a v7 id's last 16 hex characters are its random bits)
    let suffix = Uuid::now_v7().simple().to_string();
    let name = format!("{prefix}{}", &suffix[16..]);
    debug_assert!(name.len() <= INVITE_NAME_MAX);
    let key = SessionKey::new(&host.game, SCID, TEMPLATE, &name)
        .ok_or_else(|| ApiError::invalid("worldId", "a world id"))?;
    let now = doc::now_iso();
    let profile = doc::SessionProfile::of(&host.game);
    let mut session = doc::new_target_session(profile, Uuid::now_v7(), &now);
    if official {
        // server-only (never answered): each member's view of an official group session
        session[doc::FI_OFFICIAL] = json!(true);
    }
    session["constants"]["custom"] = json!({
        "hopperId": 0,
        "EnableFullArbitration": true,
        "gameServerPlatform": {
            "gameVariantId": doc::GAME_VARIANT,
            "gsiSetId": doc::GSI_SET,
            "maxAllowedPlayers": 12,
        },
    });
    session["properties"]["system"]["joinRestriction"] = json!("none");
    session["properties"]["system"]["readRestriction"] = json!("none");
    for m in members {
        let me = Me {
            xuid: m.xuid,
            gamertag: doc::game_tag(m.gamertag.as_deref(), m.xuid),
            device_token: String::new(),
            title_id: profile.title_id,
        };
        doc::invite(&mut session, &me, &now);
    }
    let mut tx = pool.begin().await?;
    // The host's own route (its hosted world); each member reads its own (`mpsd::as_seen_by`).
    let route = freeroam::world_for_in(&mut tx, host, &Default::default()).await?;
    let location = doc::location_of(&session, profile);
    doc::set_server(
        &mut session,
        doc::server_entry(route.map(|r| r.addr), &location),
    );
    doc::count_members(&mut session);
    check_storable(&session)?;
    // the world's earlier invite sessions end whoever owned them (CR-S92: a player world's are
    // owned by each invitation's starter too)
    freeroam::end_world_sessions_with_prefix_in(&mut tx, &host.game, world, &prefix).await?;
    let existing = freeroam::lock_session_in(&mut tx, &key, host.profile_id).await?;
    if existing.live {
        // A fresh 128-bit name already alive: refuse rather than overwrite anyone's session.
        return Err(ApiError::unavailable());
    }
    freeroam::save_session_in(&mut tx, &key, host.profile_id, &session, true)
        .await
        .map_err(stored)?;
    tx.commit().await?;
    Ok(key)
}
