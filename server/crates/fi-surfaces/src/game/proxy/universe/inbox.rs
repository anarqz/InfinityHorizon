//! The in-game inbox of Supporters (SER-M3 step 7, OD-48 item 2; GAME_PLATFORM.md §3a "Supporter
//! social"): Xls `Notifications.RetrieveForUser`, `MarkRead`, `Clear` and `SendTextNotification`,
//! ported from the Beta's `forza/notifications.rs` (`handle`, `sync_rivals`, `challenge_target`)
//! and `db/news.rs`, over [`fi_db::inbox`]. Served on saves to tickets that keep cloud data only
//! (`vip` and `hardcore`, OD-52): every other request is served `Features::Free`
//! (`Features::for_ticket`: the matchmaking deployment, a free, noob or lapsed ticket anywhere),
//! which keeps the empty Pure replies of `forza::pure`, and `SendTextNotification` is
//! `GAME.NOT_SERVED` to it.
//! `StartNewSession`, `GetCategories` and `GetThumbnail` stay Pure.
//!
//! - **RetrieveForUser** (polled every 30 s): at most every [`inbox::SYNC_EVERY_S`] seconds per
//!   profile, the sync: news (the platform's `notice` rows of the game in the caller's language,
//!   live now, past the profile's mark; a deleted or ended one leaves the inbox), the rivals sync
//!   (each visible friend's route time the caller has not beaten, a `TimeTrialTimeBeaten`), then
//!   pruning. Then the live notifications, newest first up to `maxResults` and the parameter
//!   block's byte budget, delivered oldest first: each parameter struct encoded with the schema
//!   into one `parameterBlock` (its size in `cbParameters`), FILETIME dates, `expirationDate` 30
//!   days after `sentDate`, `activeNotificationIds` the delivered ids (the game drops the others),
//!   `reconcileActiveNotifications: true`.
//! - **MarkRead**, **Clear**: the caller's own notification only; `{}`. A cleared news item or
//!   rival time never comes back.
//! - **SendTextNotification**: to a friend (`fi_db::friends::are_friends_in`) visible to the
//!   sender (same game and mode, active, unbanned); the text cleaned (control and formatting
//!   characters out, single spaces) to at most [`MESSAGE_CHARS`] characters and shown after the
//!   sender's gamertag; at most [`inbox::MESSAGES_PER_HOUR`] an hour per sender; once per request
//!   fact (a replayed request id answers the same `notificationId`); Xls error 1 with the Beta's
//!   messages otherwise. The text is never logged.
//! - **GetRival** (`super::ghosts`) asks [`challenge`] first: a pending rival time of the inbox on
//!   that track (Meus Rivais → "Ir para") is raced as that friend's current time on that event.

use super::super::forza::{self, Lang, Reply, Served, XlsError};
use super::super::rpc::{self, Ctx};
use super::boards::{game_time, int, shown_class};
use super::ghosts;
use crate::errors::ApiError;
use fi_db::inbox::{self, NewMessage, Notification, RivalTime, Sent, WantedRival};
use fi_db::{friends, gameplane, scores};
use fi_gamedb::Catalog;
use fi_xtsw::{Limits, Schema, truncate_utf16};
use serde_json::{Value as Json, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// The class these methods are on.
pub const CLASS: &str = "Xls.WebServices.Notifications";

/// A message's text, characters (the Beta's).
pub const MESSAGE_CHARS: usize = 200;
/// `MessageNotificationParameters.Message`, UTF-16 units (the schema's capacity).
pub const MESSAGE_FIELD: usize = 512;
/// `maxResults` when the game sends none (the Beta's), and the most delivered (what the game
/// asks: 512 in every captured poll).
pub const DEFAULT_RESULTS: i64 = 64;
/// The most notifications one poll delivers.
pub const MAX_RESULTS: i64 = 512;
/// `cbParameterBlockSize` when the game sends none (the Beta's).
pub const DEFAULT_BLOCK: usize = 0x10000;
/// The largest parameter block: the game's blob limit in replies.
pub const MAX_BLOCK: usize = 0x80000;

/// How `(xls, class, method)` is answered on saves; `None`: not this module's.
pub(crate) fn served(xls: bool, short: &str, method: &str) -> Option<Served> {
    match (xls, short, method) {
        (
            true,
            "Notifications",
            "RetrieveForUser" | "MarkRead" | "Clear" | "SendTextNotification",
        ) => Some(Served::Store),
        _ => None,
    }
}

/// A stored method of this module, on the request's transaction.
pub(crate) async fn stored(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    short: &str,
    method: &str,
    params: Json,
) -> Result<Reply, ApiError> {
    // A request served `Free` (the matchmaking deployment) never reaches this; its inbox stays
    // the free set's empty one all the same.
    if ctx.features != super::super::Features::Supporter {
        return match forza::pure(true, short, method, &params) {
            Some(reply) => Ok(Ok(reply)),
            None => Err(ctx.not_served(short, method)),
        };
    }
    let owner = ctx.owner();
    let id = params["notificationId"].as_i64().unwrap_or(0);
    match method {
        "RetrieveForUser" => retrieve(conn, ctx, &params).await,
        "MarkRead" => {
            inbox::mark_read_in(conn, owner.game, owner.profile_id, id).await?;
            Ok(Ok(json!({})))
        }
        "Clear" => {
            inbox::clear_in(conn, owner.game, owner.profile_id, id).await?;
            Ok(Ok(json!({})))
        }
        "SendTextNotification" => send(conn, ctx, &params).await,
        _ => Err(ctx.not_served(short, method)),
    }
}

/// The parameter struct of each notification type (the game's switch, FUN_140f15ec0; the Beta's
/// `struct_for`).
pub fn struct_for(kind: i64) -> Option<&'static str> {
    Some(match kind {
        0x0000 => "PermissionsDirtyNotificationParameters",
        0x0100 => "NewDlcNotificationParameters",
        0x0200 | 0x0201 | 0x0203 | 0x0602 | 0x8401 => "MessageNotificationParameters",
        0x0600 => "BannedNotificationParameters",
        0x0601 => "UnbannedNotificationParameters",
        0x0700 => "ClubMembershipRequestParameters",
        0x0701 => "ClubInvitationParameters",
        0x0702 => "ClubMemberRemoveRecommendationParameters",
        0x0703 => "ClubKickParameters",
        0x0704 => "ClubDemotionParameters",
        0x0705 => "ClubMemberAcceptedParameters",
        0x0706 => "ClubMemberQuitParameters",
        0x0707 => "ClubNewMemberParameters",
        0x0708 => "ClubNewPromotionParameters",
        0x0709 => "ClubNoticeBoardMessageParameters",
        0x8000 => "UserBadgeGrantedNotificationParameters",
        0x8001 => "SetCreditsNotificationParameters",
        0x8301 => "SharedCarNotificationParameters",
        0x8302 => "CurationPayoutNotificationParameters",
        0x8303 => "DrivatarPayoutNotificationParameters",
        0x8402 => "TimeTrialRecommendNotificationParameters",
        0x8403 => "TimeTrialTimeBeatenNotificationParameters",
        0x8500 => "GiftReceiveMoneyNotificationParameters",
        0x8501 => "GiftReceiveItemNotificationParameters",
        0x8502 => "GiftReceiveCarNotificationParameters",
        0x8503 => "StorefrontOutOfStockNotificationParameters",
        0x8504 => "StorefrontMoneyWaitingNotificationParameters",
        _ => return None,
    })
}

/// 100-ns intervals since 1601-01-01 (FILETIME), from Unix milliseconds.
pub fn filetime(unix_ms: i64) -> i64 {
    unix_ms
        .saturating_add(11_644_473_600_000)
        .saturating_mul(10_000)
}

/// The language code of notices and track names.
pub fn lang_code(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "en",
        Lang::PtBr => "pt-BR",
    }
}

/// `maxResults` and `cbParameterBlockSize` of a RetrieveForUser, within what a reply holds: a
/// value that is not positive is the Beta's default ([`DEFAULT_RESULTS`], [`DEFAULT_BLOCK`]).
pub fn budgets(params: &Json) -> (i64, usize) {
    let max = params["maxResults"]
        .as_i64()
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_RESULTS)
        .min(MAX_RESULTS);
    let block = params["cbParameterBlockSize"]
        .as_i64()
        .filter(|n| *n > 0)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(DEFAULT_BLOCK)
        .min(MAX_BLOCK);
    (max, block)
}

/// A notification's parameter struct encoded with the schema; `None` (left out, logged by id) when
/// it does not encode or is over what `cbParameters` (a u16) holds.
fn encoded(n: &Notification) -> Option<Vec<u8>> {
    match Schema::fh2().encode(&n.structure, &n.params, &Limits::RESULTS) {
        Ok(bytes) if bytes.len() <= usize::from(u16::MAX) => Some(bytes),
        Ok(_) => {
            tracing::error!(notification = n.id, "a notification is over cbParameters");
            None
        }
        Err(e) => {
            tracing::error!(notification = n.id, error = %e, "a notification does not encode");
            None
        }
    }
}

/// RetrieveForUser's reply from the live notifications `rows` (newest first, at most
/// `maxResults` already): the newest whose structs fit `block` bytes, delivered oldest first, each
/// struct back to back in `parameterBlock` with its size in `cbParameters`, as the Beta's.
pub fn retrieve_reply(xuid: u64, rows: &[Notification], block: usize) -> Json {
    let mut kept: Vec<(&Notification, Vec<u8>)> = Vec::new();
    let mut used = 0usize;
    for n in rows {
        let Some(bytes) = encoded(n) else { continue };
        if used + bytes.len() > block {
            break;
        }
        used += bytes.len();
        kept.push((n, bytes));
    }
    kept.reverse();
    let mut parameter_block = Vec::with_capacity(used);
    let mut results = Vec::with_capacity(kept.len());
    let mut ids = Vec::with_capacity(kept.len());
    for (n, bytes) in kept {
        let sent = n.sent_at.millis();
        results.push(json!({
            "id": n.id,
            "xuidRecipient": xuid,
            "notificationType": n.kind,
            "hasThumbnail": false,
            "isRead": n.read,
            "deleteAction": 0,
            "expirationDate": filetime(sent.saturating_add(inbox::TTL_MS)),
            "sentDate": filetime(sent),
            "cbParameters": bytes.len(),
        }));
        ids.push(n.id);
        parameter_block.extend_from_slice(&bytes);
    }
    json!({
        "results": results,
        "parameterBlock": fi_xtsw::hex(&parameter_block),
        "nextRetrieveSeconds": forza::POLL_SECONDS,
        "reconcileActiveNotifications": true,
        "activeNotificationIds": ids,
    })
}

async fn retrieve(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Reply, ApiError> {
    let owner = ctx.owner();
    if inbox::begin_sync_in(conn, owner.game, owner.profile_id).await? {
        let added = sync_news(conn, ctx).await?
            + sync_rivals(conn, ctx).await?
            + u64::from(recommend_monthly(conn, ctx).await?);
        // Pruned when the sync wrote rows (delivery never shows an expired one anyway).
        if added > 0 {
            inbox::prune_in(conn, owner.game, owner.profile_id).await?;
        }
    }
    let (max, block) = budgets(params);
    let rows = inbox::live_in(conn, owner.game, owner.profile_id, max).await?;
    let xuid = u64::try_from(ctx.req.ticket.xuid).unwrap_or(0);
    Ok(Ok(retrieve_reply(xuid, &rows, block)))
}

/// Characters that show nothing or reorder what follows and never belong in a message: zero-width
/// and joiner marks, bidi marks and overrides (U+061C, U+200E-U+200F, U+202A-U+202E,
/// U+2066-U+2069), the BOM, interlinear annotations, the Mongolian vowel separator, the Hangul
/// fillers (U+115F, U+1160, U+3164, U+FFA0) and the tag characters (U+E0000-U+E007F).
fn invisible(c: char) -> bool {
    matches!(
        u32::from(c),
        0x061C
            | 0x115F
            | 0x1160
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFEFF
            | 0xFFA0
            | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F
    )
}

/// Player text as others see it (the Beta's `clean_text`): whitespace as single spaces, no control
/// (NUL included) or invisible characters ([`invisible`]), at most `max` characters.
pub fn clean_text(text: &str, max: usize) -> String {
    let kept: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !invisible(*c))
        .collect();
    kept.split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// A news item's text (the Beta's `news::clean`): the title, a line break, the body (Markdown as
/// written), each line cleaned, at most the Message field's 512 units.
pub fn news_text(title: &str, body: &str) -> String {
    let text = format!("{title}\n{body}").replace("\r\n", "\n");
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| clean_text(l, MESSAGE_FIELD))
        .collect();
    let joined = lines.join("\n");
    truncate_utf16(joined.trim_matches('\n'), MESSAGE_FIELD).to_owned()
}

/// A news item's MessageNotificationParameters, with the notice's id (ours: a deleted notice
/// leaves the inbox; the encoder ignores it).
pub fn news_params(notice: Uuid, title: &str, body: &str) -> Json {
    json!({ "Message": news_text(title, body), "SRAUri": "", "noticeId": notice })
}

/// A friend's message as the recipient sees it: the sender's gamertag, then the cleaned text
/// (MessageNotificationParameters has no sender field).
pub fn message_params(sender_tag: &str, text: &str) -> Json {
    let message = format!("{sender_tag}: {text}");
    json!({ "Message": truncate_utf16(&message, MESSAGE_FIELD), "SRAUri": "" })
}

/// SendTextNotification's reply.
pub fn sent_reply(id: i64) -> Json {
    json!({ "notificationId": id })
}

/// Xls error 1: not a visible friend, or nothing to send (the Beta's message).
pub fn friends_only() -> XlsError {
    XlsError {
        code: 1,
        message: "messages go to friends only",
    }
}

/// Xls error 1: the sender's hourly cap (the Beta's message).
pub fn too_many() -> XlsError {
    XlsError {
        code: 1,
        message: "too many messages",
    }
}

async fn send(conn: &mut PgConnection, ctx: &Ctx<'_>, params: &Json) -> Result<Reply, ApiError> {
    let game = ctx.req.ticket.game();
    let source = ctx.fact_id();
    // A replayed request id: the same answer, nothing sent.
    if let Some(id) = inbox::sent_in(conn, game, source).await? {
        return Ok(Ok(sent_reply(id)));
    }
    let text = clean_text(params["message"].as_str().unwrap_or(""), MESSAGE_CHARS);
    let to = params["xuidRecipient"]
        .as_u64()
        .and_then(|x| i64::try_from(x).ok());
    let viewer = ctx.viewer();
    let recipient = match to {
        Some(xuid) if !text.is_empty() => inbox::recipient_in(conn, &viewer, xuid).await?,
        _ => None,
    };
    let Some(recipient) = recipient else {
        return Ok(Err(friends_only()));
    };
    if !friends::are_friends_in(conn, game, viewer.profile_id, recipient).await? {
        return Ok(Err(friends_only()));
    }
    let me = gameplane::player_in(conn, game, viewer.profile_id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    let tag = rpc::player_of(me).gamertag;
    let message = message_params(&tag, &text);
    let sent = inbox::send_message_in(
        conn,
        &NewMessage {
            game,
            recipient,
            sender: viewer.profile_id,
            source_id: source,
            params: &message,
        },
    )
    .await
    .map_err(ApiError::from_stored)?;
    Ok(match sent {
        Sent::Sent(id) | Sent::Replayed(id) => Ok(sent_reply(id)),
        Sent::Limited => Err(too_many()),
    })
}

async fn sync_news(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<u64, ApiError> {
    let owner = ctx.owner();
    inbox::retract_news_in(conn, owner.game, owner.profile_id).await?;
    let items = inbox::news_in(conn, owner.game, owner.profile_id, lang_code(ctx.lang)).await?;
    let built: Vec<(Uuid, Json)> = items
        .iter()
        .map(|n| (n.id, news_params(n.id, &n.title, &n.body)))
        .collect();
    Ok(inbox::deliver_news_in(conn, owner.game, owner.profile_id, &built).await?)
}

/// Where "Ir para" in Meus Rivais takes the player (the game's FUN_140d6b6b0, the Beta's
/// `challenge_target`): `(category, timeTrialId)`; a showcase (scoreboard type 6) names its event,
/// a Monthly Rivals event (board 3, S4) its event in the Monthly category, any other
/// board is a festival route (`timeTrialId` -1, the track with car select).
pub fn challenge_target(board: i32, event: i32) -> (i32, i32) {
    match board {
        6 => (0, event),
        super::boards::MONTHLY_BOARD
            if super::rivals::event(Catalog::fh2(), i64::from(event)).is_some() =>
        {
            (super::rivals::MONTHLY_CATEGORY as i32, event)
        }
        _ => (0, -1),
    }
}

/// A rival time's dedup key: the mode, the friend, the event and the exact time, so an improved
/// time is a new notification and a deleted one is never sent again.
pub fn rival_key(mode: &str, t: &RivalTime) -> String {
    format!(
        "rival:{mode}:{}:{}:{}:{}:{:016x}",
        t.sender,
        t.key.scoreboard_type,
        t.key.game_scoreboard_id,
        t.key.track_id,
        t.score.to_bits()
    )
}

/// The TimeTrialTimeBeatenNotificationParameters of a friend's time, every field within the
/// schema's capacity (names cut, the class within a u8), with the board and event (ours, for the
/// challenge; the encoder ignores them).
pub fn time_beaten_params(catalog: &Catalog, lang: Lang, t: &RivalTime) -> Json {
    let (category, time_trial) = challenge_target(t.key.scoreboard_type, t.key.game_scoreboard_id);
    let tag = rpc::player_of(t.player.clone()).gamertag;
    let track = i64::from(t.key.track_id);
    let name = catalog
        .track(track)
        .map(|tr| tr.name(lang_code(lang)).to_owned())
        .unwrap_or_else(|| match lang {
            Lang::En => format!("track #{track}"),
            Lang::PtBr => format!("pista #{track}"),
        });
    // A Monthly Rivals event shows its own title.
    let title = (category == super::rivals::MONTHLY_CATEGORY as i32)
        .then(|| super::rivals::event(catalog, i64::from(time_trial)))
        .flatten()
        .map_or_else(
            || name.clone(),
            |e| super::rivals::texts(catalog, &e, lang).0,
        );
    let class = shown_class(catalog, t.car_id, i64::from(t.car_class_id)).clamp(0, 255);
    json!({
        "senderXuid": u64::try_from(t.player.xuid).unwrap_or(0),
        "senderGamertag": truncate_utf16(&tag, forza::GAME_TAG_CHARS),
        "senderClubTag": "",
        "senderClubRole": 0,
        "senderFlags": 0,
        "sentTime": game_time(t.submitted_at),
        "timeTrialId": time_trial,
        "carId": t.car_id,
        "carClassId": class,
        "localizedTimeTrialName": truncate_utf16(&title, 128),
        "trackName": truncate_utf16(&name, 64),
        "trackId": t.key.track_id,
        "score": t.score,
        "scoreType": 1,
        "category": category,
        "rivalBoard": t.key.scoreboard_type,
        "rivalEvent": t.key.game_scoreboard_id,
    })
}

/// The month's Monthly Rivals recommendation (0x8402, S4; other projects used as reference): once per profile and
/// month, its featured event, in the caller's language.
async fn recommend_monthly(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<bool, ApiError> {
    let catalog = Catalog::fh2();
    let month = super::rivals::month_of(fi_domain::UnixMs::now_system());
    let Some(e) = super::rivals::month_events(catalog, month)
        .into_iter()
        .next()
    else {
        return Ok(false);
    };
    let owner = ctx.owner();
    let params = recommend_params(catalog, ctx.lang, &e);
    Ok(inbox::deliver_once_in(
        conn,
        owner.game,
        owner.profile_id,
        inbox::RECOMMEND,
        inbox::RECOMMEND_STRUCT,
        &format!("rivals-month:{month}"),
        &params,
    )
    .await?)
}

/// TimeTrialRecommendNotificationParameters of a Monthly Rivals event.
pub fn recommend_params(catalog: &Catalog, lang: Lang, e: &super::rivals::RivalEvent) -> Json {
    let (name, description, track) = super::rivals::texts(catalog, e, lang);
    json!({
        "TimeTrialId": e.id,
        "localizedTimeTrialName": name,
        "trackName": track,
        "description": description,
        "category": super::rivals::MONTHLY_CATEGORY,
    })
}

async fn sync_rivals(conn: &mut PgConnection, ctx: &Ctx<'_>) -> Result<u64, ApiError> {
    let viewer = ctx.viewer();
    let catalog = Catalog::fh2();
    // This month's Monthly Rivals events first (at most one each: the nearest faster player on
    // each the caller raced, S4), then the friends' route times.
    let month = super::rivals::month_of(fi_domain::UnixMs::now_system());
    let events: Vec<i32> = super::rivals::month_events(catalog, month)
        .iter()
        .filter_map(|e| i32::try_from(e.id).ok())
        .collect();
    let mut times =
        inbox::event_rival_times_in(conn, &viewer, super::boards::MONTHLY_BOARD, &events).await?;
    for t in inbox::rival_times_in(conn, &viewer).await? {
        if !times
            .iter()
            .any(|m| m.sender == t.sender && m.key == t.key && m.score == t.score)
        {
            times.push(t);
        }
    }
    times.truncate(usize::try_from(inbox::MAX_RIVALS).unwrap_or(usize::MAX));
    let wanted: Vec<WantedRival> = times
        .iter()
        .map(|t| WantedRival {
            dedup_key: rival_key(viewer.mode, t),
            sender: t.sender,
            params: time_beaten_params(catalog, ctx.lang, t),
        })
        .collect();
    let done = inbox::sync_rivals_in(conn, viewer.game, viewer.profile_id, &wanted).await?;
    tracing::debug!(
        session = %viewer.session_id,
        wanted = wanted.len(),
        added = done.added,
        cleared = done.cleared,
        "rivals sync"
    );
    Ok(done.added)
}

/// GetRival's pending challenge (the Beta's `challenge_rival`): a request for a time board (score
/// type 1) on a track whose live rival notification (from `rivalXuid` when it names one; of event
/// `rivalEventId` when it is not negative; of the request's board) names a visible player: their time on
/// that event, with its own ghost, else the visible ghost nearest to it. `None`: no such challenge
/// (the caller picks a rival as before).
pub(crate) async fn challenge(
    conn: &mut PgConnection,
    ctx: &Ctx<'_>,
    params: &Json,
) -> Result<Option<Json>, ApiError> {
    let track = int(params, "trackId");
    if int(params, "scoreType") != 1 || track < 0 {
        return Ok(None);
    }
    let event = Some(int(params, "rivalEventId")).filter(|e| *e >= 0);
    let sender = params["rivalXuid"]
        .as_u64()
        .filter(|x| *x != 0)
        .and_then(|x| i64::try_from(x).ok());
    let viewer = ctx.viewer();
    let board = int(params, "scoreboardType");
    let Some(row) = inbox::pending_challenge_in(conn, &viewer, board, track, event, sender).await?
    else {
        return Ok(None);
    };
    let ghost = match row.ghost_file_id {
        Some(own) => Some(own),
        None => {
            scores::nearest_ghost_in(
                conn,
                &viewer,
                row.key.scoreboard_type,
                row.key.score_type,
                track,
                (board == super::boards::MONTHLY_BOARD).then_some(row.key.game_scoreboard_id),
                row.score,
            )
            .await?
        }
    };
    Ok(Some(ghosts::rival_reply(Catalog::fh2(), &row, ghost)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_db::gameplane::{Card, Player};
    use fi_db::scores::BoardKey;
    use fi_domain::UnixMs;
    use fi_xtsw::{decode_results, encode_results};

    fn rival(track: i32, name: &str) -> RivalTime {
        RivalTime {
            sender: Uuid::nil(),
            player: Player {
                xuid: 2_535_100_000_000_002,
                name: Some(name.into()),
                card: Card::default(),
                tables_tran_id: None,
            },
            key: BoardKey {
                scoreboard_type: 1,
                score_type: 1,
                game_scoreboard_id: 70,
                track_id: track,
            },
            score: 95.25,
            car_id: 247,
            car_class_id: 0,
            submitted_at: UnixMs::from_millis(1_792_000_000_123),
        }
    }

    fn row(id: i64, kind: i32, structure: &str, params: Json, read: bool) -> Notification {
        Notification {
            id,
            kind,
            structure: structure.into(),
            params,
            sent_at: UnixMs::from_millis(1_792_000_000_000 + id),
            read,
        }
    }

    /// The parameter block split by `cbParameters`, each struct decoded with its type's schema.
    fn split(reply: &Json) -> Vec<Json> {
        let block = ghosts::unhex(reply["parameterBlock"].as_str().unwrap()).unwrap();
        let mut at = 0;
        let mut out = Vec::new();
        for r in reply["results"].as_array().unwrap() {
            let n = usize::try_from(r["cbParameters"].as_u64().unwrap()).unwrap();
            let ty = struct_for(r["notificationType"].as_i64().unwrap()).unwrap();
            let v = Schema::fh2()
                .decode(ty, &block[at..at + n], &Limits::RESULTS)
                .unwrap();
            out.push(v.to_json());
            at += n;
        }
        assert_eq!(at, block.len(), "the block is exactly the structs");
        out
    }

    fn encodes(method: &str, reply: &Json) -> Json {
        let schema = Schema::fh2();
        let bytes = encode_results(schema, CLASS, method, reply)
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        decode_results(schema, CLASS, method, &bytes)
            .unwrap()
            .to_json()
    }

    /// Every reply of this module, and every parameter struct it stores, encodes with the game's
    /// schema.
    #[test]
    fn every_reply_encodes_with_the_schema() {
        let c = Catalog::fh2();
        let news = news_params(Uuid::nil(), "Title", "Body");
        let message = message_params("Sender#00001", "hi");
        let time = time_beaten_params(c, Lang::PtBr, &rival(0, "Rival"));
        let rows = vec![
            row(
                12,
                inbox::TIME_BEATEN,
                inbox::TIME_BEATEN_STRUCT,
                time.clone(),
                false,
            ),
            row(
                11,
                inbox::MESSAGE,
                inbox::MESSAGE_STRUCT,
                message.clone(),
                true,
            ),
            row(
                10,
                inbox::MESSAGE,
                inbox::MESSAGE_STRUCT,
                news.clone(),
                false,
            ),
        ];
        let reply = retrieve_reply(2_535_100_000_000_001, &rows, DEFAULT_BLOCK);
        let back = encodes("RetrieveForUser", &reply);
        let ids: Vec<i64> = back["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [10, 11, 12], "oldest first");
        assert_eq!(back["activeNotificationIds"], json!([10, 11, 12]));
        assert_eq!(back["reconcileActiveNotifications"], true);
        assert_eq!(back["nextRetrieveSeconds"], forza::POLL_SECONDS);
        let first = &back["results"][0];
        assert_eq!(first["xuidRecipient"], 2_535_100_000_000_001u64);
        assert_eq!(first["notificationType"], 0x0200);
        assert_eq!(back["results"][1]["isRead"], true);
        assert_eq!(back["results"][2]["notificationType"], 0x8403);
        let sent = first["sentDate"].as_i64().unwrap();
        assert_eq!(sent, filetime(1_792_000_000_010));
        assert_eq!(
            first["expirationDate"].as_i64().unwrap() - sent,
            30 * 86_400 * 10_000_000
        );
        let structs = split(&back);
        assert_eq!(structs[0]["Message"], "Title\nBody");
        assert_eq!(structs[1]["Message"], "Sender#00001: hi");
        assert_eq!(structs[2]["senderGamertag"], "Rival");
        assert_eq!(structs[2]["trackId"], 0);
        assert_eq!(structs[2]["score"], 95.25);
        assert_eq!(structs[2]["timeTrialId"], -1);
        assert_eq!(structs[2]["sentTime"], "2026-10-14T17:46:40.123Z");
        // Ours, ignored by the encoder.
        assert!(time.get("rivalBoard").is_some());
        assert!(structs[2].get("rivalBoard").is_none());
        assert!(structs[0].get("noticeId").is_none());
        // Empty: the free deployment's reply exactly.
        assert_eq!(
            retrieve_reply(1, &[], DEFAULT_BLOCK)
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            forza::pure(true, "Notifications", "RetrieveForUser", &json!({}))
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
        let empty = encodes("RetrieveForUser", &retrieve_reply(1, &[], DEFAULT_BLOCK));
        assert_eq!(
            empty,
            encodes(
                "RetrieveForUser",
                &forza::pure(true, "Notifications", "RetrieveForUser", &json!({})).unwrap()
            )
        );
        assert_eq!(
            encodes("SendTextNotification", &sent_reply(42))["notificationId"],
            42
        );
        for method in ["MarkRead", "Clear"] {
            assert!(
                encode_results(Schema::fh2(), CLASS, method, &json!({}))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn the_block_keeps_the_newest_within_its_budgets() {
        let m = |id| {
            row(
                id,
                inbox::MESSAGE,
                inbox::MESSAGE_STRUCT,
                message_params("A#00001", "x"),
                false,
            )
        };
        let rows: Vec<Notification> = (1..=5).rev().map(m).collect();
        let one = encoded(&rows[0]).unwrap().len();
        // Room for two: the two newest, oldest first.
        let reply = retrieve_reply(1, &rows, 2 * one + one / 2);
        assert_eq!(reply["activeNotificationIds"], json!([4, 5]));
        assert_eq!(split(&encodes("RetrieveForUser", &reply)).len(), 2);
        assert_eq!(retrieve_reply(1, &rows, one - 1)["results"], json!([]));
        // A row that does not encode is left out, the others delivered.
        let mut bad = rows.clone();
        bad[1].structure = "NoSuchParameters".into();
        assert_eq!(
            retrieve_reply(1, &bad, DEFAULT_BLOCK)["activeNotificationIds"],
            json!([1, 2, 3, 5])
        );
        assert_eq!(
            budgets(&json!({"maxResults": 512, "cbParameterBlockSize": 51200})),
            (512, 51200)
        );
        assert_eq!(budgets(&json!({})), (DEFAULT_RESULTS, DEFAULT_BLOCK));
        assert_eq!(
            budgets(&json!({"maxResults": -1, "cbParameterBlockSize": 0})),
            (DEFAULT_RESULTS, DEFAULT_BLOCK)
        );
        assert_eq!(
            budgets(&json!({"maxResults": 100000, "cbParameterBlockSize": 1 << 30})),
            (MAX_RESULTS, MAX_BLOCK)
        );
    }

    #[test]
    fn texts_are_cleaned_and_fit_their_fields() {
        assert_eq!(
            clean_text("  hi \u{0}there\u{202E}\n\tyou  ", 200),
            "hi there you"
        );
        assert_eq!(clean_text(&"é".repeat(300), 200).chars().count(), 200);
        assert_eq!(clean_text("\u{200B}\u{FEFF}", 200), "");
        // Bidi marks, Hangul fillers, the Mongolian vowel separator and tag characters too.
        assert_eq!(
            clean_text(
                "a\u{061C}b\u{3164}c\u{115F}\u{1160}d\u{180E}e\u{FFA0}f\u{E0041}\u{E007F}g\u{2066}h",
                200
            ),
            "abcdefgh"
        );
        assert_eq!(clean_text("\u{3164}", 200), "", "a name of filler is empty");
        assert_eq!(
            news_text("Title", "line one\r\n\r\nline\u{7} two"),
            "Title\nline one\n\nline two"
        );
        assert_eq!(
            news_text("T", &"x".repeat(5000)).encode_utf16().count(),
            MESSAGE_FIELD
        );
        // Emoji are two UTF-16 units: cut at a character boundary within the field.
        let long = message_params(&"N".repeat(15), &"😀".repeat(MESSAGE_CHARS));
        let text = long["Message"].as_str().unwrap();
        assert!(text.encode_utf16().count() <= MESSAGE_FIELD);
        encodes_struct(inbox::MESSAGE_STRUCT, &long);
        encodes_struct(
            inbox::MESSAGE_STRUCT,
            &news_params(Uuid::nil(), "T", &"ü".repeat(4000)),
        );
        assert_eq!(
            news_params(Uuid::nil(), "T", "B")["noticeId"],
            Uuid::nil().to_string()
        );
    }

    fn encodes_struct(ty: &str, v: &Json) -> Json {
        let bytes = Schema::fh2()
            .encode(ty, v, &Limits::RESULTS)
            .unwrap_or_else(|e| panic!("{ty}: {e}"));
        Schema::fh2()
            .decode(ty, &bytes, &Limits::RESULTS)
            .unwrap()
            .to_json()
    }

    #[test]
    fn the_monthly_recommendation_encodes_in_both_languages() {
        let c = Catalog::fh2();
        let e = &super::super::rivals::month_events(c, 9)[0];
        for lang in [Lang::En, Lang::PtBr] {
            let p = recommend_params(c, lang, e);
            let back = encodes_struct(inbox::RECOMMEND_STRUCT, &p);
            assert_eq!(back["TimeTrialId"], e.id);
            assert_eq!(back["category"], super::super::rivals::MONTHLY_CATEGORY);
            assert_eq!(back["localizedTimeTrialName"], p["localizedTimeTrialName"]);
        }
        assert_eq!(
            struct_for(i64::from(inbox::RECOMMEND)),
            Some(inbox::RECOMMEND_STRUCT)
        );
    }

    #[test]
    fn rival_times_name_the_track_in_the_callers_language() {
        let c = Catalog::fh2();
        // The catalog's names (its pt-BR names are the English ones today).
        for lang in [Lang::En, Lang::PtBr] {
            let p = time_beaten_params(c, lang, &rival(0, "Rival"));
            assert_eq!(p["trackName"], c.track(0).unwrap().name(lang_code(lang)));
            assert_eq!(p["localizedTimeTrialName"], p["trackName"]);
        }
        // An unknown track, a long name, a class above the stock one: within the fields.
        let mut t = rival(32_000, "Gameloadingveryverylong");
        t.car_class_id = 255;
        let p = time_beaten_params(c, Lang::PtBr, &t);
        let back = encodes_struct(inbox::TIME_BEATEN_STRUCT, &p);
        assert_eq!(back["trackName"], "pista #32000");
        assert_eq!(back["senderGamertag"].as_str().unwrap().chars().count(), 15);
        assert!(back["carClassId"].as_u64().unwrap() <= 255);
        assert_eq!(back["scoreType"], 1);
        assert_eq!(back["category"], 0);
        // A showcase names its event.
        assert_eq!(challenge_target(6, 70), (0, 70));
        assert_eq!(challenge_target(1, 70), (0, -1));
        // A Monthly Rivals event (board 3): its event in the Monthly category.
        let monthly = super::super::rivals::month_events(Catalog::fh2(), 9)[0].id as i32;
        assert_eq!(challenge_target(3, monthly), (4, monthly));
        assert_eq!(challenge_target(3, 70), (0, -1));
        // Keys: the exact time and the mode count.
        let a = rival_key("hardcore", &t);
        let mut faster = t.clone();
        faster.score = 95.24;
        assert_ne!(a, rival_key("hardcore", &faster));
        assert_ne!(a, rival_key("noob", &t));
        assert_eq!(a, rival_key("hardcore", &t.clone()));
        assert!(a.len() <= 200);
    }

    #[test]
    fn served_on_saves_for_every_ticket_and_the_free_plane_keeps_the_empty_inbox() {
        use super::super::super::Features;
        for method in [
            "RetrieveForUser",
            "MarkRead",
            "Clear",
            "SendTextNotification",
        ] {
            assert_eq!(served(true, "Notifications", method), Some(Served::Store));
            assert_eq!(
                forza::served(Features::Supporter, true, "Notifications", method),
                Some(Served::Store)
            );
            // OD-129: every ticket on saves is served `Supporter` (the stored inbox), whatever
            // its tier, mode or line; only the free plane (matchmaking) keeps the empty inbox.
            let key = "Xls.WebServices.Notifications";
            for (tier, mode, line) in [
                ("free", "hardcore", false),
                ("vip", "noob", false),
                ("free", "noob", true),
            ] {
                let mut c = crate::game::ticket::tests::claims("vip", Some("fh2"), 1_792_245_600);
                c.tier = tier.into();
                c.mode = mode.into();
                c.line = line;
                assert_eq!(
                    rpc::lookup(Features::Supporter.for_ticket(&c, true), key, method)
                        .map(|r| r.served),
                    Some(Served::Store),
                    "{tier} {mode} {method}"
                );
                assert_eq!(
                    rpc::lookup(Features::Free.for_ticket(&c, true), key, method).map(|r| r.served),
                    forza::served(Features::Free, true, "Notifications", method),
                    "{tier} {mode} {method}"
                );
            }
        }
        for method in ["RetrieveForUser", "MarkRead", "Clear"] {
            assert_eq!(
                forza::served(Features::Free, true, "Notifications", method),
                Some(Served::Pure)
            );
        }
        assert_eq!(
            forza::served(
                Features::Free,
                true,
                "Notifications",
                "SendTextNotification"
            ),
            None
        );

        for method in ["StartNewSession", "GetCategories", "GetThumbnail"] {
            assert_eq!(served(true, "Notifications", method), None);
            assert_eq!(
                forza::served(Features::Supporter, true, "Notifications", method),
                Some(Served::Pure)
            );
        }
        assert_eq!(served(false, "Notifications", "RetrieveForUser"), None);
        for kind in [0x0200, 0x8403, 0x0705, 0x0706, 0x0707, 0x0708] {
            assert!(
                Schema::fh2().type_def(struct_for(kind).unwrap()).is_some(),
                "{kind:#x}"
            );
        }
        assert_eq!(struct_for(0x1234), None);
    }

    /// Golden decode (opt-in) of the Beta's captured Notifications traffic
    /// (`server/captures/*.jsonl`): every request decodes with the schema; every captured
    /// RetrieveForUser reply decodes, its `parameterBlock` splits exactly by `cbParameters` into
    /// structs of the type's schema that re-encode byte for byte; the same notifications, stored
    /// as this module stores them (struct name and JSON) and delivered by [`retrieve_reply`] with
    /// the request's budgets, come back as the same structs, ids and dates; an empty captured inbox
    /// equals ours. The captures hold real player identities: they stay outside the repository and
    /// the test prints only counts, method names and notification types, never values.
    ///
    /// `FI_FH2_CAPTURES_DIR=<dir> cargo test -p fi-api universe::inbox -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the Beta's captures from FI_FH2_CAPTURES_DIR (outside the repository)"]
    fn captured_notifications_decode_and_come_back_the_same() {
        use super::super::super::rpc::class_key;
        use fi_auth::b64;
        use fi_xtsw::{Envelope, decode_params, parse_envelope};
        use std::collections::BTreeMap;
        let dir = std::env::var("FI_FH2_CAPTURES_DIR")
            .expect("set FI_FH2_CAPTURES_DIR to a directory of Beta capture .jsonl files");
        let schema = Schema::fh2();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read FI_FH2_CAPTURES_DIR")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        let mut count: BTreeMap<String, usize> = BTreeMap::new();
        let mut failures: BTreeMap<String, usize> = BTreeMap::new();
        fn bump(m: &mut BTreeMap<String, usize>, k: String) {
            *m.entry(k).or_default() += 1;
        }
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read a capture file");
            for line in text.lines() {
                let Ok(r) = serde_json::from_str::<Json>(line) else {
                    continue;
                };
                let (Some(class), Some(method)) =
                    (r["forza_class"].as_str(), r["forza_method"].as_str())
                else {
                    continue;
                };
                let key = class_key(class);
                if key != CLASS {
                    continue;
                }
                bump(&mut count, format!("{method}: requests"));
                let body = b64::std_decode(r["request_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                let params = match decode_params(schema, key, method, &body) {
                    Ok(v) => v.into_json(),
                    Err(_) => {
                        bump(&mut failures, format!("{method}: request does not decode"));
                        continue;
                    }
                };
                bump(&mut count, format!("{method}: decoded"));
                match method {
                    "MarkRead" | "Clear" => {
                        if params["notificationId"].as_i64().is_some() {
                            bump(&mut count, format!("{method}: notificationId"));
                        } else {
                            bump(&mut failures, format!("{method}: no notificationId"));
                        }
                        continue;
                    }
                    "RetrieveForUser" => {}
                    _ => continue,
                }
                let (max, block) = budgets(&params);
                bump(
                    &mut count,
                    format!("RetrieveForUser: budgets {max}/{block}"),
                );
                let beta = b64::std_decode(r["response_body_b64"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                let Ok(Envelope::Ok { results }) = parse_envelope(&beta) else {
                    bump(&mut count, "RetrieveForUser: Beta not ok".into());
                    continue;
                };
                let Ok(reply) = decode_results(schema, key, method, results) else {
                    bump(
                        &mut failures,
                        "RetrieveForUser: Beta reply does not decode".into(),
                    );
                    continue;
                };
                let reply = reply.to_json();
                let listed = reply["results"].as_array().cloned().unwrap_or_default();
                if listed.is_empty() {
                    let ours = encodes(method, &retrieve_reply(1, &[], block));
                    if ours == reply {
                        bump(&mut count, "RetrieveForUser: empty, equal".into());
                    } else {
                        bump(&mut failures, "RetrieveForUser: empty differs".into());
                    }
                    continue;
                }
                // The Beta's block, split by cbParameters, each struct of its type.
                let Some(bytes) = ghosts::unhex(reply["parameterBlock"].as_str().unwrap_or(""))
                else {
                    bump(&mut failures, "RetrieveForUser: block is not hex".into());
                    continue;
                };
                let mut at = 0usize;
                let mut rows = Vec::new();
                let mut whole = true;
                for n in &listed {
                    let kind = n["notificationType"].as_i64().unwrap_or(-1);
                    let size =
                        usize::try_from(n["cbParameters"].as_u64().unwrap_or(0)).unwrap_or(0);
                    let (Some(ty), Some(part)) = (struct_for(kind), bytes.get(at..at + size))
                    else {
                        bump(
                            &mut failures,
                            format!("RetrieveForUser: {kind:#06x} unknown or short"),
                        );
                        whole = false;
                        break;
                    };
                    at += size;
                    let Ok(v) = schema.decode(ty, part, &Limits::RESULTS) else {
                        bump(
                            &mut failures,
                            format!("RetrieveForUser: {kind:#06x} does not decode"),
                        );
                        whole = false;
                        break;
                    };
                    let json = v.to_json();
                    match schema.encode(ty, &json, &Limits::RESULTS) {
                        Ok(again) if again == part => {
                            bump(
                                &mut count,
                                format!("struct {kind:#06x}: decoded, re-encoded"),
                            );
                        }
                        _ => bump(
                            &mut failures,
                            format!("struct {kind:#06x}: re-encode differs"),
                        ),
                    }
                    let sent = n["sentDate"].as_i64().unwrap_or(0) / 10_000 - 11_644_473_600_000;
                    if n["expirationDate"].as_i64().unwrap_or(0) != filetime(sent + inbox::TTL_MS) {
                        bump(
                            &mut failures,
                            "RetrieveForUser: expiry is not 30 days".into(),
                        );
                    }
                    rows.push(Notification {
                        id: n["id"].as_i64().unwrap_or(0),
                        kind: i32::try_from(kind).unwrap_or(0),
                        structure: ty.to_owned(),
                        params: json,
                        sent_at: UnixMs::from_millis(sent),
                        read: n["isRead"].as_bool().unwrap_or(false),
                    });
                }
                if !whole {
                    continue;
                }
                if at != bytes.len() {
                    bump(
                        &mut failures,
                        "RetrieveForUser: block longer than its structs".into(),
                    );
                    continue;
                }
                // Stored as this module stores them, delivered with the request's budgets.
                rows.reverse();
                let xuid = 2_535_100_000_000_001;
                let ours = encodes(method, &retrieve_reply(xuid, &rows, block));
                let mut want = reply.clone();
                for n in want["results"].as_array_mut().unwrap() {
                    n["xuidRecipient"] = xuid.into();
                }
                if ours == want {
                    bump(&mut count, "RetrieveForUser: listed, equal".into());
                } else {
                    bump(&mut failures, "RetrieveForUser: listed differs".into());
                }
            }
        }
        println!("{} files", files.len());
        for (k, n) in &count {
            println!("{n:>8}  {k}");
        }
        for (k, n) in &failures {
            println!("{n:>8}  FAILED {k}");
        }
        assert!(
            count.keys().any(|k| k.starts_with("RetrieveForUser")),
            "no inbox in the captures"
        );
        assert!(failures.is_empty(), "{failures:?}");
    }
}
