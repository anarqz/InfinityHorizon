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
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text("worlds.private_hold")));
        return Decided(kind, FreeRoamResult.None,
            $"no move: {world} is a private lobby running {activity ?? "an activity not read yet"}; the seat stays (no quick join; A7: a private Road Trip is created from the menu, never moved into)");
    }

    /// <summary>
    /// T-L6: kinds 3/4 (MATCHMAKING_API §2.10). One create call, no retry (the hook's wait is 20 s and the call counts against
    /// <c>mm.join</c>): granted → the proof is installed (the overlay's join path) and the seat is marked private; refused → none
    /// and one notice: <c>lobbies_full</c>, <c>pool</c> (the time left), <c>client_update</c>, an older server
    /// (<c>REQUEST.INVALID {field: "private"}</c>), rate limited, or a failure.
    /// </summary>
    private async Task<byte> PrivateCreateCoreAsync(byte kind, CancellationToken cancellation)
    {
        var activity = ActivityOf(kind) ?? WorldActivities.FreeRoam;
        if (!PrivateCreateCapable) return Decided(kind, FreeRoamResult.None, "no private lobby (private creates are off in this session)");
        using var budget = new CancellationTokenSource(FreeRoamAskBudget, _clock);
        using var bounded = CancellationTokenSource.CreateLinkedTokenSource(cancellation, budget.Token);
        var token = bounded.Token;
        try
        {
            if (await AccessAsync(token).ConfigureAwait(false) is { Allowed: false })
                return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, "no private lobby (online lobbies are not allowed now)");
            if (OwnWorld) return PrivateRefused(kind, "worlds.close_before_join", "this player hosts a lobby");
            if (WireMods().Refusal is { } mods) return PrivateRefused(kind, mods, $"mods: {mods}");
            Volatile.Write(ref _lobbiesFullAfter, -1);
            var joined = await JoinAsync(new UiFrame.JoinWorld(string.Empty, string.Empty), activity, token, kind, privateCreate: true).ConfigureAwait(false);
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
            var notice = full ? _text("worlds.lobbies_full")
                : refusal == ConflictReasons.Pool ? PoolNotice() ?? joined.Message
                : joined.Message;
            _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, notice));
            return Decided(kind, FreeRoamResult.None, $"no private lobby (create {activity} refused: {(full ? ConflictReasons.LobbiesFull : refusal ?? joined.Result.ToString(System.Globalization.CultureInfo.InvariantCulture))})");
        }
        catch (OperationCanceledException) when (budget.IsCancellationRequested && !cancellation.IsCancellationRequested && OnlineOpen)
        {
            _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text("worlds.private_failed")));
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
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Warn, 2, 60, _text(key)));
        return Decided(kind, FreeRoamResult.None, $"no private lobby ({why})");
    }
}
