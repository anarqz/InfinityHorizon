// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Windows.Media;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (the window never freezes): when the Profiles cards and rows are rebuilt (every /me, game and settle), a card whose
/// content did not change keeps the one the window draws (<see cref="ListSync"/>): re-adding the same four profile cards cost ~80 ms of
/// layout, on any page. "The same" is by content: lists item by item, brushes by colour (each rebuild makes new list and brush objects).
/// </summary>
public sealed partial class ShellViewModel
{
    internal static bool SameBrush(Brush? a, Brush? b) => ReferenceEquals(a, b)
        || a is SolidColorBrush x && b is SolidColorBrush y && x.Color == y.Color && x.Opacity.Equals(y.Opacity);

    internal static bool SameBoardCard(BoardCard? a, BoardCard? b) => ReferenceEquals(a, b)
        || a is not null && b is not null && a.Offers.Sources.SequenceEqual(b.Offers.Sources)
        && (a with { Offers = a.Offers with { Sources = b.Offers.Sources } }).Equals(b);

    internal static bool SameProfileRow(ProfileRow? a, ProfileRow? b) => ReferenceEquals(a, b)
        || a is not null && b is not null && SameBoardCard(a.Card, b.Card) && (a with { Card = b.Card }).Equals(b);

    internal static bool SameGameRow(ProfileGameRow a, ProfileGameRow b) => ReferenceEquals(a, b)
        || SameBrush(a.Accent, b.Accent) && SameBrush(a.Soft, b.Soft) && a.Menu.SequenceEqual(b.Menu)
        && (a with { Accent = b.Accent, Soft = b.Soft, Menu = b.Menu }).Equals(b);

    internal static bool SameSaveCard(SaveCard a, SaveCard b) => ReferenceEquals(a, b)
        || SameProfileRow(a.Row, b.Row) && a.Menu.SequenceEqual(b.Menu) && a.Games.Count == b.Games.Count
        && a.Games.Zip(b.Games).All(g => SameGameRow(g.First, g.Second))
        && (a with { Row = b.Row, Menu = b.Menu, Games = b.Games }).Equals(b);

    /// <summary>
    /// The plain Profiles list as the window draws it (offline, or a server before CR-S98): <see cref="ProfileRows"/> is the data every
    /// path reads at once; this copy follows it a couple of rows per frame (a game switch replaces every row, ~30 ms of layout even while
    /// the page is hidden). OD-240: HARDCORE rows only; AZIMUTH is one identity behind the scenes, never a row.
    /// </summary>
    public System.Collections.ObjectModel.ObservableCollection<ProfileRow> ProfileRowsShown { get; } = [];

    private bool _profileRowsShownPending;

    private void ScheduleProfileRowsShown()
    {
        if (_profileRowsShownPending) return;
        if (Later(SyncProfileRowsShown) is not { } later) { ListSync.Sync(ProfileRowsShown, [.. ProfileRows.Where(r => r.Hardcore)], SameProfileRow, SameProfileRowKey); return; }
        _profileRowsShownPending = true;
        later();
    }

    private void SyncProfileRowsShown()
    {
        _profileRowsShownPending = false;
        ListSync.Sync(ProfileRowsShown, [.. ProfileRows.Where(r => r.Hardcore)], SameProfileRow, SameProfileRowKey, Later(ScheduleProfileRowsShown), budget: 2);
    }

    /// <summary>A card stands for one mode's profile (or that mode's explainer).</summary>
    private static bool SameSaveCardKey(SaveCard a, SaveCard b) => a.Slot == b.Slot && a.Row?.Card?.Player.ProfileId == b.Row?.Card?.Player.ProfileId
        && a.Row?.Profile.ProfileId == b.Row?.Profile.ProfileId;

    private static bool SameProfileRowKey(ProfileRow a, ProfileRow b) => a.Profile.ProfileId == b.Profile.ProfileId && a.Profile.Game == b.Profile.Game
        && a.Card?.Player.ProfileId == b.Card?.Player.ProfileId;
}
