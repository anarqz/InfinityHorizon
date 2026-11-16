// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Infinity.Core.Mods;

/// <summary>A pinned download of a catalog entry: HTTPS address, SHA-256 (lowercase hex) and size.</summary>
public sealed record CatalogDownload(Uri Url, string Sha256, long Bytes);

/// <summary>
/// OD-116: the mod catalog is one unsigned JSON file, the official one committed on <c>main</c> of InfinityMods (OD-166) and
/// read from raw.githubusercontent.com. Its only integrity check is the SHA-256 (and size) pinned for every download: a download
/// that differs is refused. No signature, sequence, expiry, download-host list, <c>required</c> or <c>availability</c>.
/// <para>
/// Schema 2: <c>{"schemaVersion": 2, "mods": [entry…]}</c>. A mod entry carries its details (<c>id</c>, <c>version</c>,
/// <c>name</c>/<c>description</c> {en, pt-BR}, <c>author</c>), <c>download</c> {url, sha256, bytes} (or OD-117's
/// <c>externalSources</c>), optional <c>additionalDownloads</c> {name: download} and <c>install</c>: the existing manifest body
/// (files, config, runtime, dependencies, multiplayer…) without the fields the entry itself carries. The launcher rebuilds the
/// manifest from both, so every rule of <see cref="ModManifest"/> still applies: OD-232's ExtraOptions mods only. An entry in a
/// retired format, the former <c>infinity-mod-runtime</c> entry or a <c>requires</c> refuses the catalog, like any invalid entry.
/// </para>
/// <para>
/// Schema 3 (T-1117, OfficialModCatalog.Index.cs): an index of card details that pins one detail file per mod; the official catalog
/// is that index. A custom catalog is either, told apart by its <c>schemaVersion</c>.
/// </para>
/// </summary>
public static partial class OfficialModCatalog
{
    public const int SchemaVersion = 2;
    public const int MaxCatalogBytes = 1024 * 1024;
    public const long MaxDownloadBytes = 64L * 1024 * 1024;
    /// <summary>T-1117: the official catalog is the schema-3 index <c>catalog/index.json</c> on InfinityMods' <c>main</c>.</summary>
    public static Uri OfficialUrl { get; } = new("https://raw.githubusercontent.com/anarqz/InfinityMods/main/catalog/index.json");

    /// <summary>
    /// OD-166: the official schema-2 file until 3.1.6. Never fetched (no fallback to it, OD-144); a setting that still holds it counts as
    /// the official catalog, so it reads <see cref="OfficialUrl"/>.
    /// </summary>
    public static Uri FormerOfficialUrl { get; } = new("https://raw.githubusercontent.com/anarqz/InfinityMods/main/catalog.json");

    /// <summary>
    /// OD-166: the former official address (the launcher repository's <c>mods/catalog.json</c>, removed in T-1115). Never fetched; a
    /// setting that still holds it counts as the official catalog, so it reads <see cref="OfficialUrl"/>.
    /// </summary>
    public static Uri LegacyOfficialUrl { get; } = new($"https://raw.githubusercontent.com/{Update.GitHubReleases.Repository}/main/mods/catalog.json");

    /// <summary>The official address, or a former one (any other is a player-chosen, unofficial catalog).</summary>
    public static bool IsOfficial(Uri url) => Same(url, OfficialUrl) || Same(url, FormerOfficialUrl) || Same(url, LegacyOfficialUrl);

    /// <summary>The address actually read: a former official address reads the official one.</summary>
    private static Uri Canonical(Uri url) => IsOfficial(url) ? OfficialUrl : url;

    private static bool Same(Uri a, Uri b) => Uri.Compare(a, b, UriComponents.AbsoluteUri, UriFormat.UriEscaped, StringComparison.Ordinal) == 0;

    private static readonly string[] EntryFields = ["id", "version", "name", "description", "author"];
    private static readonly string[] DroppedFields = ["required", "availability"];
    /// <summary>Fields the entry carries, so an <c>install</c> object never repeats or overrides them.</summary>
    private static readonly string[] ReservedInstallFields = [.. EntryFields, "package", "additionalPackages", "payload", "externalSources"];

    /// <summary>Parses a schema-2 catalog into manifests (rebuilt from the entries).</summary>
    public static IReadOnlyList<ModManifest> Parse(byte[] raw)
    {
        ArgumentNullException.ThrowIfNull(raw);
        if (raw.Length is <= 0 or > MaxCatalogBytes || raw.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw new ModException("invalid catalog", ModErrors.Catalog);
        try
        {
            using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 40 });
            var root = json.RootElement;
            ModManifest.RejectDuplicates(root);
            if (root.GetProperty("schemaVersion").GetInt32() != SchemaVersion) throw new ModException("unsupported catalog schema", ModErrors.Catalog);
            // OD-116: the root carries only these (a root-level required or availability, or anything else, is refused, never ignored).
            if (root.EnumerateObject().Any(p => p.Name is not ("schemaVersion" or "mods"))) throw new ModException("unknown catalog field", ModErrors.Catalog);
            var list = root.GetProperty("mods");
            if (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > 512) throw new ModException("invalid catalog mods", ModErrors.Catalog);
            var mods = new List<ModManifest>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var entry in list.EnumerateArray())
            {
                var id = entry.GetProperty("id").GetString()!;
                if (!ids.Add(id)) throw new ModException("duplicate catalog id", ModErrors.Catalog);
                mods.Add(EntryManifest(entry));
            }
            return mods.AsReadOnly();
        }
        // A bad entry (its manifest's own refusal included) is a bad catalog: the player sees one message for it.
        catch (Exception error) when (IsEntryError(error)) { throw new ModException("invalid catalog: " + error.Message, ModErrors.Catalog); }
    }

    private static bool IsEntryError(Exception error) => error is JsonException or KeyNotFoundException or InvalidOperationException or FormatException
        or ArgumentException or ModException { Code: not ModErrors.Catalog };

    /// <summary>The rules of one schema-2 entry (a schema-3 index entry merged with its detail file is one too).</summary>
    private static ModManifest EntryManifest(JsonElement entry)
    {
        // OD-116 ("drop both"): no required mods and no availability, on any entry or inside its install;
        // refused rather than silently ignored, so a catalog author never believes they take effect.
        foreach (var dropped in DroppedFields)
            if (entry.TryGetProperty(dropped, out _) || entry.TryGetProperty("install", out var body) && body.ValueKind == JsonValueKind.Object && body.TryGetProperty(dropped, out _))
                throw new ModException("catalog v2 has no '" + dropped + "' (OD-116)", ModErrors.Catalog);
        // OD-232: no native mod runtime to list or require (its former entry has no install, so it is refused below too).
        if (entry.TryGetProperty("requires", out _)) throw new ModException("catalog v2 has no 'requires' (OD-232)", ModErrors.Catalog);
        return Manifest(entry);
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
        // OD-117: a mod of the player's own files lists its sources instead of any download (and never both).
        if (entry.TryGetProperty("externalSources", out var sources))
        {
            if (entry.TryGetProperty("download", out _) || entry.TryGetProperty("additionalDownloads", out _))
                throw new ModException("an entry has a download or external sources, not both", ModErrors.Catalog);
            node["externalSources"] = JsonNode.Parse(sources.GetRawText());
        }
        else node["package"] = Package(Download(entry.GetProperty("download")));
        if (entry.TryGetProperty("additionalDownloads", out var additional))
        {
            if (additional.ValueKind != JsonValueKind.Object) throw new ModException("invalid additional downloads", ModErrors.Catalog);
            var packages = new JsonObject();
            foreach (var item in additional.EnumerateObject()) packages[item.Name] = Package(Download(item.Value));
            node["additionalPackages"] = packages;
        }
        var manifest = new ModManifest(Encoding.UTF8.GetBytes(node.ToJsonString()));
        if (manifest.IsBuiltIn) throw new ModException("a catalog cannot carry a built-in mod", ModErrors.Catalog);
        return manifest;
    }

    /// <summary>
    /// The schema-2 entry for a reviewed manifest (the catalog tool): its details and downloads lifted out, the rest as <c>install</c>.
    /// <see cref="Manifest(JsonElement)"/> rebuilds a manifest with the same content (<see cref="ModManifest.SameContent"/>).
    /// </summary>
    public static JsonObject Entry(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (manifest.IsBuiltIn) throw new ModException("a catalog cannot carry a built-in mod", ModErrors.Catalog);
        if (manifest.Retired) throw new ModException("a catalog carries only ExtraOptions mods (OD-232)", ModErrors.Catalog);
        var body = JsonNode.Parse(manifest.Document.GetRawText())!.AsObject();
        var entry = new JsonObject();
        foreach (var field in EntryFields) { entry[field] = body[field]?.DeepClone(); body.Remove(field); }
        static JsonObject Download(JsonNode package) => new() { ["url"] = package["url"]!.DeepClone(), ["sha256"] = package["sha256"]!.DeepClone(), ["bytes"] = package["bytes"]!.DeepClone() };
        if (manifest.IsExternal) { entry["externalSources"] = body["externalSources"]!.DeepClone(); body.Remove("externalSources"); }
        else if (body["package"] is { } package && package["format"]?.GetValue<string>() is null or "zip") { entry["download"] = Download(package); body.Remove("package"); }
        else throw new ModException("a catalog download is a zip package", ModErrors.Catalog);
        if (body["additionalPackages"] is JsonObject additional)
        {
            var downloads = new JsonObject();
            foreach (var (name, value) in additional) downloads[name] = Download(value!);
            entry["additionalDownloads"] = downloads; body.Remove("additionalPackages");
        }
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

    /// <summary>
    /// Fetches the catalog (any HTTPS host: the pinned hashes are the check), parses it and caches it for offline listing. A former
    /// official address reads the official one (InfinityMods' index is the only official catalog). A schema-3 index also fetches the
    /// detail files whose pin is not cached yet (<see cref="DetailConcurrency"/> at a time); a schema-2 file is the whole catalog.
    /// </summary>
    public static async Task<ModCatalog> LoadAsync(HttpClient http, Uri url, string cacheDirectory, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(url);
        url = Canonical(url);
        var raw = await FetchAsync(http, url, MaxCatalogBytes, cancellation).ConfigureAwait(false);
        var path = CachePath(cacheDirectory, url);
        ModCatalog catalog;
        if (DeclaredSchema(raw) == IndexSchemaVersion)
        {
            var index = ParseIndex(raw, url, FollowsDownloadRules);
            var details = DetailCache(cacheDirectory, url);
            var mods = await DetailsAsync(index, details, (detail, token) => FetchAsync(http, detail, MaxDetailBytes, token), cancellation).ConfigureAwait(false);
            catalog = Build(url, mods, cached: false);
            // the details first: a cached index always has its pinned details next to it
            await WriteAtomicAsync(path, raw, cancellation).ConfigureAwait(false);
            PruneDetails(details, index);
        }
        else
        {
            catalog = Build(url, Parse(raw), cached: false);
            await WriteAtomicAsync(path, raw, cancellation).ConfigureAwait(false);
        }
        return catalog;
    }

    private static async Task WriteAtomicAsync(string path, byte[] raw, CancellationToken cancellation)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        await File.WriteAllBytesAsync(staging, raw, cancellation).ConfigureAwait(false);
        File.Move(staging, path, overwrite: true);
    }

    private static async Task<byte[]> FetchAsync(HttpClient http, Uri url, int limit, CancellationToken cancellation)
    {
        using var response = await ModDownloads.DownloadAsync(http, url, _ => true, cancellation).ConfigureAwait(false);
        if (response.StatusCode != System.Net.HttpStatusCode.OK) throw new ModException("catalog unavailable: " + (int)response.StatusCode, ModErrors.Catalog);
        await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        try { return await DevelopmentModCatalog.ReadBoundedAsync(body, limit, cancellation).ConfigureAwait(false); }
        catch (ModException error) when (error.Code == ModErrors.Download) { throw new ModException("catalog too large", ModErrors.Catalog); }
    }

    /// <summary>The last catalog fetched from <paramref name="url"/>, or null (none, or no longer readable: a schema-3 index needs every detail it pins).</summary>
    public static ModCatalog? LoadCached(Uri url, string cacheDirectory)
    {
        ArgumentNullException.ThrowIfNull(url);
        url = Canonical(url);
        var path = CachePath(cacheDirectory, url);
        try
        {
            if (!File.Exists(path) || new FileInfo(path).Length > MaxCatalogBytes) return null;
            var raw = File.ReadAllBytes(path);
            if (DeclaredSchema(raw) != IndexSchemaVersion) return Build(url, Parse(raw), cached: true);
            var index = ParseIndex(raw, url, FollowsDownloadRules);
            var details = DetailCache(cacheDirectory, url);
            var mods = index.Select(entry => MergedManifest(entry, ReadCachedDetail(details, entry)
                ?? throw new ModException("a pinned detail is not cached", ModErrors.Catalog))).ToArray();
            return Build(url, mods, cached: true);
        }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>
    /// A schema-2 catalog read through the development route (<c>FI_DEV_MOD_CATALOG_URL</c>, development builds only): parsed by the same
    /// rules, never official or trusted, so a tester can load v2 from a local file without an HTTPS host.
    /// </summary>
    public static ModCatalog Development(byte[] raw) => Build(DevelopmentUrl, Parse(raw), cached: false);

    private static Uri DevelopmentUrl { get; } = new("file:///development-catalog.json");

    /// <summary>The bytes declare <c>schemaVersion</c> 2 (the development route tells v2 from the older development format).</summary>
    public static bool IsSchema2(byte[] raw) => DeclaredSchema(raw) == SchemaVersion;

    /// <summary>The bytes declare <c>schemaVersion</c> 3: a catalog index (T-1117).</summary>
    public static bool IsSchema3(byte[] raw) => DeclaredSchema(raw) == IndexSchemaVersion;

    /// <summary>The root's integer <c>schemaVersion</c>, or null (not JSON, not an object, none).</summary>
    private static int? DeclaredSchema(byte[] raw)
    {
        try
        {
            using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 40 });
            return json.RootElement.ValueKind == JsonValueKind.Object && json.RootElement.TryGetProperty("schemaVersion", out var version)
                && version.TryGetInt32(out var number) ? number : null;
        }
        catch (JsonException) { return null; }
    }

    private static ModCatalog Build(Uri url, IReadOnlyList<ModManifest> mods, bool cached)
    {
        var official = IsOfficial(url);
        return new(official ? "infinity-official" : "unofficial", 0, null, null, [], mods, Trusted: official, Cached: cached)
        { AnyDownloadHost = true };
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
