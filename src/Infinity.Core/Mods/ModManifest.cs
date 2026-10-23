// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>Immutable manifest snapshot. Installation approval belongs to the launcher, not the JSON.</summary>
public sealed class ModManifest
{
    private readonly byte[] _bytes;
    public JsonElement Document { get; }
    public string Id => Document.GetProperty("id").GetString()!;
    public string Version => Document.GetProperty("version").GetString()!;
    public string Digest { get; }
    public string PackageUrl => Document.GetProperty("package").GetProperty("url").GetString()!;
    public string PackageHash => Document.GetProperty("package").GetProperty("sha256").GetString()!;
    public long PackageBytes => Document.GetProperty("package").GetProperty("bytes").GetInt64();
    public IReadOnlyList<ManifestFile> Files { get; }
    public IReadOnlyList<ManifestDependency> Dependencies { get; }
    public ModMultiplayer? Multiplayer { get; }
    public bool NeedsDatabaseCoordination => Document.GetProperty("databaseMigrations").GetArrayLength() > 0
        || Document.GetProperty("runtimeOutputs").GetArrayLength() > 0;
    public string? ConfigPath => Document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
        ? config.GetProperty("path").GetString() : null;

    public bool IsNative => Document.TryGetProperty("runtime", out var value) && value.GetString() == "infinity-native";
    // Capability support is independent of mod identity. Catalog trust is a separate boundary.
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
                }
            }
        }
        if (Document.TryGetProperty("database", out var db))
        {
            if (!IsNative || db.GetProperty("root").GetString() != "game" || db.GetProperty("access").GetString() != "startup"
                || db.GetProperty("revision").GetInt32() < 1 || db.GetProperty("tables").GetArrayLength() is < 1 or > 32) throw new ModException("unsupported database capability");
            var target = db.GetProperty("path").GetString()!; var seed = db.GetProperty("seedFrom").GetString()!;
            ValidatePath(target); ValidatePath(seed); if (target.Equals(seed, StringComparison.OrdinalIgnoreCase)) throw new ModException("session database must be distinct from the baseline");
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
        if (Id.Length is < 1 or > 64 || Id.Any(c => !(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-')))
            throw new ModException("invalid mod id");
        Digest = Hash(_bytes);
        var files = new List<ManifestFile>();
        foreach (var root in new[] { "game", "azimuth" })
            foreach (var file in Document.GetProperty("files").GetProperty(root).EnumerateArray())
            {
                var item = new ManifestFile(root, file.GetProperty("source").GetString()!, file.GetProperty("destination").GetString()!,
                    file.GetProperty("sha256").GetString()!, file.GetProperty("bytes").GetInt64(),
                    file.GetProperty("role").GetString()!, file.GetProperty("mode").GetString()!,
                    file.TryGetProperty("package", out var package) ? package.GetString()! : "primary");
                ValidatePath(item.Source); ValidatePath(item.Destination);
                if (item.Bytes is < 0 or > 64 * 1024 * 1024 || (item.Hash.Length != 64 || item.Hash.Any(c => !char.IsAsciiHexDigit(c)))
                    || item.Mode is not ("add" or "replace" or "add-or-replace" or "seed")) throw new ModException("invalid file mapping");
                if (item.Role is not ("binary" or "config" or "asset" or "user-asset" or "license")
                    || item.Mode == "seed" && item.Role is not ("config" or "user-asset")
                    || item.Role == "config" && (item.Root != "azimuth" || item.Mode != "seed" || item.Destination != ConfigPath)
                    || item.Root == "azimuth" && item.Destination.Equals(ConfigPath, StringComparison.OrdinalIgnoreCase) && item.Role != "config")
                    throw new ModException("invalid file role or settings mapping");
                files.Add(item);
            }
        if (files.Count > 512 || files.Select(f => f.Root + "/" + f.Destination).Distinct(StringComparer.OrdinalIgnoreCase).Count() != files.Count)
            throw new ModException("duplicate or excessive files");
        Files = files.AsReadOnly();
        Dependencies = Document.GetProperty("dependencies").EnumerateArray().Select(d => new ManifestDependency(
            d.GetProperty("id").GetString()!, d.GetProperty("versions").EnumerateArray().Select(v => v.GetString()!).ToArray())).ToArray();
        if (Dependencies.Count > 64 || Dependencies.Select(d => d.Id).Distinct(StringComparer.OrdinalIgnoreCase).Count() != Dependencies.Count
            || Dependencies.Any(d => d.Id.Equals(Id, StringComparison.OrdinalIgnoreCase) || d.Id.Length is < 1 or > 64
                || d.Id.Any(c => !(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-')) || d.Versions.Count == 0
                || d.Versions.Any(string.IsNullOrWhiteSpace))) throw new ModException("invalid dependencies");
        Multiplayer = ModMultiplayer.Parse(Document);
        ValidatePackage(Document.GetProperty("package"));
        var packageNames = new HashSet<string>(StringComparer.Ordinal) { "primary" };
        if (Document.TryGetProperty("additionalPackages", out var additional))
            foreach (var p in additional.EnumerateObject())
            {
                if (!packageNames.Add(p.Name) || packageNames.Count > 16) throw new ModException("invalid additional package");
                ValidatePackage(p.Value);
            }
        if (Files.Any(f => !packageNames.Contains(f.Package))) throw new ModException("unknown package mapping");
        if (Document.TryGetProperty("launch", out var launch) && launch.TryGetProperty("wineDllOverrides", out var overrides))
            foreach (var item in overrides.EnumerateArray())
            {
                var name = item.GetString()!;
                if (name.Length is < 1 or > 64 || name.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '_' && c != '-')
                    || !Files.Any(f => f.Root == "azimuth" && f.Role == "binary" && f.Destination.Equals(name + ".dll", StringComparison.OrdinalIgnoreCase)))
                    throw new ModException("DLL override must name a mapped binary");
            }
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

public sealed record ManifestFile(string Root, string Source, string Destination, string Hash, long Bytes, string Role, string Mode, string Package = "primary");
public sealed record ManifestDependency(string Id, IReadOnlyList<string> Versions);

/// <summary>Unsigned development discovery. Production must use a separate signed catalog policy.</summary>
public static class DevelopmentModCatalog
{
    public static async Task<IReadOnlyList<ModManifest>> LoadAsync(HttpClient http, Uri url, CancellationToken cancellation)
    {
        var bytes = await ReadAsync(http, url, 256 * 1024, cancellation).ConfigureAwait(false);
        using var json = JsonDocument.Parse(bytes);
        ModManifest.RejectDuplicates(json.RootElement);
        if (json.RootElement.GetProperty("schemaVersion").GetInt32() != 1) throw new ModException("unsupported catalog");
        var entries = json.RootElement.GetProperty("mods");
        if (entries.GetArrayLength() > 64) throw new ModException("catalog too large");
        var mods = new List<ModManifest>();
        foreach (var entry in entries.EnumerateArray())
        {
            var address = new Uri(entry.GetProperty("manifestUrl").GetString()!, UriKind.Absolute);
            // A network catalog may never cause local-file reads, even in development.
            if (address.IsFile && !url.IsFile) throw new ModException("remote catalog references a local file");
            var raw = await ReadAsync(http, address, 256 * 1024, cancellation).ConfigureAwait(false);
            if (ModManifest.Hash(raw) != entry.GetProperty("manifestSha256").GetString()) throw new ModException("manifest digest mismatch");
            var mod = new ModManifest(raw);
            if (mod.Id != entry.GetProperty("id").GetString() || mod.Version != entry.GetProperty("version").GetString()
                || mods.Any(m => m.Id == mod.Id)) throw new ModException("catalog identity mismatch");
            mods.Add(mod);
        }
        return mods.AsReadOnly();
    }

    private static async Task<byte[]> ReadAsync(HttpClient http, Uri url, int limit, CancellationToken cancellation)
    {
        if (url.IsFile)
        {
            if (url.IsUnc || new FileInfo(url.LocalPath).Length > limit) throw new ModException("invalid local catalog file");
            return await File.ReadAllBytesAsync(url.LocalPath, cancellation).ConfigureAwait(false);
        }
        if (url.Scheme != "https" && !(url.Scheme == "http" && url.IsLoopback)
            || url.UserInfo.Length > 0 || url.Fragment.Length > 0) throw new ModException("invalid catalog URL");
        using var response = await http.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        if (response.RequestMessage?.RequestUri != url) throw new ModException("catalog redirects are not supported");
        await using var stream = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        return await ReadBoundedAsync(stream, limit, cancellation).ConfigureAwait(false);
    }

    internal static async Task<byte[]> ReadBoundedAsync(Stream stream, long limit, CancellationToken cancellation)
    {
        using var result = new MemoryStream();
        var buffer = new byte[81920];
        int count;
        while ((count = await stream.ReadAsync(buffer, cancellation).ConfigureAwait(false)) > 0)
        {
            if (result.Length + count > limit) throw new ModException("download too large");
            result.Write(buffer, 0, count);
        }
        return result.ToArray();
    }
}
