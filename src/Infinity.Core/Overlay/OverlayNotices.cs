// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Overlay;

/// <summary>Readable notices for the native overlay's Title\nBody toast format (OD-73).</summary>
public static class OverlayNotices
{
    public static UiFrame.Toast Create(byte level, string body, Strings? text = null)
    {
        level = Math.Min(level, UiLevel.Error);
        var key = level switch
        {
            UiLevel.Error => "overlay.notice_error",
            UiLevel.Warn => "overlay.notice_warning",
            _ => "overlay.notice_info",
        };
        var title = text?[key] ?? "Infinity";
        var icon = level switch { UiLevel.Error => (byte)3, UiLevel.Warn => (byte)2, _ => (byte)0 };
        return new UiFrame.Toast(level, icon, level == UiLevel.Error ? (ushort)150 : (ushort)80,
            title + "\n" + body.Trim());
    }

    /// <summary>A blocked save's code, with <c> retry=&lt;seconds&gt;</c> when the server gave a <c>Retry-After</c> (read back by <see cref="ParseBlocked"/>).</summary>
    public static string BlockedDetail(string code, TimeSpan? retryAfter) => retryAfter is { } wait
        ? code + " retry=" + ((long)Math.Ceiling(wait.TotalSeconds)).ToString(System.Globalization.CultureInfo.InvariantCulture)
        : code;

    /// <summary>The code and <c>Retry-After</c> of <see cref="BlockedDetail"/>.</summary>
    public static (string Code, TimeSpan? RetryAfter) ParseBlocked(string detail)
    {
        ArgumentNullException.ThrowIfNull(detail);
        var at = detail.IndexOf(" retry=", StringComparison.Ordinal);
        return at >= 0 && long.TryParse(detail.AsSpan(at + 7), System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var seconds)
            ? (detail[..at], TimeSpan.FromSeconds(seconds))
            : (detail, null);
    }

    /// <summary>Whole hours, at least 1, for "about {hours} h".</summary>
    public static int Hours(TimeSpan wait) => Math.Max(1, (int)Math.Ceiling(wait.TotalHours));

    public static UiFrame.Toast SaveBlocked(Strings text, string detail)
    {
        ArgumentNullException.ThrowIfNull(text);
        var (code, retryAfter) = ParseBlocked(detail);
        if (code == "SAVES.RATE_LIMITED" && retryAfter is { } wait)
        {
            return new UiFrame.Toast(UiLevel.Error, 3, 150, text["overlay.save_stopped"] + "\n"
                + text.Format("saves.rate_limited_wait", ("hours", Hours(wait).ToString(System.Globalization.CultureInfo.CurrentCulture))));
        }
        var key = code switch
        {
            "SERVICE.UNAVAILABLE" => "saves.seed_unavailable",
            "SAVES.REVISION_CONFLICT" => "saves.conflict",
            "SAVES.LOCAL_BUSY" => "saves.busy",
            "SAVES.SERVICE_MISSING" => "saves.service_missing",
            "SAVES.RATE_LIMITED" => "saves.rate_limited", // CR-S52: a seed's commit waits for the day's checks (Retry-After)
            _ => "saves.blocked",
        };
        return new UiFrame.Toast(UiLevel.Error, 3, 150,
            text["overlay.save_stopped"] + "\n" + text[key]);
    }
}
