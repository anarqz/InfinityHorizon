// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;
using System.Text.Json;
using Infinity.Core.Update;

namespace Infinity.Core.News;

/// <summary>A text in English and/or Brazilian Portuguese; a missing language falls back to the other (NEWS_FEED.md §2).</summary>
public sealed record NewsLocalized(string? En, string? PtBr)
{
    public string For(string locale) => (locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? PtBr ?? En : En ?? PtBr) ?? "";
}

/// <summary>One entry of <c>news.json</c>. <see cref="Date"/> stays the published <c>YYYY-MM-DD</c> text (it sorts as written).</summary>
public sealed record NewsItem(string Id, string Date, NewsLocalized Title, NewsLocalized Summary, string? Cover, bool Pinned, LauncherVersion? MinLauncher)
{
    public DateOnly? Day => DateOnly.TryParseExact(Date, "yyyy-MM-dd", out var day) ? day : null;
}

/// <summary>
/// OD-119: the editorial news feed of <c>anarqz/InfinityNews</c> (docs/contracts/NEWS_FEED.md). We write the news ourselves, so the
/// reading is lenient (Alex): unknown fields are ignored and an item that cannot be shown is skipped, never the whole feed.
/// </summary>
public sealed class NewsFeed
{
    public const int SchemaVersion = 1;
    /// <summary>Generous caps so a mistake never eats memory (not a content rule).</summary>
    public const int MaxIndexBytes = 1024 * 1024, MaxMarkdownBytes = 1024 * 1024, MaxImageBytes = 8 * 1024 * 1024, MaxItems = 200;

    public IReadOnlyList<NewsItem> Items { get; }
    /// <summary>Items that could not be shown (no usable id or title), for the log.</summary>
    public int Skipped { get; }

    private NewsFeed(IReadOnlyList<NewsItem> items, int skipped) { Items = items; Skipped = skipped; }

    /// <summary>An item id usable as a folder name under <c>news/</c> (no separators, no dot segments).</summary>
    public static bool IsId(string? id) => !string.IsNullOrWhiteSpace(id) && id.Length <= 128 && id is not ("." or "..")
        && id.IndexOfAny(['/', '\\', ':', '?', '#']) < 0;

    /// <summary>A path relative to the item's folder that stays inside it (a cover or a markdown image).</summary>
    public static bool IsItemPath(string? path) => ItemPath(path) is not null;

    /// <summary>The path relative to the item's folder (a leading <c>./</c> dropped), or null when it would leave the folder.</summary>
    public static string? ItemPath(string? path)
    {
        if (path is not null && path.StartsWith("./", StringComparison.Ordinal)) path = path[2..];
        return Inside(path) ? path : null;
    }

    private static bool Inside(string? path) => !string.IsNullOrWhiteSpace(path) && !path.Contains('\\') && !path.Contains(':')
        && !path.StartsWith('/') && path.Split('/').All(s => s.Length > 0 && s is not ("." or ".."));

    /// <summary>Reads <c>news.json</c>; throws only when it is not a JSON object with an <c>items</c> list at all.</summary>
    public static NewsFeed Parse(ReadOnlySpan<byte> raw)
    {
        if (raw.StartsWith("﻿"u8)) raw = raw[3..];
        JsonDocument json;
        try { json = JsonDocument.Parse(raw.ToArray(), new JsonDocumentOptions { MaxDepth = 64, AllowTrailingCommas = true, CommentHandling = JsonCommentHandling.Skip }); }
        catch (JsonException) { throw new InvalidDataException("news.json is not JSON"); }
        using (json)
        {
            if (json.RootElement.ValueKind != JsonValueKind.Object || !json.RootElement.TryGetProperty("items", out var list) || list.ValueKind != JsonValueKind.Array)
                throw new InvalidDataException("news.json has no items list");
            var items = new List<NewsItem>(); var ids = new HashSet<string>(StringComparer.Ordinal); var skipped = 0;
            foreach (var element in list.EnumerateArray())
            {
                if (items.Count >= MaxItems) { skipped++; continue; }
                if (Item(element) is { } item && ids.Add(item.Id)) items.Add(item); else skipped++;
            }
            return new(items.AsReadOnly(), skipped);
        }
    }

    private static NewsItem? Item(JsonElement item)
    {
        if (item.ValueKind != JsonValueKind.Object) return null;
        var id = String(item, "id");
        var title = Text(item, "title");
        if (!IsId(id) || title is null) return null;
        var cover = ItemPath(String(item, "cover"));
        var pinned = item.TryGetProperty("pinned", out var p) && p.ValueKind == JsonValueKind.True;
        LauncherVersion? minimum = null;
        // A minLauncher that cannot be read hides the item (review): a typo must never show a gated item to older launchers.
        if (item.TryGetProperty("minLauncher", out var m) && !(m.ValueKind == JsonValueKind.String && LauncherVersion.TryParse(m.GetString(), out minimum))) return null;
        return new(id!, String(item, "date") ?? "", title, Text(item, "summary") ?? new(null, null), cover, pinned, minimum);
    }

    private static NewsLocalized? Text(JsonElement item, string name)
    {
        if (!item.TryGetProperty(name, out var text)) return null;
        if (text.ValueKind == JsonValueKind.String) return text.GetString() is { Length: > 0 } plain ? new(plain, null) : null;
        if (text.ValueKind != JsonValueKind.Object) return null;
        string? en = String(text, "en"), pt = String(text, "pt-BR");
        return en is { Length: > 0 } || pt is { Length: > 0 } ? new(NullIfEmpty(en), NullIfEmpty(pt)) : null;
    }

    private static string? NullIfEmpty(string? value) => string.IsNullOrWhiteSpace(value) ? null : value;

    private static string? String(JsonElement item, string name) =>
        item.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    /// <summary>What this launcher shows: items whose <c>minLauncher</c> it meets; pinned first, then date descending, then id.</summary>
    public IReadOnlyList<NewsItem> For(LauncherVersion? launcher) => Items
        .Where(i => i.MinLauncher is null || launcher is not null && launcher.CompareTo(i.MinLauncher) >= 0)
        .OrderByDescending(i => i.Pinned).ThenByDescending(i => i.Date, StringComparer.Ordinal).ThenBy(i => i.Id, StringComparer.Ordinal)
        .ToArray();

    /// <summary>A markdown file's bytes as text (a BOM is dropped; invalid bytes become replacement characters).</summary>
    public static string Markdown(ReadOnlySpan<byte> raw) => Encoding.UTF8.GetString(raw.StartsWith("﻿"u8) ? raw[3..] : raw);
}
