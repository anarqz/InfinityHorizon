// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Immutable;
using System.Net;
using System.Net.Http.Headers;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Update;

// Local GitHub transport metadata, not the independently signed launcher update descriptor.
public sealed record GitHubRelease(LauncherVersion Version, string Tag, Uri PageUrl, IReadOnlyList<GitHubReleaseAsset> Assets);
public sealed record GitHubReleaseAsset(long Id, string Name, long Size, Uri DownloadUrl, string? Sha256 = null);

/// <summary>Unauthenticated fixed-repository discovery and bounded downloads. Absence/failure is never a current-version result.</summary>
public sealed class GitHubReleases : IDisposable
{
    public const string Repository = "anarqz/InfinityHorizon";
    public const string ApiVersion = "2022-11-28";
    public const int MaxMetadataBytes = 1024 * 1024;
    public const int MaxAssets = 64;
    public const long MaxAssetBytes = 512L * 1024 * 1024;
    public const int MaxRedirects = 5;
    public static Uri LatestUrl { get; } = new($"https://api.github.com/repos/{Repository}/releases/latest");
    private static readonly UTF8Encoding Utf8 = new(false, true);
    private readonly HttpClient _http;
    private readonly bool _ownsHttp;

    public GitHubReleases()
    {
        _http = new(new SocketsHttpHandler
        {
            AllowAutoRedirect = false, UseCookies = false, Credentials = null,
            AutomaticDecompression = DecompressionMethods.None,
        }) { Timeout = Timeout.InfiniteTimeSpan };
        _ownsHttp = true;
    }

    // Private fixture seam: its handler must not follow redirects. No caller/default credentials are accepted.
    internal GitHubReleases(HttpClient http)
    {
        ArgumentNullException.ThrowIfNull(http);
        if (http.DefaultRequestHeaders.Any()) throw new ArgumentException("A dedicated credential-free HTTP client is required", nameof(http));
        _http = http;
    }

    public async Task<GitHubRelease?> GetLatestAsync(CancellationToken cancellation = default)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(TimeSpan.FromSeconds(10));
        DedicatedClient();
        using var request = Request(LatestUrl, metadata: true);
        using var response = await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, bound.Token).ConfigureAwait(false);
        CheckFinalUrl(response, LatestUrl);
        if (response.StatusCode == HttpStatusCode.NotFound) return null;
        Status(response);
        if (response.Content.Headers.ContentType?.MediaType is not ("application/json" or "application/vnd.github+json")) Invalid();
        var bytes = await ReadMetadataAsync(response.Content, bound.Token).ConfigureAwait(false);
        return Parse(bytes);
    }

    public async Task<byte[]> DownloadBytesAsync(GitHubReleaseAsset asset, long maxBytes, CancellationToken cancellation = default)
    {
        using var output = new MemoryStream();
        await DownloadAssetAsync(asset, output, maxBytes, cancellation: cancellation).ConfigureAwait(false);
        return output.ToArray();
    }

    public async Task DownloadAssetAsync(GitHubReleaseAsset asset, Stream destination, long maxBytes,
        IProgress<long>? progress = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(asset); ArgumentNullException.ThrowIfNull(destination);
        if (!destination.CanWrite) throw new ArgumentException("A writable destination is required", nameof(destination));
        if (maxBytes is < 0 or > MaxAssetBytes) throw new ArgumentOutOfRangeException(nameof(maxBytes));
        if (asset.Id <= 0 || !AssetName(asset.Name) || asset.Size < 0 || asset.Size > maxBytes || !Digest(asset.Sha256)
            || !DownloadUrl(asset.DownloadUrl, asset.Name)) Invalid();
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(TimeSpan.FromMinutes(5));
        var location = asset.DownloadUrl;
        var visited = new HashSet<string>(StringComparer.Ordinal);
        for (var redirects = 0; ; redirects++)
        {
            if (!AssetHost(location) || !visited.Add(location.AbsoluteUri)) Invalid();
            DedicatedClient();
            using var request = Request(location, metadata: false);
            using var response = await SendHeadersAsync(request, bound.Token).ConfigureAwait(false);
            CheckFinalUrl(response, location);
            if (response.StatusCode is HttpStatusCode.MovedPermanently or HttpStatusCode.Found or HttpStatusCode.SeeOther
                or HttpStatusCode.TemporaryRedirect or HttpStatusCode.PermanentRedirect)
            {
                var next = response.Headers.Location;
                if (redirects >= MaxRedirects || next is null) Invalid();
                location = next.IsAbsoluteUri ? next : new Uri(location, next);
                if (!AssetHost(location) || location.Host.Equals("github.com", StringComparison.OrdinalIgnoreCase)
                    && !location.AbsoluteUri.Equals(asset.DownloadUrl.AbsoluteUri, StringComparison.Ordinal)) Invalid();
                continue;
            }
            Status(response);
            if (response.Content.Headers.ContentLength is { } length && length != asset.Size
                || response.Content.Headers.ContentEncoding.Count != 0
                || response.Content.Headers.ContentType?.MediaType is "text/html" or "application/xhtml+xml") Invalid();
            await CopyAsync(response.Content, destination, asset, progress, bound.Token).ConfigureAwait(false);
            return;
        }
    }

    private void DedicatedClient()
    {
        if (_http.DefaultRequestHeaders.Any()) throw new InvalidOperationException("A dedicated credential-free HTTP client is required");
    }

    private async Task<HttpResponseMessage> SendHeadersAsync(HttpRequestMessage request, CancellationToken cancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(TimeSpan.FromSeconds(10));
        return await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, bound.Token).ConfigureAwait(false);
    }

    private static async Task CopyAsync(HttpContent content, Stream destination, GitHubReleaseAsset asset, IProgress<long>? progress, CancellationToken cancellation)
    {
        await using var input = await content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        using var digest = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        var buffer = new byte[64 * 1024]; long total = 0;
        progress?.Report(0);
        while (true)
        {
            cancellation.ThrowIfCancellationRequested();
            using var readBound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            readBound.CancelAfter(TimeSpan.FromSeconds(10));
            var count = await input.ReadAsync(buffer, readBound.Token).ConfigureAwait(false);
            cancellation.ThrowIfCancellationRequested();
            if (count == 0) break;
            if (total > asset.Size - count) Invalid();
            await destination.WriteAsync(buffer.AsMemory(0, count), cancellation).ConfigureAwait(false);
            digest.AppendData(buffer, 0, count); total += count; progress?.Report(total);
        }
        if (total != asset.Size || asset.Sha256 is { } expected && Convert.ToHexStringLower(digest.GetHashAndReset()) != expected) Invalid();
        await destination.FlushAsync(cancellation).ConfigureAwait(false);
    }

    private static HttpRequestMessage Request(Uri location, bool metadata)
    {
        var request = new HttpRequestMessage(HttpMethod.Get, location);
        request.Headers.UserAgent.ParseAdd("InfinityHorizon-Updater/1.0");
        request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue(metadata ? "application/vnd.github+json" : "application/octet-stream"));
        request.Headers.TryAddWithoutValidation("X-GitHub-Api-Version", ApiVersion);
        request.Headers.CacheControl = new CacheControlHeaderValue { NoCache = true, NoStore = true };
        return request;
    }

    private static void CheckFinalUrl(HttpResponseMessage response, Uri expected)
    {
        if (response.RequestMessage?.RequestUri is { } actual && actual.AbsoluteUri != expected.AbsoluteUri) Invalid();
    }
    private static void Status(HttpResponseMessage response)
    {
        if (response.StatusCode != HttpStatusCode.OK)
            throw new HttpRequestException("GitHub release request failed", null, response.StatusCode);
    }

    private static async Task<byte[]> ReadMetadataAsync(HttpContent content, CancellationToken cancellation)
    {
        if (content.Headers.ContentLength > MaxMetadataBytes || content.Headers.ContentEncoding.Count != 0) Invalid();
        await using var input = await content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        using var output = new MemoryStream(); var buffer = new byte[8192];
        int count;
        while ((count = await input.ReadAsync(buffer, cancellation).ConfigureAwait(false)) != 0)
        {
            if (output.Length + count > MaxMetadataBytes) Invalid();
            output.Write(buffer, 0, count);
        }
        return output.ToArray();
    }

    private static GitHubRelease Parse(byte[] bytes)
    {
        try
        {
            _ = Utf8.GetCharCount(bytes);
            var reader = new Utf8JsonReader(bytes, new JsonReaderOptions { MaxDepth = 32 });
            var names = new Stack<HashSet<string>>();
            while (reader.Read())
            {
                if (reader.TokenType == JsonTokenType.StartObject) names.Push(new(StringComparer.Ordinal));
                else if (reader.TokenType == JsonTokenType.EndObject) names.Pop();
                else if (reader.TokenType == JsonTokenType.PropertyName && !names.Peek().Add(reader.GetString()!)) Invalid();
            }
            using var document = JsonDocument.Parse(bytes, new JsonDocumentOptions { MaxDepth = 32 });
            var root = document.RootElement;
            var tag = Text(root, "tag_name", LauncherVersion.MaxLength + 1);
            if (!TagVersion(tag, out var version) || root.GetProperty("draft").ValueKind != JsonValueKind.False
                || root.GetProperty("prerelease").ValueKind != JsonValueKind.False) Invalid();
            var id = Positive(root, "id");
            var page = new Uri(Text(root, "html_url", 2048), UriKind.Absolute);
            if (!RepositoryUrl(page, "releases/tag", tag) || !RepositoryApiUrl(new(Text(root, "url", 2048)), "releases/" + id.ToString(System.Globalization.CultureInfo.InvariantCulture))) Invalid();
            var assets = root.GetProperty("assets");
            if (assets.ValueKind != JsonValueKind.Array || assets.GetArrayLength() > MaxAssets) Invalid();
            var result = ImmutableArray.CreateBuilder<GitHubReleaseAsset>();
            var assetNames = new HashSet<string>(StringComparer.OrdinalIgnoreCase); var ids = new HashSet<long>();
            foreach (var item in assets.EnumerateArray())
            {
                var assetId = Positive(item, "id"); var name = Text(item, "name", 128);
                long size = 0;
                if (!AssetName(name) || !assetNames.Add(name) || !ids.Add(assetId) || item.GetProperty("state").GetString() != "uploaded"
                    || !item.GetProperty("size").TryGetInt64(out size) || size < 0 || size > MaxAssetBytes) Invalid();
                var url = new Uri(Text(item, "browser_download_url", 2048), UriKind.Absolute);
                if (!DownloadUrl(url, name, tag) || !RepositoryApiUrl(new(Text(item, "url", 2048)), "releases/assets/" + assetId.ToString(System.Globalization.CultureInfo.InvariantCulture))) Invalid();
                string? sha = null;
                if (item.TryGetProperty("digest", out var declared) && declared.ValueKind != JsonValueKind.Null)
                {
                    var value = declared.GetString();
                    if (value is not { Length: 71 } || !value.StartsWith("sha256:", StringComparison.Ordinal) || !Digest(value[7..])) Invalid();
                    sha = value[7..];
                }
                result.Add(new(assetId, name, size, url, sha));
            }
            return new(version, tag, page, result.ToImmutable());
        }
        catch (Exception error) when (error is JsonException or ArgumentException or InvalidOperationException or KeyNotFoundException or FormatException)
        { throw new InvalidDataException("Invalid GitHub release metadata"); }
    }

    private static string Text(JsonElement value, string property, int limit)
    {
        var text = value.GetProperty(property).GetString();
        if (text is not { Length: > 0 } || text.Length > limit) Invalid();
        return text;
    }
    private static long Positive(JsonElement value, string property)
    { if (!value.GetProperty(property).TryGetInt64(out var result) || result <= 0) Invalid(); return result; }
    private static bool TagVersion(string tag, [System.Diagnostics.CodeAnalysis.NotNullWhen(true)] out LauncherVersion? version) =>
        LauncherVersion.TryParse(tag.StartsWith('v') ? tag[1..] : tag, out version);
    private static bool AssetName(string? name) => name is { Length: > 0 and <= 128 } && char.IsAsciiLetterOrDigit(name[0])
        && name.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' or '_' or '+') && !name.EndsWith('.')
        && Integrity.CriticalFileReader.RelativePath(name);
    private static bool Digest(string? value) => value is null || value.Length == 64 && value.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    private static bool Https(Uri uri) => uri.IsAbsoluteUri && uri.Scheme == Uri.UriSchemeHttps && uri.IsDefaultPort
        && uri.UserInfo.Length == 0 && uri.Fragment.Length == 0 && uri.AbsoluteUri.Length <= 8192 && !uri.OriginalString.Contains('\\');
    private static bool AssetHost(Uri uri) => Https(uri) && uri.Host is "github.com" or "release-assets.githubusercontent.com" or "objects.githubusercontent.com" or "github-releases.githubusercontent.com";
    private static bool RepositoryApiUrl(Uri uri, string suffix) => Https(uri) && uri.Host == "api.github.com" && uri.Query.Length == 0
        && uri.AbsolutePath == "/repos/" + Repository + "/" + suffix;
    private static bool RepositoryUrl(Uri uri, string route, string tag, string? name = null) => Https(uri) && uri.Host == "github.com" && uri.Query.Length == 0
        && uri.AbsolutePath.Split('/').Select(Uri.UnescapeDataString).SequenceEqual(("/" + Repository + "/" + route + "/" + tag + (name is null ? "" : "/" + name)).Split('/'), StringComparer.Ordinal);
    private static bool DownloadUrl(Uri uri, string name, string? expectedTag = null)
    {
        if (!Https(uri) || uri.Host != "github.com" || uri.Query.Length != 0) return false;
        var segments = uri.AbsolutePath.Split('/');
        if (segments.Length != 7) return false;
        var tag = Uri.UnescapeDataString(segments[5]);
        return TagVersion(tag, out _) && (expectedTag is null || tag == expectedTag) && RepositoryUrl(uri, "releases/download", tag, name);
    }

    [System.Diagnostics.CodeAnalysis.DoesNotReturn]
    private static void Invalid() => throw new InvalidDataException("Invalid GitHub release metadata or download");
    public void Dispose() { if (_ownsHttp) _http.Dispose(); }
}
