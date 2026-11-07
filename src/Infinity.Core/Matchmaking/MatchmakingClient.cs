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

    /// <summary><c>GET /mm/v1/watch</c> → 200 (§3.9): the peer-observation rules. The default (a 404) serves only the test fakes.</summary>
    Task<MmResult<WatchRules>> WatchAsync(CancellationToken cancellation = default) =>
        Task.FromResult(new MmResult<WatchRules>(HttpStatusCode.NotFound, null,
            new MmError(HttpStatusCode.NotFound, "HTTP.404", "no watch rules", MmErrorKind.Other, null, null), null));
    /// <summary><c>POST /mm/v1/worlds/{worldId}/observations</c> → 204 (§3.9); a repeated id answers 204 too.</summary>
    Task<MmResult<MmNone>> ObserveAsync(string worldId, PeerObservation observation, CancellationToken cancellation = default) =>
        Task.FromResult(new MmResult<MmNone>(HttpStatusCode.NotFound, default,
            new MmError(HttpStatusCode.NotFound, "MM.WORLD_NOT_FOUND", "unavailable", MmErrorKind.WorldNotFound, null, null), null));
    /// <summary>Retain only a short-lived credential for authenticated own-seat/host cleanup after suspension.</summary>
    void BeginCleanup() { }
    void EndCleanup() { }
    Task<MmResult<WorldView>> GetAsync(string worldId, CancellationToken cancellation = default);

    Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, CancellationToken cancellation = default);

    /// <summary>
    /// T-L4 (OD-204, WORLDS_OFFICIAL §12.5): the invite with the user action's <c>Idempotency-Key</c> (one per action; its retry reuses
    /// it). The route is idempotent by state (200 with the pending invite, MATCHMAKING_API §3.4), so a server that ignores the header
    /// answers the same.
    /// </summary>
    Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, string idempotencyKey, CancellationToken cancellation) =>
        InviteAsync(worldId, xuid, cancellation);

    /// <summary>
    /// OD-210 (CR-S101, MATCHMAKING_API §2.11, W14): <c>PUT /mm/v1/worlds/{worldId}/modes</c> <see cref="WorldJoinModes"/> → 200
    /// <see cref="WorldView"/>: who may join a private official world from now on; its current leader only (else 409 <c>not_leader</c>).
    /// A test double without it refuses.
    /// </summary>
    Task<MmResult<WorldView>> SetWorldModesAsync(string worldId, string modes, CancellationToken cancellation = default) =>
        throw new NotSupportedException("world join modes");

    /// <summary>
    /// CR-S102: "Ask to join" a friend's official world (<c>POST /mm/v1/worlds/{worldId}/requests</c>) with the asker's mod set (null:
    /// vanilla, no body); its leader answers. A server without it answers 409 <c>{reason: "official"}</c>.
    /// </summary>
    Task<MmResult<Invite>> RequestAsync(string worldId, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation = default);

    /// <summary>CR-S102: the leader accepts a request to join its world (<c>POST …/requests/{requestId}/accept</c>).</summary>
    Task<MmResult<Invite>> AcceptRequestAsync(string worldId, string requestId, CancellationToken cancellation = default);

    /// <summary>
    /// <c>POST /mm/v1/join</c>: an official world by id, with the caller's mod set (null: vanilla, left out) and the
    /// <paramref name="activity"/> asked for (null: any; another is 409 <c>{reason: "activity"}</c>). OD-190: with no
    /// <paramref name="worldId"/>, a quick join into an official world running it.
    /// </summary>
    Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, IReadOnlyList<ModEntry>? mods, string? activity, CancellationToken cancellation = default);

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, CR-S96): the private create, <c>POST /mm/v1/join {activity, private: true}</c>: the caller's own
    /// private official world, or a spare promoted for it, ending the caller's other live seat. A test double without it refuses.
    /// </summary>
    Task<MmResult<JoinAnswer>> PrivateCreateAsync(string activity, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation) =>
        throw new NotSupportedException("private create");

    Task<MmResult<SeatAnswer>> SeatAsync(CancellationToken cancellation = default);

    Task<MmResult<SeatAnswer>> SeatWithMembersAsync(CancellationToken cancellation = default) => SeatAsync(cancellation);

    Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, CancellationToken cancellation = default);

    Task<MmResult<MmNone>> LeaveSeatAsync(CancellationToken cancellation = default);

    Task<MmResult<MmNone>> LeaveGrantAsync(string grantId, CancellationToken cancellation = default);


    /// <summary>
    /// The players of the world the caller holds a seat in (CR-L25, OD-63; deployed 2026-10-19).
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

    public Task<MmResult<WorldView>> GetAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<WorldView>(HttpMethod.Get, World(worldId), null, cancellation);

    public Task<MmResult<WorldMembers>> MembersAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync<WorldMembers>(HttpMethod.Get, World(worldId) + "/members", null, cancellation);

    public Task<MmResult<WatchRules>> WatchAsync(CancellationToken cancellation = default) =>
        SendAsync<WatchRules>(HttpMethod.Get, "/mm/v1/watch", null, cancellation);

    public Task<MmResult<MmNone>> ObserveAsync(string worldId, PeerObservation observation, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(observation);
        return SendAsync<MmNone>(HttpMethod.Post, World(worldId) + "/observations", observation, cancellation);
    }

    public Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(xuid);
        return SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/invites", new InviteRequest { Xuid = xuid }, cancellation);
    }

    public Task<MmResult<Invite>> InviteAsync(string worldId, string xuid, string idempotencyKey, CancellationToken cancellation)
    {
        ArgumentException.ThrowIfNullOrEmpty(xuid);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/invites", new InviteRequest { Xuid = xuid }, cancellation, idempotencyKey);
    }

    /// <summary>
    /// OD-210 (CR-S101 §2.11, W14): <c>PUT /mm/v1/worlds/{worldId}/modes {modes}</c>, one of <see cref="WorldModes"/>; anything else
    /// throws before sending. Idempotent by state (the same modes again answers the same <see cref="WorldView"/>).
    /// </summary>
    public Task<MmResult<WorldView>> SetWorldModesAsync(string worldId, string modes, CancellationToken cancellation = default)
    {
        if (modes is null || !WorldModes.All.Contains(modes)) throw new ArgumentException("one of any, hardcore, noob", nameof(modes));
        return SendAsync<WorldView>(HttpMethod.Put, World(worldId) + "/modes", new WorldJoinModes { Modes = modes }, cancellation);
    }

    /// <summary>CR-S102: the asker's set as <c>{mods}</c>; a vanilla request keeps the empty body.</summary>
    public Task<MmResult<Invite>> RequestAsync(string worldId, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation = default) =>
        SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/requests",
            mods is { Count: > 0 } ? new RequestToJoin { Mods = mods } : null, cancellation);
    public Task<MmResult<Invite>> AcceptRequestAsync(string worldId, string requestId, CancellationToken cancellation = default) =>
        SendAsync<Invite>(HttpMethod.Post, World(worldId) + "/requests/" + Id(requestId, nameof(requestId)) + "/accept", null, cancellation);

    /// <summary>
    /// <c>POST /mm/v1/join {worldId}</c> (an official world by id), or OD-190's quick join <c>{activity}</c>. A 429 answers
    /// <c>status: "rate_limited"</c> with <c>retryAfterSeconds</c> (from <c>Retry-After</c> when the body is the generic error body).
    /// <paramref name="mods"/> is the caller's set (null or empty: vanilla, left out of the body).
    /// </summary>
    public async Task<MmResult<JoinAnswer>> JoinAsync(string? worldId, IReadOnlyList<ModEntry>? mods, string? activity, CancellationToken cancellation = default)
    {
        // OD-190 (WORLDS_OFFICIAL §11.2): no world id is a quick join, which names its activity (car meets too)
        var quick = worldId is null;
        // fm5-9: a join by world id names the world's activity, FM5's race included (CR-S99); T3's invite accept threw here
        if (activity is not null && !(quick ? WorldActivities.Placement : WorldActivities.Joinable).Contains(activity))
        {
            throw new ArgumentException(quick ? "freeroam, carmeet or roadtrip" : "freeroam, roadtrip or race", nameof(activity));
        }
        if (quick && activity is null)
        {
            throw new ArgumentException("a world id, or a quick join's activity");
        }
        var body = new JoinRequest
        {
            WorldId = worldId is null ? null : Id(worldId, nameof(worldId)),
            Mods = mods is { Count: > 0 } ? mods : null,
            Activity = activity,
        };
        return await PostJoinAsync(body, cancellation).ConfigureAwait(false);
    }
    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10): the private create, <c>POST /mm/v1/join {activity, private: true}</c> (<paramref name="activity"/>
    /// one of <see cref="WorldActivities.PrivateCreate"/>: FH2's <c>freeroam</c> or <c>roadtrip</c>, FM5's <c>race</c>, CR-S99). Answered as
    /// any join (a 429 reads as <c>status: "rate_limited"</c>).
    /// </summary>
    public Task<MmResult<JoinAnswer>> PrivateCreateAsync(string activity, IReadOnlyList<ModEntry>? mods, CancellationToken cancellation)
    {
        if (!WorldActivities.PrivateCreate.Contains(activity)) throw new ArgumentException("freeroam, roadtrip or race", nameof(activity));
        return PostJoinAsync(new JoinRequest
        {
            Mods = mods is { Count: > 0 } ? mods : null,
            Activity = activity,
            Private = true,
        }, cancellation);
    }

    private async Task<MmResult<JoinAnswer>> PostJoinAsync(JoinRequest body, CancellationToken cancellation)
    {
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

    public Task<MmResult<SeatAnswer>> RenewSeatAsync(string grantId, CancellationToken cancellation = default) =>
        SendAsync<SeatAnswer>(HttpMethod.Post, "/mm/v1/seat/renew", new SeatRenew { GrantId = Id(grantId, nameof(grantId)) }, cancellation);

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

    private async Task<MmResult<T>> SendAsync<T>(HttpMethod method, string route, object? body, CancellationToken cancellation,
        string? idempotencyKey = null)
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
        if (idempotencyKey is not null) request.Headers.TryAddWithoutValidation("Idempotency-Key", idempotencyKey);
        if (body is not null)
        {
            request.Content = JsonContent.Create(body, body.GetType(), options: ApiClient.Json);
        }
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        return await ReadAsync<T>(response, cancellation).ConfigureAwait(false);
    }

    private static bool CleanupRoute(string route) => route == "/mm/v1/seat"
        || route.StartsWith("/mm/v1/join/", StringComparison.Ordinal) && !route[12..].Contains('/');

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
