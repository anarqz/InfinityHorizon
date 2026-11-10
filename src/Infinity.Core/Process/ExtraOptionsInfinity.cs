// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Mods;

namespace Infinity.Core.Process;

/// <summary>
/// OD-125: the hook is ExtraOptions.Infinity, the <c>winmm.dll</c> beside <c>azimuth.exe</c>. ExtraOptions refuses to run
/// without <c>FH2_ExtraOptions.ini</c> beside it, so the launcher writes ExtraOptions.Infinity's own
/// (<c>native/extraoptions-infinity/FH2_ExtraOptions.ini</c>, embedded) when there is none. A file the player already has
/// is kept: the settings the Infinity server decides are forced off by ExtraOptions.Infinity itself (patch 0002, 0004 for FM5), and the
/// launcher writes them off too before every launch (<see cref="EnforceLocks"/>). OD-233: every ini the launcher writes passes
/// ExtraOptions 0.6.0's own parser rules (<see cref="ExtraOptionsIniRules"/>), since an invalid one turns every feature off.
/// </summary>
public static class ExtraOptionsInfinity
{
    public const string IniFile = "FH2_ExtraOptions.ini";

    /// <summary>The upstream ExtraOptions release ExtraOptions.Infinity is built from (<c>native/extraoptions</c>, MOD_VERSION).</summary>
    public const string ExtraOptionsVersion = "0.6.0";

    private static readonly System.Collections.Concurrent.ConcurrentDictionary<string, ModManifest> Manifests = new();

    private static ModManifest Load(string resource) => Manifests.GetOrAdd(resource, name =>
    {
        using var stream = typeof(ExtraOptionsInfinity).Assembly.GetManifestResourceStream(name)
            ?? throw new InvalidOperationException("the ExtraOptions.Infinity manifest is not embedded");
        using var buffer = new MemoryStream();
        stream.CopyTo(buffer);
        return new ModManifest(buffer.ToArray());
    });

    /// <summary>
    /// OD-125/127: ExtraOptions as part of Infinity, for the Mods page: its options (<c>FH2_ExtraOptions.ini</c>) and their locks.
    /// No package or files: the hook install puts it in place, so it is never installed, disabled or removed as a mod.
    /// </summary>
    public static ModManifest Manifest => ManifestFor(GameDefinition.Fh2);

    /// <summary>W11: the card and settings of one game's ExtraOptions.Infinity (the one winmm.dll serves both games).</summary>
    public static ModManifest ManifestFor(GameDefinition game) => Load(game.ExtraOptionsManifestResource);

    /// <summary>The player's ExtraOptions settings; the ini is written first when there is none, as the hook needs it anyway.</summary>
    public static ManifestIni ReadSettings(string azimuthRoot) => ReadSettings(azimuthRoot, GameDefinition.Fh2);

    /// <summary>
    /// A game's settings. A game that seeds an ini has it written first; one that does not (FM5) reads a missing file as the
    /// manifest's defaults, and nothing is written until the player saves.
    /// </summary>
    public static ManifestIni ReadSettings(string azimuthRoot, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(game);
        if (game.ExtraOptionsSeedResource is not null) EnsureIni(azimuthRoot, game);
        return new ManifestIni(ManifestFor(game), SourceBytes(azimuthRoot, game));
    }

    private static byte[] SourceBytes(string azimuthRoot, GameDefinition game)
    {
        var path = Path.Combine(azimuthRoot, game.ExtraOptionsIni);
        return File.Exists(path) ? File.ReadAllBytes(path) : ManifestIni.Defaults(ManifestFor(game));
    }

    /// <summary>
    /// Writes edited settings with the locks enforced, only if the file is still the one they were read from
    /// (otherwise <see cref="ModSettingsChangedException"/>); replaced in one step, never half written.
    /// </summary>
    public static void SaveSettings(string azimuthRoot, ManifestIni settings) => SaveSettings(azimuthRoot, settings, GameDefinition.Fh2);

    public static void SaveSettings(string azimuthRoot, ManifestIni settings, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(settings);
        ArgumentNullException.ThrowIfNull(game);
        if (settings.ManifestDigest != ManifestFor(game).Digest) throw new ModException("settings belong to another manifest", ModErrors.Settings);
        var path = Path.Combine(azimuthRoot, game.ExtraOptionsIni);
        // a missing file stands for the defaults (games that seed none); it must still be missing or unchanged
        var current = File.Exists(path) || game.ExtraOptionsSeedResource is not null ? (File.Exists(path) ? File.ReadAllBytes(path) : null) : ManifestIni.Defaults(ManifestFor(game));
        if (current is null || ModManifest.Hash(current) != settings.SourceHash) throw new ModSettingsChangedException();
        settings.EnforceLocks();
        if (!InlineComments(game)) settings.DropInlineComments();
        var bytes = settings.ToBytes();
        if (ExtraOptionsIniRules.Problem(game, bytes) is { } problem)
            throw new ModException("the settings file would not be valid for ExtraOptions " + ExtraOptionsVersion + ": " + problem, ModErrors.Settings);
        var temporary = path + ".fi-new";
        File.WriteAllBytes(temporary, bytes);
        File.Move(temporary, path, overwrite: true);
    }

    /// <summary>FH2's parser ends a value at <c>;</c> or <c>#</c>; FM5's <c>GetPrivateProfileString</c> keeps them in it.</summary>
    private static bool InlineComments(GameDefinition game) => game.Id != "fm5";

    /// <summary>
    /// OD-233, before every launch (game closed): the game's locked settings (the server's: <c>GenerateMissingSaveCommits</c>,
    /// <c>OfflineVIP</c>, <c>UnlockDLC</c>, and the switches Infinity keeps on) are written into its ini, which ExtraOptions.Infinity forces
    /// anyway (patches 0002, 0004), so the file says what runs. A game that seeds no ini (FM5) gets one with its manifest's settings and
    /// those locks. The player's other lines, comments and choices stay; nothing is written when nothing changes, and never a file
    /// ExtraOptions would refuse (<see cref="ModException"/>). Returns what happened, for the log.
    /// </summary>
    public static string EnforceLocks(string azimuthRoot, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(game);
        var path = Path.Combine(azimuthRoot, game.ExtraOptionsIni);
        var exists = File.Exists(path);
        if (!exists && game.ExtraOptionsSeedResource is not null) return "no ini to lock";
        var source = exists ? File.ReadAllBytes(path) : ManifestIni.Defaults(ManifestFor(game));
        var settings = new ManifestIni(ManifestFor(game), source);
        settings.EnforceLocks();
        if (!InlineComments(game)) settings.DropInlineComments();
        var bytes = settings.ToBytes();
        var invalid = ExtraOptionsIniRules.Problem(game, source);
        if (exists && bytes.AsSpan().SequenceEqual(source)) return invalid is null ? "locks kept" : "locks kept; the ini is not valid for ExtraOptions: " + invalid;
        if (ExtraOptionsIniRules.Problem(game, bytes) is { } problem)
            throw new ModException("the settings file would not be valid for ExtraOptions " + ExtraOptionsVersion + ": " + problem, ModErrors.Settings);
        var temporary = path + ".fi-new";
        File.WriteAllBytes(temporary, bytes);
        File.Move(temporary, path, overwrite: true);
        return exists ? "locks written" : "ini written with the locks";
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

    /// <summary>Writes the embedded ini beside <c>azimuth.exe</c> when none is there; returns what happened, for the log.</summary>
    public static string EnsureIni(string azimuthRoot) => EnsureIni(azimuthRoot, GameDefinition.Fh2);

    /// <summary>The same for one game; a game with no seed ini has nothing to write.</summary>
    public static string EnsureIni(string azimuthRoot, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(game);
        var path = Path.Combine(azimuthRoot, game.ExtraOptionsIni);
        if (File.Exists(path))
        {
            return "ini kept";
        }
        if (game.ExtraOptionsSeedResource is not { } resource) return "no ini needed";
        using var stream = typeof(ExtraOptionsInfinity).Assembly.GetManifestResourceStream(resource)
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
