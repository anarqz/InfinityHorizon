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
    public static string EnsureFh2(string azimuthRoot, string? gameFolder)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var path = Path.Combine(azimuthRoot, "data", IniFile);
        var lines = File.Exists(path) ? Read(path) : [];
        var changed = new List<string>();
        var title = SectionAt(lines, "title-" + Fh2Id);
        if (string.IsNullOrWhiteSpace(ValueOf(lines, title, "folder")))
        {
            if (string.IsNullOrWhiteSpace(gameFolder))
            {
                return "FH2 not added (no game folder known): Azimuth opens its own launcher";
            }
            if (title < 0)
            {
                lines.Add("[title-" + Fh2Id + "]");
                lines.Add("folder=" + gameFolder);
                lines.Add("content=");
                lines.Add("name=" + Fh2Name);
            }
            else
            {
                Set(lines, title, "folder", gameFolder);
                if (string.IsNullOrWhiteSpace(ValueOf(lines, title, "name"))) Set(lines, title, "name", Fh2Name);
            }
            changed.Add("title added");
        }
        var launcher = SectionAt(lines, "launcher");
        if (launcher < 0)
        {
            lines.InsertRange(0, ["[launcher]", "game=" + Fh2Id]);
            changed.Add("launcher game set");
        }
        else if (!string.Equals(ValueOf(lines, launcher, "game"), Fh2Id, StringComparison.OrdinalIgnoreCase))
        {
            Set(lines, launcher, "game", Fh2Id);
            changed.Add("launcher game set");
        }
        if (changed.Count == 0)
        {
            return "FH2 already added";
        }
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var temporary = path + ".fi-new";
        File.WriteAllText(temporary, string.Join("\r\n", lines) + "\r\n", new UnicodeEncoding(bigEndian: false, byteOrderMark: true));
        File.Move(temporary, path, overwrite: true);
        return string.Join(", ", changed);
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
