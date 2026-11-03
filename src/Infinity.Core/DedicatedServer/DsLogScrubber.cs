// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// What the launcher log gets from a child's stdout and stderr (DEDICATED_SERVER.md §2.3, §13): the engine promises no
/// identities there, and the launcher does not take it on trust. The per-launch secret and the pipe name are replaced;
/// IPv4/IPv6 addresses (with ports), runs of 6 or more digits (a XUID, a port pair), hex numbers (<c>0x…</c>) and long
/// tokens (a key, a grant) are masked. Canonical UUIDs (world ids) and 40-hex commit hashes are kept, and so are Rust
/// paths (<c>a::b</c>) and clock times. A stdout line is a JSON object: every key and string value is scrubbed, numbers
/// of XUID size (integer or float, 10^14 and above) become <c>"#"</c>, and the object is re-serialized, so nothing is cut
/// blindly. <see cref="Cap"/> bounds what is forwarded.
/// </summary>
public sealed partial class DsLogScrubber(string? secret, string? pipeName)
{
    /// <summary>The longest forwarded line (§2.3: the engine's own bound is 8 KiB a line).</summary>
    public const int MaxForwardedChars = 8 * 1024;

    public const string TruncatedMark = "…[truncated]";

    private readonly string? _secret = string.IsNullOrEmpty(secret) ? null : secret;
    private readonly string? _pipe = string.IsNullOrEmpty(pipeName) ? null : pipeName;
    private readonly string? _pipeHex = pipeName is { Length: > 32 } p ? p[^32..] : null;

    [GeneratedRegex(@"(?<![0-9A-Za-z-])(?:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}|[0-9a-f]{40})(?![0-9A-Za-z-])")]
    private static partial Regex Kept();

    [GeneratedRegex(@"\b[0-9]{1,3}(\.[0-9]{1,3}){3}(:[0-9]{1,5})?\b")]
    private static partial Regex Ipv4();

    [GeneratedRegex(@"(?<![0-9A-Za-z_:])\[?[0-9A-Fa-f]{0,4}(:[0-9A-Fa-f]{0,4}){2,7}\]?(:[0-9]{1,5})?(?![0-9A-Za-z_:])")]
    private static partial Regex Ipv6();

    [GeneratedRegex(@"0[xX][0-9A-Fa-f]{6,}")]
    private static partial Regex HexNumber();

    [GeneratedRegex("[A-Za-z0-9+/_=-]{32,}")]
    private static partial Regex LongToken();

    [GeneratedRegex("[0-9]{6,}")]
    private static partial Regex LongDigits();

    /// <summary>One text value (a log line of stderr, a JSON key or string).</summary>
    public string Text(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (_secret is not null)
        {
            text = text.Replace(_secret, "#secret", StringComparison.OrdinalIgnoreCase);
        }
        if (_pipe is not null)
        {
            text = text.Replace(_pipe, "#pipe", StringComparison.OrdinalIgnoreCase);
        }
        if (_pipeHex is not null)
        {
            text = text.Replace(_pipeHex, "#pipe", StringComparison.OrdinalIgnoreCase);
        }
        // world ids and commit hashes are not identities: set aside, restored after the masks
        var kept = new List<string>();
        text = Kept().Replace(text, m =>
        {
            kept.Add(m.Value);
            return "\u0001" + (kept.Count - 1).ToString(CultureInfo.InvariantCulture) + "\u0002";
        });
        text = Ipv4().Replace(text, "#ip");
        // an IPv6 address, not a clock (12:00:00) or a lone "::": "::" with a hex digit, a hex letter, or five colons or more
        text = Ipv6().Replace(text, m => m.Value.Contains("::", StringComparison.Ordinal) && m.Value.Any(char.IsAsciiHexDigit)
            || m.Value.Any(char.IsAsciiLetter) || m.Value.Count(c => c == ':') >= 5 ? "#ip" : m.Value);
        text = HexNumber().Replace(text, "#");
        text = LongToken().Replace(text, "#token");
        text = LongDigits().Replace(text, "#");
        for (var i = 0; i < kept.Count; i++)
        {
            text = text.Replace("\u0001" + i.ToString(CultureInfo.InvariantCulture) + "\u0002", kept[i], StringComparison.Ordinal);
        }
        return text;
    }

    /// <summary>A stdout line; null when it is not one JSON object or cannot be walked (the caller kills the child).</summary>
    public string? JsonLine(ReadOnlySpan<byte> line)
    {
#pragma warning disable CA1031 // anything unexpected (a duplicate key, a depth limit) is "not a JSON line": a violation, never a dead reader
        try
        {
            return JsonNode.Parse(line, documentOptions: new JsonDocumentOptions { MaxDepth = 64 }) is JsonObject obj ? Scrub(obj)!.ToJsonString() : null;
        }
        catch (Exception)
        {
            return null;
        }
#pragma warning restore CA1031
    }

    /// <summary>Bounds a forwarded line, marking the cut.</summary>
    public static string Cap(string line)
    {
        ArgumentNullException.ThrowIfNull(line);
        return line.Length <= MaxForwardedChars ? line : line[..(MaxForwardedChars - TruncatedMark.Length)] + TruncatedMark;
    }

    private JsonNode? Scrub(JsonNode? node)
    {
        switch (node)
        {
            case JsonObject obj:
                // rebuilt, so keys are scrubbed too ({"<xuid>": 1} leaks as much as a value)
                var clean = new JsonObject();
                foreach (var (key, value) in obj.ToArray())
                {
                    var name = Text(key);
                    while (clean.ContainsKey(name))
                    {
                        name += "~";
                    }
                    clean[name] = Scrub(value?.DeepClone());
                }
                return clean;
            case JsonArray array:
                var items = new JsonArray();
                foreach (var item in array.ToArray())
                {
                    items.Add(Scrub(item?.DeepClone()));
                }
                return items;
            case JsonValue value when value.GetValueKind() == JsonValueKind.String:
                return JsonValue.Create(Text(value.GetValue<string>()));
            case JsonValue value when value.GetValueKind() == JsonValueKind.Number:
                // a XUID as an integer, a float (2.5332748e15) or anything of that size
                return value.TryGetValue<double>(out var number) && Math.Abs(number) >= 1e14 ? JsonValue.Create("#") : value.DeepClone();
            default:
                return node?.DeepClone();
        }
    }
}
