// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// The games this launcher may play (OD-206): <see cref="All"/> is the allowed titles. Which of them a PC offers comes from its
/// Azimuth folder (<see cref="Available"/>), each game's folder and cover from <c>data\azimuth.ini</c> (<see cref="AzimuthTitles"/>).
/// </summary>
/// <param name="Id">The server's game id.</param>
/// <param name="TitleId">The Xbox title id, as <c>AppxManifest.xml</c> and <c>data\titles</c> write it.</param>
/// <param name="StashDir">The mode cache stash, relative to the Azimuth folder.</param>
/// <param name="Caches">The caches of one mode (<see cref="ModeCaches"/>), relative to the Azimuth folder.</param>
/// <param name="CarDatabase">The file the hook sends the SHA-256 of, relative to <c>Mount</c>.</param>
/// <param name="FullFeatures">Cloud saves, mods, ExtraOptions' settings and the integrity check. FM5's first version has none.</param>
public sealed record GameDefinition(string Id, string Name, string TitleId, string StashDir, IReadOnlyList<string> Caches, string CarDatabase, bool FullFeatures)
{
    public static readonly GameDefinition Fh2 = new("fh2", AzimuthTitles.Fh2Name, "1A96545B", ModeCaches.Fh2StashDir, ModeCaches.Fh2Caches,
        Path.Combine("media", "stripped", "gamedbRC.slt"), FullFeatures: true);

    /// <summary>Which caches FM5 keeps per mode is not known yet, so none is switched.</summary>
    public static readonly GameDefinition Fm5 = new("fm5", "Forza Motorsport 5", "7B35D917", "data/fi-launcher/mode-cache-fm5", [],
        Path.Combine("media", "db", "gamedb.slt"), FullFeatures: false)
    {
        PrivateCreateActivity = Matchmaking.WorldActivities.Race, HasPoolWorlds = false, HardcoreCareers = false,
        ExtraOptionsIni = "FM5_ExtraOptions.ini", ExtraOptionsManifestResource = "Infinity.Core.ExtraOptionsInfinity.Fm5.manifest.json",
        ExtraOptionsSeedResource = null,
    };

    /// <summary>
    /// CR-S98 §6.2: a Hardcore profile may have a career of this game. FM5 has none until it keeps cloud saves (M11): no Start card for a
    /// Hardcore profile, and no Hardcore in the new-profile form.
    /// </summary>
    public bool HardcoreCareers { get; init; } = true;

    /// <summary>W11: the game has a Mods page (its ExtraOptions card, and the catalog mods that declare the game).</summary>
    public bool HasModsPage { get; init; } = true;

    /// <summary>W10/W11: ExtraOptions.Infinity's settings file for the game, beside <c>azimuth.exe</c>.</summary>
    public string ExtraOptionsIni { get; init; } = "FH2_ExtraOptions.ini";

    /// <summary>The embedded manifest that describes the game's ExtraOptions card and its settings.</summary>
    public string ExtraOptionsManifestResource { get; init; } = "Infinity.Core.ExtraOptionsInfinity.manifest.json";

    /// <summary>
    /// The embedded ini written when the game has none. Null: no file is written, and a missing file or key means the
    /// settings' defaults (FM5 reads only the keys it finds).
    /// </summary>
    public string? ExtraOptionsSeedResource { get; init; } = "Infinity.Core.FH2_ExtraOptions.ini";

    /// <summary>
    /// CR-S103: the game has official pool worlds a flagged player is moved to (FH2). Without them (FM5) the server ends a flagged
    /// player's seat with reason <c>pool</c> and starts the kick cooldown: the launcher says so and does not move the player.
    /// </summary>
    public bool HasPoolWorlds { get; init; } = true;

    /// <summary>
    /// The activity of every private lobby the game creates (CR-S99: FM5's are races). Null: the activity follows the game's own
    /// lobby kind, as FH2's free roam and Road Trip do.
    /// </summary>
    public string? PrivateCreateActivity { get; init; }

    /// <summary>The allowed titles.</summary>
    public static readonly IReadOnlyList<GameDefinition> All = [Fh2, Fm5];

    /// <summary>The title as Azimuth's <c>[title-&lt;id&gt;]</c> writes it.</summary>
    public string TitleKey => TitleId.ToLowerInvariant();

    public static GameDefinition? ById(string? id) => All.FirstOrDefault(game => game.Id == id);

    /// <summary>
    /// The allowed titles with a folder in <c>data\titles</c>. A new install has none until Azimuth first runs a game, so with
    /// none found FH2 stays available, as it always was.
    /// </summary>
    public static IReadOnlyList<GameDefinition> Available(string? azimuthRoot)
    {
        if (string.IsNullOrWhiteSpace(azimuthRoot)) return [Fh2];
        try
        {
            var titles = Path.Combine(azimuthRoot, "data", "titles");
            if (!Directory.Exists(titles)) return [Fh2];
            var found = Directory.GetDirectories(titles).Select(Path.GetFileName).ToHashSet(StringComparer.OrdinalIgnoreCase);
            var available = All.Where(game => found.Contains(game.TitleId)).ToList();
            return available.Count == 0 ? [Fh2] : available;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return [Fh2]; }
    }
}
