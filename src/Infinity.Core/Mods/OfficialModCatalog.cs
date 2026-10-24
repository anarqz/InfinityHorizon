// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Infinity.Core.Mods;

/// <summary>A pinned download of a catalog entry: HTTPS address, SHA-256 (lowercase hex) and size.</summary>
public sealed record CatalogDownload(Uri Url, string Sha256, long Bytes);

/// <summary>The catalog's runtime entry (OD-116): the signed native mod runtime as a download of its own; native mods depend on it.</summary>
public sealed record ModRuntimeEntry(string Id, string Version, CatalogDownload Download);

/// <summary>
/// OD-116: the mod catalog is one unsigned JSON file, the official one committed on <c>main</c> of the launcher repository and
/// read from raw.githubusercontent.com. Its only integrity check is the SHA-256 (and size) pinned for every download: a download
/// that differs is refused. No signature, sequence, expiry, download-host list, <c>required</c> or <c>availability</c>.
/// <para>
/// Schema 2: <c>{"schemaVersion": 2, "mods": [entry…]}</c>. A mod entry carries its details (<c>id</c>, <c>version</c>,
/// <c>name</c>/<c>description</c> {en, pt-BR}, <c>author</c>), <c>download</c> {url, sha256, bytes}, optional
/// <c>additionalDownloads</c> {name: download}, optional <c>requires</c> (["infinity-mod-runtime"]) and <c>install</c>: the existing
/// manifest body (files, config, runtime, dependencies, multiplayer…) without the fields the entry itself carries. The launcher
/// rebuilds the manifest from both, so every rule of <see cref="ModManifest"/> still applies. The entry <c>infinity-mod-runtime</c>
/// has no <c>install</c>: its download is the runtime package (<see cref="ModRuntimePackage"/>).
/// </para>
/// </summary>
public static class OfficialModCatalog
{
    public const int SchemaVersion = 2;
    public const int MaxCatalogBytes = 1024 * 1024;
    public const string RuntimeId = "infinity-mod-runtime";
    public const long MaxDownloadBytes = 64L * 1024 * 1024;
    public static Uri OfficialUrl { get; } = new($"https://raw.githubusercontent.com/{Update.GitHubReleases.Repository}/main/mods/catalog.json");

    /// <summary>The official address (any other is a player-chosen, unofficial catalog).</summary>
    public static bool IsOfficial(Uri url) => Uri.Compare(url, OfficialUrl, UriComponents.AbsoluteUri, UriFormat.UriEscaped, StringComparison.Ordinal) == 0;

    private static readonly string[] EntryFields = ["id", "version", "name", "description", "author"];
    /// <summary>Fields the entry carries, so an <c>install</c> object never repeats or overrides them.</summary>
    private static readonly string[] ReservedInstallFields = [.. EntryFields, "package", "additionalPackages", "payload"];

    /// <summary>Parses a schema-2 catalog into manifests (rebuilt from the entries) and its runtime entry.</summary>
    public static (IReadOnlyList<ModManifest> Mods, ModRuntimeEntry? Runtime) Parse(byte[] raw)
    {
        ArgumentNullException.ThrowIfNull(raw);
        if (raw.Length is <= 0 or > MaxCatalogBytes || raw.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw new ModException("invalid catalog", ModErrors.Catalog);
        try
        {
            using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 40 });
            var root = json.RootElement;
            ModManifest.RejectDuplicates(root);
            if (root.GetProperty("schemaVersion").GetInt32() != SchemaVersion) throw new ModException("unsupported catalog schema", ModErrors.Catalog);
            var list = root.GetProperty("mods");
            if (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > 512) throw new ModException("invalid catalog mods", ModErrors.Catalog);
            var mods = new List<ModManifest>(); ModRuntimeEntry? runtime = null; var requires = new List<string>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var entry in list.EnumerateArray())
            {
                var id = entry.GetProperty("id").GetString()!;
                if (!ids.Add(id)) throw new ModException("duplicate catalog id", ModErrors.Catalog);
                if (id == RuntimeId)
                {
                    if (entry.TryGetProperty("install", out _)) throw new ModException("the runtime entry installs no files", ModErrors.Catalog);
                    var version = entry.GetProperty("version").GetString()!;
                    if (version.Length is < 1 or > 64) throw new ModException("invalid runtime version", ModErrors.Catalog);
                    runtime = new(id, version, Download(entry.GetProperty("download")));
                    continue;
                }
                var manifest = Manifest(entry);
                if (entry.TryGetProperty("requires", out var required))
                    foreach (var name in required.EnumerateArray())
                        requires.Add(name.GetString() is RuntimeId ? RuntimeId : throw new ModException("unknown catalog requirement", ModErrors.Catalog));
                mods.Add(manifest);
            }
            if (requires.Count > 0 && runtime is null) throw new ModException("a mod requires the runtime the catalog does not list", ModErrors.Catalog);
            return (mods.AsReadOnly(), runtime);
        }
        // A bad entry (its manifest's own refusal included) is a bad catalog: the player sees one message for it.
        catch (Exception error) when (error is JsonException or KeyNotFoundException or InvalidOperationException or FormatException or ArgumentException
            or ModException { Code: not ModErrors.Catalog })
        { throw new ModException("invalid catalog: " + error.Message, ModErrors.Catalog); }
    }

    /// <summary>One mod entry's manifest: the <c>install</c> body with the entry's details and downloads put back in place.</summary>
    internal static ModManifest Manifest(JsonElement entry)
    {
        var install = entry.GetProperty("install");
        if (install.ValueKind != JsonValueKind.Object) throw new ModException("an entry's install must be an object", ModErrors.Catalog);
        if (install.EnumerateObject().Any(p => ReservedInstallFields.Contains(p.Name, StringComparer.Ordinal)))
            throw new ModException("install repeats a field of the entry", ModErrors.Catalog);
        var node = JsonNode.Parse(install.GetRawText())!.AsObject();
        foreach (var field in EntryFields) node[field] = JsonNode.Parse(entry.GetProperty(field).GetRawText());
        node["package"] = Package(Download(entry.GetProperty("download")));
        if (entry.TryGetProperty("additionalDownloads", out var additional))
        {
            if (additional.ValueKind != JsonValueKind.Object) throw new ModException("invalid additional downloads", ModErrors.Catalog);
            var packages = new JsonObject();
            foreach (var item in additional.EnumerateObject()) packages[item.Name] = Package(Download(item.Value));
            node["additionalPackages"] = packages;
        }
        var manifest = new ModManifest(Encoding.UTF8.GetBytes(node.ToJsonString()));
        // OD-114: the reserved custom prefix comes only from the player's custom folder, never from a catalog.
        if (manifest.IsCustom) throw new ModException("a catalog cannot carry a custom mod", ModErrors.Catalog);
        return manifest;
    }

    /// <summary>
    /// The schema-2 entry for a reviewed manifest (the catalog tool): its details and downloads lifted out, the rest as <c>install</c>.
    /// <see cref="Manifest(JsonElement)"/> rebuilds a manifest with the same content (<see cref="ModManifest.SameContent"/>).
    /// </summary>
    public static JsonObject Entry(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (manifest.IsCustom) throw new ModException("a catalog cannot carry a custom mod", ModErrors.Catalog);
        var body = JsonNode.Parse(manifest.Document.GetRawText())!.AsObject();
        var entry = new JsonObject();
        foreach (var field in EntryFields) { entry[field] = body[field]?.DeepClone(); body.Remove(field); }
        static JsonObject Download(JsonNode package) => new() { ["url"] = package["url"]!.DeepClone(), ["sha256"] = package["sha256"]!.DeepClone(), ["bytes"] = package["bytes"]!.DeepClone() };
        if (body["package"] is { } package && package["format"]?.GetValue<string>() is null or "zip") { entry["download"] = Download(package); body.Remove("package"); }
        else throw new ModException("a catalog download is a zip package", ModErrors.Catalog);
        if (body["additionalPackages"] is JsonObject additional)
        {
            var downloads = new JsonObject();
            foreach (var (name, value) in additional) downloads[name] = Download(value!);
            entry["additionalDownloads"] = downloads; body.Remove("additionalPackages");
        }
        if (manifest.NeedsRuntime) entry["requires"] = new JsonArray(RuntimeId);
        entry["install"] = body;
        return entry;
    }

    private static JsonObject Package(CatalogDownload download) => new()
    {
        ["url"] = download.Url.AbsoluteUri, ["sha256"] = download.Sha256, ["bytes"] = download.Bytes, ["format"] = "zip",
    };

    private static CatalogDownload Download(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new ModException("invalid download", ModErrors.Catalog);
        var hash = value.GetProperty("sha256").GetString()!; var bytes = value.GetProperty("bytes").GetInt64();
        if (!Uri.TryCreate(value.GetProperty("url").GetString(), UriKind.Absolute, out var url) || url.Scheme != Uri.UriSchemeHttps
            || !url.IsDefaultPort || url.UserInfo.Length > 0 || url.Fragment.Length > 0
            || hash.Length != 64 || hash.Any(c => !char.IsAsciiHexDigitLower(c) && !char.IsAsciiDigit(c)) || bytes is <= 0 or > MaxDownloadBytes)
            throw new ModException("invalid download", ModErrors.Catalog);
        return new(url, hash, bytes);
    }

    /// <summary>Fetches the catalog (any HTTPS host: the pinned hashes are the check), parses it and caches it for offline listing.</summary>
    public static async Task<ModCatalog> LoadAsync(HttpClient http, Uri url, string cacheDirectory, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(url);
        byte[] raw;
        using (var response = await ModDownloads.DownloadAsync(http, url, _ => true, cancellation).ConfigureAwait(false))
        {
            if (response.StatusCode != System.Net.HttpStatusCode.OK) throw new ModException("catalog unavailable: " + (int)response.StatusCode, ModErrors.Catalog);
            await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
            try { raw = await DevelopmentModCatalog.ReadBoundedAsync(body, MaxCatalogBytes, cancellation).ConfigureAwait(false); }
            catch (ModException error) when (error.Code == ModErrors.Download) { throw new ModException("catalog too large", ModErrors.Catalog); }
        }
        var catalog = Build(url, raw, cached: false);
        var path = CachePath(cacheDirectory, url);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        await File.WriteAllBytesAsync(staging, raw, cancellation).ConfigureAwait(false);
        File.Move(staging, path, overwrite: true);
        return catalog;
    }

    /// <summary>The last catalog fetched from <paramref name="url"/>, or null (none, or no longer readable).</summary>
    public static ModCatalog? LoadCached(Uri url, string cacheDirectory)
    {
        var path = CachePath(cacheDirectory, url);
        try { return File.Exists(path) && new FileInfo(path).Length <= MaxCatalogBytes ? Build(url, File.ReadAllBytes(path), cached: true) : null; }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { return null; }
    }

    private static ModCatalog Build(Uri url, byte[] raw, bool cached)
    {
        var (mods, runtime) = Parse(raw);
        var official = IsOfficial(url);
        return new(official ? "infinity-official" : "unofficial", 0, null, null, [], mods, Trusted: official, Cached: cached)
        { Runtime = runtime, AnyDownloadHost = true };
    }

    /// <summary>One cache file per catalog address, so switching back to the official catalog never shows a custom one's mods.</summary>
    private static string CachePath(string cacheDirectory, Uri url) =>
        Path.Combine(Path.GetFullPath(cacheDirectory), Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(url.AbsoluteUri)))[..16] + ".json");
}

/// <summary>"Use a different catalog" (OD-116): the player's catalog address, kept in the state folder; none means the official one.</summary>
public static class ModCatalogSetting
{
    public const string FileName = "mod-catalog-url.json";
    private sealed record Saved(string Url);

    public static Uri? Load(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!File.Exists(path)) return null;
            var saved = JsonSerializer.Deserialize<Saved>(File.ReadAllBytes(path));
            return saved?.Url is { } text && Valid(text) is { } url && !OfficialModCatalog.IsOfficial(url) ? url : null;
        }
        catch (Exception error) when (error is IOException or JsonException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>Saves a custom address, or removes it (null or the official address) so the official catalog is used.</summary>
    public static void Save(string stateDirectory, Uri? url)
    {
        var path = Path.Combine(stateDirectory, FileName);
        if (url is null || OfficialModCatalog.IsOfficial(url)) { ManagedModFiles.RemoveFile(path); return; }
        if (Valid(url.OriginalString) is null) throw new ModException("invalid catalog address", ModErrors.Catalog);
        Directory.CreateDirectory(stateDirectory);
        File.WriteAllBytes(path + ".tmp", JsonSerializer.SerializeToUtf8Bytes(new Saved(url.AbsoluteUri)));
        File.Move(path + ".tmp", path, overwrite: true);
    }

    /// <summary>The download rules (<see cref="ModDownloads"/>): HTTPS on the default port, no credentials or fragment.</summary>
    public static Uri? Valid(string? text) => Uri.TryCreate(text?.Trim(), UriKind.Absolute, out var url) && url.Scheme == Uri.UriSchemeHttps
        && url.IsDefaultPort && url.UserInfo.Length == 0 && url.Fragment.Length == 0 ? url : null;
}
