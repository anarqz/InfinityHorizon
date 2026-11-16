//! The values the save system talks about (docs/server/CLOUD_SAVES.md §2).
//!
//! A profile has one **save line**: a strictly increasing sequence of immutable **revisions**
//! (1, 2, 3, ...; 0 means "no save yet"). Each revision points at one content-addressed **blob**
//! (the FH2SAVE1 bundle, named by its SHA-256). Exactly one **lease** at a time may write the
//! line; each grant carries a **fencing token** larger than every token granted before, so a
//! writer that lost its lease (expiry, takeover) is refused even if it still believes it holds it.
//!
//! The line also accrues **play time**: the time some session held a live lease on it, credited
//! on every grant, heartbeat, commit and release under the store's lock and on its clock. The
//! Hardcore gates measure allowances on play time, not on wall-clock time (CLOUD_SAVES.md §7).

use fi_domain::{DeviceId, PlaySessionId, ProfileId, ProfileMode, UnixMs};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Fencing token: the ordinal of a lease grant on one save line, starting at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FenceToken(u64);

impl FenceToken {
    /// The first token of a line.
    pub const FIRST: FenceToken = FenceToken(1);

    /// A token read from storage or a request; `None` for 0, which is never granted.
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The token granted after this one.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for FenceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A revision number of a save line. [`Revision::NONE`] (0) is the empty line.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    /// No save yet.
    pub const NONE: Revision = Revision(0);

    /// From a number.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The revision a commit on top of this one creates.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Whether the line has no save.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The client's idempotency key for one commit (`Idempotency-Key`). Retrying a commit with the
/// same key returns the first result instead of creating a second revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitId(Uuid);

impl CommitId {
    /// A new random key.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// From a UUID; `None` for the nil UUID.
    pub fn from_uuid(value: Uuid) -> Option<Self> {
        (!value.is_nil()).then_some(Self(value))
    }

    /// The UUID.
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for CommitId {
    fn default() -> Self {
        Self::new()
    }
}

/// A SHA-256 digest. Text form: 64 lower-case hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(sha2::Sha256::digest(bytes).into())
    }

    /// Lower-case hex.
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.0 {
            out.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
            out.push(char::from(b"0123456789abcdef"[(b & 15) as usize]));
        }
        out
    }

    /// From a 32-byte slice (a `bytea` column).
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self)
    }
}

/// Not 64 hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a SHA-256 digest is 64 hex digits")]
pub struct InvalidDigest;

impl FromStr for Digest {
    type Err = InvalidDigest;

    fn from_str(s: &str) -> Result<Self, InvalidDigest> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(InvalidDigest);
        }
        let nibble = |c: u8| -> Result<u8, InvalidDigest> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(InvalidDigest),
            }
        };
        let mut out = [0u8; 32];
        let (pairs, _) = bytes.as_chunks::<2>();
        for (i, [high, low]) in pairs.iter().enumerate() {
            out[i] = nibble(*high)? << 4 | nibble(*low)?;
        }
        Ok(Self(out))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", &self.hex()[..12])
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Who asks for a lease: one play session on one device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHolder {
    /// The play session (generated by the launcher; also the pool slot's id).
    pub session: PlaySessionId,
    /// The launcher install.
    pub device: DeviceId,
    /// The PC's name, shown to the player when another PC asks for the lease ("in use on DESKTOP-7").
    pub device_label: String,
}

/// A granted lease, as stored. It may already be expired: compare with the store's `now`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// Who holds it.
    pub holder: LeaseHolder,
    /// Its fencing token.
    pub token: FenceToken,
    /// When it was granted.
    pub acquired_at: UnixMs,
    /// When it lapses unless renewed (heartbeat, or any successful commit).
    pub expires_at: UnixMs,
    /// The renewal length the holder asked for.
    pub ttl_ms: u32,
    /// Play time has been credited to the line up to this instant (the grant, then every
    /// heartbeat and commit). Never later than `expires_at`.
    pub accounted_at: UnixMs,
}

impl Lease {
    /// Whether the lease still holds at `now`.
    pub fn is_live(&self, now: UnixMs) -> bool {
        now < self.expires_at
    }
}

/// How hard an acquire pushes against a lease held by another session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireMode {
    /// Take the line only if it is free, expired, or held by an older session of the same device.
    Normal,
    /// The player confirmed "use it here instead": revoke the other holder. Its token is fenced.
    Takeover,
    /// A heartbeat that found its own lease lapsed (PC asleep, network down): re-grant it only if
    /// the line's stored lease is still this session's `previous` token, i.e. nobody was granted
    /// the line since. Otherwise `SAVES.LEASE_LOST` (SAVES_API.md §3.2).
    Resume {
        /// The token the session last held.
        previous: FenceToken,
    },
}

/// A lease request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcquireRequest {
    /// The requesting session.
    pub holder: LeaseHolder,
    /// Requested renewal length, between [`crate::lease::MIN_TTL_MS`] and [`crate::lease::MAX_TTL_MS`].
    pub ttl_ms: u32,
    /// Normal or takeover.
    pub mode: AcquireMode,
}

/// What happened to the lease that was there before a grant (audit, and the account plane marks
/// the replaced play session as taken over).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replaced {
    /// It had lapsed.
    Expired(Lease),
    /// An older session of the same device (a crashed launcher) was replaced.
    SameDevice(Lease),
    /// Another device was revoked on the player's confirmation.
    TakenOver(Lease),
}

/// Result of an acquire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseGrant {
    /// The lease the session now holds.
    pub lease: Lease,
    /// `true` when the same session already held it (a retried acquire): nothing changed.
    pub existing: bool,
    /// The lease this grant replaced, if any.
    pub replaced: Option<Replaced>,
}

/// Result of a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Released {
    /// The session's lease was released.
    Released,
    /// There was nothing to release (a retried release).
    AlreadyReleased,
}

/// Progress numbers of a save, read by the server from the bundle itself (never trusted from the
/// client): what the Hardcore gates compare and what conflict screens show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSummary {
    /// ProfileData `Credits`.
    pub credits: Option<i64>,
    /// ProfileData `XP`.
    pub xp: Option<i64>,
    /// The driver level, from 0 (the game shows it plus one). The server derives it from `xp` on
    /// the game's curve ([`crate::gates::with_driver_level`]) before it keeps or measures a
    /// summary; [`crate::bundle::summary`] alone (no game rules: the launcher, tools) reads
    /// ProfileData `Main/Level`, which is not the driver level (the level-ups done inside the
    /// current colour band: 0 at 223,773 XP, driver level 15 in game). Revisions stored before
    /// 2026-10-31 hold `Main/Level`; the gates derive their level again from their XP.
    pub level: Option<i64>,
    /// Cars in `Career_Garage` (needs the server's SQLite reader; `None` until SER-M2 lands it).
    pub cars: Option<i64>,
    /// ProfileData `TotalWinnings`: lifetime credits earned from races (the guardrails' credits
    /// conservation, OD-62). `None` when unreadable, and in revisions stored before it was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winnings: Option<i64>,
    /// ProfileData `NumSkillTokens`: skill tokens earned (OD-62's token conservation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    /// SaveState `SkillsState/SkillPointsTotal`: lifetime skill points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_points: Option<i64>,
    /// SaveState `SkillsState/UnspentSkillPointsTotal`: points not yet turned into a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unspent_skill_points: Option<i64>,
}

impl SaveSummary {
    /// The baseline the gates measure against when no revision of the line has readable numbers:
    /// everything at zero, so every number must fit on its own (fail closed).
    pub const ZERO: SaveSummary = SaveSummary {
        credits: Some(0),
        xp: Some(0),
        level: Some(0),
        cars: None,
        winnings: None,
        tokens: None,
        skill_points: None,
        unspent_skill_points: None,
    };

    /// Whether every number the gates need is present.
    pub fn is_complete_for_gates(&self) -> bool {
        self.credits.is_some() && self.xp.is_some() && self.level.is_some()
    }
}

/// Why a revision exists. Wire strings are stored in `save_revision.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// A checkpoint or end-of-session upload by the player's launcher.
    Upload,
    /// The player resolved a conflict by keeping this PC's save on top of the cloud's.
    ResolveKeepLocal,
    /// An older revision brought back as the newest (history is never rewritten).
    Restore,
    /// A Hardcore profile's first save, generated by the server from a starter template (loaded
    /// at run time from a private store, never from the repository).
    FreshCareer,
    /// A Noob profile's first save: the player's offline career.
    ImportOffline,
    /// Copied from the Beta server by the migration.
    BetaImport,
    /// A moderator reset this profile.
    ModerationReset,
    /// A season reset.
    SeasonReset,
    /// A career reset.
    CareerReset,
    /// A moderator accepted a parked bundle (a gate refusal reviewed and overturned); the
    /// revision names it in `parked_from`.
    ParkedAccept,
    /// A Hardcore line's revision 1 from Azimuth's own first save (CR-S51, SAVES_API.md §4.9):
    /// uploaded by the player's launcher on a line whose start is `fresh_start`, verified on
    /// receipt by the fresh-start check (CLOUD_SAVES.md §7.4) instead of the guardrails. The
    /// line's base and a rate anchor, as a seed's `fresh_career`.
    FreshStart,
    /// A Hardcore line's revision 1 from another of the account's profiles' save (CR-S52,
    /// SAVES_API.md §4.10): uploaded by the player's launcher on a server allowing it
    /// (`FI_HARDCORE_FROM_PROFILE`), verified on receipt by the seed check (CLOUD_SAVES.md §7.6)
    /// and recorded with its source profile. The line's base and a rate anchor, as `fresh_start`.
    SeedFromProfile,
}

impl CommitKind {
    /// Every kind (the `save_revision.kind` CHECK lists the same strings).
    pub const ALL: [CommitKind; 12] = [
        CommitKind::Upload,
        CommitKind::ResolveKeepLocal,
        CommitKind::Restore,
        CommitKind::FreshCareer,
        CommitKind::ImportOffline,
        CommitKind::BetaImport,
        CommitKind::ModerationReset,
        CommitKind::SeasonReset,
        CommitKind::CareerReset,
        CommitKind::ParkedAccept,
        CommitKind::FreshStart,
        CommitKind::SeedFromProfile,
    ];

    /// Stable wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            CommitKind::Upload => "upload",
            CommitKind::ResolveKeepLocal => "resolve_keep_local",
            CommitKind::Restore => "restore",
            CommitKind::FreshCareer => "fresh_career",
            CommitKind::ImportOffline => "import_offline",
            CommitKind::BetaImport => "beta_import",
            CommitKind::ModerationReset => "moderation_reset",
            CommitKind::SeasonReset => "season_reset",
            CommitKind::CareerReset => "career_reset",
            CommitKind::ParkedAccept => "parked_accept",
            CommitKind::FreshStart => "fresh_start",
            CommitKind::SeedFromProfile => "seed_from_profile",
        }
    }

    /// Parses a stored kind.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether the bytes come from a player's PC (the Hardcore gates, or for a `fresh_start` the
    /// fresh-start check, measure these).
    pub const fn is_player_upload(self) -> bool {
        matches!(
            self,
            CommitKind::Upload
                | CommitKind::ResolveKeepLocal
                | CommitKind::ImportOffline
                | CommitKind::FreshStart
                | CommitKind::SeedFromProfile
        )
    }

    /// Whether the guardrails' rates start from zero at this revision (CLOUD_SAVES.md §7.3): a new
    /// career (fresh career, Azimuth's verified fresh start, Beta import), a reset, or a
    /// moderator's acceptance of a parked bundle.
    /// Not a restore (review G2-2: restoring an older revision and gaining its progress back would
    /// otherwise wipe the rates whenever a player liked), which the replay steps through like an
    /// upload; not a player's upload.
    pub const fn is_rate_anchor(self) -> bool {
        matches!(
            self,
            CommitKind::FreshCareer
                | CommitKind::FreshStart
                | CommitKind::SeedFromProfile
                | CommitKind::BetaImport
                | CommitKind::ModerationReset
                | CommitKind::SeasonReset
                | CommitKind::CareerReset
                | CommitKind::ParkedAccept
        )
    }

    /// Whether the spin allowance's level high-water mark restarts at this revision
    /// ([`CommitKind::is_rate_anchor`], and a restore: the restored career earns its level-ups,
    /// and their spins, again; review G2-3).
    pub const fn restarts_level_high(self) -> bool {
        self.is_rate_anchor() || matches!(self, CommitKind::Restore)
    }

    /// Whether the commit is a reset: it moves `reset_revision`, and players may no longer
    /// restore anything older on their own (CLOUD_SAVES.md §8).
    pub const fn is_reset(self) -> bool {
        matches!(
            self,
            CommitKind::ModerationReset | CommitKind::SeasonReset | CommitKind::CareerReset
        )
    }

    /// Whether `actor` may write a revision of this kind (the `save_revision_kind_actor` CHECK
    /// says the same).
    pub const fn allows(self, actor: Actor) -> bool {
        match self {
            CommitKind::Upload
            | CommitKind::ResolveKeepLocal
            | CommitKind::ImportOffline
            | CommitKind::FreshStart
            | CommitKind::SeedFromProfile => matches!(actor, Actor::Player),
            CommitKind::Restore => matches!(actor, Actor::Player | Actor::Moderator),
            CommitKind::FreshCareer => matches!(actor, Actor::System),
            CommitKind::BetaImport => matches!(actor, Actor::Import),
            CommitKind::ModerationReset | CommitKind::ParkedAccept => {
                matches!(actor, Actor::Moderator)
            }
            CommitKind::SeasonReset | CommitKind::CareerReset => {
                matches!(actor, Actor::System | Actor::Moderator)
            }
        }
    }
}

/// Who wrote a revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The player (through the launcher).
    Player,
    /// A server job (fresh career, scheduled resets).
    System,
    /// A moderator (resets, restores on a support request).
    Moderator,
    /// The Beta migration.
    Import,
}

impl Actor {
    /// Every actor.
    pub const ALL: [Actor; 4] = [
        Actor::Player,
        Actor::System,
        Actor::Moderator,
        Actor::Import,
    ];

    /// Stable wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Actor::Player => "player",
            Actor::System => "system",
            Actor::Moderator => "moderator",
            Actor::Import => "import",
        }
    }

    /// Parses a stored actor.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

/// A content-addressed bundle: its SHA-256 and size, and its format when its bytes were at hand.
///
/// Equality is the bundle's identity (digest and size): `format` is a function of the bytes, so
/// two refs of the same digest name the same format whether or not either read it.
#[derive(Clone, Copy, Debug)]
pub struct BlobRef {
    /// SHA-256 of the bundle bytes (also its object name `blobs/sha256/<hex>`).
    pub sha256: Digest,
    /// Bundle size in bytes.
    pub size: u32,
    /// The format the bytes' magic names ([`crate::SaveFormat::of_magic`], CR-S105 review 4),
    /// stored as `save_blob.format` when the blob's row is created; `None` for bytes of neither
    /// magic (an unreadable bundle parked) or a ref built without its bytes (read back from a
    /// store), whose row takes its line's format.
    pub format: Option<crate::SaveFormat>,
}

impl PartialEq for BlobRef {
    fn eq(&self, other: &Self) -> bool {
        self.sha256 == other.sha256 && self.size == other.size
    }
}

impl Eq for BlobRef {}

impl BlobRef {
    /// The ref of `bytes`: their digest, size and format by magic.
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: Digest::of(bytes),
            size: bytes.len() as u32,
            format: crate::SaveFormat::of_magic(bytes),
        }
    }

    /// Object name in the cloud-saves bucket.
    pub fn storage_key(&self) -> String {
        format!("blobs/sha256/{}", self.sha256.hex())
    }
}

/// A commit: one new revision on top of `expected`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRequest {
    /// Whose line.
    pub profile: ProfileId,
    /// The committing session (must hold the lease).
    pub session: PlaySessionId,
    /// Its fencing token.
    pub token: FenceToken,
    /// The head the client based this save on (`If-Match`).
    pub expected: Revision,
    /// Idempotency key.
    pub commit_id: CommitId,
    /// The bundle, already stored in the blob store.
    pub blob: BlobRef,
    /// Content digest of the save's data files (crate::bundle::content_digest): equal for equal
    /// saves whatever folder label or packing the bundle carries.
    pub content: Digest,
    /// Why.
    pub kind: CommitKind,
    /// Who.
    pub actor: Actor,
    /// The device the bytes came from, for player uploads.
    pub device: Option<DeviceId>,
    /// Azimuth's revision folder name (16 upper-case hex digits) the bundle was read from.
    pub client_label: Option<String>,
    /// For [`CommitKind::Restore`]: the revision brought back.
    pub restored_from: Option<Revision>,
    /// Progress numbers read from the bundle by the server.
    pub summary: SaveSummary,
    /// For player uploads on a Hardcore line: the head the gates measured this bundle against.
    /// The store refuses such a commit unless that is exactly the head it lands on, so no path
    /// can commit a Hardcore upload whose gate verdict belongs to another head (CLOUD_SAVES.md
    /// §5.1). `None` for every other commit.
    pub gated_base: Option<Revision>,
    /// For [`CommitKind::ParkedAccept`]: the parked bundle the moderator accepted.
    pub parked_from: Option<Uuid>,
    /// For a gated player upload (hybrid Hardcore checks, CLOUD_SAVES.md §7.5): what the nightly
    /// guardrails pass needs beyond the line's history, recorded with the revision
    /// (`save_revision.guard_input`), which queues it for the pass. `None` for every other commit.
    pub guard_input: Option<GuardInput>,
    /// A [`CommitKind::SeedFromProfile`] commit (CR-S52): the profile whose save it is, recorded
    /// with the revision as its provenance (`save_seed_source`). `None` for every other commit.
    pub seed_source: Option<ProfileId>,
}

/// What the nightly guardrails pass reads with a revision besides the line's history (CLOUD_SAVES.md
/// §7.5): the profile's last accepted game sync the upload was compared with, as read at the
/// upload (the RPC baseline moves on afterwards), and the play time it was measured at.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GuardInput {
    /// The game sync, `None` before any.
    pub game: Option<fi_guard::GameSync>,
    /// The line's play time the upload was measured at (the head's, read by the upload; the
    /// commit credits a little more under its lock), so the pass measures what the upload did.
    /// `None`: the revision's own play time.
    pub play_ms: Option<u64>,
}

// Only compared in tests and the conformance suite, never hashed; a NaN never reaches it (the
// hours are computed from a clock difference).
impl Eq for GuardInput {}

/// An immutable revision, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionRecord {
    /// Whose line.
    pub profile: ProfileId,
    /// Its number.
    pub revision: Revision,
    /// The idempotency key that created it.
    pub commit_id: CommitId,
    /// Its bundle.
    pub blob: BlobRef,
    /// Its content digest.
    pub content: Digest,
    /// Why it exists.
    pub kind: CommitKind,
    /// Who wrote it.
    pub actor: Actor,
    /// The session that held the lease.
    pub session: PlaySessionId,
    /// The token it was written under.
    pub token: FenceToken,
    /// Source device (player uploads).
    pub device: Option<DeviceId>,
    /// Azimuth folder label.
    pub client_label: Option<String>,
    /// For restores: the source revision.
    pub restored_from: Option<Revision>,
    /// For accepted parked bundles: the parked bundle.
    pub parked_from: Option<Uuid>,
    /// Progress numbers.
    pub summary: SaveSummary,
    /// Server time of the commit.
    pub committed_at: UnixMs,
    /// The line's accrued play time at the commit (milliseconds a session held a live lease).
    pub play_ms: u64,
    /// Kept by retention whatever its age (pre-reset copies, support pins).
    pub pinned: bool,
}

/// Result of a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The revision (new, or the one a replayed key created).
    pub record: RevisionRecord,
    /// `true` when the idempotency key had already committed: nothing new was written.
    pub replayed: bool,
    /// `true` when this commit created the blob's row. Garbage collection may have deleted an
    /// object whose row was gone between the caller's put and this commit, so the caller puts the
    /// object again (create-only, so a second put of a present object is a no-op; CLOUD_SAVES.md
    /// §6).
    pub blob_created: bool,
}

/// A save line's head, read without taking the lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadView {
    /// Whose line.
    pub profile: ProfileId,
    /// The profile's mode (Postgres: `profile.mode`).
    pub mode: ProfileMode,
    /// The newest revision ([`Revision::NONE`] when empty).
    pub revision: Revision,
    /// The newest revision's record.
    pub current: Option<RevisionRecord>,
    /// The lease as stored (possibly expired; compare with `now`).
    pub lease: Option<Lease>,
    /// Highest token ever granted on the line.
    pub fence_seq: u64,
    /// The newest reset revision ([`Revision::NONE`] if the line was never reset). Players may
    /// not restore anything older on their own.
    pub reset_revision: Revision,
    /// The line's play time at `now`, including the live lease's part not credited yet.
    pub play_ms: u64,
    /// The store's clock when it read the head (database time for Postgres).
    pub now: UnixMs,
}

impl HeadView {
    /// The live lease at the time of the read, if any.
    pub fn live_lease(&self) -> Option<&Lease> {
        self.lease.as_ref().filter(|l| l.is_live(self.now))
    }
}

/// Why a bundle was put aside instead of committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkReason {
    /// The player kept the cloud's save in a conflict; this PC's save is kept here.
    ConflictLoser,
    /// The Hardcore gates refused it; kept for review (a moderator may accept it).
    GateRefused,
    /// The server could not read the numbers it needs from it.
    Unreadable,
    /// The head, copied before a restore replaced it.
    PreRestore,
    /// The head, copied before a reset replaced it.
    PreReset,
}

impl ParkReason {
    /// Every reason.
    pub const ALL: [ParkReason; 5] = [
        ParkReason::ConflictLoser,
        ParkReason::GateRefused,
        ParkReason::Unreadable,
        ParkReason::PreRestore,
        ParkReason::PreReset,
    ];

    /// Stable wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            ParkReason::ConflictLoser => "conflict_loser",
            ParkReason::GateRefused => "gate_refused",
            ParkReason::Unreadable => "unreadable",
            ParkReason::PreRestore => "pre_restore",
            ParkReason::PreReset => "pre_reset",
        }
    }

    /// Parses a stored reason.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// Whether the bundle is one the player sent (counted against
    /// [`crate::SavePolicy::max_parked`]), not the server's own copy of the head.
    pub const fn sent_by_player(self) -> bool {
        matches!(
            self,
            ParkReason::ConflictLoser | ParkReason::GateRefused | ParkReason::Unreadable
        )
    }
}

/// A bundle to keep outside the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkRequest {
    /// Whose.
    pub profile: ProfileId,
    /// The bundle (already in the blob store).
    pub blob: BlobRef,
    /// Its content digest.
    pub content: Digest,
    /// Why.
    pub reason: ParkReason,
    /// The head the client based it on.
    pub based_on: Option<Revision>,
    /// Session that sent it.
    pub session: Option<PlaySessionId>,
    /// Device that sent it.
    pub device: Option<DeviceId>,
    /// Its numbers, when readable.
    pub summary: Option<SaveSummary>,
    /// Human-readable reason (gate message), never personal data.
    pub detail: String,
    /// How long to keep it.
    pub retain_ms: i64,
}

/// A parked bundle, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedRecord {
    /// Its id.
    pub id: Uuid,
    /// What was parked.
    pub request: ParkRequest,
    /// When.
    pub parked_at: UnixMs,
    /// When retention drops it.
    pub expires_at: UnixMs,
    /// `true` when parking created the blob's row (see [`Committed::blob_created`]).
    pub blob_created: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_text_round_trips() {
        let d = Digest::of(b"forza");
        let text = d.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Digest>().unwrap(), d);
        assert_eq!(text.to_ascii_uppercase().parse::<Digest>().unwrap(), d);
        assert!("xyz".parse::<Digest>().is_err());
        assert_eq!(serde_json::to_string(&d).unwrap(), format!("\"{text}\""));
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(
            Digest::of(b"abc").hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn kinds_and_reasons_round_trip() {
        for k in CommitKind::ALL {
            assert_eq!(CommitKind::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        for r in ParkReason::ALL {
            assert_eq!(ParkReason::parse(r.as_str()), Some(r));
        }
        for a in Actor::ALL {
            assert_eq!(Actor::parse(a.as_str()), Some(a));
        }
        assert!(CommitKind::Upload.is_player_upload());
        assert!(!CommitKind::Restore.is_player_upload());
        assert!(CommitKind::SeasonReset.is_reset() && !CommitKind::Restore.is_reset());
        // Review G2-2: a restore is not where the rates start; it restarts the level mark only.
        assert!(!CommitKind::Restore.is_rate_anchor());
        assert!(CommitKind::Restore.restarts_level_high());
        for k in CommitKind::ALL {
            // CR-S51: Azimuth's verified first save is both the player's bytes and a new career.
            assert_eq!(
                k.is_rate_anchor(),
                (!k.is_player_upload() && k != CommitKind::Restore)
                    || matches!(k, CommitKind::FreshStart | CommitKind::SeedFromProfile),
                "{k:?}"
            );
            assert!(!k.is_rate_anchor() || k.restarts_level_high());
        }
        for k in CommitKind::ALL {
            assert!(
                Actor::ALL.into_iter().any(|a| k.allows(a)),
                "{k:?} has a writer"
            );
            assert_eq!(
                k.is_player_upload(),
                k.allows(Actor::Player) && k != CommitKind::Restore
            );
        }
        assert!(
            !CommitKind::ParkedAccept.allows(Actor::Player),
            "only moderators accept"
        );
        assert!(!CommitKind::FreshCareer.allows(Actor::Player));
    }

    #[test]
    fn tokens_start_at_one() {
        assert_eq!(FenceToken::new(0), None);
        assert_eq!(FenceToken::FIRST.next().get(), 2);
        assert!(Revision::NONE.is_none());
        assert_eq!(Revision::NONE.next(), Revision::new(1));
    }
}
