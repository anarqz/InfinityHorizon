// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

public sealed record ModCard(ModManifest Manifest, string Name, string Description, string Status, string Requirements,
    string Action, bool CanAct, bool HasOptions, string Credits, string Badge);

public sealed partial class ShellViewModel
{
    private IReadOnlyList<ModManifest> _manifestCatalog = [];
    private bool _modOperation;
    private bool _catalogLoaded;
    private string _modProgress = "";
    private string? _catalogFailure;
    private string _modSearch = "";
    private ModCard? _selectedMod;
    private bool _showModInfo;
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
    public bool NoMatchingMods => HasManifestCatalog && !FilteredModCards.Any();
    public ModCard? SelectedMod => _selectedMod;
    public bool ShowModInfo => _showModInfo && ShowMods && _selectedMod is not null;
    public string SelectedModFiles => _selectedMod is not { } selected ? "" :
        S.Format("mods.file_summary", ("game", selected.Manifest.Files.Count(f => f.Root == "game").ToString(System.Globalization.CultureInfo.CurrentCulture)),
            ("azimuth", selected.Manifest.Files.Count(f => f.Root == "azimuth").ToString(System.Globalization.CultureInfo.CurrentCulture)))
        + (selected.Manifest.ConfigPath is { } config ? "\n" + S["mods.settings_file"] + " " + config : "");

    public void SelectMod(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle) return;
        if (card.HasOptions && !_showModOptions && !_iniDirty)
        {
            RefreshMods();
            card = ModCards.FirstOrDefault(current => current.Manifest.Id == card.Manifest.Id) ?? card;
        }
        _selectedMod = card;
        _showModInfo = !card.HasOptions;
        _showModOptions = card.HasOptions;
        _showLobbyPane = false;
        if (card.HasOptions && !(_iniDirty && _iniManifest == card.Manifest.Digest)) LoadSelectedOptions();
        Changed(string.Empty);
    }

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
    public bool HasManifestCatalog => _catalogLoaded;
    public bool ModOperationBusy => _modOperation;
    public bool ModContentEnabled => !_modOperation;
    public string ModProgress => _modProgress;
    public string ModCatalogStatus => _catalogFailure ?? S[_catalogLoaded ? "mods.catalog_dev" : "mods.catalog_unloaded"];
    public string ModCatalogSummary => _catalogFailure ?? S.Format("mods.catalog_count",
        ("count", _manifestCatalog.Count.ToString(System.Globalization.CultureInfo.CurrentCulture)));
    public string ModsSubtitle => S[_catalogLoaded ? "mods.catalog_subtitle" : "mods.subtitle"];
    public bool ShowModRecovery => ModsIdle && ModRecoveryRequired;
    public bool ModRecoveryRequired
    {
        get
        {
            try { return _install is not null && ManagedMods().RecoveryRequired; }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { return true; }
        }
    }
    private bool HasManagedModState
    {
        get
        {
            try
            {
                if (_install is null) return false;
                var manager = ManagedMods();
                return manager.RecoveryRequired || Directory.Exists(manager.BackupRoot)
                    && Directory.EnumerateFiles(manager.BackupRoot, "*.receipt.json").Any();
            }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { return true; }
        }
    }
    public string ModFolder => _install?.Root ?? S["setup.not_found"];
    public string ModBackupFolder
    {
        get
        {
            try { return _install is null ? "" : ManagedMods().BackupRoot; }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException) { return S["mods.files_changed"]; }
        }
    }
    private ManagedModFiles ManagedMods() => new(_install!.Root, ContentLocation?.Mount,
        _config.Production ? Directory.GetParent(_stateDirectory)!.FullName : _stateDirectory, GameRunning,
        DevelopmentMods ? Environment.GetEnvironmentVariable("FI_DEV_MOD_PACKAGE_CACHE") : null);

    public async Task OpenDevelopmentModsAsync(string? azimuth)
    {
        if (!DevelopmentMods || !ModsIdle) return;
        if (azimuth is not null) Install = Infinity.Core.Process.AzimuthInstall.Detect(azimuth);
        else await FindAzimuthAsync(null, openSetupWhenNeeded: false);
        _onMods = true; _onSetup = false; _onBugReport = false;
        Changed(string.Empty);
        await LoadModCatalogAsync();
    }

    public async Task LoadModCatalogAsync()
    {
        if (!DevelopmentMods || !ModsIdle) return;
        await RunModOperationAsync(async () =>
        {
            _modProgress = S["mods.progress.catalog"]; Changed(nameof(ModProgress));
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
            if (!Uri.TryCreate(Environment.GetEnvironmentVariable("FI_DEV_MOD_CATALOG_URL"), UriKind.Absolute, out var uri))
                throw new ModException("invalid catalog URL");
            var manifests = await DevelopmentModCatalog.LoadAsync(ModDownloads, uri, timeout.Token);
            _manifestCatalog = manifests;
            _catalogLoaded = true;
            _catalogFailure = null;
        }, S["mods.catalog_loaded"]);
        if (!_catalogLoaded) _catalogFailure = S["mods.catalog_failed"];
        Changed(string.Empty);
    }

    public async Task ActOnModAsync(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!ModsIdle || _install is null) return;
        RefreshModCards();
        var current = ModCards.FirstOrDefault(c => c.Manifest.Digest == card.Manifest.Digest);
        if (current is not { CanAct: true }) return;
        var mod = current.Manifest;
        await RunModOperationAsync(async () =>
        {
            var manager = ManagedMods();
            var receipt = manager.Receipt(mod);
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            if (receipt is null && manager.CanAdopt(mod)) await Task.Run(() => manager.Adopt(mod));
            else if (receipt is null)
            {
                using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
                await manager.InstallAsync(ModDownloads, mod, timeout.Token, progress);
            }
            else await Task.Run(() => manager.SetEnabled(mod, !receipt.Enabled, progress));
        }, S["mods.action_done"]);
    }

    public Task RecoverModsAsync() => RunModOperationAsync(() => Task.Run(() => ManagedMods().Recover()), S["mods.recovered"]);

    private async Task RunModOperationAsync(Func<Task> operation, string done, bool preserveOptionsOnFailure = false)
    {
        if (!ModsIdle) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        _modOperation = true; _modProgress = S["mods.progress.prepare"]; Busy = true;
        var succeeded = false;
        try { await operation(); Message = done; succeeded = true; }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException or HttpRequestException
            or OperationCanceledException or JsonException or InvalidOperationException or KeyNotFoundException or ArgumentException or OverflowException or FormatException)
        {
            Log("mods: operation failed: " + error.GetType().Name);
            Message = S[error is ModSettingsChangedException ? "mods.settings_changed"
                : error is HttpRequestException or OperationCanceledException ? "mods.download_failed" : "mods.transaction_failed"];
        }
        finally
        {
            _modOperation = false; Busy = false;
            if (succeeded || !preserveOptionsOnFailure) RefreshMods();
            RefreshModCards(); Changed(string.Empty);
        }
    }

    private void RefreshModCards()
    {
        ModCards.Clear();
        foreach (var mod in _manifestCatalog)
        {
            var status = S["mods.not_installed"];
            var badge = status;
            var installed = false;
            var action = S["mods.install"];
            var canAct = ModsIdle && _install is not null && mod.SupportedForInstall && mod.Document.GetProperty("compatibility").GetProperty("azimuthVersions")
                    .EnumerateArray().Any(v => v.GetString() == _install.Version);
            try
            {
                if (_install is not null)
                {
                    var manager = ManagedMods();
                    if (manager.RecoveryRequired) { status = S["mods.recovery_needed"]; badge = S["mods.badge_attention"]; canAct = false; }
                    else if (manager.Receipt(mod) is { } receipt)
                    {
                        var verified = manager.Verified(mod);
                        installed = true;
                        badge = S[!verified ? "mods.badge_attention" : receipt.Enabled ? "mods.badge_on" : "mods.badge_off"];
                        status = verified ? S[receipt.Enabled ? "mods.managed_on" : "mods.managed_off"] : S["mods.files_changed"];
                        action = S[receipt.Enabled ? "mods.disable_action" : "mods.enable_action"];
                        canAct &= verified;
                    }
                    else if (manager.CanAdopt(mod)) { status = S["mods.unmanaged"]; badge = status; action = S["mods.adopt"]; }
                    if (canAct)
                    {
                        if (manager.Receipt(mod) is { Enabled: true }) manager.CheckDependents(mod.Id);
                        else manager.CheckDependencies(mod);
                    }
                }
            }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException or JsonException)
            { canAct = false; status = S["mods.requirements_unmet"]; badge = S["mods.badge_attention"]; }
            if (!mod.SupportedForInstall) { status = S["mods.review_blocked"]; badge = S["mods.badge_review"]; }
            var requirements = mod.Dependencies.Count == 0 ? S["mods.no_dependencies"]
                : S["mods.dependencies"] + " " + string.Join(", ", mod.Dependencies.Select(d =>
                    (_manifestCatalog.FirstOrDefault(m => m.Id == d.Id)?.Text("name", S.Locale) ?? d.Id) + " " + string.Join(" / ", d.Versions)));
            if (_install is null) requirements += "\n" + S["mods.choose_folder"];
            ModCards.Add(new(mod, mod.Text("name", S.Locale), mod.Text("description", S.Locale), status, requirements,
                action, canAct, mod.ConfigPath is not null && installed,
                mod.Document.GetProperty("author").GetString() + " · " + mod.Document.GetProperty("license").GetString() + " · " + mod.Version, badge));
        }
        if (_selectedMod is { } selected) _selectedMod = ModCards.FirstOrDefault(card => card.Manifest.Id == selected.Manifest.Id);
        Changed(nameof(FilteredModCards)); Changed(nameof(NoMatchingMods)); Changed(nameof(SelectedMod)); Changed(nameof(SelectedModFiles));
    }
}
