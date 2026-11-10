// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (the window never freezes): the Mods page's cards are one list the window keeps (<see cref="ListSync"/>). Every
/// "every property changed" notification used to hand the page a new filtered sequence, and the page rebuilt all its 47 cards (about
/// 150 ms of layout each time, on any page). Now the shown list changes in place: a card that did not change keeps its drawing.
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly ObservableCollection<ModCard> _shownModCards = [];
    private readonly ObservableCollection<ModSetCard> _shownModSetCards = [];

    private IReadOnlyList<ModCard>? _modCardsWanted;
    private IReadOnlyList<ModSetCard>? _modSetCardsWanted;

    /// <summary><paramref name="wanted"/> in the one list the window shows (changed in place; many new cards a few per frame).</summary>
    private ObservableCollection<ModCard> ShownModCards(IEnumerable<ModCard> wanted)
    {
        IReadOnlyList<ModCard> list = [.. wanted];
        _modCardsWanted = list;
        SyncModCards(list);
        return _shownModCards;
    }

    private void SyncModCards(IReadOnlyList<ModCard> list) =>
        ListSync.Sync(_shownModCards, list, SameModCard, (a, b) => a.Manifest.Id == b.Manifest.Id,
            Later(() => { if (ReferenceEquals(_modCardsWanted, list)) SyncModCards(list); }));

    private ObservableCollection<ModSetCard> ShownModSetCards(IEnumerable<ModSetCard> wanted)
    {
        IReadOnlyList<ModSetCard> list = [.. wanted];
        _modSetCardsWanted = list;
        SyncModSetCards(list);
        return _shownModSetCards;
    }

    private void SyncModSetCards(IReadOnlyList<ModSetCard> list) =>
        ListSync.Sync(_shownModSetCards, list, SameSet, (a, b) => a.Hint == b.Hint,
            Later(() => { if (ReferenceEquals(_modSetCardsWanted, list)) SyncModSetCards(list); }), budget: 2);

    /// <summary>
    /// The rest of a sync on a later frame (after the window drew and took input), when this thread runs the window's dispatcher;
    /// null elsewhere (tests: everything at once).
    /// </summary>
    private static Action? Later(Action rest) =>
        System.Windows.Application.Current?.Dispatcher is { } dispatcher && dispatcher.CheckAccess()
            ? () => dispatcher.BeginInvoke(System.Windows.Threading.DispatcherPriority.Background, rest)
            : null;

    /// <summary>
    /// Nothing the card shows or acts on changed: the same manifest content (a refresh reads the installed manifests again, as new objects)
    /// and every other field equal.
    /// </summary>
    internal static bool SameModCard(ModCard a, ModCard b) =>
        ReferenceEquals(a, b) || a.Manifest.Digest == b.Manifest.Digest && a.Dependency?.Digest == b.Dependency?.Digest && a.Next?.Digest == b.Next?.Digest
        && (a with { Manifest = b.Manifest, Dependency = b.Dependency, Next = b.Next }).Equals(b);

    private static bool SameSet(ModSetCard a, ModSetCard b) =>
        ReferenceEquals(a, b) || a.Hint == b.Hint && a.Summary == b.Summary && a.Notice == b.Notice && a.CanInstallAll == b.CanInstallAll && a.Report == b.Report
        && a.Expanded == b.Expanded && a.Mods.Select(m => m.Digest).SequenceEqual(b.Mods.Select(m => m.Digest)) && a.Packs.Count == b.Packs.Count
        && a.Packs.Zip(b.Packs).All(p => SameModCard(p.First, p.Second));
}
