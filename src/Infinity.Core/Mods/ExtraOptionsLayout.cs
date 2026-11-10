// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Mods;

/// <summary>Why the move to the per-game mods layout stopped (OD-233); <see cref="Folder"/> names the folder or file it stopped at.</summary>
public sealed class ExtraOptionsLayoutException(string reason, string folder, string message, Exception? inner = null)
    : ModException(message + ": " + folder, ModErrors.Layout, inner)
{
    /// <summary><c>linked</c>, <c>conflict</c>, <c>move</c>, <c>loadorder</c> or <c>game_running</c> (the player-facing text's key).</summary>
    public string Reason { get; } = reason;
    public string Folder { get; } = folder;
}

/// <summary>What one run of <see cref="ExtraOptionsLayout.Migrate"/> did: the folders moved (as <c>&lt;game folder&gt;\&lt;folder&gt;</c>) and a log line per step.</summary>
public sealed record ExtraOptionsLayoutResult(IReadOnlyList<string> Moved, IReadOnlyList<string> Notes)
{
    public static ExtraOptionsLayoutResult Nothing { get; } = new([], []);
}

/// <summary>
/// OD-233: ExtraOptions 0.6.0 keeps each game's mods in its own folder (<c>shared/addon_layout.h</c>): <c>mods\Forza Horizon 2\&lt;id&gt;</c> and
/// <c>mods\Forza Motorsport 5\&lt;id&gt;</c>, each with its own <c>loadorder.txt</c>; <c>mods\Cache</c> is the loader's own cache (never a mod,
/// never moved or synced), and <c>mods\Forza Horizon 3</c> is reserved. The launcher moves the old flat layout (<c>mods\&lt;id&gt;</c> and
/// <c>mods\loadorder.txt</c>, ExtraOptions 0.4.4's) before a launch, per game, and a manifest destination written for the old layout
/// (<c>mods/&lt;id&gt;/…</c>) is read as its game's folder (<see cref="CanonicalDestination"/>).
/// </summary>
public static class ExtraOptionsLayout
{
    public const string Fh2Folder = "Forza Horizon 2";
    public const string Fm5Folder = "Forza Motorsport 5";
    public const string Fh3Folder = "Forza Horizon 3";
    public const string CacheFolder = "Cache";
    private const int MaxLoadOrder = 64 * 1024;

    /// <summary>The game's folder under <c>mods\</c>, as ExtraOptions 0.6.0 names it.</summary>
    public static string GameFolder(string game) => game switch
    {
        "fh2" => Fh2Folder,
        "fm5" => Fm5Folder,
        _ => throw new ModException("no ExtraOptions mods folder for game " + game, ModErrors.Unsupported),
    };

    /// <summary>The game has a mods folder of its own (FH2, FM5; ExtraOptions.Infinity builds no FH3).</summary>
    public static bool HasModsFolder(string game) => game is "fh2" or "fm5";

    /// <summary>The game id of a game folder's name (letter case ignored), or null.</summary>
    public static string? GameOfFolder(string name) =>
        string.Equals(name, Fh2Folder, StringComparison.OrdinalIgnoreCase) ? "fh2"
        : string.Equals(name, Fm5Folder, StringComparison.OrdinalIgnoreCase) ? "fm5" : null;

    /// <summary>ExtraOptions' reserved names under <c>mods\</c> (<c>layout_reserved</c>): the game folders and <c>Cache</c>.</summary>
    public static bool Reserved(string name) =>
        name is "." or ".." || new[] { Fh2Folder, Fm5Folder, Fh3Folder, CacheFolder }.Contains(name, StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// An Azimuth-relative path of the old flat layout (<c>mods/&lt;folder&gt;/…</c>, the folder not a reserved name) as its game's
    /// (<c>mods/&lt;game folder&gt;/&lt;folder&gt;/…</c>); any other path is returned unchanged.
    /// </summary>
    public static string CanonicalDestination(string destination, string game)
    {
        ArgumentNullException.ThrowIfNull(destination);
        return LegacyFolder(destination) is null ? destination : ExtraOptionsMods.ModsFolder + "/" + GameFolder(game) + destination[ExtraOptionsMods.ModsFolder.Length..];
    }

    /// <summary>The folder of an old flat-layout path (<c>mods/&lt;folder&gt;</c> or below, the folder not reserved), or null.</summary>
    public static string? LegacyFolder(string relative)
    {
        ArgumentNullException.ThrowIfNull(relative);
        var path = relative.Replace('\\', '/');
        if (!path.StartsWith(ExtraOptionsMods.ModsFolder + "/", StringComparison.OrdinalIgnoreCase)) return null;
        var rest = path[(ExtraOptionsMods.ModsFolder.Length + 1)..];
        var slash = rest.IndexOf('/');
        var folder = slash < 0 ? rest : rest[..slash];
        return folder.Length == 0 || Reserved(folder) || folder.Equals(ExtraOptionsMods.LoadOrderFile, StringComparison.OrdinalIgnoreCase) ? null : folder;
    }

    /// <summary>True while <c>mods\</c> still has the old flat layout: a folder that is no reserved name, or the root <c>loadorder.txt</c>.</summary>
    public static bool NeedsMigration(string azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var root = ExtraOptionsMods.ModsRoot(azimuthRoot);
        if (!Directory.Exists(root)) return false;
        return File.Exists(Path.Combine(root, ExtraOptionsMods.LoadOrderFile))
            || new DirectoryInfo(root).EnumerateDirectories().Any(d => !Reserved(d.Name));
    }

    /// <summary>
    /// Moves the old flat layout to the per-game one, game closed (the caller's check, and <paramref name="gameRunning"/> before each move).
    /// Every folder of <c>mods\</c> but the reserved names moves, whole, to <c>mods\&lt;game folder&gt;\&lt;folder&gt;</c>:
    /// <paramref name="gameOf"/> names its game (a dot folder, such as the launcher's <c>.replaced</c>, is FH2's: the old layout was FH2's,
    /// as ExtraOptions' own migration has it). The root <c>loadorder.txt</c> is split by where each listed folder now is (FM5's lines to FM5's
    /// file, everything else, comments included, to FH2's), appended to a game's existing file without repeating a folder it lists; a copy
    /// goes to <paramref name="backupDirectory"/>, then the root file is removed.
    /// <para>
    /// Nothing is ever overwritten or deleted: every destination is checked before the first move (a folder already in its game's folder,
    /// or a linked one, stops it with <see cref="ExtraOptionsLayoutException"/>), and a move Windows refuses stops it where it is. The root
    /// <c>loadorder.txt</c> stays until every folder has moved, so the next run continues exactly there; a run on the per-game layout
    /// changes nothing.
    /// </para>
    /// <para>
    /// <paramref name="include"/>, when given, limits the run to the folders it accepts (the ones whose game is already known); the others
    /// stay, and so does the root <c>loadorder.txt</c>, until a run without a limit (or one that accepts every remaining folder) moves them.
    /// </para>
    /// </summary>
    public static ExtraOptionsLayoutResult Migrate(string azimuthRoot, Func<string, string> gameOf, string? backupDirectory = null, Func<bool>? gameRunning = null,
        Func<string, bool>? include = null)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(gameOf);
        var root = ExtraOptionsMods.ModsRoot(azimuthRoot);
        var rootInfo = new DirectoryInfo(root);
        if (!rootInfo.Exists) return ExtraOptionsLayoutResult.Nothing;
        if (rootInfo.LinkTarget is not null || rootInfo.Attributes.HasFlag(FileAttributes.ReparsePoint))
            throw new ExtraOptionsLayoutException("linked", ExtraOptionsMods.ModsFolder, "the mods folder is a link");
        var legacyOrder = Path.Combine(root, ExtraOptionsMods.LoadOrderFile);
        var folders = rootInfo.EnumerateDirectories().Where(d => !Reserved(d.Name)).OrderBy(d => d.Name, StringComparer.OrdinalIgnoreCase).ToList();
        // a folder left out waits (and with it the root loadorder.txt, which is split only once every folder has moved)
        var waiting = include is null ? 0 : folders.RemoveAll(d => !include(d.Name));
        if (folders.Count == 0 && (waiting > 0 || !File.Exists(legacyOrder))) return ExtraOptionsLayoutResult.Nothing;

        // Preflight: nothing moves unless every folder can.
        foreach (var game in new[] { "fh2", "fm5" })
        {
            var folder = new DirectoryInfo(Path.Combine(root, GameFolder(game)));
            if (File.Exists(folder.FullName) || folder.Exists && (folder.LinkTarget is not null || folder.Attributes.HasFlag(FileAttributes.ReparsePoint)))
                throw new ExtraOptionsLayoutException("linked", folder.Name, "a game's mods folder is a link or a file");
        }
        var plan = new List<(DirectoryInfo Source, string Game, string Target)>();
        foreach (var folder in folders)
        {
            if (folder.LinkTarget is not null || folder.Attributes.HasFlag(FileAttributes.ReparsePoint))
                throw new ExtraOptionsLayoutException("linked", folder.Name, "a linked mod folder is moved by hand");
            var game = folder.Name.StartsWith('.') ? "fh2" : gameOf(folder.Name);
            var target = Path.Combine(root, GameFolder(game), folder.Name);
            if (Directory.Exists(target) || File.Exists(target))
                throw new ExtraOptionsLayoutException("conflict", folder.Name, "the mod folder is already in its game's folder");
            plan.Add((folder, game, target));
        }
        List<string>? lines = null; var bom = false; var newLine = "\r\n";
        if (waiting == 0 && File.Exists(legacyOrder))
        {
            if ((File.GetAttributes(legacyOrder) & FileAttributes.ReparsePoint) != 0)
                throw new ExtraOptionsLayoutException("linked", ExtraOptionsMods.LoadOrderFile, "loadorder.txt is a link");
            try { (lines, bom, newLine) = ReadLines(legacyOrder); }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or DecoderFallbackException)
            { throw new ExtraOptionsLayoutException("loadorder", ExtraOptionsMods.LoadOrderFile, "loadorder.txt cannot be read", error); }
        }

        var moved = new List<string>(); var notes = new List<string>();
        foreach (var (source, game, target) in plan)
        {
            if (gameRunning?.Invoke() == true) throw new ExtraOptionsLayoutException("game_running", source.Name, "the game is running");
            try
            {
                Directory.CreateDirectory(Path.GetDirectoryName(target)!);
                Directory.Move(source.FullName, target);
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException)
            { throw new ExtraOptionsLayoutException("move", source.Name, "Windows refused to move the mod folder (" + error.GetType().Name + ")", error); }
            moved.Add(GameFolder(game) + "\\" + source.Name);
            notes.Add("mods layout: " + source.Name + " moved to mods\\" + GameFolder(game));
        }

        if (lines is not null)
        {
            var byGame = new Dictionary<string, List<string>> { ["fh2"] = [], ["fm5"] = [] };
            if (lines.Count > 0 && lines[^1].Length == 0) lines.RemoveAt(lines.Count - 1);
            foreach (var line in lines)
            {
                var entry = ExtraOptionsMods.Entry(line);
                var game = entry is { } e && Directory.Exists(Path.Combine(root, Fm5Folder, e.Folder)) && !Directory.Exists(Path.Combine(root, Fh2Folder, e.Folder)) ? "fm5" : "fh2";
                byGame[game].Add(line);
            }
            foreach (var (game, gameLines) in byGame)
            {
                if (gameLines.Count == 0 || gameLines.All(l => l.Trim(' ', '\t').Length == 0)) continue;
                var file = Path.Combine(root, GameFolder(game), ExtraOptionsMods.LoadOrderFile);
                try
                {
                    if (File.Exists(file))
                    {
                        if ((File.GetAttributes(file) & FileAttributes.ReparsePoint) != 0)
                            throw new ExtraOptionsLayoutException("linked", GameFolder(game) + "\\" + ExtraOptionsMods.LoadOrderFile, "loadorder.txt is a link");
                        var (existing, existingBom, existingNewLine) = ReadLines(file);
                        if (existing.Count > 0 && existing[^1].Length == 0) existing.RemoveAt(existing.Count - 1);
                        var listed = existing.Select(ExtraOptionsMods.Entry).OfType<(string Folder, bool Enabled)>().Select(e => e.Folder).ToHashSet(StringComparer.OrdinalIgnoreCase);
                        // the game's own file keeps its lines; a folder it already lists keeps its own line (and state)
                        var added = gameLines.Where(l => ExtraOptionsMods.Entry(l) is { } e && listed.Add(e.Folder)).ToList();
                        if (added.Count == 0) continue;
                        WriteLines(file, [.. existing, .. added], existingBom, existingNewLine);
                    }
                    else
                    {
                        Directory.CreateDirectory(Path.GetDirectoryName(file)!);
                        WriteLines(file, gameLines, bom, newLine);
                    }
                    notes.Add("mods layout: loadorder.txt lines moved to mods\\" + GameFolder(game));
                }
                catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or DecoderFallbackException)
                { throw new ExtraOptionsLayoutException("loadorder", GameFolder(game) + "\\" + ExtraOptionsMods.LoadOrderFile, "loadorder.txt cannot be written", error); }
            }
            try
            {
                if (backupDirectory is not null)
                {
                    Directory.CreateDirectory(backupDirectory);
                    File.Copy(legacyOrder, Path.Combine(backupDirectory, "loadorder-" + DateTime.UtcNow.ToString("yyyyMMddHHmmss", System.Globalization.CultureInfo.InvariantCulture)
                        + "-" + Guid.NewGuid().ToString("N")[..8] + ".txt"));
                }
                ManagedModFiles.RemoveFile(legacyOrder); // the one safe delete of mod code
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException)
            { throw new ExtraOptionsLayoutException("loadorder", ExtraOptionsMods.LoadOrderFile, "the old loadorder.txt cannot be removed", error); }
            notes.Add("mods layout: the old mods\\loadorder.txt is split per game");
        }
        return new(moved, notes);
    }

    private static (List<string> Lines, bool Bom, string NewLine) ReadLines(string path)
    {
        var bytes = File.ReadAllBytes(path);
        if (bytes.Length > MaxLoadOrder) throw new InvalidDataException("loadorder.txt is larger than ExtraOptions reads (64 KiB)");
        var bom = bytes.Length >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf;
        var text = new UTF8Encoding(false, true).GetString(bytes, bom ? 3 : 0, bytes.Length - (bom ? 3 : 0));
        var newLine = text.Contains("\r\n", StringComparison.Ordinal) || !text.Contains('\n') ? "\r\n" : "\n";
        return ([.. text.Replace("\r\n", "\n").Split('\n')], bom, newLine);
    }

    private static void WriteLines(string path, List<string> lines, bool bom, string newLine)
    {
        var text = string.Join(newLine, lines) + (lines.Count > 0 && lines[^1].Length > 0 ? newLine : "");
        var bytes = new UTF8Encoding(bom).GetPreamble().Concat(new UTF8Encoding(false).GetBytes(text)).ToArray();
        if (bytes.Length > MaxLoadOrder) throw new InvalidDataException("loadorder.txt would exceed ExtraOptions' 64 KiB");
        var temporary = path + ".fi-new";
        File.WriteAllBytes(temporary, bytes);
        File.Move(temporary, path, overwrite: true);
    }
}
