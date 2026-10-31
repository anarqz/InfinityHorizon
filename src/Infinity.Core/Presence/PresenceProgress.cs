// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Presence;

/// <summary>
/// The card's level and credits (DISCORD_PRESENCE.md §2, CR-S81). The level is only the server's driver level
/// (<see cref="ProfilePresence.Level"/>, the level the last accepted sync's XP reaches, counted from 0): the level in the hook's
/// <c>Progress</c> frame is the game's <c>playerRaceHistory[0].level</c>, the step inside the current colour band, not the driver level,
/// so it is never kept. No server level (offline, a server without the route, before the first accepted sync): no level. Credits take
/// the hook's fresher reading during play, else the server's. Not thread-safe: the UI thread's.
/// </summary>
public sealed class PresenceProgress
{
    private ulong? _hookCredits;

    /// <summary>The hook's <c>Progress</c> frame: only its credits are kept; <paramref name="inBandLevel"/> is ignored (CR-S81).</summary>
    public void Hook(uint inBandLevel, ulong credits)
    {
        _ = inBandLevel;
        _hookCredits = credits;
    }

    /// <summary>The game started or exited: the hook's reading is forgotten.</summary>
    public void Reset() => _hookCredits = null;

    /// <summary>The driver level (from 0) for <see cref="PresenceInputs.DriverLevel"/>: the server's, else null.</summary>
    public static uint? DriverLevel(ProfilePresence? server) => server?.Level;

    /// <summary>The credits: during play the hook's when it sent any, else the server's.</summary>
    public ulong? Credits(bool game, ProfilePresence? server) => game ? _hookCredits ?? server?.Credits : server?.Credits;
}
