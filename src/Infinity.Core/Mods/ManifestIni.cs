// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>An INI value the manifest cannot accept (<c>invalid</c>, read as its default) or a key bound twice (<c>duplicate</c>, the first line wins).</summary>
public sealed record ManifestIniIssue(string OptionId, string Kind);

/// <summary>
/// The byte-preserving editor of a mod's main INI, typed by its manifest. Reading is tolerant (M6):
/// out-of-schema and duplicate values become <see cref="Issues"/> with <see cref="Repair"/>, never an
/// exception; writing is strict and invariant on disk.
/// </summary>
public sealed class ManifestIni
{
    private readonly ModManifest _manifest;
    private readonly List<string> _lines;
    private readonly string _newline;
    private readonly List<string> _endings = [];
    private readonly Encoding _encoding;
    public string SourceHash { get; }
    internal string ManifestDigest => _manifest.Digest;
    public IReadOnlyDictionary<string, JsonElement> Values => Read().Values;
    public IReadOnlyList<ManifestIniIssue> Issues => Read().Issues;
    public ManifestIni(ModManifest manifest, byte[] bytes)
    {
        _manifest = manifest;
        if (bytes.Length > 64 * 1024) throw new ModException("settings too large");
        SourceHash = ModManifest.Hash(bytes);
        int skip;
        if (bytes.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) (_encoding, skip) = (new UTF8Encoding(true), 3);
        else if (bytes.AsSpan().StartsWith(new byte[] { 0xff, 0xfe })) (_encoding, skip) = (new UnicodeEncoding(false, true, true), 2);
        else if (bytes.AsSpan().StartsWith(new byte[] { 0xfe, 0xff })) (_encoding, skip) = (new UnicodeEncoding(true, true, true), 2);
        else
        {
            try { _ = new UTF8Encoding(false, true).GetString(bytes); (_encoding, skip) = (new UTF8Encoding(false), 0); }
            catch (DecoderFallbackException) { (_encoding, skip) = (Encoding.Latin1, 0); }
        }
        var text = _encoding.GetString(bytes, skip, bytes.Length - skip);
        _lines = [];
        var start = 0;
        while (start <= text.Length)
        {
            var nl = text.IndexOf('\n', start);
            if (nl < 0)
            {
                if (start < text.Length || _lines.Count == 0) { _lines.Add(text[start..]); _endings.Add(""); }
                break;
            }
            var cr = nl > start && text[nl - 1] == '\r';
            _lines.Add(text[start..(cr ? nl - 1 : nl)]); _endings.Add(cr ? "\r\n" : "\n"); start = nl + 1;
        }
        _newline = _endings.Contains("\r\n", StringComparer.Ordinal) ? "\r\n" : "\n";
        _ = Read();
    }
    public static byte[] Defaults(ModManifest manifest)
    {
        var text = new StringBuilder();
        foreach (var group in Options(manifest).GroupBy(o => o.GetProperty("section").GetString()))
        {
            text.Append('[').Append(group.Key).Append("]\r\n");
            foreach (var option in group) text.Append(option.GetProperty("key").GetString()).Append('=').Append(Encode(option.GetProperty("default"))).Append("\r\n");
        }
        return Encoding.UTF8.GetBytes(text.ToString());
    }
    /// <summary>Sets a value from the form: numbers in <paramref name="culture"/> (then invariant), written invariant. A locked option accepts only its locked value.</summary>
    public void Set(string id, string text, CultureInfo? culture = null)
    {
        var option = Options(_manifest).Single(o => o.GetProperty("id").GetString() == id);
        var value = Parse(option, text, culture);
        try { ModMultiplayer.ValidateValue(option, value); }
        catch (ModException) { throw new ModException("invalid setting value", ModErrors.Settings); }
        if (option.TryGetProperty("lockedValue", out var locked) && !JsonElement.DeepEquals(locked, value)) throw new ModException("this setting is locked by Infinity", ModErrors.Locked);
        var section = option.GetProperty("section").GetString()!;
        var key = option.GetProperty("key").GetString()!;
        var index = Find(section, key);
        if (index >= 0)
        {
            var line = _lines[index]; var start = line.IndexOf('=') + 1;
            while (start < line.Length && char.IsWhiteSpace(line[start])) start++;
            var end = line.IndexOfAny([';', '#'], start); if (end < 0) end = line.Length;
            while (end > start && char.IsWhiteSpace(line[end - 1])) end--;
            var quoted = end - start >= 2 && line[start] == '"' && line[end - 1] == '"';
            _lines[index] = line[..start] + (quoted ? "\"" + Encode(value) + "\"" : Encode(value)) + line[end..];
        }
        else
        {
            var header = _lines.FindIndex(line => string.Equals(Section(line), section, StringComparison.OrdinalIgnoreCase));
            if (header >= 0)
            {
                var next = _lines.FindIndex(header + 1, line => Section(line) is not null);
                InsertLine(next < 0 ? _lines.Count : next, key + "=" + Encode(value));
            }
            else { InsertLine(_lines.Count, "[" + section + "]"); InsertLine(_lines.Count, key + "=" + Encode(value)); }
        }
    }
    /// <summary>Invalid values back to their defaults; a key's later duplicate lines become comments (kept, never deleted).</summary>
    public void Repair()
    {
        foreach (var issue in Issues.Where(i => i.Kind == "duplicate"))
        {
            var option = Options(_manifest).Single(o => o.GetProperty("id").GetString() == issue.OptionId);
            var lines = Lines(option.GetProperty("section").GetString()!, option.GetProperty("key").GetString()!);
            foreach (var index in lines.Skip(1)) _lines[index] = "; " + _lines[index];
        }
        foreach (var issue in Issues.Where(i => i.Kind == "invalid"))
        {
            var option = Options(_manifest).Single(o => o.GetProperty("id").GetString() == issue.OptionId);
            Set(issue.OptionId, Encode(option.TryGetProperty("lockedValue", out var locked) ? locked : option.GetProperty("default")));
        }
    }

    public void EnforceLocks()
    {
        foreach (var option in Options(_manifest))
            if (option.TryGetProperty("lockedValue", out var value)) Set(option.GetProperty("id").GetString()!, Encode(value));
    }
    public byte[] ToBytes()
    {
        var text = new StringBuilder();
        for (var i = 0; i < _lines.Count; i++) text.Append(_lines[i]).Append(_endings[i]);
        return [.. _encoding.GetPreamble(), .. _encoding.GetBytes(text.ToString())];
    }
    private void InsertLine(int index, string text)
    {
        if (index < _lines.Count) { _lines.Insert(index, text); _endings.Insert(index, _newline); return; }
        var lastHadNone = _lines.Count > 0 && _endings[^1].Length == 0;
        if (lastHadNone) _endings[^1] = _newline;
        _lines.Add(text); _endings.Add(lastHadNone ? "" : _newline);
    }
    private (Dictionary<string, JsonElement> Values, List<ManifestIniIssue> Issues) Read()
    {
        var result = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        var issues = new List<ManifestIniIssue>();
        foreach (var option in Options(_manifest))
        {
            var id = option.GetProperty("id").GetString()!;
            var lines = Lines(option.GetProperty("section").GetString()!, option.GetProperty("key").GetString()!);
            if (lines.Count > 1) issues.Add(new(id, "duplicate"));
            JsonElement value = option.GetProperty("default").Clone();
            if (lines.Count > 0)
            {
                try
                {
                    var parsed = Parse(option, _lines[lines[0]][(_lines[lines[0]].IndexOf('=') + 1)..].Split(';', '#')[0].Trim().Trim('"'), null);
                    ModMultiplayer.ValidateValue(option, parsed);
                    value = parsed;
                }
                catch (ModException) { issues.Add(new(id, "invalid")); }
            }
            result.Add(id, value);
        }
        return (result, issues);
    }
    private int Find(string section, string key) => Lines(section, key) is { Count: > 0 } found ? found[0] : -1;
    private List<int> Lines(string section, string key)
    {
        var active = ""; var found = new List<int>();
        for (var i = 0; i < _lines.Count; i++)
        {
            var line = _lines[i].Trim();
            if (Section(line) is { } name) active = name;
            else if (string.Equals(active, section, StringComparison.OrdinalIgnoreCase) && !line.StartsWith(';') && !line.StartsWith('#')
                && line.IndexOf('=') is var at && at > 0 && string.Equals(line[..at].Trim(), key, StringComparison.OrdinalIgnoreCase))
                found.Add(i);
        }
        return found;
    }
    private static string? Section(string line)
    {
        line = line.Trim(); var end = line.IndexOf(']');
        return line.StartsWith('[') && end > 1 ? line[1..end].Trim() : null;
    }
    private static JsonElement Parse(JsonElement option, string value, CultureInfo? culture) => option.GetProperty("type").GetString() switch
    {
        "boolean" => JsonSerializer.SerializeToElement(value.Trim() switch { "1" or "true" => true, "0" or "false" => false, _ => throw new ModException("invalid switch", ModErrors.Settings) }),
        "number" or "integer" => culture is not null && decimal.TryParse(value.Trim(), NumberStyles.AllowDecimalPoint | NumberStyles.AllowLeadingSign, culture, out var local)
            ? JsonSerializer.SerializeToElement(local)
            : decimal.TryParse(value.Trim(), NumberStyles.AllowDecimalPoint | NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var number)
            ? JsonSerializer.SerializeToElement(number) : throw new ModException("invalid number", ModErrors.Settings),
        "enum" => JsonSerializer.SerializeToElement(value),
        _ => throw new ModException("unsupported option"),
    };
    /// <summary>The on-disk text of a value: 0/1, the exact enum token, or an invariant number.</summary>
    public static string Encode(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.True => "1", JsonValueKind.False => "0", JsonValueKind.String => value.GetString()!,
        JsonValueKind.Number when value.TryGetDecimal(out var number) => number.ToString(CultureInfo.InvariantCulture),
        _ => value.GetRawText(),
    };

    /// <summary>The form text of a value: numbers in <paramref name="culture"/> (pt-BR shows 0,5).</summary>
    public static string Display(JsonElement value, CultureInfo culture) =>
        value.ValueKind == JsonValueKind.Number && value.TryGetDecimal(out var number) ? number.ToString(culture) : Encode(value);
    private static JsonElement.ArrayEnumerator Options(ModManifest manifest) => manifest.Document.GetProperty("config").GetProperty("options").EnumerateArray();
}
