//! Account, profile and token bodies (ACCOUNTS.md §2, §5).

use serde::{Deserialize, Serialize};

/// `GET /launcher/v1/info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Contract version (1).
    pub api_version: u32,
    /// Independent new-account control (OD-100); legacy answers default to enabled.
    #[serde(default = "default_registration_enabled")]
    pub registration_enabled: bool,
    /// Stable id of this deployment (`forzainfinity-prod`); launcher state is keyed by it, never
    /// by the API hostname (ACCOUNTS.md §5, CR-L1).
    pub realm: String,
    /// Older launchers must update.
    pub min_launcher: String,
    /// Oldest hook build allowed online; also the first build that speaks the launch protocol
    /// (ACCOUNTS.md §5, CR-L12). Older hooks are refused with `GAME.UPDATE_REQUIRED`.
    pub min_hook: String,
    /// Newest launcher release.
    pub latest_launcher: String,
    /// Newest hook release.
    pub latest_hook: String,
    /// SHA-256 of every allowed `gamedbRC.slt`.
    pub allowed_car_db: Vec<String>,
    /// Checkpoint commits while playing (SAVES_API.md §4.4, CR-L2).
    pub checkpoints: CheckpointPolicy,
    /// Message of the day.
    pub motd: Option<String>,
    /// The origin of each platform v1 surface (CORE_API.md §2.1); empty strings before a
    /// deployment names them.
    #[serde(default)]
    pub services: ServiceMap,
    /// The games this deployment serves and their switches (ADR 0902).
    #[serde(default)]
    pub games: Vec<GameInfo>,
    /// The Identity Platform web configuration of the launcher's native sign-in (ACCOUNTS.md §5,
    /// OD-43). Always sent; `default` only lets an answer of an older server parse.
    #[serde(default)]
    pub idp: IdpConfig,
    /// The Ed25519 public keys of this instance's launch tickets and relay grants (ACCOUNTS.md §5,
    /// CR-S49): only when the instance sets `FI_PUBLISH_TRUST_KEYS=true` (a custom server, CR-L39),
    /// so the official wire is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustKeys>,
    /// Which profiles keep a cloud save line on this server (ACCOUNTS.md §5, CR-S51): explanatory
    /// only (admission decides). Absent: a server before CR-S51, whose rule is OD-52's (only a
    /// Supporter's Hardcore profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saves: Option<SavesPolicy>,
}

/// `ServerInfo.saves` (ACCOUNTS.md §5, CR-S51). OD-239 (CR-S111) removed `cloud` (the per-server
/// `FI_CLOUD_SAVES` list): every Hardcore profile keeps a line, no Noob one; `Profile.cloudLine`
/// says it per profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavesPolicy {
    /// `FI_HARDCORE_FROM_PROFILE` (CR-S52, SAVES_API.md §4.10): a Hardcore line at revision 0 may
    /// start from another of the account's profiles' save, checked on the spot (`POST .../save/
    /// check`, then a `seed_from_profile` commit). Absent (a server before CR-S52): `false`.
    #[serde(default)]
    pub hardcore_from_profile: bool,
}

/// `ServerInfo.trust` (ACCOUNTS.md §5, CR-S49): `kid:hex` entries, the raw 32-byte Ed25519 public
/// key in 64 lowercase hex characters, current first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustKeys {
    /// The keys that verify this instance's launch tickets.
    pub launch_ticket: Vec<String>,
    /// The keys that verify this instance's relay grants (empty without worlds).
    pub relay_grant: Vec<String>,
}

fn default_registration_enabled() -> bool {
    true
}

/// `ServerInfo.idp` (ACCOUNTS.md §4.1, §5; OD-43): what the launcher needs to call Identity
/// Platform's REST API itself. Public by design; nothing here is a secret.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdpConfig {
    /// `firebase` or `local` (ACCOUNTS.md §4.1 "Two identity backends"; absent: `firebase`).
    /// Omitted for `firebase`, so the production wire stays what shipped launchers parse.
    #[serde(default, skip_serializing_if = "IdpKind::is_firebase")]
    pub kind: IdpKind,
    /// The public Web API key (`?key=` on every Identity Toolkit call); `firebase` only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// The Identity Platform project (`aud` of its ID tokens); `firebase` only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_id: String,
    /// Locally only: the Auth emulator's origin as devices reach it (`http://127.0.0.1:9399`);
    /// absent in prod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator_url: Option<String>,
}

/// The sign-in backend of an instance (ACCOUNTS.md §4.1 "Two identity backends", OD-122).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdpKind {
    /// Identity Platform / Firebase Authentication (every launcher before CR-L38 assumes it).
    #[default]
    Firebase,
    /// The instance's built-in e-mail and password provider (`/launcher/v1/auth/sign-*`).
    Local,
}

impl IdpKind {
    /// Whether this is the default, omitted on the wire.
    pub fn is_firebase(&self) -> bool {
        *self == IdpKind::Firebase
    }
}

/// The service map of `GET /launcher/v1/info` (CORE_API.md §2.1): no client hard-codes an origin.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceMap {
    /// `https://api.<domain>`: identity, `/me`, `/core/v1`, `/portal/v1`, `/mcp`.
    pub core: String,
    /// `https://gate.<domain>`: admission, heartbeat, leave, queue.
    pub gate: String,
    /// `https://saves.<domain>`: the save line and the in-game universe.
    pub saves: String,
    /// `https://mm.<domain>`: worlds, joins, grants.
    pub matchmaking: String,
}

/// A game of `GET /launcher/v1/info` (CORE_API.md §2.1, ADR 0902).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameInfo {
    /// `fh2`.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Oldest hook build allowed online for this game.
    pub min_hook: String,
    /// Oldest launcher allowed for this game.
    pub min_launcher: String,
    /// Takes players now (false while in maintenance or switched off).
    pub accepting: bool,
    /// End of the maintenance in force.
    pub maintenance_until: Option<String>,
}

/// Checkpoint policy of `GET /launcher/v1/info` (SAVES_API.md §4.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointPolicy {
    /// `false`: commit only before launch and after exit.
    pub enabled: bool,
    /// Smallest gap between two checkpoint commits (300).
    pub min_interval_secs: u32,
    /// Commits per session hour the server accepts (30, `SAVES.RATE_LIMITED` beyond).
    pub max_commits_per_hour: u32,
}

/// A launcher install as it introduces itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Device {
    /// Install id (UUIDv7 made on first run).
    pub device_id: String,
    /// PC name.
    pub label: String,
    /// `windows`, `proton`, `linux`.
    pub platform: String,
    /// Same-device tracking value (OD-120, CR-S44): 43 base64url characters, write-only (never
    /// returned), kept only as an HMAC on the device binding: base64url SHA-256 of
    /// `forzainfinity-device-v1:` + lower(MachineGuid) (OD-120 follow-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// A bound device as `/me` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundDevice {
    /// Install id.
    pub device_id: String,
    /// PC name.
    pub label: String,
    /// Platform.
    pub platform: String,
    /// When bound.
    pub bound_at: String,
    /// Last seen.
    pub last_seen_at: String,
}

/// `GET /launcher/v1/profiles/{xuid}/presence` → 200 (ACCOUNTS.md §5 "Presence", CR-S72, OD-136):
/// what the launcher's Discord presence shows of the caller's own profile. Every field is absent
/// when unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePresence {
    /// The profile's Car Club of its current mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club: Option<PresenceClub>,
    /// Driver level, from the last accepted profile sync of its mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    /// Spendable credits, from the same sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<i64>,
    /// When that sync was accepted (RFC 3339); absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<String>,
    /// With `?car=`: the absolute https URL of the profile's newest servable shared livery
    /// preview of that car (ACCOUNTS.md §5a, CR-S77); absent without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub livery_image: Option<String>,
}

/// A club in [`ProfilePresence`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceClub {
    /// 2 to 4 of `A-Z0-9`.
    pub tag: String,
    /// 1 to 24 characters.
    pub name: String,
}

/// A game profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// Its id.
    pub profile_id: String,
    /// `fh2`.
    pub game: String,
    /// Decimal XUID.
    pub xuid: String,
    /// The plain `Name` (OD-120; `Name#12345` before the backfill).
    pub gamertag: String,
    /// Equal to `gamertag` since OD-120 (`Name-12345` before), kept for wire compatibility.
    pub handle: String,
    /// `hardcore`, `noob`.
    pub mode: String,
    /// `active`, `deletion_pending`.
    pub status: String,
    /// When created.
    pub created_at: String,
    /// Last session end.
    pub last_played_at: Option<String>,
    /// Set while the profile was renamed by the OD-120 clash backfill and its free rename is
    /// unused (ACCOUNTS.md §5 "Plain names and renames"); absent or null otherwise.
    #[serde(default)]
    pub rename_notice: Option<RenameNotice>,
    /// CR-S51: whether this profile keeps a cloud save line on this server now (its tier and
    /// mode in the server's `FI_CLOUD_SAVES`, read at the time of the answer). Explanatory: the
    /// admission decides (its `lease` and `save`). Absent: a server before CR-S51 (OD-52's rule,
    /// a Supporter's Hardcore profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_line: Option<bool>,
    /// CR-S51: whether its next admission needs a content scan (OD-113, CONTENT_API.md §3): a
    /// Hardcore profile that keeps a cloud line. The launcher scans before `POST
    /// /gate/v1/sessions`; admission stays authoritative (`CONTENT.*`). Absent: a server before
    /// CR-S51 (a Supporter's Hardcore profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_required: Option<bool>,
    /// CR-S83 (OD-168 follow-up DB-Q4): whether the profile appears on the play dashboard
    /// (default `true`; [`WebsiteVisibility`]). Absent: a server before CR-S83.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website_listed: Option<bool>,
}

/// Why a profile carries another name than the one its player chose (OD-120 backfill).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameNotice {
    /// The plain name it had before the clash rename.
    pub previous_name: String,
    /// When the backfill renamed it.
    pub renamed_at: String,
}

/// `POST /launcher/v1/profiles/{xuid}/name` (ACCOUNTS.md §5 "Plain names and renames", OD-120):
/// the free rename after a clash rename; the current name keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameChange {
    /// The plain name to take.
    pub name: String,
}

/// `POST /launcher/v1/profiles/{xuid}/website` (ACCOUNTS.md §5, CR-S83, OD-168 follow-up DB-Q4):
/// whether the caller's own profile appears on the play dashboard (DASHBOARD_API.md).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebsiteVisibility {
    /// `false` hides it from every dashboard list and page.
    pub listed: bool,
}

/// VIP access as the account plane reads it (monetization's read model).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    /// `free`, `vip`.
    pub tier: String,
    /// fi-entitlements status string.
    pub status: String,
    /// End of access.
    pub access_until: Option<String>,
    /// `monthly`, `yearly`, `pass`, `comp`.
    pub plan: Option<String>,
    /// Provider of the current coverage.
    pub provider: Option<String>,
    /// Will renew.
    pub auto_renew: bool,
    /// A renewal charge is failing.
    pub past_due: bool,
}

/// A pending deletion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deletion {
    /// When requested.
    pub requested_at: String,
    /// When everything is erased unless cancelled.
    pub purge_at: String,
}

/// Something the launcher or portal shows on the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    /// `deletion_pending`, `strike`, `parked_save`, `vip_ending`, `update_required`.
    pub kind: String,
    /// `info`, `warning`, `error`.
    pub level: String,
    /// Localized text.
    pub text: String,
    /// Since when.
    pub since: String,
}

/// `GET /launcher/v1/me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    /// The account.
    pub account_id: String,
    /// `player`, `moderator`.
    pub role: String,
    /// Its profiles: every live career (CR-S98), as before.
    pub profiles: Vec<Profile>,
    /// CR-S98 (CAREERS.md §3): its profiles above their careers, oldest first. Absent: a server
    /// before CR-S98.
    #[serde(default)]
    pub players: Vec<Player>,
    /// VIP access.
    pub access: Access,
    /// Pending deletion.
    pub deletion: Option<Deletion>,
    /// Bound devices.
    pub devices: Vec<BoundDevice>,
    /// Notices.
    pub notices: Vec<Notice>,
}

/// A player-facing profile above its careers (CR-S98, CAREERS.md §1, §3): the gamertag, the XUID
/// and the mode are the same in every game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    /// The profile's id (its FH2 career's for a profile made before CR-S98).
    pub profile_id: String,
    /// The plain gamertag.
    pub gamertag: String,
    /// `hardcore`, `noob`.
    pub mode: String,
    /// Decimal XUID.
    pub xuid: String,
    /// `active`, `deletion_pending` (CAREERS.md §6.4).
    pub status: String,
    /// A live career's OD-120 clash-rename notice while its free rename is unused (null when
    /// none, as on `Profile`).
    #[serde(default)]
    pub rename_notice: Option<RenameNotice>,
    /// Its live careers, oldest first (none after every career was deleted).
    pub careers: Vec<Career>,
}

/// One game's career of a [`Player`] (CAREERS.md §3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Career {
    /// The game code.
    pub game: String,
    /// The career's id: the `profileId` of its [`Profile`] in `profiles`.
    pub career_id: String,
    /// When created.
    pub created_at: String,
    /// Last session end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_at: Option<String>,
    /// The cloud save line's head. Absent: the game keeps no line, or nothing is saved yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CareerCloud>,
    /// Its progress projection (`profile_progress`, CAREERS.md §6.4). Absent before the first sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<CareerProgress>,
}

/// A career's progress (CAREERS.md §6.4); each field absent when unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CareerProgress {
    /// Driver level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
    /// Credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<i64>,
    /// Cars owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cars: Option<i32>,
    /// Seconds played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_seconds: Option<i64>,
    /// When the projection last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<String>,
}

/// A career's cloud save head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CareerCloud {
    /// The head revision.
    pub revision: u64,
    /// When it was committed.
    pub saved_at: String,
    /// OD-239 (CR-S111): `true` on a career that no longer keeps a save line (a Noob/Azimuth
    /// career's old line): read-only, offered for export through the download route (SAVES_API.md
    /// §4.2) until its erasure 90 days after the release; absent on a live (Hardcore) line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_only: Option<bool>,
}

/// `POST /launcher/v1/players/{xuid}/careers` (CR-S98): the game of the career to start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CareerCreate {
    /// The game code.
    pub game: String,
}

/// A profile to create.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewProfile {
    /// `fh2`.
    pub game: String,
    /// Name part of the gamertag.
    pub name: String,
    /// `hardcore`, `noob`.
    pub mode: String,
    /// Deprecated (OD-60): left the contract, ignored by the server (the player picks the starter
    /// car in the game; a Hardcore line starts from the server's seed). Accepted, null or a number,
    /// only because the bodies refuse unknown fields and launchers still send it; removed once they
    /// stop (the frozen Rust launcher still serializes it; new clients leave it out).
    #[serde(default)]
    pub starter_car: Option<u32>,
}

/// `POST /launcher/v1/accounts` as it was before OD-20: no identity ticket. Withdrawn: the server
/// accepts [`LauncherAccountCreate`]; removed when the launcher migrates (LAU-M2, register I-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAccount {
    /// This launcher install.
    pub device: Device,
    /// The first profile.
    pub profile: NewProfile,
}

/// `POST /launcher/v1/accounts` → 201 before OD-20 (with a site key). Withdrawn: the server answers
/// [`LauncherAccountCreated`]; removed when the launcher migrates (LAU-M2, register I-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCreated {
    /// The account.
    pub account_id: String,
    /// Shown once.
    pub site_key: String,
    /// Device-bound, rotating.
    pub refresh_token: String,
    /// 15 minutes.
    pub access_token: String,
    /// Its expiry.
    pub access_expires_at: String,
    /// The first profile.
    pub profile: Profile,
}

/// `POST /launcher/v1/sign-in`. Withdrawn by OD-20 (ACCOUNTS.md §12): site keys only link Beta
/// accounts ([`LegacyLink`]); removed when the launcher moves to [`TokenGrant`] (register I-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteKeySignIn {
    /// Any gamertag of the account.
    pub gamertag: String,
    /// `XXXX-XXXX-XXXX`.
    pub site_key: String,
    /// This launcher install.
    pub device: Device,
}

/// `POST /launcher/v1/legacy/exchange`. Withdrawn by OD-20: replaced by [`LegacyLink`] with
/// `legacy_token` (register I-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacySignIn {
    /// Beta device token (64 hex) from `FH2Online_Accounts.ini`.
    pub legacy_token: String,
    /// This launcher install.
    pub device: Device,
}

/// A successful sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedIn {
    /// The account.
    pub account_id: String,
    /// Device-bound, rotating.
    pub refresh_token: String,
    /// 15 minutes.
    pub access_token: String,
    /// Its expiry.
    pub access_expires_at: String,
    /// The profile the legacy token belonged to (legacy exchange only).
    pub profile: Option<Profile>,
}

/// `POST /launcher/v1/auth/sign-up` and `/auth/sign-in` on a `local` instance (ACCOUNTS.md §5).
/// `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalCredentials {
    /// The e-mail address (trimmed and lower-cased by the server).
    pub email: String,
    /// 8-128 characters.
    pub password: String,
}

impl std::fmt::Debug for LocalCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCredentials").finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/auth/sign-up` → 201 and `/auth/sign-in` → 200: an ID token for
/// `POST /launcher/v1/auth/token`, opaque to the client. `Debug` never prints the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalIdToken {
    /// The ID token.
    pub id_token: String,
    /// Its life in seconds (300).
    pub expires_in: u32,
}

impl std::fmt::Debug for LocalIdToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalIdToken")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/auth/password` on a `local` instance: a fresh ID token and the new password.
/// `Debug` prints neither.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalPasswordChange {
    /// From a sign-in within the last 5 minutes.
    pub id_token: String,
    /// 8-128 characters.
    pub new_password: String,
}

impl std::fmt::Debug for LocalPasswordChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalPasswordChange")
            .finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/auth/confirmation` on a `local` instance (CR-S53): the ID token of a sign-in
/// whose exchange answered `AUTH.EMAIL_UNVERIFIED`. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalConfirmationRequest {
    /// From `/auth/sign-in` or `/auth/sign-up` (5 minutes).
    pub id_token: String,
}

impl std::fmt::Debug for LocalConfirmationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalConfirmationRequest")
            .finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/auth/confirmation/code` on a `local` instance (CR-S53 follow-up "6-digit
/// code"): a current ID token of `/auth/sign-in` (unconfirmed accepted) and the mailed code
/// (6 digits; spaces ignored). → 200 [`LocalIdToken`], confirmed. `Debug` prints neither.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalConfirmationCode {
    /// From `/auth/sign-in` (5 minutes), current for the credential.
    pub id_token: String,
    /// The 6 digits of the mail.
    pub code: String,
}

impl std::fmt::Debug for LocalConfirmationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalConfirmationCode")
            .finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/auth/password-reset` on a `local` instance (CR-S53): the address to mail a
/// password link to. Always 202, whether or not it has a credential. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalPasswordResetRequest {
    /// The e-mail address.
    pub email: String,
}

impl std::fmt::Debug for LocalPasswordResetRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalPasswordResetRequest")
            .finish_non_exhaustive()
    }
}

/// `POST /launcher/v1/token`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Refresh {
    /// The current refresh token.
    pub refresh_token: String,
}

/// `POST /launcher/v1/token` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPair {
    /// New access token.
    pub access_token: String,
    /// Its expiry.
    pub access_expires_at: String,
    /// The rotated refresh token (the old one is dead).
    pub refresh_token: String,
}

/// `POST /launcher/v1/profiles/{xuid}/mode` (ACCOUNTS.md §5 "Mode change", OD-52).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModeChange {
    /// `noob` (`hardcore` is always `ACCOUNTS.MODE_CHANGE_FORBIDDEN`).
    pub mode: String,
    /// `true` after the player saw the warning of `ACCOUNTS.MODE_CHANGE_CONFIRM`; absent or
    /// `false` asks for that warning and changes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
}

/// `details` of 409 `ACCOUNTS.MODE_CHANGE_CONFIRM`: what a Hardcore → Noob change deletes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeChangeWarning {
    /// The cloud save line's head revision: the launcher has it on the PC before confirming.
    pub save_revision: u64,
    /// Rows deleted, by category (`scores`, `ghosts`, `xp`, `drivatars`, `inbox`, `onlineProfile`,
    /// `saveRevisions`; zeros included). Clients ignore categories they do not know. Friendships
    /// are never deleted (they belong to the account).
    pub deletes: std::collections::BTreeMap<String, u64>,
}

/// `POST .../me/deletion`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeletionRequest {
    /// The player types one of their gamertags to confirm.
    pub confirm_gamertag: String,
}

/// `POST .../me/site-key` → 201. Withdrawn by OD-20 (ACCOUNTS.md §12, register I-10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteKeyIssued {
    /// Shown once.
    pub site_key: String,
}

/// `POST /launcher/v1/portal-codes` → 201.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalCode {
    /// Single use.
    pub code: String,
    /// 60 s after issue.
    pub expires_at: String,
}

/// `POST /launcher/v1/auth/token`: the Identity Platform sign-in exchange (ACCOUNTS.md §4.1, §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "grantType",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TokenGrant {
    /// Native sign-in (OD-43): a fresh Identity Platform ID token the launcher minted itself with
    /// `accounts:signInWithPassword`, verified as in ACCOUNTS.md §4.1.
    IdToken {
        /// The ID token (JWT), at most 5 minutes after its `iat`.
        id_token: String,
        /// This launcher install.
        device: Device,
    },
}

/// The outcome of every Identity Platform exchange (ACCOUNTS.md §4.1): `/launcher/v1/auth/token`,
/// `/launcher/v1/legacy/link` (always `signed_in`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SignInResult {
    /// The identity maps to an account; the device is bound.
    SignedIn {
        /// The account.
        account_id: String,
        /// Device-bound, rotating.
        refresh_token: String,
        /// 15 minutes.
        access_token: String,
        /// Its expiry.
        access_expires_at: String,
        /// The linked Beta profile (legacy link with a device token only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<Box<Profile>>,
    },
    /// Unknown identity: create an account or link a Beta account with the ticket.
    NeedsAccount {
        /// `fi_it_…`, 15 minutes, single use.
        identity_ticket: String,
        /// Its expiry.
        identity_ticket_expires_at: String,
        /// The identity's verified e-mail, to show the player which identity this is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
}

/// `POST /launcher/v1/legacy/link`: an identity ticket plus exactly one Beta credential, either
/// `gamertag` and `site_key` or `legacy_token` (ACCOUNTS.md §4.1, §10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyLink {
    /// From a `needs_account` [`SignInResult`].
    pub identity_ticket: String,
    /// This launcher install.
    pub device: Device,
    /// Any gamertag of the Beta account (with `site_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamertag: Option<String>,
    /// `XXXX-XXXX-XXXX` (with `gamertag`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_key: Option<String>,
    /// Beta device token (64 hex) from `FH2Online_Accounts.ini`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_token: Option<String>,
}

/// A fresh Identity Platform ID token proving a recent sign-in (`POST /launcher/v1/me/deletion`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reauth {
    /// Verified as ACCOUNTS.md §4.1; both `iat` and `auth_time` within five minutes.
    pub id_token: String,
}

/// `POST /launcher/v1/accounts`: the account and its first profile, from a verified identity
/// (ACCOUNTS.md §4.1, §5). Replaces [`CreateAccount`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LauncherAccountCreate {
    /// From a `needs_account` [`SignInResult`].
    pub identity_ticket: String,
    /// This launcher install (bound to the new account).
    pub device: Device,
    /// The first profile.
    pub profile: NewProfile,
}

/// `POST /launcher/v1/accounts` → 201 (no site key: sign-in is Identity Platform). Replaces
/// [`AccountCreated`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherAccountCreated {
    /// The account.
    pub account_id: String,
    /// Device-bound, rotating.
    pub refresh_token: String,
    /// 15 minutes.
    pub access_token: String,
    /// Its expiry.
    pub access_expires_at: String,
    /// The first profile.
    pub profile: Profile,
}

/// `POST /launcher/v1/me/deletion`: typed confirmation plus a fresh ID token
/// (ACCOUNTS.md §5, §8). The launcher's form of [`DeletionRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LauncherDeletionRequest {
    /// One of the account's gamertags, typed by the player.
    pub confirm_gamertag: String,
    /// The password sign-in run again: its `auth_time` must be recent.
    pub reauth: Reauth,
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use serde_json::json;

    fn device() -> Device {
        Device {
            device_id: "0192f0c4-0000-7000-8000-000000000001".into(),
            label: "Steam Deck".into(),
            platform: "proton".into(),
            fingerprint: None,
        }
    }

    #[test]
    fn retired_launcher_grants_are_refused() {
        for grant in [
            json!({"grantType": "authorization_code", "code": "c", "codeVerifier": "v",
                   "redirectUri": "http://127.0.0.1:1234/callback", "device": device()}),
            json!({"grantType": "device_code", "deviceCode": "d", "device": device()}),
        ] {
            assert!(serde_json::from_value::<TokenGrant>(grant).is_err());
        }
    }

    #[test]
    fn the_id_token_grant_uses_the_contract_spelling() {
        let grant = TokenGrant::IdToken {
            id_token: "eyJ.x.y".into(),
            device: device(),
        };
        let wire = serde_json::to_value(&grant).unwrap();
        assert_eq!(
            wire,
            json!({"grantType": "id_token", "idToken": "eyJ.x.y", "device": device()})
        );
        assert_eq!(serde_json::from_value::<TokenGrant>(wire).unwrap(), grant);
        let extra = json!({"grantType": "id_token", "idToken": "t", "device": device(),
                           "password": "never"});
        assert!(
            serde_json::from_value::<TokenGrant>(extra).is_err(),
            "unknown fields refused"
        );
        let snake = json!({"grantType": "id_token", "id_token": "t", "device": device()});
        assert!(serde_json::from_value::<TokenGrant>(snake).is_err());
    }

    #[test]
    fn sign_in_results_are_tagged_by_status() {
        let needs = SignInResult::NeedsAccount {
            identity_ticket: "fi_it_x".into(),
            identity_ticket_expires_at: "2026-10-17T14:15:00.000000Z".into(),
            email: None,
        };
        let wire = serde_json::to_value(&needs).unwrap();
        assert_eq!(
            wire,
            json!({"status": "needs_account", "identityTicket": "fi_it_x",
                   "identityTicketExpiresAt": "2026-10-17T14:15:00.000000Z"})
        );
        let signed = json!({"status": "signed_in", "accountId": "a", "refreshToken": "r",
                            "accessToken": "t", "accessExpiresAt": "e"});
        assert!(matches!(
            serde_json::from_value::<SignInResult>(signed).unwrap(),
            SignInResult::SignedIn { profile: None, .. }
        ));
    }
}

#[cfg(test)]
mod platform_v1_tests {
    use super::*;

    #[test]
    fn trust_is_absent_unless_published() {
        let mut info: ServerInfo = serde_json::from_value(serde_json::json!({
            "apiVersion": 1, "realm": "r", "minLauncher": "0.1.0", "minHook": "0.1.0",
            "latestLauncher": "0.1.0", "latestHook": "0.1.0", "allowedCarDb": [],
            "checkpoints": {"enabled": true, "minIntervalSecs": 300, "maxCommitsPerHour": 30},
            "motd": null
        }))
        .unwrap();
        assert_eq!(info.trust, None);
        let wire = serde_json::to_value(&info).unwrap();
        assert!(wire.get("trust").is_none());
        let lt = format!("lt-1:{}", "0a".repeat(32));
        info.trust = Some(TrustKeys {
            launch_ticket: vec![lt.clone()],
            relay_grant: vec![],
        });
        let wire = serde_json::to_value(&info).unwrap();
        assert_eq!(
            wire["trust"],
            serde_json::json!({"launchTicket": [lt], "relayGrant": []})
        );
        assert_eq!(serde_json::from_value::<ServerInfo>(wire).unwrap(), info);
    }

    #[test]
    fn idp_omits_the_emulator_url_outside_emulator_mode() {
        let prod = IdpConfig {
            kind: IdpKind::Firebase,
            api_key: "public-web-api-key".into(),
            project_id: "forza-infinity".into(),
            emulator_url: None,
        };
        let wire = serde_json::to_value(&prod).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"apiKey": "public-web-api-key", "projectId": "forza-infinity"})
        );
        assert_eq!(serde_json::from_value::<IdpConfig>(wire).unwrap(), prod);
        let local = serde_json::json!({"apiKey": "demo-api-key", "projectId": "demo-forza-infinity",
                                       "emulatorUrl": "http://127.0.0.1:9399"});
        assert_eq!(
            serde_json::from_value::<IdpConfig>(local)
                .unwrap()
                .emulator_url
                .as_deref(),
            Some("http://127.0.0.1:9399")
        );
        // A `local` instance says only its kind (OD-122); `kind` absent reads as `firebase`.
        let own = IdpConfig {
            kind: IdpKind::Local,
            ..IdpConfig::default()
        };
        assert_eq!(
            serde_json::to_value(&own).unwrap(),
            serde_json::json!({"kind": "local"})
        );
        assert_eq!(
            serde_json::from_value::<IdpConfig>(serde_json::json!({"kind": "local"})).unwrap(),
            own
        );
    }
}
