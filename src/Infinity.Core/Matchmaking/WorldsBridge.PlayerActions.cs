// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

public sealed partial class WorldsBridge
{
    /// <summary>OD-193 F-2: the game's PLAYER ACTIONS friend and report items (null: the overlay's asks are not answered).</summary>
    public PlayerActions? PlayerActions { get; init; }

    /// <summary>
    /// CR-S90: the world a report names, only when the overlay's roster ("my world", as last read) lists that player; null
    /// otherwise (the server then checks no shared world).
    /// </summary>
    public string? RosterWorldOf(string xuid)
    {
        var mine = Volatile.Read(ref _mineSnapshot);
        return mine.Role != UiWorldRole.None && !string.IsNullOrEmpty(mine.WorldId) && mine.Members.Any(m => m.Xuid == xuid) ? mine.WorldId : null;
    }
}
