// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;
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

    /// <summary>OD-211 (CORE_API §4.2a, CR-S102): the profile's page on the website; absent when it is not listed (CR-S83): no link then.</summary>
    public string? ProfileUrl { get; init; }
}

/// <summary>OD-211 (CORE_API §4.2a): a friend account's linked Discord; absent when it has none.</summary>
public sealed record FriendDiscord
{
    public required string Username { get; init; }

    public string? GlobalName { get; init; }

    public required string AvatarUrl { get; init; }
}

/// <summary>OD-211 (CORE_API §4.2a): what a friend does in its world (<c>activity</c>: freeroam | carmeet | roadtrip | race).</summary>
public sealed record FriendDoing
{
    public required string Activity { get; init; }

    public required bool Private { get; init; }

    /// <summary>Only where the hook reports a race in progress.</summary>
    public bool? Racing { get; init; }
}

/// <summary>OD-211 (CORE_API §4.2a): a friend whose account started a play session since the caller's previous invites poll.</summary>
public sealed record FriendOnline
{
    public required string FriendId { get; init; }

    public required string Gamertag { get; init; }

    public required string Game { get; init; }

    public required string Mode { get; init; }

    public required string Since { get; init; }
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


    /// <summary>
    /// OD-190 (CORE_API §4.1): only when <see cref="WorldId"/> is an official, non-group world the friend holds a live seat in, its
    /// activity (<c>freeroam</c>, <c>carmeet</c>, <c>roadtrip</c>); "Join friend" is then <c>POST /mm/v1/join {worldId}</c>.
    /// </summary>
    public string? OfficialActivity { get; init; }

    /// <summary>OD-211 (CORE_API §4.2a): the world's activity, whether it is private, and whether a race is in progress; absent on older servers.</summary>
    public FriendDoing? Doing { get; init; }
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

    /// <summary>OD-211 (CORE_API §4.2a): the friend account's creation (an instant); absent on older servers.</summary>
    public string? AccountSince { get; init; }

    /// <summary>OD-211 (CORE_API §4.2a): only when the account linked Discord.</summary>
    public FriendDiscord? Discord { get; init; }
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

    /// <summary>OD-204 (MATCHMAKING_API §2.10, CR-S96): an invite to a private official world; omitted elsewhere.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public bool? Private { get; init; }

    /// <summary>OD-204: the private world's game session (the owner's created session); omitted elsewhere.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public GameSessionReference? Session { get; init; }
}

/// <summary>OD-204 (MATCHMAKING_API §2.10): <c>{scid, templateName, name}</c>, a game session as the game names it.</summary>
public sealed record GameSessionReference
{
    public required string Scid { get; init; }

    public required string TemplateName { get; init; }

    public required string Name { get; init; }
}

/// <summary><c>GET /launcher/v1/invites[?asXuid=]</c> → 200 (polled about every 10 s): newest first, at most 20.</summary>
public sealed record InviteList
{
    public required IReadOnlyList<InviteReceived> Items { get; init; }

    public required string GeneratedAt { get; init; }

    /// <summary>OD-211 (CORE_API §4.2a, CR-S102): friends who came online since the previous poll; absent on older servers and on the first poll.</summary>
    public IReadOnlyList<FriendOnline>? Online { get; init; }
}

/// <summary>
/// <c>POST /launcher/v1/friends</c> body: exactly one of <see cref="Gamertag"/> and <see cref="Xuid"/> (CR-S89, OD-193: the game's
/// PLAYER ACTIONS names the player by XUID).
/// </summary>
public sealed record FriendAdd
{
    public required string Game { get; init; }

    /// <summary>The plain name (OD-120).</summary>
    [System.Text.Json.Serialization.JsonIgnore(Condition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull)]
    public string? Gamertag { get; init; }

    /// <summary>A decimal XUID of any profile of the other account in <see cref="Game"/>.</summary>
    [System.Text.Json.Serialization.JsonIgnore(Condition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull)]
    public string? Xuid { get; init; }
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

/// <summary>CR-S68 (CORE_API.md §4.4): a voice mute's action.</summary>
public static class VoiceMuteActions
{
    public const string Muted = "muted", Unmuted = "unmuted";
}

/// <summary>
/// <c>POST /launcher/v1/voice/mutes</c> body (CR-S68): a mute from the overlay, idempotent by state (no <c>Idempotency-Key</c>).
/// The XUID never reaches a log.
/// </summary>
public sealed record VoiceMuteReport
{
    public required string Xuid { get; init; }

    /// <summary><see cref="VoiceMuteActions"/>.</summary>
    public required string Action { get; init; }

    public required string WorldId { get; init; }

    public override string ToString() => $"VoiceMuteReport {{ {Action} }}";
}

/// <summary><c>POST /launcher/v1/voice/mutes</c> → 202.</summary>
public sealed record VoiceMuteAccepted
{
    public required string Xuid { get; init; }

    public required string Action { get; init; }
}

/// <summary>
/// <c>GET /core/v1/me/notices</c> item (CORE_API.md §2.3). <c>Text</c> is the server's English line; the launcher localizes by
/// <c>Kind</c> where it knows it (<c>voice_blocked</c>). <c>Until</c> (CR-S68): the end of what the notice is about.
/// </summary>
public sealed record AccountNotice
{
    public required string Kind { get; init; }

    public required string Level { get; init; }

    public required string Text { get; init; }

    public required string Since { get; init; }

    public string? Game { get; init; }

    public int? Position { get; init; }

    public string? Until { get; init; }
}

/// <summary><c>GET /core/v1/me/notices</c> → 200.</summary>
public sealed record AccountNoticeList
{
    public required IReadOnlyList<AccountNotice> Items { get; init; }
}

/// <summary>The account notice kinds the launcher acts on (CORE_API.md §2.3).</summary>
public static class AccountNoticeKinds
{
    /// <summary>CR-S68: a moderator voice ban or a community mute is in force until <see cref="AccountNotice.Until"/>.</summary>
    public const string VoiceBlocked = "voice_blocked";
}