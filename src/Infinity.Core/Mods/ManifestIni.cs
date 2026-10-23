// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>Strict, manifest-bound INI editor. Preserves unknown lines; rejects ambiguous known keys.</summary>
public sealed class ManifestIni
{
    private readonly ModManifest _manifest;
    private readonly List<string> _lines;
    private readonly string _newline;
    private readonly List<string> _endings = [];
    private readonly Encoding _encoding;
    public string SourceHash { get; }
    internal string ManifestDigest => _manifest.Digest;
    public IReadOnlyDictionary<string, JsonElement> Values => Read();
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
        var sections = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var line in _lines) if (Section(line) is { } section && !sections.Add(section)) throw new ModException("duplicate settings section");
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
    public void Set(string id, string text)
    {
        var option = Options(_manifest).Single(o => o.GetProperty("id").GetString() == id);
        var value = Parse(option, text);
        ModMultiplayer.ValidateValue(option, value);
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
    private Dictionary<string, JsonElement> Read()
    {
        var result = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        foreach (var option in Options(_manifest))
        {
            var index = Find(option.GetProperty("section").GetString()!, option.GetProperty("key").GetString()!);
            var value = index < 0 ? option.GetProperty("default").Clone() : Parse(option, _lines[index][(_lines[index].IndexOf('=') + 1)..].Split(';', '#')[0].Trim().Trim('"'));
            ModMultiplayer.ValidateValue(option, value);
            result.Add(option.GetProperty("id").GetString()!, value);
        }
        return result;
    }
    private int Find(string section, string key)
    {
        var active = ""; var found = -1;
        for (var i = 0; i < _lines.Count; i++)
        {
            var line = _lines[i].Trim();
            if (Section(line) is { } name) active = name;
            else if (string.Equals(active, section, StringComparison.OrdinalIgnoreCase) && line.IndexOf('=') is var at && at > 0
                && string.Equals(line[..at].Trim(), key, StringComparison.OrdinalIgnoreCase))
            { if (found >= 0) throw new ModException("duplicate settings key"); found = i; }
        }
        return found;
    }
    private static string? Section(string line)
    {
        line = line.Trim(); var end = line.IndexOf(']');
        return line.StartsWith('[') && end > 1 ? line[1..end].Trim() : null;
    }
    private static JsonElement Parse(JsonElement option, string value) => option.GetProperty("type").GetString() switch
    {
        "boolean" => JsonSerializer.SerializeToElement(value switch { "1" or "true" => true, "0" or "false" => false, _ => throw new ModException("invalid switch") }),
        "number" or "integer" => decimal.TryParse(value, NumberStyles.AllowDecimalPoint | NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var number)
            ? JsonSerializer.SerializeToElement(number) : throw new ModException("invalid number"),
        "enum" => JsonSerializer.SerializeToElement(value),
        _ => throw new ModException("unsupported option"),
    };
    public static string Encode(JsonElement value) => value.ValueKind switch
    { JsonValueKind.True => "1", JsonValueKind.False => "0", JsonValueKind.String => value.GetString()!, _ => value.GetRawText() };
    private static JsonElement.ArrayEnumerator Options(ModManifest manifest) => manifest.Document.GetProperty("config").GetProperty("options").EnumerateArray();
}
