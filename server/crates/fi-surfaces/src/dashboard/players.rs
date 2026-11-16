//! `GET /internal/dashboard/v1/players`, `/players/{name}` and `/media/avatar/{name}`
//! (DASHBOARD_API.md §4, PD-3, OD-169, OD-170): the play dashboard's players, read from
//! `fi_db::dashboard::players`, the game's own board ranking (`fi_db::dashboard::boards`) and the
//! stored online profile laid out as the in-game Statistics screen (`fi_gamedb::stats`).
//!
//! - **List**: website-listed profiles of one mode; `q` a case-insensitive prefix of the plain name
//!   (2-15 letters or digits), `sort` `level` (default), `recent` or `name`, 25 per page, pages 1
//!   to 100.
//! - **Profile**: the plain name in any mode (case-insensitive); a missing name, a malformed one
//!   and a profile hidden from the website are the same 404 `PLAYERS.NOT_FOUND` (no enumeration).
//!   `records` counts the catalog boards (every §3 family) where the profile is the game's #1;
//!   `bests` lists its rank on the speed traps, speed zones, Showcases, its 10 best-ranked rivals
//!   routes (by rank, then board) and the stats boards. Both rank only the boards the profile has
//!   rows on, with the game's ranking; rows on boards the catalog does not have (an unknown
//!   camera, a future Monthly event) count nowhere.
//! - **Avatar**: the linked Discord avatar of a listed profile, fetched from the Discord CDN
//!   (`/avatars/{user id}/{hash}.png?size=128`, no redirects, short timeouts, at most
//!   [`AVATAR_MAX_BYTES`], PNG signature checked) and relayed as `image/png`; anything else is a
//!   bare 404. Nothing is cached here (the site caches it); the URL is never logged.

use super::boards::{self, Fam, board_car, car_name, class_name, count, family_read, in_unit};
use super::{Dash, DashQuery, game, query, required_mode, unknown_game};
use crate::errors::ApiError;
use crate::views::at;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fi_db::dashboard::{self, avatar as db_avatar, boards as db_boards, players as db};
use fi_db::scores::{BoardFilter, BoardKey};
use fi_gamedb::Catalog;
use fi_gamedb::stats::StatsLayout;
use fi_protocol::ErrorCode;
use fi_protocol::dashboard::{
    AVATAR_MAX_BYTES, BESTS_MAX_RIVALS, BoardCar, PLAYERS_MAX_PAGE, PLAYERS_PAGE_SIZE,
    PLAYERS_QUERY_MAX, PLAYERS_QUERY_MIN, PersonalBest, PlayerCard, PlayerClub, PlayerList,
    PlayerProfile, StatFormat, StatRow, StatTab,
};
use std::collections::HashSet;
use std::time::Duration;

/// The Discord CDN.
pub const DISCORD_CDN: &str = "https://cdn.discordapp.com";

/// The 8-byte PNG signature.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Where avatars are fetched from: the CDN's base URL and the HTTP client.
#[derive(Clone, Debug)]
pub struct Avatars {
    client: reqwest::Client,
    base: String,
}

impl Avatars {
    /// The Discord CDN, HTTPS only.
    pub fn discord() -> Option<Avatars> {
        Avatars::new(DISCORD_CDN, Duration::from_secs(3))
    }

    /// Another base (tests: a local stub), with the whole-request `timeout`; `None` when the
    /// client cannot be built.
    pub fn new(base: &str, timeout: Duration) -> Option<Avatars> {
        let client = reqwest::Client::builder()
            .https_only(base.starts_with("https://"))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2).min(timeout))
            .timeout(timeout)
            .build()
            .ok()?;
        Some(Avatars {
            client,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// The avatar `hash` of Discord user `user`: the PNG bytes, or `None` for a non-200 answer, a
    /// redirect, a timeout, a body over [`AVATAR_MAX_BYTES`] or a body that is not a PNG.
    pub async fn fetch(&self, user: i64, hash: &str) -> Option<Vec<u8>> {
        // Both are the database's shapes (a positive id, `^(a_)?[0-9a-f]{32}$`); refuse anything else.
        let hash_ok = {
            let h = hash.strip_prefix("a_").unwrap_or(hash);
            h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit())
        };
        if user <= 0 || !hash_ok {
            return None;
        }
        let url = format!("{}/avatars/{user}/{hash}.png?size=128", self.base);
        let mut res = self.client.get(url).send().await.ok()?;
        if res.status() != reqwest::StatusCode::OK {
            return None;
        }
        if res
            .content_length()
            .is_some_and(|n| n > AVATAR_MAX_BYTES as u64)
        {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if body.len() + chunk.len() > AVATAR_MAX_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        body.starts_with(&PNG_SIGNATURE).then_some(body)
    }
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::PlayersNotFound, "no such player")
}

/// A plain name as the URL may carry one (OD-120: a letter, then letters or digits, at most 20).
fn plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.len() <= 20
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// `q`: 2-15 letters or digits, lower-cased; absent or blank is no filter.
fn prefix_of(q: &DashQuery) -> Result<Option<String>, ApiError> {
    match q.q.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(text)
            if (PLAYERS_QUERY_MIN..=PLAYERS_QUERY_MAX).contains(&text.len())
                && text.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Ok(Some(text.to_ascii_lowercase()))
        }
        Some(_) => Err(ApiError::invalid("q", "q is 2 to 15 letters or digits")),
    }
}

fn sort_of(q: &DashQuery) -> Result<db::Sort, ApiError> {
    match q.sort.as_deref().map(str::trim) {
        None | Some("level") => Ok(db::Sort::Level),
        Some("recent") => Ok(db::Sort::Recent),
        Some("name") => Ok(db::Sort::Name),
        Some(_) => Err(ApiError::invalid("sort", "sort is level, recent or name")),
    }
}

fn page_of(q: &DashQuery) -> Result<u32, ApiError> {
    match q.page.as_deref().map(str::trim) {
        None => Ok(1),
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=PLAYERS_MAX_PAGE).contains(n))
            .ok_or_else(|| ApiError::invalid("page", "page is 1 to 100")),
    }
}

/// The wire card of a listed profile.
pub fn card(row: &db::CardRow) -> PlayerCard {
    PlayerCard {
        name: row.name.clone(),
        discord_name: row.discord_name.clone(),
        avatar: row.avatar,
        mode: row.mode.clone(),
        level: row.level.and_then(|l| u32::try_from(l).ok()),
        xp: row.xp.and_then(|x| u64::try_from(x).ok()),
        online: row.online,
        last_seen_at: row.last_seen.map(at),
        club: row.club.clone().map(|(tag, name)| PlayerClub { tag, name }),
        ref_: None,
    }
}

/// `GET /internal/dashboard/v1/players?game&mode&q&sort&page`: 200 [`PlayerList`]; 400
/// `REQUEST.INVALID` `mode`, `q`, `sort`, `page` (1 to 100) or `game`.
pub(super) async fn list(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<PlayerList>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let prefix = prefix_of(&q)?;
    let sort = sort_of(&q)?;
    let page = page_of(&q)?;
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let size = i64::from(PLAYERS_PAGE_SIZE);
    let (total, rows) = db::players_in(
        &mut conn,
        &game,
        &mode,
        prefix.as_deref(),
        sort,
        (i64::from(page) - 1) * size,
        size,
    )
    .await?;
    Ok(Json(PlayerList {
        total: count(total),
        page,
        rows: rows.iter().map(card).collect(),
    }))
}

/// The families `bests` lists, in order.
const BEST_FAMILIES: [Fam; 5] = [
    Fam::SpeedTraps,
    Fam::SpeedZones,
    Fam::Showcases,
    Fam::Rivals,
    Fam::Stats,
];

/// The catalog boards of `fam` the profile has a row on (`rows`), in catalog order.
fn boards_with_rows(fam: Fam, month: i64, rows: &[BoardKey]) -> Vec<boards::Board> {
    let routes: HashSet<(i32, i32, i32)> = rows
        .iter()
        .map(|k| (k.scoreboard_type, k.score_type, k.track_id))
        .collect();
    let events: HashSet<(i32, i32, i32)> = rows
        .iter()
        .map(|k| (k.scoreboard_type, k.score_type, k.game_scoreboard_id))
        .collect();
    boards::family_boards(fam, month)
        .into_iter()
        .filter(|b| {
            let (sbt, st) = (b.one.scoreboard_type, b.one.score_type);
            match b.one.filter {
                BoardFilter::Route(t) => routes.contains(&(sbt, st, t)),
                BoardFilter::Event(e) => events.contains(&(sbt, st, e)),
            }
        })
        .collect()
}

/// `records` and `bests` of one profile: per family, only the boards it has rows on, ranked with
/// the game's ranking.
async fn ranks(
    conn: &mut sqlx::PgConnection,
    game: &str,
    p: &db::CardRow,
) -> Result<(u64, Vec<PersonalBest>), ApiError> {
    let month = boards::current_month();
    let rows = db_boards::profile_boards_in(conn, game, &p.mode, p.profile_id).await?;
    let mut ranked_families = Vec::new();
    for fam in boards::FAMILIES {
        let candidates = boards_with_rows(fam, month, &rows);
        if candidates.is_empty() {
            continue;
        }
        let keys: Vec<i32> = candidates.iter().map(|b| b.key).collect();
        let ranked = db_boards::profile_ranks_in(
            conn,
            game,
            &p.mode,
            &family_read(fam, &keys),
            p.profile_id,
        )
        .await?;
        ranked_families.push((fam, candidates, ranked));
    }
    Ok(records_and_bests(&ranked_families))
}

/// A profile's ranks in one family: the family, its catalog boards the profile has rows on (in
/// catalog order) and the profile's ranks on them.
pub type RankedFamily = (Fam, Vec<boards::Board>, Vec<db_boards::ProfileRank>);

/// `records` (the boards where the profile is the game's #1, every family) and `bests` (its rank
/// on the speed traps, speed zones, Showcases, its 10 best-ranked rivals routes by rank then
/// catalog order, and the stats boards) from its ranks per family: `(family, its catalog boards in
/// catalog order, the profile's ranks on them)`. The profile route and the hourly snapshot both
/// build them here.
pub fn records_and_bests(families: &[RankedFamily]) -> (u64, Vec<PersonalBest>) {
    let mut records = 0;
    let mut bests = Vec::new();
    for (fam, candidates, ranked) in families {
        let fam = *fam;
        records += ranked.iter().filter(|r| r.rank == 1).count() as u64;
        if !BEST_FAMILIES.contains(&fam) {
            continue;
        }
        let mut family: Vec<PersonalBest> = candidates
            .iter()
            .filter_map(|b| Some((b, ranked.iter().find(|r| r.board == b.key)?)))
            .map(|(b, r)| PersonalBest {
                family: fam.slug().to_owned(),
                board: b.summary.board.clone(),
                title: b.summary.title.clone(),
                unit: b.unit,
                rank: count(r.rank),
                score: in_unit(b.unit, r.score),
                car: (fam != Fam::Stats).then(|| board_car(r.car_id, r.car_class_id, r.car_pi)),
                at: at(r.submitted_at),
            })
            .collect();
        if fam == Fam::Rivals {
            // The 10 best-ranked routes, by rank then catalog order (the sort is stable).
            family.sort_by_key(|b| b.rank);
            family.truncate(BESTS_MAX_RIVALS);
        }
        bests.extend(family);
    }
    (records, bests)
}

/// A `car` stat row's car: the stock class and the PI the game shows for it
/// ([`fi_gamedb::pi::catalog_display`]: the catalog's raw placed in its stock class's band, T-PD-S6)
/// ("Car {id}" without class letter for a car the base catalog lacks).
pub(crate) fn stat_car(id: i32) -> BoardCar {
    let catalog = Catalog::fh2();
    match catalog.car(i64::from(id)) {
        Some(c) => BoardCar {
            id,
            name: c.name(),
            class: class_name(catalog, id, i16::try_from(c.class_id).unwrap_or(-1)),
            pi: fi_gamedb::pi::catalog_display(c),
        },
        None => BoardCar {
            id,
            name: car_name(catalog, id),
            class: String::new(),
            pi: 0,
        },
    }
}

/// The Statistics tabs of a stored online profile (`fi_gamedb::stats`).
pub fn stat_tabs(data: &serde_json::Value) -> Vec<StatTab> {
    StatsLayout::fh2()
        .evaluate(data)
        .into_iter()
        .filter_map(|(tab, rows)| {
            let rows: Vec<StatRow> = rows
                .into_iter()
                .filter_map(|r| {
                    let format = StatFormat::parse(&r.format)?;
                    Some(StatRow {
                        key: r.key,
                        format,
                        value: r.value,
                        of: r.of,
                        car: (format == StatFormat::Car)
                            .then(|| i32::try_from(r.value as i64).ok().map(stat_car))
                            .flatten(),
                        station: r.station,
                    })
                })
                .collect();
            (!rows.is_empty()).then_some(StatTab { tab, rows })
        })
        .collect()
}

/// `GET /internal/dashboard/v1/players/{name}?game`: 200 [`PlayerProfile`]; 404
/// `PLAYERS.NOT_FOUND` for a missing, malformed or hidden name alike.
pub(super) async fn profile(
    State(s): State<Dash>,
    Path(name): Path<String>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<PlayerProfile>, ApiError> {
    let q = query(q)?;
    if !plain_name(&name) {
        return Err(not_found());
    }
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    let p = db::player_in(&mut conn, &game, &name)
        .await?
        .ok_or_else(not_found)?;
    let (records, bests) = ranks(&mut conn, &game, &p).await?;
    let data = fi_db::gameplane::profile_in(&mut conn, &game, p.profile_id).await?;
    Ok(Json(PlayerProfile {
        card: card(&p),
        since: at(p.since),
        records,
        bests,
        tabs: data.as_ref().map(stat_tabs).unwrap_or_default(),
    }))
}

fn bare_404() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// `GET /internal/dashboard/v1/media/avatar/{name}?game`: 200 `image/png`, or a bare 404 (no such
/// listed player, no linked avatar, a CDN refusal or a non-PNG body).
pub(super) async fn avatar(
    State(s): State<Dash>,
    Path(name): Path<String>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Response {
    let Ok(q) = query(q) else {
        return bare_404();
    };
    let Some(avatars) = s.avatars.clone() else {
        return bare_404();
    };
    if !plain_name(&name) {
        return bare_404();
    }
    let game = game(&q);
    // The connection goes back to the pool before the network call.
    let found = match s.pool.acquire().await {
        Ok(mut conn) => db_avatar::avatar_of_in(&mut conn, &game, &name).await,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Ok(Some((user, hash))) = found else {
        return bare_404();
    };
    match avatars.fetch(user, &hash).await {
        Some(png) => ([(CONTENT_TYPE, HeaderValue::from_static("image/png"))], png).into_response(),
        None => bare_404(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use serde_json::json;

    #[test]
    fn names_and_queries_are_checked() {
        assert!(plain_name("Driver7") && plain_name("a"));
        for bad in ["", "7up", "a b", "a_b", "Ação", &"a".repeat(21), "a%"] {
            assert!(!plain_name(bad), "{bad}");
        }
        let q = |text: &str| DashQuery {
            q: Some(text.into()),
            ..DashQuery::default()
        };
        assert_eq!(prefix_of(&q("DrI")).unwrap(), Some("dri".into()));
        assert_eq!(prefix_of(&q(" ")).unwrap(), None);
        for bad in ["a", &"a".repeat(16), "a%", "a_", "ab cd"] {
            assert!(prefix_of(&q(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn stat_tabs_carry_cars_stations_and_totals() {
        let data = json!({
            "miscStats": [{"favouriteCar": 247, "favouriteRadioStation": 0, "photosTaken": 9}],
            "compareStats": [{"barnFinds": 4}],
        });
        let tabs = stat_tabs(&data);
        assert_eq!(
            tabs.iter().map(|t| t.tab.as_str()).collect::<Vec<_>>(),
            vec!["general", "discovery"]
        );
        let general = &tabs[0].rows;
        let car = general.iter().find(|r| r.key == "favoriteCar").unwrap();
        assert_eq!(car.format, StatFormat::Car);
        assert_eq!(
            car.car,
            Some(BoardCar {
                id: 247,
                name: "Toyota 2000 GT 1969".into(),
                class: "D".into(),
                pi: 404
            })
        );
        let radio = general
            .iter()
            .find(|r| r.key == "favoriteRadioStation")
            .unwrap();
        assert_eq!((radio.value, radio.station.as_deref()), (0.0, None));
        let barns = &tabs[1].rows[0];
        assert_eq!(
            (barns.format, barns.value, barns.of),
            (StatFormat::Of, 4.0, Some(10))
        );
    }

    /// A stub CDN: `/avatars/1/<hash>.png` per behaviour.
    async fn stub() -> String {
        let png = || {
            let mut b = PNG_SIGNATURE.to_vec();
            b.extend_from_slice(&[0u8; 64]);
            b
        };
        let big = move || {
            let mut b = PNG_SIGNATURE.to_vec();
            b.resize(AVATAR_MAX_BYTES + 1, 0);
            b
        };
        let app = Router::new().route(
            "/avatars/{user}/{file}",
            get(
                move |Path((user, file)): Path<(String, String)>| async move {
                    if user == "2" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    match file.trim_end_matches(".png") {
                        h if h == "0".repeat(32) => png().into_response(),
                        h if h == "1".repeat(32) => b"GIF89a not a png".to_vec().into_response(),
                        h if h == "2".repeat(32) => big().into_response(),
                        h if h == "3".repeat(32) => (
                            StatusCode::FOUND,
                            [(axum::http::header::LOCATION, "/avatars/1/x.png")],
                        )
                            .into_response(),
                        h if h == "4".repeat(32) => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            png().into_response()
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A raw HTTP server answering one chunked PNG body over the cap, without Content-Length.
    async fn chunked_stub() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ntransfer-encoding: chunked\r\n\r\n";
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut first = PNG_SIGNATURE.to_vec();
                    first.resize(16_384, 0);
                    for i in 0..40 {
                        let chunk = if i == 0 {
                            first.clone()
                        } else {
                            vec![0u8; 16_384]
                        };
                        let frame = format!("{:x}\r\n", chunk.len());
                        if sock.write_all(frame.as_bytes()).await.is_err()
                            || sock.write_all(&chunk).await.is_err()
                            || sock.write_all(b"\r\n").await.is_err()
                        {
                            return;
                        }
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_avatar_fetch_takes_only_small_pngs() {
        let base = stub().await;
        let a = Avatars::new(&base, Duration::from_millis(500)).unwrap();
        let h = |c: char| c.to_string().repeat(32);
        let png = a.fetch(1, &h('0')).await.unwrap();
        assert!(png.starts_with(&PNG_SIGNATURE));
        assert!(a.fetch(1, &h('1')).await.is_none(), "not a PNG");
        assert!(a.fetch(1, &h('2')).await.is_none(), "over the cap");
        assert!(a.fetch(1, &h('3')).await.is_none(), "a redirect");
        assert!(a.fetch(1, &h('4')).await.is_none(), "too slow");
        let chunked = Avatars::new(&chunked_stub().await, Duration::from_secs(2)).unwrap();
        assert!(
            chunked.fetch(1, &h('0')).await.is_none(),
            "streamed over the cap"
        );
        assert!(a.fetch(1, &h('6')).await.is_none(), "404");
        assert!(a.fetch(2, &h('0')).await.is_none(), "another user: 404");
        // Shapes the database never holds are refused before any request.
        assert!(a.fetch(0, &h('0')).await.is_none());
        assert!(a.fetch(1, "../../x").await.is_none());
        assert!(
            a.fetch(1, &format!("a_{}", h('0'))).await.is_none(),
            "animated: 404 here"
        );
        // The real CDN client is HTTPS only.
        assert!(Avatars::discord().is_some());
    }

    #[tokio::test]
    #[ignore = "needs Postgres (FI_TEST_DATABASE_URL)"]
    async fn the_avatar_route_relays_a_listed_players_png_only() {
        use tower::ServiceExt;
        let url = std::env::var("FI_TEST_DATABASE_URL").expect("FI_TEST_DATABASE_URL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        fi_db::migrate::run(&pool).await.unwrap();
        let (profile, name): (String, String) = sqlx::query_as(
            "WITH a AS (INSERT INTO account (origin, discord_user_id, discord_username, discord_avatar, \
               discord_linked_at, discord_refreshed_at) \
               VALUES ('registered', 1400000000000000000 + (random() * 1e12)::bigint, 'av_user', \
                 '00000000000000000000000000000000', now(), now()) RETURNING id), \
             p AS (INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin) \
               SELECT a.id, 'fh2', nextval('profile_xuid_seq'), 'Av' || substr(md5(random()::text), 1, 12), 1, \
                 'noob', 'created' FROM a RETURNING id, name) \
             SELECT p.id::text, p.name FROM p",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let avatars = Avatars::new(&stub().await, Duration::from_secs(2));
        let app = super::super::router_with(
            pool.clone(),
            super::super::DashboardKey::new("k3y"),
            avatars,
        );
        let get = |path: String| {
            let app = app.clone();
            async move {
                let res = app
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .header(fi_protocol::dashboard::KEY_HEADER, "k3y")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = res.status();
                let kind = res
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (status, kind, body)
            }
        };
        let path = |n: &str| format!("/internal/dashboard/v1/media/avatar/{n}");
        let (status, kind, body) = get(path(&name.to_lowercase())).await;
        assert_eq!((status, kind.as_str()), (StatusCode::OK, "image/png"));
        assert!(body.starts_with(&PNG_SIGNATURE));
        assert_eq!(get(path("Nobody0")).await.0, StatusCode::NOT_FOUND);
        // Hidden from the website: 404.
        sqlx::query("UPDATE profile SET web_listed = false WHERE id = $1::uuid")
            .bind(&profile)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(get(path(&name)).await.0, StatusCode::NOT_FOUND);
        // Listed again but without an avatar: 404.
        sqlx::query("UPDATE profile SET web_listed = true WHERE id = $1::uuid")
            .bind(&profile)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE account SET discord_avatar = NULL WHERE id = (SELECT account_id FROM profile WHERE id = $1::uuid)",
        )
        .bind(&profile)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(get(path(&name)).await.0, StatusCode::NOT_FOUND);
    }
}
