// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>The launcher's quick moderation actions (owner-decisions 2026-10-27 "Moderators kick a player"; CORE_API.md §5).</summary>
public enum ModerationAction
{
    /// <summary><c>POST /core/v1/admin/kicks</c> (MOD, ADM): ends the player's live sessions, optional 0-60 min cooldown.</summary>
    Kick,
    /// <summary><c>POST /core/v1/admin/strikes</c> (MOD, ADM): a manual strike on the profile.</summary>
    Strike,
    /// <summary><c>POST /core/v1/admin/bans</c> (ADM only): the account, for a time or for good.</summary>
    Ban,
}

/// <summary>Who sees the menu: <c>GET /launcher/v1/me</c> <c>role</c>. The server checks the role on every call regardless.</summary>
public static class ModerationRoles
{
    public static bool CanModerate(string? role) => role is "moderator" or "admin";

    public static bool CanBan(string? role) => role is "admin";
}

/// <summary>The request bodies, checked as the server checks them so a bad form never leaves the PC.</summary>
public static class ModerationRequests
{
    public const int ReasonMax = 500;

    /// <summary>The kick's cooldown choices, minutes (the contract allows 0 to 60).</summary>
    public static IReadOnlyList<int> CooldownChoices { get; } = [0, 5, 15, 30, 60];

    /// <summary>The ban's durations, hours; <c>null</c> is permanent.</summary>
    public static IReadOnlyList<int?> BanHours { get; } = [1, 24, 168, 720, null];

    /// <summary>A decimal XUID (1-20 digits, not zero).</summary>
    public static bool IsXuid(string? value) =>
        value is { Length: >= 1 and <= 20 } && value.All(char.IsAsciiDigit)
        && ulong.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) && xuid != 0;

    /// <summary>The i18n key of what is wrong with <paramref name="reason"/>, or null when the server would take it.</summary>
    public static string? ReasonProblem(string? reason)
    {
        var text = reason?.Trim() ?? string.Empty;
        if (text.Length == 0) return "moderation.reason_required";
        if (text.EnumerateRunes().Count() > ReasonMax) return "moderation.reason_too_long";
        return text.Any(char.IsControl) ? "moderation.reason_control" : null;
    }

    public static NewKick Kick(string xuid, string reason, int cooldownMinutes)
    {
        if (!IsXuid(xuid)) throw new ArgumentException("not a decimal XUID", nameof(xuid));
        if (cooldownMinutes is < 0 or > 60) throw new ArgumentOutOfRangeException(nameof(cooldownMinutes));
        return new NewKick(xuid, null, Reason(reason), cooldownMinutes);
    }

    public static NewStrike Strike(string xuid, string reason)
    {
        if (!IsXuid(xuid)) throw new ArgumentException("not a decimal XUID", nameof(xuid));
        return new NewStrike(xuid, Reason(reason));
    }

    /// <summary>A ban of <paramref name="accountId"/> for <paramref name="hours"/> from <paramref name="now"/> (the server's clock); null hours is permanent.</summary>
    public static NewBan Ban(string accountId, int? hours, string reason, DateTimeOffset now)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(accountId);
        if (hours is <= 0) throw new ArgumentOutOfRangeException(nameof(hours));
        var until = hours is { } h ? now.ToUniversalTime().AddHours(h).ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", CultureInfo.InvariantCulture) : null;
        return new NewBan(accountId, until, Reason(reason));
    }

    /// <summary>The ban's typed confirmation: the target's name, case aside.</summary>
    public static bool BanConfirmed(string? typed, string gamertag) =>
        !string.IsNullOrWhiteSpace(gamertag) && string.Equals(typed?.Trim(), gamertag.Trim(), StringComparison.OrdinalIgnoreCase);

    private static string Reason(string reason)
    {
        if (ReasonProblem(reason) is { } problem) throw new ArgumentException(problem, nameof(reason));
        return reason.Trim();
    }
}

/// <summary>What the kicked player's launcher reads (GATE_API.md §2/§4, CR-S62).</summary>
public static class KickCooldown
{
    /// <summary>409 <c>SESSIONS.ENDED {reason: "kicked"}</c> on a heartbeat, poll or ticket: a moderator removed the session.</summary>
    public static bool IsKickedEnd(ApiException error) =>
        error is { Code: ErrorCodes.SessionsEnded } && Detail(error.Details, "reason") == "kicked";

    /// <summary>
    /// How long 403 <c>SESSIONS.KICKED</c> still refuses admission: <c>Retry-After</c> (the seconds left), else <c>details.until</c>
    /// minus <paramref name="now"/>; null when neither says (or the time has passed).
    /// </summary>
    public static TimeSpan? Remaining(ApiException error, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(error);
        if (error.Code != ErrorCodes.SessionsKicked) return null;
        if (error.RetryAfter is { } after && after > TimeSpan.Zero) return after;
        return Detail(error.Details, "until") is { } until
            && DateTimeOffset.TryParse(until, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var end) && end > now
            ? end - now : null;
    }

    /// <summary>Whole minutes, rounded up, at least 1; null without a time.</summary>
    public static int? Minutes(TimeSpan? wait) =>
        wait is { } w && w > TimeSpan.Zero ? Math.Max(1, (int)Math.Ceiling(w.TotalMinutes)) : null;

    internal static string? Detail(JsonElement? details, string key) =>
        details is { ValueKind: JsonValueKind.Object } d && d.TryGetProperty(key, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() : null;
}

/// <summary>The role routes' refusals in the moderator's words (an i18n key), before the generic <c>error.&lt;CODE&gt;</c> text.</summary>
public static class ModerationErrors
{
    /// <summary>The key for <paramref name="error"/>, or null to show the code's generic text.</summary>
    public static string? TextKey(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Code switch
        {
            // oneself, or a moderator acting on a moderator or an admin
            ErrorCodes.AuthForbidden => "moderation.forbidden",
            ErrorCodes.AuthRoleRequired => KickCooldown.Detail(error.Details, "role") == "admin" ? "moderation.admin_required" : "moderation.role_required",
            ErrorCodes.PortalNotFound when error.Status == HttpStatusCode.NotFound => "moderation.not_found",
            ErrorCodes.RequestInvalid => "moderation.invalid",
            _ => null,
        };
    }

    /// <summary><c>REQUEST.INVALID</c>'s <c>details.field</c>, when it is a plain field name.</summary>
    public static string? Field(ApiException error) =>
        error is null ? null : KickCooldown.Detail(error.Details, "field") is { Length: > 0 and <= 40 } field && field.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '.')
            ? field : null;
}
