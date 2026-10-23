// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed record ModMultiplayer(string Impact, string CompatibilityId, int Revision, IReadOnlyList<string> Settings)
{
    internal static ModMultiplayer? Parse(JsonElement document)
    {
        var isNative = document.TryGetProperty("runtime", out var runtime) && runtime.GetString() == "infinity-native";
        if (!document.TryGetProperty("multiplayer", out var value))
        {
            if (isNative) throw new ModException("native mod needs multiplayer metadata");
            return null;
        }
        if (value.ValueKind != JsonValueKind.Object || value.EnumerateObject().Count() != 6
            || value.GetProperty("schemaVersion").GetInt32() != 1 || value.GetProperty("match").GetString() != "exact")
            throw new ModException("unsupported multiplayer metadata");
        var impact = value.GetProperty("impact").GetString()!;
        var id = value.GetProperty("compatibilityId").GetString()!;
        var revision = value.GetProperty("revision").GetInt32();
        var settings = value.GetProperty("settings").EnumerateArray().Select(s => s.GetString()!).ToArray();
        if (impact is not ("gameplay" or "cosmetic" or "unknown") || !Identifier(id) || revision < 1
            || settings.Length > 64 || settings.Any(s => !Identifier(s)) || settings.Distinct(StringComparer.Ordinal).Count() != settings.Length)
            throw new ModException("invalid multiplayer metadata");
        var options = document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
            ? config.GetProperty("options").EnumerateArray().ToArray() : [];
        if (options.Select(o => o.GetProperty("id").GetString()).Distinct(StringComparer.Ordinal).Count() != options.Length
            || settings.Any(s => !options.Any(o => o.GetProperty("id").GetString() == s)))
            throw new ModException("unknown multiplayer setting");
        if (isNative)
        {
            var native = document.GetProperty("nativeHost");
            if (native.GetProperty("abi").GetInt32() != 1 || native.GetProperty("runtime").GetString() != "infinity_mod.dll"
                || native.GetProperty("entryPoint").GetString() != "InfinityModInitializeV1") throw new ModException("unsupported native ABI");
            var binary = native.GetProperty("binary").GetString()!;
            ModManifest.ValidatePath(binary);
            if (!document.GetProperty("files").GetProperty("azimuth").EnumerateArray().Any(f =>
                f.GetProperty("destination").GetString() == binary && f.GetProperty("role").GetString() == "binary"))
                throw new ModException("native binary missing from mappings");
            if (document.GetProperty("config").ValueKind != JsonValueKind.Object) throw new ModException("native settings required");
        }
        return new(impact, id, revision, Array.AsReadOnly(settings));
    }

    /// <summary>
    /// The one classification shared by the lobby mod set (OD-112, ModSets) and Hardcore profiles (OD-112
    /// follow-up): a mod affects multiplayer exactly when its manifest declares <c>multiplayer</c> with an
    /// impact other than <c>cosmetic</c> (<c>gameplay</c> and <c>unknown</c> count). A manifest without
    /// <c>multiplayer</c> (a client-only mod; native mods must declare it) does not.
    /// </summary>
    public bool EntersLobbySet => Impact != "cosmetic";

    /// <summary>See <see cref="EntersLobbySet"/>: a Hardcore profile refuses exactly these mods.</summary>
    public static bool AffectsMultiplayer(ModManifest manifest) =>
        (manifest ?? throw new ArgumentNullException(nameof(manifest))).Multiplayer is { EntersLobbySet: true };

    private static bool Identifier(string? value) => value is { Length: > 0 and <= 64 }
        && value.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');

    public string Fingerprint(ModManifest manifest, IReadOnlyDictionary<string, JsonElement> effectiveSettings)
    {
        if (manifest.Multiplayer != this) throw new ModException("multiplayer metadata does not belong to manifest");
        var document = manifest.Document;
        var options = document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
            ? config.GetProperty("options").EnumerateArray().ToDictionary(o => o.GetProperty("id").GetString()!, StringComparer.Ordinal)
            : new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        using var output = new MemoryStream();
        using (var writer = new Utf8JsonWriter(output))
        {
            writer.WriteStartObject();
            writer.WriteString("format", "infinity-mod-compatibility-v1");
            writer.WriteString("id", manifest.Id); writer.WriteString("version", manifest.Version);
            writer.WriteString("runtime", document.TryGetProperty("runtime", out var runtime) ? runtime.GetString() : "unknown");
            writer.WriteString("compatibilityId", CompatibilityId); writer.WriteNumber("revision", Revision);
            writer.WriteString("impact", Impact);
            writer.WritePropertyName("game"); Canonical(writer, document.GetProperty("compatibility"));
            writer.WritePropertyName("nativeHost");
            if (document.TryGetProperty("nativeHost", out var host)) Canonical(writer, host); else writer.WriteNullValue();
            writer.WriteStartArray("files");
            foreach (var file in manifest.Files.Where(f => f.Role != "license").OrderBy(f => f.Root, StringComparer.Ordinal).ThenBy(f => f.Destination, StringComparer.Ordinal))
            {
                writer.WriteStartArray(); writer.WriteStringValue(file.Root); writer.WriteStringValue(file.Destination);
                writer.WriteStringValue(file.Hash); writer.WriteNumberValue(file.Bytes); writer.WriteStringValue(file.Mode); writer.WriteEndArray();
            }
            writer.WriteEndArray();
            writer.WriteStartArray("dependencies");
            foreach (var dependency in document.GetProperty("dependencies").EnumerateArray().OrderBy(d => d.GetProperty("id").GetString(), StringComparer.Ordinal)) Canonical(writer, dependency);
            writer.WriteEndArray();
            foreach (var field in new[] { "database", "launch", "readiness", "incompatibleFiles" })
                if (document.TryGetProperty(field, out var capability)) { writer.WritePropertyName(field); Canonical(writer, capability); }
            writer.WritePropertyName("migrations"); Canonical(writer, document.GetProperty("databaseMigrations"));
            writer.WriteStartObject("config");
            if (document.GetProperty("config") is { ValueKind: JsonValueKind.Object } settingsConfig)
                foreach (var field in new[] { "root", "path", "format" })
                    if (settingsConfig.TryGetProperty(field, out var property)) { writer.WritePropertyName(field); Canonical(writer, property); }
            writer.WriteStartArray("bindings");
            foreach (var key in Settings.Order(StringComparer.Ordinal))
            {
                var option = options[key]; writer.WriteStartObject();
                foreach (var field in new[] { "id", "section", "key", "type", "encoding", "default", "minimum", "maximum", "step", "decimals" })
                    if (option.TryGetProperty(field, out var property)) { writer.WritePropertyName(field); Canonical(writer, property); }
                if (option.TryGetProperty("choices", out var choices))
                {
                    writer.WriteStartArray("choices");
                    foreach (var choice in choices.EnumerateArray()) Canonical(writer, choice.GetProperty("value"));
                    writer.WriteEndArray();
                }
                writer.WriteEndObject();
            }
            writer.WriteEndArray(); writer.WriteEndObject();
            writer.WriteStartObject("settings");
            foreach (var key in Settings.Order(StringComparer.Ordinal))
            {
                if (!effectiveSettings.TryGetValue(key, out var value)) throw new ModException("missing effective multiplayer setting");
                ValidateValue(options[key], value);
                writer.WritePropertyName(key); Canonical(writer, value);
            }
            writer.WriteEndObject(); writer.WriteEndObject();
        }
        return ModManifest.Hash(output.ToArray());
    }

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

    private static void Canonical(Utf8JsonWriter writer, JsonElement value)
    {
        switch (value.ValueKind)
        {
            case JsonValueKind.Object:
                writer.WriteStartObject();
                foreach (var property in value.EnumerateObject().OrderBy(p => p.Name, StringComparer.Ordinal))
                { writer.WritePropertyName(property.Name); Canonical(writer, property.Value); }
                writer.WriteEndObject(); break;
            case JsonValueKind.Array:
                writer.WriteStartArray(); foreach (var item in value.EnumerateArray()) Canonical(writer, item); writer.WriteEndArray(); break;
            case JsonValueKind.Number:
                if (!value.TryGetDecimal(out var number)) throw new ModException("unsupported compatibility number");
                writer.WriteRawValue(number.ToString("G29", CultureInfo.InvariantCulture)); break;
            default: value.WriteTo(writer); break;
        }
    }
}
