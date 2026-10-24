// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;
using Infinity.Core.Update;

namespace Infinity.Core.News;

/// <summary>The feed as shown: the last good copy and when it was last confirmed; <see cref="Stale"/> when the latest fetch failed.</summary>
public sealed record NewsSnapshot(NewsFeed Feed, DateTimeOffset AsOf, bool Stale);

/// <summary>An item's markdown in the language found (the player's, else the other one).</summary>
public sealed record NewsMarkdown(string Text, string Language);

/// <summary>
/// OD-119 (NEWS_FEED.md §4): reads the feed from <c>raw.githubusercontent.com</c> only (never api.github.com, never an Infinity host),
/// so it works signed out and offline-first. Conditional requests (ETag); redirects only on the raw host; a failed fetch keeps the
/// last good copy, shown with its date. A development base may be a local folder (<c>file://</c>).
/// </summary>
public sealed class NewsClient : IDisposable
{
    public const string RawHost = "raw.githubusercontent.com";
    public static Uri OfficialBase { get; } = new("https://" + RawHost + "/anarqz/InfinityNews/main/");
    private const int MaxRedirects = 3;
    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(15);

    private readonly Uri _base;
    private readonly string _cache;
    private readonly HttpClient _http;
    private readonly Func<DateTimeOffset> _now;

    public NewsClient(Uri baseUri, string cacheDirectory, HttpMessageHandler? handler = null, Func<DateTimeOffset>? now = null)
    {
        ArgumentNullException.ThrowIfNull(baseUri); ArgumentNullException.ThrowIfNull(cacheDirectory);
        if (!baseUri.AbsoluteUri.EndsWith('/') || !(baseUri.IsFile && !baseUri.IsUnc || Allowed(baseUri)))
            throw new ArgumentException("the news base must be a raw.githubusercontent.com folder or a local folder", nameof(baseUri));
        _base = baseUri; _cache = Path.GetFullPath(cacheDirectory); _now = now ?? (() => DateTimeOffset.UtcNow);
        _http = new HttpClient(handler ?? new SocketsHttpHandler { AllowAutoRedirect = false, AutomaticDecompression = DecompressionMethods.All }, disposeHandler: true)
        { Timeout = Timeout.InfiniteTimeSpan };
    }

    /// <summary>The why of the last failed fetch, for the log (a status or an exception type; never a body).</summary>
    public string? LastError { get; private set; }

    private static bool Allowed(Uri url) => url.IsAbsoluteUri && url.Scheme == Uri.UriSchemeHttps && url.IsDefaultPort && url.UserInfo.Length == 0
        && url.Fragment.Length == 0 && url.Query.Length == 0 && string.Equals(url.IdnHost, RawHost, StringComparison.OrdinalIgnoreCase);

    /// <summary>The last good copy, without the network (offline-first: shown at once, then refreshed).</summary>
    public NewsSnapshot? Cached()
    {
        if (Read("news.json") is not { } cached) return null;
        try { return new(NewsFeed.Parse(cached.Bytes), cached.AsOf, Stale: false); }
        catch (InvalidDataException) { return null; }
    }

    /// <summary>Fetches <c>news.json</c>; on any failure the last good copy (marked stale), or null when there has never been one.</summary>
    public async Task<NewsSnapshot?> RefreshAsync(CancellationToken cancellation)
    {
        var fetched = await FetchAsync("news.json", NewsFeed.MaxIndexBytes, bytes => Valid(() => NewsFeed.Parse(bytes)), cancellation).ConfigureAwait(false);
        if (fetched is not { Bytes: { } bytes }) return null;
        return new(NewsFeed.Parse(bytes), fetched.AsOf, fetched.Stale);
    }

    /// <summary>The item's markdown: the player's language, else the other; null when neither can be read.</summary>
    public async Task<NewsMarkdown?> MarkdownAsync(NewsItem item, string locale, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(item);
        if (!NewsFeed.IsId(item.Id)) return null;
        var first = locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en";
        foreach (var language in new[] { first, first == "en" ? "pt-BR" : "en" })
        {
            var fetched = await FetchAsync($"news/{item.Id}/{language}.md", NewsFeed.MaxMarkdownBytes, _ => true, cancellation).ConfigureAwait(false);
            if (fetched is { Bytes: { } bytes }) return new(NewsFeed.Markdown(bytes), language);
        }
        return null;
    }

    /// <summary>An image of the item (its cover or a markdown image), from the item's folder; null when it cannot be read (the alt text shows).</summary>
    public async Task<byte[]?> AssetAsync(NewsItem item, string path, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(item);
        if (!NewsFeed.IsId(item.Id) || !NewsFeed.IsItemPath(path)) return null;
        var fetched = await FetchAsync($"news/{item.Id}/{path}", NewsFeed.MaxImageBytes, _ => true, cancellation).ConfigureAwait(false);
        return fetched?.Bytes;
    }

    private static bool Valid(Action parse)
    {
        try { parse(); return true; }
        catch (InvalidDataException) { return false; }
    }

    private sealed record Fetched(byte[]? Bytes, DateTimeOffset AsOf, bool Stale);
    private sealed record Meta(string? ETag, DateTimeOffset AsOf);
    private sealed record Stored(byte[] Bytes, string? ETag, DateTimeOffset AsOf);

    /// <summary>
    /// One file under the base: from the network (304 keeps the cache), validated before it replaces the cache. A 404 means absent
    /// (null bytes, the cache dropped); any other failure falls back to the cached copy, marked stale; null when there is none.
    /// </summary>
    private async Task<Fetched?> FetchAsync(string relative, int limit, Func<byte[], bool> valid, CancellationToken cancellation)
    {
        var cached = Read(relative);
        try
        {
            var (status, bytes, etag) = await GetAsync(new Uri(_base, relative), cached?.ETag, limit, cancellation).ConfigureAwait(false);
            if (status == HttpStatusCode.NotModified && cached is not null) { Write(relative, cached.Bytes, cached.ETag); return new(cached.Bytes, _now(), false); }
            if (status == HttpStatusCode.NotFound) { Forget(relative); return new(null, _now(), false); }
            if (status != HttpStatusCode.OK || bytes is null) throw new HttpRequestException("HTTP " + (int)status, null, status);
            if (!valid(bytes)) throw new InvalidDataException("invalid " + Path.GetExtension(relative) + " file");
            Write(relative, bytes, etag);
            LastError = null;
            return new(bytes, _now(), false);
        }
        catch (Exception error) when (error is HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException
            || error is OperationCanceledException && !cancellation.IsCancellationRequested)
        {
            LastError = error is HttpRequestException { StatusCode: { } code } ? "HTTP " + (int)code : error.GetType().Name;
            return cached is null ? null : new(cached.Bytes, cached.AsOf, true);
        }
    }

    private async Task<(HttpStatusCode Status, byte[]? Bytes, string? ETag)> GetAsync(Uri url, string? etag, int limit, CancellationToken cancellation)
    {
        if (url.IsFile)
        {
            // Development: a local fixture folder. Same bounds; never a path outside the base.
            var path = Path.GetFullPath(url.LocalPath);
            if (!path.StartsWith(Path.GetFullPath(_base.LocalPath), StringComparison.OrdinalIgnoreCase)) return (HttpStatusCode.NotFound, null, null);
            if (!File.Exists(path)) return (HttpStatusCode.NotFound, null, null);
            if (new FileInfo(path).Length > limit) throw new InvalidDataException("file over its limit");
            return (HttpStatusCode.OK, await File.ReadAllBytesAsync(path, cancellation).ConfigureAwait(false), null);
        }
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(RequestTimeout);
        for (var hop = 0; ; hop++)
        {
            if (!Allowed(url)) throw new HttpRequestException("news redirect off " + RawHost);
            using var request = new HttpRequestMessage(HttpMethod.Get, url);
            request.Headers.TryAddWithoutValidation("User-Agent", "infinity-launcher");
            if (etag is not null && EntityTagHeaderValue.TryParse(etag, out var tag)) request.Headers.IfNoneMatch.Add(tag);
            using var response = await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, bound.Token).ConfigureAwait(false);
            if ((int)response.StatusCode is 301 or 302 or 303 or 307 or 308)
            {
                if (hop >= MaxRedirects || response.Headers.Location is not { } next) throw new HttpRequestException("too many news redirects");
                url = next.IsAbsoluteUri ? next : new Uri(url, next);
                continue;
            }
            if (response.StatusCode != HttpStatusCode.OK) return (response.StatusCode, null, null);
            if (response.Content.Headers.ContentLength > limit) throw new InvalidDataException("response over its limit");
            await using var stream = await response.Content.ReadAsStreamAsync(bound.Token).ConfigureAwait(false);
            using var buffer = new MemoryStream();
            var chunk = new byte[81920];
            int read;
            while ((read = await stream.ReadAsync(chunk, bound.Token).ConfigureAwait(false)) > 0)
            {
                if (buffer.Length + read > limit) throw new InvalidDataException("response over its limit");
                buffer.Write(chunk, 0, read);
            }
            return (HttpStatusCode.OK, buffer.ToArray(), response.Headers.ETag?.ToString());
        }
    }

    // The cache: one file per fetched path under <cache>, with a small .meta beside it (ETag, when it was last confirmed).

    private string CachePath(string relative) => Path.Combine(_cache, relative.Replace('/', Path.DirectorySeparatorChar));

    private Stored? Read(string relative)
    {
        var path = CachePath(relative);
        try
        {
            if (!File.Exists(path) || !File.Exists(path + ".meta")) return null;
            var meta = JsonSerializer.Deserialize<Meta>(File.ReadAllBytes(path + ".meta"));
            return meta is null ? null : new(File.ReadAllBytes(path), meta.ETag, meta.AsOf);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException) { return null; }
    }

    private void Write(string relative, byte[] bytes, string? etag)
    {
        var path = CachePath(relative);
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            Atomic(path, bytes);
            Atomic(path + ".meta", JsonSerializer.SerializeToUtf8Bytes(new Meta(etag, _now())));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { } // a cache that cannot be written only costs a refetch
    }

    private void Forget(string relative)
    {
        var path = CachePath(relative);
        try { File.Delete(path); File.Delete(path + ".meta"); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
    }

    private static void Atomic(string path, byte[] bytes)
    {
        var temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        File.WriteAllBytes(temporary, bytes);
        File.Move(temporary, path, overwrite: true);
    }

    public void Dispose() => _http.Dispose();
}

/// <summary>Read/unread per item id, local to this PC (NEWS_FEED.md §4). Bounded: the newest 500 ids are kept.</summary>
public sealed class NewsReadState(string stateDirectory)
{
    private const int MaxIds = 500;
    private readonly string _path = Path.Combine(Path.GetFullPath(stateDirectory), "news-read.json");
    private List<string>? _ids;

    private List<string> Ids
    {
        get
        {
            if (_ids is not null) return _ids;
            try { _ids = File.Exists(_path) ? JsonSerializer.Deserialize<List<string>>(File.ReadAllBytes(_path))?.Where(NewsFeed.IsId).ToList() : null; }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException) { }
            return _ids ??= [];
        }
    }

    public bool IsRead(string id) => Ids.Contains(id, StringComparer.Ordinal);

    public void MarkRead(string id)
    {
        if (!NewsFeed.IsId(id) || IsRead(id)) return;
        Ids.Add(id);
        if (Ids.Count > MaxIds) Ids.RemoveRange(0, Ids.Count - MaxIds);
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            File.WriteAllBytes(_path, JsonSerializer.SerializeToUtf8Bytes(Ids));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
    }
}
