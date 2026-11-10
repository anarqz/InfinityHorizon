// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 ("NEVER [...] a freezed screen, even for 1s"): a list the window shows is changed in place, never cleared and refilled.
/// An item the window already draws keeps its container (its card's visual tree is not built again); a changed item replaces only its own;
/// a moved one moves. Clearing a 47-card list and adding it back rebuilt every card on the UI thread at each refresh. Many new items at
/// once (the search cleared, the game switched back) are added a few per frame (<paramref name="later"/>), so no single layout pass builds
/// them all.
/// </summary>
internal static class ListSync
{
    /// <summary>
    /// Makes <paramref name="target"/> hold <paramref name="items"/>, in order. <paramref name="same"/>: the shown item can stay as it is
    /// (nothing it displays changed); <paramref name="key"/>: the two stand for the same thing (a changed one is replaced in its place).
    /// With <paramref name="later"/>, at most <paramref name="budget"/> new items are added now and the rest is handed to it (the caller
    /// runs the same sync again on a later frame); without, everything is added now.
    /// </summary>
    internal static void Sync<T>(ObservableCollection<T> target, IReadOnlyList<T> items, Func<T, T, bool> same, Func<T, T, bool> key,
        Action? later = null, int budget = 8)
    {
        ArgumentNullException.ThrowIfNull(target);
        ArgumentNullException.ThrowIfNull(items);
        // what is shown and no longer wanted goes first (cheap), so the order below only inserts, moves and replaces
        for (var k = target.Count - 1; k >= 0; k--)
            if (!items.Any(want => key(target[k], want))) target.RemoveAt(k);
        var built = 0;
        for (var i = 0; i < items.Count; i++)
        {
            var want = items[i];
            if (i < target.Count && same(target[i], want)) continue;
            var found = -1;
            for (var k = i; k < target.Count; k++)
                if (key(target[k], want)) { found = k; break; }
            // a new or changed item builds its drawing: past the budget, the rest waits for the next frame
            var builds = found < 0 || !same(target[found], want);
            if (builds && later is not null && built >= budget) { later(); return; }
            if (found < 0) { target.Insert(i, want); built++; continue; }
            if (found != i) target.Move(found, i);
            if (builds) { target[i] = want; built++; }
        }
        while (target.Count > items.Count) target.RemoveAt(target.Count - 1);
    }
}
