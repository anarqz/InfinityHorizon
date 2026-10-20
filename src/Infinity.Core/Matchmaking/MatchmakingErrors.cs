// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Matchmaking;

/// <summary>The error codes of MATCHMAKING_API.md §8 and CORE_API.md §4.2 the launcher branches on (parity-tested against errors.ts).</summary>
public static class MatchmakingErrorCodes
{
    public const string WorldNotFound = "MM.WORLD_NOT_FOUND";
    public const string NotHost = "MM.NOT_HOST";
    public const string AlreadyHosting = "MM.ALREADY_HOSTING";
    public const string GrantInvalid = "MM.GRANT_INVALID";
    public const string VipRequired = "PORTAL.VIP_REQUIRED";
    public const string NotFound = "PORTAL.NOT_FOUND";
    public const string Conflict = "PORTAL.CONFLICT";
    public const string ModeMismatch = "PORTAL.MODE_MISMATCH";
    public const string WrongPool = "GAME.WRONG_POOL";
    public const string Invalid = "REQUEST.INVALID";
    public const string RateLimited = "REQUEST.RATE_LIMITED";
    public const string TokenInvalid = "AUTH.TOKEN_INVALID";
    public const string TokenExpired = "AUTH.TOKEN_EXPIRED";
    public const string SessionExpired = "AUTH.SESSION_EXPIRED";
    public const string DeletionPending = "ACCOUNTS.DELETION_PENDING";

}

/// <summary><c>PORTAL.CONFLICT {reason}</c> values of §8.</summary>
public static class ConflictReasons
{
    public const string NoCode = "no_code";
    public const string Full = "full";
    public const string PendingLimit = "pending_limit";
    public const string OwnWorld = "own_world";
    public const string Seated = "seated";
    public const string HostNotSupporter = "host_not_supporter";
    public const string Ended = "ended";
    public const string SeatedElsewhere = "seated_elsewhere";
    public const string NotAllowed = "not_allowed";

    public static IReadOnlyList<string> All { get; } = [NoCode, Full, PendingLimit, OwnWorld, Seated, HostNotSupporter, Ended, SeatedElsewhere, NotAllowed];
}

/// <summary><c>PORTAL.VIP_REQUIRED {capability}</c> values of §8.</summary>
public static class Capabilities
{
    public const string WorldBan = "world_ban";
    public const string WorldAudience = "world_audience";
    public const string SocialFriends = "social.friends";
}

/// <summary>What an error means to the launcher, from its <c>code</c>.</summary>
public enum MmErrorKind
{
    /// <summary>A code this client does not branch on (or a non-contract body: <c>HTTP.&lt;status&gt;</c>).</summary>
    Other,

    /// <summary><c>MM.WORLD_NOT_FOUND</c>: unknown, closed, private or banned; a host opens a new world with a new id.</summary>
    WorldNotFound,

    /// <summary><c>MM.NOT_HOST</c>: another host's world, or a closed one.</summary>
    NotHost,

    /// <summary><c>MM.ALREADY_HOSTING {worldId}</c>: the profile already hosts <see cref="MmError.WorldId"/>.</summary>
    AlreadyHosting,

    /// <summary><c>MM.GRANT_INVALID</c>.</summary>
    GrantInvalid,

    /// <summary><c>PORTAL.VIP_REQUIRED {capability}</c>: an Infinity Supporter option (<see cref="MmError.Capability"/>).</summary>
    SupporterRequired,

    /// <summary><c>PORTAL.NOT_FOUND</c>: one answer for anyone but a Supporter friend, or an unknown request.</summary>
    NotFound,

    /// <summary><c>PORTAL.CONFLICT {reason}</c> (<see cref="MmError.Reason"/>, one of <see cref="ConflictReasons"/>).</summary>
    Conflict,

    /// <summary><c>PORTAL.MODE_MISMATCH</c>: a world has its host's mode.</summary>
    ModeMismatch,

    /// <summary><c>GAME.WRONG_POOL</c>: a ticket of another game or pool.</summary>
    WrongPool,

    /// <summary><c>REQUEST.INVALID {field}</c> (<see cref="MmError.Field"/>).</summary>
    Invalid,

    /// <summary><c>REQUEST.RATE_LIMITED</c>: wait <see cref="MmError.RetryAfter"/>.</summary>
    RateLimited,

    /// <summary>The credential was refused (<c>AUTH.*</c>, any 401): refresh the ticket or the access token.</summary>
    Unauthorized,

    /// <summary>A 5xx: transient.</summary>
    Unavailable,
}

/// <summary>A refused request, typed from the contract's error body (<c>{code, message, details}</c>).</summary>
public sealed record MmError(
    HttpStatusCode Status,
    string Code,
    string Message,
    MmErrorKind Kind,
    JsonElement? Details,
    TimeSpan? RetryAfter)
{
    /// <summary><c>details.worldId</c> (<c>MM.ALREADY_HOSTING</c>).</summary>
    public string? WorldId => Detail("worldId");

    /// <summary><c>details.capability</c> (<c>PORTAL.VIP_REQUIRED</c>).</summary>
    public string? Capability => Detail("capability");

    /// <summary><c>details.reason</c> (<c>PORTAL.CONFLICT</c>).</summary>
    public string? Reason => Detail("reason");

    /// <summary><c>details.field</c> (<c>REQUEST.INVALID</c>).</summary>
    public string? Field => Detail("field");

    /// <summary><c>details.game</c> (<c>GAME.WRONG_POOL</c>).</summary>
    public string? Game => Detail("game");

    private string? Detail(string name) =>
        Details is { ValueKind: JsonValueKind.Object } d && d.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String
            ? v.GetString()
            : null;

    public static MmErrorKind KindOf(HttpStatusCode status, string code) => code switch
    {
        MatchmakingErrorCodes.WorldNotFound => MmErrorKind.WorldNotFound,
        MatchmakingErrorCodes.NotHost => MmErrorKind.NotHost,
        MatchmakingErrorCodes.AlreadyHosting => MmErrorKind.AlreadyHosting,
        MatchmakingErrorCodes.GrantInvalid => MmErrorKind.GrantInvalid,
        MatchmakingErrorCodes.VipRequired => MmErrorKind.SupporterRequired,
        MatchmakingErrorCodes.NotFound => MmErrorKind.NotFound,
        MatchmakingErrorCodes.Conflict => MmErrorKind.Conflict,
        MatchmakingErrorCodes.ModeMismatch => MmErrorKind.ModeMismatch,
        MatchmakingErrorCodes.WrongPool => MmErrorKind.WrongPool,
        MatchmakingErrorCodes.Invalid => MmErrorKind.Invalid,
        MatchmakingErrorCodes.RateLimited => MmErrorKind.RateLimited,
        _ when status == HttpStatusCode.Unauthorized || code.StartsWith("AUTH.", StringComparison.Ordinal) => MmErrorKind.Unauthorized,
        _ when status == HttpStatusCode.TooManyRequests => MmErrorKind.RateLimited,
        _ when (int)status >= 500 => MmErrorKind.Unavailable,
        _ => MmErrorKind.Other,
    };

    public static MmError From(HttpStatusCode status, ErrorBody body, TimeSpan? retryAfter)
    {
        ArgumentNullException.ThrowIfNull(body);
        return new MmError(status, body.Code, body.Message, KindOf(status, body.Code), body.Details, retryAfter);
    }

    /// <summary>The same typing for a core answer (<see cref="ApiClient"/> throws <see cref="ApiException"/>).</summary>
    public static MmError From(ApiException exception)
    {
        ArgumentNullException.ThrowIfNull(exception);
        return new MmError(exception.Status, exception.Code, exception.Message, KindOf(exception.Status, exception.Code), exception.Details, null);
    }
}
