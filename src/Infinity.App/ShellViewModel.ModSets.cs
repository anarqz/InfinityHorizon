// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// OD-232: one card for a set of catalog mods sharing an external source's hint (the ZS FM5 / FM6 Addon Packs): the hint, how many
/// packs and how many are installed, "Install all" (one folder pick for every pack), and the packs' own cards as rows with their
/// usual actions. The last "Install all" report stays on the card.
/// </summary>
public sealed class ModSetCard(string hint, IReadOnlyList<ModManifest> mods, IReadOnlyList<ModCard> packs, string summary, string notice,
    bool canInstallAll, string report, bool expanded, Action<string, bool> expand) : INotifyPropertyChanged
{
    public string Hint { get; } = hint;
    public string Title => Hint;
    /// <summary>The set's manifests in install order (its Common first).</summary>
    public IReadOnlyList<ModManifest> Mods { get; } = mods;
    public IReadOnlyList<ModCard> Packs { get; } = packs;
    public string Summary { get; } = summary;
    public string Notice { get; } = notice;
    public bool CanInstallAll { get; } = canInstallAll;
    public string Report { get; } = report;
    public bool HasReport => Report.Length > 0;
    private bool _expanded = expanded;
    /// <summary>Kept across refreshes (the cards are rebuilt after every action).</summary>
    public bool Expanded
    {
        get => _expanded;
        set { if (_expanded == value) return; _expanded = value; expand(Hint, value); PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Expanded))); }
    }

    public event PropertyChangedEventHandler? PropertyChanged;
}

public sealed partial class ShellViewModel
{
    private readonly HashSet<string> _expandedSets = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _setReports = new(StringComparer.Ordinal);
    public ObservableCollection<ModSetCard> ModSetCards { get; } = [];
    // owner 2026-11-10: one list the window keeps, changed in place (ShellViewModel.ModCardsView.cs)
    public IEnumerable<ModSetCard> FilteredModSetCards => ShownModSetCards(ModSetCards.Where(set => string.IsNullOrWhiteSpace(_modSearch)
        || set.Title.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase) || set.Packs.Any(MatchesSearch)));
    public bool HasModSetCards => ModSetCards.Count > 0;

    private bool MatchesSearch(ModCard card) => string.IsNullOrWhiteSpace(_modSearch)
        || card.Name.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase)
        || card.Manifest.Id.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase)
        || card.Credits.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase);

    /// <summary>The ids of the mods shown inside a set card (never also as their own card).</summary>
    private HashSet<string> SetMemberIds => ModSetCards.SelectMany(s => s.Packs).Select(p => p.Manifest.Id).ToHashSet(StringComparer.Ordinal);

    /// <summary>Builds the set cards from the current mod cards (after <see cref="RefreshModCards"/> filled them).</summary>
    private void RefreshModSetCards()
    {
        ModSetCards.Clear();
        var game = CurrentGame;
        var catalog = _manifestCatalog.Where(m => m.IsForGame(game.Id) && m.Id != ManagedModFiles.BuiltInExtraOptions).ToArray();
        foreach (var (hint, mods) in ManagedModFiles.ExternalSets(catalog))
        {
            var packs = mods.Select(m => ModCards.FirstOrDefault(c => c.Manifest.Id == m.Id)).OfType<ModCard>().ToList();
            if (packs.Count < 2) continue;
            var installed = packs.Count(p => p.Installed);
            var summary = S.Format("modset.summary", ("count", packs.Count.ToString(CultureInfo.CurrentCulture)), ("installed", installed.ToString(CultureInfo.CurrentCulture)));
            var canInstall = ModsIdle && _install is not null && !ModRecoveryRequired && _catalog is not null && !CatalogExpired;
            ModSetCards.Add(new(hint, mods, packs, summary, S.Format("external.notice", ("hint", hint)), canInstall,
                _setReports.GetValueOrDefault(hint) ?? "", _expandedSets.Contains(hint), (key, open) => { if (open) _expandedSets.Add(key); else _expandedSets.Remove(key); }));
        }
        Changed(nameof(FilteredModSetCards)); Changed(nameof(HasModSetCards));
    }

    /// <summary>"Install all…": the launcher's picker for the set's folder (a ZIP of one pack is accepted too), then <see cref="InstallSetFromAsync"/>.</summary>
    public async Task InstallSetAsync(ModSetCard set)
    {
        ArgumentNullException.ThrowIfNull(set);
        if (!ModsIdle || _install is null || ModManagerFactory() is not { } factory) return;
        string? start = null;
        try
        {
            var manager = await Task.Run(factory);
            start = set.Mods.Select(m => manager.ExternalChoices(m.Id).Values.FirstOrDefault()).FirstOrDefault(p => p is not null);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { Log("mods: set pick not read (" + error.GetType().Name + ")"); }
        OpenPicker(new(S.Format("modset.pick_title", ("hint", set.Hint)), true, true, path => InstallSetFromAsync(set.Hint, path))
        {
            Start = start,
            PickFolderLabel = S["modset.pick_this"],
            FileFilter = f => f.Extension.Equals(".zip", StringComparison.OrdinalIgnoreCase),
        });
    }

    /// <summary>Installs every pack of the set <paramref name="hint"/> that <paramref name="picked"/> holds (Common first), with a per-pack report.</summary>
    public async Task InstallSetFromAsync(string hint, string picked)
    {
        ArgumentNullException.ThrowIfNull(hint); ArgumentNullException.ThrowIfNull(picked);
        if (ModSetCards.FirstOrDefault(s => s.Hint == hint) is not { CanInstallAll: true } set || ModManagerFactory() is not { } factory) return;
        var mods = set.Mods; var hardcore = ProfileHardcore;
        IReadOnlyList<ExternalSetResult> results = []; string? unusable = null;
        var names = mods.ToDictionary(m => m.Id, m => m.Text("name", S.Locale), StringComparer.Ordinal);
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<ExternalSetProgress>(p =>
                ModStep(S.Format("modset.progress", ("mod", names.GetValueOrDefault(p.ModId) ?? p.ModId),
                    ("n", (p.Index + 1).ToString(CultureInfo.CurrentCulture)), ("total", p.Total.ToString(CultureInfo.CurrentCulture)))));
            var manager = await Task.Run(factory);
            try { results = await Task.Run(() => manager.InstallExternalSet(mods, picked, progress, CancellationToken.None, hardcore)); }
            catch (ModException error) when (error.Code == ModErrors.ExternalFiles)
            {
                // the pick itself cannot be read (gone, a link, too many files): the reason the single-pack pane gives
                unusable = S.Format("external.unusable", ("reason", S["external.reason." + error.Message] is var reason && reason != "external.reason." + error.Message
                    ? reason : S["external.reason.other"]));
                throw;
            }
            foreach (var result in results.Where(r => r.Outcome is ExternalSetOutcome.Installed or ExternalSetOutcome.Updated)) RecordModSource(result.Mod.Id);
        }, S["mods.action_done"], label: S.Format("mods.lock.set", ("set", hint), ("count", mods.Count.ToString(CultureInfo.CurrentCulture))));
        if (unusable is not null) { Message = unusable; return; }
        if (results.Count == 0) return; // refused as a whole; RunModOperationAsync said why
        Log("mods: set " + hint + ": " + string.Join(", ", results.Select(r => r.Mod.Id + " " + r.Outcome + (r.Reason is { } why ? " (" + why + ")" : ""))));
        _setReports[hint] = string.Join("\n", results.Select(r => SetResultLine(r, names)));
        Message = S.Format("modset.done",
            ("installed", results.Count(r => r.Outcome is ExternalSetOutcome.Installed or ExternalSetOutcome.Updated).ToString(CultureInfo.CurrentCulture)),
            ("current", results.Count(r => r.Outcome == ExternalSetOutcome.UpToDate).ToString(CultureInfo.CurrentCulture)),
            ("missing", results.Count(r => r.Outcome == ExternalSetOutcome.NotFound).ToString(CultureInfo.CurrentCulture)),
            ("failed", results.Count(r => r.Outcome == ExternalSetOutcome.Failed).ToString(CultureInfo.CurrentCulture)));
        _expandedSets.Add(hint);
        RefreshModSetCards();
        Changed(string.Empty);
    }

    private string SetResultLine(ExternalSetResult result, IReadOnlyDictionary<string, string> names)
    {
        var name = names.GetValueOrDefault(result.Mod.Id) ?? result.Mod.Id;
        return result.Outcome switch
        {
            ExternalSetOutcome.Installed => S.Format("modset.result.installed", ("mod", name)),
            ExternalSetOutcome.Updated => S.Format("modset.result.updated", ("mod", name)),
            ExternalSetOutcome.UpToDate => S.Format("modset.result.up_to_date", ("mod", name)),
            ExternalSetOutcome.NotFound => S.Format("modset.result.not_found", ("mod", name)),
            _ => S.Format("modset.result.failed", ("mod", name), ("reason", SetFailureReason(result, names))),
        };
    }

    private string SetFailureReason(ExternalSetResult result, IReadOnlyDictionary<string, string> names) => result.Reason switch
    {
        "files" => S.Format("modset.reason.files", ("missing", result.Missing.ToString(CultureInfo.CurrentCulture)), ("different", result.Different.ToString(CultureInfo.CurrentCulture))),
        "retired" => S["modset.reason.retired"],
        { } reason when reason.StartsWith("dependency:", StringComparison.Ordinal) =>
            S.Format("modset.reason.dependency", ("mod", names.GetValueOrDefault(reason["dependency:".Length..]) ?? reason["dependency:".Length..])),
        { } code => ModErrorText(code),
        null => S["mods.transaction_failed"],
    };

    /// <summary>A pack row's Remove (the information pane's Uninstall, for that row's mod).</summary>
    public Task UninstallModCardAsync(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle || !card.CanUninstall || ModRecoveryRequired || ModManagerFactory() is not { } factory) return Task.CompletedTask;
        return RunModOperationAsync(() => Task.Run(() => factory().Uninstall(card.Manifest.Id)), S["mods.uninstalled"],
            label: S.Format("mods.lock.uninstall", ("mod", ModName(card.Manifest))));
    }
}
