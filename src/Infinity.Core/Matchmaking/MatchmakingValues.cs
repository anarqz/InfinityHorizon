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
    public const string Code = "code";
    public const string Closed = "closed";
    public const string RateLimited = "rate_limited";

    public static IReadOnlyList<string> All { get; } = [Granted, Full, Locked, NotAllowed, Code, Closed, RateLimited];
}

/// <summary><c>JoinAnswer.reason</c> with <c>not_allowed</c>.</summary>
public static class NotAllowedReasons
{
    public const string OwnWorld = "own_world";
    public const string Mode = "mode";
    public const string Tier = "tier";
    public const string Host = "host";
    public const string Friends = "friends";
    public const string Mods = "mods";
    public const string Latency = "latency";

    public static IReadOnlyList<string> All { get; } = [OwnWorld, Mode, Tier, Host, Friends, Mods, Latency];
}

/// <summary><c>WorldView.why</c>, in the order the server decides them.</summary>
public static class WorldWhy
{
    public const string Own = "own";
    public const string Mode = "mode";
    public const string Tier = "tier";
    public const string Host = "host";
    public const string Friends = "friends";
    public const string Code = "code";
    public const string Full = "full";
    public const string Latency = "latency";

    public static IReadOnlyList<string> All { get; } = [Own, Mode, Tier, Host, Friends, Code, Full, Latency];
}

/// <summary><c>World.kind</c>: the host's tier.</summary>
public static class WorldKinds
{
    public const string Free = "free";
    public const string Supporter = "supporter";

    public static IReadOnlyList<string> All { get; } = [Free, Supporter];
}

/// <summary>Who may join (OD-57); every host may pick any of them (OD-129).</summary>
public static class WorldAudiences
{
    public const string Everyone = "everyone";
    public const string Supporters = "supporters";
    public const string Friends = "friends";
    public const string Private = "private";

    public static IReadOnlyList<string> All { get; } = [Everyone, Supporters, Friends, Private];
}

/// <summary>The list's filter, required (OD-58); every ticket may use each of them (OD-129).</summary>
public static class WorldFilters
{
    public const string Global = "global";
    public const string Near = "near";
    public const string Friends = "friends";

    /// <summary>
    /// CR-L40 (b): every open world of the game, every mode, modded or not (never with <c>modded</c>); an older server
    /// answers 400 <c>{field: "filter"}</c>.
    /// </summary>
    public const string Every = "all";

    public static IReadOnlyList<string> All { get; } = [Global, Near, Friends, Every];
}

/// <summary>Caller → host estimate, best first (§3.5).</summary>
public static class PingClasses
{
    public const string Local = "local";
    public const string Near = "near";
    public const string Far = "far";
    public const string Remote = "remote";
    public const string Unknown = "unknown";

    public static IReadOnlyList<string> All { get; } = [Local, Near, Far, Remote, Unknown];
}

/// <summary>The host's latency limit (OD-59): under 60, 100 or 150 ms; absent: any.</summary>
public static class MaxRttSteps
{
    public static IReadOnlyList<uint> All { get; } = [60, 100, 150];
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
    public const string Hosting = "hosting";

    public static IReadOnlyList<string> All { get; } = [Reserved, Seated, Hosting];
}

public static class SeatEnds
{
    public const string Expired = "expired";
    public const string Released = "released";
    public const string Kicked = "kicked";
    public const string WorldClosed = "world_closed";

    public static IReadOnlyList<string> All { get; } = [Expired, Released, Kicked, WorldClosed];
}

public static class ModerationActions
{
    public const string Kick = "kick";
    public const string Ban = "ban";
    public const string Unban = "unban";

    public static IReadOnlyList<string> All { get; } = [Kick, Ban, Unban];
}

/// <summary>The contract's numbers (matchmaking.ts / fi-protocol constants).</summary>
public static class MatchmakingLimits
{
    public const uint MaxPlayers = 12;
    public const int WorldTtlSecs = 45;
    public const int GrantTtlSecs = 60;
    public const int ListCap = 5;
    public const int LockedCap = 3;
    public const int CodeDigits = 8;
    public const int InviteTtlSecs = 120;
    public const uint MaxPending = 4;
    public const string RelayGrantPrefix = "fi-relay-grant-v1\n";

    /// <summary>Entries of a mod set (OD-112, §2.7).</summary>
    public const int ModsMax = 32;
}

/// <summary>A world code (OD-57): 8 decimal digits on the wire, shown <c>XXXX-XXXX</c>.</summary>
public static class WorldCode
{
    /// <summary>
    /// The 8 digits of a code as typed: 8 ASCII digits, or 9 characters with a hyphen after the
    /// fourth, surrounding whitespace trimmed (fi-protocol <c>normalize_code</c>); <c>null</c> for
    /// anything else, which the client never sends.
    /// </summary>
    public static string? Normalize(string? text)
    {
        var t = text?.Trim();
        if (t is null)
        {
            return null;
        }
        var digits = t.Length switch
        {
            MatchmakingLimits.CodeDigits => t,
            MatchmakingLimits.CodeDigits + 1 when t[4] == '-' => string.Concat(t.AsSpan(0, 4), t.AsSpan(5)),
            _ => null,
        };
        return digits is not null && digits.All(char.IsAsciiDigit) ? digits : null;
    }

    /// <summary>The overlay's <c>XXXX-XXXX</c> of a valid code; <see cref="ArgumentException"/> otherwise.</summary>
    public static string Format(string code)
    {
        var digits = Normalize(code) ?? throw new ArgumentException("not a world code", nameof(code));
        return string.Concat(digits.AsSpan(0, 4), "-", digits.AsSpan(4));
    }
}
