// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// OD-125: the hook is ExtraOptions.Infinity, the <c>winmm.dll</c> beside <c>azimuth.exe</c>. ExtraOptions refuses to run
/// without <c>FH2_ExtraOptions.ini</c> beside it, so the launcher writes ExtraOptions.Infinity's own
/// (<c>native/extraoptions-infinity/FH2_ExtraOptions.ini</c>, embedded) when there is none. A file the player already has
/// is kept: the settings the Infinity server decides are forced off by ExtraOptions.Infinity itself (patch 0002).
/// </summary>
public static class ExtraOptionsInfinity
{
    public const string IniFile = "FH2_ExtraOptions.ini";
    private const string Resource = "Infinity.Core.FH2_ExtraOptions.ini";

    /// <summary>Writes the embedded ini beside <c>azimuth.exe</c> when none is there; returns what happened, for the log.</summary>
    public static string EnsureIni(string azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var path = Path.Combine(azimuthRoot, IniFile);
        if (File.Exists(path))
        {
            return "ini kept";
        }
        using var stream = typeof(ExtraOptionsInfinity).Assembly.GetManifestResourceStream(Resource)
            ?? throw new InvalidOperationException("the ExtraOptions.Infinity ini is not embedded");
        var temporary = path + ".fi-new";
        using (var file = new FileStream(temporary, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            stream.CopyTo(file);
        }
        File.Move(temporary, path, overwrite: false);
        return "ini written";
    }
}
