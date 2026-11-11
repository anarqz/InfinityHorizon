// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.IO.Compression;
using System.Xml;
using Infinity.Core.Process;

namespace Infinity.Core.Saves;

/// <summary>Which ProfileData field of a local save a game's driver level is read from.</summary>
public enum LevelField
{
    /// <summary>The level as the save stores it (FM5 <c>Main/Level</c>).</summary>
    Level,

    /// <summary>The total XP, turned into the level through the game's own XP table (FH2 <c>Main/XP</c>).</summary>
    Xp,
}

/// <summary>The game's own XP table, read from the player's game install at run time (never from the launcher).</summary>
/// <param name="Archive">The zip, relative to the game's folder that Azimuth names in <c>data\azimuth.ini</c>.</param>
/// <param name="Entry">The XML inside it.</param>
public sealed record XpTableFile(string Archive, string Entry);

/// <summary>
/// OD-240 #6 (owner, 2026-11-11): where an AZIMUTH row reads its driver level from this PC's save, as a <see cref="GameDefinition"/> row
/// (never <c>if game</c>): a stored level, or the XP through the game's own table.
/// </summary>
/// <param name="Field">The ProfileData field read.</param>
/// <param name="Table">The XP table (only for <see cref="LevelField.Xp"/>).</param>
/// <param name="ShownOffset">What the game adds to the stored level when it shows it (CR-S81: the level is stored from 0).</param>
public sealed record LocalLevelSource(LevelField Field, XpTableFile? Table = null, int ShownOffset = 1);

/// <summary>A local save's driver level as the game shows it, or why it is not known (<see cref="Why"/> is a string key, never a guess).</summary>
public sealed record LocalLevel(long? Shown, string? Why)
{
    public static LocalLevel Unknown(string why) => new(null, why);
}

/// <summary>
/// One band of the game's XP table: <c>NumLevels</c> level-ups, step <c>k</c> (from 0, restarting in each band) costing
/// <c>XPPerLevel + (k + 1) x XPIncreasePerLevel</c> (the game's parser, hardcore-save-fields.md §5).
/// </summary>
public sealed record XpBand(long Levels, long XpPerLevel, long XpIncreasePerLevel);

/// <summary>The game's XP table parsed from its own file: the stored level is the number of steps whose cumulative XP is at most the save's XP.</summary>
public sealed class XpTable
{
    private XpTable(IReadOnlyList<XpBand> bands) => Bands = bands;

    public IReadOnlyList<XpBand> Bands { get; }

    /// <summary>Every level-up the table has: the highest level the game shows.</summary>
    public long TopLevel => Bands.Sum(b => b.Levels);

    /// <summary>
    /// The table of <c>&lt;Wristbands&gt;&lt;WristbandEntry NumLevels XPPerLevel XPIncreasePerLevel …/&gt;…</c>, in order. Throws
    /// <see cref="InvalidDataException"/> when it is not that (no band, a missing or negative number): the row then says why.
    /// </summary>
    public static XpTable Parse(Stream xml)
    {
        ArgumentNullException.ThrowIfNull(xml);
        var settings = new XmlReaderSettings { DtdProcessing = DtdProcessing.Prohibit, XmlResolver = null, IgnoreComments = true, MaxCharactersInDocument = 1 << 20 };
        var bands = new List<XpBand>();
        try
        {
            using var reader = XmlReader.Create(xml, settings);
            while (reader.Read())
            {
                if (reader.NodeType != XmlNodeType.Element || reader.LocalName != "WristbandEntry") continue;
                bands.Add(new XpBand(Number(reader, "NumLevels"), Number(reader, "XPPerLevel"), Number(reader, "XPIncreasePerLevel")));
            }
        }
        catch (XmlException error) { throw new InvalidDataException("the XP table is not XML: " + error.Message, error); }
        if (bands.Count == 0) throw new InvalidDataException("the XP table has no band");
        return new XpTable(bands);

        static long Number(XmlReader reader, string name) =>
            long.TryParse(reader.GetAttribute(name), System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var n)
                ? n : throw new InvalidDataException("the XP table's " + name + " is missing or not a number");
    }

    /// <summary>The stored level <paramref name="xp"/> reaches (from 0).</summary>
    public long LevelOf(long xp)
    {
        long level = 0, needed = 0;
        foreach (var band in Bands)
        {
            for (var k = 0L; k < band.Levels; k++)
            {
                needed += Math.Max(band.XpPerLevel + (k + 1) * band.XpIncreasePerLevel, 1);
                if (needed > xp) return level;
                level++;
            }
        }
        return level;
    }
}

/// <summary>
/// OD-240 #6: an AZIMUTH row's driver level, read from this PC's save only (<see cref="SaveProfile.Level"/> or <see cref="SaveProfile.Xp"/>,
/// as the game's <see cref="LocalLevelSource"/> says), the XP table read from the player's own game install and cached per file (path,
/// time, size). Every failure is a reason, never a guessed level.
/// </summary>
public static class LocalLevels
{
    public const string NoValue = "sm.level_why.no_value";
    public const string NoGame = "sm.level_why.no_game";
    public const string NoTable = "sm.level_why.no_table";
    public const string BadTable = "sm.level_why.bad_table";
    public const string NotKnown = "sm.level_why.not_known";

    private static readonly ConcurrentDictionary<string, (DateTime Time, long Size, XpTable? Table, string? Why)> Tables = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>The level of <paramref name="save"/> in <paramref name="game"/>, the game's folder resolved from <paramref name="azimuthRoot"/>.</summary>
    public static LocalLevel Of(GameDefinition game, SaveProfile save, string? azimuthRoot) =>
        Of(game, save, () => AzimuthTitles.GameFolder(azimuthRoot, game));

    /// <summary>The same, with the game's folder given (tests).</summary>
    public static LocalLevel Of(GameDefinition game, SaveProfile save, Func<string?> gameFolder)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(save);
        ArgumentNullException.ThrowIfNull(gameFolder);
        if (game.LocalLevel is not { } source) return LocalLevel.Unknown(NotKnown);
        if (source.Field == LevelField.Level)
            return save.Level is { } stored and >= 0 ? new LocalLevel(stored + source.ShownOffset, null) : LocalLevel.Unknown(NoValue);
        if (save.Xp is not { } xp || xp < 0) return LocalLevel.Unknown(NoValue);
        if (source.Table is not { } file) return LocalLevel.Unknown(NotKnown);
        if (gameFolder() is not { } folder) return LocalLevel.Unknown(NoGame);
        var (table, why) = TableAt(Path.Combine(folder, file.Archive), file.Entry);
        return table is null ? LocalLevel.Unknown(why ?? BadTable) : new LocalLevel(Math.Min(table.LevelOf(xp) + source.ShownOffset, table.TopLevel), null);
    }

    /// <summary>The XP table in <paramref name="archive"/>'s <paramref name="entry"/>, cached per path, time and size; or why not.</summary>
    public static (XpTable? Table, string? Why) TableAt(string archive, string entry)
    {
        ArgumentNullException.ThrowIfNull(archive);
        ArgumentNullException.ThrowIfNull(entry);
        FileInfo info;
        try
        {
            info = new FileInfo(archive);
            if (!info.Exists) return (null, NoTable);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return (null, NoTable); }
        if (Tables.TryGetValue(archive, out var cached) && cached.Time == info.LastWriteTimeUtc && cached.Size == info.Length) return (cached.Table, cached.Why);
        (XpTable? Table, string? Why) read;
        try
        {
            using var zip = ZipFile.OpenRead(archive);
            var found = zip.Entries.FirstOrDefault(e => string.Equals(e.FullName, entry, StringComparison.OrdinalIgnoreCase));
            if (found is null) read = (null, NoTable);
            else
            {
                using var stream = found.Open();
                read = (XpTable.Parse(stream), null);
            }
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or NotSupportedException) { read = (null, BadTable); }
        Tables[archive] = (info.LastWriteTimeUtc, info.Length, read.Table, read.Why);
        return read;
    }
}
