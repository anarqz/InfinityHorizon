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

    /// <summary>
    /// FREE ROAM ONLINE is about to ask for its world (the hook's <c>FreeRoamNeed</c>, CR-L40 §3.2): a seat or a
    /// hosted world answers at once; else the closest matching lobby is joined, else an <c>everyone</c> lobby is opened.
    /// Concurrent asks share one run.
    /// </summary>
    public Task<byte> EnsureFreeRoamWorldAsync(CancellationToken cancellation)
    {
        if (!OnlineOpen) return Task.FromResult(FreeRoamResult.NotOnline);
        if (HasFreeRoamWorld) return Task.FromResult(FreeRoamResult.Ready);
        lock (_freeRoamState)
        {
            if (_freeRoamRun is { IsCompleted: false } running) return running.WaitAsync(cancellation);
            return _freeRoamRun = FreeRoamCoreAsync(cancellation);
        }
    }

    // a seat (reserved or seated) or a world of its own, as this bridge last knew them; no request
    private bool HasFreeRoamWorld => _seat.GrantId is not null || HostsOrOpensWorld || Volatile.Read(ref _mineSnapshot).Role != UiWorldRole.None;

    private async Task<byte> FreeRoamCoreAsync(CancellationToken cancellation)
    {
        try
        {
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false })
            {
                _log("worlds: free roam: online lobbies are not allowed now");
                return OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline;
            }
            if (WireMods().Refusal is { } refusal)
            {
                _log($"worlds: free roam: no lobby yet ({refusal})");
                return FreeRoamResult.None;
            }
            var local = ModSet.State;
            IReadOnlyList<(WorldView World, LobbyRow Row)> candidates = [];
            try
            {
                var read = await ReadLobbiesAsync(WorldFilters.Every, notFull: true, cancellation).ConfigureAwait(false);
                if (read.FailureKey is { } failure)
                {
                    // open lobbies may exist: a transient failure never splits players into a new one
                    _log($"worlds: free roam: the lobby list failed ({failure})");
                    return FreeRoamResult.None;
                }
                // matching rows the player enters without a code: everyone's or Supporters' (the server's canJoin; OD-129 opens both to every ticket)
                candidates = OrderLobbies(read.Worlds.Select(w => (w.World, SessionRow(w.World, w.Locked, local))))
                    .Where(r => Matching(r.Row) && !r.World.NeedsCode
                        && r.World.Audience is WorldAudiences.Everyone or WorldAudiences.Supporters)
                    .Take(FreeRoamJoinAttempts).ToList();
            }
            catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested)
            {
                _log($"worlds: free roam: the lobby list failed ({error.GetType().Name})");
                return FreeRoamResult.None;
            }
            // the world the game left moments ago comes first (whatever its audience), so a retry never splits the players into a new lobby
            if (Interlocked.Exchange(ref _recentlyLeft, null) is { } recent && Now - recent.LeftAt <= RejoinWindow && !HasFreeRoamWorld)
            {
                cancellation.ThrowIfCancellationRequested();
                var back = await JoinAsync(new UiFrame.JoinWorld(recent.WorldId, string.Empty), cancellation).ConfigureAwait(false);
                if (back.Result == UiJoin.Reserved)
                {
                    _log($"worlds: free roam: back in {recent.WorldId}, the world it left {(int)(Now - recent.LeftAt).TotalSeconds} s ago");
                    return FreeRoamResult.Ready;
                }
                _log($"worlds: free roam: {recent.WorldId}, left {(int)(Now - recent.LeftAt).TotalSeconds} s ago, refused ({back.Result})");
                candidates = [.. candidates.Where(c => c.World.Id != recent.WorldId)];
            }
            // a world the player left on purpose is not where its next lobby search sends it
            if (Volatile.Read(ref _leftOnPurpose) is { } avoided && Now - avoided.LeftAt <= RejoinWindow)
                candidates = [.. candidates.Where(c => c.World.Id != avoided.WorldId)];
            foreach (var (world, _) in candidates)
            {
                if (HasFreeRoamWorld) return FreeRoamResult.Ready;
                cancellation.ThrowIfCancellationRequested(); // the hook's wait is bounded: no join or open past it
                var joined = await JoinAsync(new UiFrame.JoinWorld(world.Id, string.Empty), cancellation).ConfigureAwait(false);
                if (joined.Result == UiJoin.Reserved)
                {
                    _log($"worlds: free roam: joined {world.Id}");
                    FreeRoamToast("worlds.free_roam_joined", world.Name);
                    return FreeRoamResult.Ready;
                }
                if (joined.Result is not (UiJoin.Full or UiJoin.Closed))
                {
                    // the player's own join in flight (SlowDown), a seat this bridge does not know (already in a
                    // lobby), access or mods: opening a lobby now could leave the profile both seated and hosting
                    _log($"worlds: free roam: join of {world.Id} refused ({joined.Result})");
                    return HasFreeRoamWorld ? FreeRoamResult.Ready : OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline;
                }
            }
            if (HasFreeRoamWorld || Volatile.Read(ref _joinAttempt) != 0) return HasFreeRoamWorld ? FreeRoamResult.Ready : FreeRoamResult.None;
            if (!OnlineOpen) return FreeRoamResult.NotOnline;
            cancellation.ThrowIfCancellationRequested();
            var name = DefaultLobbyName(_text("worlds.default_name"), Gamertag);
            var hosted = await CreateAsync(new UiFrame.CreateWorld(name, 0, FreeRoamPlayers, 0), cancellation).ConfigureAwait(false);
            if (hosted.Result == 0)
            {
                FreeRoamToast("worlds.free_roam_opened", name);
                return FreeRoamResult.Ready;
            }
            _log($"worlds: free roam: no lobby opened ({hosted.Result})");
            return OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline;
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested || !OnlineOpen)
        {
            return OnlineOpen ? FreeRoamResult.None : FreeRoamResult.NotOnline;
        }
    }

    private void FreeRoamToast(string key, string lobby) =>
        _freeRoamToasts.Enqueue(new UiFrame.Toast(UiLevel.Info, 1, 60, _text(key).Replace("{lobby}", lobby, StringComparison.Ordinal)));

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
