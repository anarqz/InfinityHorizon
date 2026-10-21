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
    bool RegistrationEnabled = true);

/// <summary>Identity Platform for the native sign-in (OD-43): <c>emulatorUrl</c> only on local stacks.</summary>
public sealed record IdpConfig(string ApiKey, string ProjectId, string? EmulatorUrl);

/// <summary>The origin of each platform v1 surface (CORE_API.md §2.1): no client hard-codes one.</summary>
public sealed record ServiceMap(string Core, string Gate, string Saves, string Matchmaking);

public sealed record GameInfo(string Id, string Title, string MinHook, string MinLauncher, bool Accepting, string? MaintenanceUntil);

public sealed record CheckpointPolicy(bool Enabled, uint MinIntervalSecs, uint MaxCommitsPerHour);

/// <summary>A launcher install as it introduces itself (<c>platform</c>: windows, proton, linux).</summary>
public sealed record Device(string DeviceId, string Label, string Platform);

public sealed record BoundDevice(string DeviceId, string Label, string Platform, string BoundAt, string LastSeenAt);

/// <summary>A game profile (<c>xuid</c> decimal, <c>gamertag</c> <c>Name#12345</c>, <c>mode</c> hardcore or noob).</summary>
public sealed record Profile(
    string ProfileId,
    string Game,
    string Xuid,
    string Gamertag,
    string Handle,
    string Mode,
    string Status,
    string CreatedAt,
    string? LastPlayedAt);

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

/// <summary><c>POST /gate/v1/sessions</c> (SAVES_API.md §3.1's body).</summary>
public sealed record PlaySessionCreate(
    string SessionId,
    string Game,
    string Xuid,
    string Nonce,
    string LauncherVersion,
    string HookSha256,
    string CarDbSha256,
    ulong BaseRevision,
    bool TakeOver);

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
    string? LapsedSince = null);

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
