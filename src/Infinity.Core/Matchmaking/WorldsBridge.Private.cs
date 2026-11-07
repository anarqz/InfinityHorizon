// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>What a lobby ask does while the seat is in a private official world (<see cref="PrivateLobbyPolicy.Ask"/>).</summary>
public enum PrivateSeatAsk
{
    /// <summary>The private world runs this ask: Ready, the seat stays.</summary>
    Stay,

    /// <summary>The private world does not run this ask: the seat stays, the ask is answered none, nothing is quick joined.</summary>
    Hold,
}

/// <summary>
/// T-L6 (WORLDS_OFFICIAL §12.2 item 11, §12.6, §12.8b): the single seam for what a lobby ask does from inside a private lobby.
/// A7 (answered after T-R3): a private Road Trip is a separate creation from the game's menu, never a move from inside a lobby,
/// so a Road Trip ask from a private free roam holds; the leader-started private Road Trip (no OD-194 vote) is engine work (T-L9).
/// A car meet ask from a private lobby holds too (car meets stay public, OD-204 item 2; no move rule exists). A3 (leadership,
/// invites by the current leader) is the engine's and the server's (§12.8b) and T-L3/T-L4's for invites: no client decision here,
/// and any future "who may invite" seam reads the current leader's XUID, never the owner's. A private seat is never quick joined
/// out of.
/// </summary>
public static class PrivateLobbyPolicy
{
    /// <summary>
    /// A lobby ask of <paramref name="kind"/> (<see cref="FreeRoamKind"/> 0-2) in a private world running <paramref name="activity"/>
    /// (null: not read yet). A free roam ask stays in a free roam world (or one not read yet, as for any seat); a Road Trip ask stays
    /// in a private Road Trip world; a car meet ask, or any other activity, holds (A7: no move out of a private lobby).
    /// </summary>
    public static PrivateSeatAsk Ask(byte kind, string? activity) => kind switch
    {
        FreeRoamKind.FreeRoam when activity is null or WorldActivities.FreeRoam => PrivateSeatAsk.Stay,
        FreeRoamKind.RoadTrip when activity == WorldActivities.RoadTrip => PrivateSeatAsk.Stay,
        _ => PrivateSeatAsk.Hold,
    };

    /// <summary>
    /// T-L3 (A3, §12.8b): who may invite into a private lobby, the launcher's copy of the overlay's one rule (lobby_policy.h
    /// InviteGateOf): the current leader, by decimal XUID; an unknown leader lets nobody invite. The server refuses anyone else
    /// (409 <c>not_leader</c>, <see cref="PrivateLobbyNotices.NotLeader"/>). T-L4's picker checks it before the invites route.
    /// </summary>
    public static bool MayInvite(string? leaderXuid, string? ownXuid) =>
        !string.IsNullOrEmpty(leaderXuid) && !string.IsNullOrEmpty(ownXuid) && string.Equals(leaderXuid, ownXuid, StringComparison.Ordinal);
}

// T-L6 (OD-204, WORLDS_OFFICIAL §12.2, MATCHMAKING_API §2.10): the game's own menu creates a private free roam (or Road Trip);
// the hook holds that session write and asks here (FreeRoamNeed kinds 3/4). The launcher calls the private create
// (POST /mm/v1/join {activity, private: true}), installs the grant's FIUP proof context as any official join does, and answers
// Ready; the hook then lets the write through. A refusal answers none with one bilingual notice, and the hook fails the write
// (NET_OFFLINE_ERROR). The seat is a third class: official transport (direct, FIUP), a player world's lifetime (kept past the
// 30 s rule, rejoined by world id, exempt from test.11, never moved by a quick join).
public sealed partial class WorldsBridge
{
    // the world of the last seat known to be private (the private create's grant, a seat read, a world read); compared by id
    private string? _seatPrivate;

    // the hook's reference of the private session it holds (PrivateSession 0x28), for the log and T-L4/T-L5's invites
    private PrivateSessionRef? _privateSession;
    private sealed record PrivateSessionRef(string Scid, string TemplateName, string Name);

    /// <summary>
    /// T-L6: this session serves private creates: the server took <c>features: ["private_official_v1"]</c> and the hook reported
    /// <see cref="Session.CaptureWhat.PrivateCreate"/> (the supervisor then answered it). Off: kinds 3/4 answer none.
    /// </summary>
    public bool PrivateCreateCapable { get; set; }

    /// <summary>The seat's world is a private official world (as last known).</summary>
    public bool InPrivateLobby => _seat.GrantId is not null && SeatPrivate(Volatile.Read(ref _seatWorld));

    private bool SeatPrivate(string? world) => world is not null && Volatile.Read(ref _seatPrivate) == world;

    /// <summary>A seat read, a renewal or a world read said <paramref name="world"/> is private (MATCHMAKING_API §2.10).</summary>
    private void ObservePrivate(string world, WorldView view)
    {
        if (view.Private != true || view.Id != world) return;
        if (Interlocked.Exchange(ref _seatPrivate, world) != world) _log($"worlds: {world} is a private lobby");
        Volatile.Write(ref _seatOfficial, world); // transport stays official (direct, FIUP)
    }

    // T-L4 (a): 1 while SeatAnswer.privateWorldId may name the world the next lobby ask rejoins first: from this bridge's start (a
    // crash or a restart is a new launcher session) and after a private seat the server released, until a seat is granted, that
    // rejoin was refused, or the player left a private lobby on purpose
    private int _privateRejoinArmed = 1;
    // the private world the player left itself (the overlay's Leave): privateWorldId never brings it back
    private string? _privateLeft;

    /// <summary>
    /// T-L4 (a), MATCHMAKING_API §2.10 <c>SeatAnswer.privateWorldId</c>, WORLDS_OFFICIAL §12.6 "Crash or reconnect": a seat read with no
    /// seat names the open private world this account may rejoin; while armed, the next lobby ask joins it first by its world id
    /// (<c>POST /mm/v1/join {worldId}</c>), before any quick join. Absent (an older server): the released seat's world, as T-L6.
    /// </summary>
    private void ObservePrivateRejoin(string? privateWorld, SeatEnded? ended, string? generatedAt)
    {
        if (string.IsNullOrEmpty(privateWorld) || Volatile.Read(ref _privateRejoinArmed) == 0 || Volatile.Read(ref _privateLeft) == privateWorld) return;
        // review H1 (owner: the crash hold stays ~30 s, no longer client hold): only a seat of that very world that ended moments ago
        // (a crash or a lapse: released or expired, at most PrivateRejoinWithin before the read) makes it the first choice; a world the
        // account merely has access to (left long ago, or by the game's own menu earlier) is never joined from privateWorldId
        if (ended is null || ended.WorldId != privateWorld || ended.Reason is not (SeatEnds.Released or SeatEnds.Expired)) return;
        if (Parse(ended.At) is not { } endedAt) return;
        var reference = Parse(generatedAt) ?? Now;
        var ago = reference - endedAt;
        if (ago > PrivateRejoinWithin || ago < -PrivateRejoinWithin) return;
        Volatile.Write(ref _seatPrivate, privateWorld); // rejoined by id, never placed again by activity
        if (Volatile.Read(ref _recentlyLeft) is { } recent && recent.WorldId == privateWorld) return; // its first time stands
        Volatile.Write(ref _recentlyLeft, new RecentWorld(privateWorld, Now - (ago > TimeSpan.Zero ? ago : TimeSpan.Zero)));
        _log($"worlds: the server names private lobby {privateWorld}, whose seat ended {(int)Math.Max(0, ago.TotalSeconds)} s ago; the next lobby ask joins it first");

        static DateTimeOffset? Parse(string? at) => DateTimeOffset.TryParse(at, System.Globalization.CultureInfo.InvariantCulture,
            System.Globalization.DateTimeStyles.AssumeUniversal | System.Globalization.DateTimeStyles.AdjustToUniversal, out var parsed) ? parsed : null;
    }

    /// <summary>
    /// T-L4 (a), review H1: how recently the private world's seat must have ended for <c>privateWorldId</c> to lead the next lobby ask
    /// (the server's crash hold, owner 2026-11-06: about 30 s; the client adds no longer hold).
    /// </summary>
    public static readonly TimeSpan PrivateRejoinWithin = TimeSpan.FromSeconds(30);

    /// <summary>The hook's PrivateSession frame: the reference of the session the game is creating (logged without its name).</summary>
    public void PrivateSessionWritten(string scid, string templateName, string name)
    {
        Volatile.Write(ref _privateSession, new PrivateSessionRef(scid, templateName, name));
        _log($"worlds: private create: the game writes its session (template {templateName})");
    }

    // W-14 wording for a kept seat: a private lobby's or a player world's
    private string KeptSeat(string world) => SeatPrivate(world) ? "a private lobby's seat" : "a player world's seat";

    /// <summary>
    /// T-L6: a lobby ask while the seat is in a private world (<see cref="PrivateLobbyPolicy"/>): Ready when the world runs it, else
    /// none with the seat kept (no quick join, A7); null when the seat is not a private one.
    /// </summary>
    private byte? PrivateSeatDecision(byte kind)
    {
        if (_seat.GrantId is null || Volatile.Read(ref _seatWorld) is not { } world || !SeatPrivate(world)) return null;
        string? activity;
        lock (_mineState) activity = _seatActivity is { } seen && seen.WorldId == world ? seen.Activity : null;
        if (PrivateLobbyPolicy.Ask(kind, activity) == PrivateSeatAsk.Stay) return Decided(kind, FreeRoamResult.Ready, Staying("its private lobby"));
        // T-L3: the hold's notice always shows, private lobby or not (PrivateLobbyNotices' rule)
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text("worlds.private_hold"), UiToastFlags.Always));
        return Decided(kind, FreeRoamResult.None,
            $"no move: {world} is a private lobby running {activity ?? "an activity not read yet"}; the seat stays (no quick join; A7: a private Road Trip is created from the menu, never moved into)");
    }

    /// <summary>
    /// T-L4 follow-up (b): how long a private create answered <c>lobbies_full</c> keeps asking (its <c>Retry-After</c>, 2 s when absent),
    /// from the first ask: about 10 s, always inside <see cref="FreeRoamAskBudget"/> and so inside the hook's 20 s FreeRoamNeed wait.
    /// </summary>
    public static readonly TimeSpan PrivateCreateFullBudget = TimeSpan.FromSeconds(10);

    /// <summary>The wait before asking again after a <c>lobbies_full</c> without <c>Retry-After</c> (MATCHMAKING_API §2.9 sends 2).</summary>
    public static readonly TimeSpan PrivateCreateFullRetry = TimeSpan.FromSeconds(2);

    /// <summary>T-L4 (b), review M2: the most create calls one ask sends (half the <c>mm.join</c> bucket of 6 a minute).</summary>
    public const int PrivateCreateMaxAsks = 3;

    /// <summary>
    /// T-L6: kinds 3/4 (MATCHMAKING_API §2.10): granted → the proof is installed (the overlay's join path) and the seat is marked
    /// private; refused → none and one notice: <c>lobbies_full</c>, <c>pool</c> (the time left), <c>client_update</c>, an older server
    /// (<c>REQUEST.INVALID {field: "private"}</c>), rate limited, or a failure. T-L4 (b): only <c>lobbies_full</c> is asked again, after
    /// its <c>Retry-After</c> (<see cref="PrivateCreateFullRetry"/> when absent), while the next ask still starts within
    /// <see cref="PrivateCreateFullBudget"/> of the first and the whole ask within <see cref="FreeRoamAskBudget"/>, at most
    /// <see cref="PrivateCreateMaxAsks"/> calls (each counts against <c>mm.join</c>, 6 a minute: the game's retry and a rejoin keep room); its notice shows only when it still fails.
    /// </summary>
    private async Task<byte> PrivateCreateCoreAsync(byte kind, CancellationToken cancellation)
    {
        var activity = ActivityOf(kind) ?? WorldActivities.FreeRoam;
        if (!PrivateCreateCapable) return Decided(kind, FreeRoamResult.None, "no private lobby (private creates are off in this session)");
        var started = _clock.GetTimestamp();
        using var budget = new CancellationTokenSource(FreeRoamAskBudget, _clock);
        using var bounded = CancellationTokenSource.CreateLinkedTokenSource(cancellation, budget.Token);
        var token = bounded.Token;
        try
        {
            if (await AccessAsync(token).ConfigureAwait(false) is { Allowed: false })
                return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, "no private lobby (online lobbies are not allowed now)");
            if (OwnWorld) return PrivateRefused(kind, "worlds.close_before_join", "this player hosts a lobby");
            if (WireMods().Refusal is { } mods) return PrivateRefused(kind, mods, $"mods: {mods}");
            UiFrame.JoinResult joined;
            var asks = 0;
            TimeSpan? firstAsk = null;
            while (true)
            {
                firstAsk ??= _clock.GetElapsedTime(started);
                Volatile.Write(ref _lobbiesFullAfter, -1);
                asks++;
                joined = await JoinAsync(new UiFrame.JoinWorld(string.Empty, string.Empty), activity, token, kind, privateCreate: true).ConfigureAwait(false);
                if (joined.Result == UiJoin.Reserved || Volatile.Read(ref _lobbiesFullAfter) is not (var ticks and >= 0) || !OnlineOpen) break;
                var after = ticks > 0 ? TimeSpan.FromTicks(Math.Max(ticks, TimeSpan.TicksPerSecond)) : PrivateCreateFullRetry;
                var now = _clock.GetElapsedTime(started);
                if (asks >= PrivateCreateMaxAsks || now + after - firstAsk.Value > PrivateCreateFullBudget || now + after >= FreeRoamAskBudget) break;
                _log($"worlds: {KindName(kind)}: every {activity} lobby is full; asking for its private lobby again in {after.TotalSeconds.ToString("0.#", System.Globalization.CultureInfo.InvariantCulture)} s");
                await Task.Delay(after, _clock, token).ConfigureAwait(false);
            }
            if (joined.Result == UiJoin.Reserved && Volatile.Read(ref _seatWorld) is { } world)
            {
                Volatile.Write(ref _seatPrivate, world);
                Volatile.Write(ref _seatOfficial, world);
                lock (_mineState) _seatActivity = (world, activity);
                Volatile.Write(ref _recentlyLeft, null); // the private world is this ask's answer
                return Decided(kind, FreeRoamResult.Ready, $"joined its private lobby {world} ({activity})");
            }
            if (!OnlineOpen) return Decided(kind, FreeRoamResult.NotOnline, "no private lobby (online is closed)");
            var full = Interlocked.Exchange(ref _lobbiesFullAfter, -1) >= 0;
            var refusal = Volatile.Read(ref _lastJoinRefusal);
            // T-L3: the one table words every reason it knows (A10's pool with the time left); any other refusal keeps the join's own
            // words (mods, access, a game not loaded); either way the notice always shows, in an official lobby too
            var reason = full ? ConflictReasons.LobbiesFull
                : PrivateLobbyNotices.Handles(refusal) ? refusal
                : joined.Result == UiJoin.SlowDown ? PrivateLobbyNotices.RateLimited
                : null;
            var notice = reason is not null ? PrivateLobbyNotices.Text(_text, reason, reason == ConflictReasons.Pool ? PoolTimeLeft() : null)
                : string.IsNullOrWhiteSpace(joined.Message) ? PrivateLobbyNotices.Text(_text, PrivateLobbyNotices.Failed)
                : joined.Message;
            _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(notice));
            return Decided(kind, FreeRoamResult.None, $"no private lobby (create {activity} refused: {(full ? ConflictReasons.LobbiesFull : refusal ?? joined.Result.ToString(System.Globalization.CultureInfo.InvariantCulture))})");
        }
        catch (OperationCanceledException) when (budget.IsCancellationRequested && !cancellation.IsCancellationRequested && OnlineOpen)
        {
            _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.Failed));
            return Decided(kind, FreeRoamResult.None, $"no private lobby (no answer within {(int)FreeRoamAskBudget.TotalSeconds} s)");
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested || !OnlineOpen)
        {
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline,
                OnlineOpen ? "no private lobby (the game stopped waiting)" : "no private lobby (online is closed)");
        }
        catch (NotSupportedException)
        {
            return PrivateRefused(kind, "worlds.private_unavailable", "this matchmaking client has no private create");
        }
    }

    private byte PrivateRefused(byte kind, string key, string why)
    {
        _freeRoamToasts.Enqueue(PrivateLobbyNotices.Toast(_text(key))); // T-L3: always shown
        return Decided(kind, FreeRoamResult.None, $"no private lobby ({why})");
    }

    /// <summary>
    /// A10 (T-L3): how long the flagged Hardcore profile has no online lobby: the 409 pool's own <c>until</c> (CR-S96), else the last
    /// seat or list read's <c>pool.until</c>; null when neither is known (the notice then names no time).
    /// </summary>
    private TimeSpan? PoolTimeLeft()
    {
        var ticks = Volatile.Read(ref _lastPoolUntilTicks);
        if (ticks > 0) return new DateTimeOffset(ticks, TimeSpan.Zero) - Now;
        return PoolUntil is { } until ? until - Now : null;
    }
}
