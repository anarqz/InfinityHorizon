// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
﻿namespace Infinity.Core.Session;

/// <summary>
/// The hook's <see cref="Frame.Event"/> kinds (launch.c <c>fi_launch_event</c>; HOOK_MIGRATION.md):
/// what the game did that the launcher acts on. The detail is free text, never a secret.
/// </summary>
public static class HookEvents
{
    /// <summary>The game committed a save revision (detail: its folder).</summary>
    public const byte SaveRevision = 1;

    /// <summary>Reserved legacy event; .73 no longer changes or opens the game's online menu.</summary>
    public const byte OnlineMenu = 2;

    /// <summary>The game joined a free roam world (detail <c>host:&lt;port&gt;</c> or <c>join:&lt;ip:port&gt;</c>).</summary>
    public const byte FreeRoamEntered = 3;

    /// <summary>The game left free roam (HORIZON SOLO, the association destroyed): a hosted world closes (OD-57).</summary>
    public const byte FreeRoamLeft = 4;

    /// <summary>Loaded local player/car: detail generation:0/1, repeated every 500 ms.</summary>
    public const byte GameReadiness = 5;

    /// <summary>
    /// A notice for the player (hook .73, OD-73): detail <c>&lt;level&gt;|&lt;text&gt;</c>, level 0 info,
    /// 1 warning, 2 error, the text already in the game's language (at most 250 UTF-8 bytes). The
    /// launcher shows it as an overlay toast; the hook no longer writes it into the pause menu.
    /// </summary>
    public const byte Notice = 6;

    /// <summary>
    /// The server's <c>X-Fi-Activity-State</c> for this session (hook activity.c, OD-121, GATE_API.md §4a): detail
    /// <c>active|menu|idle|no_signal</c>, with <c>;evict_in=N</c> only while eviction is enforced.
    /// </summary>
    public const byte Activity = 7;

    /// <summary>
    /// An <see cref="Activity"/> detail as the overlay's idle warning: only when it carries <c>evict_in</c> (the seat
    /// goes to a waiting player in N seconds); null otherwise, or when the detail is malformed.
    /// </summary>
    public static Overlay.UiFrame.Toast? IdleWarning(string detail, Strings? strings = null)
    {
        var parts = detail.Split(';');
        if (parts.Length != 2 || parts[0] is not ("active" or "menu" or "idle" or "no_signal") || !parts[1].StartsWith("evict_in=", StringComparison.Ordinal))
            return null;
        var digits = parts[1]["evict_in=".Length..];
        if (digits.Length is 0 or > 6 || !digits.All(char.IsAsciiDigit)) return null;
        var seconds = int.Parse(digits, System.Globalization.CultureInfo.InvariantCulture);
        var text = (strings?["overlay.idle_warning"] ?? "You seem to be away and players are waiting. Drive within {seconds} s to keep your seat.")
            .Replace("{seconds}", seconds.ToString(System.Globalization.CultureInfo.InvariantCulture), StringComparison.Ordinal);
        return Overlay.OverlayNotices.Create(Overlay.UiLevel.Warn, text, strings);
    }

    /// <summary>A <see cref="Notice"/> detail as an overlay toast; null when it is not one.</summary>
    public static Overlay.UiFrame.Toast? NoticeToast(string detail, Strings? strings = null)
    {
        if (detail is not { Length: > 2 } || detail[1] != '|' || detail[0] is < '0' or > '2') return null;
        var level = (byte)(detail[0] - '0');
        var text = detail[2..].Trim();
        if(text=="connection.direct_failed")text=strings?[text] ?? "The direct connection could not be established. Try again or check Connection details.";
        // OD122-L4: the hook refused the start (it plays offline on the profile's own save, never Azimuth's)
        if (text == "launch.ticket_refused") text = strings?[text] ?? "Online is off for this session: the game did not accept the launcher's session ticket. You play offline on this profile's own save; Azimuth's own save is untouched.";
        if (text == "launch.late") text = strings?[text] ?? "Online is off for this session: the game started before the launcher's session reached it. You play offline on this profile's own save; Azimuth's own save is untouched.";
        if (text.Length == 0) return null;
        return Overlay.OverlayNotices.Create(level, text, strings);
    }
}
