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
    public const string ExtraOptionsVersion = "0.4.4";

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
    /// <summary>The pause menu's online tabs (owner 2026-10-29): on by default, no longer locked off by Infinity.</summary>
    public static readonly IReadOnlyList<string> OnlineTabs =
        ["frontend-enablemarketplace", "frontend-enablesocial", "frontend-enableclubs", "frontend-enablerivals"];

    /// <summary>The Treasure Map switch, held off for Hardcore profiles (owner 2026-10-29).</summary>
    public const string TreasureMap = "content-unlocktreasuremap";

    /// <summary>
    /// Once per launcher state (<paramref name="markerFile"/>): an existing ini's online tabs were 0 only because Infinity
    /// locked them off, never the player's choice, so they are turned on; after that the player's own choice is kept.
    /// </summary>
    public static string EnableOnlineTabsOnce(string azimuthRoot, string markerFile)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(markerFile);
        if (File.Exists(markerFile)) return "online tabs: kept as set";
        var settings = ReadSettings(azimuthRoot);
        var off = OnlineTabs.Where(id => !settings.Values.TryGetValue(id, out var value) || value.ValueKind != System.Text.Json.JsonValueKind.True).ToList();
        foreach (var id in off) settings.Set(id, "1");
        if (off.Count > 0) SaveSettings(azimuthRoot, settings);
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(markerFile))!);
        File.WriteAllText(markerFile, DateTime.UtcNow.ToString("O", System.Globalization.CultureInfo.InvariantCulture));
        return off.Count > 0 ? "online tabs turned on: " + string.Join(", ", off) : "online tabs: already on";
    }

    /// <summary>
    /// Before a launch (game closed): a Hardcore profile never plays with the Treasure Map. The player's switch is turned off and
    /// remembered in <paramref name="heldFile"/>; any other launch turns it back on if a Hardcore launch held it and it is still off.
    /// </summary>
    public static string? HoldTreasureMapForHardcore(string azimuthRoot, bool hardcore, string heldFile)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(heldFile);
        var settings = ReadSettings(azimuthRoot);
        var on = settings.Values.TryGetValue(TreasureMap, out var value) && value.ValueKind == System.Text.Json.JsonValueKind.True;
        if (hardcore)
        {
            if (!on) return null;
            settings.Set(TreasureMap, "0");
            SaveSettings(azimuthRoot, settings);
            Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(heldFile))!);
            File.WriteAllText(heldFile, "1");
            return "treasure map off for Hardcore";
        }
        if (!File.Exists(heldFile)) return null;
        if (!on)
        {
            settings.Set(TreasureMap, "1");
            SaveSettings(azimuthRoot, settings);
        }
        File.Delete(heldFile);
        return on ? null : "treasure map on again after Hardcore";
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
