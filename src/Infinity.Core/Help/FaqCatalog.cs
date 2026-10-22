// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Infinity.Core.Help;

[JsonUnmappedMemberHandling(JsonUnmappedMemberHandling.Disallow)]
public sealed record FaqArticle(
    [property: JsonPropertyName("title_en"), JsonRequired] string TitleEn,
    [property: JsonPropertyName("title_br"), JsonRequired] string TitleBr,
    [property: JsonPropertyName("descr_en"), JsonRequired] string DescriptionEn,
    [property: JsonPropertyName("descr_br"), JsonRequired] string DescriptionBr,
    [property: JsonPropertyName("keywords"), JsonRequired] string[] Keywords,
    [property: JsonPropertyName("discordLink"), JsonRequired] string? DiscordLink)
{
    public string Title(string locale) => locale == "pt-BR" ? TitleBr : TitleEn;
    public string Description(string locale) => locale == "pt-BR" ? DescriptionBr : DescriptionEn;
}

public static class FaqCatalog
{
    private static readonly Lazy<IReadOnlyList<FaqArticle>> Bundled = new(() =>
    {
        using var stream = typeof(FaqCatalog).Assembly.GetManifestResourceStream("Infinity.Core.Help.faq.json")
            ?? throw new InvalidOperationException("Missing FAQ catalog.");
        using var reader = new StreamReader(stream);
        return Parse(reader.ReadToEnd());
    });

    public static IReadOnlyList<FaqArticle> Articles => Bundled.Value;

    public static IReadOnlyList<FaqArticle> Parse(string json)
    {
        var articles = JsonSerializer.Deserialize<FaqArticle[]>(json) ?? throw new JsonException("Expected FAQ array.");
        foreach (var item in articles)
        {
            if (item is null || string.IsNullOrWhiteSpace(item.TitleEn) || string.IsNullOrWhiteSpace(item.TitleBr) ||
                string.IsNullOrWhiteSpace(item.DescriptionEn) || string.IsNullOrWhiteSpace(item.DescriptionBr) ||
                item.Keywords is null || item.Keywords.Any(string.IsNullOrWhiteSpace) ||
                item.DiscordLink is { } link && !IsDiscordLink(link))
                throw new JsonException("Invalid FAQ article.");
        }
        return Array.AsReadOnly(articles);
    }

    public static bool IsDiscordLink(string url) => Uri.TryCreate(url, UriKind.Absolute, out var uri) &&
        uri.Scheme == Uri.UriSchemeHttps && uri.IsDefaultPort && uri.UserInfo.Length == 0 &&
        uri.Host is "discord.gg" or "discord.com" && uri.AbsolutePath.Length > 1;

    public static IEnumerable<FaqArticle> Search(string? query)
    {
        var terms = (query ?? string.Empty).Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries);
        return Articles.Where(article =>
        {
            var text = string.Join(' ', article.TitleEn, article.TitleBr,
                string.Concat(FaqMarkup.Parse(article.DescriptionEn).Select(s => s.Text)),
                string.Concat(FaqMarkup.Parse(article.DescriptionBr).Select(s => s.Text)), string.Join(' ', article.Keywords));
            return terms.All(term => CultureInfo.InvariantCulture.CompareInfo.IndexOf(text, term,
                CompareOptions.IgnoreCase | CompareOptions.IgnoreNonSpace) >= 0);
        });
    }
}
