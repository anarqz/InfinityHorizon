// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

public sealed record ModCard(ModManifest Manifest, string Name, string Description, string Status, string Requirements,
    string Action, bool CanAct, bool HasOptions, string Credits);

public sealed partial class ShellViewModel
{
    private IReadOnlyList<ModManifest> _manifestCatalog = [];
    private bool _modOperation;
    private bool _catalogLoaded;
    private string _modProgress = "";
    private string? _catalogFailure;
    public ObservableCollection<ModCard> ModCards { get; } = [];

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
    public string ModsSubtitle => S[_catalogLoaded ? "mods.catalog_subtitle" : "mods.subtitle"];
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
    // Backups describe game files, so dev and production must see the same ownership records.
    private ManagedModFiles ManagedMods() => new(_install!.Root, ContentLocation?.Mount,
        _config.Production ? Directory.GetParent(_stateDirectory)!.FullName : _stateDirectory, GameRunning);

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
        // Existing legacy installs retain their original control path; no historical original
        // is inferred from an already modified installation.
        if (mod.Id == Eo.Id && ManagedMods().Receipt(mod) is null && EoInstalled)
        {
            await RunModOperationAsync(() => Task.Run(() => Mods().SetEnabled(Eo, !EoEnabled)),
                S[EoEnabled ? "mods.turned_off" : "mods.turned_on"]);
            return;
        }
        await RunModOperationAsync(async () =>
        {
            var manager = ManagedMods();
            var receipt = manager.Receipt(mod);
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            if (receipt is null)
            {
                using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
                await manager.InstallAsync(ModDownloads, mod, timeout.Token, progress);
            }
            else await Task.Run(() => manager.SetEnabled(mod, !receipt.Enabled, progress));
        }, S["mods.action_done"]);
    }

    public Task RecoverModsAsync() => RunModOperationAsync(() => Task.Run(() => ManagedMods().Recover()), S["mods.recovered"]);

    private async Task RunModOperationAsync(Func<Task> operation, string done)
    {
        if (!ModsIdle) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        _modOperation = true; _modProgress = S["mods.progress.prepare"]; Busy = true;
        try { await operation(); Message = done; }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException or HttpRequestException
            or OperationCanceledException or JsonException or InvalidOperationException or KeyNotFoundException or ArgumentException or OverflowException or FormatException)
        {
            Log("mods: operation failed: " + error.GetType().Name);
            Message = S[error is HttpRequestException or OperationCanceledException ? "mods.download_failed" : "mods.transaction_failed"];
        }
        finally
        {
            _modOperation = false; Busy = false;
            RefreshMods(); RefreshModCards(); Changed(string.Empty);
        }
    }

    private void RefreshModCards()
    {
        ModCards.Clear();
        foreach (var mod in _manifestCatalog)
        {
            var status = S["mods.not_installed"];
            var action = S["mods.install"];
            var canAct = ModsIdle && _install is not null && mod.ApprovedForFileInstall && !mod.NeedsDatabaseCoordination
                && mod.Dependencies.Count == 0 && mod.Document.GetProperty("compatibility").GetProperty("azimuthVersions")
                    .EnumerateArray().Any(v => v.GetString() == _install.Version);
            try
            {
                if (_install is not null)
                {
                    var manager = ManagedMods();
                    if (manager.RecoveryRequired) { status = S["mods.recovery_needed"]; canAct = false; }
                    else if (manager.Receipt(mod) is { } receipt)
                    {
                        var verified = manager.Verified(mod);
                        status = verified ? S[receipt.Enabled ? "mods.managed_on" : "mods.managed_off"] : S["mods.files_changed"];
                        action = S[receipt.Enabled ? "mods.disable_action" : "mods.enable_action"];
                        canAct &= verified;
                    }
                    else if (mod.Id == Eo.Id && EoInstalled)
                    {
                        status = EoStatus + " · " + S["mods.legacy"];
                        action = S[EoEnabled ? "mods.disable_action" : "mods.enable_action"];
                        canAct &= EoOfficial;
                    }
                }
            }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException or JsonException)
            { canAct = false; status = S["mods.files_changed"]; }
            if (!mod.ApprovedForFileInstall) status = S["mods.review_blocked"];
            var requirements = mod.Dependencies.Count == 0 ? S["mods.no_dependencies"]
                : S["mods.dependencies"] + " " + string.Join(", ", mod.Dependencies.Select(d =>
                    (_manifestCatalog.FirstOrDefault(m => m.Id == d.Id)?.Text("name", S.Locale) ?? d.Id) + " " + string.Join(" / ", d.Versions)));
            if (mod.NeedsDatabaseCoordination || mod.Id == "fh2-mod-platform") requirements += "\n" + S["mods.database_blocked"];
            if (mod.Id == "nitrosystem") requirements += "\n" + S["mods.nitro_settings"];
            if (_install is null) requirements += "\n" + S["mods.choose_folder"];
            else if (mod.ApprovedForFileInstall && EoTarget is null) requirements += "\n" + EoAzimuthText;
            ModCards.Add(new(mod, mod.Text("name", S.Locale), mod.Text("description", S.Locale), status, requirements,
                action, canAct, mod.Id == Eo.Id && HasIni,
                mod.Document.GetProperty("author").GetString() + " · " + mod.Document.GetProperty("license").GetString() + " · " + mod.Version));
        }
    }
}
