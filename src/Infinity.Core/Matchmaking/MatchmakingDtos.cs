// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;

namespace Infinity.Core.Matchmaking;

// The wire bodies of /mm/v1 (docs/contracts/MATCHMAKING_API.md §2-§3.4), mirrored from
// crates/fi-protocol/src/matchmaking.rs field for field and held to packages/contracts/src/matchmaking.ts
// by Infinity.Core.Tests ContractParityTests (the TypeScript mirror is fi-protocol's own parity
// target). Never add a field the contract does not have. Wire enums stay strings (as in fi-protocol);
// their values are the constants of MatchmakingValues.cs. Instants are the wire strings.
// `required` members must be in every answer (System.Text.Json refuses a body without them);
// optional ones are nullable and, on request bodies, left out when null (the contract's "absent").

/// <summary>The host of a world, as the list shows it (<c>tier</c>: <c>free</c> | <c>vip</c>).</summary>
public sealed record WorldHost
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    public required string Tier { get; init; }
}

/// <summary>A world's common fields (TypeScript <c>World</c>); never an IP address.</summary>
public record World
{
    public required string Id { get; init; }

    public required string Game { get; init; }

    public required string Name { get; init; }

    public required WorldHost Host { get; init; }

    /// <summary>The host's tier: <c>free</c> | <c>supporter</c> (who may join is <see cref="WorldView.Audience"/>).</summary>
    public required string Kind { get; init; }

    public required string Mode { get; init; }

    public required uint Players { get; init; }

    public required uint MaxPlayers { get; init; }

    public string? Region { get; init; }

    /// <summary>One of <see cref="PingClasses"/>; optional in both mirrors (the contract's §2.1 writes it unconditionally).</summary>
    public string? PingClass { get; init; }

    public string? Message { get; init; }

    /// <summary>Deprecated alias of <see cref="WorldView.NeedsCode"/>.</summary>
    public required bool Locked { get; init; }

    public required bool Races { get; init; }

    public required string Since { get; init; }
}

/// <summary>A world as one caller sees it (MATCHMAKING_API.md §2.1).</summary>
public record WorldView : World
{
    /// <summary>One of <see cref="WorldAudiences"/>.</summary>
    public required string Audience { get; init; }

    public required bool NeedsCode { get; init; }

    public required uint OpenSeats { get; init; }

    /// <summary>The estimated caller → host round trip, rounded to 10 ms; absent when unlocated.</summary>
    public uint? RttMs { get; init; }

    /// <summary>The host's latency limit: 60, 100 or 150 (<see cref="MaxRttSteps"/>).</summary>
    public uint? MaxRttMs { get; init; }

    public required bool CanJoin { get; init; }

    /// <summary>One of <see cref="WorldWhy"/>, only when <see cref="CanJoin"/> is false.</summary>
    public string? Why { get; init; }

    public required bool FriendHost { get; init; }

    public required bool Invited { get; init; }

    /// <summary>Only to the host of a <c>friends</c> or <c>private</c> world: 8 digits.</summary>
    public string? Code { get; init; }

    /// <summary>
    /// OD-109: hosted by a dedicated lobby (a launcher with no game). Its host is not a player:
    /// <see cref="World.Players"/> and <see cref="World.MaxPlayers"/> count guests only. Servers without
    /// dedicated support omit it, so it is not <c>required</c> here (absent reads as false).
    /// </summary>
    public bool Dedicated { get; init; }

    /// <summary>OD-112 (§2.7): the world has a mod set. Absent on older servers (reads as vanilla).</summary>
    public bool Modded { get; init; }

    /// <summary>OD-112: the host's mod set, sorted by id; empty for a vanilla world (and from older servers).</summary>
    public IReadOnlyList<ModEntry> Mods { get; init; } = [];

    /// <summary>OD-112: the players embedded on the list's rows, one world and by-code (≤ 12, host first unless dedicated); absent elsewhere.</summary>
    public IReadOnlyList<ListMember>? Members { get; init; }

    /// <summary>CR-S57 (§5b): <c>relay</c> = the lobby was born on a relay; absent on older servers (reads as direct).</summary>
    public string Transport { get; init; } = Transports.Direct;

    /// <summary>The world's relay, only when <see cref="Transport"/> is <c>relay</c> (shown "via &lt;name&gt;").</summary>
    public WorldRelay? Relay { get; init; }
}

/// <summary>CR-S57: how a pair's datagrams travel.</summary>
public static class Transports
{
    public const string Direct = "direct", Relay = "relay";
}

/// <summary>CR-S57: a launcher's NAT test.</summary>
public static class NatTypes
{
    public const string Open = "open", Restricted = "restricted", Symmetric = "symmetric";
}

/// <summary>A world's relay (CR-S57): its announce key and name (untrusted display text).</summary>
public sealed record WorldRelay
{
    public required string PublicKey { get; init; }

    public required string Name { get; init; }
}

/// <summary>CR-S57 signed <c>relay</c> claim: the relay both sides meet at (its key, host, binding and rendezvous ports).</summary>
public sealed record RelayGrantRelay
{
    [JsonPropertyName("key")] public required string Key { get; init; }
    [JsonPropertyName("addr")] public required string Addr { get; init; }
    [JsonPropertyName("binding")] public required ushort Binding { get; init; }
    [JsonPropertyName("port")] public required ushort Port { get; init; }
}

/// <summary>
/// One mod of a mod set (OD-112, MATCHMAKING_API.md §2.7): <c>id</c> the manifest's multiplayer
/// <c>compatibilityId</c>, <c>version</c> its revision (decimal), <c>fingerprint</c> 64 lowercase hex
/// digits (<c>infinity-mod-compatibility-v1</c>). A set is sorted by id, unique, at most 32.
/// </summary>
public sealed record ModEntry
{
    public required string Id { get; init; }

    public required string Version { get; init; }

    public required string Fingerprint { get; init; }
}

/// <summary>A player embedded in a listed world (OD-112): no XUID, no relation (<c>tier</c> free | vip; <c>role</c> host | seated | reserved).</summary>
public sealed record ListMember
{
    public required string Gamertag { get; init; }

    public required string Tier { get; init; }

    public required string Role { get; init; }
}

/// <summary><c>not_allowed {reason: "mods"}</c>: the world's set (<c>expected</c>) and the caller's (<c>actual</c>).</summary>
public sealed record ModsMismatch
{
    public required IReadOnlyList<ModEntry> Expected { get; init; }

    public required IReadOnlyList<ModEntry> Actual { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/requests</c> optional body (OD-112; absent: vanilla).</summary>
public sealed record RequestToJoin
{
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }
}

public sealed record ListViewer
{
    public required bool Located { get; init; }

    public string? Region { get; init; }
}

/// <summary><c>GET /mm/v1/worlds?filter=&amp;notFull=</c> → 200 (OD-58: at most 5 items and 3 locked).</summary>
public sealed record WorldList
{
    public required string Filter { get; init; }

    public required IReadOnlyList<WorldView> Items { get; init; }

    /// <summary>Always empty since OD-129; an older server listed up to 3 Supporter-only worlds here for a free ticket (<c>why: "tier"</c>).</summary>
    public required IReadOnlyList<WorldView> Locked { get; init; }

    public required string GeneratedAt { get; init; }

    public required ListViewer Viewer { get; init; }
}

/// <summary>The host's relay UDP port and up to 4 private LAN addresses (TypeScript <c>AnnouncePort</c>).</summary>
public sealed record AnnouncePort
{
    public required ushort Port { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<string>? Lan { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public AnnounceExternalPort? External { get; init; }
}

public sealed record AnnounceExternalPort
{
    public required ushort Port { get; init; }
    public required string Source { get; init; }
}

/// <summary><c>PUT /mm/v1/worlds/{worldId}</c> body (§2.5): open or change the host's world.</summary>
public sealed record WorldSettings
{
    public required string Name { get; init; }

    public required string Mode { get; init; }

    public required uint MaxPlayers { get; init; }

    public required string Audience { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public uint? MaxRttMs { get; init; }

    public required bool Races { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Message { get; init; }

    /// <summary>The XUIDs that may join (≤ 256), for every host (OD-129).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<string>? Allow { get; init; }

    public required AnnouncePort Endpoint { get; init; }

    /// <summary>OD-112: the host's mod set (null: vanilla, left out); fixed once the world opens (another set is 409 <c>PORTAL.CONFLICT {reason: "mods"}</c>).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }

    /// <summary>CR-S57: the host's NAT test, read when the world opens (<see cref="NatTypes"/>).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Nat { get; init; }

    /// <summary>CR-S57: measured relay round trips (announce key → ms, ≤ 16, 0-5000).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, int>? Relays { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/announce</c> body, every 15 s: <c>peers</c> are who is in the world now (≤ 11).</summary>
public sealed record Heartbeat
{
    public required IReadOnlyList<string> Peers { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public AnnouncePort? Endpoint { get; init; }
}

/// <summary>The host's answer to <c>PUT</c> and the announce: the world as its host sees it (<c>code</c> included) and the punches.</summary>
public sealed record WorldAnnounced : WorldView
{
    public required IReadOnlyList<Punch> Punches { get; init; }
}

/// <summary>Base64 of the canonical claims and the base64 Ed25519 signature over <c>"fi-relay-grant-v1\n"</c> ‖ claims.</summary>
public sealed record RelayGrant
{
    public required string Claims { get; init; }

    public required string Signature { get; init; }
    public RelayUdpProof? UdpProof { get; init; }
    public override string ToString() => "RelayGrant { redacted }";
}

/// <summary>Grant-scoped TLS-only secret; never include in state, logging or UDP.</summary>
public sealed record RelayUdpProof
{
    public required uint Version { get; init; }
    public required string Key { get; init; }
    public override string ToString() => "RelayUdpProof { redacted }";
    internal byte[]? Decode()
    {
        if (Version != 1 || Key is not { Length: 44 }) return null;
        try { var bytes = Convert.FromBase64String(Key); return bytes.Length == 32 && Convert.ToBase64String(bytes) == Key ? bytes : null; }
        catch (FormatException) { return null; }
    }
}

public sealed record PeerEndpoint
{
    public required string Ip { get; init; }

    public required ushort Port { get; init; }
}

/// <summary>One grant the host's relay must allow, or drop (<c>kicked</c>).</summary>
public sealed record Punch
{
    public NetworkHints? Candidates { get; init; }
    public required string GrantId { get; init; }

    public required string Xuid { get; init; }

    public PeerEndpoint? Endpoint { get; init; }

    public required RelayGrant Relay { get; init; }

    public required bool Kicked { get; init; }

    /// <summary>CR-S57: <c>relay</c> = hold a relay leg for this grant; absent on older servers (direct).</summary>
    public string Transport { get; init; } = Transports.Direct;
}

/// <summary>Existing GET /mm/v1/punches response (fi_protocol::matchmaking::Punches).</summary>
public sealed record Punches
{
    public required IReadOnlyList<Punch> Items { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/moderation</c> body (<c>until</c>: <c>ban</c> only).</summary>
public sealed record Moderation
{
    /// <summary>One of <see cref="ModerationActions"/>.</summary>
    public required string Action { get; init; }

    public required string Xuid { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Until { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Reason { get; init; }
}

public sealed record ModerationResult
{
    public required string Action { get; init; }

    public required string Xuid { get; init; }

    public string? Until { get; init; }
}

/// <summary><c>POST /mm/v1/join</c> body: a world id, a code (8 digits), or both.</summary>
public sealed record JoinRequest
{
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? WorldId { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Code { get; init; }

    /// <summary>OD-112: the caller's mod set (null: vanilla, left out; every launcher up to 1.0.8 sends none).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }

    /// <summary>CR-S57: the joiner's NAT test.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Nat { get; init; }

    /// <summary>CR-S57: the joiner's measured relay round trips.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, int>? Relays { get; init; }
}

/// <summary>A reserved seat (<c>expiresAt</c> ≤ 60 s away, renewable up to <c>holdUntil</c>).</summary>
public sealed record Grant
{
    public required string Id { get; init; }

    public required string WorldId { get; init; }

    public required string ExpiresAt { get; init; }

    public required string HoldUntil { get; init; }

    public required RelayGrant Relay { get; init; }

    /// <summary>CR-S57: the joiner's expected transport now; absent on older servers (direct).</summary>
    public string Transport { get; init; } = Transports.Direct;
}

/// <summary>Seats of a full world.</summary>
public sealed record WorldSeats
{
    public required uint Players { get; init; }

    public required uint MaxPlayers { get; init; }
}

/// <summary>The live seat that locks the caller (follow it, or leave it with <c>DELETE /mm/v1/seat</c>).</summary>
public sealed record LockedSeat
{
    public required string WorldId { get; init; }

    public required string GrantId { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary>With <c>not_allowed {reason: "latency"}</c>.</summary>
public sealed record LatencyRefusal
{
    public required uint RttMs { get; init; }

    public required uint MaxRttMs { get; init; }
}

/// <summary>
/// <c>POST /mm/v1/join</c> → 200, or 429 with <c>status: "rate_limited"</c>. One shape for every
/// status (as both mirrors have it): <c>granted</c> → <see cref="Grant"/>, <c>full</c> →
/// <see cref="World"/>, <c>locked</c> → <see cref="Locked"/>, <c>not_allowed</c> →
/// <see cref="Reason"/> (and <see cref="Latency"/>), <c>rate_limited</c> → <see cref="RetryAfterSeconds"/>.
/// </summary>
public sealed record JoinAnswer
{
    /// <summary>One of <see cref="JoinStatuses"/>.</summary>
    public required string Status { get; init; }

    public Grant? Grant { get; init; }

    public WorldSeats? World { get; init; }

    public LockedSeat? Locked { get; init; }

    /// <summary>One of <see cref="NotAllowedReasons"/>.</summary>
    public string? Reason { get; init; }

    public LatencyRefusal? Latency { get; init; }

    /// <summary>With <c>not_allowed {reason: "mods"}</c> (OD-112).</summary>
    public ModsMismatch? Details { get; init; }

    public uint? RetryAfterSeconds { get; init; }
}

/// <summary>The caller's live seat, or the world it hosts (<c>state</c>: one of <see cref="SeatStates"/>).</summary>
public sealed record Seat
{
    public required string State { get; init; }

    public required string WorldId { get; init; }

    public required WorldView World { get; init; }

    /// <summary>Reserved and seated.</summary>
    public string? GrantId { get; init; }

    /// <summary>Reserved.</summary>
    public string? ExpiresAt { get; init; }

    /// <summary>Reserved.</summary>
    public string? HoldUntil { get; init; }

    /// <summary>Reserved and seated: role <c>joiner</c>, fresh (≤ 60 s).</summary>
    public RelayGrant? Relay { get; init; }

    /// <summary>CR-S57, reserved and seated: the joiner's expected transport now.</summary>
    public string? Transport { get; init; }
}

/// <summary>How the caller's last seat ended (<c>reason</c>: one of <see cref="SeatEnds"/>).</summary>
public sealed record SeatEnded
{
    public required string WorldId { get; init; }

    public required string GrantId { get; init; }

    public required string At { get; init; }

    public required string Reason { get; init; }
}

/// <summary>The caller's account's latest request to join (pending, or answered within two minutes).</summary>
public sealed record SeatRequest
{
    public required string Id { get; init; }

    public required string WorldId { get; init; }

    /// <summary>One of <see cref="OfferStates"/>.</summary>
    public required string State { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary><c>GET /mm/v1/seat</c> and <c>POST /mm/v1/seat/renew</c> → 200.</summary>
public sealed record SeatAnswer
{
    /// <summary>The live seat; <c>null</c> on the wire when none.</summary>
    public required Seat? Seat { get; init; }

    public SeatEnded? Ended { get; init; }

    public SeatRequest? Request { get; init; }

    /// <summary>Opt-in atomic roster from GET seat?includeMembers=true; absent on older servers/no seat.</summary>
    public WorldMembers? Members { get; init; }

    public required string GeneratedAt { get; init; }
}

/// <summary><c>POST /mm/v1/seat/renew</c> body.</summary>
public sealed record SeatRenew
{
    public required string GrantId { get; init; }

    /// <summary>CR-S57: <c>relay</c> once the joiner's punch failed and it moved to the grant's relay.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Transport { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/invites</c> body: any XUID of the friend account (a handle works).</summary>
public sealed record InviteRequest
{
    public required string Xuid { get; init; }
}

/// <summary>An invite (<c>…/invites</c>) or a request to join (<c>…/requests</c>): 201, 200 for the pending one.</summary>
public sealed record Invite
{
    public required string Id { get; init; }

    /// <summary>One of <see cref="OfferKinds"/>.</summary>
    public required string Kind { get; init; }

    public required string WorldId { get; init; }

    /// <summary>The handle of the account that answers: the invitee, or the host.</summary>
    public required string To { get; init; }

    /// <summary>One of <see cref="OfferStates"/>.</summary>
    public required string State { get; init; }

    public required string CreatedAt { get; init; }

    public required string ExpiresAt { get; init; }

    /// <summary><c>accepted</c>: the seat it gave.</summary>
    public string? GrantId { get; init; }
}

/// <summary>
/// One player of a world, as its host or a seat holder sees it (CR-L25, OD-63:
/// <c>GET /mm/v1/worlds/{worldId}/members</c>; the host, then seated by arrival, then reserved;
/// no addresses, no account ids). The XUID is an action key only, never shown (OD-61).
/// </summary>
public sealed record WorldMember
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    /// <summary><c>free</c> or <c>vip</c>.</summary>
    public required string Tier { get; init; }

    /// <summary><c>host</c>, <c>seated</c> or <c>reserved</c>.</summary>
    public required string Role { get; init; }

    /// <summary>To the caller's account (OD-55): <c>none</c>, <c>friends</c>, <c>outgoing</c>, <c>incoming</c> or <c>self</c>.</summary>
    public string? Friend { get; init; }

    public string? Since { get; init; }
}

/// <summary>CR-L25 (OD-63) → 200.</summary>
public sealed record WorldMembers
{
    public string? WorldId { get; init; }

    public required IReadOnlyList<WorldMember> Items { get; init; }

    public string? GeneratedAt { get; init; }
}
