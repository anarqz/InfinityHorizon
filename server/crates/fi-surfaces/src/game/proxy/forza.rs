//! The Forza RPC methods served (SER-M3 step 4; the free set PV1-Q11, OD-34), which deployment
//! serves which ([`served`], the one allowlist), and their replies as pure functions of the player
//! and the decoded `_Parameters`, ported from the Beta's
//! `server/src/forza.rs` (`handle` :111-293, `user_logon` :480-528, `user_data` :531-548,
//! `permissions` :474) and `forza/notifications.rs` (an empty inbox for now). The storage half of
//! the stored methods is `super::rpc`; these builders are what the golden replay checks.
//!
//! Replies are `_Results` as JSON; the codec (`fi_xtsw`, lenient JSON → value) fills every field
//! a builder leaves out with zero, empty, false or the default datetime, as the Beta's did.

use super::Features;
use fi_db::clubs::Badge;
use fi_db::gameplane::Card;
use serde_json::{Value as Json, json};

/// How often the game polls Notifications.RetrieveForUser (it also honours `nextRetrieveSeconds`).
pub const POLL_SECONDS: i64 = 30;

/// The gamertag fields of the game hold 15 characters (UTF-16 units), like Xbox's.
pub const GAME_TAG_CHARS: usize = 15;

/// The language of server texts (`X-Fh2-Lang`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// English (the default).
    En,
    /// Brazilian Portuguese.
    PtBr,
}

impl Lang {
    /// From `X-Fh2-Lang`: any `pt` tag is pt-BR, anything else English.
    pub fn from_header(value: Option<&str>) -> Lang {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v.starts_with("pt") => Lang::PtBr,
            _ => Lang::En,
        }
    }
}

/// The player as the replies show them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// XUID.
    pub xuid: u64,
    /// The gamertag as the game shows it (at most [`GAME_TAG_CHARS`]).
    pub gamertag: String,
    /// The card last reported.
    pub card: Card,
}

/// An Xls error answer (status 2, HRESULT `0x80510000 | code`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XlsError {
    /// The code.
    pub code: u32,
    /// English, for the game's log.
    pub message: &'static str,
}

/// A builder's answer: `_Results` JSON or an Xls error.
pub type Reply = Result<Json, XlsError>;

/// How a served method is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// From the parameters alone ([`pure`]).
    Pure,
    /// With the profile's stored state (`super::rpc`).
    Store,
}

/// The one allowlist of the Forza RPC (GAME_PLATFORM.md §3a): how `(Xls class, short class name,
/// method)` is answered on a deployment serving `features`; `None`: not served
/// (`GAME.NOT_SERVED`).
///
/// [`Features::Free`] (services/matchmaking, PV1-Q11, OD-34) is listed method by method and never
/// derived from the Supporter set, so a universe-shaped method served to Supporters later
/// (Scoreboard, Storefront, ContentVersioning, OnlineProfile liveries, groups and
/// setups: SER-M3 steps 7 to 9) is never served to a free ticket by accident: logon,
/// permissions, user data and card, economy, credits, daily bonus, telemetry, owned products,
/// friends reported, compare stats (empty: Supporters get their friends' from
/// `super::universe::social`), the notification inbox (empty), Reconcile, the profile sync
/// (stored for every ticket, OD-128: the drivatars' levels and cars), the drivatars (every ticket
/// and both modes, OD-128, `super::universe::drivatars`), Car Clubs (every ticket, one mode per
/// club, CR-S70, `super::universe::clubs`), and the car liveries with the two curated Storefront
/// lists (CR-S59, `super::liveries`, every ticket).
pub fn served(features: Features, xls: bool, short: &str, method: &str) -> Option<Served> {
    use Served::{Pure, Store};
    // The drivatars, every ticket whatever its tier and mode (OD-128).
    if let Some(served) = super::universe::drivatars::served(xls, short, method) {
        return Some(served);
    }
    // Car Clubs, every ticket (OD-129), each club of one mode (OD-130, CR-S70).
    if let Some(served) = super::universe::clubs::served(xls, short, method) {
        return Some(served);
    }
    // The Storefront, every ticket (OD-138, CR-S74): shared designs, vinyl groups and tunes.
    if let Some(served) = super::universe::storefront::served(xls, short, method) {
        return Some(served);
    }
    // The Supporters' universe (SER-M3 step 6, OD-48): saves only, never derived for free.
    if features == Features::Supporter
        && let Some(served) = super::universe::served(xls, short, method)
    {
        return Some(served);
    }
    Some(match (features, xls, short, method) {
        (_, false, "User", "Logon") => Store,
        (_, true, "User", "GetUserData") => Store,
        (_, false, "User", "SetPlayerCardInfo") => Store,
        // Every ticket's profile sync is stored (OD-128: the drivatars' levels and cars).
        (_, false, "OnlineProfile", "UpdateAll" | "UpdateAllTables") => Store,
        (_, true, "Transaction", "Reconcile") => Store,
        // The players' car liveries, every tier (CR-S59, `super::liveries`): stored per profile
        // and car, fetched by id by whoever is shown the car. Not a catalogue: the curated
        // Storefront lists are answered empty; the rest of ContentVersioning, Storefront and the
        // livery groups stay unserved.
        (_, xls, short, method) if super::liveries::is_livery(xls, short, method) => Store,
        (_, xls, short, method) if super::liveries::curated(xls, short, method).is_some() => Pure,
        (_, false, "User", "GetPermissionsTable")
        | (_, false, "User", "GetEconomySettings")
        | (_, false, "User", "RetrieveOnlineCredits")
        | (_, false, "User", "GetOwnedProducts")
        | (_, false, "User", "GetCompareStatsForRelatedUsers")
        | (_, false, "User", "GetCompareStatsByBucket")
        | (_, false, "DailyCreditBonus", "GetPayout")
        | (_, false, "GameplayLog", "Write")
        | (_, false, "Hopper", "GetPlayerSkills")
        | (_, false, "Hopper", "GetGameOptions")
        | (_, true, "User", "SetFriends")
        | (_, true, "Notifications", "StartNewSession")
        | (_, true, "Notifications", "RetrieveForUser")
        | (_, true, "Notifications", "MarkRead")
        | (_, true, "Notifications", "Clear")
        | (_, true, "Notifications", "GetCategories")
        | (_, true, "Notifications", "GetThumbnail") => Pure,
        // Scoreboard is `super::universe`'s (above, Supporters only; its GetLadderRows is the
        // clubs'); the rest of Storefront and ContentVersioning come with SER-M3 step 9.
        _ => return None,
    })
}

/// Whether the game reconciles the method's transactions (Xls `Transaction.Reconcile`), so a
/// request of it with `X-TransactionId` keeps its reply: `User.RetrieveOnlineCredits` (credits the
/// service grants), the only method the Beta's captures show with a transaction id (137 requests,
/// every one of them). Any other method's transaction id is not stored, and a Reconcile of it
/// answers Xls error 6.
pub fn reconcilable(xls: bool, short: &str, method: &str) -> bool {
    matches!(
        (xls, short, method),
        (false, "User", "RetrieveOnlineCredits")
    )
}

/// The reply of a [`Served::Pure`] method; `None` for any other.
pub fn pure(xls: bool, short: &str, method: &str, params: &Json) -> Option<Json> {
    if let Some(curated) = super::liveries::curated(xls, short, method) {
        return Some(curated);
    }
    Some(match (xls, short, method) {
        (false, "User", "GetPermissionsTable") => permissions_table(),
        (false, "User", "GetEconomySettings") => economy_settings(),
        // Transactional (X-TransactionId): credits granted by the service. None yet.
        (false, "User", "RetrieveOnlineCredits") => json!({ "credit": 0 }),
        // No products: an empty `ownedProducts` (the codec fills what a reply leaves out).
        (false, "User", "GetOwnedProducts") => json!({}),
        // "Meus rivais" compares the player's friends: none for a free player (OD-40); a
        // Supporter's are `super::universe::social`'s (SER-M3 step 7).
        (false, "User", "GetCompareStatsForRelatedUsers") => json!({ "compareStats": [] }),
        (false, "User", "GetCompareStatsByBucket") => json!({ "bucketedCompareStats": [] }),
        (false, "DailyCreditBonus", "GetPayout") => json!({ "dailyPayout": {} }),
        // Telemetry: acknowledged, not kept.
        (false, "GameplayLog", "Write") => json!({}),
        // The matchmaking skill a car meet ticket asks for first (SAURON capture 2026-10-30, the
        // only method the game called that was refused): every requested player at the skill the
        // game itself sends when unanswered, on its current team (OD-144).
        (false, "Hopper", "GetPlayerSkills") => player_skills(params),
        // A Monthly Rivals event's rules (S2; other projects used as reference), keyed on its hopperId.
        (false, "Hopper", "GetGameOptions") => {
            super::universe::rivals::game_options(fi_gamedb::Catalog::fh2(), params)
        }
        // The friends the console knows (from social.xboxlive.com, i.e. ours): nothing to keep.
        (true, "User", "SetFriends") => json!({}),
        (true, "Notifications", "StartNewSession") => {
            json!({ "nonce": 1, "nextRetrieveSeconds": POLL_SECONDS })
        }
        // The free inbox is always empty (OD-40); a Supporter's is `universe::inbox`'s.
        (true, "Notifications", "RetrieveForUser") => json!({
            "results": [],
            "parameterBlock": "",
            "nextRetrieveSeconds": POLL_SECONDS,
            "reconcileActiveNotifications": true,
            "activeNotificationIds": [],
        }),
        (true, "Notifications", "MarkRead") | (true, "Notifications", "Clear") => json!({}),
        // The categories come with the Logon reply; this list stays empty, as in the Beta.
        (true, "Notifications", "GetCategories") => json!({ "results": [] }),
        (true, "Notifications", "GetThumbnail") => json!({ "data": "", "required": 0 }),
        _ => return None,
    })
}

/// The skill the game puts in its car meet ticket when GetPlayerSkills goes unanswered.
pub const DEFAULT_SKILL: i64 = 25;
/// Most players a GetPlayerSkills reply carries (a lobby is 12).
const MAX_SKILLS: usize = 16;

/// GetPlayerSkills' reply: each requested player (at most [`MAX_SKILLS`]) at [`DEFAULT_SKILL`],
/// kept on its current team; no level target.
fn player_skills(params: &Json) -> Json {
    let skills: Vec<Json> = params["playerSkillRequests"]
        .as_array()
        .map(|requests| {
            requests
                .iter()
                .take(MAX_SKILLS)
                .map(|r| {
                    json!({
                        "xuid": r["xuid"].clone(),
                        "skillLevel": DEFAULT_SKILL,
                        "teamIndexRecommendation": r["currentTeamIndex"].as_i64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "playerSkillData": skills, "playerLevelToTarget": 0 })
}

/// The MethodPermission list of GetPermissionsTable and Logon: every id the game's table holds
/// (0x100), enabled with access 0. The PermissionsLoader asks at boot, and until it gets an answer
/// the game treats the service as down; a method missing from the table reads as access 4
/// ("server unavailable"), and rivals need ids 0x51/0x54/0x59/0x5b to upload times and ghosts.
pub fn permissions() -> Json {
    (0..0x100)
        .map(|id| json!({ "methodId": id, "tier": 0, "isEnabled": true, "access": 0 }))
        .collect()
}

/// User.GetPermissionsTable.
pub fn permissions_table() -> Json {
    json!({ "permissions": permissions(), "featureAreasBanMask": 0, "userFlags": 0 })
}

/// The game data file of `User.GetEconomySettings`' settings.
pub const ECONOMY_SETTINGS_FILE: &str = "fh2/economy-settings.json";

/// The settings object of [`ECONOMY_SETTINGS_FILE`], read once.
pub static ECONOMY_SETTINGS: fi_gamedata::Lazy<Json> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(ECONOMY_SETTINGS_FILE, economy_from_json));

/// The settings of an economy settings JSON: an object.
pub fn economy_from_json(text: &str) -> Result<Json, String> {
    let v: Json = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if v.as_object().is_none_or(|o| o.is_empty()) {
        return Err("the settings must be a non-empty object".to_owned());
    }
    Ok(v)
}

/// User.GetEconomySettings: the game's own offline values (read from its ForzaEconomyModule by the
/// Beta's economy probe, 2026-10-13), so the game data's [`ECONOMY_SETTINGS_FILE`]. The module uses
/// whatever the server sends instead of its own, so these stay identical; zeros would zero the
/// rewards.
pub fn economy_settings() -> Json {
    json!({ "settings": *ECONOMY_SETTINGS })
}

/// The message center's tabs (at most 7), sent in the Logon reply: a page shows nothing until it
/// has categories. A notification's category is `(notificationType >> 8) & 0x7f`.
pub fn categories(lang: Lang) -> Json {
    let list: [(i64, &str, &str, &str, &str); 7] = [
        (
            2,
            "Messages",
            "Messages from other players and the server",
            "Mensagens",
            "Mensagens de outros jogadores e do servidor",
        ),
        (
            4,
            "Rivals",
            "Challenges: friends who beat your times",
            "Rivais",
            "Desafios: amigos que bateram seus tempos",
        ),
        (
            7,
            "Club",
            "Your club's invites, members and notice board",
            "Clube",
            "Convites, membros e mural do seu clube",
        ),
        (
            5,
            "Gifts",
            "Credits, cars and items received",
            "Presentes",
            "Créditos, carros e itens recebidos",
        ),
        (
            3,
            "Rewards",
            "Drivatar and shared content payouts",
            "Recompensas",
            "Pagamentos de drivatar e de conteúdo compartilhado",
        ),
        (
            1,
            "News",
            "Infinity news",
            "Notícias",
            "Notícias do Infinity",
        ),
        (6, "Notices", "Account notices", "Avisos", "Avisos da conta"),
    ];
    list.iter()
        .map(|&(t, en, en_d, pt, pt_d)| {
            let (name, description) = match lang {
                Lang::En => (en, en_d),
                Lang::PtBr => (pt, pt_d),
            };
            json!({ "type": t, "name": name, "description": description, "backgroundImageFileId": 0 })
        })
        .collect()
}

/// The UserData struct (Logon, Xls User.GetUserData, every row naming a player) of a player
/// whose club is not read (boards, drivatars, inbox): no club, not taking invites.
pub fn user_data(player: &Player) -> Json {
    user_data_in(player, &Badge::default())
}

/// The UserData struct with the player's Car Club fields (CR-S70): its club of the reader's mode
/// (`clubId`, `clubTag`, `roleInClub`; 0 and empty outside a club) and `acceptsClubInvites`.
/// `clubTopTierCount` stays 0: the ladder's reward tiers are not served (no owner decision).
pub fn user_data_in(player: &Player, badge: &Badge) -> Json {
    let (club_id, tag, role) = match &badge.club {
        Some((id, tag, role)) => (*id, tag.as_str(), *role),
        None => (0, "", 0),
    };
    json!({
        "qwXuid": player.xuid,
        "wzGamerTag": player.gamertag,
        "flags": 0,
        "clubId": club_id,
        "clubTag": tag,
        "clubTopTierCount": 0,
        "roleInClub": role,
        "acceptsClubInvites": badge.accepts_invites,
        "currentCareerLevel": player.card.career_level,
        "currentBadgeId": player.card.badge_id,
        "currentPlayerTitleId": player.card.player_title_id,
    })
}

/// Xls User.GetUserData.
pub fn get_user_data(player: &Player, badge: &Badge) -> Json {
    json!({ "userData": user_data_in(player, badge) })
}

/// The card a Logon reports (`loginData`).
pub fn logon_card(params: &Json) -> Card {
    card_of(&params["loginData"])
}

/// The card of a SetPlayerCardInfo (or of `loginData`).
pub fn card_of(v: &Json) -> Card {
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    Card {
        career_level: u32::try_from(n("currentCareerLevel")).unwrap_or(u32::MAX),
        badge_id: u16::try_from(n("currentBadgeId")).unwrap_or(0),
        player_title_id: u16::try_from(n("currentPlayerTitleId")).unwrap_or(0),
    }
}

/// The game's title version of a Logon (kept for support).
pub fn logon_title_version(params: &Json) -> Option<i32> {
    params["loginData"]["titleVersion"]
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
}

/// User.Logon: the game repeats it until it succeeds, and nothing else of the Forza services
/// starts before. `tables_tran_id` is the stored checkpoint of the last UpdateAllTables: equal to
/// the client's, it means "nothing to resend"; without one, the client's own count is echoed.
pub fn logon(
    player: &Player,
    badge: &Badge,
    tables_tran_id: Option<i32>,
    params: &Json,
    lang: Lang,
) -> Json {
    let checkpoint = tables_tran_id
        .map(Json::from)
        .unwrap_or_else(|| params["loginData"]["clientProfile"]["transactionCount"].clone());
    json!({
        "responseData": {
            "isProfileValid": true,
            "lastOnlineProfileUpdateAllTablesTranId": checkpoint,
            // The Logon reply replaces the permissions table too.
            "permissions": permissions(),
            "featureAreasBanMask": 0,
            "userFlags": 0,
            "upsellOfferId": 0,
            "upsellText": "",
            "vipOfferIds": [],
            "userData": user_data_in(player, badge),
            "notificationNonce": 0,
            "notificationPollIntervalSeconds": POLL_SECONDS,
            "defaultFriendsPollIntervalSeconds": 300,
            "maxFriendsPollIntervalSeconds": 900,
            "notificationCategories": categories(lang),
            "gameplayLogConfig": { "Enabled": false, "FlushInterval": 0, "Throttle": "" },
            "carsToUnshareFromGame": [],
            "defaultTimeoutMs": 30000,
            "defaultContentTimeoutMs": 60000,
            "defaultInGameTimeoutMs": 15000,
            "makeDiscounts": [],
            "carDiscounts": [],
            "makePartsDiscounts": [],
            "carPartsDiscounts": [],
        }
    })
}

/// The reply of a successful OnlineProfile.UpdateAll or UpdateAllTables (an empty `_Results`),
/// the same whether the update was stored or refused by a gate: the game cannot tell the two
/// apart.
pub fn updated() -> Json {
    json!({})
}

/// Transaction.Reconcile: the whole kept reply (12-byte header and results), which the game
/// parses as if it had just arrived; without one, Xls error 6 closes the transaction as failed so
/// the game stops asking.
pub fn reconcile(kept: Option<&[u8]>) -> Reply {
    match kept {
        Some(body) => Ok(json!({
            "WebRequestPayload": fi_xtsw::hex(body),
            "cbMaxAvailable": body.len(),
        })),
        None => Err(XlsError {
            code: 6,
            message: "unknown transaction",
        }),
    }
}

/// The numbers of an UpdateAll kept as a `profile_sync` progression fact, from the merged
/// profile, as an accepted sync's (`accepted: true`, `refusal: null`); a sync the integrity gates
/// refuse records it with `accepted: false` and the gate's rule in `refusal` (`super::rpc`).
/// `level` is `playerRaceHistory[0].level` as the game reported it, the in-band step (level-ups
/// inside the current colour band, CR-S81); `driverLevel` is the driver level the XP reaches
/// (`fi_gamedb::gates::driver_level`, null without an XP), the one the gates measure.
pub fn profile_sync_fact(merged: &Json) -> serde_json::Map<String, Json> {
    let history = &merged["playerHistory"][0];
    let race = &merged["playerRaceHistory"][0];
    let top = race["topSpeed"].as_f64().filter(|v| v.is_finite());
    let mut data = serde_json::Map::new();
    data.insert("credits".into(), history["credits"].as_u64().into());
    data.insert("xp".into(), race["xp"].as_u64().into());
    data.insert("level".into(), race["level"].as_u64().into());
    let driver = race["xp"]
        .as_u64()
        .map(|xp| fi_gamedb::gates::driver_level(i64::try_from(xp).unwrap_or(i64::MAX)));
    data.insert("driverLevel".into(), driver.into());
    data.insert("winnings".into(), race["totalWinnings"].as_u64().into());
    data.insert("topSpeedMps".into(), top.into());
    data.insert("accepted".into(), true.into());
    data.insert("refusal".into(), Json::Null);
    data
}

/// One table of an UpdateAllTables: its rows by key and the keys its `delete<Table>` list removes.
#[derive(Clone, Debug, PartialEq)]
pub struct TableUpdate {
    /// The table's name (`garageCars`).
    pub name: String,
    /// Rows by their key (`id`, `Id` or `trackId`; else `#<index>`).
    pub rows: serde_json::Map<String, Json>,
    /// Keys to remove.
    pub deletes: Vec<String>,
}

fn key_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The tables of an UpdateAllTables (the Beta's `apply_tables`): every list field but
/// `transactionId` and the `delete*` lists, with rows or deletions. Takes the decoded parameters
/// by value: the rows move into the result, nothing is copied.
pub fn table_updates(params: Json) -> Vec<TableUpdate> {
    let Json::Object(obj) = params else {
        return Vec::new();
    };
    let (delete_lists, lists): (serde_json::Map<String, Json>, serde_json::Map<String, Json>) =
        obj.into_iter().partition(|(k, _)| k.starts_with("delete"));
    let mut out = Vec::new();
    for (name, value) in lists {
        if name == "transactionId" {
            continue;
        }
        let Json::Array(rows) = value else {
            continue;
        };
        let mut chars = name.chars();
        let delete_key = match chars.next() {
            Some(first) => format!("delete{}{}", first.to_ascii_uppercase(), chars.as_str()),
            None => continue,
        };
        let deletes: Vec<String> = delete_lists
            .get(&delete_key)
            .and_then(Json::as_array)
            .map(|a| a.iter().map(key_text).collect())
            .unwrap_or_default();
        if rows.is_empty() && deletes.is_empty() {
            continue;
        }
        let mut by_key = serde_json::Map::new();
        for (i, row) in rows.into_iter().enumerate() {
            let key = ["id", "Id", "trackId"]
                .iter()
                .find_map(|k| row.get(*k))
                .map(key_text)
                .unwrap_or_else(|| format!("#{i}"));
            by_key.insert(key, row);
        }
        out.push(TableUpdate {
            name,
            rows: by_key,
            deletes,
        });
    }
    out
}

/// The `tables_sync` fact of an UpdateAllTables (EVENTS.md §3.3), as an accepted sync's
/// (`accepted: true`, `refusal: null`); a refused one is recorded with `accepted: false` and the
/// garage gate's rule in `refusal` (`super::rpc`).
pub fn tables_sync_fact(tables: &[TableUpdate]) -> serde_json::Map<String, Json> {
    let mut data = serde_json::Map::new();
    data.insert(
        "tables".into(),
        tables.iter().map(|t| Json::from(t.name.clone())).collect(),
    );
    data.insert(
        "rows".into(),
        tables.iter().map(|t| t.rows.len()).sum::<usize>().into(),
    );
    data.insert("accepted".into(), true.into());
    data.insert("refusal".into(), Json::Null);
    data
}

/// The gamertag the game shows for a plain name (OD-120): the first [`GAME_TAG_CHARS`]
/// characters.
pub fn game_tag(name: &str) -> String {
    name.chars().take(GAME_TAG_CHARS).collect()
}

const NAME_FIRST: &[&str] = &[
    "Apex", "Late", "Neon", "Curb", "Ghost", "Redline", "Slip", "Turbo", "Horizon", "Gravel",
    "Nitro", "Midnight", "Drift", "Torque", "Chrome", "Dusty", "Rapid", "Silent", "Wild", "Lucky",
    "Brave", "Crimson", "Solar", "Rally",
];
const NAME_SECOND: &[&str] = &[
    "Hunter", "Braker", "Rider", "Kisser", "Line", "Rita", "Sam", "Taxi", "Hero", "Grace", "Nico",
    "Mara", "King", "Fox", "Wolf", "Viper", "Pilot", "Comet", "Duke", "Nova", "Rex", "Joy",
    "Blaze", "Storm",
];

/// A stable, readable gamertag for an XUID that is not a player (the game looks up every grid
/// driver; offline AI drivatars get a name), at most [`GAME_TAG_CHARS`].
pub fn generated_gamertag(xuid: u64) -> String {
    let mut h = xuid ^ 0x9e37_79b9_7f4a_7c15;
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    let len = |l: usize| u64::try_from(l).unwrap_or(1);
    let a = NAME_FIRST[usize::try_from(h % len(NAME_FIRST.len())).unwrap_or(0)];
    let b = NAME_SECOND[usize::try_from((h >> 16) % len(NAME_SECOND.len())).unwrap_or(0)];
    let n = (h >> 32) % 100;
    let mut tag = format!("{a}{b}{n}");
    tag.truncate(GAME_TAG_CHARS);
    tag
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_xtsw::{Schema, decode_results, encode_results, ok_envelope, parse_envelope};

    fn player() -> Player {
        Player {
            xuid: 2_535_100_000_000_001,
            gamertag: game_tag("Tester"),
            card: Card {
                career_level: 7,
                badge_id: 3,
                player_title_id: 9,
            },
        }
    }

    /// Every reply encodes with the game's schema (an encode failure would be a 503 in service).
    #[test]
    fn player_skills_answer_every_requested_player_on_its_team() {
        let params = json!({"hopperId": 3, "playerCount": 2, "playerSkillRequests": [
            {"xuid": 2_535_100_000_000_001_u64, "partyId": 0, "currentTeamIndex": 1},
            {"xuid": 2_535_100_000_000_002_u64, "partyId": 0}
        ]});
        let reply = pure(false, "Hopper", "GetPlayerSkills", &params).unwrap();
        assert_eq!(
            reply,
            json!({"playerSkillData": [
                {"xuid": 2_535_100_000_000_001_u64, "skillLevel": DEFAULT_SKILL, "teamIndexRecommendation": 1},
                {"xuid": 2_535_100_000_000_002_u64, "skillLevel": DEFAULT_SKILL, "teamIndexRecommendation": 0}
            ], "playerLevelToTarget": 0})
        );
        let many = json!({"playerSkillRequests": vec![json!({"xuid": 1}); 40]});
        let reply = pure(false, "Hopper", "GetPlayerSkills", &many).unwrap();
        assert_eq!(
            reply["playerSkillData"].as_array().unwrap().len(),
            MAX_SKILLS
        );
        let none = pure(false, "Hopper", "GetPlayerSkills", &json!({})).unwrap();
        assert_eq!(none["playerSkillData"], json!([]));
        for features in [Features::Supporter, Features::Free] {
            assert_eq!(
                served(features, false, "Hopper", "GetPlayerSkills"),
                Some(Served::Pure)
            );
        }
    }

    #[test]
    fn every_reply_encodes_with_the_schema() {
        let schema = Schema::fh2();
        let p = player();
        let mut cases: Vec<(&str, &str, Json)> = Vec::new();
        for (class, xls, short) in [
            ("Forza.WebServices.User", false, "User"),
            (
                "Forza.WebServices.DailyCreditBonus",
                false,
                "DailyCreditBonus",
            ),
            ("Forza.WebServices.GameplayLog", false, "GameplayLog"),
            ("Xls.WebServices.User", true, "User"),
            ("Xls.WebServices.Notifications", true, "Notifications"),
        ] {
            for method in [
                "GetPermissionsTable",
                "GetEconomySettings",
                "RetrieveOnlineCredits",
                "GetOwnedProducts",
                "GetCompareStatsForRelatedUsers",
                "GetCompareStatsByBucket",
                "GetPayout",
                "Write",
                "SetFriends",
                "StartNewSession",
                "RetrieveForUser",
                "MarkRead",
                "Clear",
                "GetCategories",
                "GetThumbnail",
            ] {
                // The free set: every pure reply is answered there (on saves the compare stats and
                // the inbox are the universe's: `universe::social`, `universe::inbox`).
                if served(Features::Free, xls, short, method) == Some(Served::Pure) {
                    assert!(
                        schema.method(class, method).is_some(),
                        "{class}.{method} is served but not in the schema"
                    );
                    cases.push((class, method, pure(xls, short, method, &json!({})).unwrap()));
                }
            }
        }
        assert_eq!(cases.len(), 15, "every pure method has a case");
        for method in ["UpdateAll", "UpdateAllTables"] {
            cases.push(("Forza.WebServices.OnlineProfile", method, updated()));
        }
        for lang in [Lang::En, Lang::PtBr] {
            cases.push((
                "Forza.WebServices.User",
                "Logon",
                logon(&p, &Badge::default(), Some(12), &json!({}), lang),
            ));
        }
        cases.push((
            "Forza.WebServices.User",
            "Logon",
            logon(
                &p,
                &Badge::default(),
                None,
                &json!({"loginData": {"clientProfile": {"transactionCount": 5}}}),
                Lang::En,
            ),
        ));
        cases.push((
            "Xls.WebServices.User",
            "GetUserData",
            get_user_data(&p, &Badge::default()),
        ));
        cases.push((
            "Xls.WebServices.User",
            "GetUserData",
            get_user_data(
                &p,
                &Badge {
                    club: Some((7, "ABCD".into(), 1)),
                    accepts_invites: true,
                },
            ),
        ));
        cases.push((
            "Xls.WebServices.Transaction",
            "Reconcile",
            reconcile(Some(&ok_envelope(&[0, 0, 0, 7]))).unwrap(),
        ));
        for (class, method, reply) in cases {
            let bytes = encode_results(schema, class, method, &reply)
                .unwrap_or_else(|e| panic!("{class}.{method}: {e}"));
            let envelope = ok_envelope(&bytes);
            assert!(matches!(
                parse_envelope(&envelope),
                Ok(fi_xtsw::Envelope::Ok { .. })
            ));
            decode_results(schema, class, method, &bytes).unwrap();
        }
    }

    #[test]
    fn logon_carries_the_checkpoint_the_card_and_256_permissions() {
        let p = player();
        let r = logon(
            &p,
            &Badge::default(),
            None,
            &json!({"loginData": {"clientProfile": {"transactionCount": 5}}}),
            Lang::PtBr,
        );
        let d = &r["responseData"];
        assert_eq!(d["lastOnlineProfileUpdateAllTablesTranId"], 5);
        assert_eq!(d["permissions"].as_array().unwrap().len(), 256);
        assert_eq!(d["notificationCategories"].as_array().unwrap().len(), 7);
        assert_eq!(d["notificationCategories"][0]["name"], "Mensagens");
        assert_eq!(d["userData"]["wzGamerTag"], "Tester");
        assert_eq!(d["userData"]["currentCareerLevel"], 7);
        let r = logon(&p, &Badge::default(), Some(31), &json!({}), Lang::En);
        assert_eq!(
            r["responseData"]["lastOnlineProfileUpdateAllTablesTranId"],
            31
        );
        assert_eq!(
            r["responseData"]["notificationCategories"][0]["name"],
            "Messages"
        );
        assert_eq!(
            logon_card(
                &json!({"loginData": {"currentCareerLevel": 12, "currentBadgeId": 4, "currentPlayerTitleId": 70000}})
            ),
            Card {
                career_level: 12,
                badge_id: 4,
                player_title_id: 0
            }
        );
    }

    #[test]
    fn gamertags_fit_the_game() {
        assert_eq!(game_tag("Tester"), "Tester");
        assert_eq!(game_tag("ABCDEFGHIJKLMNOPQRST"), "ABCDEFGHIJKLMNO");
        let g = generated_gamertag(2_535_100_000_000_001);
        assert!(g.chars().count() <= 15 && !g.is_empty());
        assert_eq!(g, generated_gamertag(2_535_100_000_000_001));
        assert_eq!(Lang::from_header(Some("pt-BR")), Lang::PtBr);
        assert_eq!(Lang::from_header(Some(" PT ")), Lang::PtBr);
        assert_eq!(Lang::from_header(Some("en-US")), Lang::En);
        assert_eq!(Lang::from_header(None), Lang::En);
    }

    #[test]
    fn table_updates_key_rows_and_pair_deletions() {
        let t = table_updates(json!({
            "transactionId": 9,
            "garageCars": [{"id": 1, "carId": 289}, {"id": 2, "carId": 461}],
            "deleteGarageCars": [3],
            "trackStats": [{"trackId": 77, "x": 1}],
            "tracksUsed": [{"value": 1}],
            "purchasedParts": [],
            "deletePurchasedParts": [],
            "raceFeats": [],
        }));
        let names: Vec<&str> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["garageCars", "trackStats", "tracksUsed"]);
        assert_eq!(t[0].deletes, ["3"]);
        assert_eq!(t[0].rows.keys().collect::<Vec<_>>(), ["1", "2"]);
        assert_eq!(t[1].rows.keys().collect::<Vec<_>>(), ["77"]);
        assert_eq!(t[2].rows.keys().collect::<Vec<_>>(), ["#0"]);
        let fact = tables_sync_fact(&t);
        assert_eq!(fact["rows"], 4);
        assert_eq!(
            (&fact["accepted"], &fact["refusal"]),
            (&json!(true), &Json::Null)
        );
        assert_eq!(
            fact["tables"],
            json!(["garageCars", "trackStats", "tracksUsed"])
        );
    }

    #[test]
    fn profile_sync_reads_the_merged_numbers() {
        let fact = profile_sync_fact(&json!({
            "playerHistory": [{"credits": 1500}],
            "playerRaceHistory": [{"xp": 300, "level": 4, "totalWinnings": 9000, "topSpeed": 81.5}],
        }));
        assert_eq!(
            Json::Object(fact),
            json!({"credits": 1500, "xp": 300, "level": 4, "driverLevel": 0, "winnings": 9000, "topSpeedMps": 81.5, "accepted": true, "refusal": null})
        );
        // CR-S81: past Yellow the reported level is the in-band step (1 at 230,494 XP); the fact
        // keeps it and adds the driver level (15).
        let green = profile_sync_fact(&json!({"playerRaceHistory": [{"xp": 230_494, "level": 1}]}));
        assert_eq!(
            (&green["level"], &green["driverLevel"]),
            (&json!(1), &json!(15))
        );
        let empty = profile_sync_fact(&json!({}));
        assert_eq!(empty["credits"], Json::Null);
        assert_eq!(empty["driverLevel"], Json::Null);
        assert_eq!(empty["accepted"], true);
    }

    #[test]
    fn reconcile_hands_back_the_reply_or_closes_the_transaction() {
        let kept = ok_envelope(&[0, 0, 0, 7]);
        let r = reconcile(Some(&kept)).unwrap();
        assert_eq!(r["cbMaxAvailable"], 16);
        assert_eq!(r["WebRequestPayload"], fi_xtsw::hex(&kept));
        assert_eq!(reconcile(None).unwrap_err().code, 6);
        // Only the credits grant keeps its reply; it is served.
        assert!(reconcilable(false, "User", "RetrieveOnlineCredits"));
        for features in [Features::Supporter, Features::Free] {
            assert!(served(features, false, "User", "RetrieveOnlineCredits").is_some());
        }
        for (xls, short, method) in [
            (false, "User", "Logon"),
            (false, "OnlineProfile", "UpdateAll"),
            (false, "GameplayLog", "Write"),
            (true, "User", "RetrieveOnlineCredits"),
            (true, "Transaction", "Reconcile"),
        ] {
            assert!(!reconcilable(xls, short, method), "{short}.{method}");
        }
    }

    /// The allowlist over every class.method of the game's schema: what a free ticket
    /// (services/matchmaking) and a Supporter ticket (services/saves) are served, and how. A
    /// method not listed here is served to neither; a universe-shaped one is never free.
    #[test]
    fn the_allowlist_over_every_method_of_the_schema() {
        use Served::{Pure, Store};
        // (class, method, free, Supporter)
        let expected: [(&str, &str, Served, Served); 32] = [
            // The players' car liveries, every tier (CR-S59), and the curated lists (empty).
            (
                "Forza.WebServices.Storefront",
                "GetLiveriesForDrivatar",
                Pure,
                Pure,
            ),
            (
                "Forza.WebServices.Storefront",
                "GetFeaturedLiveriesForRandomCars",
                Pure,
                Pure,
            ),
            (
                "Forza.WebServices.ContentVersioning",
                "CreateVersionedLivery",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.ContentVersioning",
                "RemoveReferenceToVersionedLivery",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.ContentVersioning",
                "RetrieveVersionedLiveryById",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.OnlineProfile",
                "UpdateLivery",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.DailyCreditBonus",
                "GetPayout",
                Pure,
                Pure,
            ),
            ("Forza.WebServices.GameplayLog", "Write", Pure, Pure),
            ("Forza.WebServices.Hopper", "GetPlayerSkills", Pure, Pure),
            ("Forza.WebServices.Hopper", "GetGameOptions", Pure, Pure),
            // Every ticket's profile sync is stored and the drivatars served (OD-128).
            ("Forza.WebServices.OnlineProfile", "UpdateAll", Store, Store),
            (
                "Forza.WebServices.OnlineProfile",
                "UpdateAllTables",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.Drivatar",
                "GetDrivatarInformationForRace",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.Drivatar",
                "GetDrivatarsForCache",
                Store,
                Store,
            ),
            (
                "Forza.WebServices.Drivatar",
                "SaveDrivatarInformation",
                Store,
                Store,
            ),
            // Free: empty (OD-40); Supporters: their friends' (SER-M3 step 7, universe::social).
            (
                "Forza.WebServices.User",
                "GetCompareStatsByBucket",
                Pure,
                Store,
            ),
            (
                "Forza.WebServices.User",
                "GetCompareStatsForRelatedUsers",
                Pure,
                Store,
            ),
            ("Forza.WebServices.User", "GetEconomySettings", Pure, Pure),
            ("Forza.WebServices.User", "GetOwnedProducts", Pure, Pure),
            ("Forza.WebServices.User", "GetPermissionsTable", Pure, Pure),
            ("Forza.WebServices.User", "Logon", Store, Store),
            (
                "Forza.WebServices.User",
                "RetrieveOnlineCredits",
                Pure,
                Pure,
            ),
            ("Forza.WebServices.User", "SetPlayerCardInfo", Store, Store),
            ("Xls.WebServices.Notifications", "Clear", Pure, Store),
            ("Xls.WebServices.Notifications", "GetCategories", Pure, Pure),
            ("Xls.WebServices.Notifications", "GetThumbnail", Pure, Pure),
            ("Xls.WebServices.Notifications", "MarkRead", Pure, Store),
            (
                "Xls.WebServices.Notifications",
                "RetrieveForUser",
                Pure,
                Store,
            ),
            (
                "Xls.WebServices.Notifications",
                "StartNewSession",
                Pure,
                Pure,
            ),
            ("Xls.WebServices.Transaction", "Reconcile", Store, Store),
            ("Xls.WebServices.User", "GetUserData", Store, Store),
            ("Xls.WebServices.User", "SetFriends", Pure, Pure),
        ];
        // The Supporters' universe (SER-M3 steps 6 and 7): saves only, never free.
        let supporter_only: [(&str, &str, Served); 10] = [
            ("Forza.WebServices.Scoreboard", "DownloadFile", Store),
            ("Forza.WebServices.Scoreboard", "GetBestScore", Store),
            ("Forza.WebServices.Scoreboard", "GetRival", Store),
            ("Forza.WebServices.Scoreboard", "GetRows", Store),
            ("Forza.WebServices.Scoreboard", "GetRowsAnthem", Store),
            ("Forza.WebServices.Scoreboard", "GetTimeTrials", Store),
            ("Forza.WebServices.Scoreboard", "SubmitScore", Store),
            ("Forza.WebServices.Scoreboard", "SubmitXpScore", Store),
            ("Forza.WebServices.Scoreboard", "UploadFile", Store),
            (
                "Xls.WebServices.Notifications",
                "SendTextNotification",
                Store,
            ),
        ];
        let universe = [
            "Scoreboard",
            "Drivatar",
            "CarClub",
            "Club",
            "Storefront",
            "ContentVersioning",
        ];
        // Car Clubs (CR-S70): every ticket on both deployments, stored (OD-129, OD-130).
        let clubs_served = |xls: bool, short: &str, method: &str| {
            super::super::universe::clubs::served(xls, short, method).is_some()
        };
        let schema = Schema::fh2();
        // The Storefront (CR-S74): every ticket on both deployments, stored (OD-138).
        let storefront_served = |xls: bool, short: &str, method: &str| {
            super::super::universe::storefront::served(xls, short, method).is_some()
        };
        let (mut methods, mut listed, mut clubs, mut storefront) = (0, 0, 0, 0);
        for (class, service) in schema.services() {
            let short = class.rsplit('.').next().unwrap();
            let xls = class.starts_with("Xls.");
            for method in service.methods.keys() {
                methods += 1;
                let free = served(Features::Free, xls, short, method);
                let supporter = served(Features::Supporter, xls, short, method);
                let only = supporter_only
                    .iter()
                    .find(|(c, m, _)| *c == class && *m == method);
                if storefront_served(xls, short, method) {
                    storefront += 1;
                    assert_eq!(
                        (free, supporter),
                        (Some(Store), Some(Store)),
                        "{class}.{method}"
                    );
                    continue;
                }
                if clubs_served(xls, short, method) {
                    clubs += 1;
                    assert_eq!(
                        (free, supporter),
                        (Some(Store), Some(Store)),
                        "{class}.{method}"
                    );
                    continue;
                }
                match expected
                    .iter()
                    .find(|(c, m, _, _)| *c == class && *m == method)
                {
                    Some((_, _, f, s)) => {
                        listed += 1;
                        assert_eq!((free, supporter), (Some(*f), Some(*s)), "{class}.{method}");
                    }
                    None => match only {
                        Some((_, _, s)) => {
                            listed += 1;
                            assert_eq!((free, supporter), (None, Some(*s)), "{class}.{method}");
                        }
                        None => assert_eq!((free, supporter), (None, None), "{class}.{method}"),
                    },
                }
                if (universe
                    .iter()
                    .any(|u| short.starts_with(u) && short != "Drivatar")
                    || (short == "OnlineProfile" && !method.starts_with("UpdateAll")))
                    && !super::super::liveries::is_livery(xls, short, method)
                    && super::super::liveries::curated(xls, short, method).is_none()
                {
                    assert_eq!(free, None, "{class}.{method} is universe-shaped");
                }
                // What a free ticket is served with stored state is its logon and card, the reply
                // ring Reconcile reads, its car liveries (CR-S59), its profile sync and the
                // drivatars (OD-128).
                if free == Some(Store) {
                    assert!(
                        matches!(
                            (short, method.as_str()),
                            ("User", "Logon" | "GetUserData" | "SetPlayerCardInfo")
                                | ("Transaction", "Reconcile")
                                | ("OnlineProfile", "UpdateAll" | "UpdateAllTables")
                                | ("Drivatar", _)
                        ) || super::super::liveries::is_livery(xls, short, method),
                        "{class}.{method}"
                    );
                }
            }
        }
        assert_eq!(
            listed,
            expected.len() + supporter_only.len(),
            "every listed method is in the schema"
        );
        assert_eq!(methods, 201, "the schema's class.methods");
        assert_eq!(
            clubs, 29,
            "every CarClub and Club method, GetLadderRows, AcceptClubInvites"
        );
        assert_eq!(
            storefront, 41,
            "the Storefront's served methods, the three Creates, the group and tune versions, the libraries"
        );
        for features in [Features::Free, Features::Supporter] {
            assert_eq!(served(features, false, "User", "NoSuchMethod"), None);
            assert_eq!(served(features, true, "OnlineProfile", "UpdateAll"), None);
        }
    }
}
