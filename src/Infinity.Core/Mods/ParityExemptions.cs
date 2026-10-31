// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>One entry of InfinityMods' <c>parity-non-required.json</c>: the id the mod loader gives the mod, its name and description.</summary>
public sealed record ParityExemption(string Id, string Name, string Description);

/// <summary>
/// OD-166: mods left out of the lobby's mod set (MATCHMAKING_API.md §2.7 "The parity set"). The list is one unsigned JSON file on
/// InfinityMods' <c>main</c>, <c>{"schemaVersion": 1, "mods": [{id, name, description}]}</c>, read from raw.githubusercontent.com,
/// cached on disk, with a built-in copy for a start without either. An installed mod is exempt when the id the mod loader gives it
/// (its <c>mod.json</c> id, else its folder name; <see cref="ExtraOptionsMod.Id"/>) equals an entry's id, whatever its files (the
/// owner, 2026-10-31: id only, no hash). Parsing is strict: an unknown field, a bad or repeated id refuses the whole file, and the
/// launcher then uses the cached copy, else the built-in one.
/// </summary>
public sealed class ParityExemptions
{
    public const int SchemaVersion = 1;
    public const int MaxBytes = 256 * 1024;
    public const int MaxMods = 256;
    public const string CacheFile = "parity-non-required.json";
    private const string BuiltInResource = "Infinity.Core.parity-non-required.json";

    public static Uri OfficialUrl { get; } = new("https://raw.githubusercontent.com/anarqz/InfinityMods/main/parity-non-required.json");

    /// <summary>No exemption (every mod stays in the set).</summary>
    public static ParityExemptions Empty { get; } = new([], "none");

    private readonly HashSet<string> _ids;

    private ParityExemptions(IReadOnlyList<ParityExemption> mods, string source)
    {
        Mods = mods;
        Source = source;
        _ids = mods.Select(m => m.Id).ToHashSet(StringComparer.Ordinal);
    }

    public IReadOnlyList<ParityExemption> Mods { get; }

    /// <summary>Where the list came from: <c>network</c>, <c>cache</c>, <c>built-in</c> (or <c>none</c>).</summary>
    public string Source { get; }

    /// <summary>The mod the loader knows by this id (<see cref="ExtraOptionsMod.Id"/>) is exempt; compared exactly (the loader's ids are lower case).</summary>
    public bool Exempts(string loaderId) => loaderId is not null && _ids.Contains(loaderId);

    /// <summary>Parses the file strictly (see the class remarks); throws <see cref="ModException"/> (<see cref="ModErrors.Catalog"/>).</summary>
    public static ParityExemptions Parse(byte[] raw, string source = "network")
    {
        ArgumentNullException.ThrowIfNull(raw);
        if (raw.Length is <= 0 or > MaxBytes || raw.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw Invalid("size or encoding");
        try
        {
            using var json = JsonDocument.Parse(raw, new JsonDocumentOptions { MaxDepth = 8 });
            var root = json.RootElement;
            if (root.ValueKind != JsonValueKind.Object) throw Invalid("not an object");
            ModManifest.RejectDuplicates(root);
            if (root.EnumerateObject().Any(p => p.Name is not ("schemaVersion" or "mods"))) throw Invalid("unknown field");
            if (root.GetProperty("schemaVersion") is not { ValueKind: JsonValueKind.Number } version || !version.TryGetInt32(out var number) || number != SchemaVersion)
                throw Invalid("unsupported schema");
            var list = root.GetProperty("mods");
            if (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > MaxMods) throw Invalid("mods");
            var mods = new List<ParityExemption>();
            var ids = new HashSet<string>(StringComparer.Ordinal);
            foreach (var entry in list.EnumerateArray())
            {
                if (entry.ValueKind != JsonValueKind.Object || entry.EnumerateObject().Any(p => p.Name is not ("id" or "name" or "description")))
                    throw Invalid("entry fields");
                var id = Text(entry, "id", 64);
                if (!ModManifest.ValidId(id) || ModManifest.IsCustomId(id)) throw Invalid("id");
                if (!ids.Add(id)) throw Invalid("duplicate id");
                mods.Add(new(id, Text(entry, "name", 128), Text(entry, "description", 1024)));
            }
            return new(mods.AsReadOnly(), source);
        }
        catch (Exception error) when (error is JsonException or KeyNotFoundException or InvalidOperationException or FormatException
            or ModException { Code: not ModErrors.Catalog })
        { throw Invalid(error.GetType().Name); }
    }

    private static string Text(JsonElement entry, string field, int max)
    {
        var value = entry.GetProperty(field);
        return value.ValueKind == JsonValueKind.String && value.GetString() is { Length: > 0 } text && text.Length <= max ? text : throw Invalid(field);
    }

    private static ModException Invalid(string what) => new("invalid parity exemptions: " + what, ModErrors.Catalog);

    /// <summary>The copy built into this launcher (equal to InfinityMods' file at release time).</summary>
    public static ParityExemptions BuiltIn()
    {
        using var stream = typeof(ParityExemptions).Assembly.GetManifestResourceStream(BuiltInResource)
            ?? throw new InvalidOperationException("the built-in parity exemptions are missing");
        using var buffer = new MemoryStream();
        stream.CopyTo(buffer);
        return Parse(buffer.ToArray(), "built-in");
    }

    /// <summary>The cached copy if it is still valid, else the built-in one. Never throws for a missing or bad cache.</summary>
    public static ParityExemptions CachedOrBuiltIn(string cacheDirectory)
    {
        ArgumentNullException.ThrowIfNull(cacheDirectory);
        try
        {
            var path = Path.Combine(Path.GetFullPath(cacheDirectory), CacheFile);
            if (File.Exists(path) && new FileInfo(path).Length <= MaxBytes) return Parse(File.ReadAllBytes(path), "cache");
        }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { }
        return BuiltIn();
    }

    /// <summary>
    /// Fetches the list (<paramref name="url"/>, by default <see cref="OfficialUrl"/>), checks it and caches it. An unreachable,
    /// refused or invalid answer falls back to <see cref="CachedOrBuiltIn"/>; <paramref name="log"/> gets one line naming the error
    /// type only. A cancellation of <paramref name="cancellation"/> is passed on.
    /// </summary>
    public static async Task<ParityExemptions> LoadAsync(HttpClient http, string cacheDirectory, CancellationToken cancellation, Uri? url = null, Action<string>? log = null)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(cacheDirectory);
        try
        {
            byte[] raw;
            using (var response = await ModDownloads.DownloadAsync(http, url ?? OfficialUrl, _ => true, cancellation).ConfigureAwait(false))
            {
                if (response.StatusCode != System.Net.HttpStatusCode.OK) throw new ModException("parity exemptions unavailable: " + (int)response.StatusCode, ModErrors.Catalog);
                await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
                try { raw = await DevelopmentModCatalog.ReadBoundedAsync(body, MaxBytes, cancellation).ConfigureAwait(false); }
                catch (ModException error) when (error.Code == ModErrors.Download) { throw Invalid("too large"); }
            }
            var list = Parse(raw);
            var path = Path.Combine(Path.GetFullPath(cacheDirectory), CacheFile);
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            await File.WriteAllBytesAsync(staging, raw, cancellation).ConfigureAwait(false);
            File.Move(staging, path, overwrite: true);
            return list;
        }
        catch (Exception error) when (!cancellation.IsCancellationRequested
            && error is ModException or HttpRequestException or IOException or UnauthorizedAccessException or OperationCanceledException)
        {
            var fallback = CachedOrBuiltIn(cacheDirectory);
            log?.Invoke("mods: parity exemptions not fetched (" + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "") + "), using the " + fallback.Source + " copy");
            return fallback;
        }
    }
}
