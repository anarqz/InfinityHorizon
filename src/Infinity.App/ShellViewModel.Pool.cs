// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.App;

/// <summary>
/// OD-149 (MATCHMAKING_API.md §3.9): the sidebar's red notice while the play session's Hardcore profile is isolated by the
/// anti-cheat system ("Isolated by the anti-cheat system for 1 h 23 min..."), with the Discord link to dispute it. It comes
/// from the play session's own list and seat reads (no request of its own) and is refreshed with the sidebar's server figures.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The notice, or empty when the profile is not isolated (or no play session read it).</summary>
    public string PoolNotice => _playWorlds?.PoolNotice() ?? "";

    public bool ShowPoolNotice => SignedIn && PoolNotice.Length > 0;

    private void RefreshPoolNotice()
    {
        Changed(nameof(PoolNotice));
        Changed(nameof(ShowPoolNotice));
    }
}
