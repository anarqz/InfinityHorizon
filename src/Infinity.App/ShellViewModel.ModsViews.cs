// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// Alex 2026-11-12: "What about we have a separated 'page' showing the manually installed mods? People tend to install a lot of manual
/// mods." The Mods page has two views, switched from its toolbar: Catalog (the catalog's cards and sets) and Manual (the mods copied into
/// the chosen game's mods folder by hand, as compact rows). The view is remembered for the session and follows the chosen game; the search
/// filters whichever view is shown.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The view shown: false Catalog, true Manual. Session only.</summary>
    private bool _manualModsView;

    /// <summary>The views exist where the chosen game has a mods folder of ExtraOptions (OD-233); otherwise the page is the catalog.</summary>
    public bool ShowModsViews => _install is not null && ExtraOptionsLayout.HasModsFolder(CurrentGame.Id);
    public bool ShowManualView => ShowModsViews && _manualModsView;
    public bool ShowCatalogView => !ShowManualView;
    public string ManualViewLabel => S.Format("mods.view_manual", ("count", ExtraOptionsModItems.Count.ToString(CultureInfo.CurrentCulture)));

    public void SelectModsView(bool manual)
    {
        if (_manualModsView == manual) return;
        _manualModsView = manual;
        ChangedModsView();
    }

    /// <summary>The search filters this view too (name, folder and details).</summary>
    public IEnumerable<ExtraOptionsModItem> FilteredExtraOptionsModItems => string.IsNullOrWhiteSpace(_modSearch) ? ExtraOptionsModItems
        : ExtraOptionsModItems.Where(i => i.Name.Contains(_modSearch.Trim(), StringComparison.CurrentCultureIgnoreCase)
            || i.Folder.Contains(_modSearch.Trim(), StringComparison.CurrentCultureIgnoreCase) || i.Details.Contains(_modSearch.Trim(), StringComparison.CurrentCultureIgnoreCase));
    public bool NoMatchingManualMods => ShowManualView && HasExtraOptionsMods && !FilteredExtraOptionsModItems.Any();
    public bool ShowManualEmpty => ShowManualView && !HasExtraOptionsMods;
    public string ManualModsEmpty => S.Format("eomods.empty", ("game", GameLabel));

    /// <summary>The search shows over a view that has something to search.</summary>
    public bool ShowModSearch => ShowManualView ? HasExtraOptionsMods : HasModCards;

    /// <summary>The view's one muted line; its tooltip has the full explanation, the online note and the folder's path.</summary>
    public string ManualModsHintShort => S[ProfileHardcore ? "eomods.hint_short_hardcore" : "eomods.hint_short"];
    public string ManualModsHintTooltip => string.Join("\n", new[] { ExtraOptionsModsHint, S["eomods.manual_online"], ExtraOptionsModsFolder }.Where(s => s.Length > 0));

    /// <summary>"Open folder": the chosen game's mods folder in Explorer (created first when it is not there yet).</summary>
    public void OpenManualModsFolder()
    {
        if (_install is not { } install || !ExtraOptionsLayout.HasModsFolder(CurrentGame.Id)) return;
        var folder = ExtraOptionsMods.ModsDirectory(install.Root, CurrentGame.Id);
        try
        {
            Directory.CreateDirectory(folder);
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", "\"" + folder + "\"") { UseShellExecute = true })?.Dispose();
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            Log("mods: folder not opened: " + e.GetType().Name);
            Message = S["eomods.folder_not_opened"];
        }
    }

    /// <summary>A row's ⓘ pressed: its information in full in the message (the tooltip shows it on hover).</summary>
    public void ShowManualModInformation(ExtraOptionsModItem item)
    {
        ArgumentNullException.ThrowIfNull(item);
        Message = item.Info;
    }

    private void ChangedModsView()
    {
        foreach (var name in new[] { nameof(ShowModsViews), nameof(ShowManualView), nameof(ShowCatalogView), nameof(ManualViewLabel), nameof(FilteredExtraOptionsModItems),
                     nameof(NoMatchingManualMods), nameof(ShowManualEmpty), nameof(ManualModsEmpty), nameof(ShowModSearch), nameof(ManualModsHintShort), nameof(ManualModsHintTooltip),
                     nameof(NoMatchingMods), nameof(ShowModsEmptyForGame) })
            Changed(name);
    }
}
