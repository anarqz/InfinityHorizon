// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// Owner 2026-10-28: Infinity needs an Azimuth the player has opened once and set Forza Horizon 2 up in. A fresh download's
/// <c>data</c> holds only <c>bin</c> (and the licence text); the first run adds its own folders (<c>cache</c>, <c>logs</c>,
/// <c>titles</c>). Folders only: <c>data\azimuth.ini</c> is no proof, since the launcher itself may write it.
/// </summary>
public static class AzimuthFirstRun
{
    /// <summary>True when <c>data</c> has no folder but <c>bin</c> (or no <c>data</c> at all); an unreadable folder never blocks.</summary>
    public static bool NeverRun(string azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var data = Path.Combine(azimuthRoot, "data");
        try
        {
            if (!Directory.Exists(data)) return true;
            return Directory.EnumerateDirectories(data).All(folder => string.Equals(Path.GetFileName(folder), "bin", StringComparison.OrdinalIgnoreCase));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return false; }
    }
}
