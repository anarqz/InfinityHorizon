// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>
/// OD-215: every authenticated <c>/launcher/v1</c> request is limited per account (10/s, burst 50) and writes also 60/min; past either the
/// server answers 429 <c>REQUEST.RATE_LIMITED</c> with <c>Retry-After</c> and <c>details.limit</c> = <c>launcher.account</c> or
/// <c>launcher.account_write</c>. Deletions keep their own 3 an hour. This reads such a refusal into the string key that says it, with the
/// seconds to wait (null when the server gave none).
/// </summary>
public static class RateLimits
{
    public const string Account = "launcher.account";
    public const string AccountWrite = "launcher.account_write";

    /// <summary>The <c>details.limit</c> of a refusal, or null.</summary>
    public static string? Limit(JsonElement? details) => ProfileSeed.Detail(details, "limit");

    /// <summary>The OD-215 per-account limits (not the deletion's 3 an hour).</summary>
    public static bool IsAccountLimit(JsonElement? details) => Limit(details) is Account or AccountWrite;

    /// <summary>The key and seconds of a 429 <c>REQUEST.RATE_LIMITED</c>; null for any other error.</summary>
    public static (string Key, int? Seconds)? Describe(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        if (error.Code != ErrorCodes.RequestRateLimited) return null;
        return Describe(Limit(error.Details), error.RetryAfter);
    }

    public static (string Key, int? Seconds) Describe(string? limit, TimeSpan? retryAfter)
    {
        int? seconds = retryAfter is { } wait && wait > TimeSpan.Zero ? Math.Max(1, (int)Math.Ceiling(wait.TotalSeconds)) : null;
        var kind = limit switch
        {
            AccountWrite => "rate.writes",
            Account => "rate.requests",
            _ => "rate.generic",
        };
        return (seconds is null ? kind : kind + "_wait", seconds);
    }

    /// <summary>Every key <see cref="Describe(string?, TimeSpan?)"/> gives (each in en and pt-BR).</summary>
    public static IReadOnlyList<string> TextKeys { get; } =
        ["rate.writes", "rate.writes_wait", "rate.requests", "rate.requests_wait", "rate.generic", "rate.generic_wait"];
}
