//! The documents the game reads, shaped as the Beta's `freeroam.rs` (ported from fh2-mods'
//! `fh2srv.py`, MIT) wrote them: fixed game-server-directory answers, MPSD session documents
//! (FH2's contract 105, FM5's 104) with their members and the game-server entry, SmartMatch ticket
//! details. What differs per game is its [`SessionProfile`] (FM5_LOBBIES.md §3). Pure JSON work;
//! nothing here reads or writes Postgres. Every access to a client-written shape goes through
//! [`obj`], which never panics on a non-object.

use fi_domain::UnixMs;
use fi_domain::game::{GameRules, Session};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;
use uuid::Uuid;

/// The game's free-roam variant id (fh2srv.py 86-88).
pub const GAME_VARIANT: &str = "00000000-0000-0000-0000-00000000f001";
/// Its variant schema id.
pub const VARIANT_SCHEMA: &str = "00000000-0000-0000-0000-00000000f002";
/// Its server-image set id.
pub const GSI_SET: &str = "00000000-0000-0000-0000-00000000f003";
/// The server image the game asks for: `Release_` + its build hash + `.FreeRoam`.
pub const GSI_NAME: &str = "Release_6CC152FD.FreeRoam";
/// The game's default datacenter.
pub const LOCATION: &str = "North Central US";
/// The hopper that meets in a world.
pub const FREEROAM_HOPPER: &str = "freeroam_qos";
/// The car meet's hopper, found in the caller's world (the island car meet shares it).
pub const CARMEET_HOPPER: &str = "carmeet_qos";
/// The template of a car meet's target session: the game's target template (other projects used as
/// reference and the §3.8 invite session use it), never the ticket session's `defaultTemplate_qos`.
pub const CARMEET_TARGET_TEMPLATE: &str = "defaultTemplate";
/// Online Road Trip's hopper, found in the caller's world (S8; other projects used as reference).
pub const ROADTRIP_HOPPER: &str = "roadtrip_qos";
/// Base64 of `fh2net-game-server`: the server address when the caller has no world.
pub const PLACEHOLDER_ADDRESS: &str = "ZmgybmV0LWdhbWUtc2VydmVy";
/// The host name when the caller has no world.
pub const PLACEHOLDER_HOST: &str = "127.0.0.1";
/// Prefix of the address text the hook decodes.
pub const ADDRESS_PREFIX: &str = "fh2inf:";
/// The one QoS server's name.
pub const QOS_FQDN: &str = "qos.fh2net.local";
/// Base64 of `fh2net-qos-server`.
pub const QOS_ADDRESS: &str = "ZmgybmV0LXFvcy1zZXJ2ZXI=";
/// The longest gamertag the game shows.
pub const GAME_TAG_CHARS: usize = 15;
/// The title id the game's members carry.
pub const ACTIVE_TITLE_ID: &str = "446059611";
/// One game's free-roam session values (FM5_LOBBIES.md §3), chosen by the ticket's game through
/// [`GameRules::session`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionProfile {
    /// The MPSD contract version written in documents.
    pub contract_version: u32,
    /// The only service config accepted in paths; `None`: any (FH2, as the Beta).
    pub scid: Option<&'static str>,
    /// The `hopperId` of the game's own private session ([`is_private_create`]).
    pub private_hopper_id: i64,
    /// `maxMembersCount` / `maxAllowedPlayers`.
    pub max_members: u32,
    /// The `variants` GSI set name.
    pub gsi_set_name: &'static str,
    /// The GSI set id.
    pub gsi_set: &'static str,
    /// The variant id.
    pub game_variant: &'static str,
    /// The variant schema id.
    pub variant_schema: &'static str,
    /// The title id members carry (`activeTitleId`).
    pub title_id: &'static str,
    /// The default datacenter.
    pub location: &'static str,
    /// SmartMatch (`momatch`), car meets and Road Trip: FH2's lobbies.
    pub fh2_lobbies: bool,
    /// `DELETE …/sessions/{name}/members/me` is the caller leaving (FM5's game sends it; the lab's
    /// `sessions.py` answered it).
    pub member_leave_path: bool,
}

/// FH2's profile: the values the platform has always written.
pub const FH2_SESSION: SessionProfile = SessionProfile {
    contract_version: 105,
    scid: None,
    private_hopper_id: 2,
    max_members: 12,
    gsi_set_name: GSI_NAME,
    gsi_set: GSI_SET,
    game_variant: GAME_VARIANT,
    variant_schema: VARIANT_SCHEMA,
    title_id: ACTIVE_TITLE_ID,
    location: LOCATION,
    fh2_lobbies: true,
    member_leave_path: false,
};

/// FM5's service config.
pub const FM5_SCID: &str = "a72e0e3d-ce31-4ae0-89b8-741c625ab197";

/// FM5's profile (CR-S99): the lab's working values (`tools/fm5-probe`), with a GSI set name the
/// game matches by `Priv_D6ED0951.*` and the hopper's `cloudGsiSetPattern`.
pub const FM5_SESSION: SessionProfile = SessionProfile {
    contract_version: 104,
    scid: Some(FM5_SCID),
    private_hopper_id: -1,
    max_members: 16,
    gsi_set_name: "Priv_D6ED0951.Infinity",
    gsi_set: "00000000-0000-0000-0000-00000000f503",
    game_variant: "00000000-0000-0000-0000-00000000f501",
    variant_schema: "00000000-0000-0000-0000-00000000f502",
    // 0x7B35D917
    title_id: "2067126551",
    location: LOCATION,
    fh2_lobbies: false,
    member_leave_path: true,
};

impl SessionProfile {
    /// The profile of `game`'s rules (a stored test title gets FH2's, as [`GameRules::of`]).
    pub fn of(game: &str) -> &'static SessionProfile {
        match GameRules::of(game).session {
            Session::Fh2 => &FH2_SESSION,
            Session::Fm5 => &FM5_SESSION,
        }
    }

    /// Whether a path's service config is this game's.
    pub fn accepts_scid(&self, scid: &str) -> bool {
        self.scid.is_none_or(|s| s.eq_ignore_ascii_case(scid))
    }
}

/// Top-level fields only the server writes (the world address among them).
pub const SERVER_FIELDS: [&str; 7] = [
    "servers",
    "membersInfo",
    "correlationId",
    "contractVersion",
    "startTime",
    "nextTimer",
    "hostCandidates",
];

/// `secureDeviceAddress`: base64 (standard, padded) of `fh2inf:<ip>:<port>`, IPv6 in brackets.
pub fn secure_device_address(addr: &SocketAddr) -> String {
    fi_auth::b64::std_encode(format!("{ADDRESS_PREFIX}{addr}").as_bytes())
}

/// `servers.gameServerPlatform` for `world` (the placeholder without one): the game reads
/// `properties.custom` `{status, datacenter, secureDeviceAddress, hostName}`.
pub fn server_entry(world: Option<SocketAddr>, location: &str) -> Value {
    let (address, host) = address_and_host(world);
    json!({
        "constants": { "system": {}, "custom": {} },
        "properties": { "system": {}, "custom": {
            "status": "Active", "datacenter": location, "secureDeviceAddress": address, "hostName": host,
        } },
    })
}

/// The address and host name of `world` (the placeholder pair without one).
pub fn address_and_host(world: Option<SocketAddr>) -> (String, String) {
    match world {
        Some(w) => (secure_device_address(&w), w.ip().to_string()),
        None => (PLACEHOLDER_ADDRESS.to_owned(), PLACEHOLDER_HOST.to_owned()),
    }
}

/// `POST gameserverds …/variants`: every value a JSON string (the game's parser throws on
/// numbers and booleans there).
pub fn variants(p: &SessionProfile) -> Value {
    json!({
        "variants": [{
            "gamevariantId": p.game_variant, "name": p.gsi_set_name, "isPublisher": "false", "rank": "1",
            "gameVariantSchemaId": p.variant_schema,
        }],
        "variantSchemas": [{ "variantSchemaId": p.variant_schema, "schemaContent": "{}", "name": p.gsi_set_name }],
        "gsiSets": [{
            "minRequiredPlayers": "1", "maxAllowedPlayers": p.max_members.to_string(), "gsiSetId": p.gsi_set,
            "gsiSetName": p.gsi_set_name, "selectionOrder": "1", "variantSchemaId": p.variant_schema,
        }],
    })
}

/// `GET gameserverds /qosservers`: exactly one key, `qosServers` (with a second spelling the
/// game read an empty list).
pub fn qos_servers(p: &SessionProfile) -> Value {
    json!({
        "qosServers": [{
            "serverFqdn": QOS_FQDN, "serverSecureDeviceAddress": QOS_ADDRESS, "targetLocation": p.location,
        }],
    })
}

/// `POST gameserverms …/clusters`: fulfilled at once.
pub fn cluster(world: Option<SocketAddr>, p: &SessionProfile) -> Value {
    let (address, host) = address_and_host(world);
    json!({
        "pollIntervalMilliseconds": 1000,
        "fulfillmentState": "Fulfilled",
        "hostName": host,
        "region": p.location,
        "secureDeviceAddress": address,
        "portMappings": [{ "name": "Game", "value": 5100, "internal": 5100, "external": 5100 }],
    })
}

/// The object at `key` of `v`, replacing whatever else is there (a non-object `v` becomes `{}`).
pub fn obj<'a>(v: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    let Value::Object(m) = v else {
        unreachable!("made an object above")
    };
    let slot = m
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    match slot {
        Value::Object(o) => o,
        _ => unreachable!("made an object above"),
    }
}

/// Deep merge (fh2srv.py 128-132): objects merge, a `null` removes the key, anything else
/// replaces.
pub fn merge(dst: &mut Map<String, Value>, src: Map<String, Value>) {
    for (k, v) in src {
        match v {
            Value::Null => {
                dst.remove(&k);
            }
            Value::Object(o) if dst.get(&k).is_some_and(Value::is_object) => {
                if let Some(Value::Object(d)) = dst.get_mut(&k) {
                    merge(d, o);
                }
            }
            v => {
                dst.insert(k, v);
            }
        }
    }
}

/// Now, as the session directory wrote instants: UTC, three fraction digits.
pub fn now_iso() -> String {
    iso_ms(UnixMs::now_system().millis())
}

/// Unix milliseconds as `2026-10-17T14:00:00.123Z`.
pub fn iso_ms(millis: i64) -> String {
    let wire = fi_protocol::instant::format_unix_ms(millis).unwrap_or_default();
    // `…T14:00:00.123000Z` → `…T14:00:00.123Z`
    match wire.get(..23) {
        Some(head) => format!("{head}Z"),
        None => wire,
    }
}

/// A new session document: the fields the game's session deserializer reads.
pub fn new_session(p: &SessionProfile, correlation: Uuid, now: &str) -> Value {
    json!({
        "contractVersion": p.contract_version, "correlationId": correlation.to_string(), "startTime": now, "nextTimer": now,
        "constants": { "system": {}, "custom": {} },
        "properties": { "system": { "keywords": [], "turn": [], "joinRestriction": "none" }, "custom": {} },
        "servers": {}, "members": {}, "hostCandidates": [],
        "membersInfo": { "first": 0, "next": 0, "count": 0, "accepted": 0, "active": 0 },
    })
}

/// A SmartMatch target session: open, the game's members (FH2: 12).
pub fn new_target_session(p: &SessionProfile, correlation: Uuid, now: &str) -> Value {
    let mut doc = new_session(p, correlation, now);
    doc["constants"]["system"] = json!({ "visibility": "open", "maxMembersCount": p.max_members });
    doc
}

/// The caller as a session member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Me {
    /// XUID.
    pub xuid: i64,
    /// The gamertag the game shows (≤ 15 characters).
    pub gamertag: String,
    /// 32 upper-case hex digits, stable for the play session.
    pub device_token: String,
    /// The game's title id ([`SessionProfile::title_id`]).
    pub title_id: &'static str,
}

/// The device token of a member: stable for one play session, not computable from the public
/// XUID (the Beta used a per-run secret; the play session id is known only to its ticket).
pub fn device_token(play_session: Uuid, profile: Uuid) -> String {
    let mut seed = b"fi-freeroam-device-v1\n".to_vec();
    seed.extend_from_slice(play_session.as_bytes());
    seed.extend_from_slice(profile.as_bytes());
    fi_auth::sha256(&seed)[..16]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// A gamertag cut to the game's 15 characters, keeping `#number` visible; `Driver <n>` without
/// one.
pub fn game_tag(gamertag: Option<&str>, xuid: i64) -> String {
    let Some(tag) = gamertag.filter(|t| !t.is_empty()) else {
        return format!("Driver {}", xuid.rem_euclid(10_000));
    };
    if tag.chars().count() <= GAME_TAG_CHARS {
        return tag.to_owned();
    }
    match tag.rsplit_once('#') {
        Some((name, number)) if number.chars().count() < GAME_TAG_CHARS => {
            let room = GAME_TAG_CHARS - 1 - number.chars().count();
            format!("{}#{number}", name.chars().take(room).collect::<String>())
        }
        _ => tag.chars().take(GAME_TAG_CHARS).collect(),
    }
}

fn new_member(me: &Me, index: u64, now: &str, reserved: bool) -> Value {
    json!({
        "constants": { "system": { "xuid": me.xuid.to_string(), "index": index }, "custom": {} },
        "properties": { "system": { "active": false, "ready": false }, "custom": {} },
        "gamertag": me.gamertag,
        "deviceToken": me.device_token,
        "nat": "open",
        "activeTitleId": me.title_id,
        "reserved": reserved,
        "joinTime": now,
        "turn": false,
        "initializationEpisode": 0,
    })
}

/// The member key of `xuid` in `doc`.
pub fn member_of(doc: &Value, xuid: i64) -> Option<String> {
    let wanted = json!(xuid.to_string());
    doc["members"]
        .as_object()?
        .iter()
        .find(|(_, v)| v["constants"]["system"]["xuid"] == wanted)
        .map(|(k, _)| k.clone())
}

fn next_index(doc: &mut Value) -> u64 {
    let info = obj(doc, "membersInfo");
    let next = info.get("next").and_then(Value::as_u64).unwrap_or(0);
    info.insert("next".into(), json!(next + 1));
    next
}

/// Keeps the server-owned shape a body may have nulled out: `constants`, `servers`, `members`
/// and `membersInfo` are objects.
fn ensure_shape(doc: &mut Value) {
    for field in ["constants", "servers", "members"] {
        obj(doc, field);
    }
    let info = obj(doc, "membersInfo");
    for field in ["first", "next", "count", "accepted", "active"] {
        info.entry(field.to_owned()).or_insert(json!(0));
    }
}

/// An MPSD write (`PUT`/`POST …/sessions/{name}`, fh2srv.py 179-202) applied to `doc`: the
/// server's fields dropped from the body, the rest merged, and `members.me` as the caller (an
/// object adds or updates the caller, anything else removes it); other members of the body are
/// ignored, so nobody writes another player into a session (the proxy already refuses such a
/// body, and a `me` naming another XUID, with `GAME.FOREIGN_XUID`: `proxy::foreign`).
pub fn apply_write(doc: &mut Value, mut body: Map<String, Value>, me: &Me, now: &str) {
    let members = match body.remove("members") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for field in SERVER_FIELDS {
        body.remove(field);
    }
    if let Value::Object(d) = doc {
        merge(d, body);
    }
    ensure_shape(doc);
    for (name, member) in members {
        if name != "me" {
            continue;
        }
        let index = member_of(doc, me.xuid);
        let Value::Object(member) = member else {
            if let Some(i) = index {
                obj(doc, "members").remove(&i);
            }
            continue;
        };
        let index = match index {
            Some(i) => i,
            None => {
                let next = next_index(doc);
                let key = next.to_string();
                obj(doc, "members").insert(key.clone(), new_member(me, next, now, false));
                key
            }
        };
        let number: u64 = index.parse().unwrap_or(0);
        let slot = obj(doc, "members")
            .entry(index)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = slot {
            merge(m, member);
        }
        // The member stays the caller whatever the body said.
        let constants = obj(slot, "constants");
        let system = obj_in(constants, "system");
        system.insert("xuid".into(), json!(me.xuid.to_string()));
        system.insert("index".into(), json!(number));
    }
}

fn obj_in<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = m
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    match slot {
        Value::Object(o) => o,
        _ => unreachable!("made an object above"),
    }
}

/// The datacenter a document asks for (its `constants.custom.gameServerPlatform.location`), else
/// the game's.
pub fn location_of(doc: &Value, p: &SessionProfile) -> String {
    doc["constants"]["custom"]["gameServerPlatform"]["location"]
        .as_str()
        .unwrap_or(p.location)
        .to_owned()
}

/// Whether a write is the game's own private session (WORLDS_OFFICIAL.md §12.8a, T-R2): the
/// **request body** declares `gameServerPlatform` and the private hopper with no `matchmaking`, on
/// `defaultTemplate_qos`, under a name of the game's (every server-made session is `fh2inf-…`).
/// `CreatedPrivateSession` is no trigger: the game does not send it here, and a car meet invitation
/// writes it. Road Trip's marker is unknown: a second run showed this same body. FM5's private
/// session is the same shape with its `hopperId` -1 (FM5_LOBBIES.md §2, T1 #23).
pub fn is_private_create(
    p: &SessionProfile,
    template: &str,
    name: &str,
    body: &serde_json::Map<String, Value>,
) -> bool {
    let custom = body
        .get("constants")
        .and_then(|c| c.get("custom"))
        .unwrap_or(&Value::Null);
    template.eq_ignore_ascii_case("defaultTemplate_qos")
        && !name.starts_with("fh2inf-")
        && custom
            .get("gameServerPlatform")
            .is_some_and(Value::is_object)
        && custom.get("hopperId").and_then(Value::as_i64) == Some(p.private_hopper_id)
        && custom.get("matchmaking").is_none()
        && body.get("matchmaking").is_none()
}

/// `Some(location)` when the document is a game session (`constants.custom.gameServerPlatform`)
/// still without its server (fh2srv.py 203-211).
pub fn needs_server(doc: &Value, p: &SessionProfile) -> Option<String> {
    let platform = &doc["constants"]["custom"]["gameServerPlatform"];
    if platform.is_null() || doc["servers"].get("gameServerPlatform").is_some() {
        return None;
    }
    Some(location_of(doc, p))
}

/// A car meet invite session as a member reads it (an official group session, and since CR-S93 a
/// player world's too: its world server is `servers.gameServerPlatform` either way, so no member
/// needs another as its host): that member first (index 0), the others after it in their order. The game
/// joins such a session only from index 0 with nobody named host: as index 1 it left whatever the
/// rest showed (a named host, an active first member, filled addresses; local test 2026-11-03, 6 of
/// 6), while every read at index 0 joined. The world server is `servers.gameServerPlatform`, so no
/// member needs another as its host. Only the answer changes, never the stored document; a member's
/// writes still land on its own entry by XUID.
pub fn reader_first(doc: &mut Value, reader: i64) {
    // the reader's own host claim, or none, whatever another stored (CR-S93: a private car meet's
    // joiner saw the owner's stored host as index 1 and claimed the role ~20/s, live 2026-11-04)
    let claim = doc[FI_HOST_CLAIMS][reader.to_string()]
        .as_str()
        .map(str::to_owned);
    if let Value::Object(props) = &mut doc["properties"] {
        let system = obj_in(props, "system");
        match &claim {
            Some(c) => system.insert("host".into(), json!(c)),
            None => system.remove("host"),
        };
    }
    let Some(members) = doc.get_mut("members").and_then(Value::as_object_mut) else {
        return;
    };
    let me = json!(reader.to_string());
    let mut entries: Vec<(u64, Value)> = members
        .iter()
        .filter_map(|(k, m)| k.parse::<u64>().ok().map(|i| (i, m.clone())))
        .collect();
    if !entries
        .iter()
        .any(|(_, m)| m["constants"]["system"]["xuid"] == me)
    {
        return;
    }
    entries.sort_by_key(|(i, m)| (m["constants"]["system"]["xuid"] != me, *i));
    members.clear();
    let mut reader_active = false;
    for (index, (_, mut m)) in entries.into_iter().enumerate() {
        if let Value::Object(o) = &mut m {
            obj_in(obj_in(o, "constants"), "system").insert("index".into(), json!(index));
            let system = obj_in(obj_in(o, "properties"), "system");
            if index == 0 {
                reader_active = system.get("active").and_then(Value::as_bool) == Some(true);
            } else {
                // the others shown inactive: a reader that saw another member active claimed the
                // host role in a loop instead of joining (local test 2026-11-03, 01a1a396)
                system.insert("active".into(), json!(false));
            }
        }
        members.insert(index.to_string(), m);
    }
    let count = members.len();
    if let Value::Object(info) = &mut doc["membersInfo"] {
        info.insert("first".into(), json!(0));
        info.insert("next".into(), json!(count));
        info.insert("active".into(), json!(u64::from(reader_active)));
    }
}

/// The server-only marker of an official car meet's group session (never answered).
pub const FI_OFFICIAL: &str = "fiOfficial";
/// The server-only per-member host claims of an official group session (never answered).
pub const FI_HOST_CLAIMS: &str = "fiHostClaims";

/// Removes the server-only fields before a document is answered.
pub fn strip_server_fields(doc: &mut Value) {
    if let Value::Object(o) = doc {
        o.remove(FI_OFFICIAL);
        o.remove(FI_HOST_CLAIMS);
    }
}

/// A shared session as a member reads it: every member without a `secureDeviceAddress` shows
/// the reader's own route to the world (`servers.gameServerPlatform`'s address). A joiner looks the
/// host's member up by `properties.system.host` and connects to its address; an empty one made it
/// report "the session you were trying to join no longer exists" (live, 2026-11-02 14:16). Every
/// address leads to the same world server. Only the answer changes, never the stored document.
/// Only a session that names a host (`properties.system.host`, written by a hosting game) is
/// filled: in a host-less one (an official car meet's group session) a joiner that found every
/// member already holding an address left it instead of marking itself active (local test
/// 2026-11-03); other projects used as reference leave those addresses empty.
/// The reader's own entry is never filled: a member's address is its own device, and a car meet
/// joiner that found the world's address on its own not-yet-joined entry left the session (local
/// test 2026-11-03, 01a1a38a).
pub fn fill_member_addresses(doc: &mut Value, reader: Option<i64>) {
    let reader = reader.map(|x| json!(x.to_string()));
    let names_host = doc["properties"]["system"]["host"]
        .as_str()
        .is_some_and(|h| !h.is_empty());
    if !names_host {
        return;
    }
    let Some(address) =
        doc["servers"]["gameServerPlatform"]["properties"]["custom"]["secureDeviceAddress"]
            .as_str()
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
    else {
        return;
    };
    let Some(members) = doc.get_mut("members").and_then(Value::as_object_mut) else {
        return;
    };
    for member in members.values_mut() {
        if reader.is_some() && member["constants"]["system"]["xuid"] == *reader.as_ref().unwrap() {
            continue;
        }
        let Value::Object(m) = member else { continue };
        let system = obj_in(obj_in(m, "properties"), "system");
        let empty = system
            .get("secureDeviceAddress")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if empty {
            system.insert("secureDeviceAddress".into(), json!(address));
        }
    }
}

/// Whether `doc`, stored under `template`, is the game's own car meet ticket session: a `_qos`
/// template whose constants carry `hopperId` 3. It never gets a server: other projects used as reference leave every `_qos`
/// session without one, and a ticket session already holding an Active server made the game drop
/// its car meet ticket 0.9 s after the POST, before any poll (live, 2026-11-02 13:05).
pub fn is_car_meet_ticket_session(template: &str, doc: &Value) -> bool {
    let hopper = &doc["constants"]["custom"]["hopperId"];
    template.to_ascii_lowercase().ends_with("_qos")
        && (hopper.as_i64() == Some(3) || hopper.as_str() == Some("3"))
}

/// Removes `servers.gameServerPlatform`.
pub fn clear_server(doc: &mut Value) {
    if let Some(servers) = doc.get_mut("servers").and_then(Value::as_object_mut) {
        servers.remove("gameServerPlatform");
    }
}

/// Sets `servers.gameServerPlatform`.
pub fn set_server(doc: &mut Value, entry: Value) {
    obj(doc, "servers").insert("gameServerPlatform".into(), entry);
}

/// `membersInfo.count`, `accepted`, `active` and `first` from the members (fh2srv.py 212-214).
pub fn count_members(doc: &mut Value) {
    let (count, active, first) = match doc["members"].as_object() {
        Some(m) => (
            m.len(),
            m.values()
                .filter(|v| v["properties"]["system"]["active"] == json!(true))
                .count(),
            m.keys().filter_map(|k| k.parse::<u64>().ok()).min(),
        ),
        None => (0, 0, None),
    };
    let info = obj(doc, "membersInfo");
    info.insert("count".into(), json!(count));
    info.insert("accepted".into(), json!(count));
    info.insert("active".into(), json!(active));
    if let Some(first) = first {
        info.insert("first".into(), json!(first));
    }
}

/// The ticket session's SmartMatch result: `servers.matchmaking` says found, with the target.
pub fn set_found(ticket_session: &mut Value, target_ref: &Value) {
    obj(ticket_session, "servers").insert(
        "matchmaking".into(),
        json!({
            "constants": { "system": {}, "custom": {} },
            "properties": { "system": {
                "status": "found", "statusDetails": "", "typicalWait": 0, "targetSessionRef": target_ref,
            }, "custom": {} },
        }),
    );
}

/// The caller reserved in a target session (SmartMatch did this; the game then writes `me`
/// active), unless already a member.
pub fn reserve(target: &mut Value, me: &Me, now: &str) {
    if member_of(target, me.xuid).is_some() {
        return;
    }
    let next = next_index(target);
    obj(target, "members").insert(next.to_string(), new_member(me, next, now, true));
    let count = obj(target, "members").len();
    let info = obj(target, "membersInfo");
    info.insert("count".into(), json!(count));
    info.insert("accepted".into(), json!(count));
}

/// Puts `me` into a car meet invite session (MATCHMAKING_API.md §3.8) as an inactive member that is
/// not reserved: the game looks for itself there and PUTs `properties.system.active = true`. A
/// reserved entry sends a joiner's game down the invite-accept path, which never completes here,
/// and it leaves the session again (local test 2026-11-03; other projects used as reference list invitees unreserved too).
pub fn invite(target: &mut Value, me: &Me, now: &str) {
    if member_of(target, me.xuid).is_some() {
        return;
    }
    let next = next_index(target);
    obj(target, "members").insert(next.to_string(), new_member(me, next, now, false));
    let count = obj(target, "members").len();
    let info = obj(target, "membersInfo");
    info.insert("count".into(), json!(count));
    info.insert("accepted".into(), json!(count));
}

/// Why a document cannot be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unstorable {
    /// A key or string holds U+0000 (Postgres's jsonb refuses it).
    Nul,
    /// Over the limit as Postgres writes it out.
    TooLarge,
}

/// Checks a document against what Postgres stores: no U+0000 in keys or strings, and at most
/// `max` bytes in jsonb's text form, where separators carry a space and a number is written out
/// in full (`1e308` is 309 digits there), so a small body can never grow past the limit in the
/// database. Stops counting as soon as the limit is passed.
pub fn storable(doc: &Value, max: usize) -> Result<(), Unstorable> {
    let mut total = 0usize;
    stored_len(doc, max, &mut total)?;
    Ok(())
}

fn add(total: &mut usize, n: usize, max: usize) -> Result<(), Unstorable> {
    *total = total.saturating_add(n);
    if *total > max {
        return Err(Unstorable::TooLarge);
    }
    Ok(())
}

fn text_len(s: &str, max: usize, total: &mut usize) -> Result<(), Unstorable> {
    if s.contains('\0') {
        return Err(Unstorable::Nul);
    }
    add(
        total,
        serde_json::to_string(s).map_or(usize::MAX, |t| t.len()),
        max,
    )
}

fn stored_len(v: &Value, max: usize, total: &mut usize) -> Result<(), Unstorable> {
    match v {
        Value::Null | Value::Bool(_) => add(total, 5, max),
        Value::Number(n) => {
            let len = match n.as_f64().filter(|_| n.is_f64()) {
                Some(f) => format!("{f}").len(),
                None => n.to_string().len(),
            };
            add(total, len, max)
        }
        Value::String(s) => text_len(s, max, total),
        Value::Array(a) => {
            add(total, 2 + 2 * a.len(), max)?;
            a.iter().try_for_each(|x| stored_len(x, max, total))
        }
        Value::Object(m) => {
            add(total, 2 + 4 * m.len(), max)?;
            m.iter().try_for_each(|(k, x)| {
                text_len(k, max, total)?;
                stored_len(x, max, total)
            })
        }
    }
}

#[cfg(test)]
mod private_create_tests {
    use super::{FH2_SESSION, FM5_SESSION, is_private_create};
    use serde_json::{Value, json};

    fn body(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// T-R2's body and everything near it that must not count (WORLDS_OFFICIAL.md §12.8a).
    #[test]
    fn only_the_games_own_private_session_is_a_private_create() {
        let t = "defaultTemplate_qos";
        let gsp = json!({"maxAllowedPlayers": 12, "location": "North Central US"});
        let private =
            body(json!({"constants": {"custom": {"hopperId": 2, "gameServerPlatform": gsp}}}));
        assert!(is_private_create(&FH2_SESSION, t, "6a1f", &private));
        assert!(is_private_create(
            &FH2_SESSION,
            "defaulttemplate_QOS",
            "6a1f",
            &private
        ));
        // Server-made: a car meet invitation's session, a match's target.
        assert!(!is_private_create(
            &FH2_SESSION,
            t,
            "fh2inf-invite-abc",
            &private
        ));
        assert!(!is_private_create(
            &FH2_SESSION,
            t,
            "fh2inf-3-abc",
            &private
        ));
        // Another template.
        assert!(!is_private_create(
            &FH2_SESSION,
            "CarMeet_qos",
            "6a1f",
            &private
        ));
        // A public hopper, a matchmade one, none, no platform.
        for other in [
            json!({"constants": {"custom": {"hopperId": 3, "gameServerPlatform": gsp}}}),
            json!({"constants": {"custom": {"hopperId": 2, "gameServerPlatform": gsp, "matchmaking": {}}}}),
            json!({"constants": {"custom": {"gameServerPlatform": gsp}}}),
            json!({"constants": {"custom": {"hopperId": 2}}}),
            json!({"properties": {"custom": {"CreatedPrivateSession": true}}}),
            json!({"members": {"me": {"properties": {"system": {"active": true}}}}}),
        ] {
            assert!(!is_private_create(&FH2_SESSION, t, "6a1f", &body(other)));
        }
        // FH2's private hopper is not FM5's, and FM5's is not FH2's.
        assert!(!is_private_create(&FM5_SESSION, t, "6a1f", &private));
    }

    /// FM5's host PUT (FM5_LOBBIES.md §2, T1 #23): `hopperId` -1, an empty `gameServerPlatform`.
    #[test]
    fn fm5s_private_session_is_its_hopper_minus_one() {
        let t = "defaultTemplate_qos";
        let fm5 = body(json!({"constants": {"custom": {
            "gameServerPlatform": {}, "hopperId": -1, "hopperName": "",
        }}}));
        assert!(is_private_create(&FM5_SESSION, t, "0c3e", &fm5));
        assert!(!is_private_create(&FH2_SESSION, t, "0c3e", &fm5));
        for other in [
            json!({"constants": {"custom": {"gameServerPlatform": {}, "hopperId": 2}}}),
            json!({"constants": {"custom": {"gameServerPlatform": {}, "hopperId": -1, "matchmaking": {}}}}),
            json!({"constants": {"custom": {"hopperId": -1}}}),
        ] {
            assert!(!is_private_create(&FM5_SESSION, t, "0c3e", &body(other)));
        }
        let mut with_mm = fm5.clone();
        with_mm.insert("matchmaking".into(), json!({}));
        assert!(!is_private_create(&FM5_SESSION, t, "0c3e", &with_mm));
    }
}

/// FH2's documents as the code wrote them before the per-game profile (CR-S99): every answer of
/// FH2's profile must stay byte for byte what these literals, copied from that code, give.
#[cfg(test)]
mod fh2_golden {
    use super::*;

    fn bytes(v: &Value) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }

    #[test]
    fn fh2s_documents_are_unchanged() {
        let variants_before = json!({
            "variants": [{
                "gamevariantId": "00000000-0000-0000-0000-00000000f001", "name": "Release_6CC152FD.FreeRoam",
                "isPublisher": "false", "rank": "1", "gameVariantSchemaId": "00000000-0000-0000-0000-00000000f002",
            }],
            "variantSchemas": [{ "variantSchemaId": "00000000-0000-0000-0000-00000000f002", "schemaContent": "{}",
                "name": "Release_6CC152FD.FreeRoam" }],
            "gsiSets": [{
                "minRequiredPlayers": "1", "maxAllowedPlayers": "12", "gsiSetId": "00000000-0000-0000-0000-00000000f003",
                "gsiSetName": "Release_6CC152FD.FreeRoam", "selectionOrder": "1",
                "variantSchemaId": "00000000-0000-0000-0000-00000000f002",
            }],
        });
        assert_eq!(bytes(&variants(&FH2_SESSION)), bytes(&variants_before));
        let qos_before = json!({
            "qosServers": [{
                "serverFqdn": "qos.fh2net.local", "serverSecureDeviceAddress": "ZmgybmV0LXFvcy1zZXJ2ZXI=",
                "targetLocation": "North Central US",
            }],
        });
        assert_eq!(bytes(&qos_servers(&FH2_SESSION)), bytes(&qos_before));
        let world: SocketAddr = "192.0.2.7:5101".parse().unwrap();
        for w in [None, Some(world)] {
            let (address, host) = address_and_host(w);
            let cluster_before = json!({
                "pollIntervalMilliseconds": 1000, "fulfillmentState": "Fulfilled", "hostName": host,
                "region": "North Central US", "secureDeviceAddress": address,
                "portMappings": [{ "name": "Game", "value": 5100, "internal": 5100, "external": 5100 }],
            });
            assert_eq!(bytes(&cluster(w, &FH2_SESSION)), bytes(&cluster_before));
        }
        let now = "2026-11-07T12:00:00.000Z";
        let session_before = json!({
            "contractVersion": 105, "correlationId": Uuid::nil().to_string(), "startTime": now, "nextTimer": now,
            "constants": { "system": {}, "custom": {} },
            "properties": { "system": { "keywords": [], "turn": [], "joinRestriction": "none" }, "custom": {} },
            "servers": {}, "members": {}, "hostCandidates": [],
            "membersInfo": { "first": 0, "next": 0, "count": 0, "accepted": 0, "active": 0 },
        });
        assert_eq!(
            bytes(&new_session(&FH2_SESSION, Uuid::nil(), now)),
            bytes(&session_before)
        );
        let mut target_before = session_before.clone();
        target_before["constants"]["system"] =
            json!({ "visibility": "open", "maxMembersCount": 12 });
        assert_eq!(
            bytes(&new_target_session(&FH2_SESSION, Uuid::nil(), now)),
            bytes(&target_before)
        );
        let me = Me {
            xuid: 2_535_100_000_000_001,
            gamertag: "Driver#00001".into(),
            device_token: device_token(Uuid::nil(), Uuid::nil()),
            title_id: FH2_SESSION.title_id,
        };
        let member_before = json!({
            "constants": { "system": { "xuid": me.xuid.to_string(), "index": 0 }, "custom": {} },
            "properties": { "system": { "active": false, "ready": false }, "custom": {} },
            "gamertag": me.gamertag, "deviceToken": me.device_token, "nat": "open",
            "activeTitleId": "446059611", "reserved": true, "joinTime": now, "turn": false,
            "initializationEpisode": 0,
        });
        let mut target = new_target_session(&FH2_SESSION, Uuid::nil(), now);
        reserve(&mut target, &me, now);
        assert_eq!(bytes(&target["members"]["0"]), bytes(&member_before));
        let mut doc = session_before;
        doc["constants"]["custom"]["gameServerPlatform"] = json!({"maxAllowedPlayers": 12});
        assert_eq!(location_of(&doc, &FH2_SESSION), "North Central US");
        assert_eq!(SessionProfile::of("fh2"), &FH2_SESSION);
        assert_eq!(SessionProfile::of("t0123456789"), &FH2_SESSION);
        assert!(FH2_SESSION.accepts_scid("00000000-0000-0000-0000-0000000000aa"));
    }

    #[test]
    fn fm5s_documents_are_the_labs() {
        let v = variants(&FM5_SESSION);
        assert_eq!(v["gsiSets"][0]["maxAllowedPlayers"], "16");
        assert_eq!(
            v["gsiSets"][0]["gsiSetId"],
            "00000000-0000-0000-0000-00000000f503"
        );
        assert_eq!(v["gsiSets"][0]["gsiSetName"], "Priv_D6ED0951.Infinity");
        assert_eq!(
            v["variants"][0]["gamevariantId"],
            "00000000-0000-0000-0000-00000000f501"
        );
        assert_eq!(
            v["variantSchemas"][0]["variantSchemaId"],
            "00000000-0000-0000-0000-00000000f502"
        );
        let s = new_session(&FM5_SESSION, Uuid::nil(), "t");
        assert_eq!(s["contractVersion"], 104);
        let t = new_target_session(&FM5_SESSION, Uuid::nil(), "t");
        assert_eq!(t["constants"]["system"]["maxMembersCount"], 16);
        assert_eq!(FM5_SESSION.title_id, 0x7B35_D917_u32.to_string());
        assert_eq!(SessionProfile::of("fm5"), &FM5_SESSION);
        assert!(FM5_SESSION.accepts_scid("A72E0E3D-CE31-4AE0-89B8-741C625AB197"));
        assert!(!FM5_SESSION.accepts_scid("00000000-0000-0000-0000-0000000000aa"));
        for g in fi_domain::game::GAMES {
            assert_eq!(
                SessionProfile::of(g.code).max_members,
                g.max_players,
                "{}",
                g.code
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Me {
        Me {
            xuid: 2_535_100_000_000_001,
            gamertag: "Driver#00001".into(),
            device_token: device_token(Uuid::nil(), Uuid::nil()),
            title_id: ACTIVE_TITLE_ID,
        }
    }

    #[test]
    fn addresses_are_the_hooks_format() {
        let v4: SocketAddr = "192.0.2.10:5101".parse().unwrap();
        assert_eq!(
            secure_device_address(&v4),
            "ZmgyaW5mOjE5Mi4wLjIuMTA6NTEwMQ=="
        );
        let v6: SocketAddr = "[2001:db8::1]:5101".parse().unwrap();
        let text = fi_auth::b64::std_decode(&secure_device_address(&v6)).unwrap();
        assert_eq!(text, b"fh2inf:[2001:db8::1]:5101");
        assert_eq!(
            secure_device_address(&v6),
            "ZmgyaW5mOlsyMDAxOmRiODo6MV06NTEwMQ=="
        );
        assert_eq!(
            fi_auth::b64::std_decode(PLACEHOLDER_ADDRESS).unwrap(),
            b"fh2net-game-server"
        );
        let entry = server_entry(Some(v4), "West Europe");
        let custom = &entry["properties"]["custom"];
        assert_eq!(custom["status"], "Active");
        assert_eq!(custom["datacenter"], "West Europe");
        assert_eq!(custom["hostName"], "192.0.2.10");
        let none = cluster(None, &FH2_SESSION);
        assert_eq!(none["secureDeviceAddress"], PLACEHOLDER_ADDRESS);
        assert_eq!(none["hostName"], PLACEHOLDER_HOST);
        assert_eq!(none["fulfillmentState"], "Fulfilled");
    }

    #[test]
    fn directory_answers_are_strings_and_one_qos_key() {
        fn all_strings(v: &Value) -> bool {
            match v {
                Value::Object(m) => m.values().all(all_strings),
                Value::Array(a) => a.iter().all(all_strings),
                Value::String(_) => true,
                _ => false,
            }
        }
        assert!(all_strings(&variants(&FH2_SESSION)));
        let q = qos_servers(&FH2_SESSION);
        assert_eq!(q.as_object().unwrap().len(), 1);
        assert_eq!(q["qosServers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn instants_have_three_fraction_digits() {
        assert_eq!(iso_ms(1_792_245_600_123), "2026-10-17T14:00:00.123Z");
        assert_eq!(now_iso().len(), 24);
    }

    #[test]
    fn merge_deletes_on_null_and_merges_objects() {
        let mut d = json!({"a": {"b": 1, "c": 2}, "x": 1})
            .as_object()
            .unwrap()
            .clone();
        merge(
            &mut d,
            json!({"a": {"b": null, "d": 4}, "x": {"y": 1}, "z": 3})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            Value::Object(d),
            json!({"a": {"c": 2, "d": 4}, "x": {"y": 1}, "z": 3})
        );
    }

    #[test]
    fn writes_keep_server_fields_and_only_me_is_a_member() {
        let now = "2026-10-18T14:00:00.000Z";
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), now);
        let body = json!({
            "servers": {"gameServerPlatform": {"evil": true}},
            "correlationId": "x", "membersInfo": null, "constants": null,
            "properties": {"custom": {"k": 1}},
            "members": {
                "me": {"constants": {"system": {"xuid": "1", "initialize": true}},
                       "properties": {"system": {"active": true}}},
                "reserve_1": {"constants": {"system": {"xuid": "99"}}}
            }
        });
        apply_write(&mut doc, body.as_object().unwrap().clone(), &me(), now);
        count_members(&mut doc);
        assert_eq!(doc["servers"], json!({}), "servers are the server's");
        assert_eq!(doc["correlationId"], Uuid::nil().to_string());
        assert_eq!(
            doc["constants"],
            json!({}),
            "a nulled object comes back empty"
        );
        assert_eq!(doc["properties"]["custom"]["k"], 1);
        let members = doc["members"].as_object().unwrap();
        assert_eq!(members.len(), 1);
        let m = &doc["members"]["0"];
        assert_eq!(
            m["constants"]["system"]["xuid"], "2535100000000001",
            "me is the caller"
        );
        assert_eq!(m["constants"]["system"]["initialize"], true);
        assert_eq!(m["gamertag"], "Driver#00001");
        assert_eq!(m["deviceToken"].as_str().unwrap().len(), 32);
        assert_eq!(doc["membersInfo"]["count"], 1);
        assert_eq!(doc["membersInfo"]["active"], 1);
        assert_eq!(doc["membersInfo"]["next"], 1);
        // Hostile shapes never panic.
        let hostile = json!({"members": {"me": {"constants": 5}}, "membersInfo": 3});
        apply_write(&mut doc, hostile.as_object().unwrap().clone(), &me(), now);
        assert_eq!(
            doc["members"]["0"]["constants"]["system"]["xuid"],
            "2535100000000001"
        );
        // `me: null` leaves.
        let leave = json!({"members": {"me": null}});
        apply_write(&mut doc, leave.as_object().unwrap().clone(), &me(), now);
        count_members(&mut doc);
        assert_eq!(doc["members"], json!({}));
        assert_eq!(doc["membersInfo"]["count"], 0);
    }

    #[test]
    fn a_host_less_session_shows_its_reader_first() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc["members"] = json!({
            "0": {"constants": {"system": {"xuid": "1", "index": 0}}, "properties": {"system": {"active": true}}},
            "1": {"constants": {"system": {"xuid": "2", "index": 1}}, "properties": {"system": {"active": false}}},
        });
        let mut seen = doc.clone();
        reader_first(&mut seen, 2);
        assert_eq!(seen["members"]["0"]["constants"]["system"]["xuid"], "2");
        assert_eq!(seen["members"]["0"]["constants"]["system"]["index"], 0);
        assert_eq!(seen["members"]["1"]["constants"]["system"]["xuid"], "1");
        assert_eq!(seen["members"]["1"]["constants"]["system"]["index"], 1);
        assert_eq!(
            seen["members"]["1"]["properties"]["system"]["active"], false,
            "others inactive"
        );
        assert_eq!(seen["membersInfo"]["active"], 0);
        assert!(seen["properties"]["system"]["host"].is_null());
        // the first member's own view is unchanged
        let mut first = doc.clone();
        reader_first(&mut first, 1);
        assert_eq!(first["members"], doc["members"]);
        // CR-S93: a host the owner named is no reason to keep the stored order any more (a private
        // car meet's joiner claimed the role in a loop): the reader is first and sees no host
        doc["properties"]["system"]["host"] = json!("H");
        reader_first(&mut doc, 2);
        assert_eq!(doc["members"]["0"]["constants"]["system"]["xuid"], "2");
        assert!(doc["properties"]["system"]["host"].is_null());
    }

    #[test]
    fn an_official_session_shows_each_reader_its_own_host_claim() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc[FI_OFFICIAL] = json!(true);
        doc[FI_HOST_CLAIMS] = json!({"2": "T2"});
        doc["properties"]["system"]["host"] = json!("T1");
        doc["members"] = json!({
            "0": {"constants": {"system": {"xuid": "1"}}, "properties": {"system": {"active": true}}},
            "1": {"constants": {"system": {"xuid": "2"}}, "properties": {"system": {"active": true}}},
        });
        let mut two = doc.clone();
        reader_first(&mut two, 2);
        assert_eq!(two["properties"]["system"]["host"], "T2");
        assert_eq!(two["members"]["0"]["constants"]["system"]["xuid"], "2");
        let mut one = doc.clone();
        reader_first(&mut one, 1);
        assert!(
            one["properties"]["system"]["host"].is_null(),
            "no claim of its own: no host"
        );
        strip_server_fields(&mut one);
        assert!(one.get(FI_OFFICIAL).is_none() && one.get(FI_HOST_CLAIMS).is_none());
    }

    #[test]
    fn the_readers_own_entry_keeps_no_address() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc["properties"]["system"]["host"] = json!("H");
        doc["members"] = json!({
            "0": {"constants": {"system": {"xuid": "1"}}, "properties": {"system": {"active": true}}},
            "1": {"constants": {"system": {"xuid": "2"}}, "properties": {"system": {"active": false}}},
        });
        set_server(
            &mut doc,
            server_entry(Some("192.0.2.7:5101".parse().unwrap()), LOCATION),
        );
        fill_member_addresses(&mut doc, Some(2));
        let route = secure_device_address(&"192.0.2.7:5101".parse().unwrap());
        assert_eq!(
            doc["members"]["0"]["properties"]["system"]["secureDeviceAddress"],
            route
        );
        assert!(doc["members"]["1"]["properties"]["system"]["secureDeviceAddress"].is_null());
    }

    #[test]
    fn a_session_without_a_host_keeps_its_member_addresses_empty() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc["members"] = json!({"0": {"properties": {"system": {"active": false}}}});
        set_server(
            &mut doc,
            server_entry(Some("192.0.2.7:5101".parse().unwrap()), LOCATION),
        );
        let before = doc.clone();
        fill_member_addresses(&mut doc, None);
        assert_eq!(doc, before);
    }

    #[test]
    fn members_without_an_address_show_the_readers_route() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc["members"] = json!({
            "0": {"properties": {"system": {"active": true, "secureDeviceAddress": ""}}},
            "1": {"properties": {"system": {"active": true, "secureDeviceAddress": "T1dO"}}},
            "2": {"properties": {"system": {"active": true}}},
        });
        doc["properties"]["system"]["host"] = json!("0");
        let before = doc.clone();
        fill_member_addresses(&mut doc, None);
        assert_eq!(doc, before, "no server entry, nothing to fill");
        set_server(
            &mut doc,
            server_entry(Some("192.0.2.7:5101".parse().unwrap()), LOCATION),
        );
        fill_member_addresses(&mut doc, None);
        let route = secure_device_address(&"192.0.2.7:5101".parse().unwrap());
        assert_eq!(
            doc["members"]["0"]["properties"]["system"]["secureDeviceAddress"],
            route
        );
        assert_eq!(
            doc["members"]["1"]["properties"]["system"]["secureDeviceAddress"],
            "T1dO"
        );
        assert_eq!(
            doc["members"]["2"]["properties"]["system"]["secureDeviceAddress"],
            route
        );
    }

    #[test]
    fn a_car_meet_ticket_session_is_known_and_loses_its_server() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        doc["constants"]["custom"]["gameServerPlatform"] = json!({"maxAllowedPlayers": 12});
        doc["constants"]["custom"]["hopperId"] = json!(3);
        assert!(is_car_meet_ticket_session("defaultTemplate_qos", &doc));
        assert!(!is_car_meet_ticket_session("defaultTemplate", &doc));
        doc["constants"]["custom"]["hopperId"] = json!("3");
        assert!(is_car_meet_ticket_session("DEFAULTTEMPLATE_QOS", &doc));
        doc["constants"]["custom"]["hopperId"] = json!(1);
        assert!(!is_car_meet_ticket_session("defaultTemplate_qos", &doc));
        set_server(&mut doc, server_entry(None, LOCATION));
        clear_server(&mut doc);
        assert!(doc["servers"].get("gameServerPlatform").is_none());
        let mut bare = json!({});
        clear_server(&mut bare);
        assert_eq!(bare, json!({}));
    }

    #[test]
    fn game_sessions_get_their_server_once() {
        let mut doc = new_session(&FH2_SESSION, Uuid::nil(), "t");
        assert_eq!(needs_server(&doc, &FH2_SESSION), None);
        doc["constants"]["custom"]["gameServerPlatform"] = json!({"maxAllowedPlayers": 12});
        assert_eq!(needs_server(&doc, &FH2_SESSION).as_deref(), Some(LOCATION));
        set_server(&mut doc, server_entry(None, LOCATION));
        assert_eq!(needs_server(&doc, &FH2_SESSION), None);
        let mut target = new_target_session(&FH2_SESSION, Uuid::nil(), "t");
        reserve(&mut target, &me(), "t");
        reserve(&mut target, &me(), "t");
        assert_eq!(target["membersInfo"]["count"], 1);
        assert_eq!(target["members"]["0"]["reserved"], true);
        assert_eq!(target["constants"]["system"]["maxMembersCount"], 12);
        let mut ticket = new_session(&FH2_SESSION, Uuid::nil(), "t");
        set_found(&mut ticket, &json!({"name": "n"}));
        assert_eq!(
            ticket["servers"]["matchmaking"]["properties"]["system"]["status"],
            "found"
        );
    }

    #[test]
    fn storable_counts_what_postgres_writes_and_refuses_nul() {
        let max = 128 << 10;
        assert_eq!(
            storable(&new_session(&FH2_SESSION, Uuid::nil(), "t"), max),
            Ok(())
        );
        // 900 copies of 1e308: 5 KB as a body, 280 KB as jsonb text.
        let big = json!({ "a": vec![json!(1e308); 900] });
        assert!(serde_json::to_vec(&big).unwrap().len() < 8 << 10);
        assert_eq!(storable(&big, max), Err(Unstorable::TooLarge));
        let tiny = json!({ "a": vec![json!(1e-300); 900] });
        assert_eq!(storable(&tiny, max), Err(Unstorable::TooLarge));
        assert_eq!(storable(&json!({ "a": vec![json!(12); 900] }), max), Ok(()));
        assert_eq!(
            storable(&json!({"a": "x\u{0}y"}), max),
            Err(Unstorable::Nul)
        );
        assert_eq!(storable(&json!({"\u{0}": 1}), max), Err(Unstorable::Nul));
        assert_eq!(
            storable(&json!({"a": "x".repeat(max)}), max),
            Err(Unstorable::TooLarge)
        );
    }

    #[test]
    fn game_tags_fit_fifteen_characters() {
        assert_eq!(game_tag(Some("Driver#00001"), 1), "Driver#00001");
        assert_eq!(
            game_tag(Some("Abcdefghijklmnopqrst#12345"), 1),
            "Abcdefghi#12345"
        );
        assert_eq!(game_tag(None, 2_535_100_000_001_234), "Driver 1234");
        let t = device_token(Uuid::now_v7(), Uuid::now_v7());
        assert!(
            t.len() == 32
                && t.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase())
        );
        assert_ne!(t, device_token(Uuid::now_v7(), Uuid::now_v7()));
    }
}
