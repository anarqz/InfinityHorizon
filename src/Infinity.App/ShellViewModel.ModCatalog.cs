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
    /// <summary>OD-114: a custom, unofficial mod from the player's custom folder (shown with its badge).</summary>
    public bool Custom => Manifest.IsCustom;
    public string CustomBadge { get; init; } = "";
    /// <summary>The custom-mod or unofficial-catalog badge is shown.</summary>
    public bool ShowSourceBadge => CustomBadge.Length > 0;
    /// <summary>The player still has to confirm this exact custom manifest (the update's, when one is pending) before acting.</summary>
    public bool NeedsConfirmation { get; init; }
}

/// <summary>What the background refresh learned about one manifest (M10: no disk access while building cards).</summary>
internal sealed record ModCardFacts(ModManifest Manifest, ModInstallation? Installed, bool CanAdopt, string? Blocker, ModManifest? Next, bool Confirmed = false);

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
    private int _customProblems;
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
    public bool ShowModInfo => _showModInfo && ShowMods && _selectedMod is not null && _externalMod is null;
    /// <summary>OD-117: an installed mod of the player's own files can be re-checked or pointed at another copy.</summary>
    public bool SelectedModExternal => ModsIdle && _selectedMod is { Installed: true } selected && selected.Manifest.IsExternal;
    public string SelectedModFiles => _selectedMod is not { } selected ? "" :
        S.Format("mods.file_summary", ("game", selected.Manifest.Files.Count(f => f.Root == "game").ToString(CultureInfo.CurrentCulture)),
            ("azimuth", selected.Manifest.Files.Count(f => f.Root == "azimuth").ToString(CultureInfo.CurrentCulture)))
        + (selected.Manifest.ConfigPath is { } config ? "\n" + S["mods.settings_file"] + " " + config : "");
    public bool SelectedModDrift => ModsIdle && _selectedMod is { Drift: true };
    public bool SelectedModCanDisableKeeping => ModsIdle && _selectedMod is { Drift: true, Enabled: true };
    public bool SelectedModSettingsIssues => ModsIdle && _selectedMod is { SettingsIssues: true };
    public bool SelectedModCanUninstall => ModsIdle && _selectedMod is { CanUninstall: true } && !ModRecoveryRequired;
    public bool SelectedModUpdate => _selectedMod is { UpdateAvailable: true, NeedsConfirmation: false };
    /// <summary>OD-114: the confirmation an unreviewed custom mod needs before its first install or enable (and each new digest).</summary>
    public bool SelectedModNeedsConfirmation => _selectedMod is { Custom: true, NeedsConfirmation: true };
    public string SelectedModConfirmation => _selectedMod is not { Custom: true } selected ? ""
        : S["mods.custom_confirm_text"] + ((selected.Next ?? selected.Manifest).HasExecutableCode ? "\n\n" + S["mods.custom_executable"] : "");
    public bool CanConfirmCustomMod => ModsIdle && _install is not null && !ModRecoveryRequired && SelectedModNeedsConfirmation;
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
        _selectedMod = card; _externalMod = null; // the set-up pane gives way to the chosen card
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
        _selectedMod = card; _showModInfo = true; _showModOptions = false; _showLobbyPane = false; _externalMod = null;
        Changed(string.Empty);
    }

    public void CloseModPanel()
    {
        _installPreviewDigest = null;
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
    /// <summary>
    /// Native (infinity_mod.dll) mods need the signed runtime and helper next to the launcher (<see cref="ModRuntimePackage"/>): development
    /// packages carry them; a production package only once WINDOWS ships them with a hook that hosts mods (OD-113, MODS.md §6).
    /// </summary>
    /// <remarks>OD-116: or installable, the catalog listing <c>infinity-mod-runtime</c> (installing a native mod installs it first).</remarks>
    public bool NativeModsAvailable => DevelopmentMods || _nativeRuntime is not null || _catalog?.Runtime is not null;
    /// <summary>A native mod installs its runtime with it: none verified yet, but the catalog lists one.</summary>
    private bool RuntimeComesWithMod => _nativeRuntime is null && _catalog?.Runtime is not null;
    public bool HasManifestCatalog => _catalogLoaded;
    public bool ModOperationBusy => _modOperation;
    public bool ModContentEnabled => !_modOperation;
    public string ModProgress => _modProgress;
    private bool CatalogExpired => _catalog?.Expired(DateTimeOffset.UtcNow) == true;
    public string ModCatalogStatus => _catalogFailure ?? (!_catalogLoaded ? S[_catalogLoading ? "mods.progress.catalog" : "mods.catalog_unloaded"]
        : CatalogExpired ? S["mods.catalog_expired"] : DevelopmentMods ? S["mods.catalog_dev"] : _catalog is { Trusted: false } ? S["catalog.unofficial_short"]
        : _catalog is { Cached: true } ? S["mods.catalog_cached"] : S["mods.catalog_production"]);
    public string ModCatalogSummary => S.Format((_catalog is { Trusted: true } ? "mods.catalog_count_official" : "mods.catalog_count") + (ModCards.Count == 1 ? "_one" : ""),
        ("count", ModCards.Count.ToString(CultureInfo.CurrentCulture))) + (_catalogFailure is { } failure ? " · " + failure : "")
        + (_customProblems > 0 ? " · " + Counted("mods.custom_problems", _customProblems) : "");
    /// <summary>OD-114: where the player puts custom mods, one folder per mod (shown on the Mods page).</summary>
    public string CustomModsFolderText => S.Format("mods.custom_folder", ("path", ManagedModFiles.CustomModsFolder(ModHome)))
        + (Infinity.Core.Process.WineHost.Detected && Infinity.Core.Process.WineHost.UnixPath(ManagedModFiles.CustomModsFolder(ModHome)) is { } unix
            ? "\n" + S.Format("mods.custom_folder_linux", ("path", unix)) : "");

    /// <summary>Opens the custom mods folder (created if missing): Explorer on Windows, the prefix's explorer under Wine/Proton.</summary>
    public void OpenCustomModsFolder()
    {
        try { Infinity.Core.Process.FolderOpener.Open(ManagedModFiles.CustomModsFolder(ModHome)); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidOperationException or System.ComponentModel.Win32Exception)
        {
            Log("mods: custom folder could not be opened: " + error.GetType().Name);
            Message = S["mods.custom_folder_failed"];
        }
    }

    private bool _showModUrl;
    private string _modUrl = "";
    /// <summary>Add from URL: the address field, shown under the Mods toolbar.</summary>
    public bool ShowModUrl => _showModUrl && ModsIdle;
    public string ModUrl
    {
        get => _modUrl;
        set { _modUrl = value ?? ""; Changed(); Changed(nameof(CanAddModFromUrl)); }
    }
    public bool CanAddModFromUrl => ModsIdle && CustomModUrl(_modUrl) is not null;
    /// <summary>The downloader's own rules (ModDownloads): HTTPS on the default port, no credentials or fragment.</summary>
    private static Uri? CustomModUrl(string text) => Uri.TryCreate(text.Trim(), UriKind.Absolute, out var url) && url.Scheme == Uri.UriSchemeHttps
        && url.IsDefaultPort && url.UserInfo.Length == 0 && url.Fragment.Length == 0 ? url : null;

    /// <summary>Where modders learn to adapt a mod to Infinity's schema (shown when a ZIP has no infinity-mod.json).</summary>
    public const string ModSchemaPage = "https://infinity-horizon.com/mods";
    private bool _modUrlIncompatible;
    /// <summary>The last ZIP had no infinity-mod.json: the field stays open with the warning and the schema page's link.</summary>
    public bool ModUrlIncompatible => _modUrlIncompatible && ShowModUrl;
    public string ModUrlIncompatibleText => S.Format("mods.error.archive_no_manifest", ("url", ModSchemaPage));

    public void ToggleModUrl()
    {
        _showModUrl = !_showModUrl; _modUrlIncompatible = false;
        Changed(string.Empty);
    }

    /// <summary>
    /// Add from URL: downloads a ZIP with one custom mod, places it as <c>mods/custom/&lt;its custom. id&gt;</c>, then shows its card so the
    /// player can read and confirm it. Adding never confirms nor installs it.
    /// </summary>
    public async Task AddModFromUrlAsync()
    {
        if (!CanAddModFromUrl) return;
        var url = CustomModUrl(_modUrl)!; var home = ModHome;
        CustomModImport? added = null;
        _modUrlIncompatible = false;
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S[stage == "download" ? "mods.progress.url_download" : "mods.progress.url_verify"]; Changed(nameof(ModProgress)); });
            using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(15));
            try { added = await ManagedModFiles.ImportCustomFromUrlAsync(ModDownloads, home, url, progress, timeout.Token); }
            catch (ModException error) when (error.Code == ModErrors.ArchiveNoManifest) { _modUrlIncompatible = true; throw; }
        }, S["mods.action_done"]);
        if (_modUrlIncompatible) { Message = ModUrlIncompatibleText; Changed(string.Empty); return; }
        if (added is null) return;
        Log("mods: custom mod added from URL: " + added.Manifest.Id + (added.Replaced ? " (replaced)" : added.Unchanged ? " (unchanged)" : ""));
        Message = S.Format(added.Unchanged ? "mods.url_unchanged" : added.Replaced ? "mods.url_replaced" : "mods.url_added", ("mod", added.Manifest.Text("name", S.Locale)));
        _modUrl = ""; _showModUrl = false;
        if (ModCards.FirstOrDefault(c => c.Manifest.Id == added.Manifest.Id) is { } card) ShowModInformation(card);
        Changed(string.Empty);
    }
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
    // OD-118 T1f: while the mod catalog loads or after it failed, Play says so (it carries the runtime native and data mods need; it never blocks Play).
    public string? ModPlayNotice => _install is null ? null : _modLaunchFailure is { } failure ? failure : ModPlayBlocked ? S["mods.play_blocked"]
        : !_catalogLoaded && _catalogLoading ? S["play.catalog_loading"]
        : !_catalogLoaded && _catalogFailure is not null ? S["play.catalog_failed"]
        : HardcoreRefusedMods is { Count: > 0 } refused ? HardcoreRefusal(refused)
        : _modStateError is not null ? S["mods.state_unreadable"] : _modHealth.HasWarnings ? S["mods.play_warning"] : null;
    public bool ShowModPlayNotice => ModPlayNotice is not null && !Playing;

    /// <summary>OD-118 T1f: the catalog failed and is not loading: PLAY offers a retry next to the reason.</summary>
    /// <summary>Mods: the catalog failed (and none is shown): Try again and "Use a different catalog…" (T2b).</summary>
    public bool ModCatalogFailed => _catalogFailure is not null && !_catalogLoaded && !DevelopmentMods;

    public bool ShowCatalogRetry => _install is not null && !_catalogLoaded && !_catalogLoading && _catalogFailure is not null && !Playing;

    public string ModFolder => _install?.Root ?? S["setup.not_found"];

    /// <summary>The stable launcher data home: the bucket (game_backup) and the custom mods folder live here, shared by every profile.</summary>
    private string ModHome => _config.Production ? Directory.GetParent(_stateDirectory)!.FullName : _stateDirectory;

    /// <summary>The bucket lives in the stable launcher data home (shared by development and production profiles).</summary>
    private Func<ManagedModFiles>? ModManagerFactory()
    {
        if (_install is null) return null;
        var azimuth = _install.Root; var game = ContentLocation?.Mount;
        var state = ModHome;
        var cache = DevelopmentMods ? Environment.GetEnvironmentVariable("FI_DEV_MOD_PACKAGE_CACHE") : null;
        var dataSql = DataModSqlReady;
        return () => new ManagedModFiles(azimuth, game, state, GameRunning, cache) { DataSqlRuntimeAvailable = () => dataSql };
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

    /// <summary>The catalog could not be loaded, naming the host it was read from (the official one is GitHub; a custom one is its own).</summary>
    private string CatalogFailureText => S.Format("mods.catalog_failed_host", ("host", CatalogUri.IsFile ? "file" : CatalogUri.Host));

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
        if (!loaded)
        {
            _catalogFailure = CatalogFailureText;
            if (Message == S["mods.download_failed"]) Message = _catalogFailure; // T2b: a network failure names the catalog's host; a refusal keeps its reason
        }
        Changed(string.Empty);
    }

    /// <summary>On opening Mods and once at startup: the cached signed catalog at once, then the network, without the overlay; failures only say so.</summary>
    private async Task LoadModCatalogQuietlyAsync()
    {
        if (_catalogLoading) return;
        _catalogLoading = true;
        try
        {
            var address = CatalogUri;
            if (!DevelopmentMods && !_catalogLoaded && await Task.Run(() => OfficialModCatalog.LoadCached(address, CatalogCache)) is { } cached && address == CatalogUri)
            {
                ApplyCatalog(cached);
                await RefreshModsAsync(); // OD-115: the cached catalog's requirements decide Play while the network fetch runs
            }
            await FetchCatalogAsync();
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
        {
            Log("mods: catalog unavailable: " + ErrorChain(error) + (error is ModException mod ? " " + mod.Code : ""));
            _catalogFailure = CatalogFailureText;
            if (!_catalogLoaded) _ = RetryCatalogOnceAsync(); // T1f: a first-start fetch failure (seen under Proton) gets one more try
        }
        finally { _catalogLoading = false; Changed(string.Empty); }
        await RefreshModsAsync();
    }

    private bool _catalogRetryScheduled;

    /// <summary>OD-118 T1f: one delayed retry of the quiet catalog load after it failed (GitHub, never Infinity), once per run.</summary>
    private async Task RetryCatalogOnceAsync()
    {
        if (_catalogRetryScheduled) return;
        _catalogRetryScheduled = true;
        try { await Task.Delay(GitHubRetryDelay, _updateStop.Token); }
        catch (OperationCanceledException) { return; }
        if (_catalogLoaded || Playing || !ModsIdle) return;
        Log("mods: retrying the catalog once");
        await LoadModCatalogQuietlyAsync();
    }

    /// <summary>OD-118 T1f: the quiet catalog load when the shell opens without one (Play offline, or the game folder found later).</summary>
    private void EnsureModCatalog()
    {
        if (_install is not null && InShell && !_catalogLoaded && !_catalogLoading && !DevelopmentMods) _ = LoadModCatalogQuietlyAsync();
    }

    /// <summary>
    /// A failure's cause for the log (T1f: why GitHub fails on a first start under Proton): each exception's type and message, inner
    /// ones included. URL queries and IP addresses are removed (logs never carry them).
    /// </summary>
    internal static string ErrorChain(Exception error)
    {
        var parts = new List<string>();
        for (var e = error; e is not null && parts.Count < 5; e = e.InnerException)
        {
            var message = System.Text.RegularExpressions.Regex.Replace(e.Message, @"(https?://[^\s?#]+)[?#]\S*", "…");
            message = System.Text.RegularExpressions.Regex.Replace(message, @"\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b|\[[0-9A-Fa-f:]{3,}\](:\d+)?", "<ip>");
            parts.Add(e.GetType().Name + ": " + (message.Length > 160 ? message[..160] + "…" : message));
        }
        return string.Join(" <- ", parts);
    }


    /// <summary>OD-116: the player's catalog ("Use a different catalog"), else the official file on the launcher repository's main.</summary>
    private Uri CatalogUri => CustomCatalog ?? OfficialModCatalog.OfficialUrl;
    private string CatalogCache => Path.Combine(_stateDirectory, "mod-catalog-v2");

    /// <summary>Tests only: replaces the network catalog fetch (returns the catalog, or throws as a failed fetch).</summary>
    internal Func<CancellationToken, Task<ModCatalog>>? CatalogFetchForTests { get; set; }

    private async Task FetchCatalogAsync()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        ModCatalog catalog;
        if (CatalogFetchForTests is { } fake) catalog = await fake(timeout.Token);
        else if (DevelopmentMods)
        {
            if (!Uri.TryCreate(Environment.GetEnvironmentVariable("FI_DEV_MOD_CATALOG_URL"), UriKind.Absolute, out var uri))
                throw new ModException("invalid catalog URL", ModErrors.Catalog);
            catalog = await DevelopmentModCatalog.LoadAsync(ModDownloads, uri, timeout.Token);
        }
        else
        {
            var address = CatalogUri;
            catalog = await OfficialModCatalog.LoadAsync(ModDownloads, address, CatalogCache, timeout.Token);
            // A fetch for an address the player switched away from meanwhile never replaces the chosen catalog (and its warnings).
            if (address != CatalogUri) { Log("mods: dropped a catalog fetched for a previous address"); return; }
        }
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
        var key = NativeModKey;
        var home = ModHome;
        var (health, error, facts, problems) = await Task.Run(() => ReadModState(factory, catalog, home));
        _nativeRuntime = await Task.Run(() => LocateRuntime(home, key));
        if (version != _modRefresh) return;
        if (error is not null && error != _modStateError) Log("mods: state could not be read: " + error);
        _modHealth = health; _modStateError = error; _modFacts = facts;
        if (problems.Count > 0 && problems.Count != _customProblems)
            Log("mods: custom folders skipped: " + string.Join(", ", problems.Select(p => p.Folder + " " + p.Reason)));
        _customProblems = problems.Count;
        RefreshModCards();
        if (_selectedMod is { HasOptions: true } && !_iniDirty) await LoadSelectedOptionsAsync();
        Changed(string.Empty);
    }

    private static (ModHealth Health, string? Error, IReadOnlyList<ModCardFacts> Facts, IReadOnlyList<CustomModProblem> Problems) ReadModState(
        Func<ManagedModFiles>? factory, IReadOnlyList<ModManifest> official, string home)
    {
        // OD-114: custom mods are rescanned on every refresh and listed with the official catalog (their ids can never collide).
        CustomModScan custom;
        try { custom = ManagedModFiles.ScanCustomMods(home); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { custom = new([], [new("", ManagedModFiles.Code(error))]); }
        var catalog = official.Concat(custom.Mods.Where(m => official.All(o => o.Id != m.Id))).ToArray();
        if (factory is null) return (ModHealth.Empty, null, catalog.Select(m => new ModCardFacts(m, null, false, null, null)).ToArray(), custom.Problems);
        ManagedModFiles manager;
        try { manager = factory(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        { return (ModHealth.Empty, ManagedModFiles.Code(error), catalog.Select(m => new ModCardFacts(m, null, false, ManagedModFiles.Code(error), null)).ToArray(), custom.Problems); }
        bool Confirmed(ModManifest m) { try { return m.IsCustom && manager.CustomConfirmed(m); } catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return false; } }
        var health = manager.Inspect();
        var facts = new List<ModCardFacts>();
        foreach (var listed in catalog)
        {
            var installed = health.Installed.FirstOrDefault(i => i.Id == listed.Id);
            // OD-116: a catalog entry rebuilds its manifest, so the reviewed manifest a 1.0.9 installed has other bytes for the same
            // content; the installed one stands for it (no update to review, receipts and settings stay bound to it).
            var manifest = installed?.Manifest is { } same && same.SameContent(listed) ? same : listed;
            if (installed?.Manifest is { } current && current.Digest != manifest.Digest)
            {
                facts.Add(new(current, installed, false, Blocker(() => { if (installed.Enabled) { manager.CheckDependencies(manifest); manager.CheckDependents(manifest.Id, manifest); } }), manifest, Confirmed(manifest)));
                continue;
            }
            var adopt = false;
            if (installed is null) try { adopt = manager.CanAdopt(manifest); } catch (Exception error) when (ManagedModFiles.IsStateError(error)) { }
            facts.Add(new(manifest, installed, adopt, installed?.Problem ?? Blocker(() =>
            {
                if (installed is { Enabled: true }) manager.CheckDependents(manifest.Id);
                else if (installed is null || installed.Drift.Count == 0) { manager.CheckDependencies(manifest); manager.CheckDatabaseBase(manifest); }
            }), null, Confirmed(manifest)));
        }
        foreach (var installed in health.Installed.Where(i => i.Manifest is not null && catalog.All(m => m.Id != i.Id)))
            facts.Add(new(installed.Manifest!, installed, false, Blocker(() => { if (installed.Enabled) manager.CheckDependents(installed.Id); else manager.CheckDependencies(installed.Manifest!); }), null, Confirmed(installed.Manifest!)));
        return (health, null, facts, custom.Problems);
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
            // Custom executable code runs only through the development runtime until custom signing is designed (OD-114).
            var nativeBlocked = mod.IsNative && !(mod.IsCustom ? DevelopmentMods : NativeModsAvailable);
            var custom = mod.IsCustom;
            string status, badge, action; bool canAct;
            if (_modHealth.RecoveryHere) { status = S["mods.recovery_needed"]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (installed?.Problem is not null) { status = S["mods.recovery_problems"]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (fact.Next is { } next)
            {
                status = S.Format("mods.update_available", ("from", mod.Version), ("to", next.Version));
                badge = S["mods.update"]; action = S["mods.review_update"];
                canAct = (custom ? !next.CustomBlocked : !expired) && next.SupportedForInstall && !(next.IsNative && !(custom ? DevelopmentMods : NativeModsAvailable))
                    && fact.Blocker is null && installed!.Drift.Count == 0;
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
                status = expired ? S["mods.catalog_expired"] : nativeBlocked ? S["mods.native_unavailable"]
                    : mod.IsExternal ? S.Format("external.card_status", ("hint", mod.ExternalSources[0].Hint ?? mod.ExternalSources[0].LabelText(S.Locale)))
                    : mod.NeedsRuntime && !custom && RuntimeComesWithMod ? S["mods.runtime_with_mod"] : S["mods.not_installed"];
                // OD-117: a mod of the player's own files opens its set-up pane instead of downloading anything.
                badge = S[mod.IsExternal ? "external.badge" : "mods.not_installed"]; action = S[mod.IsExternal ? "external.setup" : "mods.install"];
                // A custom mod needs no catalog (it has no download); an official one waits for a fresh catalog.
                canAct = compatible && !nativeBlocked && fact.Blocker is null && (custom ? !mod.CustomBlocked : !expired && _catalog is not null);
            }
            if (fact.Blocker is not null && installed?.Problem is null && fact.Next is null && !(installed is { Enabled: true } && installed.Drift.Count > 0))
            { status = ModErrorText(fact.Blocker); badge = S["mods.badge_attention"]; }
            if ((!mod.SupportedForInstall || mod.CustomBlocked) && installed is null) { status = S["mods.review_blocked"]; badge = S["mods.badge_review"]; canAct = false; }
            // FIMOD004: a data mod's SQL needs the runtime and hook that apply it; until then listed, never installed or enabled.
            if (((fact.Next ?? mod).HasDatabaseSql || (fact.Next ?? mod).DatabaseBase is not null) && !DataModSqlInstallable && !(installed is { Enabled: true } && fact.Next is null))
            { status = S["mods.data_sql_runtime"]; badge = S["mods.badge_review"]; canAct = false; }
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
                // OD-116: every mod of a player-chosen catalog says so, and a native one that it runs code with the game.
                CustomBadge = custom ? S["mods.custom_badge"]
                    : UsingCustomCatalog && _manifestCatalog.Any(m => m.Id == mod.Id) || installed is not null && UnofficialSource(mod.Id) is not null
                    ? S[mod.IsNative ? "catalog.badge_native" : "catalog.badge"] : "",
                // Disabling never needs it; installing, enabling and each update's new digest do.
                NeedsConfirmation = custom && !fact.Confirmed && !(installed is { Enabled: true } && fact.Next is null),
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
        // OD-114 follow-up: one database base at a time, naming the enabled mod that already replaces it
        ModException { Code: ModErrors.DatabaseBase, Detail: { } other } => S.Format("mods.error.database_base_named", ("mod",
            _modFacts.FirstOrDefault(f => f.Manifest.Id == other)?.Manifest.Text("name", S.Locale) ?? other)),
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
        if (current.UpdateAvailable || current.NeedsConfirmation) { ShowModInformation(current); return; } // reviewed (and a custom mod confirmed) first, in the information pane
        if (!current.Installed && current.Manifest.IsExternal) { await OpenExternalSetupAsync(current); return; }
        if (NeedsInstallPreview(current)) { ShowInstallPreview(current); return; } // Alex (1.0.11): where it goes and what it replaces, first
        var mod = current.Manifest;
        var factory = ModManagerFactory()!;
        var hosts = CatalogHosts;
        var hardcore = ProfileHardcore;
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            if (mod.NeedsRuntime && !(current.Installed && current.Enabled)) await RequireRuntimeAsync(); // OD-116: installing a native mod installs its runtime
            var manager = await Task.Run(ModManagerFactory() ?? factory); // after the runtime step: a data mod sees the runtime it installed
            if (!current.Installed && current.Action == S["mods.adopt"]) await Task.Run(() => manager.Adopt(mod, hardcore));
            else if (!current.Installed && mod.IsCustom) await Task.Run(() => manager.InstallCustom(mod, progress, hardcore: hardcore));
            else if (!current.Installed)
            {
                using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
                await manager.InstallAsync(ModDownloads, mod, hosts, timeout.Token, progress, hardcore: hardcore);
                RecordModSource(mod.Id);
            }
            else await Task.Run(() => manager.SetEnabled(mod, !current.Enabled, progress, hardcore));
        }, S["mods.action_done"]);
    }

    /// <summary>This catalog's download host rule (OD-116: any HTTPS host, every download pinned); nothing without a catalog.</summary>
    private Func<string, bool> CatalogHosts => _catalog?.AllowsHost ?? (_ => false);

    /// <summary>A native or SQL data mod's runtime, installed from the catalog when missing or changed; refused when none verifies.</summary>
    private async Task RequireRuntimeAsync()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
        if (await EnsureRuntimeAsync(timeout.Token) is null) throw new ModException("no verified mod runtime", ModErrors.RuntimeMissing);
    }

    public Task ConfirmModUpdateAsync()
    {
        if (_selectedMod is not { UpdateAvailable: true, CanAct: true, Next: { } next } card) return Task.CompletedTask;
        if (next.IsExternal) return OpenExternalSetupAsync(card, update: true); // after its review; only files whose pins changed are asked for again
        var factory = ModManagerFactory()!; var hosts = CatalogHosts; var hardcore = ProfileHardcore;
        return RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            if (next.NeedsRuntime) await RequireRuntimeAsync();
            using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
            var manager = await Task.Run(ModManagerFactory() ?? factory); // after the runtime step: a data mod sees the runtime it installed
            if (next.IsCustom) await Task.Run(() => manager.InstallCustom(next, progress, update: true, hardcore: hardcore));
            else { await manager.InstallAsync(ModDownloads, next, hosts, timeout.Token, progress, update: true, hardcore: hardcore); RecordModSource(next.Id); }
        }, S["mods.updated"]);
    }

    /// <summary>
    /// OD-114: the player's explicit confirmation of an unreviewed custom mod, recorded for its exact manifest digest (the pending
    /// update's when there is one); then the card's action continues (an update still shows its reviewed change first).
    /// </summary>
    public async Task ConfirmCustomModAsync()
    {
        if (!CanConfirmCustomMod || _selectedMod is not { } selected || ModManagerFactory() is not { } factory) return;
        var target = selected.Next ?? selected.Manifest;
        var confirmed = false;
        await RunModOperationAsync(async () => { await Task.Run(() => factory().ConfirmCustom(target)); confirmed = true; }, S["mods.custom_confirmed"]);
        if (!confirmed || ModCards.FirstOrDefault(c => c.Manifest.Id == selected.Manifest.Id) is not { } card) return;
        if (card.UpdateAvailable) ShowModInformation(card);
        else { _showModInfo = false; await ActOnModAsync(card); }
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
