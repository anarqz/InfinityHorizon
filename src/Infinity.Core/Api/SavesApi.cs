// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>
/// One answer of the save routes: the body on 2xx, else the contract's error body (or
/// <c>HTTP.&lt;status&gt;</c>). <see cref="RetryAfter"/> from <c>Retry-After</c>, <see cref="Replayed"/>
/// from <c>X-Fi-Replayed: true</c>. Network failures are not answers: they throw
/// (<see cref="HttpRequestException"/>, a timeout), and the caller treats them as transient.
/// </summary>
public sealed record SaveApiResult<T>(HttpStatusCode Status, T? Value, ErrorBody? Error, TimeSpan? RetryAfter, bool Replayed)
{
    public bool Ok => Error is null;
}

/// <summary><c>GET .../save/revisions/{revision}/bundle</c>: the bytes and what the server declared about them.</summary>
public sealed record SaveBundleDownload(byte[] Bytes, string? BundleSha256, string? ContentSha256, ulong? Revision);

/// <summary>
/// <c>PUT .../save</c> (SAVES_API.md §4.3): <paramref name="Expected"/> becomes <c>If-Match: "&lt;n&gt;"</c>
/// (<c>"0"</c> on an empty line), <paramref name="CommitId"/> the <c>Idempotency-Key</c> every retry reuses,
/// <paramref name="SessionId"/>/<paramref name="FenceToken"/> the lease (<c>X-Fi-Session-Id</c>, <c>X-Fi-Fence</c>).
/// </summary>
public sealed record SaveCommitRequest(
    ulong Expected,
    string CommitId,
    string SessionId,
    ulong FenceToken,
    string Kind,
    string ContentSha256,
    byte[] Bundle);

/// <summary>The cloud save routes, behind an interface so the save logic tests with a fake.</summary>
public interface ISaveApi
{
    Task<SaveApiResult<SaveHead>> HeadAsync(string access, string xuid, CancellationToken cancellation = default);

    Task<SaveApiResult<RevisionPage>> RevisionsAsync(string access, string xuid, ulong? before, uint limit, CancellationToken cancellation = default);

    Task<SaveApiResult<SaveBundleDownload>> BundleAsync(string access, string xuid, ulong revision, CancellationToken cancellation = default);

    Task<SaveApiResult<CommitResponse>> CommitAsync(string access, string xuid, SaveCommitRequest commit, CancellationToken cancellation = default);
}

/// <summary>
/// HTTP client of SAVES_API.md §4's <c>/launcher/v1/profiles/{xuid}/save*</c> routes, served by the
/// saves deployable (<c>/launcher/v1/info</c> <c>services.saves</c>; fi-protocol <c>routes::launcher::SAVE*</c>).
/// Same origin rules and <c>X-Fi-Client</c> as <see cref="ApiClient"/>; holds no tokens.
/// </summary>
public sealed class SavesApiClient : ISaveApi
{
    private readonly HttpClient _http;
    private readonly string _client;

    public SavesApiClient(HttpClient http, Uri saves, string clientHeader)
    {
        _http = http ?? throw new ArgumentNullException(nameof(http));
        Saves = ApiClient.CheckOrigin(saves);
        _client = clientHeader;
    }

    public Uri Saves { get; }

    private static string Base(string xuid) => $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}/save";

    private HttpRequestMessage Request(HttpMethod method, string path, string access)
    {
        var request = new HttpRequestMessage(method, new Uri(Saves, path));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", _client);
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", access);
        return request;
    }

    public async Task<SaveApiResult<SaveHead>> HeadAsync(string access, string xuid, CancellationToken cancellation = default)
    {
        using var request = Request(HttpMethod.Get, Base(xuid), access);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<SaveHead>(response, cancellation).ConfigureAwait(false);
    }

    public async Task<SaveApiResult<RevisionPage>> RevisionsAsync(string access, string xuid, ulong? before, uint limit, CancellationToken cancellation = default)
    {
        var query = string.Create(CultureInfo.InvariantCulture, $"?limit={limit}") + (before is { } b ? string.Create(CultureInfo.InvariantCulture, $"&before={b}") : "");
        using var request = Request(HttpMethod.Get, Base(xuid) + "/revisions" + query, access);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<RevisionPage>(response, cancellation).ConfigureAwait(false);
    }

    public async Task<SaveApiResult<SaveBundleDownload>> BundleAsync(string access, string xuid, ulong revision, CancellationToken cancellation = default)
    {
        using var request = Request(HttpMethod.Get, Base(xuid) + string.Create(CultureInfo.InvariantCulture, $"/revisions/{revision}/bundle"), access);
        using var response = await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
        if (!response.IsSuccessStatusCode)
        {
            return await Failure<SaveBundleDownload>(response, cancellation).ConfigureAwait(false);
        }
        // Read at most one byte past the cap: a larger body is refused by the caller, never buffered whole.
        var stream = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        await using (stream.ConfigureAwait(false))
        {
            using var buffer = new MemoryStream();
            var chunk = new byte[16 << 10];
            int read;
            while (buffer.Length <= Infinity.Core.Saves.Fh2Bundle.MaxBundle && (read = await stream.ReadAsync(chunk, cancellation).ConfigureAwait(false)) > 0)
            {
                buffer.Write(chunk, 0, read);
            }
            var body = new SaveBundleDownload(
                buffer.ToArray(),
                ParseReprDigest(Header(response, "Repr-Digest")),
                Header(response, "X-Fi-Content-Sha256")?.Trim().ToLowerInvariant(),
                ParseEtag(response.Headers.ETag?.Tag ?? Header(response, "ETag")));
            return new SaveApiResult<SaveBundleDownload>(response.StatusCode, body, null, RetryAfter(response), false);
        }
    }

    public async Task<SaveApiResult<CommitResponse>> CommitAsync(string access, string xuid, SaveCommitRequest commit, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(commit);
        using var request = Request(HttpMethod.Put, Base(xuid), access);
        request.Headers.TryAddWithoutValidation("If-Match", string.Create(CultureInfo.InvariantCulture, $"\"{commit.Expected}\""));
        request.Headers.TryAddWithoutValidation("Idempotency-Key", commit.CommitId);
        request.Headers.TryAddWithoutValidation("X-Fi-Session-Id", commit.SessionId);
        request.Headers.TryAddWithoutValidation("X-Fi-Fence", commit.FenceToken.ToString(CultureInfo.InvariantCulture));
        request.Headers.TryAddWithoutValidation("X-Fi-Commit-Kind", commit.Kind);
        request.Headers.TryAddWithoutValidation("Repr-Digest", ReprDigest(commit.Bundle));
        request.Headers.TryAddWithoutValidation("X-Fi-Content-Sha256", commit.ContentSha256);
        request.Content = new ByteArrayContent(commit.Bundle);
        request.Content.Headers.ContentType = new MediaTypeHeaderValue(Infinity.Core.Saves.Fh2Bundle.MediaType);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<CommitResponse>(response, cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// <c>POST .../save/seed</c> (SAVES_API.md §4.8, OD-60): a Hardcore line at revision 0 gets the
    /// server's clean snapshot as revision 1 (201; 200 when already seeded). <paramref name="idempotencyKey"/>
    /// is one per seeding, reused by its retries; the session's id and fence go together or not at all.
    /// </summary>
    public async Task<SaveApiResult<SaveHead>> SeedAsync(string access, string xuid, string idempotencyKey, string? sessionId, ulong? fence, CancellationToken cancellation = default)
    {
        using var request = Request(HttpMethod.Post, Base(xuid) + "/seed", access);
        request.Headers.TryAddWithoutValidation("Idempotency-Key", idempotencyKey);
        if (sessionId is not null && fence is { } f)
        {
            request.Headers.TryAddWithoutValidation("X-Fi-Session-Id", sessionId);
            request.Headers.TryAddWithoutValidation("X-Fi-Fence", f.ToString(CultureInfo.InvariantCulture));
        }
        request.Content = new StringContent("{}", System.Text.Encoding.UTF8, "application/json");
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<SaveHead>(response, cancellation).ConfigureAwait(false);
    }
    /// <summary>RFC 9530 <c>Repr-Digest</c> of a body: <c>sha-256=:&lt;base64&gt;:</c>.</summary>
    public static string ReprDigest(ReadOnlySpan<byte> body) =>
        "sha-256=:" + Convert.ToBase64String(System.Security.Cryptography.SHA256.HashData(body)) + ":";

    /// <summary>The SHA-256 (lower hex) a <c>Repr-Digest</c> carries; other algorithms are ignored (RFC 9530).</summary>
    public static string? ParseReprDigest(string? value)
    {
        if (value is null)
        {
            return null;
        }
        foreach (var item in value.Split(',').Select(i => i.Trim()))
        {
            if (item.StartsWith("sha-256=:", StringComparison.Ordinal) && item.EndsWith(':') && item.Length > 10)
            {
                try
                {
                    var bytes = Convert.FromBase64String(item[9..^1]);
                    if (bytes.Length == 32)
                    {
                        return Convert.ToHexStringLower(bytes);
                    }
                }
                catch (FormatException)
                {
                    return null;
                }
            }
        }
        return null;
    }

    private static ulong? ParseEtag(string? tag)
    {
        var text = tag?.Trim();
        if (text is null)
        {
            return null;
        }
        if (text.StartsWith("W/", StringComparison.Ordinal))
        {
            text = text[2..];
        }
        text = text.Trim('"');
        return ulong.TryParse(text, NumberStyles.None, CultureInfo.InvariantCulture, out var n) ? n : null;
    }

    private static string? Header(HttpResponseMessage response, string name) =>
        response.Headers.TryGetValues(name, out var values) || response.Content.Headers.TryGetValues(name, out values)
            ? values.FirstOrDefault()
            : null;

    private static TimeSpan? RetryAfter(HttpResponseMessage response) => response.Headers.RetryAfter switch
    {
        { Delta: { } delta } => delta,
        { Date: { } date } => date - DateTimeOffset.UtcNow is var d && d > TimeSpan.Zero ? d : TimeSpan.Zero,
        _ => null,
    };

    private static async Task<SaveApiResult<T>> Json<T>(HttpResponseMessage response, CancellationToken cancellation)
    {
        if (!response.IsSuccessStatusCode)
        {
            return await Failure<T>(response, cancellation).ConfigureAwait(false);
        }
        var replayed = Header(response, "X-Fi-Replayed") is { } r && r.Trim().Equals("true", StringComparison.OrdinalIgnoreCase);
        try
        {
            var value = await response.Content.ReadFromJsonAsync<T>(ApiClient.Json, cancellation).ConfigureAwait(false);
            return value is null
                ? new SaveApiResult<T>(response.StatusCode, default, new ErrorBody("HTTP.EMPTY", "empty body", null), null, false)
                : new SaveApiResult<T>(response.StatusCode, value, null, RetryAfter(response), replayed);
        }
        catch (JsonException e)
        {
            return new SaveApiResult<T>(response.StatusCode, default, new ErrorBody("HTTP.BAD_BODY", e.Message, null), null, false);
        }
    }

    private static async Task<SaveApiResult<T>> Failure<T>(HttpResponseMessage response, CancellationToken cancellation)
    {
        ErrorBody? error = null;
        try
        {
            error = await response.Content.ReadFromJsonAsync<ErrorBody>(ApiClient.Json, cancellation).ConfigureAwait(false);
        }
        catch (JsonException)
        {
            // Not the contract's body (a proxy, an HTML page): keep the status.
        }
        error ??= new ErrorBody($"HTTP.{(int)response.StatusCode}", response.ReasonPhrase ?? string.Empty, null);
        return new SaveApiResult<T>(response.StatusCode, default, error, RetryAfter(response), false);
    }
}
