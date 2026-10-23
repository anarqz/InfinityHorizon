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
    public bool NeedsDatabaseCoordination => Document.GetProperty("databaseMigrations").GetArrayLength() > 0
        || Document.GetProperty("runtimeOutputs").GetArrayLength() > 0;
    public string? ConfigPath => Document.GetProperty("config") is { ValueKind: JsonValueKind.Object } config
        ? config.GetProperty("path").GetString() : null;

    // Development rollout: the reviewed ExtraOptions snapshot alone may write files. Other entries
    // are discoverable, but changing a remote review.status cannot grant installation authority.
    public bool ApprovedForFileInstall => Digest == "67f44d71e6b3c2023dc71c6252461873f4546ebb688c62b16d9f8e477fed163a";

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
        _ = ConfigPath;
        if (Id.Length is < 1 or > 64 || Id.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '-'))
            throw new ModException("invalid mod id");
        Digest = Hash(_bytes);
        var files = new List<ManifestFile>();
        foreach (var root in new[] { "game", "azimuth" })
            foreach (var file in Document.GetProperty("files").GetProperty(root).EnumerateArray())
            {
                var item = new ManifestFile(root, file.GetProperty("source").GetString()!, file.GetProperty("destination").GetString()!,
                    file.GetProperty("sha256").GetString()!, file.GetProperty("bytes").GetInt64(),
                    file.GetProperty("role").GetString()!, file.GetProperty("mode").GetString()!);
                ValidatePath(item.Source); ValidatePath(item.Destination);
                if (item.Bytes is < 0 or > 64 * 1024 * 1024 || item.Hash.Length != 64
                    || item.Mode is not ("add" or "replace" or "add-or-replace" or "seed")) throw new ModException("invalid file mapping");
                files.Add(item);
            }
        if (files.Count > 512 || files.Select(f => f.Root + "/" + f.Destination).Distinct(StringComparer.OrdinalIgnoreCase).Count() != files.Count)
            throw new ModException("duplicate or excessive files");
        Files = files.AsReadOnly();
        Dependencies = Document.GetProperty("dependencies").EnumerateArray().Select(d => new ManifestDependency(
            d.GetProperty("id").GetString()!, d.GetProperty("versions").EnumerateArray().Select(v => v.GetString()!).ToArray())).ToArray();
        if (Dependencies.Count > 64) throw new ModException("too many dependencies");
        if (PackageBytes is <= 0 or > 64 * 1024 * 1024 || PackageHash.Length != 64
            || !Uri.TryCreate(PackageUrl, UriKind.Absolute, out _)) throw new ModException("invalid package");
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

public sealed record ManifestFile(string Root, string Source, string Destination, string Hash, long Bytes, string Role, string Mode);
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
