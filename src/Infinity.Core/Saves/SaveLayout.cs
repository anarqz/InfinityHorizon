// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Saves;

/// <summary>
/// One game's save layout under Azimuth's <c>data\titles\&lt;title&gt;\saves</c> (catalog data, <see cref="Process.GameDefinition.Saves"/>):
/// <c>&lt;PlayerDir&gt;\&lt;UserPrefix&gt;&lt;XUID upper hex&gt;\&lt;RevisionsDir&gt;\&lt;16 hex&gt;\{commit, data\&lt;file&gt;}</c>. A career's revision holds
/// <see cref="RequiredFiles"/> and only names among <see cref="AllowedFiles"/>: a save of another game (or anything else) is refused by
/// its file set, never by a game id. Some games (FM5) keep part of a career in sibling containers next to the <c>User_</c> folder
/// (<see cref="SiblingPrefixes"/>: liveries, garage thumbnails, replays) that the career's files point to by name.
/// </summary>
/// <param name="PlayerDir">The local player's folder (<c>player-1</c>).</param>
/// <param name="UserPrefix">A profile's folder name before its XUID in upper hex (<c>User_</c>).</param>
/// <param name="RevisionsDir">The profile's revisions folder (<c>revisions</c>).</param>
/// <param name="RequiredFiles">Data files every career revision of the game has.</param>
/// <param name="AllowedFiles">The only data file names a career revision of the game may hold.</param>
public sealed record SaveLayout(string PlayerDir, string UserPrefix, string RevisionsDir, IReadOnlyList<string> RequiredFiles, IReadOnlyList<string> AllowedFiles)
{
    /// <summary>
    /// Name prefixes of the sibling containers a career points to (each <c>&lt;prefix&gt;&lt;rest&gt;\revisions\…</c> beside the <c>User_</c>
    /// folder). Empty: the career is its <c>User_</c> folder only (FH2).
    /// </summary>
    public IReadOnlyList<string> SiblingPrefixes { get; init; } = [];

    /// <summary>
    /// Where the career names the sibling containers it needs (FM5: <c>Career_Garage.LiveryFileName</c> and <c>.Thumbnail</c>). Null: the
    /// career names none. The restore set is the <c>User_</c> revision plus the containers named here (fm5-save-format.md §1).
    /// </summary>
    public SiblingReferences? References { get; init; }

    /// <summary>Sibling prefixes no career row names that are copied when they came with the save (FM5: <c>Replay_</c>, optional).</summary>
    public IReadOnlyList<string> OptionalSiblingPrefixes { get; init; } = [];

    /// <summary>The folder, beside <c>revisions</c> in a save copied off a PC, that holds the sibling containers (<c>containers</c>).</summary>
    public string StagedSiblingsDir { get; init; } = "containers";

    /// <summary>CR-S105: the game's cloud save bundle (FH2SAVE1, or FM5SAVE1 with the sibling containers).</summary>
    public ICareerBundle Bundle { get; init; } = Fh2CareerBundle.Instance;

    /// <summary>FH2 (the FH2SAVE1 bundle's names, <see cref="Fh2Bundle.SaveFiles"/>).</summary>
    public static SaveLayout Fh2 { get; } = new("player-1", "User_", "revisions",
        ["PlayerDatabase", "ProfileData", "SaveState"],
        ["BinarySaveState", "PlayerDatabase", "ProfileData", "SaveState", "TransactionLogFile", "VersionFlags"]);

    /// <summary>
    /// FM5 (SAURON RE 2026-11-08): no SaveState; <c>Career_Garage.LiveryFileName/.Thumbnail</c> name sibling <c>BaseLivery_&lt;car&gt;_&lt;ts&gt;</c>
    /// and <c>GarageThumbnail_&lt;car&gt;_&lt;ts&gt;</c> containers, and replays are <c>Replay_*</c> containers, all in <c>player-1</c>.
    /// </summary>
    public static SaveLayout Fm5 { get; } = WithBundle(new("player-1", "User_", "revisions",
        ["PlayerDatabase", "ProfileData"],
        ["PlayerDatabase", "ProfileData", "TransactionLogFile", "VersionFlags"])
    {
        SiblingPrefixes = ["BaseLivery_", "GarageThumbnail_", "Replay_"],
        References = new SiblingReferences("PlayerDatabase", 8, "Career_Garage", ["LiveryFileName", "Thumbnail"], ["CS:"]),
        OptionalSiblingPrefixes = ["Replay_"],
    });

    /// <summary>CR-S105: FM5SAVE1 reads the career's references through the layout itself.</summary>
    private static SaveLayout WithBundle(SaveLayout layout) => layout with { Bundle = new Fm5CareerBundle(layout) };

    /// <summary><c>User_&lt;XUID upper hex&gt;</c>.</summary>
    public string UserFolder(ulong xuid) => UserPrefix + xuid.ToString("X", CultureInfo.InvariantCulture);

    /// <summary><c>&lt;saves&gt;\player-1\User_&lt;hex&gt;</c> of <paramref name="xuid"/>.</summary>
    public string ProfileDir(string savesRoot, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        return Path.Combine(savesRoot, PlayerDir, UserFolder(xuid));
    }

    /// <summary>A sibling container's name: one of <see cref="SiblingPrefixes"/>, then a plain name (letters, digits, '_', '-').</summary>
    public bool IsSibling(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        return name.Length <= 96 && SiblingPrefixes.Any(p => name.Length > p.Length && name.StartsWith(p, StringComparison.Ordinal))
            && name.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-');
    }
}

/// <summary>
/// The table columns of a career data file that name sibling containers (fm5-save-format.md §1): <paramref name="File"/> is a SQLite
/// database after a <paramref name="HeaderBytes"/>-byte header (<c>ssmc</c> + u32); each value, less one of
/// <paramref name="StripPrefixes"/> (<c>CS:</c>), is a container name.
/// </summary>
public sealed record SiblingReferences(string File, int HeaderBytes, string Table, IReadOnlyList<string> Columns, IReadOnlyList<string> StripPrefixes);
