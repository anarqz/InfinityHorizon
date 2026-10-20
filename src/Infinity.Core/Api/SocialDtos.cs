// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Matchmaking;

namespace Infinity.Core.Api;

// Core's social reads the launcher polls (CORE_API.md §4.1 friends with presence, §4.2 the invites
// inbox), mirrored from crates/fi-protocol/src/core.rs and held to packages/contracts/src/core.ts by
// ContractParityTests. Never add a field the contract does not have.

/// <summary>A profile of a friend account (<c>mode</c>: hardcore | noob).</summary>
public sealed record FriendProfile
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    public required string Game { get; init; }

    public required string Mode { get; init; }
}

/// <summary>The profile a friend plays now, or played last (<c>state</c>: offline | in_game | in_world).</summary>
public sealed record FriendPlaying
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    public required string Game { get; init; }

    public required string Mode { get; init; }

    public required string State { get; init; }

    /// <summary><c>in_world</c>: the world it hosts or holds a seat in.</summary>
    public string? WorldId { get; init; }

    /// <summary>Only with <c>?asXuid=</c>, only <c>in_world</c>: a join of that profile would be granted now, the code aside.</summary>
    public bool? Joinable { get; init; }
}

/// <summary>The friend's newest live invite to the caller.</summary>
public sealed record FriendInvite
{
    public required string Id { get; init; }

    public required string WorldId { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary>A friend account (OD-55): <c>id</c> is its handle; <c>tier</c> free | vip now (OD-56).</summary>
public sealed record Friend
{
    public required string Id { get; init; }

    public required IReadOnlyList<FriendProfile> Profiles { get; init; }

    public required bool Favorite { get; init; }

    public required bool Online { get; init; }

    public required string Since { get; init; }

    public required string Tier { get; init; }

    public FriendPlaying? Playing { get; init; }

    public FriendInvite? Invite { get; init; }
}

/// <summary>A live friend request: the other account.</summary>
public sealed record FriendRequest
{
    public required string Id { get; init; }

    public required IReadOnlyList<FriendProfile> Profiles { get; init; }

    public required string Since { get; init; }
}

/// <summary><c>GET /launcher/v1/friends[?asXuid=]</c> → 200, in the presence order (in_world, in_game, inviting, the rest).</summary>
public sealed record FriendList
{
    public required IReadOnlyList<Friend> Friends { get; init; }

    public required IReadOnlyList<FriendRequest> Incoming { get; init; }

    public required IReadOnlyList<FriendRequest> Outgoing { get; init; }
}

/// <summary>Who sent an inbox item: the host's profile (an invite) or the asker's (a request).</summary>
public sealed record InviteFrom
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    public required string Mode { get; init; }

    public required string Tier { get; init; }
}

/// <summary>
/// A pending invite to the caller's account (<c>kind: invite</c>: accept by joining) or a request
/// to join the caller's world (<c>kind: request</c>: accept with
/// <c>POST /mm/v1/worlds/{worldId}/requests/{id}/accept</c>).
/// </summary>
public sealed record InviteReceived
{
    public required string Id { get; init; }

    /// <summary>One of <see cref="OfferKinds"/>.</summary>
    public required string Kind { get; init; }

    /// <summary>As the caller's current profile sees it.</summary>
    public required WorldView World { get; init; }

    public required InviteFrom From { get; init; }

    /// <summary><c>false</c>: switch to a profile of the world's mode to join; absent: no current profile.</summary>
    public bool? SameMode { get; init; }

    public required string CreatedAt { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary><c>GET /launcher/v1/invites[?asXuid=]</c> → 200 (polled about every 10 s): newest first, at most 20.</summary>
public sealed record InviteList
{
    public required IReadOnlyList<InviteReceived> Items { get; init; }

    public required string GeneratedAt { get; init; }
}

/// <summary><c>POST /launcher/v1/friends</c> body.</summary>
public sealed record FriendAdd
{
    public required string Game { get; init; }

    public required string Gamertag { get; init; }
}

/// <summary><c>POST /launcher/v1/friends</c> → 200: <c>requested</c> (with the request) or <c>friends</c> (with the friend).</summary>
public sealed record FriendAddResult
{
    public required string State { get; init; }

    public FriendRequest? Request { get; init; }

    public Friend? Friend { get; init; }
}

/// <summary><c>POST /launcher/v1/reports</c> body (CR-L25, OD-63; no free text).</summary>
public sealed record GameReport
{
    public required string Game { get; init; }

    public required string Xuid { get; init; }

    /// <summary><c>cheating</c>, <c>abuse</c> or <c>other</c>.</summary>
    public required string Reason { get; init; }

    [System.Text.Json.Serialization.JsonIgnore(Condition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull)]
    public string? WorldId { get; init; }
}

/// <summary><c>POST /launcher/v1/reports</c> → 202.</summary>
public sealed record GameReportResult
{
    public required string Id { get; init; }
}