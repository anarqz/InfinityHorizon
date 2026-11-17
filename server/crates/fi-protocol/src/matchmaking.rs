//! DTOs of `/mm/v1` (docs/contracts/MATCHMAKING_API.md; CR-L17, ADR 0017; worlds in the overlay,
//! OD-54 to OD-58, CR-S21): the world rows ([`World`], [`WorldView`]) and the filtered list,
//! hosting (settings, heartbeat), codes, the join answer (`granted | full | locked | not_allowed |
//! code | closed | rate_limited`; a world never queues, OD-58), seats and the relay grant,
//! invites and requests to join (OD-56, OD-59), the world's roster (OD-63), punches, host
//! moderation and the public capacity. Mirror:
//! `packages/contracts/src/matchmaking.ts` (parity test below). Routes: [`crate::routes::mm`].
//!
//! Network hints are private to the exact live world host and granted peer. They never prove
//! socket ownership; FIUP is required before game data (UDP_SOCKET_PROOF.md).

use serde::{Deserialize, Serialize};

/// `JoinAnswer.status` values.
pub const JOIN_STATUSES: [&str; 7] = [
    "granted",
    "full",
    "locked",
    "not_allowed",
    "code",
    "closed",
    "rate_limited",
];
/// `JoinAnswer.reason` values (`not_allowed`). `mods` was retired by OD-181 (any declared set
/// joins any world, MATCHMAKING_API.md §2.7).
pub const NOT_ALLOWED_REASONS: [&str; 6] =
    ["own_world", "mode", "tier", "host", "friends", "latency"];
/// `WorldView.why` values, in the order they are decided (`pool`: OD-149, the pineapple pool).
pub const WHY: [&str; 9] = [
    "own", "mode", "pool", "tier", "host", "friends", "code", "full", "latency",
];
/// Who may play in a world (OD-141, CR-S76): `WorldSettings.modes`, `WorldView.modes`. A host
/// chooses its own mode or `any` (Hardcore and Noob).
pub const WORLD_MODES: [&str; 3] = ["hardcore", "noob", "any"];
/// A world's audience (OD-57).
pub const AUDIENCES: [&str; 4] = ["everyone", "supporters", "friends", "private"];
/// The list's filters (OD-58: one is required; `all`, CR-L40: every mode, modded and vanilla).
pub const FILTERS: [&str; 4] = ["global", "near", "friends", "all"];
/// Estimated ping classes, best first (MATCHMAKING_API.md §3.5).
pub const PING_CLASSES: [&str; 5] = ["local", "near", "far", "remote", "unknown"];
/// A seat's end reasons (`SeatEnded.reason`; `pool`: OD-149, its holder was flagged in a clean
/// world).
pub const SEAT_ENDS: [&str; 5] = ["expired", "released", "kicked", "world_closed", "pool"];
/// The latency limits a host may choose (OD-59: under 60, 100 or 150 ms; absent: any).
pub const MAX_RTT_STEPS: [u32; 3] = [60, 100, 150];
/// Invites and requests to join (OD-56, OD-59).
pub const OFFER_KINDS: [&str; 2] = ["invite", "request"];
/// Their states.
pub const OFFER_STATES: [&str; 5] = ["pending", "accepted", "declined", "expired", "withdrawn"];
/// `WorldMember.role` values, in the roster's order (OD-63).
pub const MEMBER_ROLES: [&str; 3] = ["host", "seated", "reserved"];
/// `WorldMember.friend` values: the caller's account's relation to the member's (OD-55).
pub const MEMBER_FRIENDS: [&str; 5] = ["none", "friends", "outgoing", "incoming", "self"];
/// Largest world.
pub const MAX_PLAYERS: u32 = 12;
/// Seconds a world lives past its last announce.
pub const WORLD_TTL_SECS: i64 = 45;
/// Seconds a seat (and a relay grant) stays usable without a renew.
pub const GRANT_TTL_SECS: i64 = 60;
/// Worlds per filter answer (OD-58).
pub const LIST_CAP: usize = 5;
/// Locked Supporter worlds per filter answer for a free ticket (OD-58).
pub const LOCKED_CAP: usize = 3;
/// Worlds of a `filter=all` answer (CR-L40; `locked` stays empty).
pub const ALL_CAP: usize = 50;
/// Digits of a world code (OD-57); shown `XXXX-XXXX`.
pub const CODE_DIGITS: usize = 8;
/// Seconds a pending invite or request holds its seat (OD-59: about two minutes).
pub const INVITE_TTL_SECS: i64 = 120;
/// Pending invites and requests one world holds at once (OD-59).
pub const MAX_PENDING: u32 = 4;
/// Entries of a mod set (OD-112, MATCHMAKING_API.md §2.7).
pub const MODS_MAX: usize = 100;
/// Signed prefix of a relay grant.
pub const RELAY_GRANT_PREFIX: &[u8] = b"fi-relay-grant-v1\n";
/// The most car-state sync ids a grant's `sync` or a world's `hostSync` carries (CR-L53,
/// MATCHMAKING_API.md §5f).
pub const SYNC_MAX: usize = 8;

/// A world code as sent (8 digits, with an optional hyphen after the fourth) as its 8 digits;
/// `None` for anything else.
pub fn normalize_code(text: &str) -> Option<String> {
    let t = text.trim();
    let digits = match t.len() {
        8 => t.to_owned(),
        9 if t.as_bytes()[4] == b'-' => format!("{}{}", &t[..4], &t[5..]),
        _ => return None,
    };
    (digits.len() == CODE_DIGITS && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// The host of a world, as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldHost {
    /// XUID, decimal.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// `free` | `vip`.
    pub tier: String,
}

/// An official world's host (OD-188/189, MATCHMAKING_API.md §2.9): the Worlds host of
/// `FI_WORLDS`; no XUID, gamertag or tier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfficialWorldHost {
    /// The `FI_WORLDS` entry's `vmId`.
    pub official: String,
    /// Its display name (the `FI_WORLDS` `name`).
    pub name: String,
}

/// [`World::host`]: a player ([`WorldHost`], `{xuid, gamertag, tier}`, unchanged) or an official
/// Worlds host ([`OfficialWorldHost`], `{official, name}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorldHostView {
    /// A player-hosted world.
    Player(WorldHost),
    /// An official world.
    Official(OfficialWorldHost),
}

impl From<WorldHost> for WorldHostView {
    fn from(h: WorldHost) -> Self {
        Self::Player(h)
    }
}

impl WorldHostView {
    /// The player host's XUID text (`None`: official).
    pub fn xuid(&self) -> Option<&str> {
        match self {
            Self::Player(h) => Some(&h.xuid),
            Self::Official(_) => None,
        }
    }

    /// The name to show: the gamertag, or the Worlds host's name.
    pub fn name(&self) -> &str {
        match self {
            Self::Player(h) => &h.gamertag,
            Self::Official(h) => &h.name,
        }
    }
}

/// A world's common fields (never an IP address); [`WorldView`] adds what the caller may do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct World {
    /// Client-minted, stable for the world's life.
    pub id: String,
    /// `fh2`.
    pub game: String,
    /// Shown in the list.
    pub name: String,
    /// The host: a player, or an official Worlds host (§2.9).
    pub host: WorldHostView,
    /// The host's tier: `free` | `supporter` (who may join is the audience).
    pub kind: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Players in the world: the host (unless [`Self::dedicated`]) and the seated peers.
    pub players: u32,
    /// At most [`MAX_PLAYERS`]; a dedicated world's counts guests only (2 to 12).
    pub max_players: u32,
    /// The host's coarse region, when located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// One of [`PING_CLASSES`] (caller → host).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_class: Option<String>,
    /// The host's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Deprecated alias of [`WorldView::needs_code`].
    pub locked: bool,
    /// Races are on.
    pub races: bool,
    /// When the world opened.
    pub since: String,
}

/// A world as one caller sees it (MATCHMAKING_API.md §2.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldView {
    /// The common fields.
    #[serde(flatten)]
    pub world: World,
    /// One of [`AUDIENCES`].
    pub audience: String,
    /// One of [`WORLD_MODES`] (OD-141, CR-S76): the host's mode, or `any` when it is open to
    /// Hardcore and Noob. The server always sends one of the three (never empty; the one mapping,
    /// `fi_surfaces::worlds::view`, falls back to `mode`). Absent in a body from an older server:
    /// parsed as empty, to be read as the world's `mode`.
    #[serde(default)]
    pub modes: String,
    /// This caller must give the world's code.
    pub needs_code: bool,
    /// `maxPlayers - players - reserved and held seats`, at least 0.
    pub open_seats: u32,
    /// The estimated round trip caller → host (ms, rounded to 10); absent when unlocated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    /// The host's latency limit: under 60, 100 or 150 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rtt_ms: Option<u32>,
    /// A join now would be granted (with the code, when `needsCode`).
    pub can_join: bool,
    /// One of [`WHY`], only when `canJoin` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The host is a profile of one of the caller's friend accounts.
    pub friend_host: bool,
    /// The caller's account holds a live invite to this world.
    pub invited: bool,
    /// The world's code: only to its host, only for `friends` and `private` worlds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// A private official world (MATCHMAKING_API.md §2.10, CR-S96): the agent's cue that it is no
    /// spare, the client's that the seat is of the private official class. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
    /// The effective leader of a private official world, a decimal XUID (WORLDS_OFFICIAL.md
    /// §12.8b): only in `GET /mm/v1/seat`'s and a renew's world, to a reader seated in it; omitted
    /// otherwise, and while nobody leads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<String>,
    /// Hosted by a dedicated lobby (OD-109, MATCHMAKING_API.md §2.6): the host is not a player.
    /// Absent in an older body: `false`. (On this view, not on [`World`], so the frozen Rust
    /// launcher's struct literals of [`World`] stay valid; the wire shape is the same.)
    #[serde(default)]
    pub dedicated: bool,
    /// The world has a mod set (OD-112, MATCHMAKING_API.md §2.7). Absent in an older body: `false`.
    #[serde(default)]
    pub modded: bool,
    /// The host's mod set, sorted by id; empty when vanilla.
    #[serde(default)]
    pub mods: Vec<ModEntry>,
    /// Embedded on the list, one world and by-code only: host first unless dedicated, at most 12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<ListMember>>,
    /// One of [`TRANSPORTS`] (CR-S57, MATCHMAKING_API.md §5b): `relay` = born on a relay. Absent
    /// in an older body: `direct`.
    #[serde(default = "direct")]
    pub transport: String,
    /// The world's relay, only when `transport` is `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<WorldRelay>,
    /// The lobby engines its host's world server runs besides free roam ([`LOBBY_KINDS`]): a car
    /// meet or Road Trip goes only to a world that runs it.
    #[serde(default)]
    pub lobbies: Vec<String>,
    /// What it runs now ([`ACTIVITY_KINDS`], OD-150); an older server's body has none: free roam.
    #[serde(default = "freeroam")]
    pub activity: String,
    /// A pineapple world (OD-149, MATCHMAKING_API.md §3.9): its host was flagged by the anti-cheat
    /// when it opened. Only flagged profiles may join it. Absent in an older body: `false`.
    #[serde(default)]
    pub pool: bool,
    /// The lobby's voice setting ([`VOICE_MODES`], CR-S67, OD-154/OD-163); an older server's body
    /// has none: `open`.
    #[serde(default = "voice_open")]
    pub voice: String,
    /// The host's game is not in the world (it left free roam, exited or crashed) while the host's
    /// launcher keeps the world running for the other players (OD-180.2, S29, register CR-S85;
    /// MATCHMAKING_API.md §2.5, §3.3): the guests' launchers show "The host left the session" once
    /// per change. Omitted when false; absent in an older body: `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_away: bool,
    /// Hosted by a Worlds host of `FI_WORLDS`, not a player (OD-188/189, MATCHMAKING_API.md
    /// §2.9): its grants name `host: {official}`, its transport is direct. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub official: bool,
}

/// The world agent's signature header (MATCHMAKING_API.md §2.9, WORLDS_OFFICIAL.md §4.1):
/// `v1 <vmId> <unix seconds> <base64 Ed25519 signature>`.
pub const WORLD_AGENT_HEADER: &str = "FI-World-Agent";
/// The first line of the world agent's signed message (WORLDS_OFFICIAL.md §4.1).
pub const WORLD_AGENT_PREFIX: &[u8] = b"fi-world-agent-v1\n";
/// The agent's clock may be this far from the server's (seconds).
pub const WORLD_AGENT_SKEW_SECS: i64 = 30;

/// `GET /mm/v1/official/hello` → 200 (MATCHMAKING_API.md §2.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfficialHello {
    /// The server's clock, Unix milliseconds.
    pub server_time_ms: i64,
    /// The relay-grant signing ring's public half, current first.
    pub grant_verify_keys: Vec<OfficialGrantKey>,
    /// [`ACTIVITY_KINDS`].
    pub activities: Vec<String>,
}

/// One relay-grant verification key of [`OfficialHello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfficialGrantKey {
    /// The key id (the claims' `kid`).
    pub kid: String,
    /// Standard base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
}

/// `PUT /mm/v1/official/worlds/{worldId}` body (MATCHMAKING_API.md §2.9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OfficialWorldSettings {
    /// One of the server's games (`FI_GAMES`); fixed once open.
    pub game: String,
    /// `hardcore` | `noob`, or `any` (CR-S101, §2.11: a mixed world, both modes); fixed once open.
    pub mode: String,
    /// [`OFFICIAL_ACTIVITY_KINDS`]; absent: `freeroam`; fixed once open (OD-190).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Guests, 2-12 (the engine is not a player).
    pub max_players: u8,
    /// A pool world (CR-S103, WORLDS_OFFICIAL.md §14): only sessions under a live cheat flag
    /// (OD-149) are placed, invited or seated there, and a clean one never; absent: a clean world.
    /// Fixed once open. FH2 only (FM5 has no pool worlds).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pool: bool,
}

fn voice_open() -> String {
    "open".into()
}

/// A world's voice settings (CR-S67, MATCHMAKING_API.md §2.5): no voice, everyone at full level,
/// or each talker placed where their car is with a Doppler shift (OD-163; computed by the clients).
pub const VOICE_MODES: [&str; 3] = ["off", "open", "proximity"];

/// `pool` on [`WorldList`] and [`SeatAnswer`] (OD-149): the caller is isolated by the anti-cheat
/// system until `until`; absent for anyone else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallerPool {
    /// When the flag ends (RFC 3339 UTC, milliseconds).
    pub until: String,
}

fn freeroam() -> String {
    "freeroam".into()
}

/// `transport` values (CR-S57): a world's, a grant's, a seat's and a punch's.
pub const TRANSPORTS: [&str; 2] = ["direct", "relay"];
/// `nat` values a launcher reports (CR-S57, MATCHMAKING_API.md §5b).
pub const NAT_TYPES: [&str; 3] = ["open", "restricted", "symmetric"];
/// At most this many entries in `relays` (CR-S57).
pub const RELAYS_MAX: usize = 16;
/// The largest round trip a `relays` entry may report (ms).
pub const RELAY_RTT_MAX_MS: u32 = 5000;

fn direct() -> String {
    "direct".to_owned()
}

/// A relay as a world shows it (CR-S57): its identity and the operator's name for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldRelay {
    /// The relay's public key (beholder identity, standard base64 of 32 bytes).
    pub public_key: String,
    /// Its name in the server's `FI_RELAYS`.
    pub name: String,
}

/// A launcher's measured round trips to relays: public key → ms (CR-S57).
pub type RelayLatencies = std::collections::BTreeMap<String, u32>;

/// One mod of a world's set (OD-112, MATCHMAKING_API.md §2.7). The server validates a set
/// (`fi_db::matchmaking::mod_set`): at most [`MODS_MAX`], sorted by `id`, unique.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModEntry {
    /// The declared id: the ExtraOptions `mod.json` `"id"` the launcher sends
    /// (`^[a-z0-9][a-z0-9._-]{0,63}$`; MATCHMAKING_API.md §5f, CR-L53), the key of the car-state
    /// sync registry. Not the manifest's `compatibilityId`.
    pub id: String,
    /// Its revision as text (1 to 16 of `[0-9A-Za-z._+-]`; display only, part of the key).
    pub version: String,
    /// 64 lowercase hex digits.
    pub fingerprint: String,
}

/// A player embedded in a listed world (OD-112): the roster's order, no XUID and no relation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMember {
    /// The plain name (OD-120).
    pub gamertag: String,
    /// `free` | `vip`.
    pub tier: String,
    /// One of [`MEMBER_ROLES`].
    pub role: String,
    /// The member's declared mod set, sorted by id (OD-181, CR-S84): the host's row carries the
    /// world's set, a seat's row the set its join declared; `[]`: none. Absent (an older server's
    /// body): none.
    #[serde(default)]
    pub mods: Vec<ModEntry>,
}

/// `POST /mm/v1/worlds/{worldId}/requests` optional body (OD-112; absent: vanilla).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestToJoin {
    /// The asker's mod set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<ModEntry>>,
    /// The asker's measured round trips to the relays (§5b, OD-160): its acceptance can then seat
    /// it on a relay-born world's relay (without it, 409 `launcher_update_required`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<RelayLatencies>,
}

/// Where the caller is, as the list estimated it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListViewer {
    /// The caller has a location (behind the edge).
    pub located: bool,
    /// Its coarse region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// `GET /mm/v1/worlds?filter=` → 200 (OD-58: at most [`LIST_CAP`] plus [`LOCKED_CAP`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldList {
    /// The filter answered.
    pub filter: String,
    /// At most [`LIST_CAP`] worlds, in the filter's order.
    pub items: Vec<WorldView>,
    /// Free tickets: at most [`LOCKED_CAP`] Supporter-only worlds (`why: "tier"`); else empty.
    pub locked: Vec<WorldView>,
    /// When the list was read.
    pub generated_at: String,
    /// The caller's location state.
    pub viewer: ListViewer,
    /// The caller is in the pineapple pool (OD-149); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<CallerPool>,
}

/// The first version's list shape (`{items: World[]}`): a [`WorldList`] parses into it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worlds {
    /// Worlds.
    pub items: Vec<World>,
    /// Never sent since OD-58 (no paging).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// When the list was read.
    pub generated_at: String,
}

/// The host's relay port (the IP is the caller's address).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnnouncePort {
    /// UDP port, 1-65535.
    pub port: u16,
    /// Up to 4 private addresses of the host's PC, given only to joiners on the host's public
    /// address (MATCHMAKING_API.md §4a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan: Option<Vec<String>>,
    /// A measured/mapped external port; never an authentication proof (CR-S28).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<AnnounceExternalPort>,
}

/// How the host obtained its external UDP port routing hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnounceExternalSource {
    /// The host's actual UDP socket observation.
    Stun,
    /// The router's explicit port mapping.
    Upnp,
}

/// An external port distinct from the local bind port (CR-S28).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnnounceExternalPort {
    /// UDP port,1..65535, validated before persistence.
    pub port: u16,
    /// The source of this routing hint.
    pub source: AnnounceExternalSource,
}

/// Whether a private rendezvous destination uses its public or local bind port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkCandidateKind {
    /// The authenticated HTTP-observed address and selected external port.
    Public,
    /// A validated local address, disclosed only behind the same observed public IP.
    Lan,
}

/// A routing hint, NEVER a verified UDP endpoint or payload authorization.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkCandidate {
    /// Public or LAN port semantics.
    pub kind: NetworkCandidateKind,
    /// Canonical unicast IP, private to the authorized host/peer.
    pub ip: String,
    /// The candidate's own port,1..65535.
    pub port: u16,
}

impl std::fmt::Debug for NetworkCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkCandidate")
            .field("kind", &self.kind)
            .field("address", &"[REDACTED]")
            .finish()
    }
}

/// Fresh peer routing hints for its exact authenticated world host, never public metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkHints {
    /// Monotonically increasing per-grant observation revision,1..u32::MAX.
    pub revision: u32,
    /// At most1 public and4 eligible LAN destinations.
    pub candidates: Vec<NetworkCandidate>,
    /// Server clock at response generation, for conservative monotonic TTL conversion.
    pub generated_at: String,
    /// Original observation+30s, clipped by current world/grant authority.
    pub expires_at: String,
}

/// `POST /mm/v1/seat/network`: publish the actual native proxy socket's routing hints.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeatNetworkRequest {
    /// Caller's exact live grant.
    pub grant_id: String,
    /// Positive, increasing revision; identical retries do not refresh freshness.
    pub revision: u32,
    /// Local bind and optional observed/mapped external port; no public IP body field.
    pub endpoint: AnnouncePort,
}

impl std::fmt::Debug for SeatNetworkRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatNetworkRequest")
            .field("grant_id", &self.grant_id)
            .field("revision", &self.revision)
            .field("endpoint", &"[REDACTED]")
            .finish()
    }
}

/// `POST /mm/v1/seat/network` →200: the granted HOST's private routing destinations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatNetworkAnswer {
    /// Exact grant/context of the request.
    pub grant_id: String,
    /// Its current world.
    pub world_id: String,
    /// Exact accepted request revision.
    pub revision: u32,
    /// Host destinations, at most1 public and4 eligible LAN addresses.
    pub candidates: Vec<NetworkCandidate>,
    /// Server clock at generation (do not rely on a local wall clock).
    pub generated_at: String,
    /// Conservative deadline for these hints; an idempotent retry may already be expired.
    pub expires_at: String,
}

/// `PUT /mm/v1/worlds/{worldId}` body (MATCHMAKING_API.md §2.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldSettings {
    /// 1 to 48 characters.
    pub name: String,
    /// `hardcore` | `noob` (the ticket's).
    pub mode: String,
    /// One of [`WORLD_MODES`]: the host's mode or `any` (OD-141, CR-S76); absent: the host's mode
    /// when the world opens, unchanged on an open world. Fixed once the world opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<String>,
    /// 1 to [`MAX_PLAYERS`].
    pub max_players: u32,
    /// One of [`AUDIENCES`].
    pub audience: String,
    /// Under 60, 100 or 150 ms; absent or null: any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rtt_ms: Option<u32>,
    /// Races are on.
    pub races: bool,
    /// Up to 140 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Supporter hosts only: the XUIDs that may join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// The relay port.
    pub endpoint: AnnouncePort,
    /// The host's mod set (OD-112; absent or empty: vanilla); fixed once the world opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<ModEntry>>,
    /// The host's NAT test, one of [`NAT_TYPES`] (CR-S57); read when the world opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<String>,
    /// The host's measured relay round trips (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<RelayLatencies>,
    /// `relay`: the host's "Always connect through the relay" (OD-146): the world is born on
    /// a relay for every guest, fixed at open; absent: chosen as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// The lobby engines the host's world server runs besides free roam, from [`LOBBY_KINDS`]
    /// (absent: none, an older launcher); written when the world opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lobbies: Option<Vec<String>>,
    /// What the world runs now ([`ACTIVITY_KINDS`], OD-150), sent as soon as the host's game changes
    /// it; absent: unchanged (a new world: `freeroam`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The lobby's voice setting ([`VOICE_MODES`], CR-S67), sent when the host changes it; absent:
    /// unchanged (a new world: `open`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
}

/// `POST /mm/v1/worlds/{worldId}/announce` body (every 15 s).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Heartbeat {
    /// XUIDs in the world now (marks seats used, releases the seats of those who left).
    pub peers: Vec<String>,
    /// A changed relay port or LAN addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<AnnouncePort>,
    /// What the world runs now ([`ACTIVITY_KINDS`], OD-150), sent as soon as the host's game changes
    /// it; absent: unchanged (a new world: `freeroam`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The lobby's voice setting ([`VOICE_MODES`], CR-S67), sent when the host changes it; absent:
    /// unchanged (a new world: `open`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The host's game is not in the world (left free roam, exited or crashed) and the host's
    /// launcher holds the world for the other players (OD-180.2, S29, CR-S85; MATCHMAKING_API.md
    /// §2.5). Unlike `activity` and `voice`, absent means `false` on every heartbeat (an older
    /// launcher never leaves it stuck); omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_away: bool,
    /// Official worlds only (OD-194, register CR-S91): the world's activity is past the point
    /// newcomers may join (a Road Trip past its destination vote); the quick join places nobody
    /// new in it. Absent means `false` on every heartbeat; omitted when false; ignored from a
    /// player host.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_progress: bool,
    /// Official private worlds only (WORLDS_OFFICIAL.md §12.8b, CR-S96): the decimal XUID of the
    /// member the engine leads with. Absent: unchanged. Ignored elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<String>,
}

/// `POST /mm/v1/worlds` body: [`WorldSettings`] and [`Heartbeat`] in one call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Announce {
    /// Client-minted UUID, stable for the world's life.
    pub world_id: String,
    /// 1 to 48 characters.
    pub name: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// As [`WorldSettings::modes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<String>,
    /// 1 to [`MAX_PLAYERS`].
    pub max_players: u32,
    /// One of [`AUDIENCES`].
    pub audience: String,
    /// Under 60, 100 or 150 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rtt_ms: Option<u32>,
    /// Races are on.
    pub races: bool,
    /// Up to 140 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The relay port.
    pub endpoint: AnnouncePort,
    /// XUIDs in the world now.
    pub peers: Vec<String>,
    /// Supporter hosts only: the XUIDs that may join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// The host's mod set (OD-112; absent or empty: vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<ModEntry>>,
    /// The host's NAT test, one of [`NAT_TYPES`] (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<String>,
    /// The host's measured relay round trips (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<RelayLatencies>,
    /// `relay`: the host's "Always connect through the relay" (OD-146): the world is born on
    /// a relay for every guest, fixed at open; absent: chosen as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// The lobby engines the host's world server runs besides free roam, from [`LOBBY_KINDS`]
    /// (absent: none, an older launcher); written when the world opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lobbies: Option<Vec<String>>,
    /// What the world runs now ([`ACTIVITY_KINDS`], OD-150), sent as soon as the host's game changes
    /// it; absent: unchanged (a new world: `freeroam`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The lobby's voice setting ([`VOICE_MODES`], CR-S67), sent when the host changes it; absent:
    /// unchanged (a new world: `open`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The host's game is not in the world (left free roam, exited or crashed) and the host's
    /// launcher holds the world for the other players (OD-180.2, S29, CR-S85; MATCHMAKING_API.md
    /// §2.5). Unlike `activity` and `voice`, absent means `false` on every heartbeat (an older
    /// launcher never leaves it stuck); omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_away: bool,
}

/// The host's answer: the world as the host sees it (with its code) and the punches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldAnnounced {
    /// The world.
    #[serde(flatten)]
    pub world: WorldView,
    /// Same as `GET /mm/v1/punches`.
    pub punches: Vec<Punch>,
    /// The host's car-state sync ids (CR-L53, MATCHMAKING_API.md §5f): the registry ids of the
    /// world's declared `mods`, the rules of [`RelayGrantClaims::sync`]; the launcher passes them
    /// as `world.context` `world.hostSync` (DEDICATED_SERVER.md §4.3). Omitted when empty and
    /// always for an official world (no host player).
    #[serde(
        rename = "hostSync",
        default,
        skip_serializing_if = "sync_is_empty",
        deserialize_with = "lenient_sync"
    )]
    pub host_sync: Option<Vec<u8>>,
}

/// `POST /mm/v1/worlds/{worldId}/moderation` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Moderation {
    /// `kick` | `ban` | `unban`.
    pub action: String,
    /// The target.
    pub xuid: String,
    /// `ban` only: until when (absent: until unbanned).
    #[serde(default)]
    pub until: Option<String>,
    /// Up to 200 characters, kept with the ban.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /mm/v1/worlds/{worldId}/moderation` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationResult {
    /// The action applied.
    pub action: String,
    /// The target.
    pub xuid: String,
    /// The ban's end, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// `POST /mm/v1/join` body: a world id, a code, or both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JoinRequest {
    /// The world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_id: Option<String>,
    /// The world's code (8 digits, an optional hyphen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The caller's mod set (OD-112; absent: vanilla).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<Vec<ModEntry>>,
    /// The joiner's NAT test, one of [`NAT_TYPES`] (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<String>,
    /// The joiner's measured relay round trips (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<RelayLatencies>,
    /// `relay`: the joiner's "Always connect through the relay" (OD-146): the seat is born on a
    /// relay, or refused (`no_relay`, `relay_busy`), never silently direct; absent: as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// The activity the caller asked for ([`OFFICIAL_ACTIVITY_KINDS`], OD-150): a world running
    /// another is 409 `PORTAL.CONFLICT {reason: "activity"}`; absent: any. Without `worldId` and
    /// `code` it is the quick join's activity (OD-190, MATCHMAKING_API.md §2.9; absent: `freeroam`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Private create (MATCHMAKING_API.md §2.10, CR-S96): without `worldId` and `code`, the
    /// caller's own private official world of `activity`, promoted from a spare when it has none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
    /// The region the launcher chose (OD-205), one a `FI_WORLDS` entry declares. Absent: any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// The relay grant's claims (canonical JSON in this field order; Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayGrantClaims {
    /// 1.
    pub v: u32,
    /// Signing key id.
    pub kid: String,
    /// Grant id.
    pub grant: String,
    /// World id.
    pub world: String,
    /// Joiner's profile id.
    pub sub: String,
    /// Joiner's XUID, decimal.
    pub xuid: String,
    /// Joiner's tier (`free` | `vip`).
    pub tier: String,
    /// The world's host.
    pub host: RelayGrantHost,
    /// The side this copy is for: `joiner` (the join answer, the seat read) or `host` (the
    /// host's punches). The relay registers a side only with a grant of that role.
    pub role: String,
    /// Issued at.
    pub iat: i64,
    /// Expires at (`exp - iat` at most [`GRANT_TTL_SECS`]).
    pub exp: i64,
    /// The relay both sides meet at (CR-S57, MATCHMAKING_API.md §5b); the last claim, omitted
    /// when the grant names none (a direct-only grant's bytes are unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayGrantRelay>,
    /// The seat's car-state sync ids (CR-L53, MATCHMAKING_API.md §5f): the registry ids of the
    /// seat's declared mods, ascending, unique, at most [`SYNC_MAX`], each in 1..=255. After
    /// `relay`, before `voiceBlockedUntil`; omitted when `None` or empty (other grants' bytes are
    /// unchanged). Read leniently ([`sync_ids`]): any other shape is no sync (fail closed, never a
    /// refusal), but a duplicate JSON member inside it still refuses the grant.
    #[serde(
        default,
        skip_serializing_if = "sync_is_empty",
        deserialize_with = "lenient_sync"
    )]
    pub sync: Option<Vec<u8>>,
    /// The joiner account's voice block in force at signing, Unix seconds (CR-S67,
    /// MATCHMAKING_API.md §3.2): the later of a moderator voice ban and a community mute. The
    /// last claim, omitted when no block is in force (other grants' bytes are unchanged).
    #[serde(
        rename = "voiceBlockedUntil",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub voice_blocked_until: Option<i64>,
}

/// Whether a `sync` claim or `hostSync` list is absent (`None` or empty: no sync).
pub fn sync_is_empty(sync: &Option<Vec<u8>>) -> bool {
    sync.as_ref().is_none_or(Vec::is_empty)
}

/// The canonical form of a sync id list (CR-L53, MATCHMAKING_API.md §5f): a JSON array of at
/// most [`SYNC_MAX`] integers in 1..=255, strictly ascending. `None` for anything else, and for
/// the empty list (no sync).
pub fn sync_ids(value: &serde_json::Value) -> Option<Vec<u8>> {
    let items = value.as_array()?;
    if items.is_empty() || items.len() > SYNC_MAX {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(items.len());
    for item in items {
        let id = u8::try_from(item.as_u64()?).ok().filter(|id| *id != 0)?;
        if out.last().is_some_and(|last| *last >= id) {
            return None;
        }
        out.push(id);
    }
    Some(out)
}

/// A JSON value read with duplicate object members refused at any depth (MATCHMAKING_API.md
/// §3.2: a duplicate key anywhere in the claims refuses the grant).
struct NoDuplicates(serde_json::Value);

impl<'de> Deserialize<'de> for NoDuplicates {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NoDuplicatesVisitor)
    }
}

struct NoDuplicatesVisitor;

impl<'de> serde::de::Visitor<'de> for NoDuplicatesVisitor {
    type Value = NoDuplicates;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_bool<E>(self, v: bool) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::Bool(v)))
    }
    fn visit_i64<E>(self, v: i64) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::from(v)))
    }
    fn visit_u64<E>(self, v: u64) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::from(v)))
    }
    fn visit_f64<E>(self, v: f64) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::from(v)))
    }
    fn visit_str<E>(self, v: &str) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::String(v.to_owned())))
    }
    fn visit_unit<E>(self) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::Null))
    }
    fn visit_none<E>(self) -> Result<NoDuplicates, E> {
        Ok(NoDuplicates(serde_json::Value::Null))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<NoDuplicates, D::Error> {
        NoDuplicates::deserialize(d)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<NoDuplicates, A::Error> {
        let mut out = Vec::new();
        while let Some(NoDuplicates(v)) = seq.next_element()? {
            out.push(v);
        }
        Ok(NoDuplicates(serde_json::Value::Array(out)))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<NoDuplicates, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let NoDuplicates(v) = map.next_value()?;
            if out.insert(k, v).is_some() {
                return Err(serde::de::Error::custom("duplicate member"));
            }
        }
        Ok(NoDuplicates(serde_json::Value::Object(out)))
    }
}

/// `sync` as read by every verifier (CR-L53, MATCHMAKING_API.md §5f): the canonical list, else
/// `None` (no sync, never a refusal); a duplicate member inside the value refuses the grant.
fn lenient_sync<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
    let NoDuplicates(value) = NoDuplicates::deserialize(d)?;
    Ok(sync_ids(&value))
}

/// The relay part of [`RelayGrantClaims`] (CR-S57).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayGrantRelay {
    /// The relay's public key (standard base64 of 32 bytes).
    pub key: String,
    /// Its host: an IPv4 literal or a DNS name.
    pub addr: String,
    /// Its binding (STUN) UDP port.
    pub binding: u16,
    /// Its rendezvous UDP port.
    pub port: u16,
}

/// The host part of [`RelayGrantClaims`]: exactly one of `{xuid}` (a player-hosted world, the
/// bytes of every grant before OD-188) or `{official}` (an official world's Worlds host,
/// WORLDS_OFFICIAL.md §3, MATCHMAKING_API.md §2.9). Strict: both members, neither, an unknown or
/// a duplicate member, or a malformed `vmId` is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayGrantHost {
    /// Host's XUID, decimal (`{"xuid": "…"}`).
    Xuid(String),
    /// The `FI_WORLDS` entry's `vmId` (`{"official": "…"}`), [`is_vm_id`].
    Official(String),
}

impl RelayGrantHost {
    /// The player host's XUID text (`None`: an official world).
    pub fn xuid(&self) -> Option<&str> {
        match self {
            Self::Xuid(x) => Some(x),
            Self::Official(_) => None,
        }
    }

    /// The official host's `vmId` (`None`: a player-hosted world).
    pub fn official(&self) -> Option<&str> {
        match self {
            Self::Official(v) => Some(v),
            Self::Xuid(_) => None,
        }
    }
}

/// A Worlds host id (`FI_WORLDS` `vmId`): `[a-z0-9-]{1,32}`.
pub fn is_vm_id(text: &str) -> bool {
    (1..=32).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Serialize)]
struct RelayGrantHostXuid<'a> {
    xuid: &'a str,
}

#[derive(Serialize)]
struct RelayGrantHostOfficial<'a> {
    official: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayGrantHostWire {
    #[serde(default)]
    xuid: Option<String>,
    #[serde(default)]
    official: Option<String>,
}

impl Serialize for RelayGrantHost {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Xuid(xuid) => RelayGrantHostXuid { xuid }.serialize(s),
            Self::Official(official) => RelayGrantHostOfficial { official }.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for RelayGrantHost {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match RelayGrantHostWire::deserialize(d)? {
            RelayGrantHostWire {
                xuid: Some(x),
                official: None,
            } => Ok(Self::Xuid(x)),
            RelayGrantHostWire {
                xuid: None,
                official: Some(v),
            } if is_vm_id(&v) => Ok(Self::Official(v)),
            RelayGrantHostWire {
                xuid: None,
                official: Some(_),
            } => Err(D::Error::custom("host.official is not a vmId")),
            _ => Err(D::Error::custom(
                "host is exactly one of {xuid} or {official}",
            )),
        }
    }
}

/// `RelayGrant`: base64 of the canonical claims, and the base64 Ed25519 signature over
/// [`RELAY_GRANT_PREFIX`] ‖ claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayGrant {
    /// Standard base64 of the claims bytes.
    pub claims: String,
    /// Standard base64 of the 64-byte signature.
    pub signature: String,
    /// Secret HTTPS-only per-grant credential; never part of signed claims or UDP (CR-S26/27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_proof: Option<RelayUdpProof>,
}

/// Secret HTTPS-only socket-proof material; Debug intentionally redacts the key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayUdpProof {
    /// FIUP protocol version1.
    pub version: u32,
    /// Canonical padded standard base64 of32 CSPRNG bytes, scoped to this grant.
    pub key: String,
}

impl std::fmt::Debug for RelayUdpProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayUdpProof")
            .field("version", &self.version)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

/// A reserved seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    /// Grant id.
    pub id: String,
    /// The world.
    pub world_id: String,
    /// At most [`GRANT_TTL_SECS`] away (renewable up to `holdUntil`).
    pub expires_at: String,
    /// The latest a renew can keep the seat reserved.
    pub hold_until: String,
    /// What the hook presents to the relay.
    pub relay: RelayGrant,
    /// One of [`TRANSPORTS`] (CR-S57). Absent in an older body: `direct`.
    #[serde(default = "direct")]
    pub transport: String,
}

/// Seats of a full world.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldSeats {
    /// Players and reserved seats.
    pub players: u32,
    /// The limit.
    pub max_players: u32,
}

/// The live seat that locks the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Locked {
    /// Its world.
    pub world_id: String,
    /// The grant.
    pub grant_id: String,
    /// When it expires (a used seat: now).
    pub expires_at: String,
}

/// `not_allowed {reason: "latency"}`: the caller's estimate and the host's limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyRefusal {
    /// The caller's estimated round trip (ms).
    pub rtt_ms: u32,
    /// The host's limit (ms).
    pub max_rtt_ms: u32,
}

/// `POST /mm/v1/join` → 200 (429 with `status: rate_limited`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinAnswer {
    /// One of [`JOIN_STATUSES`].
    pub status: String,
    /// `granted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    /// `full`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<WorldSeats>,
    /// `locked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<Locked>,
    /// `not_allowed`: one of [`NOT_ALLOWED_REASONS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `not_allowed {reason: "latency"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyRefusal>,
    /// `rate_limited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

impl JoinAnswer {
    /// An answer with only a status.
    pub fn status(status: &str) -> Self {
        Self {
            status: status.to_owned(),
            grant: None,
            world: None,
            locked: None,
            reason: None,
            latency: None,
            retry_after_seconds: None,
        }
    }
}

/// The caller's live seat, or the world it hosts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seat {
    /// `reserved` | `seated` | `hosting`.
    pub state: String,
    /// The world.
    pub world_id: String,
    /// The world as the caller sees it.
    pub world: WorldView,
    /// Reserved and seated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// Reserved: when the seat lapses without a renew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Reserved: the latest a renew reaches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_until: Option<String>,
    /// Reserved and seated: the joiner's relay grant, fresh (at most [`GRANT_TTL_SECS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayGrant>,
    /// Reserved and seated: one of [`TRANSPORTS`] (CR-S57).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Reserved and seated in a private official world (§2.10): the game session linked to it,
    /// once its owner's game wrote it (the inbox's `session`; CR-S102). Absent elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::core::InviteSession>,
}

/// How the caller's last seat ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatEnded {
    /// Its world.
    pub world_id: String,
    /// The grant.
    pub grant_id: String,
    /// When.
    pub at: String,
    /// One of [`SEAT_ENDS`].
    pub reason: String,
}

/// The caller's latest request to join (OD-59), so the asker learns its answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatRequest {
    /// Request id.
    pub id: String,
    /// The world.
    pub world_id: String,
    /// One of [`OFFER_STATES`].
    pub state: String,
    /// When it lapses (pending).
    pub expires_at: String,
}

/// Optional `GET /mm/v1/seat` query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeatQuery {
    /// Include the authorized world's roster in the same snapshot.
    #[serde(default)]
    pub include_members: bool,
}

/// `GET /mm/v1/seat`, `POST /mm/v1/seat/renew` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeatAnswer {
    /// The live seat (`null`: none).
    pub seat: Option<Seat>,
    /// The last seat, when it ended within 5 minutes and none is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<SeatEnded>,
    /// The caller's latest request to join: pending, or answered within two minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<SeatRequest>,
    /// Opt-in roster of the live seat or hosted world; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<WorldMembers>,
    /// The caller is in the pineapple pool (OD-149); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<CallerPool>,
    /// The open private official world this account may rejoin (§2.10, after a crash); absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_world_id: Option<String>,
    /// When it was read.
    pub generated_at: String,
}

/// `POST /mm/v1/seat/renew` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeatRenew {
    /// The reserved seat.
    pub grant_id: String,
    /// One of [`TRANSPORTS`]: `relay`: the joiner's punch failed and it moved to the grant's relay
    /// (CR-S57); `direct`: a Hardcore world's joiner could not reach the world's relay and reached
    /// the host directly (OD-160; 409 `PORTAL.CONFLICT {reason: "no_direct"}` elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

/// `PUT /mm/v1/worlds/{worldId}/modes` body (OD-210, CR-S101, MATCHMAKING_API.md §2.11): who may
/// join a private official world from now on; its current leader only. → 200 `WorldView`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldJoinModes {
    /// One of [`WORLD_MODES`]: `any` (Everyone), `hardcore` or `noob`.
    pub modes: String,
}

/// `POST /mm/v1/worlds/{worldId}/invites` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InviteRequest {
    /// Any XUID of the friend account's profiles (a handle works).
    pub xuid: String,
}

/// An invite or a request to join: `POST /mm/v1/worlds/{worldId}/invites` and `…/requests` →
/// 201 (200: the pending one), `…/requests/{requestId}/accept` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    /// Id.
    pub id: String,
    /// One of [`OFFER_KINDS`].
    pub kind: String,
    /// The world.
    pub world_id: String,
    /// The handle (CORE_API.md §4.1) of the account that answers: the invitee, or the host.
    pub to: String,
    /// One of [`OFFER_STATES`].
    pub state: String,
    /// When it was made.
    pub created_at: String,
    /// When it lapses ([`INVITE_TTL_SECS`]).
    pub expires_at: String,
    /// `accepted`: the seat it gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
}

/// One player of a world as its roster shows it (OD-63; never an address, an account id or an
/// e-mail).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldMember {
    /// XUID, decimal (for the actions; the overlay never displays it, OD-61).
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// `free` | `vip`: the host's tier, or the seat's at the join.
    pub tier: String,
    /// One of [`MEMBER_ROLES`].
    pub role: String,
    /// One of [`MEMBER_FRIENDS`], relative to the caller's account.
    pub friend: String,
    /// The host: when the world opened; seated: when the host's announce first listed it;
    /// reserved: when the seat was issued.
    pub since: String,
    /// The member's account's voice block in force (CR-S67, MATCHMAKING_API.md §3.7): the later
    /// of a moderator voice ban and a community mute, else `null`. Absent in an older body: `null`.
    #[serde(default)]
    pub voice_blocked_until: Option<String>,
    /// The member's declared mod set, sorted by id (OD-181, CR-S84): the host's row carries the
    /// world's set, a seat's row the set its join declared; `[]`: none. Absent (an older server's
    /// body): none.
    #[serde(default)]
    pub mods: Vec<ModEntry>,
}

/// `GET /mm/v1/worlds/{worldId}/members` → 200: the host, then the seated peers by arrival, then
/// the reserved seats by issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldMembers {
    /// The world.
    pub world_id: String,
    /// Its players.
    pub items: Vec<WorldMember>,
    /// When it was read.
    pub generated_at: String,
}

/// The lobby engines a world server may run besides free roam (other projects used as reference).
pub const LOBBY_KINDS: [&str; 2] = ["carmeet", "roadtrip"];

/// What a world runs now (OD-150, MATCHMAKING_API.md §2.5).
pub const ACTIVITY_KINDS: [&str; 2] = ["freeroam", "roadtrip"];

/// What an official world runs for its life (OD-190, WORLDS_OFFICIAL.md §11.1): `carmeet` is
/// official-only; also the quick join's `activity` (MATCHMAKING_API.md §2.9). `race` is FM5's
/// (CR-S99); which of these a game's worlds run is `fi_domain::GameRules::official_activities`.
pub const OFFICIAL_ACTIVITY_KINDS: [&str; 4] = ["freeroam", "carmeet", "roadtrip", "race"];

/// The game-mode session kinds a car meet invite asks for (§3.8; Road Trip comes with its lobby).
pub const GAME_MODE_SESSION_KINDS: [&str; 1] = ["freeroam"];
/// The kinds an official car meet's session may ask for (OD-191; Showdown is not served).
pub const OFFICIAL_GAME_MODE_SESSION_KINDS: [&str; 2] = ["freeroam", "roadtrip"];
/// Most players one game-mode session takes (the game's lobby).
pub const GAME_MODE_SESSION_MAX_MEMBERS: usize = 12;

/// `POST /mm/v1/worlds/{worldId}/game-sessions` (host's launcher; MATCHMAKING_API.md §3.8, S9;
/// other projects used as reference): the session a car meet invite's joiners go to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameModeSessionRequest {
    /// One of [`GAME_MODE_SESSION_KINDS`].
    pub kind: String,
    /// The joiners, XUIDs in decimal: 1 to [`GAME_MODE_SESSION_MAX_MEMBERS`], distinct, each the host
    /// or seated in the world.
    pub xuids: Vec<String>,
}

/// → 201: the session every joiner is sent to (`MsgCarMeetSessionReady {name, templateName}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameModeSession {
    /// The request's kind.
    pub kind: String,
    /// The session's service config id (the game's).
    pub scid: String,
    /// Its template.
    pub template_name: String,
    /// Its name.
    pub name: String,
    /// Its members (the requested XUIDs, decimal), inactive until each game joins.
    pub members: Vec<String>,
    /// OD-191 (the official route only, MATCHMAKING_API.md §2.9): the group world the session
    /// opened; absent on the host route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<GameModeSessionWorld>,
}

/// The group world of an official car meet's session (OD-191, WORLDS_OFFICIAL.md §11.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameModeSessionWorld {
    /// The new official world (the agent opens it on the engine, then announces it).
    pub world_id: String,
    /// What it runs ([`OFFICIAL_GAME_MODE_SESSION_KINDS`]).
    pub activity: String,
}

/// One limit of [`WatchRules`]: over `max_kmh` for at least `sustain_ms`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchLimit {
    /// km/h.
    pub max_kmh: f64,
    /// Milliseconds over it.
    pub sustain_ms: i64,
}

/// [`WatchRules::speed`]: over `max_kmh` for at least `sustain_ms` when the observed car's limit is
/// unknown; the hook reports every episode held over `per_car_floor_kmh` and the server judges it
/// against the car's own limit (NR-5, §3.9 "The per-car speed limit").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchSpeed {
    /// The flat limit, km/h (a car the server cannot name).
    pub max_kmh: f64,
    /// Milliseconds held over the limit.
    pub sustain_ms: i64,
    /// The lowest per-car limit of any player's car, km/h (at most `max_kmh`): the hook's
    /// reporting threshold for speed.
    pub per_car_floor_kmh: f64,
}

/// [`WatchRules::jump`]: one sample-to-sample move past `max_meters` (log only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchJump {
    /// Metres.
    pub max_meters: f64,
}

/// `GET /mm/v1/watch` (§3.9, OD-147): what a hook measures the other players' cars against in a
/// Hardcore world. The blocked-car list stays on the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WatchRules {
    /// Changes with any value; an observation names the version it measured against.
    pub version: i64,
    /// Speed.
    pub speed: WatchSpeed,
    /// Upward speed.
    pub climb: WatchLimit,
    /// Position jumps (log only).
    pub jump: WatchJump,
}

/// The kinds of [`PeerObservation`] (§3.9): `jump` and `clock` are stored and never count.
pub const OBSERVATION_KINDS: [&str; 6] = ["speed", "climb", "car", "parts", "jump", "clock"];

/// `POST /mm/v1/worlds/{worldId}/observations` body (§3.9, OD-147): what a player's hook saw of
/// another player's car in a Hardcore world. 204; the id is the idempotency key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerObservation {
    /// Client-minted UUID.
    pub id: String,
    /// The observed player's XUID, decimal.
    pub xuid: String,
    /// One of [`OBSERVATION_KINDS`].
    pub kind: String,
    /// speed/climb: the peak km/h; jump: metres; car: the ordinal; clock: the clock ratio; parts:
    /// the offending part row id (the first in slot order).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// speed/climb: how long it stayed over the limit, ms (speed: over
    /// [`WatchSpeed::per_car_floor_kmh`] when `held_kmh` is sent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustain_ms: Option<i64>,
    /// speed: the observed car's ordinal (router-1 PlayerData `ModelId`), when the hook knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car_id: Option<i64>,
    /// speed: the highest speed held without a break for at least `speed.sustainMs`, km/h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_kmh: Option<f64>,
    /// The [`WatchRules::version`] measured against.
    pub rules_version: i64,
}

/// A peer's public endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerEndpoint {
    /// IP address.
    pub ip: String,
    /// UDP port.
    pub port: u16,
}

/// One grant the host's relay must allow (or drop: `kicked`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Punch {
    /// Grant id.
    pub grant_id: String,
    /// The joiner.
    pub xuid: String,
    /// The joiner's public endpoint, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<PeerEndpoint>,
    /// Fresh private peer routing hints; never a verified endpoint (CR-S26/27/28).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates: Option<NetworkHints>,
    /// The grant, for the host's relay to verify.
    pub relay: RelayGrant,
    /// Revoked by the host: drop the peer.
    pub kicked: bool,
    /// One of [`TRANSPORTS`] (CR-S57): `relay` = hold a relay leg for this grant. Absent in an
    /// older body: `direct`.
    #[serde(default = "direct")]
    pub transport: String,
}

/// `GET /mm/v1/punches` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Punches {
    /// The host's allow-list, exactly.
    pub items: Vec<Punch>,
}

/// The free limit's state (informational since OD-58: the gate decides).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeCapacity {
    /// Live free seats.
    pub online: u32,
    /// The configured limit.
    pub limit: u32,
    /// Always 0 (a world never queues).
    pub queued: u32,
    /// The configured switch.
    pub queue_open: bool,
}

/// Open worlds by their host's tier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldCounts {
    /// Free hosts' worlds.
    pub free: u32,
    /// Supporter hosts' worlds.
    pub supporter: u32,
}

/// `GET /mm/v1/capacity?game=` → 200 (public, cached 10 s).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capacity {
    /// The game.
    pub game: String,
    /// The free limit.
    pub free: FreeCapacity,
    /// Open worlds.
    pub worlds: WorldCounts,
    /// When it was read.
    pub generated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> RelayGrant {
        RelayGrant {
            claims: "e30=".into(),
            signature: "AA==".into(),
            udp_proof: Some(RelayUdpProof {
                version: 1,
                key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
            }),
        }
    }

    fn relay_ref() -> RelayGrantRelay {
        RelayGrantRelay {
            key: "k".into(),
            addr: "203.0.113.1".into(),
            binding: 3478,
            port: 5101,
        }
    }

    fn world() -> World {
        World {
            id: "w".into(),
            game: "fh2".into(),
            name: "n".into(),
            host: WorldHost {
                xuid: "1".into(),
                gamertag: "A#1".into(),
                tier: "vip".into(),
            }
            .into(),
            kind: "supporter".into(),
            mode: "noob".into(),
            players: 1,
            max_players: 12,
            region: Some("eu".into()),
            ping_class: Some("near".into()),
            message: Some("hi".into()),
            locked: false,
            races: true,
            since: "t".into(),
        }
    }

    fn view() -> WorldView {
        WorldView {
            world: world(),
            audience: "friends".into(),
            lobbies: vec!["roadtrip".into()],
            activity: "roadtrip".into(),
            modes: "any".into(),
            needs_code: false,
            open_seats: 11,
            rtt_ms: Some(40),
            max_rtt_ms: Some(100),
            can_join: false,
            why: Some("own".into()),
            friend_host: false,
            invited: false,
            code: Some("12345678".into()),
            private: true,
            leader: Some("2535100000000001".into()),
            dedicated: false,
            modded: true,
            mods: vec![mod_entry()],
            members: Some(vec![list_member()]),
            transport: "relay".into(),
            relay: Some(WorldRelay {
                public_key: "k".into(),
                name: "n".into(),
            }),
            pool: true,
            voice: "proximity".into(),
            host_away: true,
            official: true,
        }
    }

    fn mod_entry() -> ModEntry {
        ModEntry {
            id: "a.mod".into(),
            version: "1".into(),
            fingerprint: "a".repeat(64),
        }
    }

    fn list_member() -> ListMember {
        ListMember {
            gamertag: "A#00001".into(),
            tier: "vip".into(),
            role: "host".into(),
            mods: vec![mod_entry()],
        }
    }

    fn punch() -> Punch {
        Punch {
            grant_id: "g".into(),
            xuid: "1".into(),
            endpoint: Some(PeerEndpoint {
                ip: "192.0.2.1".into(),
                port: 1,
            }),
            candidates: Some(network_hints()),
            relay: relay(),
            kicked: false,
            transport: "relay".into(),
        }
    }

    fn member() -> WorldMember {
        WorldMember {
            xuid: "1".into(),
            gamertag: "A#00001".into(),
            tier: "free".into(),
            role: "seated".into(),
            friend: "outgoing".into(),
            since: "t".into(),
            voice_blocked_until: Some("t".into()),
            mods: vec![mod_entry()],
        }
    }

    fn network_hints() -> NetworkHints {
        NetworkHints {
            revision: 1,
            candidates: vec![NetworkCandidate {
                kind: NetworkCandidateKind::Public,
                ip: "192.0.2.1".into(),
                port: 40000,
            }],
            generated_at: "t".into(),
            expires_at: "t".into(),
        }
    }

    #[test]
    fn network_hints_are_strict_requests_and_redacted_diagnostics() {
        let value = SeatNetworkRequest {
            grant_id: "g".into(),
            revision: 1,
            endpoint: AnnouncePort {
                port: 5101,
                lan: Some(vec!["192.168.1.2".into()]),
                external: None,
            },
        };
        let debug = format!("{value:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("192.168.1.2"));
        assert!(!format!("{:?}", network_hints()).contains("192.0.2.1"));
        assert!(
            serde_json::from_str::<SeatNetworkRequest>(
                r#"{"grantId":"g","revision":1,"endpoint":{"port":5101}}"#
            )
            .is_ok()
        );
        for extra in [r#","ip":"192.0.2.1""#, r#","kind":"game""#] {
            let text =
                format!(r#"{{"grantId":"g","revision":1,"endpoint":{{"port":5101}}{extra}}}"#);
            assert!(serde_json::from_str::<SeatNetworkRequest>(&text).is_err());
        }
        assert!(
            serde_json::from_str::<NetworkCandidate>(
                r#"{"kind":"verified","ip":"192.0.2.1","port":5101}"#
            )
            .is_err()
        );
    }

    #[test]
    fn member_mods_absent_means_none() {
        // OD-181 (CR-S84): an older server's member rows carry no `mods`.
        let old: ListMember =
            serde_json::from_str(r#"{"gamertag":"A#00001","tier":"free","role":"seated"}"#)
                .unwrap();
        assert!(old.mods.is_empty());
        let old: WorldMember = serde_json::from_str(
            r#"{"xuid":"1","gamertag":"A#00001","tier":"free","role":"seated","friend":"none","since":"t"}"#,
        )
        .unwrap();
        assert!(old.mods.is_empty());
        // A member's set is on the wire as `mods: [{id, version, fingerprint}]`.
        let value = serde_json::to_value(list_member()).unwrap();
        assert_eq!(value["mods"][0]["id"], "a.mod");
        assert_eq!(value["mods"][0]["version"], "1");
        assert_eq!(value["mods"][0]["fingerprint"], "a".repeat(64));
        assert_eq!(
            serde_json::to_value(member()).unwrap()["mods"],
            value["mods"]
        );
    }

    #[test]
    fn udp_proof_secret_is_redacted_in_nested_debug() {
        let value = relay();
        let secret = &value.udp_proof.as_ref().unwrap().key;
        let debug = format!("{value:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(secret));
    }

    #[test]
    fn typed_external_ports_preserve_legacy_payloads_and_reject_unknown_sources() {
        let old: AnnouncePort = serde_json::from_str(r#"{"port":5101}"#).unwrap();
        assert_eq!(old.external, None);
        for source in ["stun", "upnp"] {
            let text =
                format!(r#"{{"port":5101,"external":{{"port":40000,"source":"{source}"}}}}"#);
            let value: AnnouncePort = serde_json::from_str(&text).unwrap();
            assert_eq!(
                serde_json::to_value(value).unwrap()["external"]["source"],
                source
            );
        }
        assert!(
            serde_json::from_str::<AnnouncePort>(
                r#"{"port":5101,"external":{"port":40000,"source":"guess"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn the_answers_are_flat_rows_and_requests_are_strict() {
        let json = serde_json::to_value(WorldAnnounced {
            world: view(),
            punches: vec![punch()],
            host_sync: None,
        })
        .unwrap();
        assert!(
            json.get("hostSync").is_none(),
            "omitted without sync (CR-L53)"
        );
        let synced = serde_json::to_value(WorldAnnounced {
            world: view(),
            punches: vec![],
            host_sync: Some(vec![1, 2]),
        })
        .unwrap();
        assert_eq!(synced["hostSync"], serde_json::json!([1, 2]));
        assert_eq!(
            serde_json::from_value::<WorldAnnounced>(synced)
                .unwrap()
                .host_sync,
            Some(vec![1, 2])
        );
        let empty = serde_json::to_value(WorldAnnounced {
            world: view(),
            punches: vec![],
            host_sync: Some(vec![]),
        })
        .unwrap();
        assert!(empty.get("hostSync").is_none(), "an empty set is omitted");
        assert_eq!(json["maxPlayers"], 12);
        assert_eq!(json["audience"], "friends");
        assert_eq!(json["code"], "12345678");
        assert_eq!(json["punches"][0]["grantId"], "g");
        assert!(
            serde_json::to_string(&view())
                .unwrap()
                .find("192.")
                .is_none(),
            "no IP in a world"
        );
        // The first version's list shape still parses a filtered list (the launcher's parser).
        let list = serde_json::to_value(WorldList {
            filter: "near".into(),
            items: vec![view()],
            locked: vec![],
            generated_at: "t".into(),
            viewer: ListViewer {
                located: false,
                region: None,
            },
            pool: None,
        })
        .unwrap();
        assert!(list.get("pool").is_none(), "absent for a clean caller");
        let old: Worlds = serde_json::from_value(list).unwrap();
        assert_eq!(old.items[0], world());
        assert!(serde_json::from_str::<JoinRequest>(r#"{"worldId":"w","password":"x"}"#).is_err());
        assert!(
            serde_json::from_value::<WorldSettings>(
                serde_json::json!({"name": "n", "mode": "noob",
                "maxPlayers": 2, "audience": "everyone", "races": true, "endpoint": {"port": 1},
                "visibility": "public"})
            )
            .is_err()
        );
        let bare = serde_json::to_value(JoinAnswer::status("full")).unwrap();
        assert_eq!(bare, serde_json::json!({"status": "full"}));
        let seat = serde_json::to_value(SeatAnswer {
            seat: None,
            ended: None,
            request: None,
            members: None,
            pool: None,
            private_world_id: None,
            generated_at: "t".into(),
        })
        .unwrap();
        assert_eq!(seat, serde_json::json!({"seat": null, "generatedAt": "t"}));
        assert_eq!(
            serde_json::from_value::<SeatAnswer>(seat).unwrap().members,
            None
        );
        assert_eq!(
            serde_json::from_value::<SeatQuery>(serde_json::json!({})).unwrap(),
            SeatQuery::default()
        );
        let claims = RelayGrantClaims {
            v: 1,
            kid: "k".into(),
            grant: "g".into(),
            world: "w".into(),
            sub: "p".into(),
            xuid: "1".into(),
            tier: "free".into(),
            host: RelayGrantHost::Xuid("2".into()),
            role: "joiner".into(),
            iat: 1,
            exp: 2,
            relay: None,
            sync: None,
            voice_blocked_until: None,
        };
        assert_eq!(
            serde_json::to_string(&claims).unwrap(),
            r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2}"#,
            "canonical field order; a grant without a relay is unchanged (CR-S57)"
        );
        let with_relay = RelayGrantClaims {
            relay: Some(relay_ref()),
            ..claims
        };
        assert_eq!(
            serde_json::to_string(&with_relay).unwrap(),
            r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2,"relay":{"key":"k","addr":"203.0.113.1","binding":3478,"port":5101}}"#,
            "the relay is the last claim (CR-S57)"
        );
        let blocked = RelayGrantClaims {
            voice_blocked_until: Some(1_792_256_400),
            ..with_relay
        };
        assert_eq!(
            serde_json::to_string(&blocked).unwrap(),
            r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2,"relay":{"key":"k","addr":"203.0.113.1","binding":3478,"port":5101},"voiceBlockedUntil":1792256400}"#,
            "a voice block is the last claim, after the relay (CR-S67)"
        );
        let blocked_direct = RelayGrantClaims {
            relay: None,
            ..blocked
        };
        assert_eq!(
            serde_json::to_string(&blocked_direct).unwrap(),
            r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2,"voiceBlockedUntil":1792256400}"#
        );
        assert_eq!(
            serde_json::from_str::<RelayGrantClaims>(
                r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2,"voiceBlockedUntil":5}"#
            )
            .unwrap()
            .voice_blocked_until,
            Some(5)
        );
    }

    fn sync_claims() -> RelayGrantClaims {
        RelayGrantClaims {
            v: 1,
            kid: "k".into(),
            grant: "g".into(),
            world: "w".into(),
            sub: "p".into(),
            xuid: "1".into(),
            tier: "free".into(),
            host: RelayGrantHost::Xuid("2".into()),
            role: "joiner".into(),
            iat: 1,
            exp: 2,
            relay: None,
            sync: None,
            voice_blocked_until: None,
        }
    }

    const BASE: &str = r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2"},"role":"joiner","iat":1,"exp":2"#;

    /// CR-L53 (MATCHMAKING_API.md §3.2, §5f): `sync` sits after `relay` and before
    /// `voiceBlockedUntil`; absent or empty, the bytes are exactly today's.
    #[test]
    fn the_sync_claim_is_between_relay_and_the_voice_block_and_omitted_when_empty() {
        let plain = sync_claims();
        let today = format!("{BASE}}}");
        assert_eq!(serde_json::to_string(&plain).unwrap(), today);
        let empty = RelayGrantClaims {
            sync: Some(vec![]),
            ..plain.clone()
        };
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            today,
            "an empty set keeps the bytes unchanged"
        );
        let synced = RelayGrantClaims {
            sync: Some(vec![1, 2]),
            ..plain.clone()
        };
        assert_eq!(
            serde_json::to_string(&synced).unwrap(),
            format!(r#"{BASE},"sync":[1,2]}}"#)
        );
        let all = RelayGrantClaims {
            relay: Some(relay_ref()),
            sync: Some(vec![1]),
            voice_blocked_until: Some(9),
            ..plain.clone()
        };
        assert_eq!(
            serde_json::to_string(&all).unwrap(),
            format!(
                r#"{BASE},"relay":{{"key":"k","addr":"203.0.113.1","binding":3478,"port":5101}},"sync":[1],"voiceBlockedUntil":9}}"#
            )
        );
        let blocked = RelayGrantClaims {
            sync: Some(vec![2]),
            voice_blocked_until: Some(9),
            ..plain
        };
        assert_eq!(
            serde_json::to_string(&blocked).unwrap(),
            format!(r#"{BASE},"sync":[2],"voiceBlockedUntil":9}}"#)
        );
        for claims in [synced, all, blocked] {
            let text = serde_json::to_string(&claims).unwrap();
            assert_eq!(
                serde_json::from_str::<RelayGrantClaims>(&text).unwrap(),
                claims
            );
        }
    }

    /// CR-L53 (§5f): any other shape of `sync` reads as no sync (fail closed, never a refusal),
    /// as WINDOWS' `core/grant.rs` and `RelayGrantVerifier` read it.
    #[test]
    fn a_malformed_sync_reads_as_no_sync() {
        let read = |sync: &str| {
            serde_json::from_str::<RelayGrantClaims>(&format!(r#"{BASE},"sync":{sync}}}"#))
                .unwrap()
                .sync
        };
        assert_eq!(
            read("[1,2,3,4,5,6,7,8]"),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 8])
        );
        assert_eq!(read("[255]"), Some(vec![255]));
        for bad in [
            "[]",
            "null",
            "[0]",
            "[256]",
            "[-1]",
            "[1.5]",
            "[2,1]",
            "[1,1]",
            "[1,2,3,4,5,6,7,8,9]",
            r#"["1"]"#,
            "1",
            r#"{"a":1}"#,
            "[[1]]",
        ] {
            assert_eq!(read(bad), None, "{bad}");
        }
    }

    /// MATCHMAKING_API.md §3.2 (CR-L53): a duplicate key anywhere in the claims refuses the whole
    /// grant.
    #[test]
    fn a_duplicate_key_anywhere_refuses_the_claims() {
        let relay = r#""relay":{"key":"k","addr":"203.0.113.1","binding":3478,"port":5101}"#;
        assert!(serde_json::from_str::<RelayGrantClaims>(&format!("{BASE},{relay}}}")).is_ok());
        for bad in [
            format!(r#"{BASE},"kid":"k"}}"#),
            format!(r#"{BASE},"sync":[1],"sync":[1]}}"#),
            format!(r#"{BASE},"sync":[1],"sync":"x"}}"#),
            format!(r#"{BASE},"sync":{{"a":1,"a":1}}}}"#),
            format!(r#"{BASE},"sync":[{{"a":1,"a":2}}]}}"#),
            format!(r#"{BASE},"voiceBlockedUntil":1,"voiceBlockedUntil":1}}"#),
            r#"{"v":1,"kid":"k","grant":"g","world":"w","sub":"p","xuid":"1","tier":"free","host":{"xuid":"2","xuid":"2"},"role":"joiner","iat":1,"exp":2}"#.to_owned(),
            format!(
                r#"{BASE},"relay":{{"key":"k","key":"k","addr":"203.0.113.1","binding":3478,"port":5101}}}}"#
            ),
        ] {
            assert!(
                serde_json::from_str::<RelayGrantClaims>(&bad).is_err(),
                "{bad}"
            );
        }
    }

    /// OD-188/189 (MATCHMAKING_API.md §2.9): `World.host` is a player's `{xuid, gamertag, tier}`
    /// (unchanged) or an official Worlds host's `{official, name}`.
    #[test]
    fn a_world_host_is_a_player_or_an_official_worlds_host() {
        let player: WorldHostView =
            serde_json::from_str(r#"{"xuid":"1","gamertag":"A#1","tier":"vip"}"#).unwrap();
        assert_eq!((player.xuid(), player.name()), (Some("1"), "A#1"));
        assert_eq!(
            serde_json::to_string(&player).unwrap(),
            r#"{"xuid":"1","gamertag":"A#1","tier":"vip"}"#
        );
        let official = WorldHostView::Official(OfficialWorldHost {
            official: "local-mac".into(),
            name: "Local".into(),
        });
        let text = serde_json::to_string(&official).unwrap();
        assert_eq!(text, r#"{"official":"local-mac","name":"Local"}"#);
        assert_eq!(
            serde_json::from_str::<WorldHostView>(&text).unwrap(),
            official
        );
        assert_eq!((official.xuid(), official.name()), (None, "Local"));
        for bad in [
            r#"{}"#,
            r#"{"official":"a"}"#,
            r#"{"official":"a","name":"n","x":1}"#,
        ] {
            assert!(serde_json::from_str::<WorldHostView>(bad).is_err(), "{bad}");
        }
    }

    /// OD-188/189 (MATCHMAKING_API.md §3.2): the host claim is exactly one of `{xuid}` or
    /// `{official}`; a player grant's bytes are pinned unchanged.
    #[test]
    fn the_host_claim_is_exactly_one_of_xuid_or_official() {
        let player = RelayGrantClaims {
            v: 1,
            kid: "rg-1".into(),
            grant: "g".into(),
            world: "w".into(),
            sub: "p".into(),
            xuid: "2535100000000003".into(),
            tier: "vip".into(),
            host: RelayGrantHost::Xuid("2535100000000004".into()),
            role: "host".into(),
            iat: 1_793_750_400,
            exp: 1_793_750_460,
            relay: None,
            sync: None,
            voice_blocked_until: None,
        };
        let bytes = r#"{"v":1,"kid":"rg-1","grant":"g","world":"w","sub":"p","xuid":"2535100000000003","tier":"vip","host":{"xuid":"2535100000000004"},"role":"host","iat":1793750400,"exp":1793750460}"#;
        assert_eq!(serde_json::to_string(&player).unwrap(), bytes);
        assert_eq!(
            serde_json::from_str::<RelayGrantClaims>(bytes).unwrap(),
            player
        );
        let official = RelayGrantClaims {
            host: RelayGrantHost::Official("local-mac".into()),
            role: "joiner".into(),
            ..player.clone()
        };
        let text = serde_json::to_string(&official).unwrap();
        assert_eq!(
            text,
            r#"{"v":1,"kid":"rg-1","grant":"g","world":"w","sub":"p","xuid":"2535100000000003","tier":"vip","host":{"official":"local-mac"},"role":"joiner","iat":1793750400,"exp":1793750460}"#
        );
        assert_eq!(
            serde_json::from_str::<RelayGrantClaims>(&text).unwrap(),
            official
        );
        assert_eq!(official.host.official(), Some("local-mac"));
        assert_eq!(official.host.xuid(), None);
        assert_eq!(player.host.xuid(), Some("2535100000000004"));
        for bad in [
            r#"{}"#,
            r#"{"xuid":"2","official":"local-mac"}"#,
            r#"{"official":"local-mac","xuid":"2"}"#,
            r#"{"xuid":"2","xuid":"3"}"#,
            r#"{"official":"a","official":"b"}"#,
            r#"{"xuid":"2","other":1}"#,
            r#"{"official":"Local-Mac"}"#,
            r#"{"official":""}"#,
            r#"{"official":"abcdefghijklmnopqrstuvwxyz0123456"}"#,
            r#"{"official":"a_b"}"#,
            r#"{"xuid":null}"#,
            r#"{"official":null}"#,
            r#"{"xuid":2}"#,
            r#""2""#,
        ] {
            assert!(
                serde_json::from_str::<RelayGrantHost>(bad).is_err(),
                "{bad}"
            );
        }
        assert!(is_vm_id("abcdefghijklmnopqrstuvwxyz012345"));
    }

    #[test]
    fn codes_are_eight_digits_with_an_optional_hyphen() {
        assert_eq!(normalize_code("12345678").as_deref(), Some("12345678"));
        assert_eq!(normalize_code(" 1234-5678 ").as_deref(), Some("12345678"));
        for bad in [
            "",
            "1234567",
            "123456789",
            "1234_5678",
            "12-345678",
            "abcdefgh",
            "1234-567a",
            "１２３４５６７８",
        ] {
            assert_eq!(normalize_code(bad), None, "{bad:?}");
        }
    }

    /// OD-160: a renew names either transport; `direct` is a Hardcore joiner's fallback.
    #[test]
    fn a_seat_renew_names_either_transport() {
        for t in TRANSPORTS {
            let body = format!(r#"{{"grantId":"g","transport":"{t}"}}"#);
            let renew: SeatRenew = serde_json::from_str(&body).unwrap();
            assert_eq!(renew.transport.as_deref(), Some(t));
            assert_eq!(serde_json::to_string(&renew).unwrap(), body);
        }
        let plain: SeatRenew = serde_json::from_str(r#"{"grantId":"g"}"#).unwrap();
        assert_eq!(plain.transport, None);
        assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"grantId":"g"}"#);
    }
}
