// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

// The wire values of /mm/v1 (fi-protocol matchmaking.rs constants; the TypeScript unions of
// matchmaking.ts). ContractParityTests holds each All list equal to its TypeScript union.

/// <summary><c>JoinAnswer.status</c> (a world never queues, OD-58).</summary>
public static class JoinStatuses
{
    public const string Granted = "granted";
    public const string Full = "full";
    public const string Locked = "locked";
    public const string NotAllowed = "not_allowed";
    public const string Closed = "closed";
    public const string RateLimited = "rate_limited";

    public static IReadOnlyList<string> All { get; } = [Granted, Full, Locked, NotAllowed, Closed, RateLimited];
}

/// <summary><c>JoinAnswer.reason</c> with <c>not_allowed</c>.</summary>
public static class NotAllowedReasons
{
    public const string OwnWorld = "own_world";
    public const string Mode = "mode";
    public const string Tier = "tier";
    public const string Host = "host";
    public const string Friends = "friends";

    /// <summary>Retired by OD-181 (CR-S84): only an older server still sends it; read so its refusal keeps today's message.</summary>
    public const string Mods = "mods";
    public const string Latency = "latency";

    /// <summary>The contract's reasons (<see cref="Mods"/> is no longer one).</summary>
    public static IReadOnlyList<string> All { get; } = [OwnWorld, Mode, Tier, Host, Friends, Latency];
}

/// <summary><c>WorldView.why</c>, in the order the server decides them.</summary>
public static class WorldWhy
{
    public const string Own = "own";
    public const string Mode = "mode";
    public const string Tier = "tier";
    public const string Host = "host";
    public const string Friends = "friends";
    public const string Full = "full";
    public const string Latency = "latency";

    /// <summary>OD-149 (§3.9): a pineapple world and a clean caller (only isolated profiles join it).</summary>
    public const string Pool = "pool";

    public static IReadOnlyList<string> All { get; } = [Own, Mode, Pool, Tier, Host, Friends, Full, Latency];
}

/// <summary>
/// Who may play in a world (OD-141, CR-S76, MATCHMAKING_API.md §2.8; OD-210 W14: a private official world's leader changes it,
/// <c>PUT …/modes</c>): one mode or <see cref="Any"/>; <c>WorldView.modes</c> says which.
/// </summary>
public static class WorldModes
{
    public const string Hardcore = "hardcore";
    public const string Noob = "noob";

    /// <summary>Open to Hardcore and Noob profiles.</summary>
    public const string Any = "any";

    public static IReadOnlyList<string> All { get; } = [Hardcore, Noob, Any];

    /// <summary>
    /// The world is open to both modes. An absent or empty <c>modes</c> (a server before CR-S76) reads as the world's
    /// own <c>mode</c> (CR-L43 (4)), so it is not.
    /// </summary>
    public static bool OpenToBoth(WorldView? world) => string.Equals(world?.Modes, Any, StringComparison.Ordinal);

    /// <summary>OD-160 (§5b "Relay by mode"): a Hardcore world admits Hardcore profiles: its modes are <c>hardcore</c> or <c>any</c>.</summary>
    public static bool HardcoreWorld(string? modes) => modes is Hardcore or Any;
}

/// <summary>Invites (OD-56) and requests to join (OD-59).</summary>
public static class OfferKinds
{
    public const string Invite = "invite";
    public const string Request = "request";

    public static IReadOnlyList<string> All { get; } = [Invite, Request];
}

public static class OfferStates
{
    public const string Pending = "pending";
    public const string Accepted = "accepted";
    public const string Declined = "declined";
    public const string Expired = "expired";
    public const string Withdrawn = "withdrawn";

    public static IReadOnlyList<string> All { get; } = [Pending, Accepted, Declined, Expired, Withdrawn];
}

public static class SeatStates
{
    public const string Reserved = "reserved";
    public const string Seated = "seated";

    public static IReadOnlyList<string> All { get; } = [Reserved, Seated];
}

public static class SeatEnds
{
    public const string Expired = "expired";
    public const string Released = "released";
    public const string Kicked = "kicked";
    public const string WorldClosed = "world_closed";

    /// <summary>OD-149 (§3.9): the caller was isolated by the anti-cheat system while seated in a clean world.</summary>
    public const string Pool = "pool";

    public static IReadOnlyList<string> All { get; } = [Expired, Released, Kicked, WorldClosed, Pool];
}

/// <summary>The contract's numbers (matchmaking.ts / fi-protocol constants).</summary>
public static class MatchmakingLimits
{
    public const uint MaxPlayers = 12;
    public const int WorldTtlSecs = 45;
    public const int GrantTtlSecs = 60;
    public const int InviteTtlSecs = 120;
    public const uint MaxPending = 4;
    public const string RelayGrantPrefix = "fi-relay-grant-v1\n";

    /// <summary>Entries of a mod set (OD-112, §2.7).</summary>
    public const int ModsMax = 32;
}
