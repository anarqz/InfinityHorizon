// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;

namespace Infinity.Core.Api;

// The launcher's wire bodies, mirrored from crates/fi-protocol (accounts.rs, gate.rs, saves.rs)
// field for field. The Rust launcher used fi-protocol directly; C# cannot, so these records are a
// third mirror, held to packages/contracts (the TypeScript mirror fi-protocol already tests
// against) by Infinity.Core.Tests ContractParityTests. Never add a field here that the contract
// does not have. Instants are kept as the wire strings (ACCOUNTS.md §1).

/// <summary><c>GET /launcher/v1/info</c>.</summary>
public sealed record ServerInfo(
    uint ApiVersion,
    string Realm,
    string MinLauncher,
    string MinHook,
    string LatestLauncher,
    string LatestHook,
    IReadOnlyList<string> AllowedCarDb,
    CheckpointPolicy Checkpoints,
    string? Motd,
    ServiceMap? Services,
    IReadOnlyList<GameInfo>? Games,
    IdpConfig Idp,
    bool RegistrationEnabled = true,
    TrustKeys? Trust = null,
    SavesPolicy? Saves = null);

/// <summary><c>GET /info</c> <c>saves</c> (CR-S51): which profiles keep a cloud line on this server; absent = OD-52. Explanatory only.</summary>
public sealed record SavesPolicy(CloudLines Cloud);

/// <summary>The server's <c>FI_CLOUD_SAVES</c> over Noob/Hardcore × free/Supporter.</summary>
public sealed record CloudLines(bool NoobFree, bool HardcoreFree, bool NoobVip, bool HardcoreVip);

/// <summary>
/// <c>GET /info</c> <c>trust</c> (CR-S49): a custom server's Ed25519 public keys as <c>kid:hex</c> (64 lowercase hex), current
/// first. Absent from the official server, whose keys are compiled in.
/// </summary>
public sealed record TrustKeys(IReadOnlyList<string> LaunchTicket, IReadOnlyList<string> RelayGrant);

/// <summary>
/// The native sign-in's identity backend (OD-43, CR-L38): <c>kind</c> <c>firebase</c> (absent from servers before CR-S48) carries
/// Identity Platform's <c>apiKey</c>/<c>projectId</c> and, on local stacks only, <c>emulatorUrl</c>; <c>local</c> carries none of
/// them and the instance's own <c>/launcher/v1/auth/sign-up</c> and <c>/auth/sign-in</c> mint the ID token.
/// </summary>
public sealed record IdpConfig(string? ApiKey, string? ProjectId, string? EmulatorUrl, string? Kind = null)
{
    /// <summary>A <c>local</c> instance (ACCOUNTS.md §4.1 "Two identity backends"); anything else, absent included, is <c>firebase</c>.</summary>
    [JsonIgnore]
    public bool IsLocal => Kind == IdpKinds.Local;
}

/// <summary><c>IdpConfig.kind</c>'s wire values.</summary>
public static class IdpKinds
{
    public const string Firebase = "firebase";
    public const string Local = "local";
}

/// <summary><c>POST /launcher/v1/auth/sign-up</c> and <c>/auth/sign-in</c> on a <c>local</c> instance; never printed with its password.</summary>
public sealed record LocalCredentials(string Email, string Password)
{
    private bool PrintMembers(System.Text.StringBuilder builder)
    {
        builder.Append("Email = ").Append(Email);
        return true;
    }
}

/// <summary>Their answer: an ID token, opaque to the launcher, for <c>POST /launcher/v1/auth/token</c>; <c>expiresIn</c> in seconds.</summary>
public sealed record LocalIdToken(string IdToken, long ExpiresIn)
{
    private bool PrintMembers(System.Text.StringBuilder builder)
    {
        builder.Append("ExpiresIn = ").Append(ExpiresIn);
        return true;
    }
}

/// <summary><c>POST /launcher/v1/auth/password</c> on a <c>local</c> instance: a fresh ID token from <c>/auth/sign-in</c> and the new password.</summary>
public sealed record LocalPasswordChange(string IdToken, string NewPassword)
{
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822", Justification = "A record's PrintMembers cannot be static.")]
    private bool PrintMembers(System.Text.StringBuilder builder) => false;
}

/// <summary><c>POST /launcher/v1/auth/confirmation</c> on a <c>local</c> instance (CR-S53): the ID token just minted by sign-up or sign-in.</summary>
public sealed record LocalConfirmationRequest(string IdToken)
{
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822", Justification = "A record's PrintMembers cannot be static.")]
    private bool PrintMembers(System.Text.StringBuilder builder) => false;
}

/// <summary>
/// <c>POST /launcher/v1/auth/confirmation/code</c> on a <c>local</c> instance (CR-S53 follow-up): a current <c>/auth/sign-in</c> token and the
/// mail's 6-digit code → a confirmed <see cref="LocalIdToken"/> for <c>/auth/token</c>.
/// </summary>
public sealed record LocalConfirmationCode(string IdToken, string Code)
{
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822", Justification = "A record's PrintMembers cannot be static.")]
    private bool PrintMembers(System.Text.StringBuilder builder) => false;
}

/// <summary><c>POST /launcher/v1/auth/password-reset</c> on a <c>local</c> instance (CR-S53): → 202 whatever the address.</summary>
public sealed record LocalPasswordResetRequest(string Email);

/// <summary>The origin of each platform v1 surface (CORE_API.md §2.1): no client hard-codes one.</summary>
public sealed record ServiceMap(string Core, string Gate, string Saves, string Matchmaking);

public sealed record GameInfo(string Id, string Title, string MinHook, string MinLauncher, bool Accepting, string? MaintenanceUntil);

public sealed record CheckpointPolicy(bool Enabled, uint MinIntervalSecs, uint MaxCommitsPerHour);

/// <summary>
/// A launcher install as it introduces itself (<c>platform</c>: windows, proton, linux; ACCOUNTS.md §2). <see cref="Fingerprint"/> is
/// OD-120's write-only same-device value (<see cref="Infinity.Core.Auth.DeviceFingerprint"/>); when it cannot be computed it is left
/// out of the body, never sent empty. Servers before the OD-120 deploy refuse it, so no launcher that sends it ships before then.
/// </summary>
public sealed record Device(string DeviceId, string Label, string Platform,
    [property: System.Text.Json.Serialization.JsonIgnore(Condition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull)] string? Fingerprint = null)
{
    /// <summary>A logged or interpolated <see cref="Device"/> never carries the fingerprint.</summary>
    private bool PrintMembers(System.Text.StringBuilder builder)
    {
        builder.Append("DeviceId = ").Append(DeviceId).Append(", Label = ").Append(Label).Append(", Platform = ").Append(Platform);
        return true;
    }
}

public sealed record BoundDevice(string DeviceId, string Label, string Platform, string BoundAt, string LastSeenAt);

/// <summary>
/// A game profile (<c>xuid</c> decimal, <c>gamertag</c> the plain <c>Name</c> since OD-120, <c>Name#12345</c> from older servers,
/// <c>mode</c> hardcore or noob). <see cref="RenameNotice"/> is set while an OD-120 clash rename's free rename is unused (absent
/// from servers before OD-120).
/// </summary>
public sealed record Profile(
    string ProfileId,
    string Game,
    string Xuid,
    string Gamertag,
    string Handle,
    string Mode,
    string Status,
    string CreatedAt,
    string? LastPlayedAt,
    RenameNotice? RenameNotice = null,
    // CR-S51: this server keeps the profile in the cloud / needs the OD-113 scan before admission; absent = OD-52 (a Supporter's
    // Hardcore). Explanatory: the admission decides.
    bool? CloudLine = null,
    bool? ContentRequired = null);

/// <summary>Why a profile carries another name than the one its player chose (OD-120 backfill, ACCOUNTS.md §5).</summary>
public sealed record RenameNotice(string PreviousName, string RenamedAt);

/// <summary><c>POST /launcher/v1/profiles/{xuid}/name</c> (OD-120, CR-S44): the free rename after a clash rename.</summary>
public sealed record NameChange(string Name);

/// <summary>Access as the account plane reads it (internal tier id <c>vip</c>; shown as "Infinity Supporter").</summary>
public sealed record Access(string Tier, string Status, string? AccessUntil, string? Plan, string? Provider, bool AutoRenew, bool PastDue);

public sealed record Deletion(string RequestedAt, string PurgeAt);

/// <summary>Fresh password sign-in proof; ACCOUNTS.md §5, OD-74.</summary>
public sealed record Reauth(string IdToken);

public sealed record LauncherDeletionRequest(string ConfirmGamertag, Reauth Reauth);

public sealed record Notice(string Kind, string Level, string Text, string Since);

/// <summary><c>GET /launcher/v1/me</c>.</summary>
public sealed record Me(
    string AccountId,
    string Role,
    IReadOnlyList<Profile> Profiles,
    Access Access,
    Deletion? Deletion,
    IReadOnlyList<BoundDevice> Devices,
    IReadOnlyList<Notice> Notices);

/// <summary>A profile to create; a Hardcore one starts from the server's seed at its first launch (OD-60).</summary>
public sealed record NewProfile(string Game, string Name, string Mode);

/// <summary><c>POST /launcher/v1/auth/token</c> with <c>grantType: id_token</c>: the launcher's own e-mail/password sign-in (OD-43).</summary>
public sealed record IdTokenGrant(string IdToken, Device Device)
{
    [JsonPropertyOrder(-1)]
    public string GrantType { get; init; } = "id_token";
}

/// <summary>The outcome of <c>POST /launcher/v1/auth/token</c>, tagged by <c>status</c>.</summary>
[JsonPolymorphic(TypeDiscriminatorPropertyName = "status")]
[JsonDerivedType(typeof(SignedIn), "signed_in")]
[JsonDerivedType(typeof(NeedsAccount), "needs_account")]
public abstract record SignInResult;

/// <summary>The identity maps to an account; the device is bound.</summary>
public sealed record SignedIn(string AccountId, string RefreshToken, string AccessToken, string AccessExpiresAt, Profile? Profile) : SignInResult;

/// <summary>Unknown identity: create an account with the ticket (15 minutes, single use).</summary>
public sealed record NeedsAccount(string IdentityTicket, string IdentityTicketExpiresAt, string? Email) : SignInResult;

/// <summary><c>POST /launcher/v1/accounts</c>.</summary>
public sealed record LauncherAccountCreate(string IdentityTicket, Device Device, NewProfile Profile);

/// <summary><c>POST /launcher/v1/accounts</c> → 201.</summary>
public sealed record LauncherAccountCreated(string AccountId, string RefreshToken, string AccessToken, string AccessExpiresAt, Profile Profile);

/// <summary><c>POST /launcher/v1/token</c>.</summary>
public sealed record Refresh(string RefreshToken);

/// <summary><c>POST /launcher/v1/token</c> → 200: the old refresh token is dead.</summary>
public sealed record TokenPair(string AccessToken, string AccessExpiresAt, string RefreshToken);

/// <summary>
/// <c>POST /gate/v1/sessions</c> (TypeScript <c>GateSessionCreate</c>: SAVES_API.md §3.1's body plus
/// OD-109's optional <c>dedicated</c>). <c>dedicated: true</c> asks for a dedicated lobby host session
/// (admins only, no game, no content scan): <c>carDbSha256</c> is then absent and <c>hookSha256</c> is the
/// empty-input digest. Both are left out of the body when unset, so an ordinary session's body is
/// unchanged for servers without dedicated support. Never sent to the legacy <c>/launcher/v1/sessions</c>.
/// </summary>
public sealed record PlaySessionCreate(
    string SessionId,
    string Game,
    string Xuid,
    string Nonce,
    string LauncherVersion,
    string HookSha256,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? CarDbSha256,
    ulong BaseRevision,
    bool TakeOver,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingDefault)] bool Dedicated = false,
    // OD-112 follow-up (GATE_API.md §2 "Modded sessions"): the launch plan's multiplayer mod set is
    // not empty; left out when false so an ordinary body is unchanged byte for byte.
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingDefault)] bool Modded = false);

/// <summary>The game access ticket (PASETO <c>v4.public</c>; the hook sends it as <c>X-Fi-Session</c>).</summary>
public sealed record GateTicket(string Token, string ExpiresAt, uint HeartbeatEveryMs);

/// <summary>Where the session plays: <c>server</c> is the game plane's base URL for the hook.</summary>
public sealed record Pool(string Name, string Tier, string Server);

/// <summary>Base64 of the canonical claims JSON and of the Ed25519 signature (ACCOUNTS.md §4.2).</summary>
public sealed record LaunchTicket(string Claims, string Signature);

public sealed record Lease(ulong FenceToken, uint TtlMs, uint HeartbeatEveryMs, string ExpiresAt);

public sealed record QueuePlace(uint Position, uint Ahead, uint? EtaSeconds, uint RetryAfterSeconds);

/// <summary><c>POST /gate/v1/sessions</c> (201 active, 202 queued) and <c>GET /gate/v1/sessions/{id}</c>, tagged by <c>state</c>.</summary>
[JsonPolymorphic(TypeDiscriminatorPropertyName = "state")]
[JsonDerivedType(typeof(GateActive), "active")]
[JsonDerivedType(typeof(GateQueued), "queued")]
public abstract record GateSession;

/// <summary>
/// Admitted. <c>lease</c> and <c>save</c> come for <c>vip</c> only; the save head is kept as raw
/// JSON until the launcher's save work (LAU-M4) reads it.
/// </summary>
public sealed record GateActive(
    string SessionId,
    string Game,
    Pool Pool,
    GateTicket Ticket,
    LaunchTicket LaunchTicket,
    Lease? Lease,
    System.Text.Json.JsonElement? Save,
    string GameUntil,
    string MaxUntil) : GateSession;

public sealed record GateQueued(string SessionId, string Game, Pool Pool, QueuePlace Queue) : GateSession;

/// <summary><c>POST .../heartbeat</c>: <c>fenceToken</c> for <c>vip</c> only.</summary>
public sealed record GateHeartbeat([property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ulong? FenceToken);

/// <summary>Whether the game plane still serves the session (<c>open = false</c>: tell the hook to go offline).</summary>
public sealed record GamePlane(bool Open, string Until, string? Reason);

public sealed record GateHeartbeatResponse(Lease? Lease, GateTicket Ticket, GamePlane GamePlane);

public sealed record GateTicketResponse(GateTicket Ticket);

/// <summary><c>POST .../leave</c> → 204 (idempotent); <c>reason</c>: game_exit or player.</summary>
public sealed record GateLeave(
    string Reason,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ulong? EndRevision,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ulong? FenceToken);

/// <summary>Progress numbers the server reads from a bundle (SAVES_API.md §2), each nullable.</summary>
public sealed record SaveSummary(long? Credits, long? Xp, long? Level, long? Cars);

/// <summary>Who holds the profile's lease, as the head reports it.</summary>
public sealed record LeaseView(string DeviceLabel, string ExpiresAt, bool Live);

/// <summary><c>GET .../save</c> (SAVES_API.md §4.1), <c>ETag: "&lt;revision&gt;"</c>. An empty line is <c>revision: 0</c> with nulls.</summary>
public sealed record SaveHead(
    ulong Revision,
    string? ContentSha256,
    string? BlobSha256,
    uint? Bytes,
    string? CommittedAt,
    string? Kind,
    string? DeviceLabel,
    SaveSummary? Summary,
    LeaseView? Lease,
    // OD-52 renewal (SAVES_API.md §4.7): while the line is lapsed, the instant its coverage ended; null otherwise
    string? LapsedSince = null,
    // CR-S51 (SAVES_API.md §4.8): where a Hardcore line at revision 0 starts, "seed" or "fresh_start"; absent = seed
    string? Start = null,
    // retention (SAVES_API.md §4.1, §8): false when the head's save file is no longer kept; absent = the file exists
    bool? Bundle = null)
{
    /// <summary>The cloud copy of the head expired (retention): nothing is downloaded or installed; the local save stays.</summary>
    public bool IsCloudExpired() => Bundle == false && Revision > 0;
}

/// <summary><c>SaveHead.start</c>'s values (CR-S51).</summary>
public static class SaveStarts
{
    public const string Seed = "seed";
    public const string FreshStart = "fresh_start";
}

/// <summary>One revision of the history (TypeScript <c>Revision</c>).</summary>
public sealed record SaveRevision(
    ulong Revision,
    string ContentSha256,
    uint Bytes,
    string CommittedAt,
    string Kind,
    string? DeviceLabel,
    SaveSummary Summary,
    ulong? RestoredFrom);

/// <summary><c>GET .../save/revisions</c>: newest first; <c>next</c> is the <c>before</c> of the next page.</summary>
public sealed record RevisionPage(IReadOnlyList<SaveRevision> Items, ulong? Next);

/// <summary><c>PUT .../save</c> → 201 (or 200 with <c>X-Fi-Replayed: true</c>): the new head.</summary>
public sealed record CommitResponse(ulong Revision, string ContentSha256, string BlobSha256, string CommittedAt, SaveSummary Summary);

/// <summary><c>{"code", "message", "details"}</c>: clients branch on <c>code</c>.</summary>
public sealed record ErrorBody(string Code, string Message, System.Text.Json.JsonElement? Details);
