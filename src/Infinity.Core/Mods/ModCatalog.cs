// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Mods;

/// <summary>A loaded mod catalog: its manifests, the hosts its packages may come from, and how it was trusted.</summary>
public sealed record ModCatalog(string CatalogId, long Sequence, DateTimeOffset? IssuedAt, DateTimeOffset? ExpiresAt,
    IReadOnlyList<string> DownloadHosts, IReadOnlyList<ModManifest> Mods, bool Trusted, bool Cached)
{
    /// <summary>An expired catalog still lists and manages installed mods; it no longer installs or updates.</summary>
    public bool Expired(DateTimeOffset now) => ExpiresAt is { } expires && now >= expires;
}

/// <summary>
/// Production catalog trust (§1.2): a detached Ed25519 signature by the release key (OD-49) over
/// <c>"fi-mod-catalog-v1\n" + lowercase hex SHA-256(catalog bytes)</c>, the same construction as the
/// launcher release descriptor with its own domain prefix. Served as two assets of the pre-release
/// <c>mods-catalog</c> on the client repository's GitHub releases (a pre-release never becomes the
/// "latest" launcher release).
/// </summary>
public static class ModCatalogTrust
{
    public const string SignaturePrefix = "fi-mod-catalog-v1\n";
    public const string ReleaseTag = "mods-catalog";
    public const string AssetName = "infinity-mods-catalog.json";
    public const int MaxCatalogBytes = 1024 * 1024;
    public const int MaxManifestBytes = 256 * 1024;
    public static Uri ProductionUrl { get; } = new($"https://github.com/{Update.GitHubReleases.Repository}/releases/download/{ReleaseTag}/{AssetName}");
    /// <summary>Hosts the catalog and its signature are fetched through (GitHub's release redirect chain); package hosts come from the signed catalog.</summary>
    public static IReadOnlyList<string> TransportHosts { get; } = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "github-releases.githubusercontent.com"];
    public static Uri SignatureUrl(Uri catalog) => new((catalog ?? throw new ArgumentNullException(nameof(catalog))).AbsoluteUri + ".sig");
    public static byte[] SignatureMessage(ReadOnlySpan<byte> raw) => Encoding.ASCII.GetBytes(SignaturePrefix + Convert.ToHexStringLower(SHA256.HashData(raw)));

    public static bool Verify(ReadOnlySpan<byte> raw, ReadOnlySpan<byte> signature, ReadOnlySpan<byte> publicKey)
    {
        if (raw.Length is <= 0 or > MaxCatalogBytes || signature.Length != 64 || publicKey.Length != 32 || !Ed25519.ValidatePublicKeyFull(publicKey.ToArray(), 0)) return false;
        var message = SignatureMessage(raw);
        return Ed25519.Verify(signature.ToArray(), 0, publicKey.ToArray(), 0, message, 0, message.Length);
    }

    internal sealed record Entry(string Id, string Version, Uri ManifestUrl, string ManifestSha256);

    /// <summary>Parses a catalog. <paramref name="signed"/>: every field required, unknown fields refused, only "production" entries kept.</summary>
    internal static (string Id, long Sequence, DateTimeOffset? Issued, DateTimeOffset? Expires, IReadOnlyList<string> Hosts, IReadOnlyList<Entry> Entries) Parse(byte[] raw, bool signed)
    {
        if (raw.Length is <= 0 or > MaxCatalogBytes || raw.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw new ModException("invalid catalog", ModErrors.Catalog);
        using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 16 });
        var root = json.RootElement;
        ModManifest.RejectDuplicates(root);
        if (root.GetProperty("schemaVersion").GetInt32() != 1) throw new ModException("unsupported catalog", ModErrors.Catalog);
        if (signed && root.EnumerateObject().Any(p => p.Name is not ("schemaVersion" or "catalogId" or "sequence" or "issuedAt" or "expiresAt" or "downloadHosts" or "mods")))
            throw new ModException("unknown catalog field", ModErrors.Catalog);
        var id = root.TryGetProperty("catalogId", out var catalogId) ? catalogId.GetString()! : "development";
        if (id.Length is < 1 or > 64 || id.Any(c => !(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-'))) throw new ModException("invalid catalog id", ModErrors.Catalog);
        var sequence = root.TryGetProperty("sequence", out var seq) ? seq.GetInt64() : 0;
        DateTimeOffset? Time(string name) => root.TryGetProperty(name, out var value)
            ? DateTimeOffset.Parse(value.GetString()!, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal) : null;
        var issued = Time("issuedAt"); var expires = Time("expiresAt");
        var hosts = ModDownloads.Hosts(root.TryGetProperty("downloadHosts", out var list) ? list : null);
        if (signed && (sequence < 1 || issued is null || expires is null || expires <= issued || hosts.Count == 0))
            throw new ModException("incomplete signed catalog", ModErrors.Catalog);
        var mods = root.GetProperty("mods");
        if (mods.GetArrayLength() > 512) throw new ModException("catalog too large", ModErrors.Catalog);
        var entries = new List<Entry>();
        foreach (var entry in mods.EnumerateArray())
        {
            var availability = entry.TryGetProperty("availability", out var value) ? value.GetString() : "draft";
            if (availability is not ("draft" or "production")) throw new ModException("invalid catalog availability", ModErrors.Catalog);
            if (signed && availability != "production") continue;
            var digest = entry.GetProperty("manifestSha256").GetString()!;
            if (digest.Length != 64 || digest.Any(c => !char.IsAsciiHexDigitLower(c) && !char.IsAsciiDigit(c))) throw new ModException("invalid manifest digest", ModErrors.Catalog);
            var url = new Uri(entry.GetProperty("manifestUrl").GetString()!, UriKind.Absolute);
            if (signed && (url.Scheme != Uri.UriSchemeHttps || !hosts.Contains(url.IdnHost, StringComparer.OrdinalIgnoreCase))) throw new ModException("manifest host not allowed", ModErrors.Catalog);
            if (entries.Any(e => e.Id == entry.GetProperty("id").GetString())) throw new ModException("catalog identity mismatch", ModErrors.Catalog);
            entries.Add(new(entry.GetProperty("id").GetString()!, entry.GetProperty("version").GetString()!, url, digest));
        }
        return (id, sequence, issued, expires, hosts, entries);
    }

    internal static ModManifest Manifest(Entry entry, byte[] raw)
    {
        if (ModManifest.Hash(raw) != entry.ManifestSha256) throw new ModException("manifest digest mismatch", ModErrors.Digest);
        var mod = new ModManifest(raw);
        if (mod.Id != entry.Id || mod.Version != entry.Version) throw new ModException("catalog identity mismatch", ModErrors.Catalog);
        return mod;
    }
}

/// <summary>
/// The production catalog: fetched with a credential-free client through validated redirects,
/// verified before parsing, refused below the recorded sequence floor (rollback), then cached with
/// its manifests so installed and catalog mods stay manageable offline.
/// </summary>
public sealed class SignedModCatalog
{
    private readonly HttpClient _http;
    private readonly string _cache;
    private readonly byte[] _key;
    private readonly TimeProvider _clock;

    public SignedModCatalog(HttpClient http, string cacheDirectory) : this(http, cacheDirectory, Process.HookInstaller.ReleaseKey.ToArray(), TimeProvider.System) { }

    /// <summary>Tests: an ephemeral key instead of the embedded release key.</summary>
    internal SignedModCatalog(HttpClient http, string cacheDirectory, byte[] publicKey, TimeProvider clock)
    {
        _http = http ?? throw new ArgumentNullException(nameof(http)); _cache = Path.GetFullPath(cacheDirectory); _key = publicKey.ToArray(); _clock = clock;
        if (_key.Length != 32 || !Ed25519.ValidatePublicKeyFull(_key, 0)) throw new ArgumentException("invalid catalog key", nameof(publicKey));
    }

    private string CatalogPath => Path.Combine(_cache, ModCatalogTrust.AssetName);
    private string FloorPath => Path.Combine(_cache, "floor.json");

    public async Task<ModCatalog> LoadAsync(Uri url, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(url);
        var raw = await FetchAsync(url, ModCatalogTrust.TransportHosts, ModCatalogTrust.MaxCatalogBytes, cancellation).ConfigureAwait(false);
        var signature = await FetchAsync(ModCatalogTrust.SignatureUrl(url), ModCatalogTrust.TransportHosts, 64, cancellation).ConfigureAwait(false);
        if (!ModCatalogTrust.Verify(raw, signature, _key)) throw new ModException("catalog signature does not verify", ModErrors.Catalog);
        var parsed = ModCatalogTrust.Parse(raw, signed: true);
        CheckTime(parsed.Issued);
        var digest = ModManifest.Hash(raw);
        if (ReadFloor() is { } floor && floor.Id == parsed.Id && (parsed.Sequence < floor.Sequence || parsed.Sequence == floor.Sequence && floor.Digest != digest))
            throw new ModException("catalog is older than one already seen", ModErrors.Catalog);
        var mods = new List<ModManifest>();
        foreach (var entry in parsed.Entries)
        {
            var cached = CachedManifest(entry.ManifestSha256);
            var bytes = cached ?? await FetchAsync(entry.ManifestUrl, parsed.Hosts, ModCatalogTrust.MaxManifestBytes, cancellation).ConfigureAwait(false);
            var manifest = ModCatalogTrust.Manifest(entry, bytes);
            if (cached is null) Write(Path.Combine(_cache, "manifests", entry.ManifestSha256 + ".json"), bytes);
            mods.Add(manifest);
        }
        Write(CatalogPath, raw); Write(CatalogPath + ".sig", signature);
        Write(FloorPath, JsonSerializer.SerializeToUtf8Bytes(new Floor(parsed.Id, parsed.Sequence, digest)));
        return new(parsed.Id, parsed.Sequence, parsed.Issued, parsed.Expires, parsed.Hosts, mods.AsReadOnly(), Trusted: true, Cached: false);
    }

    /// <summary>The last verified catalog with the manifests cached for it (offline), or null.</summary>
    public ModCatalog? LoadCached()
    {
        if (!File.Exists(CatalogPath) || !File.Exists(CatalogPath + ".sig")) return null;
        var raw = File.ReadAllBytes(CatalogPath); var signature = File.ReadAllBytes(CatalogPath + ".sig");
        if (!ModCatalogTrust.Verify(raw, signature, _key)) return null;
        var parsed = ModCatalogTrust.Parse(raw, signed: true);
        var mods = parsed.Entries.Select(e => CachedManifest(e.ManifestSha256) is { } bytes ? ModCatalogTrust.Manifest(e, bytes) : null).OfType<ModManifest>().ToArray();
        return new(parsed.Id, parsed.Sequence, parsed.Issued, parsed.Expires, parsed.Hosts, mods, Trusted: true, Cached: true);
    }

    private void CheckTime(DateTimeOffset? issued)
    {
        if (issued is { } time && time > _clock.GetUtcNow().AddDays(1)) throw new ModException("catalog issued in the future", ModErrors.Catalog);
    }

    private byte[]? CachedManifest(string digest)
    {
        var path = Path.Combine(_cache, "manifests", digest + ".json");
        if (!File.Exists(path)) return null;
        var bytes = File.ReadAllBytes(path);
        return ModManifest.Hash(bytes) == digest ? bytes : null;
    }

    private sealed record Floor(string Id, long Sequence, string Digest);

    private Floor? ReadFloor()
    {
        try { return File.Exists(FloorPath) ? JsonSerializer.Deserialize<Floor>(File.ReadAllBytes(FloorPath)) : null; }
        catch (JsonException) { return null; }
    }

    private static void Write(string path, byte[] bytes)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        File.WriteAllBytes(staging, bytes);
        File.Move(staging, path, overwrite: true);
    }

    private async Task<byte[]> FetchAsync(Uri url, IReadOnlyCollection<string> hosts, int limit, CancellationToken cancellation)
    {
        using var response = await ModDownloads.DownloadAsync(_http, url, hosts, cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        return await DevelopmentModCatalog.ReadBoundedAsync(body, limit, cancellation).ConfigureAwait(false);
    }
}

/// <summary>
/// DEV only (FI_DEV_MOD_CATALOG_URL, never in production builds): an unsigned catalog from a local
/// file, loopback HTTP or HTTPS URL. Remote catalogs cannot reference local files.
/// </summary>
public static class DevelopmentModCatalog
{
    public static async Task<ModCatalog> LoadAsync(HttpClient http, Uri url, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(url);
        var bytes = await ReadAsync(http, url, 256 * 1024, cancellation).ConfigureAwait(false);
        var parsed = ModCatalogTrust.Parse(bytes, signed: false);
        if (parsed.Entries.Count > 64) throw new ModException("catalog too large", ModErrors.Catalog);
        var mods = new List<ModManifest>();
        foreach (var entry in parsed.Entries)
        {
            if (entry.ManifestUrl.IsFile && !url.IsFile) throw new ModException("remote catalog references a local file", ModErrors.Catalog);
            var raw = await ReadAsync(http, entry.ManifestUrl, 256 * 1024, cancellation).ConfigureAwait(false);
            mods.Add(ModCatalogTrust.Manifest(entry, raw));
        }
        return new(parsed.Id, parsed.Sequence, parsed.Issued, parsed.Expires, parsed.Hosts, mods.AsReadOnly(), Trusted: false, Cached: false);
    }

    private static async Task<byte[]> ReadAsync(HttpClient http, Uri url, int limit, CancellationToken cancellation)
    {
        if (url.IsFile)
        {
            if (url.IsUnc || new FileInfo(url.LocalPath).Length > limit) throw new ModException("invalid local catalog file", ModErrors.Catalog);
            return await File.ReadAllBytesAsync(url.LocalPath, cancellation).ConfigureAwait(false);
        }
        if (url.Scheme != "https" && !(url.Scheme == "http" && url.IsLoopback)
            || url.UserInfo.Length > 0 || url.Fragment.Length > 0) throw new ModException("invalid catalog URL", ModErrors.Catalog);
        using var response = await http.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        if (response.RequestMessage?.RequestUri != url) throw new ModException("catalog redirects are not supported", ModErrors.Catalog);
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
            if (result.Length + count > limit) throw new ModException("download too large", ModErrors.Download);
            result.Write(buffer, 0, count);
        }
        return result.ToArray();
    }
}
