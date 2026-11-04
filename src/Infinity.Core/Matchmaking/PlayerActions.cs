// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>Core's friend and report writes the game's PLAYER ACTIONS needs (CORE_API.md §4.1, §4.3; CR-S89, CR-S90).</summary>
public interface IPlayerActionsApi
{
    /// <summary><c>GET /launcher/v1/friends</c>.</summary>
    Task<FriendList> FriendsAsync(CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/friends {game, xuid}</c> (CR-S89).</summary>
    Task<FriendAddResult> AddAsync(string xuid, string idempotencyKey, CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/friends/{xuid}/accept</c>.</summary>
    Task AcceptAsync(string xuid, string idempotencyKey, CancellationToken cancellation);

    /// <summary><c>DELETE /launcher/v1/friends/{xuid}</c> (204): unfriend, or cancel this account's request.</summary>
    Task RemoveAsync(string xuid, string idempotencyKey, CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/reports {game, xuid, reason, worldId?}</c> (CR-S90: without <c>worldId</c>, no shared-world check).</summary>
    Task ReportAsync(string xuid, string reason, string? worldId, string idempotencyKey, CancellationToken cancellation);
}

/// <summary><see cref="IPlayerActionsApi"/> over <see cref="ApiClient"/> with the launcher session's access token.</summary>
public sealed class ApiPlayerActions(ApiClient api, Func<CancellationToken, Task<string>> access, string game = "fh2") : IPlayerActionsApi
{
    private readonly ApiClient _api = api ?? throw new ArgumentNullException(nameof(api));
    private readonly Func<CancellationToken, Task<string>> _access = access ?? throw new ArgumentNullException(nameof(access));

    public async Task<FriendList> FriendsAsync(CancellationToken cancellation) =>
        await _api.FriendsAsync(await _access(cancellation).ConfigureAwait(false), null, cancellation).ConfigureAwait(false);

    public async Task<FriendAddResult> AddAsync(string xuid, string idempotencyKey, CancellationToken cancellation) =>
        await _api.AddFriendByXuidAsync(await _access(cancellation).ConfigureAwait(false), game, xuid, idempotencyKey, cancellation).ConfigureAwait(false);

    public async Task AcceptAsync(string xuid, string idempotencyKey, CancellationToken cancellation) =>
        await _api.AcceptFriendAsync(await _access(cancellation).ConfigureAwait(false), xuid, idempotencyKey, cancellation).ConfigureAwait(false);

    public async Task RemoveAsync(string xuid, string idempotencyKey, CancellationToken cancellation) =>
        await _api.RemoveFriendAsync(await _access(cancellation).ConfigureAwait(false), xuid, idempotencyKey, cancellation).ConfigureAwait(false);

    public async Task ReportAsync(string xuid, string reason, string? worldId, string idempotencyKey, CancellationToken cancellation) =>
        await _api.ReportAsync(await _access(cancellation).ConfigureAwait(false),
            new GameReport { Game = game, Xuid = xuid, Reason = reason, WorldId = worldId }, idempotencyKey, cancellation).ConfigureAwait(false);
}

/// <summary>
/// OD-193 F-2: the game's own PLAYER ACTIONS popup offers ADD / REMOVE / ACCEPT / CANCEL FRIEND and REPORT PLAYER (the hook adds
/// the items; it asks through the overlay, which asks here over the UI pipe). The account's friend list
/// (<c>GET /launcher/v1/friends</c>) is cached and read again after every action and when older than <see cref="FreshFor"/>. A
/// player's state: a profile of a friend account → REMOVE; of an account that asked this one → ACCEPT; the profile this account
/// asked (an outgoing row's id) → CANCEL; this account's own profiles → nothing; anyone else → ADD. Each user action mints one
/// <c>Idempotency-Key</c> that its retry reuses. A report names <c>worldId</c> only when the overlay's roster ("my world") lists
/// the player (CR-S90). XUIDs never reach the log.
/// </summary>
[System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1001:Types that own disposable fields should be disposable",
    Justification = "The SemaphoreSlim's wait handle is never asked for, so it holds nothing to release; it lives as long as the play session.")]
public sealed class PlayerActions(IPlayerActionsApi api, Func<IReadOnlyCollection<string>> ownXuids, Func<string, string?> rosterWorld,
    TimeProvider? clock = null, Action<string>? log = null)
{
    /// <summary>How long a read friend list answers the popup without another read.</summary>
    public static readonly TimeSpan FreshFor = TimeSpan.FromSeconds(30);

    private readonly IPlayerActionsApi _api = api ?? throw new ArgumentNullException(nameof(api));
    private readonly Func<IReadOnlyCollection<string>> _own = ownXuids ?? throw new ArgumentNullException(nameof(ownXuids));
    private readonly Func<string, string?> _rosterWorld = rosterWorld ?? throw new ArgumentNullException(nameof(rosterWorld));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Action<string> _log = log ?? (_ => { });
    private readonly SemaphoreSlim _read = new(1, 1);
    private FriendList? _list;
    private long _readAt;

    /// <summary>The cached list (tests); null before the first good read.</summary>
    public FriendList? Cached => Volatile.Read(ref _list);

    /// <summary>The state of one player from <paramref name="list"/>: <see cref="UiPlayerState"/>.</summary>
    public static byte StateOf(FriendList? list, string xuid, IReadOnlyCollection<string> own)
    {
        ArgumentNullException.ThrowIfNull(own);
        if (own.Contains(xuid)) return UiPlayerState.Own;
        if (list is null) return UiPlayerState.Unknown;
        if (list.Friends.Any(f => f.Id == xuid || f.Profiles.Any(p => p.Xuid == xuid))) return UiPlayerState.Friend;
        if (list.Incoming.Any(r => r.Id == xuid || r.Profiles.Any(p => p.Xuid == xuid))) return UiPlayerState.Incoming;
        if (list.Outgoing.Any(r => r.Id == xuid || r.Profiles.Any(p => p.Xuid == xuid))) return UiPlayerState.Outgoing;
        return UiPlayerState.None;
    }

    /// <summary>0x1F → 0x20: the friend item for this player (a read of the list when the cache is old or missing).</summary>
    public async Task<UiFrame.PlayerState> StateAsync(UiFrame.PlayerStateQuery query, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(query);
        var own = _own();
        if (own.Contains(query.Xuid)) return new UiFrame.PlayerState(query.Xuid, UiPlayerState.Own);
        await RefreshAsync(force: false, cancellation).ConfigureAwait(false);
        return new UiFrame.PlayerState(query.Xuid, StateOf(Cached, query.Xuid, own));
    }

    /// <summary>0x21 → 0x22: one user action, then the list read again (its state is the answer's).</summary>
    public async Task<UiFrame.PlayerActionDone> ActAsync(UiFrame.PlayerActionRequest request, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(request);
        var own = _own();
        var xuid = request.Xuid;
        if (own.Contains(xuid)) return new UiFrame.PlayerActionDone(request.Id, xuid, UiPlayerResult.None, UiPlayerState.Own);
        var key = Guid.CreateVersion7().ToString(); // once per user action: the retry below reuses it
        var (result, guess) = request.Kind switch
        {
            UiPlayerAction.Report => await ReportAsync(xuid, request.Reason, key, cancellation).ConfigureAwait(false),
            _ => await FriendAsync(xuid, request.Kind, key, cancellation).ConfigureAwait(false),
        };
        if (request.Kind == UiPlayerAction.Report)
        {
            return new UiFrame.PlayerActionDone(request.Id, xuid, result, StateOf(Cached, xuid, own));
        }
        var read = await RefreshAsync(force: true, cancellation).ConfigureAwait(false);
        return new UiFrame.PlayerActionDone(request.Id, xuid, result, read ? StateOf(Cached, xuid, own) : guess);
    }

    private async Task<(byte Result, byte State)> FriendAsync(string xuid, byte kind, string key, CancellationToken cancellation)
    {
        var before = StateOf(Cached, xuid, []);
        try
        {
            switch (kind)
            {
                case UiPlayerAction.Add:
                    var added = await RetryAsync(ct => _api.AddAsync(xuid, key, ct), cancellation).ConfigureAwait(false);
                    _log($"player actions: friend add answered {added.State}");
                    return added.State == "friends"
                        ? (UiPlayerResult.FriendAdded, UiPlayerState.Friend)
                        : (UiPlayerResult.FriendRequestSent, UiPlayerState.Outgoing);
                case UiPlayerAction.Accept:
                    await RetryAsync(async ct => { await _api.AcceptAsync(xuid, key, ct).ConfigureAwait(false); return true; }, cancellation).ConfigureAwait(false);
                    _log("player actions: friend request accepted");
                    return (UiPlayerResult.FriendAdded, UiPlayerState.Friend);
                case UiPlayerAction.Remove:
                case UiPlayerAction.Cancel:
                    await RetryAsync(async ct => { await _api.RemoveAsync(xuid, key, ct).ConfigureAwait(false); return true; }, cancellation).ConfigureAwait(false);
                    _log(kind == UiPlayerAction.Remove ? "player actions: friend removed" : "player actions: friend request cancelled");
                    return (kind == UiPlayerAction.Remove ? UiPlayerResult.FriendRemoved : UiPlayerResult.RequestCancelled, UiPlayerState.None);
                default:
                    return (UiPlayerResult.ActionFailed, before);
            }
        }
        catch (ApiException e)
        {
            _log($"player actions: friend action refused (HTTP {(int)e.Status} {e.Code})");
            return (FriendRefusal(e), before);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException or IOException or System.Text.Json.JsonException or NotSupportedException)
        {
            _log($"player actions: friend action failed ({e.GetType().Name})");
            return (UiPlayerResult.ActionFailed, before);
        }
    }

    private async Task<(byte Result, byte State)> ReportAsync(string xuid, byte reason, string key, CancellationToken cancellation)
    {
        var text = reason switch { UiReport.Cheating => "cheating", UiReport.Abuse => "abuse", _ => "other" };
        var worldId = _rosterWorld(xuid); // CR-S90: only when the overlay's roster names the world
        try
        {
            try
            {
                await RetryAsync(async ct => { await _api.ReportAsync(xuid, text, worldId, key, ct).ConfigureAwait(false); return true; }, cancellation).ConfigureAwait(false);
            }
            catch (ApiException e) when (worldId is not null && e.Status == HttpStatusCode.NotFound && OnlinePause.From(e) is null)
            {
                // the roster named a world the server does not count as shared (a stale roster): any card may report (CR-S90), so
                // again without it, under a new key (the first key with another body is 409)
                _log("player actions: report refused for the roster's world; sent again without it");
                worldId = null;
                var again = Guid.CreateVersion7().ToString();
                await RetryAsync(async ct => { await _api.ReportAsync(xuid, text, null, again, ct).ConfigureAwait(false); return true; }, cancellation).ConfigureAwait(false);
            }
            _log(worldId is null ? "player actions: report submitted" : "player actions: report submitted (shared world named)");
            return (UiPlayerResult.ReportSubmitted, UiPlayerState.Unknown);
        }
        catch (ApiException e)
        {
            _log($"player actions: report refused (HTTP {(int)e.Status} {e.Code})");
            return (ReportRefusal(e), UiPlayerState.Unknown);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException or IOException or System.Text.Json.JsonException or NotSupportedException)
        {
            _log($"player actions: report failed ({e.GetType().Name})");
            return (UiPlayerResult.ReportFailed, UiPlayerState.Unknown);
        }
    }

    /// <summary>The game's words for a refused report: 429 try later, 400 (one's own profile) nothing, the rest "failed to submit".</summary>
    public static byte ReportRefusal(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        if (OnlinePause.From(error) is not null) return UiPlayerResult.ReportFailed;
        return error.Status switch
        {
            HttpStatusCode.TooManyRequests => UiPlayerResult.TryLater,
            HttpStatusCode.BadRequest when OwnProfile(error) => UiPlayerResult.None,
            _ => UiPlayerResult.ReportFailed,
        };
    }

    /// <summary>The game's words for a refused friend action (CORE_API.md §4.1).</summary>
    public static byte FriendRefusal(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        if (OnlinePause.From(error) is not null) return UiPlayerResult.ActionFailed;
        return error.Code switch
        {
            "SOCIAL.LIMIT_REACHED" => UiPlayerResult.FriendLimit,
            ErrorCodes.RequestRateLimited => UiPlayerResult.TryLater,
            "PORTAL.NOT_FOUND" => UiPlayerResult.PlayerNotFound,
            _ when error.Status == HttpStatusCode.TooManyRequests => UiPlayerResult.TryLater,
            _ when error.Status == HttpStatusCode.BadRequest && OwnProfile(error) => UiPlayerResult.None, // one's own profile: nothing to say
            _ => UiPlayerResult.ActionFailed,
        };
    }

    /// <summary>400 <c>REQUEST.INVALID {field: "xuid"}</c>: the player is a profile of this account (CORE_API §4.1, §4.3).</summary>
    private static bool OwnProfile(ApiException error) =>
        error.Code == "REQUEST.INVALID" && error.Details is { ValueKind: System.Text.Json.JsonValueKind.Object } details
        && details.TryGetProperty("field", out var field) && field.ValueKind == System.Text.Json.JsonValueKind.String && field.GetString() == "xuid";

    // One retry of a transport failure, with the same Idempotency-Key (the server answers a replay with the first outcome).
    private static async Task<T> RetryAsync<T>(Func<CancellationToken, Task<T>> call, CancellationToken cancellation)
    {
        try
        {
            return await call(cancellation).ConfigureAwait(false);
        }
        catch (HttpRequestException) when (!cancellation.IsCancellationRequested)
        {
            return await call(cancellation).ConfigureAwait(false);
        }
    }

    /// <summary>Reads the list when <paramref name="force"/>d or older than <see cref="FreshFor"/>; true when the cache is from a good read.</summary>
    public async Task<bool> RefreshAsync(bool force, CancellationToken cancellation)
    {
        await _read.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            if (!force && Cached is not null && _clock.GetElapsedTime(Volatile.Read(ref _readAt)) < FreshFor) return true;
            try
            {
                var list = await _api.FriendsAsync(cancellation).ConfigureAwait(false);
                Volatile.Write(ref _list, list);
                Volatile.Write(ref _readAt, _clock.GetTimestamp());
                return true;
            }
            catch (ApiException e)
            {
                _log($"player actions: friend list not read (HTTP {(int)e.Status} {e.Code})");
                return false;
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException or IOException or System.Text.Json.JsonException or NotSupportedException)
            {
                _log($"player actions: friend list not read ({e.GetType().Name})");
                return false;
            }
        }
        finally
        {
            _read.Release();
        }
    }
}
