// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Infinity.Core.Api;

// Exact OD-79 mirror of fi-protocol::content / CONTENT_API.md, commit b788a7d.
public sealed record ContentCriticalFile(string FileId, string Path, ulong ByteLength);
public sealed record ContentManifest(string ManifestId, string Game, string GameBuild, IReadOnlyList<ContentCriticalFile> Files);
public sealed record ContentCapabilities(byte SchemaVersion, string Game, string Enforcement, ContentManifest? Manifest, byte MaxFiles, uint MaxScanBytes);
public sealed record ContentObservedFile(string FileId, ulong? ByteLength, string? Sha256);
public sealed record ContentScan(string ScanId, string SessionId, string Game, string ManifestId, IReadOnlyList<ContentObservedFile> Files);
public sealed record ContentFileResult(string FileId, string State);
[System.Diagnostics.CodeAnalysis.SuppressMessage("Naming", "CA1711:Identifiers should not have incorrect suffix", Justification = "Exact fi-protocol content contract name")]
public sealed record ContentPermission(string SessionId, string? ScanId, string State, bool OnlineAllowed, string Reason,
    IReadOnlyList<ContentFileResult> Files, string? ExpiresAt);
public sealed record ContentScanAccepted(string ScanId, ContentPermission Permission);

public static class ContentJson
{
    public static readonly JsonSerializerOptions Strict = new(JsonSerializerDefaults.Web)
    {
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Skip, // Responses are additive; requests have fixed DTOs.
        RespectRequiredConstructorParameters = true,
        PropertyNameCaseInsensitive = false,
        NumberHandling = JsonNumberHandling.Strict,
        MaxDepth = 32,
    };

    public static T Read<T>(ReadOnlySpan<byte> bytes)
    {
        // System.Text.Json otherwise accepts duplicate names, including escaped aliases.
        var reader = new Utf8JsonReader(bytes, new JsonReaderOptions { MaxDepth = 32 });
        var names = new Stack<HashSet<string>>();
        while (reader.Read())
        {
            if (reader.TokenType == JsonTokenType.StartObject) names.Push(new(StringComparer.Ordinal));
            else if (reader.TokenType == JsonTokenType.EndObject) names.Pop();
            else if (reader.TokenType == JsonTokenType.PropertyName && !names.Peek().Add(reader.GetString()!))
                throw new JsonException("Duplicate content field");
        }
        return JsonSerializer.Deserialize<T>(bytes, Strict) ?? throw new JsonException("Missing content response");
    }
}

public sealed partial class ApiClient
{
    public Task<ContentCapabilities> ContentCapabilitiesAsync(string access, string game, CancellationToken cancellation = default) =>
        ContentRequestAsync<ContentCapabilities>(HttpMethod.Get, $"/gate/v1/content/capabilities?game={Uri.EscapeDataString(game)}", null, access, null, 8192, HttpStatusCode.OK, cancellation);

    public Task<ContentScanAccepted> SubmitContentScanAsync(string access, ContentScan scan, CancellationToken cancellation = default) =>
        SubmitContentScanObservedAsync(access, scan, null, cancellation);

    internal Task<ContentScanAccepted> SubmitContentScanObservedAsync(string access, ContentScan scan,
        Action<DateTimeOffset?>? observeResponseDate, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(scan);
        ContentId(scan.ScanId, version7: true);
        ContentId(scan.SessionId);
        ContentId(scan.ManifestId);
        return ContentRequestAsync<ContentScanAccepted>(HttpMethod.Post, "/gate/v1/content/scans", scan, access, scan.ScanId, 8192, HttpStatusCode.OK, cancellation, observeResponseDate);
    }

    public Task<ContentPermission> ContentPermissionAsync(string access, string sessionId, CancellationToken cancellation = default)
    {
        ContentId(sessionId);
        return ContentRequestAsync<ContentPermission>(HttpMethod.Get, $"/gate/v1/sessions/{sessionId}/content", null, access, null, 8192, HttpStatusCode.OK, cancellation);
    }

    private static void ContentId(string value, bool version7 = false)
    {
        if (!Guid.TryParseExact(value, "D", out var id) || id == Guid.Empty
            || (version7 && (id.Version != 7 || (value[19] | 0x20) is not ('8' or '9' or 'a' or 'b'))))
            throw new InvalidDataException("Invalid content action id");
    }

    private async Task<T> ContentRequestAsync<T>(HttpMethod method, string path, object? body, string access, string? actionId,
        int limit, HttpStatusCode expected, CancellationToken cancellation, Action<DateTimeOffset?>? observeResponseDate = null)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(TimeSpan.FromSeconds(10));
        using var request = new HttpRequestMessage(method, new Uri(Gate, path));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", _client);
        request.Headers.CacheControl = new CacheControlHeaderValue { NoStore = true };
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", access);
        if (actionId is not null) request.Headers.TryAddWithoutValidation("Idempotency-Key", actionId);
        if (body is not null)
        {
            var bytes = JsonSerializer.SerializeToUtf8Bytes(body, body.GetType(), ContentJson.Strict);
            if (bytes.Length > limit) throw new InvalidDataException("Content request too large");
            request.Content = new ByteArrayContent(bytes);
            request.Content.Headers.ContentType = new MediaTypeHeaderValue("application/json");
        }
        using var response = await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, bound.Token).ConfigureAwait(false);
        ObserveDate(response.Headers.Date, DateTimeOffset.UtcNow);
        observeResponseDate?.Invoke(response.Headers.Date);
        if (response.Content.Headers.ContentLength > limit) throw new InvalidDataException("Content response too large");
        await using var input = await response.Content.ReadAsStreamAsync(bound.Token).ConfigureAwait(false);
        using var output = new MemoryStream();
        var buffer = new byte[8192];
        int read;
        while ((read = await input.ReadAsync(buffer, bound.Token).ConfigureAwait(false)) > 0)
        {
            if (output.Length + read > limit) throw new InvalidDataException("Content response too large");
            output.Write(buffer, 0, read);
        }
        var content = output.ToArray();
        if (!response.IsSuccessStatusCode)
        {
            response.Content = new ByteArrayContent(content);
            _ = await Read<JsonElement>(response, bound.Token).ConfigureAwait(false); // Standard bounded error/retry handling.
        }
        if (response.StatusCode != expected) throw new InvalidDataException("Unexpected content status");
        return ContentJson.Read<T>(content);
    }
}
