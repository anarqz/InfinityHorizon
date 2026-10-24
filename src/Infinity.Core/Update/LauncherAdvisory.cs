// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Update;

/// <summary>
/// OD-118 (T1b): before Sign in the launcher reads only GitHub's release list, straight from the player's PC. A signed release newer
/// than this launcher is offered as advice; it never blocks Play, and the release floor still comes from the server after Sign in.
/// </summary>
public static class LauncherAdvisory
{
    /// <summary>The discovered release's version when it is newer than <paramref name="current"/>; null otherwise or when either does not parse.</summary>
    public static string? NewerOnGitHub(string current, string? discovered) =>
        LauncherVersion.TryParse(current, out var installed) && LauncherVersion.TryParse(discovered, out var release) && release > installed
            ? release.Value : null;
}
