// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>The stable error codes the launcher branches on (fi-protocol <c>ErrorCode</c>; parity-tested against errors.ts).</summary>
public static class ErrorCodes
{
    public const string ContentPolicyUnavailable = "CONTENT.POLICY_UNAVAILABLE";
    public const string ContentRecheckRequired = "CONTENT.RECHECK_REQUIRED";
    public const string ContentIncompatible = "CONTENT.INCOMPATIBLE";
    public const string ContentInconclusive = "CONTENT.INCONCLUSIVE";
    public const string ContentPolicyChanged = "CONTENT.POLICY_CHANGED";
    public const string CrashDumpsDisabled = "CRASH.DUMPS_DISABLED";
    public const string AccountsFrozen = "ACCOUNTS.FROZEN";
    public const string RequestRateLimited = "REQUEST.RATE_LIMITED";
    public const string AuthTokenInvalid = "AUTH.TOKEN_INVALID";
    public const string AuthTokenExpired = "AUTH.TOKEN_EXPIRED";
    public const string AuthRefreshInvalid = "AUTH.REFRESH_INVALID";
    public const string AuthRefreshReused = "AUTH.REFRESH_REUSED";
    public const string AuthForbidden = "AUTH.FORBIDDEN";
    /// <summary>403 <c>{role}</c>: e.g. a dedicated lobby session without <c>role = admin</c> (OD-109).</summary>
    public const string AuthRoleRequired = "AUTH.ROLE_REQUIRED";
    /// <summary>400: also what a server without dedicated lobby support answers to <c>dedicated</c> (OD-109).</summary>
    public const string RequestInvalid = "REQUEST.INVALID";
    public const string AuthReauthRequired = "AUTH.REAUTH_REQUIRED";
    public const string AccountsNotPendingDeletion = "ACCOUNTS.NOT_PENDING_DELETION";
    public const string AccountsConfirmationMismatch = "ACCOUNTS.CONFIRMATION_MISMATCH";
    public const string AuthIdentityTicketInvalid = "AUTH.IDENTITY_TICKET_INVALID";
    public const string AuthRateLimited = "AUTH.RATE_LIMITED";
    public const string AuthIdTokenInvalid = "AUTH.ID_TOKEN_INVALID";
    public const string AuthEmailUnverified = "AUTH.EMAIL_UNVERIFIED";
    public const string AuthConfirmationCodeInvalid = "AUTH.CONFIRMATION_CODE_INVALID";
    public const string AuthProviderNotAllowed = "AUTH.PROVIDER_NOT_ALLOWED";
    public const string AccountsBanned = "ACCOUNTS.BANNED";
    /// <summary>404: an unknown XUID, another account's or an erased profile (CR-S61: a deleted one too, so a lost 204 reads as done).</summary>
    public const string AccountsNotFound = "ACCOUNTS.NOT_FOUND";
    public const string AccountsDeletionPending = "ACCOUNTS.DELETION_PENDING";
    public const string AccountsRegistrationDisabled = "ACCOUNTS.REGISTRATION_DISABLED";
    /// <summary><c>local</c> sign-in: unknown e-mail and wrong password alike (CR-L38).</summary>
    public const string AuthInvalidCredentials = "AUTH.INVALID_CREDENTIALS";
    /// <summary><c>local</c> sign-up: the address already has a credential on this instance (CR-L38).</summary>
    public const string AccountsEmailTaken = "ACCOUNTS.EMAIL_TAKEN";
    public const string SharingTooManyDevices = "SHARING.TOO_MANY_DEVICES";
    public const string AccountsNameUnavailable = "ACCOUNTS.NAME_UNAVAILABLE";
    public const string AccountsProfileLimit = "ACCOUNTS.PROFILE_LIMIT";
    public const string AccountsInvalidName = "ACCOUNTS.INVALID_NAME";
    /// <summary>The profile has no free rename (<c>renameNotice</c> absent, OD-120).</summary>
    public const string AccountsRenameNotAllowed = "ACCOUNTS.RENAME_NOT_ALLOWED";
    /// <summary>The same <c>Idempotency-Key</c> with another body (CORE_API; services/api launcher/accounts.rs).</summary>
    public const string RequestIdempotencyKeyReused = "REQUEST.IDEMPOTENCY_KEY_REUSED";
    public const string GameUpdateRequired = "GAME.UPDATE_REQUIRED";
    public const string SessionsInUseElsewhere = "SESSIONS.IN_USE_ELSEWHERE";
    public const string SessionsProfileInUse = "SESSIONS.PROFILE_IN_USE";
    public const string SessionsEnded = "SESSIONS.ENDED";
    public const string SharingTooManyRecentDevices = "SHARING.TOO_MANY_RECENT_DEVICES";
    public const string PoolFull = "POOL.FULL";
    public const string ServiceMaintenance = "SERVICE.MAINTENANCE";
    public const string ServiceBudgetClosed = "SERVICE.BUDGET_CLOSED";
    public const string ServiceOnlineClosed = "SERVICE.ONLINE_CLOSED";
    public const string ServiceUnavailable = "SERVICE.UNAVAILABLE";
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
    /// <summary>CR-S58: a revision moderation flagged cannot be restored (<c>details.revision</c>).</summary>
    public const string SavesRestoreFlagged = "SAVES.RESTORE_FLAGGED";
    public const string SavesSummaryUnreadable = "SAVES.SUMMARY_UNREADABLE";
    public const string SavesHardcoreNeedsFreshCareer = "SAVES.HARDCORE_NEEDS_FRESH_CAREER";
    public const string SavesInvalidArgument = "SAVES.INVALID_ARGUMENT";
    public const string SavesRateLimited = "SAVES.RATE_LIMITED";
    public const string SavesUnavailable = "SAVES.UNAVAILABLE";
    /// <summary>403 <c>{tier, mode}</c>: the profile keeps no cloud line on this server (SAVES_API.md §4.7).</summary>
    public const string SavesNotEligible = "SAVES.NOT_ELIGIBLE";

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
    private static readonly JsonSerializerOptions StatusJson = new(JsonSerializerDefaults.Web)
    {
        RespectRequiredConstructorParameters = true,
        PropertyNameCaseInsensitive = false,
        UnmappedMemberHandling = System.Text.Json.Serialization.JsonUnmappedMemberHandling.Disallow,
        MaxDepth = 4,
    };
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
    private readonly object _serverTimeLock=new();
    private DateTimeOffset? _serverTime;
    private long _serverTimeStamp;

    /// <summary>A Date sample from the configured HTTPS origin, advanced only by monotonic elapsed time.</summary>
    public DateTimeOffset? TrustedServerNow()
    {
        lock(_serverTimeLock)return _serverTime is {} time ? time+TimeProvider.System.GetElapsedTime(_serverTimeStamp) : null;
    }
    public TimeProvider TrustedTimeProvider()=>new ServerTime(this);
    private sealed class ServerTime(ApiClient api):TimeProvider
    {
        public override DateTimeOffset GetUtcNow()=>api.TrustedServerNow() ?? throw new InvalidOperationException("Trusted server time unavailable");
    }

    /// <summary>Now on the server's clock (to the second, as <c>Date</c> carries it).</summary>
    public DateTimeOffset ServerNow(TimeProvider clock)
    {
        ArgumentNullException.ThrowIfNull(clock);
        return clock.GetUtcNow() + ClockOffset;
    }

    /// <summary>
    /// A development build talking to a literal loopback or private-LAN plain-http stack: its Date is
    /// the only server clock a LAN test has, so it is sampled too (the relay proof and network hints
    /// compare grant expiries on it). A production build samples HTTPS origins only.
    /// </summary>
#if INFINITY_PRODUCTION_BUILD
    private static bool DevelopmentStack => false;
#else
    private bool DevelopmentStack => Core.Scheme==Uri.UriSchemeHttp && Gate.Scheme==Uri.UriSchemeHttp
        && DevNet.PlainHttpUrl(Core.GetLeftPart(UriPartial.Authority)) && DevNet.PlainHttpUrl(Gate.GetLeftPart(UriPartial.Authority));
#endif

    /// <summary>Records the offset of one response's <c>Date</c> header against <paramref name="localNow"/>.</summary>
    public void ObserveDate(DateTimeOffset? serverDate, DateTimeOffset localNow)
    {
        if (serverDate is { } date)
        {
            ClockOffset = date - localNow;
            // A custom server the player chose (CR-L39) is the only clock its relay proof can use, http or not.
            if(Core.Scheme==Uri.UriSchemeHttps && Gate.Scheme==Uri.UriSchemeHttps || DevelopmentStack
                || Auth.CustomOrigins.IsAllowed(Core) && Auth.CustomOrigins.IsAllowed(Gate))
            {
                lock(_serverTimeLock)
                {
                    var now=TimeProvider.System.GetTimestamp();
                    var previous=_serverTime is {} sampled ? sampled+TimeProvider.System.GetElapsedTime(_serverTimeStamp,now) : date;
                    _serverTime=date>previous?date:previous;_serverTimeStamp=now;
                }
            }
        }
    }

    /// <summary>
    /// https anywhere; plain http only on loopback or a private LAN address with a port, or on an origin a custom server the
    /// player chose made acceptable (<see cref="Auth.CustomOrigins"/>, CR-L39).
    /// </summary>
    public static Uri CheckOrigin(Uri origin)
    {
        ArgumentNullException.ThrowIfNull(origin);
        if (origin.Scheme == Uri.UriSchemeHttps)
        {
            return origin;
        }
        var text = origin.GetLeftPart(UriPartial.Authority);
        if (origin.Scheme == Uri.UriSchemeHttp && (DevNet.PlainHttpUrl(text) || Auth.CustomOrigins.IsAllowed(origin)))
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

    /// <summary><c>POST /auth/sign-up</c> on a <c>local</c> instance (CR-L38). The caller mints the key; the launcher mints one per press of Create, so a second press
    /// after a lost answer reads <c>ACCOUNTS.EMAIL_TAKEN</c> ("sign in instead").</summary>
    public Task<LocalIdToken> LocalSignUpAsync(LocalCredentials credentials, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<LocalIdToken>(HttpMethod.Post, Core, "/launcher/v1/auth/sign-up", credentials, access: null, idempotencyKey, cancellation);

    /// <summary><c>POST /auth/sign-in</c> on a <c>local</c> instance: the ID token for <see cref="ExchangeIdTokenAsync"/>.</summary>
    public Task<LocalIdToken> LocalSignInAsync(LocalCredentials credentials, CancellationToken cancellation = default) =>
        SendAsync<LocalIdToken>(HttpMethod.Post, Core, "/launcher/v1/auth/sign-in", credentials, access: null, idempotencyKey: null, cancellation);

    /// <summary><c>POST /auth/password</c> on a <c>local</c> instance → 204; the server revokes every refresh family of the account.</summary>
    /// <summary>CR-S53 <c>POST /launcher/v1/auth/confirmation</c>: the confirmation link mailed again (202; <c>AUTH.RATE_LIMITED</c>).</summary>
    public async Task LocalConfirmationAsync(LocalConfirmationRequest request, string language, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Post, Core, "/launcher/v1/auth/confirmation", request, access: null, idempotencyKey: null, cancellation, language).ConfigureAwait(false);

    /// <summary>CR-S53 <c>POST /launcher/v1/auth/confirmation/code</c>: the mail's code confirms the address; a confirmed ID token back.</summary>
    public Task<LocalIdToken> LocalConfirmationCodeAsync(LocalConfirmationCode request, CancellationToken cancellation = default) =>
        SendAsync<LocalIdToken>(HttpMethod.Post, Core, "/launcher/v1/auth/confirmation/code", request, access: null, idempotencyKey: null, cancellation);

    /// <summary>CR-S53 <c>POST /launcher/v1/auth/password-reset</c>: 202 whatever the address (a known one gets a password link).</summary>
    public async Task LocalPasswordResetAsync(LocalPasswordResetRequest request, string language, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Post, Core, "/launcher/v1/auth/password-reset", request, access: null, idempotencyKey: null, cancellation, language).ConfigureAwait(false);

    public async Task LocalChangePasswordAsync(LocalPasswordChange change, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Post, Core, "/launcher/v1/auth/password", change, access: null, idempotencyKey: null, cancellation).ConfigureAwait(false);

    /// <summary>A public on-demand check; never schedules background polling or sends a bearer token.</summary>
    public static async Task<OnlineStatus> OnlineStatusAsync(HttpClient http, Uri server, string clientHeader, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(http);
        using var request = new HttpRequestMessage(HttpMethod.Get, new Uri(CheckOrigin(server), "/core/v1/online-status"));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", clientHeader);
        using var response = await http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
        const int limit = 4096;
        if (response.Content.Headers.ContentLength is > limit) throw new InvalidDataException("Online status is too large.");
        await using var stream = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
        var bytes = new byte[limit + 1]; var length = 0;
        while (length < bytes.Length)
        {
            var read = await stream.ReadAsync(bytes.AsMemory(length), cancellation).ConfigureAwait(false);
            if (read == 0) break; length += read;
        }
        if (length > limit) throw new InvalidDataException("Online status is too large.");
        if (!response.IsSuccessStatusCode)
        {
            var oldContent = response.Content;
            response.Content = new ByteArrayContent(bytes, 0, length);
            oldContent.Dispose();
            return await Read<OnlineStatus>(response, cancellation).ConfigureAwait(false);
        }
        ValidateStatusNames(bytes.AsSpan(0, length));
        var value = JsonSerializer.Deserialize<OnlineStatus>(bytes.AsSpan(0, length), StatusJson) ?? throw new InvalidDataException("Online status is missing.");
        Online.OnlineStatusRules.Validate(value);
        return value;
    }

    public Task<OnlineStatus> OnlineStatusAsync(CancellationToken cancellation = default) => OnlineStatusAsync(_http, Core, _client, cancellation);

    private static void ValidateStatusNames(ReadOnlySpan<byte> bytes)
    {
        var reader = new Utf8JsonReader(bytes, new JsonReaderOptions { MaxDepth = 4 });
        var names = new HashSet<string>(StringComparer.Ordinal);
        while (reader.Read())
            if (reader.TokenType == JsonTokenType.PropertyName && !names.Add(reader.GetString()!))
                throw new InvalidDataException("Duplicate online-status field.");
    }

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

    /// <summary>
    /// OD-118 upload of a local profile: the profile's own stable key, so a retry replays the first outcome instead of creating a
    /// second profile (a changed body under the same key is <c>REQUEST.IDEMPOTENCY_KEY_REUSED</c>).
    /// </summary>
    public Task<Profile> CreateProfileAsync(string access, NewProfile profile, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, "/launcher/v1/profiles", profile, access, idempotencyKey, cancellation);

    /// <summary><c>POST /launcher/v1/accounts</c> with the first local profile's stable key (OD-118).</summary>
    public Task<LauncherAccountCreated> CreateAccountAsync(LauncherAccountCreate create, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<LauncherAccountCreated>(HttpMethod.Post, Core, "/launcher/v1/accounts", create, access: null, idempotencyKey, cancellation);

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

    private async Task<T> SendAsync<T>(HttpMethod method, Uri origin, string path, object? body, string? access, string? idempotencyKey, CancellationToken cancellation,
        string? language = null)
    {
        using var request = new HttpRequestMessage(method, new Uri(origin, path));
        request.Headers.TryAddWithoutValidation("X-Fi-Client", _client);
        if (language is not null) request.Headers.TryAddWithoutValidation("Accept-Language", language); // the mail's language (CR-S53)
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
