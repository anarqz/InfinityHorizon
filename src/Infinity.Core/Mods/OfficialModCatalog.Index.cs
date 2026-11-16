// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Infinity.Core.Mods;

/// <summary>
/// A detail file that differs from the SHA-256 and size its index pins, or whose <c>id</c>/<c>version</c> differ from the entry's: most
/// likely a CDN copy older than the index (raw.githubusercontent.com caches for about five minutes), so it is transient. The load fails
/// like any other catalog failure (the cached catalog stays shown) and the launcher tries again quietly later.
/// </summary>
public sealed class CatalogDetailStaleException(string message) : ModException(message, ModErrors.Catalog);

/// <summary>
/// T-1117, catalog schema 3 (InfinityMods <c>schemas/catalog-v3.schema.json</c>): the root is an index
/// <c>{"schemaVersion": 3, "mods": [{id, version, name, description, author, path, sha256, bytes}]}</c> with schema 2's root rules; each
/// entry pins its detail file <c>{"schemaVersion": 3, id, version, download|externalSources, additionalDownloads?, requires?, install}</c>
/// by its SHA-256 and size. The index entry's card details plus the detail file without <c>schemaVersion</c>/<c>id</c>/<c>version</c> is
/// exactly a schema-2 entry, so every schema-2 entry rule (<see cref="Manifest(JsonElement)"/>) applies unchanged. Each detail is cached
/// per (catalog address, SHA-256): a refresh downloads the index and only the details whose pin changed.
/// </summary>
public static partial class OfficialModCatalog
{
    public const int IndexSchemaVersion = 3;
    public const int MaxDetailBytes = 256 * 1024;
    /// <summary>Detail files fetched at the same time, at most.</summary>
    public const int DetailConcurrency = 8;
    private const int MaxMods = 512;

    /// <summary>One index entry: its card details (id, version, name, description, author) and the pinned detail file.</summary>
    internal sealed record IndexEntry(string Id, string Version, JsonObject Card, Uri Detail, string Sha256, long Bytes);

    private static readonly string[] IndexEntryFields = [.. EntryFields, "path", "sha256", "bytes"];
    private static readonly string[] DetailFields = ["schemaVersion", "id", "version", "download", "externalSources", "additionalDownloads", "requires", "install"];

    /// <summary>The download rules (<see cref="ModDownloads"/>): HTTPS on the default port, no credentials or fragment.</summary>
    private static bool FollowsDownloadRules(Uri url) => url.IsAbsoluteUri && url.Scheme == Uri.UriSchemeHttps && url.IsDefaultPort
        && url.UserInfo.Length == 0 && url.Fragment.Length == 0;

    /// <summary>Parses a schema-3 index read from <paramref name="indexUrl"/>; each resolved detail address must pass <paramref name="allowed"/>.</summary>
    internal static IReadOnlyList<IndexEntry> ParseIndex(byte[] raw, Uri indexUrl, Func<Uri, bool> allowed)
    {
        ArgumentNullException.ThrowIfNull(raw); ArgumentNullException.ThrowIfNull(indexUrl);
        if (raw.Length is <= 0 or > MaxCatalogBytes || raw.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw new ModException("invalid catalog", ModErrors.Catalog);
        try
        {
            using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 40 });
            var root = json.RootElement;
            ModManifest.RejectDuplicates(root);
            if (root.GetProperty("schemaVersion").GetInt32() != IndexSchemaVersion) throw new ModException("unsupported catalog schema", ModErrors.Catalog);
            // schema 2's root rules: only these two (a root-level required or availability, or anything else, is refused, never ignored)
            if (root.EnumerateObject().Any(p => p.Name is not ("schemaVersion" or "mods"))) throw new ModException("unknown catalog field", ModErrors.Catalog);
            var list = root.GetProperty("mods");
            if (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > MaxMods) throw new ModException("invalid catalog mods", ModErrors.Catalog);
            var entries = new List<IndexEntry>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var entry in list.EnumerateArray())
            {
                if (entry.ValueKind != JsonValueKind.Object) throw new ModException("invalid catalog entry", ModErrors.Catalog);
                // every field, and only these: a required, availability or install in the index is refused, never ignored (OD-116)
                if (entry.EnumerateObject().Any(p => !IndexEntryFields.Contains(p.Name, StringComparer.Ordinal)))
                    throw new ModException("unknown index entry field", ModErrors.Catalog);
                var id = entry.GetProperty("id").GetString()!;
                if (!ids.Add(id)) throw new ModException("duplicate catalog id", ModErrors.Catalog);
                var version = entry.GetProperty("version").GetString()!;
                var hash = entry.GetProperty("sha256").GetString()!;
                var bytes = entry.GetProperty("bytes").GetInt64();
                if (hash.Length != 64 || hash.Any(c => !char.IsAsciiHexDigitLower(c) && !char.IsAsciiDigit(c)) || bytes is <= 0 or > MaxDetailBytes)
                    throw new ModException("invalid detail pin", ModErrors.Catalog);
                var detail = ResolveDetail(indexUrl, entry.GetProperty("path").GetString()!);
                if (detail is null || !allowed(detail)) throw new ModException("invalid detail path", ModErrors.Catalog);
                var card = new JsonObject();
                foreach (var field in EntryFields) card[field] = JsonNode.Parse(entry.GetProperty(field).GetRawText());
                entries.Add(new(id, version, card, detail, hash, bytes));
            }
            return entries.AsReadOnly();
        }
        catch (Exception error) when (IsEntryError(error)) { throw new ModException("invalid catalog: " + error.Message, ModErrors.Catalog); }
    }

    /// <summary>
    /// An entry's <c>path</c>: an absolute https URL, or a reference relative to the index (no leading "/", no ".." segment, no ? # : or
    /// backslash) that stays under the index's folder once resolved. Null when the path breaks a rule.
    /// </summary>
    internal static Uri? ResolveDetail(Uri indexUrl, string path)
    {
        ArgumentNullException.ThrowIfNull(indexUrl); ArgumentNullException.ThrowIfNull(path);
        if (path.Length is < 1 or > 2048 || path.Any(char.IsControl)) return null;
        const string Https = "https://";
        if (path.StartsWith(Https, StringComparison.Ordinal))
        {
            var rest = path[Https.Length..];
            var slash = rest.IndexOf('/', StringComparison.Ordinal);
            if (slash <= 0 || rest[..slash].IndexOfAny(['?', '#', '@', ':']) >= 0 || rest.Contains('#', StringComparison.Ordinal)) return null;
            return Uri.TryCreate(path, UriKind.Absolute, out var absolute) ? absolute : null;
        }
        if (path[0] == '/' || path.IndexOfAny(['?', '#', ':', '\\']) >= 0 || path.Split('/').Any(segment => segment == "..")
            || path.Contains("%2e", StringComparison.OrdinalIgnoreCase)) return null; // an escaped dot segment is refused like ".."
        if (!Uri.TryCreate(indexUrl, path, out var resolved) || !Uri.TryCreate(indexUrl, ".", out var folder)) return null;
        // an escaped dot segment ("%2e%2e") never leaves the index's folder either
        return resolved.AbsoluteUri.StartsWith(folder.AbsoluteUri, StringComparison.Ordinal) && resolved.AbsoluteUri.Length > folder.AbsoluteUri.Length ? resolved : null;
    }

    /// <summary>
    /// Every entry's detail bytes, in index order: the cached copy of its pin when there is one, else a fetch (at most
    /// <see cref="DetailConcurrency"/> at a time) checked against the pin and cached under <paramref name="cache"/> (null: no cache).
    /// Returns the merged manifests.
    /// </summary>
    internal static async Task<IReadOnlyList<ModManifest>> DetailsAsync(IReadOnlyList<IndexEntry> index, string? cache,
        Func<Uri, CancellationToken, Task<byte[]>> fetch, CancellationToken cancellation)
    {
        var raw = new byte[index.Count][];
        using var stop = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        using var gate = new SemaphoreSlim(DetailConcurrency);
        async Task One(int i)
        {
            var entry = index[i];
            if (cache is not null && ReadCachedDetail(cache, entry) is { } cached) { raw[i] = cached; return; }
            await gate.WaitAsync(stop.Token).ConfigureAwait(false);
            try
            {
                var bytes = await fetch(entry.Detail, stop.Token).ConfigureAwait(false);
                if (bytes.LongLength != entry.Bytes || Convert.ToHexStringLower(SHA256.HashData(bytes)) != entry.Sha256)
                    throw new CatalogDetailStaleException("the detail of '" + entry.Id + "' differs from its pin (a stale copy?)");
                raw[i] = bytes;
            }
            finally { gate.Release(); }
            // its bytes are the pinned ones: kept for the next refresh and for offline listing, whatever another detail does
            if (cache is not null) await WriteAtomicAsync(DetailPath(cache, entry.Sha256), raw[i], stop.Token).ConfigureAwait(false);
        }
        var work = Enumerable.Range(0, index.Count).Select(async i =>
        {
            try { await One(i).ConfigureAwait(false); }
            catch { await stop.CancelAsync().ConfigureAwait(false); throw; } // the first failure ends the others
        }).ToArray();
        try { await Task.WhenAll(work).ConfigureAwait(false); }
        catch
        {
            // the real cause, not a sibling's cancellation
            var cause = work.Where(t => t.IsFaulted).Select(t => t.Exception!.InnerException!).FirstOrDefault(e => e is not OperationCanceledException);
            if (cause is not null && !cancellation.IsCancellationRequested) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Throw(cause);
            throw;
        }
        var mods = new ModManifest[index.Count];
        for (var i = 0; i < index.Count; i++) mods[i] = MergedManifest(index[i], raw[i]);
        return mods;
    }

    /// <summary>
    /// The index entry with its detail file: the card details plus the detail minus <c>schemaVersion</c>/<c>id</c>/<c>version</c>, a
    /// schema-2 entry, made a manifest by the schema-2 entry rules. The bytes already match the pin.
    /// </summary>
    internal static ModManifest MergedManifest(IndexEntry entry, byte[] detail)
    {
        try
        {
            if (detail.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw new ModException("invalid detail", ModErrors.Catalog);
            using var json = JsonDocument.Parse(detail, new JsonDocumentOptions { MaxDepth = 40 });
            var root = json.RootElement;
            if (root.ValueKind != JsonValueKind.Object) throw new ModException("invalid detail", ModErrors.Catalog);
            ModManifest.RejectDuplicates(root);
            if (root.GetProperty("schemaVersion").GetInt32() != IndexSchemaVersion) throw new ModException("unsupported detail schema", ModErrors.Catalog);
            if (root.EnumerateObject().Any(p => !DetailFields.Contains(p.Name, StringComparer.Ordinal))) throw new ModException("unknown detail field", ModErrors.Catalog);
            if (root.GetProperty("id").GetString() != entry.Id || root.GetProperty("version").GetString() != entry.Version)
                throw new CatalogDetailStaleException("the detail of '" + entry.Id + "' names another id or version (a stale copy?)");
            var merged = (JsonObject)entry.Card.DeepClone();
            foreach (var property in root.EnumerateObject())
                if (property.Name is not ("schemaVersion" or "id" or "version")) merged[property.Name] = JsonNode.Parse(property.Value.GetRawText());
            using var v2 = JsonDocument.Parse(merged.ToJsonString(), new JsonDocumentOptions { MaxDepth = 40 });
            return EntryManifest(v2.RootElement);
        }
        catch (Exception error) when (IsEntryError(error)) { throw new ModException("invalid catalog: " + entry.Id + ": " + error.Message, ModErrors.Catalog); }
    }

    /// <summary>The cached detail of this pin (its bytes still hash to it), or null.</summary>
    private static byte[]? ReadCachedDetail(string cache, IndexEntry entry)
    {
        var path = DetailPath(cache, entry.Sha256);
        try
        {
            if (!File.Exists(path) || new FileInfo(path).Length != entry.Bytes) return null;
            var bytes = File.ReadAllBytes(path);
            return Convert.ToHexStringLower(SHA256.HashData(bytes)) == entry.Sha256 ? bytes : null;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>Removes the cached details the index no longer pins (the folder stays the size of one catalog).</summary>
    private static void PruneDetails(string cache, IReadOnlyList<IndexEntry> index)
    {
        if (!Directory.Exists(cache)) return;
        var kept = index.Select(e => e.Sha256 + ".json").ToHashSet(StringComparer.Ordinal);
        foreach (var file in Directory.EnumerateFiles(cache))
            // another load's staging file (".tmp") is that load's to move, never this one's to delete
            if (!kept.Contains(Path.GetFileName(file)) && !file.EndsWith(".tmp", StringComparison.OrdinalIgnoreCase))
                try { ManagedModFiles.RemoveFile(file); } catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
    }

    /// <summary>One detail folder per catalog address, next to its index's cache file.</summary>
    private static string DetailCache(string cacheDirectory, Uri url) => Path.ChangeExtension(CachePath(cacheDirectory, url), ".details");

    private static string DetailPath(string cache, string sha256) => Path.Combine(cache, sha256 + ".json");

    /// <summary>
    /// A schema-3 index read through the development route (<c>FI_DEV_MOD_CATALOG_URL</c>): the same rules, details read by
    /// <paramref name="read"/> (a local index may pin local details; a remote one only remote ones), no cache, never official or trusted.
    /// </summary>
    internal static async Task<ModCatalog> DevelopmentAsync(Uri url, byte[] raw, Func<Uri, bool> allowed,
        Func<Uri, CancellationToken, Task<byte[]>> read, CancellationToken cancellation)
    {
        var index = ParseIndex(raw, url, allowed);
        var mods = await DetailsAsync(index, cache: null, read, cancellation).ConfigureAwait(false);
        return Build(DevelopmentUrl, mods, cached: false);
    }
}
