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

    /// <summary>A <see cref="Notice"/> detail as an overlay toast; null when it is not one.</summary>
    public static Overlay.UiFrame.Toast? NoticeToast(string detail, Strings? strings = null)
    {
        if (detail is not { Length: > 2 } || detail[1] != '|' || detail[0] is < '0' or > '2') return null;
        var level = (byte)(detail[0] - '0');
        var text = detail[2..].Trim();
        if (text.Length == 0) return null;
        return Overlay.OverlayNotices.Create(level, text, strings);
    }
}
