// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Mods;

/// <summary>The settings file changed since this editing session loaded it.</summary>
public sealed class ModSettingsChangedException() : IOException("settings changed outside the launcher");

/// <summary>
/// One switch of <c>FH2_ExtraOptions.ini</c> (0 = off, 1 = on), as the official 0.2.98 file
/// documents it. <see cref="Advanced"/> options change how the game treats saves or online
/// services, so the form shows them apart with a warning. <see cref="LockedOff"/> options conflict
/// with Infinity (online tabs, VIP, Azimuth's save checks) and are kept at 0 (owner, 2026-10-19).
/// </summary>
public sealed record IniOption(string Section, string Key, bool Default, bool Advanced = false, bool LockedOff = false)
{
    /// <summary>The string-catalog key of the label: <c>eo.&lt;section&gt;.&lt;key&gt;</c> (lowercase); its help is the same plus <c>.help</c>.</summary>
    public string StringKey => $"eo.{Section.ToLowerInvariant()}.{Key.ToLowerInvariant()}";
}

/// <summary>
/// <c>FH2_ExtraOptions.ini</c> edited safely (OD-45): only the values of known switches change;
/// comments, order, blank lines, unknown sections and keys and the file's line endings stay as
/// they are. Values are 0 or 1 only. Section and key names compare ignoring case, as Windows'
/// INI readers do; a key that appears twice in one section is edited at its first line (the one
/// ExtraOptions reads).
/// </summary>
public sealed class ExtraOptionsIni
{
    public const string FileName = "FH2_ExtraOptions.ini";

    /// <summary>Every switch of the official 0.2.98 INI, in its order.</summary>
    public static IReadOnlyList<IniOption> Schema { get; } =
    [
        new("Global", "Enabled", true),
        new("Content", "UnlockDLC", true),
        new("Content", "OfflineVIP", true, LockedOff: true),
        new("Content", "UnlockTreasureMap", false),
        new("Content", "UnlockStormIslandCars", true),
        new("Frontend", "SkipIntro", true),
        new("Frontend", "GetInCarFromPauseMenu", true),
        new("Frontend", "AllRimsCategory", true),
        new("Frontend", "EnableMarketplace", true, LockedOff: true),
        new("Frontend", "EnableSocial", true, LockedOff: true),
        new("Frontend", "EnableClubs", true, LockedOff: true),
        new("Frontend", "EnableRivals", true, LockedOff: true),
        new("PhotoMode", "FreeCamera", true),
        new("Autodrive", "Enabled", true),
        new("Profile", "CustomGamerpic", true),
        new("Compatibility", "DisableParentalLocks", true, Advanced: true),
        new("Compatibility", "DisableAzimuthSaveChecksums", true, Advanced: true, LockedOff: true),
        new("Files", "EnableMediaOverrides", true, Advanced: true),
    ];

    // Each line with its own terminator ("\r\n", "\n" or "" for a last line without one), so a
    // file with mixed endings is written back exactly; new lines take the file's usual ending.
    private readonly List<string> _lines;
    private readonly List<string> _endings;
    private readonly string _newline;
    private Encoding _encoding = new UTF8Encoding(false);
    public string? SourceHash { get; private set; }

    private ExtraOptionsIni(List<string> lines, List<string> endings, string newline)
    {
        _lines = lines;
        _endings = endings;
        _newline = newline;
    }

    public static ExtraOptionsIni Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var (lines, endings) = (new List<string>(), new List<string>());
        var start = 0;
        while (start <= text.Length)
        {
            var nl = text.IndexOf('\n', start);
            if (nl < 0)
            {
                if (start < text.Length || lines.Count == 0)
                {
                    lines.Add(text[start..]);
                    endings.Add(string.Empty);
                }
                break;
            }
            var cr = nl > start && text[nl - 1] == '\r';
            lines.Add(text[start..(cr ? nl - 1 : nl)]);
            endings.Add(cr ? "\r\n" : "\n");
            start = nl + 1;
        }
        var crlf = endings.Count(e => e == "\r\n");
        var lf = endings.Count(e => e == "\n");
        return new ExtraOptionsIni(lines, endings, crlf >= lf && crlf > 0 ? "\r\n" : lf > 0 ? "\n" : "\r\n");
    }

    /// <summary>
    /// Reads the file in its own encoding, kept for <see cref="Save"/>: a UTF-8 or UTF-16 BOM, else
    /// UTF-8 when the bytes are valid UTF-8, else the bytes one to one (an ANSI file round-trips).
    /// </summary>
    public static ExtraOptionsIni Load(string path)
    {
        var bytes = File.ReadAllBytes(path);
        Encoding encoding;
        int skip;
        if (bytes.AsSpan().StartsWith((ReadOnlySpan<byte>)[0xEF, 0xBB, 0xBF]))
        {
            (encoding, skip) = (new UTF8Encoding(true), 3);
        }
        else if (bytes.AsSpan().StartsWith((ReadOnlySpan<byte>)[0xFF, 0xFE]))
        {
            (encoding, skip) = (new UnicodeEncoding(false, true), 2);
        }
        else
        {
            try
            {
                _ = new UTF8Encoding(false, true).GetString(bytes);
                (encoding, skip) = (new UTF8Encoding(false), 0);
            }
            catch (DecoderFallbackException)
            {
                (encoding, skip) = (Encoding.Latin1, 0);
            }
        }
        var ini = Parse(encoding.GetString(bytes, skip, bytes.Length - skip));
        ini._encoding = encoding;
        ini.SourceHash = ModManifest.Hash(bytes);
        return ini;
    }

    public override string ToString()
    {
        var text = new StringBuilder();
        for (var i = 0; i < _lines.Count; i++)
        {
            text.Append(_lines[i]).Append(_endings[i]);
        }
        return text.ToString();
    }

    private void InsertLine(int index, string text)
    {
        if (index < _lines.Count)
        {
            _lines.Insert(index, text);
            _endings.Insert(index, _newline);
            return;
        }
        // appended: a file that ended without a newline still does, after the new line
        var lastHadNone = _lines.Count > 0 && _endings[^1].Length == 0;
        if (lastHadNone)
        {
            _endings[^1] = _newline;
        }
        _lines.Add(text);
        _endings.Add(lastHadNone ? string.Empty : _newline);
    }

    /// <summary>The switch's value; null when the key is absent or not 0/1 (ExtraOptions then uses its default).</summary>
    public bool? Get(IniOption option)
    {
        ArgumentNullException.ThrowIfNull(option);
        return Find(option.Section, option.Key) is { } at ? ParseBool(Value(_lines[at])) : null;
    }

    /// <summary>
    /// Puts every <see cref="IniOption.LockedOff"/> switch at 0 (a missing key too, since the
    /// mod's default is on). Whether anything changed.
    /// </summary>
    public bool EnforceInfinityLocks()
    {
        var changed = false;
        foreach (var option in Schema.Where(o => o.LockedOff))
        {
            if (Get(option) != false)
            {
                Set(option, false);
                changed = true;
            }
        }
        return changed;
    }

    /// <summary>The value the game will use: the file's, else the option's default.</summary>
    public bool Effective(IniOption option) => Get(option) ?? option.Default;

    /// <summary>Sets a switch: the value after '=' on its line changes; a missing key is added at its section's end (a missing section at the file's end).</summary>
    public void Set(IniOption option, bool value)
    {
        ArgumentNullException.ThrowIfNull(option);
        var text = value ? "1" : "0";
        if (Find(option.Section, option.Key) is { } at)
        {
            var line = _lines[at];
            var equals = line.IndexOf('=', StringComparison.Ordinal);
            var rest = line[(equals + 1)..];
            var trailing = rest.Length - rest.TrimStart().Length;
            // keep anything after the value (an inline comment) and the spacing around it
            var valueEnd = trailing;
            while (valueEnd < rest.Length && !char.IsWhiteSpace(rest[valueEnd]) && rest[valueEnd] is not (';' or '#'))
            {
                valueEnd++;
            }
            _lines[at] = line[..(equals + 1)] + rest[..trailing] + text + rest[valueEnd..];
            return;
        }
        var section = FindSection(option.Section);
        if (section is null)
        {
            if (_lines.Count > 0 && _lines[^1].Length > 0)
            {
                InsertLine(_lines.Count, string.Empty);
            }
            InsertLine(_lines.Count, $"[{option.Section}]");
            InsertLine(_lines.Count, $"{option.Key}={text}");
            return;
        }
        var end = SectionEnd(section.Value);
        // before the blank lines that close the section
        while (end > section.Value + 1 && _lines[end - 1].Trim().Length == 0)
        {
            end--;
        }
        InsertLine(end, $"{option.Key}={text}");
    }

    /// <summary>Serialized settings in their original encoding, including any BOM.</summary>
    public byte[] ToBytes() => [.. _encoding.GetPreamble(), .. _encoding.GetBytes(ToString())];

    /// <summary>Writes atomically, after copying the current file to <paramref name="backupPath"/> when given.</summary>
    public void Save(string path, string? backupPath)
    {
        if (SourceHash is not null && (!File.Exists(path) || ModManifest.Hash(File.ReadAllBytes(path)) != SourceHash))
            throw new ModSettingsChangedException();
        if (backupPath is not null && File.Exists(path))
        {
            Directory.CreateDirectory(Path.GetDirectoryName(backupPath)!);
            File.Copy(path, backupPath, overwrite: true);
        }
        var temp = path + ".fi-tmp";
        File.WriteAllText(temp, ToString(), _encoding); // the file's own encoding (and BOM)
        if (SourceHash is not null && (!File.Exists(path) || ModManifest.Hash(File.ReadAllBytes(path)) != SourceHash))
        {
            File.Delete(temp);
            throw new ModSettingsChangedException();
        }
        File.Move(temp, path, overwrite: true);
        SourceHash = ModManifest.Hash(ToBytes());
    }

    private static bool? ParseBool(string value) => value switch
    {
        "1" => true,
        "0" => false,
        _ => null,
    };

    /// <summary>The value: up to an inline comment (';' or '#') or a space, without surrounding quotes.</summary>
    private static string Value(string line)
    {
        var rest = line[(line.IndexOf('=', StringComparison.Ordinal) + 1)..].Trim();
        var end = rest.IndexOfAny([';', '#']);
        rest = (end >= 0 ? rest[..end] : rest).Trim();
        if (rest.Length >= 2 && rest[0] is '"' or '\'' && rest[^1] == rest[0])
        {
            rest = rest[1..^1].Trim();
        }
        var space = rest.IndexOfAny([' ', '\t']);
        return space >= 0 ? rest[..space] : rest;
    }

    /// <summary>A section header, comment after it allowed: <c>[Name] ; text</c>.</summary>
    private static string? SectionName(string line)
    {
        var t = line.Trim();
        if (!t.StartsWith('['))
        {
            return null;
        }
        var close = t.IndexOf(']', StringComparison.Ordinal);
        if (close < 0)
        {
            return null;
        }
        var after = t[(close + 1)..].Trim();
        return after.Length == 0 || after[0] is ';' or '#' ? t[1..close].Trim() : null;
    }

    private int? FindSection(string section)
    {
        for (var i = 0; i < _lines.Count; i++)
        {
            if (string.Equals(SectionName(_lines[i]), section, StringComparison.OrdinalIgnoreCase))
            {
                return i;
            }
        }
        return null;
    }

    private int SectionEnd(int header)
    {
        var i = header + 1;
        while (i < _lines.Count && SectionName(_lines[i]) is null)
        {
            i++;
        }
        return i;
    }

    private int? Find(string section, string key)
    {
        if (FindSection(section) is not { } header)
        {
            return null;
        }
        for (var i = header + 1; i < SectionEnd(header); i++)
        {
            var t = _lines[i].TrimStart();
            if (t.StartsWith(';') || t.StartsWith('#'))
            {
                continue;
            }
            var equals = t.IndexOf('=', StringComparison.Ordinal);
            if (equals > 0 && string.Equals(t[..equals].Trim(), key, StringComparison.OrdinalIgnoreCase))
            {
                return i;
            }
        }
        return null;
    }
}
