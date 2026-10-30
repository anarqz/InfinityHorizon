// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Text;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>The control pipe's <c>FreeRoamReady.result</c> (CR-L40, LOBBIES_AND_SERVER_STATUS.md §3.1).</summary>
public static class FreeRoamResult
{
    public const byte Ready = 0;
    public const byte None = 1;
    public const byte NotOnline = 2;
}

/// <summary>CR-L40 §3.1: what the game's lobby search is for (FreeRoamNeed's kind), so the world must run that lobby.</summary>
public static class FreeRoamKind
{
    public const byte FreeRoam = 0;
    public const byte CarMeet = 1;
    public const byte RoadTrip = 2;
}

// CR-L40 §3.2: FREE ROAM ONLINE never lands alone when a lobby is open. Before the game's FreeRoam
// hopper request the hook asks; the launcher joins the closest matching lobby (the overlay's own join,
// with its access, game-readiness and mods checks) or opens one for everyone, and says so in a toast.
public sealed partial class WorldsBridge
{
    /// <summary>How many matching lobbies a FREE ROAM ONLINE request tries before it opens one (full or closed answers move on).</summary>
    public const int FreeRoamJoinAttempts = 3;

    /// <summary>The players and latency limit of the lobby FREE ROAM ONLINE opens (any latency).</summary>
    public const byte FreeRoamPlayers = 8;

    /// <summary>The longest lobby name the server keeps (the Create screen and the dedicated lobby cut there too).</summary>
    public const int MaxLobbyName = 48;

    /// <summary>The session profile's gamertag, for the name of a lobby opened for it (CR-L40 §4); null: "Infinity".</summary>
    public string? Gamertag { get; init; }

    private readonly ConcurrentQueue<UiFrame.Toast> _freeRoamToasts = new();

    /// <summary>How long a world the game just left stays its first choice for the next world it asks for.</summary>
    public static readonly TimeSpan RejoinWindow = TimeSpan.FromSeconds(60);
    private sealed record RecentWorld(string WorldId, DateTimeOffset LeftAt);
    private string? _seatWorld;
    private RecentWorld? _recentlyLeft;
    private RecentWorld? _leftOnPurpose;

    /// <summary>The player left its world itself (the overlay's Leave): no rejoin, and that world is not picked for a while.</summary>
    private void LeftOnPurpose()
    {
        Volatile.Write(ref _recentlyLeft, null);
        if (Interlocked.Exchange(ref _seatWorld, null) is { } world) Volatile.Write(ref _leftOnPurpose, new RecentWorld(world, Now));
    }
    private readonly object _freeRoamState = new();
    private Task<byte>? _freeRoamRun;
    private byte _freeRoamRunKind;

    /// <summary>
    /// How long the world outlives the game leaving free roam (the hook's Event 4): a car meet or an online Road Trip
    /// leaves its free roam session first and asks for its lobby (<c>FreeRoamNeed</c>) seconds later, in the same world.
    /// </summary>
    public static readonly TimeSpan FreeRoamLeftGrace = TimeSpan.FromSeconds(30);
    private CancellationTokenSource? _freeRoamLeave;

    // when a lobby ask was last answered with the world kept (Ready), as Now's UTC ticks; 0: none since the last leave
    private long _lobbyReadyTicks;

    /// <summary>
    /// OD-148 (refines OD-57): the game left free roam. After <see cref="FreeRoamLeftGrace"/> with no lobby ask the hosted
    /// world closes (<see cref="CloseHostAsync"/>) or the seat is let go (<see cref="LeaveAsync"/>); a lobby ask, the game
    /// joining a world again (<see cref="GameJoined"/>), the overlay's own close, leave, join or open, the session's end or
    /// another leave of free roam ends this wait first. A lobby ask answered Ready within the grace BEFORE this leave counts
    /// too (live, test.9: back from a Road Trip the game asks for free roam, opens it, and only then closes its Road Trip
    /// connection): no wait starts. Only the close or leave waits: the world's announces and the seat's renewals go on
    /// meanwhile. A hosted world whose Road Trip or car meet still has players (<see cref="IHostRelay.LobbyPlayers"/>) is
    /// not closed: it is weighed again every grace.
    /// </summary>
    public async Task FreeRoamLeftAsync(CancellationToken cancellation)
    {
        var asked = Interlocked.Exchange(ref _lobbyReadyTicks, 0); // one ask covers one leave
        if (asked != 0 && Now - new DateTimeOffset(asked, TimeSpan.Zero) is var ago && ago >= TimeSpan.Zero && ago <= FreeRoamLeftGrace)
        {
            CancelFreeRoamLeave();
            if ((HostedWorldId ?? Volatile.Read(ref _seatWorld)) is { } kept)
                _log($"worlds: free roam left; {kept} stays: the game asked for its next lobby {(int)ago.TotalSeconds} s ago");
            return;
        }
        var pending = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token);
        CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, pending)); // a new leave starts the wait again
        var world = HostedWorldId ?? Volatile.Read(ref _seatWorld);
        if (world is not null) _log($"worlds: free roam left; {world} is kept {(int)FreeRoamLeftGrace.TotalSeconds} s for the game's next lobby");
        var due = false;
        var held = false;
        try
        {
            while (true)
            {
                await Task.Delay(FreeRoamLeftGrace, _clock, pending.Token).ConfigureAwait(false);
                if (Volatile.Read(ref _freeRoamLeave) != pending) break; // called off meanwhile
                // a hosted world whose Road Trip or car meet still has players stays for them (live, test.8: the host's game
                // left its session between two Road Trip events while the other player drove on); it is weighed again later
                if (HostedWorldId is { } kept && LobbyPlayers() is var players and > 0)
                {
                    if (!held) _log($"worlds: free roam left; {kept} stays: {players} player(s) in its road trip/car meet");
                    held = true;
                    continue;
                }
                due = Interlocked.CompareExchange(ref _freeRoamLeave, null, pending) == pending; // nothing called it off meanwhile
                break;
            }
        }
        catch (OperationCanceledException) { }
        finally
        {
            Interlocked.CompareExchange(ref _freeRoamLeave, null, pending);
            pending.Dispose();
        }
        if (!due) return;
        var hosted = HostedWorldId;
        if ((hosted ?? Volatile.Read(ref _seatWorld)) is { } gone)
            _log(held && hosted is not null ? $"worlds: free roam left; closing {gone}: no player left in its road trip/car meet"
                : $"worlds: free roam left {(int)FreeRoamLeftGrace.TotalSeconds} s ago with no lobby asked; {(hosted is null ? "leaving" : "closing")} {gone}");
        await (hosted is not null ? CloseHostAsync(cancellation) : LeaveAsync(cancellation)).ConfigureAwait(false);
    }

    // the hosted relay's Road Trip and car meet players (IHostRelay.LobbyPlayers); 0 without a relay
    private int LobbyPlayers()
    {
        IHostRelay? relay;
        lock (_hostState) relay = _relay;
        return relay?.LobbyPlayers ?? 0;
    }

    /// <summary>
    /// The game joined a world again (the hook's Event 3, <c>host:&lt;port&gt;</c> or <c>join</c>): a pending close or leave
    /// after free roam is called off, the game is in the world (live, test.9: a joiner's Road Trip connection ended just
    /// before its free roam one opened).
    /// </summary>
    public void GameJoined(bool join = false)
    {
        if (join) SeatPlays("the game joined"); // a guest's join (detail join:…) is the seat's proof: no give-up
        if (Interlocked.Exchange(ref _freeRoamLeave, null) is not { } pending) return;
        CancelFreeRoamLeave(pending);
        if ((HostedWorldId ?? Volatile.Read(ref _seatWorld)) is { } world) _log($"worlds: the game joined again; {world} stays");
    }

    /// <summary>A pending close or leave after free roam is called off (the world, or the player's own choice, stays).</summary>
    private void CancelFreeRoamLeave() => CancelFreeRoamLeave(Interlocked.Exchange(ref _freeRoamLeave, null));

    private static void CancelFreeRoamLeave(CancellationTokenSource? pending)
    {
        try { pending?.Cancel(); }
        catch (ObjectDisposedException) { } // its wait already ended
        catch (AggregateException) { }
    }

    private static string KindName(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => "car meet",
        FreeRoamKind.RoadTrip => "road trip",
        _ => "free roam",
    };

    // the world a FREE ROAM ONLINE ask stays in, for the log
    private string Staying(string? why = null) => HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role == UiWorldRole.Hosting
        ? $"stay in {HostedWorldId ?? "its own world"} (hosted)"
        : $"stay in {Volatile.Read(ref _seatWorld) ?? "its seat"} (seated{(why is null ? string.Empty : ", " + why)})";

    private byte Decided(byte kind, byte result, string what)
    {
        _log($"worlds: {KindName(kind)}: {what}");
        if (result == FreeRoamResult.Ready)
        {
            Interlocked.Exchange(ref _lobbyReadyTicks, Now.UtcTicks); // the next leave of free roam is this lobby's
            ArmGiveUp(kind); // a seat that never played gets SeatGiveUp to start playing; hosting never
        }
        return result;
    }

    /// <summary>
    /// FREE ROAM ONLINE is about to ask for its world (the hook's <c>FreeRoamNeed</c>, CR-L40 §3.2): a seat or a
    /// hosted world answers at once; else the closest matching lobby is joined, else an <c>everyone</c> lobby is opened.
    /// Concurrent asks share one run.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(CancellationToken cancellation) => EnsureFreeRoamWorldAsync(FreeRoamKind.FreeRoam, cancellation);

    /// <summary>
    /// As <see cref="EnsureFreeRoamWorldAsync(CancellationToken)"/> for a car meet or an online Road Trip (<see cref="FreeRoamKind"/>):
    /// the world must run that lobby (its <see cref="WorldView.Lobbies"/>), so a seat in one that does not is left first and
    /// only such worlds are joined; a world of its own runs both. Asks of one kind share a run; another kind waits for it.
    /// OD-150: a free roam or Road Trip ask joins only worlds running that activity now (and says so in its join), else opens
    /// one with it; a car meet ask any activity. A seat is kept whatever its world runs now: the host decides (OD-151).
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(byte kind, CancellationToken cancellation)
    {
        CancelFreeRoamLeave(); // the game left free roam for this ask: its world stays and is weighed here
        if (!OnlineOpen) return Task.FromResult(Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)"));
        if (HasFreeRoamWorld && (kind == FreeRoamKind.FreeRoam || HostsOrOpensWorld) && UnprovenOtherSeat(kind) is null)
            return Task.FromResult(Decided(kind, FreeRoamResult.Ready, Staying()));
        lock (_freeRoamState)
        {
            if (_freeRoamRun is { IsCompleted: false } running)
            {
                if (_freeRoamRunKind == kind) return running.WaitAsync(cancellation);
                _freeRoamRunKind = kind;
                return _freeRoamRun = AfterAsync(running, kind, cancellation);
            }
            _freeRoamRunKind = kind;
            return _freeRoamRun = FreeRoamCoreAsync(kind, cancellation);
        }

        async Task<byte> AfterAsync(Task<byte> previous, byte next, CancellationToken token)
        {
            try { await previous.WaitAsync(token).ConfigureAwait(false); }
            catch (Exception error) when (error is not OperationCanceledException || !token.IsCancellationRequested) { }
            return await FreeRoamCoreAsync(next, token).ConfigureAwait(false);
        }
    }

    private static string? LobbyOf(byte kind) => kind switch
    {
        FreeRoamKind.CarMeet => LobbyKinds.CarMeet,
        FreeRoamKind.RoadTrip => LobbyKinds.RoadTrip,
        _ => null,
    };

    // a seat (reserved or seated) or a world of its own, as this bridge last knew them; no request
    private bool HasFreeRoamWorld => _seat.GrantId is not null || HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role != UiWorldRole.None;

    private async Task<byte> FreeRoamCoreAsync(byte kind, CancellationToken cancellation)
    {
        var lobby = LobbyOf(kind);
        var activity = ActivityOf(kind);
        if (HasFreeRoamWorld && (lobby is null || HostsOrOpensWorld) && UnprovenOtherSeat(kind) is null) return Decided(kind, FreeRoamResult.Ready, Staying());
        try
        {
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
                return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, "no world (online lobbies are not allowed now)");
            if (WireMods().Refusal is { } refusal) return Decided(kind, FreeRoamResult.None, $"no world (mods: {refusal})");
            // test.11: a seat taken for another kind that never played is not this ask's world ("stay" left the game on FINDING
            // SESSION): it is let go (not on purpose) and this kind joins or opens as usual. No CancelMatchmaking: this ask is
            // the game's live matchmaking now.
            if (UnprovenOtherSeat(kind) is { } stale)
            {
                var staleWorld = Volatile.Read(ref _seatWorld) ?? "its seat";
                if (await ReleaseUnprovenSeatAsync(stale.Grant).ConfigureAwait(false))
                    _log($"worlds: {KindName(kind)}: dropped the unproven {KindName(stale.Kind)} seat in {staleWorld}; joining again");
            }
            if (lobby is not null && _seat.GrantId is not null)
            {
                // seated in a world whose server does not run this lobby: its ticket would land nowhere, so the seat is let go
                // and a world that runs it is joined or opened. Only a read that shows it: anything unknown keeps the seat.
                if (Volatile.Read(ref _seatWorld) is not { } seatWorld) return Decided(kind, FreeRoamResult.Ready, Staying("its world is not known here"));
                MmResult<WorldView>? view = null;
                try { view = await _mm.GetAsync(seatWorld, cancellation).ConfigureAwait(false); }
                catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { }
                if (view is not { Ok: true, Value: { } seated }) return Decided(kind, FreeRoamResult.Ready, Staying("its world could not be read"));
                if (seated.Lobbies.Contains(lobby)) return Decided(kind, FreeRoamResult.Ready, Staying($"it runs {lobby}"));
                await LeaveForLobbyAsync(cancellation).ConfigureAwait(false);
                _log($"worlds: {KindName(kind)}: left {seatWorld}: it does not run {lobby}; finding a world that does");
            }
            var local = ModSet.State;
            List<(WorldView World, LobbyRow Row)> candidates = [];
            try
            {
                var read = await ReadLobbiesAsync(WorldFilters.Every, notFull: true, cancellation).ConfigureAwait(false);
                // open lobbies may exist: a transient failure never splits players into a new one
                if (read.FailureKey is { } failure) return Decided(kind, FreeRoamResult.None, $"no world (the lobby list failed: {failure})");
                // matching rows the player enters without a code: everyone's or Supporters' (the server's canJoin; OD-129 opens both to every ticket)
                candidates = OrderLobbies(read.Worlds.Select(w => (w.World, SessionRow(w.World, w.Locked, local))))
                    .Where(r => Matching(r.Row) && !r.World.NeedsCode
                        && r.World.Audience is WorldAudiences.Everyone or WorldAudiences.Supporters
                        && (lobby is null || r.World.Lobbies.Contains(lobby))
                        // OD-150: a free roam or Road Trip ask joins a world running that now; a car meet ask any world that runs one
                        && (activity is null || r.World.Activity == activity))
                    .Take(FreeRoamJoinAttempts).ToList();
            }
            catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested)
            {
                return Decided(kind, FreeRoamResult.None, $"no world (the lobby list failed: {error.GetType().Name})");
            }
            // the world the game left moments ago comes first (whatever its audience), so a retry never splits the players into a new lobby
            if (Interlocked.Exchange(ref _recentlyLeft, null) is { } recent && Now - recent.LeftAt <= RejoinWindow && !HasFreeRoamWorld
                && (lobby is null || candidates.Any(c => c.World.Id == recent.WorldId)))
            {
                cancellation.ThrowIfCancellationRequested();
                var back = await JoinAsync(new UiFrame.JoinWorld(recent.WorldId, string.Empty), activity, cancellation, kind).ConfigureAwait(false);
                if (back.Result == UiJoin.Reserved)
                    return Decided(kind, FreeRoamResult.Ready, $"joined {recent.WorldId} (back in the world it left {(int)(Now - recent.LeftAt).TotalSeconds} s ago)");
                _log($"worlds: {KindName(kind)}: {recent.WorldId}, left {(int)(Now - recent.LeftAt).TotalSeconds} s ago, refused ({back.Result})");
                candidates = [.. candidates.Where(c => c.World.Id != recent.WorldId)];
            }
            // a world the player left on purpose is not where its next lobby search sends it
            if (Volatile.Read(ref _leftOnPurpose) is { } avoided && Now - avoided.LeftAt <= RejoinWindow)
                candidates = [.. candidates.Where(c => c.World.Id != avoided.WorldId)];
            foreach (var (world, _) in candidates)
            {
                if (HasFreeRoamWorld) return Decided(kind, FreeRoamResult.Ready, Staying());
                cancellation.ThrowIfCancellationRequested(); // the hook's wait is bounded: no join or open past it
                var joined = await JoinAsync(new UiFrame.JoinWorld(world.Id, string.Empty), activity, cancellation, kind).ConfigureAwait(false);
                if (joined.Result == UiJoin.Reserved)
                {
                    FreeRoamToast("worlds.free_roam_joined", world.Name);
                    return Decided(kind, FreeRoamResult.Ready, $"joined {world.Id}");
                }
                if (joined.Result is not (UiJoin.Full or UiJoin.Closed))
                {
                    // the player's own join in flight (SlowDown), a seat this bridge does not know (already in a
                    // lobby), access or mods: opening a lobby now could leave the profile both seated and hosting
                    return HasFreeRoamWorld ? Decided(kind, FreeRoamResult.Ready, Staying($"join of {world.Id} refused ({joined.Result})"))
                        : Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, $"no world (join of {world.Id} refused: {joined.Result})");
                }
                _log($"worlds: {KindName(kind)}: {world.Id} refused ({joined.Result}); trying the next lobby");
            }
            if (HasFreeRoamWorld) return Decided(kind, FreeRoamResult.Ready, Staying());
            if (Volatile.Read(ref _joinAttempt) != 0) return Decided(kind, FreeRoamResult.None, "no world (the player's own join is in flight)");
            if (!OnlineOpen) return Decided(kind, FreeRoamResult.NotOnline, "no world (online is closed)");
            cancellation.ThrowIfCancellationRequested();
            var name = DefaultLobbyName(_text("worlds.default_name"), Gamertag);
            var hosted = await CreateAsync(new UiFrame.CreateWorld(name, 0, FreeRoamPlayers, 0), activity, cancellation).ConfigureAwait(false);
            if (hosted.Result == 0)
            {
                FreeRoamToast("worlds.free_roam_opened", name);
                return Decided(kind, FreeRoamResult.Ready, $"opened {HostedWorldId ?? name}{(candidates.Count == 0 ? " (no matching lobby)" : " (every matching lobby refused)")}");
            }
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, $"no world (no lobby opened: {hosted.Result})");
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested || !OnlineOpen)
        {
            return Decided(kind, OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline, OnlineOpen ? "no world (the game stopped waiting)" : "no world (online is closed)");
        }
    }

    /// <summary>The seat is let go to find a world that runs the asked lobby (not "on purpose": that world may come back later).</summary>
    private async Task LeaveForLobbyAsync(CancellationToken cancellation)
    {
        InvalidateMine(reset: true);
        Volatile.Write(ref _seatWorld, null);
        _seat.Left();
        await ClearSeatEndAsync().ConfigureAwait(false); // no relay leg outlives the seat
        try { Observe(await _mm.LeaveSeatAsync(cancellation).ConfigureAwait(false)); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { } // the seat lapses on its own
    }

    private void FreeRoamToast(string key, string lobby) =>
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text(key).Replace("{lobby}", lobby, StringComparison.Ordinal)));

    // OD-151: the activity of the seat's world as last read (_mineState only); null: none read for this seat yet
    private (string WorldId, string Activity)? _seatActivity;

    /// <summary>
    /// OD-151: a seat's world changed activity (its host's game went into a Road Trip or back to free roam): the guest follows
    /// the host, so the seat stays and a toast says what the host runs now. The first read of a seat only records it; the
    /// host's own world never toasts. Called under <c>_mineState</c> with each seat read and renewal.
    /// </summary>
    private void ObserveActivity(Seat seat)
    {
        if (seat.State == SeatStates.Hosting || seat.World.Id != seat.WorldId) { _seatActivity = null; return; }
        var activity = seat.World.Activity;
        if (activity is not (WorldActivities.FreeRoam or WorldActivities.RoadTrip)) return; // a value this launcher does not know
        if (_seatActivity is { } last && last.WorldId == seat.WorldId && last.Activity != activity)
        {
            _log($"worlds: activity {last.Activity} -> {activity} in {seat.WorldId} (the host's)");
            _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60,
                _text("worlds.host_activity").Replace("{mode}", ActivityName(activity), StringComparison.Ordinal)));
            // OD-151 follow-up: the guest's game starts its own matchmaking for the host's activity (the player confirms with (X))
            Volatile.Write(ref _followHost, new Session.Frame.FollowHost(
                activity == WorldActivities.RoadTrip ? Session.FollowHostLobby.RoadTrip : Session.FollowHostLobby.FreeRoam, Public: true));
        }
        _seatActivity = (seat.WorldId, activity);
    }

    // OD-151 follow-up: the FollowHost frame of the last activity change, not sent yet (the latest change wins)
    private Session.Frame.FollowHost? _followHost;

    /// <summary>Writes <see cref="Session.Frame.FollowHost"/> to the hook (the supervisor's control pipe).</summary>
    public Func<Session.Frame.FollowHost, CancellationToken, Task>? SendFollowHost { get; set; }

    /// <summary>
    /// OD-151 follow-up: sends the pending <see cref="Session.Frame.FollowHost"/> of a seat's activity change, once. Taken whether or not
    /// it can be written: a change is never replayed later (a closed or suspended session, or no hook writer, drops it with a log line).
    /// </summary>
    public async Task FollowHostAsync(CancellationToken cancellation)
    {
        if (Interlocked.Exchange(ref _followHost, null) is not { } follow) return;
        var name = follow.LobbyType == Session.FollowHostLobby.RoadTrip ? "Road Trip" : "free roam";
        if (!OnlineOpen || SendFollowHost is null) { _log($"worlds: the host is in {name}; online is closed here, the game is not asked to follow"); return; }
        try
        {
            await SendFollowHost(follow, cancellation).ConfigureAwait(false);
            _log($"worlds: the host is in {name}; the game is asked to look for it (FollowHost, the player confirms)");
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or ObjectDisposedException)
        { _log("worlds: FollowHost not sent (" + error.GetType().Name + ")"); }
    }

    /// <summary>The player-facing name of an activity: "Online Free Roam" or "Online Road Trip" (both languages).</summary>
    /// <summary>What a world ran when the lobby list last showed it (no extra read); null when it was not listed.</summary>
    private string? ListedActivity(string worldId) => _listedActivity.TryGetValue(worldId, out var activity) ? activity : null;

    private string ActivityName(string activity) =>
        _text(activity == WorldActivities.RoadTrip ? "worlds.activity_roadtrip" : "worlds.activity_freeroam");

    // OD-150: the activity a lobby ask joins by and opens with; a car meet does not change it, so it asks for none
    private static string? ActivityOf(byte kind) => kind switch
    {
        FreeRoamKind.FreeRoam => WorldActivities.FreeRoam,
        FreeRoamKind.RoadTrip => WorldActivities.RoadTrip,
        _ => null,
    };

    /// <summary>
    /// CR-L40 §4: <paramref name="template"/> (<c>{gamertag}'s lobby</c>, <c>Lobby de {gamertag}</c>) with the gamertag
    /// cut on a whole code point so the name stays within <see cref="MaxLobbyName"/> characters. No gamertag: "Infinity".
    /// </summary>
    public static string DefaultLobbyName(string? template, string? gamertag)
    {
        const string Placeholder = "{gamertag}";
        var tag = gamertag?.Trim();
        if (string.IsNullOrEmpty(tag)) return "Infinity";
        if (string.IsNullOrEmpty(template) || !template.Contains(Placeholder, StringComparison.Ordinal)) template = Placeholder + "'s lobby";
        var room = MaxLobbyName - (template.Length - Placeholder.Length);
        if (room <= 0) return tag.Length <= MaxLobbyName ? tag : Cut(tag, MaxLobbyName);
        return template.Replace(Placeholder, tag.Length <= room ? tag : Cut(tag, room), StringComparison.Ordinal);

        static string Cut(string text, int chars)
        {
            var builder = new StringBuilder();
            foreach (var rune in text.EnumerateRunes())
            {
                if (builder.Length + rune.Utf16SequenceLength > chars) break;
                builder.Append(rune.ToString());
            }
            return builder.ToString().TrimEnd();
        }
    }
}
