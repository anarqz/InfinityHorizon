// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>
/// A manifest's <c>multiplayer</c> metadata: whether the mod changes anything for other players (Hardcore profiles refuse it,
/// OD-112 follow-up). The lobby set itself comes from what ExtraOptions loads (<c>ExtraOptionsModSetSource</c>), not from it.
/// </summary>
public sealed record ModMultiplayer(string Impact, string CompatibilityId, int Revision, IReadOnlyList<string> Settings)
{
    /// <param name="legacyCustom">An installed OD-114 custom mod (retired by OD-232), whose compatibility id carries the custom prefix.</param>
    internal static ModMultiplayer? Parse(JsonElement document, bool legacyCustom = false)
    {
        if (!document.TryGetProperty("multiplayer", out var value)) return null;
        if (value.ValueKind != JsonValueKind.Object || value.EnumerateObject().Count() != 6
            || value.GetProperty("schemaVersion").GetInt32() != 1 || value.GetProperty("match").GetString() != "exact")
            throw new ModException("unsupported multiplayer metadata");
        var impact = value.GetProperty("impact").GetString()!;
        var id = value.GetProperty("compatibilityId").GetString()!;
        var revision = value.GetProperty("revision").GetInt32();
        var settings = value.GetProperty("settings").EnumerateArray().Select(s => s.GetString()!).ToArray();
        if (impact is not ("gameplay" or "cosmetic" or "unknown") || (legacyCustom ? !ModManifest.LegacyCustomId(id) : !Identifier(id)) || revision < 1
            || settings.Length > 64 || settings.Any(s => !Identifier(s)) || settings.Distinct(StringComparer.Ordinal).Count() != settings.Length)
            throw new ModException("invalid multiplayer metadata");
        var options = document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
            ? config.GetProperty("options").EnumerateArray().ToArray() : [];
        if (options.Select(o => o.GetProperty("id").GetString()).Distinct(StringComparer.Ordinal).Count() != options.Length
            || settings.Any(s => !options.Any(o => o.GetProperty("id").GetString() == s)))
            throw new ModException("unknown multiplayer setting");
        return new(impact, id, revision, Array.AsReadOnly(settings));
    }

    /// <summary>
    /// The one classification Hardcore profiles use (OD-112 follow-up): a mod affects multiplayer exactly when its manifest declares
    /// <c>multiplayer</c> with an impact other than <c>cosmetic</c> (<c>gameplay</c> and <c>unknown</c> count). A manifest without
    /// <c>multiplayer</c> (a client-only mod) does not.
    /// </summary>
    public bool EntersLobbySet => Impact != "cosmetic";

    /// <summary>See <see cref="EntersLobbySet"/>: the mods of the declared set (Hardcore's rule and the <c>modded</c> flag: <see cref="CountsAsModded"/>).</summary>
    public static bool AffectsMultiplayer(ModManifest manifest) =>
        (manifest ?? throw new ArgumentNullException(nameof(manifest))).Multiplayer is { EntersLobbySet: true };

    /// <summary>
    /// OD-236: whether the mod makes a session <c>modded</c> (<see cref="ModManifest.CountsAsModded"/>, the catalog's <c>countsAsModded</c>).
    /// <paramref name="catalog"/> (<see cref="ModdedPolicy"/>): the catalog entry of the same id decides for an installed copy, whose snapshot
    /// may predate the declaration (the field is no content, <see cref="ModManifest.SameContent"/>); a mod the catalog does not list keeps
    /// its own. No mod id is named anywhere: the catalog data decides.
    /// </summary>
    public static bool CountsAsModded(ModManifest manifest, IReadOnlyDictionary<string, bool>? catalog = null)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        return catalog is not null && catalog.TryGetValue(manifest.Id, out var listed) ? listed : manifest.CountsAsModded;
    }

    /// <summary>
    /// OD-236: a Hardcore profile keeps exactly the mods that do not count as modded (spotify-radio, garage-revving and the car packs in the
    /// official catalog); every other mod plays in Noob only (OD-233). See <see cref="CountsAsModded"/>.
    /// </summary>
    public static bool AllowedInHardcore(ModManifest manifest, IReadOnlyDictionary<string, bool>? catalog = null) => !CountsAsModded(manifest, catalog);

    /// <summary>The catalog's <c>countsAsModded</c> by mod id, for <see cref="CountsAsModded"/> (an immutable copy: safe off the UI thread).</summary>
    public static IReadOnlyDictionary<string, bool> ModdedPolicy(IEnumerable<ModManifest> catalog)
    {
        ArgumentNullException.ThrowIfNull(catalog);
        var policy = new Dictionary<string, bool>(StringComparer.Ordinal);
        foreach (var manifest in catalog) policy.TryAdd(manifest.Id, manifest.CountsAsModded);
        return policy;
    }

    private static bool Identifier(string? value) => value is { Length: > 0 and <= 64 }
        && value.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');

    internal static void ValidateValue(JsonElement option, JsonElement value)
    {
        var type = option.GetProperty("type").GetString();
        if (type == "boolean" && value.ValueKind is JsonValueKind.True or JsonValueKind.False) return;
        if (type is "integer" or "number" && value.ValueKind == JsonValueKind.Number && value.TryGetDecimal(out var number)
            && (type != "integer" || decimal.Truncate(number) == number)
            && number >= option.GetProperty("minimum").GetDecimal() && number <= option.GetProperty("maximum").GetDecimal()
            && option.GetProperty("step").GetDecimal() > 0
            && (number - option.GetProperty("minimum").GetDecimal()) % option.GetProperty("step").GetDecimal() == 0) return;
        if (type == "enum" && value.ValueKind == JsonValueKind.String && option.GetProperty("choices").EnumerateArray()
            .Any(c => c.GetProperty("value").GetString() == value.GetString())) return;
        throw new ModException("invalid effective multiplayer setting");
    }
}
