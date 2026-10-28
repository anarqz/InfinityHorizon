// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Mods;

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

    /// <summary>The upstream ExtraOptions release ExtraOptions.Infinity is built from (<c>native/extraoptions</c>, MOD_VERSION).</summary>
    public const string ExtraOptionsVersion = "0.3.9";

    private static readonly Lazy<ModManifest> BuiltIn = new(() =>
    {
        using var stream = typeof(ExtraOptionsInfinity).Assembly.GetManifestResourceStream("Infinity.Core.ExtraOptionsInfinity.manifest.json")
            ?? throw new InvalidOperationException("the ExtraOptions.Infinity manifest is not embedded");
        using var buffer = new MemoryStream();
        stream.CopyTo(buffer);
        return new ModManifest(buffer.ToArray());
    });

    /// <summary>
    /// OD-125/127: ExtraOptions as part of Infinity, for the Mods page: its options (<c>FH2_ExtraOptions.ini</c>) and their locks.
    /// No package or files: the hook install puts it in place, so it is never installed, disabled or removed as a mod.
    /// </summary>
    public static ModManifest Manifest => BuiltIn.Value;

    /// <summary>The player's ExtraOptions settings; the ini is written first when there is none, as the hook needs it anyway.</summary>
    public static ManifestIni ReadSettings(string azimuthRoot)
    {
        EnsureIni(azimuthRoot);
        return new ManifestIni(Manifest, File.ReadAllBytes(Path.Combine(azimuthRoot, IniFile)));
    }

    /// <summary>
    /// Writes edited settings with the locks enforced, only if the file is still the one they were read from
    /// (otherwise <see cref="ModSettingsChangedException"/>); replaced in one step, never half written.
    /// </summary>
    public static void SaveSettings(string azimuthRoot, ManifestIni settings)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(settings);
        if (settings.ManifestDigest != Manifest.Digest) throw new ModException("settings belong to another manifest", ModErrors.Settings);
        var path = Path.Combine(azimuthRoot, IniFile);
        if (!File.Exists(path) || ModManifest.Hash(File.ReadAllBytes(path)) != settings.SourceHash) throw new ModSettingsChangedException();
        settings.EnforceLocks();
        var temporary = path + ".fi-new";
        File.WriteAllBytes(temporary, settings.ToBytes());
        File.Move(temporary, path, overwrite: true);
    }
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
