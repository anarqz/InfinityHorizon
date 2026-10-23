// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>One mod card: a catalog manifest, an installed one, or both (an update to review).</summary>
public sealed record ModCard(ModManifest Manifest, string Name, string Description, string Status, string Requirements,
    string Action, bool CanAct, bool HasOptions, string Credits, string Badge)
{
    public bool Installed { get; init; }
    public bool Enabled { get; init; }
    public bool Drift { get; init; }
    public bool SettingsIssues { get; init; }
    public bool UpdateAvailable { get; init; }
    public string UpdateSummary { get; init; } = "";
    public ModManifest? Next { get; init; }
    public bool CanUninstall => Installed && !Drift;
}

/// <summary>What the background refresh learned about one manifest (M10: no disk access while building cards).</summary>
internal sealed record ModCardFacts(ModManifest Manifest, ModInstallation? Installed, bool CanAdopt, string? Blocker, ModManifest? Next);

public sealed partial class ShellViewModel
{
    private IReadOnlyList<ModManifest> _manifestCatalog = [];
    private ModCatalog? _catalog;
    private bool _modOperation;
    private bool _catalogLoaded;
    private bool _catalogLoading;
    private string _modProgress = "";
    private string? _catalogFailure;
    private string _modSearch = "";
    private ModCard? _selectedMod;
    private bool _showModInfo;
    private ModHealth _modHealth = ModHealth.Empty;
    private string? _modStateError;
    private IReadOnlyList<ModCardFacts> _modFacts = [];
    private int _modRefresh;
    public ObservableCollection<ModCard> ModCards { get; } = [];
    public IEnumerable<ModCard> FilteredModCards => ModCards.Where(card => string.IsNullOrWhiteSpace(_modSearch)
        || card.Name.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase)
        || card.Manifest.Id.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase)
        || card.Credits.Contains(_modSearch.Trim(), StringComparison.OrdinalIgnoreCase));
    public string ModSearch
    {
        get => _modSearch;
        set { _modSearch = value ?? ""; Changed(); Changed(nameof(FilteredModCards)); Changed(nameof(ModSearchEmpty)); Changed(nameof(NoMatchingMods)); }
    }
    public bool ModSearchEmpty => string.IsNullOrWhiteSpace(_modSearch);
    public bool HasModCards => ModCards.Count > 0;
    public bool NoMatchingMods => HasModCards && !FilteredModCards.Any();
    public ModCard? SelectedMod => _selectedMod;
    public bool ShowModInfo => _showModInfo && ShowMods && _selectedMod is not null;
    public string SelectedModFiles => _selectedMod is not { } selected ? "" :
        S.Format("mods.file_summary", ("game", selected.Manifest.Files.Count(f => f.Root == "game").ToString(CultureInfo.CurrentCulture)),
            ("azimuth", selected.Manifest.Files.Count(f => f.Root == "azimuth").ToString(CultureInfo.CurrentCulture)))
        + (selected.Manifest.ConfigPath is { } config ? "\n" + S["mods.settings_file"] + " " + config : "");
    public bool SelectedModDrift => ModsIdle && _selectedMod is { Drift: true };
    public bool SelectedModCanDisableKeeping => ModsIdle && _selectedMod is { Drift: true, Enabled: true };
    public bool SelectedModSettingsIssues => ModsIdle && _selectedMod is { SettingsIssues: true };
    public bool SelectedModCanUninstall => ModsIdle && _selectedMod is { CanUninstall: true } && !ModRecoveryRequired;
    public bool SelectedModUpdate => _selectedMod is { UpdateAvailable: true };
    public bool CanConfirmModUpdate => ModsIdle && _selectedMod is { UpdateAvailable: true, CanAct: true };

    public async Task SelectModAsync(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle) return;
        if (card.HasOptions && !_showModOptions && !_iniDirty)
        {
            await RefreshModsAsync();
            card = ModCards.FirstOrDefault(current => current.Manifest.Id == card.Manifest.Id) ?? card;
        }
        _selectedMod = card;
        _showModInfo = !card.HasOptions;
        _showModOptions = card.HasOptions;
        _showLobbyPane = false;
        if (card.HasOptions && !(_iniDirty && _iniManifest == card.Manifest.Digest)) await LoadSelectedOptionsAsync();
        Changed(string.Empty);
    }

    public void SelectMod(ModCard card) => _ = SelectModAsync(card);

    public void ShowModInformation(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle) return;
        _selectedMod = card; _showModInfo = true; _showModOptions = false; _showLobbyPane = false;
        Changed(string.Empty);
    }

    public void CloseModPanel()
    {
        _showModInfo = false; _showModOptions = false;
        Changed(string.Empty);
    }

    /// <summary>DEV only: an unsigned catalog URL override. Production builds always use the signed catalog (§1.2).</summary>
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public bool DevelopmentMods
    {
        get
        {
#if INFINITY_PRODUCTION_BUILD
            return false;
#else
            return !_config.Production && !string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable("FI_DEV_MOD_CATALOG_URL"));
#endif
        }
    }
    /// <summary>Native (infinity_mod.dll) mods need the runtime and helper, which only development packages carry (review §3).</summary>
    public bool NativeModsAvailable => DevelopmentMods;
    public bool HasManifestCatalog => _catalogLoaded;
    public bool ModOperationBusy => _modOperation;
    public bool ModContentEnabled => !_modOperation;
    public string ModProgress => _modProgress;
    private bool CatalogExpired => _catalog?.Expired(DateTimeOffset.UtcNow) == true;
    public string ModCatalogStatus => _catalogFailure ?? (!_catalogLoaded ? S[_catalogLoading ? "mods.progress.catalog" : "mods.catalog_unloaded"]
        : CatalogExpired ? S["mods.catalog_expired"] : _catalog is { Trusted: false } ? S["mods.catalog_dev"]
        : _catalog is { Cached: true } ? S["mods.catalog_cached"] : S["mods.catalog_production"]);
    public string ModCatalogSummary => S.Format(_catalog is { Trusted: true } ? "mods.catalog_count_signed" : "mods.catalog_count",
        ("count", ModCards.Count.ToString(CultureInfo.CurrentCulture))) + (_catalogFailure is { } failure ? " · " + failure : "");
    public string ModsSubtitle => S[_catalogLoaded ? "mods.catalog_subtitle" : "mods.subtitle"];

    /// <summary>Any interrupted operation in the bucket: mod changes wait for it (Play only for this folder, <see cref="ModPlayBlocked"/>).</summary>
    public bool ModRecoveryRequired => _modHealth.RecoveryHere || _modHealth.RecoveryElsewhere;
    /// <summary>The only mod state that stops ordinary Play: an interrupted change of this Azimuth folder, always shown with its recovery.</summary>
    public bool ModPlayBlocked => _install is not null && _modHealth.BlocksPlay;
    public bool ShowModRecovery => ModsIdle && (ModRecoveryRequired || _modHealth.Problems.Count > 0 || _modHealth.Installed.Any(i => i.Problem is not null)
        || _modStateError is not null || HasModRelink);
    public bool CanRecoverMods => ModsIdle && ModRecoveryRequired;
    public bool CanResolveMods => ModsIdle && (ModRecoveryRequired || _modHealth.Installed.Any(i => i.Problem is not null));
    public bool HasModRelink => _modHealth.RelinkCandidates.Count > 0 || _modHealth.GameRelink;
    public string ModRecoveryText => string.Join("\n", new[]
    {
        _modHealth.RecoveryHere ? S["mods.recovery_here"] : null,
        _modHealth.RecoveryElsewhere ? S["mods.recovery_elsewhere"] : null,
        _modHealth.Problems.Count > 0 || _modHealth.Installed.Any(i => i.Problem is not null) ? S["mods.recovery_problems"] : null,
        _modStateError is not null ? S["mods.state_unreadable"] : null,
        HasModRelink ? S["mods.relink_hint"] : null,
    }.OfType<string>());
    /// <summary>OD-112 follow-up: the selected profile is Hardcore, which allows no mod that affects multiplayer.</summary>
    private bool ProfileHardcore => LobbyProfile?.Mode == "hardcore";
    /// <summary>Enabled mods a Hardcore profile refuses (<see cref="ModMultiplayer.AffectsMultiplayer"/>, the lobby-set rule), by name.</summary>
    private IReadOnlyList<string> HardcoreRefusedMods => !ProfileHardcore ? [] : _modHealth.Installed
        .Where(i => i.Enabled && i.Manifest is { } m && ModMultiplayer.AffectsMultiplayer(m)).Select(i => i.Manifest!.Text("name", S.Locale)).ToArray();
    private string HardcoreRefusal(IEnumerable<string> names) => S.Format("mods.hardcore_refused", ("mods", string.Join(", ", names)));

    /// <summary>Shown on Play: why Play waits for mods, or that some need attention (never a silent block).</summary>
    public string? ModPlayNotice => _install is null ? null : ModPlayBlocked ? S["mods.play_blocked"]
        : HardcoreRefusedMods is { Count: > 0 } refused ? HardcoreRefusal(refused)
        : _modStateError is not null ? S["mods.state_unreadable"] : _modHealth.HasWarnings ? S["mods.play_warning"] : null;
    public bool ShowModPlayNotice => ModPlayNotice is not null && !Playing;

    public string ModFolder => _install?.Root ?? S["setup.not_found"];

    /// <summary>The bucket lives in the stable launcher data home (shared by development and production profiles).</summary>
    private Func<ManagedModFiles>? ModManagerFactory()
    {
        if (_install is null) return null;
        var azimuth = _install.Root; var game = ContentLocation?.Mount;
        var state = _config.Production ? Directory.GetParent(_stateDirectory)!.FullName : _stateDirectory;
        var cache = DevelopmentMods ? Environment.GetEnvironmentVariable("FI_DEV_MOD_PACKAGE_CACHE") : null;
        return () => new ManagedModFiles(azimuth, game, state, GameRunning, cache);
    }

    /// <summary>Constructs the manager; only for code already off the UI thread or the harness.</summary>
    private ManagedModFiles ManagedMods() => (ModManagerFactory() ?? throw new ModException("Azimuth folder not selected"))();

    public async Task OpenDevelopmentModsAsync(string? azimuth)
    {
        if (!DevelopmentMods || !ModsIdle) return;
        if (azimuth is not null) Install = Infinity.Core.Process.AzimuthInstall.Detect(azimuth);
        else await FindAzimuthAsync(null, openSetupWhenNeeded: false);
        _onMods = true; _onSetup = false; _onBugReport = false;
        Changed(string.Empty);
        await LoadModCatalogAsync();
    }

    /// <summary>Explicit refresh (the ↻ button): shows progress and the result.</summary>
    public async Task LoadModCatalogAsync()
    {
        if (!ModsIdle) return;
        var loaded = false;
        await RunModOperationAsync(async () =>
        {
            _modProgress = S["mods.progress.catalog"]; Changed(nameof(ModProgress));
            await FetchCatalogAsync();
            loaded = true;
        }, S["mods.catalog_loaded"]);
        if (!loaded) _catalogFailure = S["mods.catalog_failed"];
        Changed(string.Empty);
    }

    /// <summary>On opening Mods and once at startup: the cached signed catalog at once, then the network, without the overlay; failures only say so.</summary>
    private async Task LoadModCatalogQuietlyAsync()
    {
        if (_catalogLoading) return;
        _catalogLoading = true;
        try
        {
            if (!DevelopmentMods && !_catalogLoaded && await Task.Run(() => SignedCatalogCached()) is { } cached) ApplyCatalog(cached);
            await FetchCatalogAsync();
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
        {
            Log("mods: catalog unavailable: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : ""));
            _catalogFailure = S["mods.catalog_failed"];
        }
        finally { _catalogLoading = false; }
        await RefreshModsAsync();
    }

    private ModCatalog? SignedCatalogCached()
    {
        try { return new SignedModCatalog(ModDownloads, Path.Combine(_stateDirectory, "mod-catalog")).LoadCached(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return null; }
    }

    private async Task FetchCatalogAsync()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        ModCatalog catalog;
        if (DevelopmentMods)
        {
            if (!Uri.TryCreate(Environment.GetEnvironmentVariable("FI_DEV_MOD_CATALOG_URL"), UriKind.Absolute, out var uri))
                throw new ModException("invalid catalog URL", ModErrors.Catalog);
            catalog = await DevelopmentModCatalog.LoadAsync(ModDownloads, uri, timeout.Token);
        }
        else catalog = await new SignedModCatalog(ModDownloads, Path.Combine(_stateDirectory, "mod-catalog")).LoadAsync(ModCatalogTrust.ProductionUrl, timeout.Token);
        ApplyCatalog(catalog);
    }

    private void ApplyCatalog(ModCatalog catalog)
    {
        _catalog = catalog;
        _manifestCatalog = catalog.Mods;
        _catalogLoaded = true;
        _catalogFailure = null;
    }

    /// <summary>M10: reads mod state off the UI thread, then builds cards from that snapshot; the newest refresh wins.</summary>
    internal async Task RefreshModsAsync()
    {
        var version = ++_modRefresh;
        var factory = ModManagerFactory();
        var catalog = _manifestCatalog;
        var (health, error, facts) = await Task.Run(() => ReadModState(factory, catalog));
        if (version != _modRefresh) return;
        if (error is not null && error != _modStateError) Log("mods: state could not be read: " + error);
        _modHealth = health; _modStateError = error; _modFacts = facts;
        RefreshModCards();
        if (_selectedMod is { HasOptions: true } && !_iniDirty) await LoadSelectedOptionsAsync();
        Changed(string.Empty);
    }

    private static (ModHealth Health, string? Error, IReadOnlyList<ModCardFacts> Facts) ReadModState(Func<ManagedModFiles>? factory, IReadOnlyList<ModManifest> catalog)
    {
        if (factory is null) return (ModHealth.Empty, null, catalog.Select(m => new ModCardFacts(m, null, false, null, null)).ToArray());
        ManagedModFiles manager;
        try { manager = factory(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        { return (ModHealth.Empty, ManagedModFiles.Code(error), catalog.Select(m => new ModCardFacts(m, null, false, ManagedModFiles.Code(error), null)).ToArray()); }
        var health = manager.Inspect();
        var facts = new List<ModCardFacts>();
        foreach (var manifest in catalog)
        {
            var installed = health.Installed.FirstOrDefault(i => i.Id == manifest.Id);
            if (installed?.Manifest is { } current && current.Digest != manifest.Digest)
            {
                facts.Add(new(current, installed, false, Blocker(() => { if (installed.Enabled) { manager.CheckDependencies(manifest); manager.CheckDependents(manifest.Id, manifest); } }), manifest));
                continue;
            }
            var adopt = false;
            if (installed is null) try { adopt = manager.CanAdopt(manifest); } catch (Exception error) when (ManagedModFiles.IsStateError(error)) { }
            facts.Add(new(manifest, installed, adopt, installed?.Problem ?? Blocker(() =>
            {
                if (installed is { Enabled: true }) manager.CheckDependents(manifest.Id);
                else if (installed is null || installed.Drift.Count == 0) manager.CheckDependencies(manifest);
            }), null));
        }
        foreach (var installed in health.Installed.Where(i => i.Manifest is not null && catalog.All(m => m.Id != i.Id)))
            facts.Add(new(installed.Manifest!, installed, false, Blocker(() => { if (installed.Enabled) manager.CheckDependents(installed.Id); else manager.CheckDependencies(installed.Manifest!); }), null));
        return (health, null, facts);
    }

    private static string? Blocker(Action check)
    {
        try { check(); return null; }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return ManagedModFiles.Code(error); }
    }

    private void RefreshModCards()
    {
        ModCards.Clear();
        var expired = CatalogExpired;
        foreach (var fact in _modFacts)
        {
            var mod = fact.Manifest; var installed = fact.Installed;
            var compatible = _install is not null && mod.Document.GetProperty("compatibility").GetProperty("azimuthVersions").EnumerateArray().Any(v => v.GetString() == _install.Version);
            var nativeBlocked = mod.IsNative && !NativeModsAvailable;
            string status, badge, action; bool canAct;
            if (_modHealth.RecoveryHere) { status = S["mods.recovery_needed"]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (installed?.Problem is not null) { status = S["mods.recovery_problems"]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (fact.Next is { } next)
            {
                status = S.Format("mods.update_available", ("from", mod.Version), ("to", next.Version));
                badge = S["mods.update"]; action = S["mods.review_update"];
                canAct = !expired && next.SupportedForInstall && !(next.IsNative && !NativeModsAvailable) && fact.Blocker is null && installed!.Drift.Count == 0;
            }
            else if (installed is not null)
            {
                var drift = installed.Drift.Count > 0;
                badge = S[drift || installed.SettingsIssues.Count > 0 ? "mods.badge_attention" : installed.Enabled ? "mods.badge_on" : "mods.badge_off"];
                status = drift ? S["mods.drift_warning"] : installed.SettingsIssues.Count > 0 ? S["mods.settings_issues"] : S[installed.Enabled ? "mods.managed_on" : "mods.managed_off"];
                action = S[installed.Enabled ? "mods.disable_action" : "mods.enable_action"];
                canAct = fact.Blocker is null && !(installed.Enabled && drift) && !(!installed.Enabled && nativeBlocked);
            }
            else if (fact.CanAdopt)
            {
                status = S["mods.unmanaged"]; badge = status; action = S["mods.adopt"];
                canAct = compatible && fact.Blocker is null && !nativeBlocked;
            }
            else
            {
                status = expired ? S["mods.catalog_expired"] : nativeBlocked ? S["mods.native_unavailable"] : S["mods.not_installed"];
                badge = S["mods.not_installed"]; action = S["mods.install"];
                canAct = compatible && !expired && !nativeBlocked && fact.Blocker is null && _catalog is not null;
            }
            if (fact.Blocker is not null && installed?.Problem is null && fact.Next is null && !(installed is { Enabled: true } && installed.Drift.Count > 0))
            { status = ModErrorText(fact.Blocker); badge = S["mods.badge_attention"]; }
            if (!mod.SupportedForInstall && installed is null) { status = S["mods.review_blocked"]; badge = S["mods.badge_review"]; canAct = false; }
            if (ProfileHardcore && ModMultiplayer.AffectsMultiplayer(fact.Next ?? mod) && installed?.Problem is null)
            {
                // Hardcore: unavailable on this profile; an enabled one keeps its Disable action and its files.
                status = S["mods.hardcore_unavailable"]; badge = S["mods.badge_attention"];
                if (installed is { Enabled: true } && fact.Next is null) action = S["mods.disable_action"];
                else canAct = false;
            }
            canAct &= ModsIdle && _install is not null && !ModRecoveryRequired;
            var requirements = mod.Dependencies.Count == 0 ? S["mods.no_dependencies"]
                : S["mods.dependencies"] + " " + string.Join(", ", mod.Dependencies.Select(d =>
                    (_manifestCatalog.FirstOrDefault(m => m.Id == d.Id)?.Text("name", S.Locale) ?? d.Id) + " " + string.Join(" / ", d.Versions)));
            if (_install is null) requirements += "\n" + S["mods.choose_folder"];
            ModCards.Add(new(mod, mod.Text("name", S.Locale), mod.Text("description", S.Locale), status, requirements,
                action, canAct, mod.ConfigPath is not null && installed is not null && installed.Problem is null,
                mod.Document.GetProperty("author").GetString() + " · " + mod.Document.GetProperty("license").GetString() + " · " + mod.Version, badge)
            {
                Installed = installed is not null && installed.Problem is null, Enabled = installed?.Enabled == true,
                Drift = installed?.Drift.Count > 0, SettingsIssues = installed?.SettingsIssues.Count > 0,
                UpdateAvailable = fact.Next is not null, Next = fact.Next, UpdateSummary = fact.Next is { } upcoming ? UpdateSummary(mod, upcoming) : "",
            });
        }
        if (_selectedMod is { } selected) _selectedMod = ModCards.FirstOrDefault(card => card.Manifest.Id == selected.Manifest.Id);
        Changed(nameof(FilteredModCards)); Changed(nameof(NoMatchingMods)); Changed(nameof(SelectedMod)); Changed(nameof(SelectedModFiles)); Changed(nameof(HasModCards));
    }

    /// <summary>The reviewed change of an update (M5): versions, file mappings added/removed/changed, settings added/removed; the player's settings stay.</summary>
    private string UpdateSummary(ModManifest installed, ModManifest next)
    {
        static string Key(ManifestFile f) => f.Root + "/" + f.Destination;
        var before = installed.Files.ToDictionary(Key, StringComparer.OrdinalIgnoreCase); var after = next.Files.ToDictionary(Key, StringComparer.OrdinalIgnoreCase);
        static HashSet<string> Options(ModManifest m) => m.Document.GetProperty("config") is { ValueKind: System.Text.Json.JsonValueKind.Object } config && config.TryGetProperty("options", out var list)
            ? list.EnumerateArray().Select(o => o.GetProperty("id").GetString()!).ToHashSet(StringComparer.Ordinal) : [];
        var oldOptions = Options(installed); var newOptions = Options(next);
        string N(int n) => n.ToString(CultureInfo.CurrentCulture);
        return S.Format("mods.update_summary", ("from", installed.Version), ("to", next.Version),
            ("added", N(after.Keys.Count(k => !before.ContainsKey(k)))), ("removed", N(before.Keys.Count(k => !after.ContainsKey(k)))),
            ("changed", N(after.Count(f => before.TryGetValue(f.Key, out var old) && old.Hash != f.Value.Hash))),
            ("settings_added", N(newOptions.Count(o => !oldOptions.Contains(o)))), ("settings_removed", N(oldOptions.Count(o => !newOptions.Contains(o)))));
    }

    /// <summary>M7: a refused mod operation as a localized sentence; never a raw exception text.</summary>
    private string ModErrorText(string code) => S["mods.error." + code] is var text && text != "mods.error." + code ? text : S["mods.transaction_failed"];

    private string ModErrorText(Exception error) => error switch
    {
        ModSettingsChangedException => S["mods.settings_changed"],
        HttpRequestException or OperationCanceledException => S["mods.download_failed"],
        ModException mod => ModErrorText(mod.Code),
        _ => S["mods.transaction_failed"],
    };

    public async Task ActOnModAsync(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle || _install is null) return;
        await RefreshModsAsync();
        var current = ModCards.FirstOrDefault(c => c.Manifest.Digest == card.Manifest.Digest);
        if (current is not { CanAct: true }) return;
        if (current.UpdateAvailable) { ShowModInformation(current); return; } // reviewed first: the information pane shows the change
        var mod = current.Manifest;
        var factory = ModManagerFactory()!;
        var hosts = _catalog?.DownloadHosts ?? [];
        var hardcore = ProfileHardcore;
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            var manager = await Task.Run(factory);
            if (!current.Installed && current.Action == S["mods.adopt"]) await Task.Run(() => manager.Adopt(mod, hardcore));
            else if (!current.Installed)
            {
                using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
                await manager.InstallAsync(ModDownloads, mod, hosts, timeout.Token, progress, hardcore: hardcore);
            }
            else await Task.Run(() => manager.SetEnabled(mod, !current.Enabled, progress, hardcore));
        }, S["mods.action_done"]);
    }

    public Task ConfirmModUpdateAsync()
    {
        if (_selectedMod is not { UpdateAvailable: true, CanAct: true, Next: { } next }) return Task.CompletedTask;
        var factory = ModManagerFactory()!; var hosts = _catalog?.DownloadHosts ?? []; var hardcore = ProfileHardcore;
        return RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
            var manager = await Task.Run(factory);
            await manager.InstallAsync(ModDownloads, next, hosts, timeout.Token, progress, update: true, hardcore: hardcore);
        }, S["mods.updated"]);
    }

    private Task SelectedModOperation(Action<ManagedModFiles, ModManifest> operation, string done)
    {
        if (_selectedMod is not { } selected || ModManagerFactory() is not { } factory) return Task.CompletedTask;
        return RunModOperationAsync(() => Task.Run(() => operation(factory(), selected.Manifest)), done);
    }

    public Task AcceptModFilesAsync() => SelectedModOperation((m, mod) => m.AcceptCurrentFiles(mod), S["mods.accepted"]);
    public Task DisableModKeepingFilesAsync() => SelectedModOperation((m, mod) => m.DisableKeepingCurrentFiles(mod), S["mods.disabled_kept"]);
    public Task RepairModSettingsAsync() => SelectedModOperation((m, mod) => m.RepairSettings(mod), S["mods.repaired"]);
    public Task UninstallModAsync() => SelectedModOperation((m, mod) => m.Uninstall(mod.Id), S["mods.uninstalled"]);

    private Task ManagerOperation(Action<ManagedModFiles> operation, string done) =>
        ModManagerFactory() is { } factory ? RunModOperationAsync(() => Task.Run(() => operation(factory())), done) : Task.CompletedTask;

    public Task RecoverModsAsync() => ManagerOperation(m => m.Recover(), S["mods.recovered"]);
    public Task ResolveModRecoveryAsync() => ManagerOperation(m => m.ResolveRecovery(), S["mods.resolved"]);
    public Task RelinkModsAsync() => ManagerOperation(m =>
    {
        foreach (var folder in m.RelinkCandidates().Take(1)) m.Relink(folder);
        if (m.GameRelinkCandidates().Count > 0) m.RelinkGame();
    }, S["mods.relinked"]);

    /// <summary>Retention at startup (M5): in the background, only while nothing else runs; a refusal is only logged.</summary>
    private async Task MaintainModsAsync()
    {
        if (ModManagerFactory() is not { } factory || !ModsIdle || GameRunning()) return;
        try { await Task.Run(() => factory().Prune(DateTimeOffset.UtcNow)); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { Log("mods: retention skipped: " + ManagedModFiles.Code(error)); }
    }

    private async Task RunModOperationAsync(Func<Task> operation, string done, bool preserveOptionsOnFailure = false)
    {
        if (!ModsIdle) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        _modOperation = true; _modProgress = S["mods.progress.prepare"]; Busy = true;
        var succeeded = false;
        try { await operation(); Message = done; succeeded = true; }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
        {
            Log("mods: operation failed: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : ""));
            Message = ModErrorText(error);
        }
        finally
        {
            _modOperation = false; Busy = false;
            if (succeeded && preserveOptionsOnFailure) _iniDirty = false; // a saved draft reloads from the file; a failed one stays
            await RefreshModsAsync();
            Changed(string.Empty);
        }
    }
}
