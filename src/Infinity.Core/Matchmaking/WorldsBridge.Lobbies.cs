// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// CR-L40 (docs/launcher/LOBBIES_AND_SERVER_STATUS.md §2.2): every lobby of the game in one list. The
// launcher reads `filter=all` (an older server: two global reads, vanilla and modded, merged by id),
// orders it (matching rows first, then the others with their reason) and fits it to one UI frame.
public sealed partial class WorldsBridge
{
    /// <summary>What a list read gave: the worlds (each with whether it came from the Supporter section) or the <c>worlds.*</c> key of why not.</summary>
    private sealed record LobbyRead(string? FailureKey, IReadOnlyList<(WorldView World, bool Locked)> Worlds);

    /// <summary>
    /// The lobbies of <paramref name="filter"/>, modded and vanilla. <see cref="WorldFilters.Every"/> falls back to
    /// two <c>global</c> reads when the server refuses it (400 <c>{field: "filter"}</c>); the modded read of a
    /// pair may fail on its own (a server before OD-112 refuses <c>modded</c>), the vanilla rows still show.
    /// </summary>
    private async Task<LobbyRead> ReadLobbiesAsync(string filter, bool notFull, CancellationToken cancellation)
    {
        static LobbyRead Failed(MmResult<WorldList>? answer) =>
            new(answer?.Error?.Kind == MmErrorKind.SupporterRequired ? "worlds.friends_supporters" : "worlds.list_failed", []);
        static IEnumerable<(WorldView, bool)> Of(WorldList list) =>
            list.Items.Select(w => (w, false)).Concat(list.Locked.Select(w => (w, true)));
        if (filter == WorldFilters.Every)
        {
            var all = await OperationAsync(_listRetry, token => _mm.ListAsync(WorldFilters.Every, notFull, false, token), cancellation).ConfigureAwait(false);
            if (all is { Ok: true, Value: { } every }) return new(null, Of(every).ToList());
            if (all?.Error is not { Kind: MmErrorKind.Invalid, Field: "filter" }) return Failed(all);
            _log("worlds: the server has no all filter; two global reads instead");
            filter = WorldFilters.Global;
        }
        var vanilla = await OperationAsync(_listRetry, token => _mm.ListAsync(filter, notFull, false, token), cancellation).ConfigureAwait(false);
        if (vanilla is not { Ok: true, Value: { } first }) return Failed(vanilla);
        var worlds = Of(first).ToList();
        var modded = await OperationAsync(_listRetry, token => _mm.ListAsync(filter, notFull, true, token), cancellation).ConfigureAwait(false);
        if (modded is { Ok: true, Value: { } second })
        {
            var seen = worlds.Select(w => w.Item1.Id).ToHashSet(StringComparer.Ordinal);
            worlds.AddRange(Of(second).Where(w => seen.Add(w.Item1.Id)));
        }
        else _log($"worlds: the modded list was not read ({modded?.Error?.Kind.ToString() ?? "skipped"}); vanilla lobbies only");
        return new(null, worlds);
    }

    /// <summary>A protocol 2 row of this session's view: another mode's world is <see cref="UiLobby.LockHardcore"/> / <see cref="UiLobby.LockNoob"/>.</summary>
    private LobbyRow SessionRow(WorldView world, bool locked, ModSetState local)
    {
        var row = LobbyRow(world, locked, local);
        return string.Equals(world.Mode, _mode, StringComparison.Ordinal) ? row
            : row with { World = row.World with { Lock = ModeLock(world.Mode) } };
    }

    private static byte ModeLock(string? worldMode) =>
        string.Equals(worldMode, "hardcore", StringComparison.Ordinal) ? UiLobby.LockHardcore : UiLobby.LockNoob;

    /// <summary>A row this session can join as it is: its own mode, the same mods, and the server lets it in.</summary>
    public static bool Matching(LobbyRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        return row.World.Lock == 0 && row.ModsMatch;
    }

    /// <summary>
    /// CR-L40 §2.2 order: matching rows first, then the others; inside each, the estimated round trip
    /// ascending (unknown last), then players descending; equal rows keep the server's order.
    /// </summary>
    public static List<(WorldView World, LobbyRow Row)> OrderLobbies(IEnumerable<(WorldView World, LobbyRow Row)> rows) =>
        rows.OrderBy(r => Matching(r.Row) ? 0 : 1)
            .ThenBy(r => r.World.RttMs is null ? 1 : 0)
            .ThenBy(r => r.World.RttMs ?? 0)
            .ThenByDescending(r => r.World.Players)
            .ToList();

    /// <summary>
    /// The rows that fit one <see cref="UiFrame.WorldsListV2"/> with <paramref name="message"/>: at most
    /// <see cref="UiFrame.MaxLobbyRows"/>; past 64 KiB the players and mods of the last rows are dropped first
    /// (their pane then shows the host only), and only then the last rows themselves.
    /// </summary>
    public static IReadOnlyList<LobbyRow> FitLobbyRows(IReadOnlyList<LobbyRow> rows, string message)
    {
        ArgumentNullException.ThrowIfNull(rows);
        static int Str(string text) => 2 + Encoding.UTF8.GetByteCount(text);
        static int Size(LobbyRow r) => Str(r.World.Id) + Str(r.World.Name) + Str(r.World.Host) + 6 + 3
            + 1 + r.Members.Sum(m => Str(m.Gamertag) + 2) + 1 + r.Mods.Sum(m => Str(m.Id) + Str(m.Version));
        var fitted = rows.Take(UiFrame.MaxLobbyRows).ToList();
        // type, filter, modded, status, message, count
        var budget = Wire.Frames.MaxFrame - (1 + 3 + Str(message ?? string.Empty) + 1);
        var total = fitted.Sum(Size);
        for (var i = fitted.Count - 1; i >= 0 && total > budget; --i)
        {
            if (fitted[i].Members.Count == 0 && fitted[i].Mods.Count == 0) continue;
            var bare = fitted[i] with { Members = [], Mods = [] };
            total += Size(bare) - Size(fitted[i]);
            fitted[i] = bare;
        }
        while (fitted.Count > 0 && total > budget)
        {
            total -= Size(fitted[^1]);
            fitted.RemoveAt(fitted.Count - 1);
        }
        return fitted;
    }
}
