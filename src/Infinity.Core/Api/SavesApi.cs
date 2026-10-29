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
/// <paramref name="SeedSource"/> is the source profile's decimal XUID of a <c>seed_from_profile</c> commit (<c>X-Fi-Seed-Source</c>,
/// CR-S52), null otherwise.
/// </summary>
public sealed record SaveCommitRequest(
    ulong Expected,
    string CommitId,
    string SessionId,
    ulong FenceToken,
    string Kind,
    string ContentSha256,
    byte[] Bundle,
    string? SeedSource = null);

/// <summary>The cloud save routes, behind an interface so the save logic tests with a fake.</summary>
public interface ISaveApi
{
    Task<SaveApiResult<SaveHead>> HeadAsync(string access, string xuid, CancellationToken cancellation = default);

    Task<SaveApiResult<RevisionPage>> RevisionsAsync(string access, string xuid, ulong? before, uint limit, CancellationToken cancellation = default);

    Task<SaveApiResult<SaveBundleDownload>> BundleAsync(string access, string xuid, ulong revision, CancellationToken cancellation = default);

    Task<SaveApiResult<CommitResponse>> CommitAsync(string access, string xuid, SaveCommitRequest commit, CancellationToken cancellation = default);
}

/// <summary>What an upload dry run decided (OD-133, CR-S71).</summary>
public enum UploadDryRunVerdict
{
    /// <summary>200 <c>pass: true</c>: the commit would be taken now.</summary>
    Pass,

    /// <summary>200 <c>pass: false</c>: the commit would be refused for this career (<c>SAVES.GATE_REFUSED</c> and its rule, unreadable, invalid).</summary>
    Refused,

    /// <summary>A server before CR-S71 (404 without the contract's body, 405): there is no dry run there.</summary>
    Unsupported,

    /// <summary>No verdict: the check could not run (429 with <c>Retry-After</c>, 412 the head moved, 403, 503…); try again later.</summary>
    Error,
}

/// <summary>
/// OD-133: the server's answer to an upload dry run (SAVES_API.md §4.11): its verdict, the code the commit would answer, the gate's
/// <see cref="Rule"/> and reason codes, the commit's message, <c>Retry-After</c> and, on a 412, the head the server holds.
/// </summary>
public sealed record UploadDryRunResult(UploadDryRunVerdict Verdict, string? Code, string? Rule, IReadOnlyList<string> Reasons, string? Message = null,
    TimeSpan? RetryAfter = null, ulong? Head = null)
{
    public bool Passes => Verdict == UploadDryRunVerdict.Pass;

    public static UploadDryRunResult Pass() => new(UploadDryRunVerdict.Pass, null, null, []);

    public static UploadDryRunResult Refused(string code, string? rule, params string[] reasons) => new(UploadDryRunVerdict.Refused, code, rule, reasons);

    public static UploadDryRunResult Unsupported() => new(UploadDryRunVerdict.Unsupported, null, null, []);

    public static UploadDryRunResult Error(string code, TimeSpan? retryAfter = null, ulong? head = null) =>
        new(UploadDryRunVerdict.Error, code, null, [], RetryAfter: retryAfter, Head: head);

    /// <summary>The answer of <c>POST .../save/upload-check</c> read as a verdict (a 2xx body, or the commit's error body).</summary>
    public static UploadDryRunResult From(SaveApiResult<UploadCheck> answer)
    {
        ArgumentNullException.ThrowIfNull(answer);
        if (answer is { Ok: true, Value: { } check })
        {
            return check.Pass ? Pass()
                : new UploadDryRunResult(UploadDryRunVerdict.Refused, check.Code ?? ErrorCodes.SavesGateRefused, check.Rule, check.Reasons ?? [], check.Reason);
        }
        var error = answer.Error!;
        // an older server has no such route: 405, or a 404 that is not the contract's SAVES.PROFILE_NOT_FOUND
        if (answer.Status == HttpStatusCode.MethodNotAllowed || (answer.Status == HttpStatusCode.NotFound && error.Code != ErrorCodes.SavesProfileNotFound))
        {
            return Unsupported();
        }
        ulong? head = error.Details is { ValueKind: JsonValueKind.Object } d && d.TryGetProperty("head", out var h) && h.TryGetUInt64(out var n) ? n : null;
        return Error(error.Code, answer.RetryAfter, head);
    }
}

/// <summary>
/// OD-133: "would this upload pass?" asked before a career restore changes anything on this PC (SAVES_API.md §4.11, CR-S71). A separate
/// seam from <see cref="ISaveApi"/> so the save logic's fakes stay as they are. It takes exactly the <see cref="SaveCommitRequest"/> the
/// real <see cref="ISaveApi.CommitAsync"/> would send (built by the same <c>CloudSaves.CommitRequest</c>, without the lease), so the
/// check and the commit cannot diverge.
/// </summary>
public interface ISaveUploadDryRun
{
    bool SupportsUploadDryRun { get; }

    Task<UploadDryRunResult> DryRunUploadAsync(string access, string xuid, SaveCommitRequest commit, CancellationToken cancellation = default);
}

/// <summary>
/// HTTP client of SAVES_API.md §4's <c>/launcher/v1/profiles/{xuid}/save*</c> routes, served by the
/// saves deployable (<c>/launcher/v1/info</c> <c>services.saves</c>; fi-protocol <c>routes::launcher::SAVE*</c>).
/// Same origin rules and <c>X-Fi-Client</c> as <see cref="ApiClient"/>; holds no tokens.
/// </summary>
public sealed class SavesApiClient : ISaveApi, ISaveUploadDryRun
{
    /// <summary>CR-S71: the client speaks the upload dry run; a server without it answers 404/405 (<see cref="UploadDryRunVerdict.Unsupported"/>).</summary>
    public bool SupportsUploadDryRun => true;

    /// <summary>
    /// <c>POST .../save/upload-check</c> (SAVES_API.md §4.11, CR-S71; fi-protocol <c>routes::launcher::SAVE_UPLOAD_CHECK</c>): the
    /// commit's own headers and body (<see cref="CommitHeaders"/>: <c>If-Match</c>, <c>X-Fi-Commit-Kind</c>, <c>Repr-Digest</c>,
    /// <c>X-Fi-Content-Sha256</c>, the bundle as <c>application/x-fh2save1</c>) without the lease or <c>Idempotency-Key</c>. Nothing is
    /// stored; at most 10 an hour per account (429 <c>SAVES.RATE_LIMITED</c>, <c>Retry-After</c>).
    /// </summary>
    public async Task<UploadDryRunResult> DryRunUploadAsync(string access, string xuid, SaveCommitRequest commit, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(commit);
        using var request = Request(HttpMethod.Post, Base(xuid) + "/upload-check", access);
        CommitHeaders(request, commit, withLease: false);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return UploadDryRunResult.From(await Json<UploadCheck>(response, cancellation).ConfigureAwait(false));
    }

    /// <summary>
    /// The headers and body of a commit (<c>PUT .../save</c>, §4.3) and of its dry run (§4.11), set in one place. <paramref name="withLease"/>
    /// false (the dry run) leaves out <c>Idempotency-Key</c>, <c>X-Fi-Session-Id</c>, <c>X-Fi-Fence</c> and <c>X-Fi-Seed-Source</c>.
    /// </summary>
    private static void CommitHeaders(HttpRequestMessage request, SaveCommitRequest commit, bool withLease)
    {
        request.Headers.TryAddWithoutValidation("If-Match", string.Create(CultureInfo.InvariantCulture, $"\"{commit.Expected}\""));
        if (withLease)
        {
            request.Headers.TryAddWithoutValidation("Idempotency-Key", commit.CommitId);
            request.Headers.TryAddWithoutValidation("X-Fi-Session-Id", commit.SessionId);
            request.Headers.TryAddWithoutValidation("X-Fi-Fence", commit.FenceToken.ToString(CultureInfo.InvariantCulture));
        }
        request.Headers.TryAddWithoutValidation("X-Fi-Commit-Kind", commit.Kind);
        request.Headers.TryAddWithoutValidation("Repr-Digest", ReprDigest(commit.Bundle));
        request.Headers.TryAddWithoutValidation("X-Fi-Content-Sha256", commit.ContentSha256);
        if (withLease && commit.SeedSource is { } source)
        {
            request.Headers.TryAddWithoutValidation(SeedSourceHeader, source);
        }
        request.Content = new ByteArrayContent(commit.Bundle);
        request.Content.Headers.ContentType = new MediaTypeHeaderValue(Infinity.Core.Saves.Fh2Bundle.MediaType);
    }
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
        CommitHeaders(request, commit, withLease: true);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<CommitResponse>(response, cancellation).ConfigureAwait(false);
    }

    /// <summary>fi-protocol <c>headers::FI_SEED_SOURCE</c> (CR-S52): the source profile of a seed check or a <c>seed_from_profile</c> commit.</summary>
    public const string SeedSourceHeader = "X-Fi-Seed-Source";

    /// <summary>
    /// <c>POST .../save/check</c> (SAVES_API.md §4.10, CR-S52): the dry run of <paramref name="sourceXuid"/>'s save
    /// (<paramref name="bundle"/>, its newest sealed revision on this PC) as the start of the Hardcore line of
    /// <paramref name="targetXuid"/>, which must exist on the server already. No <c>Idempotency-Key</c>, no lease; nothing is
    /// committed, but every check counts against the account's 5 a day (429 <c>SAVES.RATE_LIMITED</c>, <c>Retry-After</c>).
    /// </summary>
    public async Task<SaveApiResult<SeedCheck>> CheckSeedAsync(string access, string targetXuid, string sourceXuid, byte[] bundle, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(bundle);
        using var request = Request(HttpMethod.Post, Base(targetXuid) + "/check", access);
        request.Headers.TryAddWithoutValidation(SeedSourceHeader, sourceXuid);
        request.Content = new ByteArrayContent(bundle);
        request.Content.Headers.ContentType = new MediaTypeHeaderValue(Infinity.Core.Saves.Fh2Bundle.MediaType);
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Json<SeedCheck>(response, cancellation).ConfigureAwait(false);
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
