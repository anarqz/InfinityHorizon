// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Matchmaking;

/// <summary>No body (a 204 answer).</summary>
public readonly record struct MmNone;

/// <summary>
/// One answer of <c>/mm/v1</c>: <see cref="Value"/> on 2xx (and on a join's 429, whose body is a
/// <see cref="JoinAnswer"/> with <c>status: "rate_limited"</c>), else <see cref="Error"/>, typed
/// from the contract's error body. <see cref="RetryAfter"/> from <c>Retry-After</c>. Network
/// failures are not answers: they throw (<see cref="HttpRequestException"/>, a timeout), and the
/// caller treats them as transient.
/// </summary>
public sealed record MmResult<T>(HttpStatusCode Status, T? Value, MmError? Error, TimeSpan? RetryAfter)
{
    public bool Ok => Error is null;

    /// <summary>201: <c>PUT</c> opened the world; an invite or request was made (200: the pending one).</summary>
    public bool Created => Status == HttpStatusCode.Created;
}

/// <summary>The matchmaking routes the launcher calls, behind an interface so its logic tests with a fake.</summary>
public interface IMatchmakingApi
{
    Task<MmResult<SeatNetworkAnswer>> SeatNetworkAsync(SeatNetworkRequest request,CancellationToken cancellation=default) =>
        Task.FromResult(new MmResult<SeatNetworkAnswer>(HttpStatusCode.NotFound,null,
            new MmError(HttpStatusCode.NotFound,"MM.WORLD_NOT_FOUND","unavailable",MmErrorKind.WorldNotFound,null,null),null));
    /// <summary>Retain only a short-lived credential for authenticated own-seat/host cleanup after suspension.</summary>
    void BeginCleanup() { }
    void EndCleanup() { }
    Task<MmResult<WorldList>> ListAsync(string filter, bool notFull = false, CancellationToken cancellation = default);

    /// <summary>OD-112: the list with the <c>modded</c> filter (false: vanilla worlds only, the server's default).</summary>
    Task<MmResult<WorldList>> ListAsync(string filter, bool notFull, bool modded, CancellationToken cancellation) =>
        ListAsync(filter, notFull, cancellation);

    Task<MmResult<WorldView>> GetAsync(string worldId, CancellationToken cancellation = default);

    Task<MmResult<WorldView>> ByCodeAsync(string code, CancellationToken cancellation = default);

    Task<MmResult<WorldAnnounced>> PutWorldAsync(string worldId, WorldSettings settings, CancellationToken cancellation = default);

    Task<MmResult<WorldAnnounced>> AnnounceAsync(string worldId, Heartbeat heartbeat, CancellationToken cancellation = default);

    Task<MmResult<WorldView>> NewCodeAsync(string worldId, CancellationToken cancellation = default);

    Task<MmResult<MmNone>> CloseAsync(string worldId, CancellationToken cancellation = default);

    Task<MmResult<ModerationResult>> ModerationAsync(string worldId, Moderation moderation, CancellationToken cancellation = default);

    Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, CancellationToken cancellation = default);

    Task<MmResult<Invite>> RequestAsync(string worldId, CancellationToken cancellation = default);

    /// <summary>OD-112: a request to join with the caller's mod set (null: vanilla, no body).</summary>
    Task<MmResult<Invite>> RequestAsync(string worldId, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation) =>
        RequestAsync(worldId, cancellation);

    Task<MmResult<Invite>> AcceptRequestAsync(string worldId, string requestId, CancellationToken cancellation = default);

    Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, CancellationToken cancellation = default);

    /// <summary>OD-112: a join with the caller's mod set (null: vanilla, left out of the body).</summary>
    Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation) =>
        JoinAsync(worldId, code, cancellation);

    /// <summary>CR-S57: a join with the joiner's NAT test and relay round trips (both null: left out, direct only).</summary>
    Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, IReadOnlyList<ModEntry>? mods, string? nat,
        IReadOnlyDictionary<string, int>? relays, CancellationToken cancellation) => JoinAsync(worldId, code, mods, cancellation);

    Task<MmResult<SeatAnswer>> SeatAsync(CancellationToken cancellation = default);

    Task<MmResult<SeatAnswer>> SeatWithMembersAsync(CancellationToken cancellation = default) => SeatAsync(cancellation);

    Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, CancellationToken cancellation = default);

    /// <summary>CR-S57: <c>{grantId, transport}</c> (<c>"relay"</c>: the joiner fell back; null: the plain renew).</summary>
    Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, string? transport, CancellationToken cancellation) => RenewSeatAsync(grantId, cancellation);

    Task<MmResult<MmNone>> LeaveSeatAsync(CancellationToken cancellation = default);

    Task<MmResult<MmNone>> LeaveGrantAsync(string grantId, CancellationToken cancellation = default);

    /// <summary>The host's complete admission/revocation snapshot, MATCHMAKING_API.md §4.</summary>
    Task<MmResult<Punches>> PunchesAsync(CancellationToken cancellation = default) =>
        Task.FromResult(new MmResult<Punches>(HttpStatusCode.NotFound, null,
            new MmError(HttpStatusCode.NotFound, "MM.WORLD_NOT_FOUND", "no punches", MmErrorKind.WorldNotFound, null, null), null));

    /// <summary>
    /// The players of a world the caller hosts or holds a seat in (CR-L25, OD-63; deployed 2026-10-19).
    /// The default (a 404) serves only the test fakes; on a 404 the caller re-reads its seat.
    /// </summary>
    Task<MmResult<WorldMembers>> MembersAsync(string worldId, CancellationToken cancellation = default) =>
        Task.FromResult(new MmResult<WorldMembers>(System.Net.HttpStatusCode.NotFound, null,
            new MmError(System.Net.HttpStatusCode.NotFound, "MM.WORLD_NOT_FOUND", "no roster", MmErrorKind.WorldNotFound, null, null), null));
}

/// <summary>
/// HTTP client of MATCHMAKING_API.md §2-§3.4 (<c>/mm/v1</c>, origin <c>services.matchmaking</c> of
/// <c>/launcher/v1/info</c>). Every request carries <c>X-Fi-Client</c> and the game access ticket
/// as <c>X-Fi-Session</c> (GATE_API.md §3; never <c>Authorization</c>), read from
/// <paramref name="ticket"/> per call so a ticket the gate's heartbeat rotated is used at once.
/// Same origin rules as <see cref="ApiClient"/>. No <c>Idempotency-Key</c>: the contract makes
/// these writes idempotent by the client-minted world id or by state. After suspension only a
/// fixed-window cleanup credential remains; it cannot renew the session or permit ordinary calls.
/// </summary>
public sealed class MatchmakingClient(HttpClient http, Uri matchmaking, string clientHeader, Func<string> ticket, TimeProvider? clock = null) : IMatchmakingApi
{
    public Task<MmResult<SeatNetworkAnswer>> SeatNetworkAsync(SeatNetworkRequest request,CancellationToken cancellation=default) =>
        SendAsync<SeatNetworkAnswer>(HttpMethod.Post,"/mm/v1/seat/network",request,cancellation);
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly object _cleanupGate = new();
    private string _lastTicket = string.Empty, _cleanupTicket = string.Empty;
    private long _cleanupStarted;
    private bool _cleanupBegun;
    public void BeginCleanup()
    {
        lock (_cleanupGate)
        {
            if (_cleanupBegun) return;
            _cleanupBegun = true;
            var current = _ticket();
            _cleanupTicket = string.IsNullOrEmpty(current) ? _lastTicket : current;
            _lastTicket = string.Empty;
            _cleanupStarted = _clock.GetTimestamp();
        }
    }
    public void EndCleanup()
    {
        lock (_cleanupGate)
        {
            _lastTicket = string.Empty;
            if (_clock.GetElapsedTime(_cleanupStarted) >= TimeSpan.FromSeconds(10)) _cleanupTicket = string.Empty;
        }
    }
    private readonly HttpClient _http = http ?? throw new ArgumentNullException(nameof(http));
    private readonly Func<string> _ticket = ticket ?? throw new ArgumentNullException(nameof(ticket));

    public Uri Matchmaking { get; } = ApiClient.CheckOrigin(matchmaking);

    /// <summary><c>GET /mm/v1/worlds?filter=</c>: <paramref name="filter"/> one of <see cref="WorldFilters"/> (required, OD-58).</summary>
    public Task<MmResult<WorldList>> ListAsync(string filter, bool notFull = false, CancellationToken cancellation = default) =>
        ListAsync(filter, notFull, false, cancellation);

    /// <summary>
    /// OD-112: <c>&amp;modded=true</c> only when asked: an older server refuses any unknown parameter, and
    /// <c>false</c> is the default (vanilla worlds only), so a vanilla list stays the same request.
    /// </summary>
    public Task<MmResult<WorldList>> ListAsync(string filter, bool notFull, bool modded, CancellationToken cancellation)
    {
        if (filter is null || !WorldFilters.All.Contains(filter))
        {
            throw new ArgumentException("one of global, near, friends", nameof(filter));
        }
        var query = "?filter=" + filter + (notFull ? "&notFull=true" : "") + (modded ? "&modded=true" : "");
        return SendAsync<WorldList>(HttpMethod.Get, "/mm/v1/worlds" + query, null, cancellation);
    }

    public Task<MmResult<WorldView>> GetAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<WorldView>(HttpMethod.Get, World(worldId), null, cancellation);

    /// <summary><c>GET /mm/v1/worlds/by-code/{code}</c>: 8 digits, a hyphen allowed; anything else throws before sending (a wrong code counts against the throttle).</summary>
    public Task<MmResult<WorldView>> ByCodeAsync(string code, CancellationToken cancellation = default) =>
        SendAsync<WorldView>(HttpMethod.Get, "/mm/v1/worlds/by-code/" + Code(code), null, cancellation);

    /// <summary><c>PUT /mm/v1/worlds/{worldId}</c>: 201 (<see cref="MmResult{T}.Created"/>) opened, 200 changed.</summary>
    public Task<MmResult<WorldAnnounced>> PutWorldAsync(string worldId, WorldSettings settings, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(settings);
        return SendAsync<WorldAnnounced>(HttpMethod.Put, World(worldId), settings, cancellation);
    }

    /// <summary><c>POST …/announce</c> every 15 s; 404 <c>MM.WORLD_NOT_FOUND</c>: open a new world with a new id.</summary>
    public Task<MmResult<WorldAnnounced>> AnnounceAsync(string worldId, Heartbeat heartbeat, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(heartbeat);
        return SendAsync<WorldAnnounced>(HttpMethod.Post, World(worldId) + "/announce", heartbeat, cancellation);
    }

    public Task<MmResult<WorldView>> NewCodeAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<WorldView>(HttpMethod.Post, World(worldId) + "/code", null, cancellation);

    /// <summary><c>DELETE /mm/v1/worlds/{worldId}</c> → 204 (the host left free roam).</summary>
    public Task<MmResult<MmNone>> CloseAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<MmNone>(HttpMethod.Delete, World(worldId), null, cancellation);

    public Task<MmResult<WorldMembers>> MembersAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<WorldMembers>(HttpMethod.Get, World(worldId) + "/members", null, cancellation);

    public Task<MmResult<ModerationResult>> ModerationAsync(string worldId, Moderation moderation, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(moderation);
        return SendAsync<ModerationResult>(HttpMethod.Post, World(worldId) + "/moderation", moderation, cancellation);
    }

    public Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(xuid);
        return SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/invites", new InviteRequest { Xuid = xuid }, cancellation);
    }

    public Task<MmResult<Invite>> RequestAsync(string worldId, CancellationToken cancellation = default) =>
        RequestAsync(worldId, null, cancellation);

    /// <summary>OD-112: the asker's set as <c>{mods}</c>; a vanilla request keeps the empty body.</summary>
    public Task<MmResult<Invite>> RequestAsync(string worldId, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation) =>
        SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/requests", mods is { Count: > 0 } ? new RequestToJoin { Mods = mods } : null, cancellation);

    public Task<MmResult<Invite>> AcceptRequestAsync(string worldId, string requestId, CancellationToken cancellation = default) =>
        SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/requests/" + Id(requestId, nameof(requestId)) + "/accept", null, cancellation);

    /// <summary>
    /// <c>POST /mm/v1/join {worldId?, code?}</c> (at least one). The code is sent as its 8 digits;
    /// one that is not 8 digits (with or without the hyphen) throws before sending. A 429 answers
    /// <c>status: "rate_limited"</c> with <c>retryAfterSeconds</c> (from <c>Retry-After</c> when
    /// the body is the generic error body).
    /// </summary>
    public Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, CancellationToken cancellation = default) =>
        JoinAsync(worldId, code, null, cancellation);

    /// <summary>OD-112: <paramref name="mods"/> is the caller's set (null or empty: vanilla, left out of the body).</summary>
    public Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation) =>
        JoinAsync(worldId, code, mods, null, null, cancellation);

    /// <summary>CR-S57: <paramref name="relays"/> is sent as given (also <c>{}</c>); null leaves <c>nat</c>/<c>relays</c> out.</summary>
    public async Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, string? code, IReadOnlyList<ModEntry>? mods, string? nat,
        IReadOnlyDictionary<string, int>? relays, CancellationToken cancellation)
    {
        if (worldId is null && code is null)
        {
            throw new ArgumentException("a world id, a code, or both");
        }
        var body = new JoinRequest
        {
            WorldId = worldId is null ? null : Id(worldId, nameof(worldId)),
            Code = code is null ? null : Code(code),
            Mods = mods is { Count: > 0 } ? mods : null,
            Nat = nat,
            Relays = relays,
        };
        var result = await SendAsync<JoinAnswer>(HttpMethod.Post, "/mm/v1/join", body, cancellation).ConfigureAwait(false);
        if (result.Error is { Kind: MmErrorKind.RateLimited } limited)
        {
            var secs = limited.RetryAfter is { } after ? (uint)Math.Ceiling(after.TotalSeconds) : (uint?)null;
            return result with
            {
                Value = new JoinAnswer { Status = JoinStatuses.RateLimited, RetryAfterSeconds = secs },
                Error = null,
            };
        }
        return result;
    }

    public Task<MmResult<SeatAnswer>> SeatAsync(CancellationToken cancellation = default) =>
        SendAsync<SeatAnswer>(HttpMethod.Get, "/mm/v1/seat", null, cancellation);

    public Task<MmResult<SeatAnswer>> SeatWithMembersAsync(CancellationToken cancellation = default) =>
        SendAsync<SeatAnswer>(HttpMethod.Get, "/mm/v1/seat?includeMembers=true", null, cancellation);

    public Task<MmResult<Punches>> PunchesAsync(CancellationToken cancellation = default) =>
        SendAsync<Punches>(HttpMethod.Get, "/mm/v1/punches", null, cancellation);

    public Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, CancellationToken cancellation = default) =>
        RenewSeatAsync(grantId, null, cancellation);

    public Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, string? transport, CancellationToken cancellation) =>
        SendAsync<SeatAnswer>(HttpMethod.Post, "/mm/v1/seat/renew", new SeatRenew { GrantId = Id(grantId, nameof(grantId)), Transport = transport }, cancellation);

    /// <summary><c>DELETE /mm/v1/seat</c> → 204: leaves the caller's live seat, whatever its id.</summary>
    public Task<MmResult<MmNone>> LeaveSeatAsync(CancellationToken cancellation = default) =>
        SendAsync<MmNone>(HttpMethod.Delete, "/mm/v1/seat", null, cancellation);

    /// <summary><c>DELETE /mm/v1/join/{grantId}</c> → 204: the same for one grant.</summary>
    public Task<MmResult<MmNone>> LeaveGrantAsync(string grantId, CancellationToken cancellation = default) =>
        SendAsync<MmNone>(HttpMethod.Delete, "/mm/v1/join/" + Uri.EscapeDataString(Id(grantId, nameof(grantId))), null, cancellation);

    private static string Id(string value, string name)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(value, name);
        return value;
    }

    private static string World(string worldId) => "/mm/v1/worlds/" + Uri.EscapeDataString(Id(worldId, nameof(worldId)));

    private static string Code(string code) =>
        WorldCode.Normalize(code) ?? throw new ArgumentException("a world code is 8 digits (XXXX-XXXX)", nameof(code));

    private async Task<MmResult<T>> SendAsync<T>(HttpMethod method, string route, object? body, CancellationToken cancellation)
    {
        string token;
        lock (_cleanupGate)
        {
            if (_cleanupBegun)
            {
                if (_clock.GetElapsedTime(_cleanupStarted) >= TimeSpan.FromSeconds(10)) _cleanupTicket = string.Empty;
                if (method != HttpMethod.Delete || !CleanupRoute(route)) throw new InvalidOperationException("Online activity has ended.");
                token = _cleanupTicket;
            }
            else { token = _ticket(); if (!string.IsNullOrEmpty(token)) _lastTicket = token; }
        }
        if (string.IsNullOrEmpty(token))
        {
            throw new InvalidOperationException("no game access ticket");
        }
        using var request = new HttpRequestMessage(method, new Uri(Matchmaking, route))
            {Version=HttpVersion.Version11,VersionPolicy=HttpVersionPolicy.RequestVersionOrLower};
        request.Headers.TryAddWithoutValidation("X-Fi-Client", clientHeader);
        request.Headers.TryAddWithoutValidation("X-Fi-Session", token);
        if (body is not null)
        {
            request.Content = JsonContent.Create(body, body.GetType(), options: ApiClient.Json);
        }
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await ReadAsync<T>(response, cancellation).ConfigureAwait(false);
    }

    private static bool CleanupRoute(string route) => route == "/mm/v1/seat"
        || route.StartsWith("/mm/v1/join/", StringComparison.Ordinal) && !route[12..].Contains('/')
        || route.StartsWith("/mm/v1/worlds/", StringComparison.Ordinal) && !route[14..].Contains('/');

    private static async Task<MmResult<T>> ReadAsync<T>(HttpResponseMessage response, CancellationToken cancellation)
    {
        var retryAfter = RetryAfter(response);
        var text = await response.Content.ReadAsStringAsync(cancellation).ConfigureAwait(false);
        if (!response.IsSuccessStatusCode)
        {
            // A join over its limit answers its own JoinAnswer, not the error body.
            if (typeof(T) == typeof(JoinAnswer) && response.StatusCode == HttpStatusCode.TooManyRequests
                && TryParse<JoinAnswer>(text) is { Status: JoinStatuses.RateLimited } answer)
            {
                return new MmResult<T>(response.StatusCode, (T)(object)answer, null, retryAfter);
            }
            var error = TryParse<ErrorBody>(text) is { Code: not null } body
                ? body
                : new ErrorBody($"HTTP.{(int)response.StatusCode}", response.ReasonPhrase ?? string.Empty, null);
            return new MmResult<T>(response.StatusCode, default, MmError.From(response.StatusCode, error, retryAfter), retryAfter);
        }
        if (typeof(T) == typeof(MmNone))
        {
            return new MmResult<T>(response.StatusCode, default, null, retryAfter);
        }
        try
        {
            var value = string.IsNullOrWhiteSpace(text) ? default : JsonSerializer.Deserialize<T>(text, ApiClient.Json);
            return value is null
                ? new MmResult<T>(response.StatusCode, default, Bad(response.StatusCode, "HTTP.EMPTY", "empty body"), retryAfter)
                : new MmResult<T>(response.StatusCode, value, null, retryAfter);
        }
        catch (JsonException e)
        {
            return new MmResult<T>(response.StatusCode, default, Bad(response.StatusCode, "HTTP.BAD_BODY", e.Message), retryAfter);
        }
    }

    private static MmError Bad(HttpStatusCode status, string code, string message) =>
        new(status, code, message, MmErrorKind.Other, null, null);

    private static TValue? TryParse<TValue>(string text)
        where TValue : class
    {
        try
        {
            return string.IsNullOrWhiteSpace(text) ? null : JsonSerializer.Deserialize<TValue>(text, ApiClient.Json);
        }
        catch (JsonException)
        {
            // Not the contract's body (a proxy, an HTML page): the caller keeps the status.
            return null;
        }
    }

    private static TimeSpan? RetryAfter(HttpResponseMessage response) => response.Headers.RetryAfter switch
    {
        { Delta: { } delta } => delta,
        { Date: { } date } => date - DateTimeOffset.UtcNow is var d && d > TimeSpan.Zero ? d : TimeSpan.Zero,
        _ => null,
    };
}
