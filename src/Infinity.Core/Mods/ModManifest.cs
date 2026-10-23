// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed class ModManifest
{
    private readonly byte[] _bytes;
    public JsonElement Document { get; }
    public string Id => Document.GetProperty("id").GetString()!;
    public string Version => Document.GetProperty("version").GetString()!;
    public string Digest { get; }
    /// <summary>The reserved prefix of custom (player-supplied, unreviewed) mod ids (OD-114); official ids never contain a dot.</summary>
    public const string CustomPrefix = "custom.";
    /// <summary>A custom mod (OD-114): loaded from the player's custom folder, never from a catalog; its payload is a local <c>files/</c> tree.</summary>
    public bool IsCustom => IsCustomId(Id);
    /// <summary>A custom mod that maps executable code (a native binary): only a development native runtime may load it.</summary>
    public bool HasExecutableCode => Files.Any(f => f.Role == "binary" || ExecutableExtension(f.Destination));
    /// <summary>A custom mod whose manifest still lists review blockers: listed, never installed or enabled.</summary>
    public bool CustomBlocked => IsCustom && Document.GetProperty("review").GetProperty("blockers").GetArrayLength() > 0;
    public string PackageUrl => Document.GetProperty("package").GetProperty("url").GetString()!;
    public string PackageHash => Document.GetProperty("package").GetProperty("sha256").GetString()!;
    public long PackageBytes => Document.GetProperty("package").GetProperty("bytes").GetInt64();
    public IReadOnlyList<ManifestFile> Files { get; }
    public IReadOnlyList<ManifestDependency> Dependencies { get; }
    /// <summary>Wine/Proton native-first overrides applied while the mod is enabled (launch.wineDllOverrides).</summary>
    public IReadOnlyList<ModWineOverride> WineDllOverrides { get; }
    /// <summary>Legacy parked copies (adoption.parkedFiles): mapped binary destination to its parked Azimuth-relative path.</summary>
    public IReadOnlyDictionary<string, string> ParkedFiles { get; }
    public ModMultiplayer? Multiplayer { get; }
    public bool NeedsDatabaseCoordination => Document.GetProperty("databaseMigrations").GetArrayLength() > 0
        || Document.GetProperty("runtimeOutputs").GetArrayLength() > 0;
    public string? ConfigPath => Document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
        ? config.GetProperty("path").GetString() : null;

    public bool IsNative => Document.TryGetProperty("runtime", out var value) && value.GetString() == "infinity-native";
    public bool SupportedForInstall => !NeedsDatabaseCoordination
        && Document.TryGetProperty("runtime", out var value) && value.GetString() is "standalone" or "infinity-native";

    public ModManifest(byte[] bytes)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        if (bytes.Length > 256 * 1024) throw new ModException("manifest too large");
        _bytes = (byte[])bytes.Clone();
        using var json = JsonDocument.Parse(_bytes, new JsonDocumentOptions { MaxDepth = 32 });
        RejectDuplicates(json.RootElement);
        Document = json.RootElement.Clone();
        if (Document.GetProperty("schemaVersion").GetInt32() != 1) throw new ModException("unsupported manifest");
        foreach (var field in new[] { "name", "description" })
        {
            var localized = Document.GetProperty(field);
            if (localized.ValueKind != JsonValueKind.Object) throw new ModException("invalid localized text");
            foreach (var value in localized.EnumerateObject()) ValidateText(value.Value);
            ValidateText(localized.GetProperty("en"));
        }
        foreach (var field in new[] { "version", "author", "license" }) ValidateText(Document.GetProperty(field));
        foreach (var version in Document.GetProperty("compatibility").GetProperty("azimuthVersions").EnumerateArray()) ValidateText(version);
        _ = NeedsDatabaseCoordination;
        if (ConfigPath is { } configPath)
        {
            ValidatePath(configPath);
            if (!configPath.EndsWith(".ini", StringComparison.OrdinalIgnoreCase)) throw new ModException("INI configuration must use an ini file");
            var config = Document.GetProperty("config");
            if (config.TryGetProperty("root", out var configRoot) && configRoot.GetString() != "azimuth"
                || config.TryGetProperty("format", out var configFormat) && configFormat.GetString() != "ini") throw new ModException("unsupported configuration format");
            if (config.TryGetProperty("options", out var options))
            {
                if (options.GetArrayLength() > 128) throw new ModException("too many settings");
                var ids = new HashSet<string>(StringComparer.Ordinal); var bindings = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
                foreach (var option in options.EnumerateArray())
                {
                    var id = option.GetProperty("id").GetString()!; var section = option.GetProperty("section").GetString()!; var key = option.GetProperty("key").GetString()!;
                    if (!ids.Add(id) || !bindings.Add(section + "\0" + key) || section.Length is < 1 or > 128 || key.Length is < 1 or > 128
                        || section.IndexOfAny(['\r','\n',']','[','\0']) >= 0 || key.IndexOfAny(['\r','\n','=','\0']) >= 0) throw new ModException("invalid setting binding");
                    if (option.GetProperty("type").GetString() == "enum")
                    {
                        var choices = option.GetProperty("choices").EnumerateArray().Select(c => c.GetProperty("value").GetString()!).ToArray();
                        if (choices.Length is < 1 or > 64 || choices.Distinct(StringComparer.Ordinal).Count() != choices.Length
                            || choices.Any(v => v.Length is < 1 or > 64 || v.Any(c => !char.IsAsciiLetterOrDigit(c) && c is not ('_' or '-'))))
                            throw new ModException("invalid enum token");
                    }
                    ModMultiplayer.ValidateValue(option, option.GetProperty("default"));
                    if (option.TryGetProperty("lockedValue", out var locked)) ModMultiplayer.ValidateValue(option, locked);
                    foreach (var field in new[] { "label", "description", "lockReason" })
                        if (option.TryGetProperty(field, out var text))
                        {
                            if (text.ValueKind != JsonValueKind.Object) throw new ModException("invalid localized text");
                            foreach (var value in text.EnumerateObject()) ValidateText(value.Value);
                        }
                    if (option.TryGetProperty("lockReason", out _) && !option.TryGetProperty("lockedValue", out _)) throw new ModException("lock reason without a locked value");
                }
            }
        }
        if (Document.TryGetProperty("database", out var db))
        {
            if (!IsNative || db.GetProperty("root").GetString() != "game" || db.GetProperty("access").GetString() != "startup"
                || db.GetProperty("revision").GetInt32() < 1 || db.GetProperty("tables").GetArrayLength() is < 1 or > 32) throw new ModException("unsupported database capability");
            var target = db.GetProperty("path").GetString()!; var seed = db.GetProperty("seedFrom").GetString()!;
            ValidatePath(target); ValidatePath(seed); if (target.Equals(seed, StringComparison.OrdinalIgnoreCase)) throw new ModException("session database must be distinct from the baseline");
            // OD-115: the runtime's session copy is game data the runtime owns, so it too is an override; the baseline is only read.
            if (!UnderMediaOverride(target)) throw new ModException("session database must be a media override", ModErrors.Unsupported);
            foreach (var table in db.GetProperty("tables").EnumerateArray())
            {
                var name = table.GetString()!;
                if (name.Length is < 1 or > 64 || name.StartsWith("sqlite_", StringComparison.OrdinalIgnoreCase)
                    || name.StartsWith("_infinity", StringComparison.OrdinalIgnoreCase) || name.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '_')) throw new ModException("invalid database table permission");
            }
        }
        if (Document.TryGetProperty("readiness", out var readiness))
        {
            ValidatePath(readiness.GetProperty("module").GetString()!); ValidatePath(readiness.GetProperty("log").GetString()!);
            if (readiness.GetProperty("contains").GetString() is not { Length: > 0 and <= 512 }) throw new ModException("invalid readiness marker");
        }
        if (!ValidId(Id)) throw new ModException("invalid mod id");
        if (IsCustom && (!Document.TryGetProperty("review", out var review) || !review.TryGetProperty("blockers", out var blockers)
            || blockers.ValueKind != JsonValueKind.Array || blockers.EnumerateArray().Any(b => b.ValueKind != JsonValueKind.String)))
            throw new ModException("invalid custom review", ModErrors.Unsupported);
        Digest = Hash(_bytes);
        foreach (var output in Document.GetProperty("runtimeOutputs").EnumerateArray())
            if (output.ValueKind == JsonValueKind.Object && output.TryGetProperty("root", out var outputRoot) && outputRoot.GetString() == "game"
                && (!output.TryGetProperty("path", out var outputPath) || outputPath.GetString() is not { } written || !UnderMediaOverride(written)))
                throw new ModException("runtime game output must be a media override", ModErrors.Unsupported);
        var reviewed = Document.TryGetProperty("review", out var reviewState) && reviewState.ValueKind == JsonValueKind.Object
            && reviewState.TryGetProperty("status", out var status) && status.GetString() == "reviewed";
        var files = new List<ManifestFile>();
        foreach (var root in new[] { "game", "azimuth" })
            foreach (var file in Document.GetProperty("files").GetProperty(root).EnumerateArray())
            {
                var item = new ManifestFile(root, file.GetProperty("source").GetString()!, file.GetProperty("destination").GetString()!,
                    file.GetProperty("sha256").GetString()!, file.GetProperty("bytes").GetInt64(),
                    file.GetProperty("role").GetString()!, file.GetProperty("mode").GetString()!,
                    file.TryGetProperty("package", out var package) ? package.GetString()! : "primary",
                    file.TryGetProperty("loader", out var loader) ? loader.ValueKind switch
                    {
                        JsonValueKind.True => true, JsonValueKind.False => false, _ => throw new ModException("invalid loader flag"),
                    } : false);
                ValidatePath(item.Source); ValidatePath(item.Destination);
                // OD-115/OD-111: a loader is a binary the game process loads by itself (a top-level proxy DLL of its root). Only a reviewed
                // official manifest may declare one; it is the only mapping allowed in the game root outside the media overrides.
                if (item.Loader && (IsCustom || !reviewed || item.Role != "binary" || item.Mode == "seed" || item.Destination.Contains('/')
                    || !item.Destination.EndsWith(".dll", StringComparison.OrdinalIgnoreCase)))
                    throw new ModException("a loader is a top-level binary of a reviewed official manifest", ModErrors.Unsupported);
                // OD-115: mods never write the game's own files; every game-root change is an override the game reads instead.
                if (item.Root == "game" && !item.Loader && !UnderMediaOverride(item.Destination))
                    throw new ModException("game files change only through media overrides", ModErrors.Unsupported);
                if (item.Bytes is < 0 or > 64 * 1024 * 1024 || (item.Hash.Length != 64 || item.Hash.Any(c => !char.IsAsciiHexDigit(c)))
                    || item.Mode is not ("add" or "replace" or "add-or-replace" or "seed")) throw new ModException("invalid file mapping");
                if (item.Role is not ("binary" or "config" or "asset" or "user-asset" or "license")
                    || item.Mode == "seed" && item.Role is not ("config" or "user-asset")
                    || item.Role == "config" && (item.Root != "azimuth" || item.Mode != "seed" || item.Destination != ConfigPath)
                    || item.Root == "azimuth" && item.Destination.Equals(ConfigPath, StringComparison.OrdinalIgnoreCase) && item.Role != "config")
                    throw new ModException("invalid file role or settings mapping");
                if (IsCustom && (item.Source != item.Root + "/" + item.Destination
                    || (item.Role == "binary" || ExecutableExtension(item.Destination)) && !IsNative))
                    throw new ModException("custom mods map their files tree and carry no executable code", ModErrors.Unsupported);
                files.Add(item);
            }
        if (files.Count > 512 || files.Select(f => f.Root + "/" + f.Destination).Distinct(StringComparer.OrdinalIgnoreCase).Count() != files.Count)
            throw new ModException("duplicate or excessive files");
        Files = files.AsReadOnly();
        Dependencies = Document.GetProperty("dependencies").EnumerateArray().Select(d => new ManifestDependency(
            d.GetProperty("id").GetString()!, d.GetProperty("versions").EnumerateArray().Select(v => v.GetString()!).ToArray())).ToArray();
        if (Dependencies.Count > 64 || Dependencies.Select(d => d.Id).Distinct(StringComparer.OrdinalIgnoreCase).Count() != Dependencies.Count
            || Dependencies.Any(d => d.Id.Equals(Id, StringComparison.OrdinalIgnoreCase) || !ValidId(d.Id) || IsCustomId(d.Id) && !IsCustom || d.Versions.Count == 0
                || d.Versions.Any(string.IsNullOrWhiteSpace))) throw new ModException("invalid dependencies");
        Multiplayer = ModMultiplayer.Parse(Document);
        if (Document.TryGetProperty("conflicts", out var conflicts) && conflicts.EnumerateArray().Any(c => c.GetString() is not { } other || !ValidId(other)))
            throw new ModException("invalid conflicts");
        if (IsCustom)
        {
            // A custom mod has no download: its payload is the player's own files tree next to its manifest.
            if (Document.TryGetProperty("package", out _) || Document.TryGetProperty("additionalPackages", out _)
                || !Document.TryGetProperty("payload", out var payload) || payload.ValueKind != JsonValueKind.Object
                || payload.EnumerateObject().Count() != 2 || payload.GetProperty("format").GetString() != "folder"
                || payload.GetProperty("path").GetString() != CustomPayloadFolder) throw new ModException("invalid custom payload", ModErrors.Unsupported);
        }
        else if (Document.TryGetProperty("payload", out _)) throw new ModException("only custom mods use a local payload", ModErrors.Unsupported);
        else ValidatePackage(Document.GetProperty("package"));
        var packageNames = new HashSet<string>(StringComparer.Ordinal) { "primary" };
        if (Document.TryGetProperty("additionalPackages", out var additional))
            foreach (var p in additional.EnumerateObject())
            {
                if (!packageNames.Add(p.Name) || packageNames.Count > 16) throw new ModException("invalid additional package");
                ValidatePackage(p.Value);
            }
        if (Files.Any(f => !packageNames.Contains(f.Package))) throw new ModException("unknown package mapping");
        var wine = new List<ModWineOverride>();
        if (Document.TryGetProperty("launch", out var launch) && launch.TryGetProperty("wineDllOverrides", out var overrides))
            foreach (var item in overrides.EnumerateArray())
            {
                // A bare module name is shorthand for { "module": name, "appDefaults": true }.
                var name = item.ValueKind == JsonValueKind.String ? item.GetString()! : item.GetProperty("module").GetString()!;
                var appDefaults = item.ValueKind != JsonValueKind.Object || !item.TryGetProperty("appDefaults", out var flag) || flag.GetBoolean();
                if (item.ValueKind == JsonValueKind.Object && item.EnumerateObject().Any(p => p.Name is not ("module" or "appDefaults")))
                    throw new ModException("unsupported DLL override field");
                if (name.Length is < 1 or > 64 || name.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '_' && c != '-')
                    || wine.Any(w => w.Module.Equals(name, StringComparison.OrdinalIgnoreCase))
                    || !Files.Any(f => f.Root == "azimuth" && f.Role == "binary" && f.Destination.Equals(name + ".dll", StringComparison.OrdinalIgnoreCase)))
                    throw new ModException("DLL override must name a mapped binary");
                wine.Add(new(name, appDefaults));
            }
        WineDllOverrides = wine.AsReadOnly();
        var parked = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        if (Document.TryGetProperty("adoption", out var adoption) && adoption.TryGetProperty("parkedFiles", out var parkedFiles))
            foreach (var item in parkedFiles.EnumerateArray())
            {
                var path = item.GetProperty("path").GetString()!; var destination = item.GetProperty("destination").GetString()!;
                ValidatePath(path);
                if (parked.Count >= 16 || !Files.Any(f => f.Root == "azimuth" && f.Role == "binary" && f.Destination == destination) || !parked.TryAdd(destination, path))
                    throw new ModException("parked legacy files must name a mapped azimuth binary");
            }
        ParkedFiles = parked;
        if (Document.TryGetProperty("incompatibleFiles", out var incompatible))
            foreach (var file in incompatible.EnumerateArray())
            {
                if (file.GetProperty("root").GetString() is not ("game" or "azimuth")) throw new ModException("invalid incompatible file root");
                ValidatePath(file.GetProperty("path").GetString()!);
            }
    }

    private static void ValidatePackage(JsonElement package)
    {
        var hash = package.GetProperty("sha256").GetString()!;
        if (package.GetProperty("bytes").GetInt64() is <= 0 or > 64 * 1024 * 1024 || hash.Length != 64
            || hash.Any(c => !char.IsAsciiHexDigit(c) || char.IsUpper(c))
            || !Uri.TryCreate(package.GetProperty("url").GetString(), UriKind.Absolute, out var url)
            || url.Scheme != "https" || url.UserInfo.Length > 0 || url.Fragment.Length > 0) throw new ModException("invalid package");
    }

    private static void ValidateText(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.String || value.GetString() is not { Length: > 0 and <= 4096 })
            throw new ModException("invalid manifest text");
    }

    /// <summary>
    /// OD-115: the game-root folder whose files the game reads in place of its own media (<c>mediaoverride/X</c> over
    /// <c>media/X</c>). Every game-root mapping, runtime output and session database lives below it, except a declared loader.
    /// </summary>
    public const string MediaOverrideFolder = "mediaoverride";

    /// <summary>A game-root relative path strictly below <see cref="MediaOverrideFolder"/> (any letter case, as Windows resolves it).</summary>
    public static bool UnderMediaOverride(string? path) => path is { Length: > 14 } && path.StartsWith(MediaOverrideFolder + "/", StringComparison.OrdinalIgnoreCase);

    /// <summary>The payload folder of a custom mod, next to its manifest: <c>files/&lt;root&gt;/&lt;destination&gt;</c>.</summary>
    public const string CustomPayloadFolder = "files";

    public static bool IsCustomId(string? id) => id is not null && id.StartsWith(CustomPrefix, StringComparison.Ordinal);

    /// <summary>Official ids: <c>^[a-z][a-z0-9-]{0,63}$</c> (no dot). Custom ids: <c>custom.</c> + <c>[a-z0-9][a-z0-9-]{0,56}</c>. The two never collide.</summary>
    public static bool ValidId(string? id)
    {
        if (id is null) return false;
        if (IsCustomId(id))
        {
            var rest = id[CustomPrefix.Length..];
            return rest.Length is >= 1 and <= 57 && (char.IsAsciiLetterLower(rest[0]) || char.IsAsciiDigit(rest[0]))
                && rest.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');
        }
        return id.Length is >= 1 and <= 64 && char.IsAsciiLetterLower(id[0]) && id.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');
    }

    private static readonly string[] Executables = [".dll", ".exe", ".asi", ".sys", ".com", ".bat", ".cmd", ".ps1", ".scr", ".msi", ".cpl", ".ocx", ".drv", ".vbs"];
    /// <summary>A file Windows or a loader would execute, by extension.</summary>
    public static bool ExecutableExtension(string path) => Executables.Any(e => path.EndsWith(e, StringComparison.OrdinalIgnoreCase));

    public byte[] Snapshot() => (byte[])_bytes.Clone();
    public string Text(string field, string locale) => Document.GetProperty(field).TryGetProperty(locale, out var text)
        ? text.GetString()! : Document.GetProperty(field).GetProperty("en").GetString()!;
    public static string Hash(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(SHA256.HashData(bytes));

    public static void ValidatePath(string path)
    {
        if (string.IsNullOrWhiteSpace(path) || path.Contains('\\') || Path.IsPathRooted(path) || path.Contains(':')
            || path.Any(c => c < 32 || "<>\"|?*".Contains(c, StringComparison.Ordinal))) throw new ModException("invalid mod path");
        foreach (var part in path.Split('/'))
        {
            var stem = part.Split('.')[0];
            var tilde = part.IndexOf('~', StringComparison.Ordinal);
            // An 8.3 short-name alias (DATA~1, AZIMUT~1.EXE) could name a protected path under another spelling.
            if (tilde >= 0 && tilde + 1 < part.Length && char.IsAsciiDigit(part[tilde + 1])) throw new ModException("invalid mod path");
            if (part is "" or "." or ".." || part.EndsWith('.') || part.EndsWith(' ')
                || new[] { "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9" }.Contains(stem, StringComparer.OrdinalIgnoreCase))
                throw new ModException("invalid mod path");
        }
    }

    internal static void RejectDuplicates(JsonElement element)
    {
        if (element.ValueKind == JsonValueKind.Object)
        {
            var names = new HashSet<string>(StringComparer.Ordinal);
            foreach (var property in element.EnumerateObject())
            {
                if (!names.Add(property.Name)) throw new ModException("duplicate JSON property");
                RejectDuplicates(property.Value);
            }
        }
        else if (element.ValueKind == JsonValueKind.Array)
            foreach (var item in element.EnumerateArray()) RejectDuplicates(item);
    }
}

/// <summary>One mapped file. <see cref="Loader"/>: a binary the game process loads by itself (OD-115 game-root exception, reviewed official manifests only).</summary>
public sealed record ManifestFile(string Root, string Source, string Destination, string Hash, long Bytes, string Role, string Mode, string Package = "primary", bool Loader = false);
public sealed record ManifestDependency(string Id, IReadOnlyList<string> Versions);
public sealed record ModWineOverride(string Module, bool AppDefaults);
