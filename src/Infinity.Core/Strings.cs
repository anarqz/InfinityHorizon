// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;

namespace Infinity.Core;

/// <summary>
/// The launcher's player-facing strings in English and pt-BR, from the catalogs the Rust launcher
/// already uses (<c>apps/launcher/i18n/*.json</c>, flat <c>key → text</c> with <c>{name}</c>
/// placeholders). A key missing in pt-BR falls back to English, then to the key itself.
/// </summary>
public sealed class Strings
{
    private readonly IReadOnlyDictionary<string, string> _chosen;
    private readonly IReadOnlyDictionary<string, string> _english;

    private Strings(string locale, IReadOnlyDictionary<string, string> chosen, IReadOnlyDictionary<string, string> english)
    {
        Locale = locale;
        _chosen = chosen;
        _english = english;
    }

    /// <summary><c>en</c> or <c>pt-BR</c>.</summary>
    public string Locale { get; }

    /// <summary><c>pt-BR</c> for any Portuguese UI culture, otherwise <c>en</c>.</summary>
    public static string LocaleFor(CultureInfo culture)
    {
        ArgumentNullException.ThrowIfNull(culture);
        return culture.TwoLetterISOLanguageName == "pt" ? "pt-BR" : "en";
    }

    public static Strings Load(string directory, string locale)
    {
        var english = Read(Path.Combine(directory, "en.json"));
        var chosen = locale == "en" ? english : Read(Path.Combine(directory, $"{locale}.json"));
        return new Strings(locale, chosen, english);
    }

    /// <summary>
    /// These strings with the keys of another catalog folder on top (the Windows launcher's own
    /// <c>i18n-win</c>, as <c>apps/launcher</c> is a frozen reference). Same fallback rules.
    /// </summary>
    public Strings With(string directory)
    {
        var english = Merge(_english, Read(Path.Combine(directory, "en.json")));
        var chosenFile = Path.Combine(directory, $"{Locale}.json");
        var chosen = Locale == "en" ? english : Merge(_chosen, File.Exists(chosenFile) ? Read(chosenFile) : []);
        return new Strings(Locale, chosen, english);
    }

    private static Dictionary<string, string> Merge(IReadOnlyDictionary<string, string> under, Dictionary<string, string> over)
    {
        var all = new Dictionary<string, string>(under);
        foreach (var (key, text) in over)
        {
            all[key] = text;
        }
        return all;
    }

    public static Strings FromJson(string locale, string chosenJson, string englishJson) =>
        new(locale, Parse(chosenJson), Parse(englishJson));

    public string this[string key] => _chosen.TryGetValue(key, out var text) || _english.TryGetValue(key, out text) ? text : key;

    /// <summary>The text of <paramref name="key"/> with each <c>{name}</c> replaced.</summary>
    public string Format(string key, params (string Name, string Value)[] args)
    {
        ArgumentNullException.ThrowIfNull(args);
        var text = this[key];
        foreach (var (name, value) in args)
        {
            text = text.Replace("{" + name + "}", value, StringComparison.Ordinal);
        }
        return text;
    }

    private static Dictionary<string, string> Read(string path) => Parse(File.ReadAllText(path));

    private static Dictionary<string, string> Parse(string json) =>
        JsonSerializer.Deserialize<Dictionary<string, string>>(json) ?? [];
}
