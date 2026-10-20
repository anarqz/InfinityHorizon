// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>The stable error codes the launcher branches on (fi-protocol <c>ErrorCode</c>; parity-tested against errors.ts).</summary>
public static class ErrorCodes
{
    public const string CrashDumpsDisabled = "CRASH.DUMPS_DISABLED";
    public const string AccountsFrozen = "ACCOUNTS.FROZEN";
    public const string RequestRateLimited = "REQUEST.RATE_LIMITED";
    public const string AuthTokenInvalid = "AUTH.TOKEN_INVALID";
    public const string AuthTokenExpired = "AUTH.TOKEN_EXPIRED";
    public const string AuthRefreshInvalid = "AUTH.REFRESH_INVALID";
    public const string AuthRefreshReused = "AUTH.REFRESH_REUSED";
    public const string AuthForbidden = "AUTH.FORBIDDEN";
    public const string AuthReauthRequired = "AUTH.REAUTH_REQUIRED";
    public const string AccountsNotPendingDeletion = "ACCOUNTS.NOT_PENDING_DELETION";
    public const string AccountsConfirmationMismatch = "ACCOUNTS.CONFIRMATION_MISMATCH";
    public const string AuthIdentityTicketInvalid = "AUTH.IDENTITY_TICKET_INVALID";
    public const string AuthRateLimited = "AUTH.RATE_LIMITED";
    public const string AuthIdTokenInvalid = "AUTH.ID_TOKEN_INVALID";
    public const string AuthEmailUnverified = "AUTH.EMAIL_UNVERIFIED";
    public const string AuthProviderNotAllowed = "AUTH.PROVIDER_NOT_ALLOWED";
    public const string AccountsBanned = "ACCOUNTS.BANNED";
    public const string SharingTooManyDevices = "SHARING.TOO_MANY_DEVICES";
    public const string AccountsNameUnavailable = "ACCOUNTS.NAME_UNAVAILABLE";
    public const string AccountsProfileLimit = "ACCOUNTS.PROFILE_LIMIT";
    public const string AccountsInvalidName = "ACCOUNTS.INVALID_NAME";
    public const string GameUpdateRequired = "GAME.UPDATE_REQUIRED";
    public const string SessionsInUseElsewhere = "SESSIONS.IN_USE_ELSEWHERE";
    public const string SessionsProfileInUse = "SESSIONS.PROFILE_IN_USE";
    public const string SessionsEnded = "SESSIONS.ENDED";
    public const string SharingTooManyRecentDevices = "SHARING.TOO_MANY_RECENT_DEVICES";
    public const string PoolFull = "POOL.FULL";
    public const string ServiceMaintenance = "SERVICE.MAINTENANCE";
    public const string RequestTooLarge = "REQUEST.TOO_LARGE";
    public const string SavesProfileNotFound = "SAVES.PROFILE_NOT_FOUND";
    public const string SavesLeaseHeld = "SAVES.LEASE_HELD";
    public const string SavesLeaseLost = "SAVES.LEASE_LOST";
    public const string SavesLeaseExpired = "SAVES.LEASE_EXPIRED";
    public const string SavesRevisionConflict = "SAVES.REVISION_CONFLICT";
    public const string SavesIdempotencyKeyReused = "SAVES.IDEMPOTENCY_KEY_REUSED";
    public const string SavesRevisionNotFound = "SAVES.REVISION_NOT_FOUND";
    public const string SavesBundleInvalid = "SAVES.BUNDLE_INVALID";
    public const string SavesDigestMismatch = "SAVES.DIGEST_MISMATCH";
    public const string SavesGateRefused = "SAVES.GATE_REFUSED";
    public const string SavesSummaryUnreadable = "SAVES.SUMMARY_UNREADABLE";
    public const string SavesHardcoreNeedsFreshCareer = "SAVES.HARDCORE_NEEDS_FRESH_CAREER";
    public const string SavesInvalidArgument = "SAVES.INVALID_ARGUMENT";
    public const string SavesRateLimited = "SAVES.RATE_LIMITED";
    public const string SavesUnavailable = "SAVES.UNAVAILABLE";

}

/// <summary>A server answer outside 2xx: its status and the contract's error body (or <c>HTTP.&lt;status&gt;</c> when it had none).</summary>
public sealed class ApiException(HttpStatusCode status, string code, string message, JsonElement? details, TimeSpan? retryAfter = null)
    : Exception($"{(int)status} {code}: {message}")
{
    public HttpStatusCode Status { get; } = status;

    public string Code { get; } = code;

    public JsonElement? Details { get; } = details;

    public TimeSpan? RetryAfter { get; } = retryAfter;
}

/// <summary>
/// The launcher's HTTP client for core (<c>/launcher/v1</c>) and the gate (<c>/gate/v1</c>):
/// <c>X-Fi-Client</c> on every request, an <c>Idempotency-Key</c> on every write the contract
/// asks it for, errors as <see cref="ApiException"/>. Plain http only to 127.0.0.1 and private
/// LAN addresses (<see cref="DevNet"/>, OD-34); everything else must be https. It holds no
/// tokens: callers pass the access token (see <c>TokenManager</c>).
/// </summary>
public sealed partial class ApiClient
{
    public static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        AllowOutOfOrderMetadataProperties = true,
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.Never,
    };

    private readonly HttpClient _http;
    private readonly string _client;

    /// <param name="http">Owned by the caller (one per process).</param>
    /// <param name="core">Core's origin (from <c>/launcher/v1/info</c> <c>services.core</c>, or the configured server).</param>
    /// <param name="gate">The gate's origin (<c>services.gate</c>).</param>
    /// <param name="clientHeader"><c>infinity-launcher/&lt;version&gt; (windows; deck=0)</c>.</param>
    public ApiClient(HttpClient http, Uri core, Uri gate, string clientHeader)
    {
        _http = http ?? throw new ArgumentNullException(nameof(http));
        Core = CheckOrigin(core);
        Gate = CheckOrigin(gate);
        _client = clientHeader;
    }

    public Uri Core { get; }

    public Uri Gate { get; }

    /// <summary>
    /// Server clock minus this PC's clock, from the last response's <c>Date</c> header. Expiries
    /// the server sends (<c>accessExpiresAt</c>, ticket <c>expiresAt</c>) are compared on the
    /// server's clock, so a PC whose clock is hours off neither refreshes on every call nor trusts
    /// a dead token.
    /// </summary>
    public TimeSpan ClockOffset { get; private set; }

    /// <summary>Now on the server's clock (to the second, as <c>Date</c> carries it).</summary>
    public DateTimeOffset ServerNow(TimeProvider clock)
    {
        ArgumentNullException.ThrowIfNull(clock);
        return clock.GetUtcNow() + ClockOffset;
    }

    /// <summary>Records the offset of one response's <c>Date</c> header against <paramref name="localNow"/>.</summary>
    public void ObserveDate(DateTimeOffset? serverDate, DateTimeOffset localNow)
    {
        if (serverDate is { } date)
        {
            ClockOffset = date - localNow;
        }
    }

    /// <summary>https anywhere; plain http only on loopback or a private LAN address with a port.</summary>
    public static Uri CheckOrigin(Uri origin)
    {
        ArgumentNullException.ThrowIfNull(origin);
        if (origin.Scheme == Uri.UriSchemeHttps)
        {
            return origin;
        }
        var text = origin.GetLeftPart(UriPartial.Authority);
        if (origin.Scheme == Uri.UriSchemeHttp && DevNet.PlainHttpUrl(text))
        {
            return origin;
        }
        throw new ArgumentException($"{text} is not https, loopback or a private LAN address", nameof(origin));
    }

    /// <summary><c>GET /launcher/v1/info</c> from a server's base URL, before the service map is known.</summary>
    public static async Task<ServerInfo> InfoAsync(HttpClient http, Uri server, string clientHeader, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(http);
        using var request = new HttpRequestMessage(HttpMethod.Get, new Uri(CheckOrigin(server), "/launcher/v1/info"));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", clientHeader);
        using var response = await http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await Read<ServerInfo>(response, cancellation).ConfigureAwait(false);
    }

    /// <summary>The origins of <paramref name="info"/>'s service map, each falling back to <paramref name="server"/> when empty.</summary>
    public static (Uri Core, Uri Gate) Origins(ServerInfo info, Uri server)
    {
        ArgumentNullException.ThrowIfNull(info);
        Uri Pick(string? origin) => string.IsNullOrEmpty(origin) ? server : new Uri(origin);
        return (Pick(info.Services?.Core), Pick(info.Services?.Gate));
    }

    /// <summary>
    /// <c>grantType: id_token</c>. A retry of the same ID token passes the same
    /// <paramref name="idempotencyKey"/>; a newly minted ID token is a new request with a new key.
    /// </summary>
    public Task<SignInResult> ExchangeIdTokenAsync(IdTokenGrant grant, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<SignInResult>(HttpMethod.Post, Core, "/launcher/v1/auth/token", grant, access: null, idempotencyKey, cancellation);

    public Task<LauncherAccountCreated> CreateAccountAsync(LauncherAccountCreate create, CancellationToken cancellation = default) =>
        SendAsync<LauncherAccountCreated>(HttpMethod.Post, Core, "/launcher/v1/accounts", create, access: null, idempotent: true, cancellation);

    public Task<TokenPair> RefreshAsync(Refresh refresh, CancellationToken cancellation = default) =>
        SendAsync<TokenPair>(HttpMethod.Post, Core, "/launcher/v1/token", refresh, access: null, idempotent: true, cancellation);

    public Task<Me> MeAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<Me>(HttpMethod.Get, Core, "/launcher/v1/me", null, access, idempotent: false, cancellation);

    public Task<Deletion> RequestDeletionAsync(string access, LauncherDeletionRequest request, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<Deletion>(HttpMethod.Post, Core, "/launcher/v1/me/deletion", request, access, idempotencyKey, cancellation);

    public async Task CancelDeletionAsync(string access, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Delete, Core, "/launcher/v1/me/deletion", null, access, idempotent: true, cancellation).ConfigureAwait(false);

    public Task<Profile> CreateProfileAsync(string access, NewProfile profile, CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, "/launcher/v1/profiles", profile, access, idempotent: true, cancellation);

    /// <summary><c>POST /gate/v1/sessions</c>: <c>Idempotency-Key</c> is the session id, so a retry is the same admission.</summary>
    public Task<GateSession> CreateSessionAsync(string access, PlaySessionCreate create, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(create);
        return SendAsync<GateSession>(HttpMethod.Post, Gate, "/gate/v1/sessions", create, access, idempotencyKey: create.SessionId, cancellation);
    }

    public Task<GateSession> SessionAsync(string access, string sessionId, CancellationToken cancellation = default) =>
        SendAsync<GateSession>(HttpMethod.Get, Gate, $"/gate/v1/sessions/{Uri.EscapeDataString(sessionId)}", null, access, idempotent: false, cancellation);

    public Task<GateHeartbeatResponse> HeartbeatAsync(string access, string sessionId, GateHeartbeat heartbeat, CancellationToken cancellation = default) =>
        SendAsync<GateHeartbeatResponse>(HttpMethod.Post, Gate, $"/gate/v1/sessions/{Uri.EscapeDataString(sessionId)}/heartbeat", heartbeat, access, idempotent: true, cancellation);

    public Task<GateTicketResponse> TicketAsync(string access, string sessionId, CancellationToken cancellation = default) =>
        SendAsync<GateTicketResponse>(HttpMethod.Post, Gate, $"/gate/v1/sessions/{Uri.EscapeDataString(sessionId)}/ticket", new { }, access, idempotent: true, cancellation);

    public async Task LeaveAsync(string access, string sessionId, GateLeave leave, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Post, Gate, $"/gate/v1/sessions/{Uri.EscapeDataString(sessionId)}/leave", leave, access, idempotent: true, cancellation).ConfigureAwait(false);

    private Task<T> SendAsync<T>(HttpMethod method, Uri origin, string path, object? body, string? access, bool idempotent, CancellationToken cancellation) =>
        SendAsync<T>(method, origin, path, body, access, idempotent ? Guid.CreateVersion7().ToString() : null, cancellation);

    private async Task<T> SendAsync<T>(HttpMethod method, Uri origin, string path, object? body, string? access, string? idempotencyKey, CancellationToken cancellation)
    {
        using var request = new HttpRequestMessage(method, new Uri(origin, path));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", _client);
        if (idempotencyKey is not null)
        {
            request.Headers.TryAddWithoutValidation("Idempotency-Key", idempotencyKey);
        }
        if (access is not null)
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", access);
        }
        if (body is not null)
        {
            request.Content = JsonContent.Create(body, body.GetType(), options: Json);
        }
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        ObserveDate(response.Headers.Date, DateTimeOffset.UtcNow);
        return await Read<T>(response, cancellation).ConfigureAwait(false);
    }

    private static async Task<T> Read<T>(HttpResponseMessage response, CancellationToken cancellation)
    {
        if (!response.IsSuccessStatusCode)
        {
            ErrorBody? error = null;
            try
            {
                error = await response.Content.ReadFromJsonAsync<ErrorBody>(Json, cancellation).ConfigureAwait(false);
            }
            catch (JsonException)
            {
                // Not the contract's body (a proxy, an HTML page): keep the status.
            }
            var retryAfter = response.Headers.RetryAfter switch
            {
                { Delta: { } delta } => delta,
                { Date: { } date } => date - (response.Headers.Date ?? DateTimeOffset.UtcNow),
                _ => (TimeSpan?)null,
            };
            throw new ApiException(response.StatusCode, error?.Code ?? $"HTTP.{(int)response.StatusCode}", error?.Message ?? response.ReasonPhrase ?? string.Empty, error?.Details, retryAfter);
        }
        if (response.StatusCode == HttpStatusCode.NoContent || response.Content.Headers.ContentLength == 0)
        {
            return default!;
        }
        return await response.Content.ReadFromJsonAsync<T>(Json, cancellation).ConfigureAwait(false)
            ?? throw new ApiException(response.StatusCode, "HTTP.EMPTY", "empty body", null);
    }
}
