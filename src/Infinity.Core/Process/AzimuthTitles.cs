// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Process;

/// <summary>
/// Azimuth 1.0 keeps its games in <c>data\azimuth.ini</c> (UTF-16LE): a <c>[title-&lt;id&gt;]</c> section per added
/// game (<c>folder</c>, <c>content</c>, <c>name</c>) and <c>[launcher] game</c>, the one <c>--skip-launcher</c> starts.
/// A game not added there opens Azimuth's own launcher instead (SAURON's 1.0.2 runtime checks). Before a launch the
/// launcher adds Forza Horizon 2 when it is missing and names it as the game; nothing else in the file changes.
/// </summary>
public static class AzimuthTitles
{
    /// <summary>Forza Horizon 2's title id, as Azimuth writes it.</summary>
    public const string Fh2Id = "1a96545b";
    public const string Fh2Name = "Forza Horizon 2";
    public const string IniFile = "azimuth.ini";

    /// <summary>
    /// Adds FH2 (<paramref name="gameFolder"/>: the folder holding <c>Mount</c>) when <c>[title-1a96545b]</c> has no
    /// folder, and sets <c>[launcher] game</c>; returns what happened, for the log. Without a known game folder a missing
    /// title is left to Azimuth's own launcher.
    /// </summary>
    public static string EnsureFh2(string azimuthRoot, string? gameFolder) => EnsureTitle(azimuthRoot, GameDefinition.Fh2, gameFolder);

    /// <summary>
    /// <see cref="EnsureFh2"/> for any game. A game other than FH2 also follows a folder that moved, and the launcher must check
    /// <see cref="Names"/> afterwards: without a known folder nothing is written, and <c>--skip-launcher</c> would start another game.
    /// </summary>
    public static string EnsureTitle(string azimuthRoot, GameDefinition game, string? gameFolder)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(game);
        var path = Path.Combine(azimuthRoot, "data", IniFile);
        var lines = File.Exists(path) ? Read(path) : [];
        var changed = new List<string>();
        var (id, label) = (game.TitleKey, game.Id.ToUpperInvariant());
        var title = SectionAt(lines, "title-" + id);
        var known = ValueOf(lines, title, "folder");
        if (string.IsNullOrWhiteSpace(known))
        {
            if (string.IsNullOrWhiteSpace(gameFolder))
            {
                return label + " not added (no game folder known): Azimuth opens its own launcher";
            }
            if (title < 0)
            {
                lines.Add("[title-" + id + "]");
                lines.Add("folder=" + gameFolder);
                lines.Add("content=");
                lines.Add("name=" + game.Name);
            }
            else
            {
                Set(lines, title, "folder", gameFolder);
                if (string.IsNullOrWhiteSpace(ValueOf(lines, title, "name"))) Set(lines, title, "name", game.Name);
            }
            changed.Add("title added");
        }
        else if (!game.FullFeatures && !string.IsNullOrWhiteSpace(gameFolder) && !string.Equals(known, gameFolder, StringComparison.OrdinalIgnoreCase))
        {
            Set(lines, title, "folder", gameFolder);
            changed.Add("game folder updated");
        }
        var launcher = SectionAt(lines, "launcher");
        if (launcher < 0)
        {
            lines.InsertRange(0, ["[launcher]", "game=" + id]);
            changed.Add("launcher game set");
        }
        else if (!string.Equals(ValueOf(lines, launcher, "game"), id, StringComparison.OrdinalIgnoreCase))
        {
            Set(lines, launcher, "game", id);
            changed.Add("launcher game set");
        }
        // Azimuth's own launcher writes [launcher] shown beside game: never left naming the other game
        launcher = SectionAt(lines, "launcher");
        if (ValueOf(lines, launcher, "shown") is { } shown && !string.Equals(shown, id, StringComparison.OrdinalIgnoreCase))
        {
            Set(lines, launcher, "shown", id);
            if (changed.Count == 0) changed.Add("launcher shown set");
        }
        if (changed.Count == 0)
        {
            return label + " already added";
        }
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var temporary = path + ".fi-new";
        File.WriteAllText(temporary, string.Join("\r\n", lines) + "\r\n", new UnicodeEncoding(bigEndian: false, byteOrderMark: true));
        File.Move(temporary, path, overwrite: true);
        return string.Join(", ", changed);
    }

    /// <summary>Whether <c>--skip-launcher</c> starts <paramref name="game"/>: it has a folder and <c>[launcher] game</c> names it.</summary>
    public static bool Names(string azimuthRoot, GameDefinition game)
    {
        var path = Path.Combine(azimuthRoot, "data", IniFile);
        if (!File.Exists(path)) return false;
        var lines = Read(path);
        return !string.IsNullOrWhiteSpace(ValueOf(lines, SectionAt(lines, "title-" + game.TitleKey), "folder"))
            && string.Equals(ValueOf(lines, SectionAt(lines, "launcher"), "game"), game.TitleKey, StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>The game's folder as <c>data\azimuth.ini</c> names it (a relative one is based on the Azimuth folder); null when it does not.</summary>
    public static string? GameFolder(string? azimuthRoot, GameDefinition game)
    {
        if (string.IsNullOrWhiteSpace(azimuthRoot)) return null;
        try
        {
            var path = Path.Combine(azimuthRoot, "data", IniFile);
            if (!File.Exists(path)) return null;
            var lines = Read(path);
            var folder = ValueOf(lines, SectionAt(lines, "title-" + game.TitleKey), "folder");
            return string.IsNullOrWhiteSpace(folder) ? null : Path.GetFullPath(Path.Combine(azimuthRoot, folder));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
    }

    /// <summary>
    /// The game's display name as <c>data\azimuth.ini</c> writes it (<c>name=</c>); null when it names none. Owner 2026-11-08: the tiles'
    /// name comes from here (FM5's manifest DisplayName is <c>ms-resource:Title</c>), else from the catalog.
    /// </summary>
    public static string? DisplayName(string? azimuthRoot, GameDefinition game)
    {
        if (string.IsNullOrWhiteSpace(azimuthRoot)) return null;
        try
        {
            var path = Path.Combine(azimuthRoot, "data", IniFile);
            if (!File.Exists(path)) return null;
            var lines = Read(path);
            var name = ValueOf(lines, SectionAt(lines, "title-" + game.TitleKey), "name");
            return string.IsNullOrWhiteSpace(name) || name.StartsWith("ms-resource:", StringComparison.OrdinalIgnoreCase) ? null : name;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
    }

    private static List<string> Read(string path)
    {
        var text = File.ReadAllText(path, Encoding.Unicode);
        return [.. text.Replace("\r\n", "\n").TrimEnd('\n').Split('\n')];
    }

    private static int SectionAt(List<string> lines, string name) =>
        lines.FindIndex(l => l.Trim().Equals("[" + name + "]", StringComparison.OrdinalIgnoreCase));

    /// <summary>The line of <paramref name="key"/> in the section starting at <paramref name="section"/>, or -1.</summary>
    private static int KeyAt(List<string> lines, int section, string key)
    {
        if (section < 0) return -1;
        for (var i = section + 1; i < lines.Count && !lines[i].TrimStart().StartsWith('['); i++)
        {
            var at = lines[i].IndexOf('=');
            if (at > 0 && lines[i][..at].Trim().Equals(key, StringComparison.OrdinalIgnoreCase)) return i;
        }
        return -1;
    }

    private static string? ValueOf(List<string> lines, int section, string key) =>
        KeyAt(lines, section, key) is var i and >= 0 ? lines[i][(lines[i].IndexOf('=') + 1)..].Trim() : null;

    private static void Set(List<string> lines, int section, string key, string value)
    {
        var i = KeyAt(lines, section, key);
        if (i >= 0)
        {
            lines[i] = key + "=" + value;
            return;
        }
        var end = section + 1;
        while (end < lines.Count && !lines[end].TrimStart().StartsWith('[')) end++;
        lines.Insert(end, key + "=" + value);
    }
}
