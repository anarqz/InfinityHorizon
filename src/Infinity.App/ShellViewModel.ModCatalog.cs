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
    /// <summary>T5d: the dependency this card waits for; its action installs (or enables) that mod first.</summary>
    public ModManifest? Dependency { get; init; }
    public bool Enabled { get; init; }
    public bool Drift { get; init; }
    public bool SettingsIssues { get; init; }
    public bool UpdateAvailable { get; init; }
    public string UpdateSummary { get; init; } = "";
    public ModManifest? Next { get; init; }
    public bool CanUninstall => Installed && !Drift;
    /// <summary>Not installed, its files kept on this PC by the uninstall (installing again needs no download); Purge deletes them.</summary>
    public bool Held { get; init; }
    /// <summary>OD-232: an installation in a format the launcher no longer supports (disable or remove only).</summary>
    public bool Retired => Manifest.Retired;
    /// <summary>OD-116: the unofficial-catalog badge.</summary>
    public string SourceBadge { get; init; } = "";
    public bool ShowSourceBadge => SourceBadge.Length > 0;
    /// <summary>The version line under the name ("v1.2.0"; a <see cref="BuiltIn"/> card says it is part of Infinity).</summary>
    public string VersionText { get; init; } = "";
    /// <summary>OD-125/127: part of Infinity (ExtraOptions.Infinity): always there and on; only its options change.</summary>
    public bool BuiltIn { get; init; }
    public bool ShowAction => !BuiltIn;
}

/// <summary>What the background refresh learned about one manifest (M10: no disk access while building cards).</summary>
internal sealed record ModCardFacts(ModManifest Manifest, ModInstallation? Installed, bool CanAdopt, string? Blocker, ModManifest? Next)
{
    /// <summary>Not installed, with its files kept on this PC by its uninstall.</summary>
    public bool Held { get; init; }
}

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
    public ObservableCollection<ModCard> ModCards { get; } = [];
    /// <summary>The mod cards shown on their own: every card but the packs a set card holds (OD-232, <see cref="ModSetCards"/>).</summary>
    // owner 2026-11-10: one list the window keeps, changed in place (ShellViewModel.ModCardsView.cs); the set members once, not per card.
    // ModCards holds the chosen game's cards only (RefreshModCards), so a game switch syncs the shown list to that game's cards in place.
    public IEnumerable<ModCard> FilteredModCards => ShownModCards(ModCards.Where(MatchesSearch).ExceptBy(SetMemberIds, card => card.Manifest.Id));
    public string ModSearch
    {
        get => _modSearch;
        set
        {
            _modSearch = value ?? ""; Changed(); Changed(nameof(FilteredModCards)); Changed(nameof(FilteredModSetCards)); Changed(nameof(ModSearchEmpty));
            Changed(nameof(NoMatchingMods));
        }
    }
    public bool ModSearchEmpty => string.IsNullOrWhiteSpace(_modSearch);
    public bool HasModCards => ModCards.Count > 0;
    public bool NoMatchingMods => HasModCards && !FilteredModCards.Any() && !FilteredModSetCards.Any();
    public ModCard? SelectedMod => _selectedMod;
    public bool ShowModInfo => _showModInfo && ShowMods && _selectedMod is not null && _externalMod is null;
    /// <summary>OD-117: an installed mod of the player's own files can be re-checked or pointed at another copy.</summary>
    public bool SelectedModExternal => ModsIdle && _selectedMod is { Installed: true, Retired: false } selected && selected.Manifest.IsExternal;
    public string SelectedModFiles => _selectedMod is not { } selected ? "" :
        S.Format("mods.file_summary", ("game", selected.Manifest.Files.Count(f => f.Root == "game").ToString(CultureInfo.CurrentCulture)),
            ("azimuth", selected.Manifest.Files.Count(f => f.Root == "azimuth").ToString(CultureInfo.CurrentCulture)))
        + (selected.Manifest.ConfigPath is { } config ? "\n" + S["mods.settings_file"] + " " + config : "");
    public bool SelectedModDrift => ModsIdle && _selectedMod is { Drift: true };
    public bool SelectedModCanDisableKeeping => ModsIdle && _selectedMod is { Drift: true, Enabled: true };
    public bool SelectedModSettingsIssues => ModsIdle && _selectedMod is { SettingsIssues: true };
    public bool SelectedModCanUninstall => ModsIdle && _selectedMod is { CanUninstall: true } && !ModRecoveryRequired;
    public bool SelectedModCanPurge => ModsIdle && _selectedMod is { Held: true, Installed: false } && !ModRecoveryRequired;
    public bool SelectedModUpdate => _selectedMod is { UpdateAvailable: true, Retired: false };
    public bool CanConfirmModUpdate => ModsIdle && _selectedMod is { UpdateAvailable: true, CanAct: true } selected
        && (selected.Next is not { IsExternal: false } next || PreviewKnown(next));

    /// <summary>
    /// A card's click: its options (an installed mod with settings, the BuiltIn options card) or its information. Owner 2026-11-10
    /// ("clicking on the built-in card's OPTIONS shows nothing"): reading is always safe, so both panes open whatever else runs (a game-file check,
    /// an update check, a mod operation); only Save and Undo wait, and the pane says why (<see cref="ModOptionsWait"/>). Never silent.
    /// </summary>
    public async Task SelectModAsync(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        // owner 2026-11-10 (build #3): opening a card reads nothing but its options file; the cards are the last read's (the start's, Play's,
        // ↻, or the one-mod read after an operation), so a click never reads every mod again
        card = ModCards.FirstOrDefault(current => current.Manifest.Id == card.Manifest.Id) ?? card;
        _selectedMod = card; _externalMod = null; // the set-up pane gives way to the chosen card
        _showModInfo = !card.HasOptions;
        _showModOptions = card.HasOptions;
        if (card.HasOptions && !(_iniDirty && _iniManifest == card.Manifest.Digest)) await LoadSelectedOptionsAsync();
        // the options could not be read: the information pane instead (with the reason in the message bar), never an empty sidebar
        if (card.HasOptions && _ini is null) { _showModOptions = false; _showModInfo = true; }
        Changed(string.Empty);
    }

    public void SelectMod(ModCard card) => _ = GuardAsync("open a mod", () => SelectModAsync(card));

    public void ShowModInformation(ModCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        _selectedMod = card; _showModInfo = true; _showModOptions = false; _externalMod = null;
        // Alex (1.0.11): the information pane of a mod not installed says where it goes and what it replaces, with its Install
        if (!card.Installed && !card.Manifest.IsExternal && card.Action != S["mods.adopt"]) { _installPreviewDigest = card.Manifest.Digest; _previewReplaced = null; }
        Changed(string.Empty);
    }

    /// <summary>Why Save and Undo of the options wait now (empty when they don't): what the launcher is busy with.</summary>
    public string ModOptionsWait => ModsIdle ? "" : S.Format("mods.options_wait", ("reason", S[ModsWaitReason]));
    public bool HasModOptionsWait => ModOptionsWait.Length > 0;

    /// <summary>
    /// A click's work (the window's handlers await it): an exception is logged and said in the message bar, never lost in a
    /// fire-and-forget task.
    /// </summary>
    internal async Task GuardAsync(string what, Func<Task> work)
    {
        try { await work(); }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Log("ui: " + what + " failed: " + ErrorChain(error));
            Message = S["mods.click_failed"];
            Changed(string.Empty);
        }
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
    public bool HasManifestCatalog => _catalogLoaded;
    public bool ModOperationBusy => _modOperation;
    public bool ModContentEnabled => !ModOperationBusy;
    public string ModProgress => _modProgress;
    /// <summary>
    /// The Mods page's own progress dialog, only when the window's overlay is not up: a mod operation runs under the screen lock
    /// (<see cref="RunModOperationAsync"/>, which shows the progress), so one operation never shows two indicators.
    /// </summary>
    public bool ShowModProgressOverlay => ModOperationBusy && !ShowBusyOverlay;

    /// <summary>What the running mod operation is doing now: the Mods dialog's text, and the screen lock's under its label.</summary>
    private void ModStep(string text)
    {
        _modProgress = text;
        Changed(nameof(ModProgress));
        if (_modOperation && _lockDepth > 0 && _modLockLabel is { } what) BusyStep(what + "\n" + text);
    }

    /// <summary>The running mod operation's screen-lock label ("Installing …"), or null.</summary>
    private string? _modLockLabel;

    /// <summary>A mod's name for the lock labels.</summary>
    private string ModName(ModManifest mod) => mod.Text("name", S.Locale);
    private bool CatalogExpired => _catalog?.Expired(DateTimeOffset.UtcNow) == true;
    public string ModCatalogStatus => _catalogFailure ?? (!_catalogLoaded ? S[_catalogLoading ? "mods.progress.catalog" : "mods.catalog_unloaded"]
        : CatalogExpired ? S["mods.catalog_expired"] : DevelopmentMods ? S["mods.catalog_dev"] : _catalog is { Trusted: false } ? S["catalog.unofficial_short"]
        : _catalog is { Cached: true } ? S["mods.catalog_cached"] : S["mods.catalog_production"]);
    public string ModCatalogSummary => S.Format((_catalog is { Trusted: true } ? "mods.catalog_count_official" : "mods.catalog_count") + (ModCards.Count == 1 ? "_one" : ""),
        ("count", ModCards.Count.ToString(CultureInfo.CurrentCulture))) + (_catalogFailure is { } failure ? " · " + failure : "")
        + (ModsReading ? " · " + S["jobs.mods_read"] : "");
    public string ModsSubtitle => S[_catalogLoaded ? "mods.catalog_subtitle" : "mods.subtitle"];

    /// <summary>Owner 2026-11-10: the page names its game ("Mods · Forza Motorsport 5").</summary>
    public string ModsTitle => S.Format("mods.title_game", ("game", GameLabel));

    /// <summary>The chosen game's mods folder, relative to Azimuth (<c>mods\Forza Motorsport 5</c>, OD-233).</summary>
    public string ModsGameFolder => ExtraOptionsLayout.HasModsFolder(CurrentGame.Id)
        ? S.Format("mods.game_folder", ("path", ExtraOptionsMods.ModsFolder + "\\" + ExtraOptionsLayout.GameFolder(CurrentGame.Id))) : "";

    /// <summary>
    /// Owner 2026-11-10: the game chips of the Mods page are PLAY's tiles (one selection: choosing a game here is choosing it on PLAY, and
    /// the reverse), so they are <see cref="GameCards"/> themselves.
    /// </summary>
    public bool ShowModGameChips => GameCards.Count > 1;

    /// <summary>A chip: the same choice as PLAY's tile (<see cref="SelectPlayGame"/>); the page reads the chosen game's mods again.</summary>
    public void SelectModsGame(GameCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!card.Enabled) { Message = card.Reason.Length > 0 ? card.Reason : S["play.tile_not_in_install"]; return; }
        if (Playing && card.Info.Id != _gate.Game) { Message = S["mods.wait.playing"]; return; }
        SelectPlayGame(card);
    }

    /// <summary>The catalog is loaded and lists nothing for the chosen game: "No catalog mods for Forza Motorsport 5 yet".</summary>
    public bool ShowModsEmptyForGame => _catalogLoaded && !ModsReading && string.IsNullOrWhiteSpace(_modSearch)
        && !ModCards.Any(c => !c.BuiltIn) && ModSetCards.Count == 0;
    public string ModsEmptyForGame => S.Format("mods.empty_game", ("game", GameLabel));

    /// <summary>Any interrupted operation in the bucket: mod changes wait for it (Play only for this folder, <see cref="ModPlayBlocked"/>).</summary>
    public bool ModRecoveryRequired => _modHealth.RecoveryHere || _modHealth.RecoveryElsewhere;
    /// <summary>The only mod state that stops ordinary Play: an interrupted change of this Azimuth folder, always shown with its recovery.</summary>
    public bool ModPlayBlocked => _install is not null && _modHealth.BlocksPlay;
    // Every panel the Mods page shows has an enabled action that addresses what it says (checked state by state by the harness):
    // - "Interrupted mod operation": an interrupted operation (Recover, Resolve) or a record whose own content is broken (Resolve
    //   archives it, ModInstallation.ResolveClears, the check Resolve itself runs);
    // - "Mods need attention": a read that failed for now, unreadable mod state, a record of another game folder, a moved installation
    //   (Try again, always; Relink and Open Setup where they apply); intact records are never archived;
    // - "Moving mods to per-game folders": the old flat layout (OD-233), never an unreadable record (Move mods now).
    public bool ShowModRecovery => ModsIdle && (ModRecoveryRequired || UnreadableMods.Length > 0);
    public bool CanRecoverMods => ModsIdle && ModRecoveryRequired;
    public bool CanResolveMods => ModsIdle && (ModRecoveryRequired || UnreadableMods.Length > 0);
    public bool HasModRelink => _modHealth.RelinkCandidates.Count > 0 || _modHealth.GameRelink;
    public bool ShowModAttention => ModsIdle && (_modHealth.Problems.Count > 0 || _modStateError is not null || RetryMods.Length > 0
        || WaitingForGameFolderMods.Length > 0 || HasModRelink);
    /// <summary>"Try again" re-reads the mod state; shown with every attention state (a failed read may clear by itself).</summary>
    public bool CanRetryMods => ModsIdle;
    /// <summary>A record of a game folder that is not the selected one: Setup's game folder (or the game it belongs to) serves it.</summary>
    public bool ShowModsOpenSetup => InShell && WaitingForGameFolderMods.Length > 0;
    /// <summary>Installed mods whose record Resolve archives (files untouched): named by their manifest, else the catalog's entry, else their id.</summary>
    private string[] UnreadableMods => ProblemNames(i => i.ResolveClears);
    /// <summary>Installed mods whose record could not be read for now (a file open elsewhere, access refused): read again, never archived.</summary>
    private string[] RetryMods => ProblemNames(i => i.Retry);
    /// <summary>Records of a game folder that is not chosen (or not the selected one): Setup's game folder, not Resolve, serves them.</summary>
    private string[] WaitingForGameFolderMods => ProblemNames(i => i.Problem == ModErrors.Relink && !_modHealth.GameRelink);
    private string[] ProblemNames(Func<ModInstallation, bool> which) => _modHealth.Installed.Where(i => i.Problem is not null && which(i))
        .Select(i => (i.NamedBy ?? _manifestCatalog.FirstOrDefault(m => m.Id == i.Id))?.Text("name", S.Locale) ?? i.Id).ToArray();
    public string ModRecoveryText => string.Join("\n", new[]
    {
        _modHealth.RecoveryHere ? S["mods.recovery_here"] : null,
        _modHealth.RecoveryElsewhere ? S["mods.recovery_elsewhere"] : null,
        UnreadableMods is { Length: > 0 } unreadable ? S.Format("mods.recovery_problems_named", ("mods", string.Join(", ", unreadable))) : null,
    }.OfType<string>());
    public string ModAttentionText => string.Join("\n", new[]
    {
        RetryMods is { Length: > 0 } retry ? S.Format("mods.attention_retry", ("mods", string.Join(", ", retry))) : null,
        _modHealth.Problems.Count > 0 || _modStateError is not null ? S["mods.attention_state"] : null,
        WaitingForGameFolderMods is { Length: > 0 } waiting ? S.Format("mods.attention_game_folder", ("mods", string.Join(", ", waiting))) : null,
        HasModRelink ? S["mods.relink_hint"] : null,
    }.OfType<string>());

    /// <summary>"Try again": the mod state read again (a file that was open elsewhere, a refused read); nothing is changed.</summary>
    public async Task RetryModStateAsync()
    {
        if (!ModsIdle) { Message = S[ModsWaitReason]; return; }
        await RefreshModsAsync();
        Message = S[ShowModAttention ? "mods.retry_still" : "mods.retry_done"];
    }

    /// <summary>
    /// OD-233: the mods folder still has the old flat layout (or its move stopped): Mods says so, with the reason of a stop, and offers the
    /// move (the one Play runs first). Not while the catalog that names a hand-copied folder's game is still loading.
    /// </summary>
    public bool ShowModsLayout => ModsIdle && _install is not null && (_modsLayoutFailure is not null
        || _modHealth.Installed.Any(i => i.Problem == ModErrors.Layout) || _modHealth.LayoutPending && !_catalogLoading);
    public string ModsLayoutText => _modsLayoutFailure is { } stopped ? S["mods.layout_pending"] + "\n" + stopped : S["mods.layout_pending"];

    /// <summary>"Move mods now": the move to the per-game folders, as Play runs it (with the catalog's games when it is loaded).</summary>
    public async Task MoveModsLayoutAsync()
    {
        if (_install is null) return;
        bool? moved = null;
        // a Lock (the player asked for it); the move runs inside it with no indicator of its own
        await RunModOperationAsync(async () => moved = await PrepareModsLayoutAsync(receiptsOnly: false, background: false, inline: true),
            S["mods.layout_moved"], label: S["jobs.mods_layout"]);
        if (moved == false && _modsLayoutFailure is { } stopped) { Message = stopped; Changed(string.Empty); }
    }

    /// <summary>After Recover: the move an interrupted operation held back (OD-233) runs now, as at startup.</summary>
    private async Task AfterModRecoveryAsync()
    {
        if (_install is null || !ModsIdle || GameRunning()) return;
        await PrepareModsLayoutAsync(receiptsOnly: !_catalogLoaded, background: true);
        await RefreshModsAsync();
    }
    /// <summary>OD-112 follow-up: the selected profile is Hardcore, which allows no mod that affects multiplayer.</summary>
    private bool ProfileHardcore => LobbyProfile?.Mode == "hardcore";
    /// <summary>Enabled mods a Hardcore profile refuses (<see cref="ModMultiplayer.AffectsMultiplayer"/>, the lobby-set rule), by name.</summary>
    private IReadOnlyList<string> HardcoreRefusedMods => !ProfileHardcore ? [] : _modHealth.Installed
        .Where(i => i.Enabled && i.Manifest is { } m && ModMultiplayer.AffectsMultiplayer(m) && m.ModsGame == CurrentGame.Id).Select(i => i.Manifest!.Text("name", S.Locale)).ToArray();
    private string HardcoreRefusal(IEnumerable<string> names) => S.Format("mods.hardcore_refused", ("mods", string.Join(", ", names)));
    /// <summary>OD-232: enabled installations of a retired format an online launch refuses (<see cref="ModLaunchPreparation.BlocksOnline"/>), by name.</summary>
    private IReadOnlyList<string> RetiredOnlineMods => !PlaysOnline ? [] : _modHealth.Installed
        .Where(i => i.Enabled && i.Manifest is { } m && ModLaunchPreparation.BlocksOnline(m)).Select(i => i.Manifest!.Text("name", S.Locale)).ToArray();
    private string RetiredOnlineRefusal(IEnumerable<string> names) => S.Format("mods.retired_online_refused", ("mods", string.Join(", ", names)));

    /// <summary>Shown on Play: why Play waits for mods, or that some need attention (never a silent block).</summary>
    // OD-118 T1f: while the mod catalog loads or after it failed, Play says so (it never blocks Play).
    public string? ModPlayNotice => _install is null ? null : _modsLayoutFailure is { } layout ? layout : _modLaunchFailure is { } failure ? failure : ModPlayBlocked ? S["mods.play_blocked"]
        : !_catalogLoaded && _catalogLoading ? S["play.catalog_loading"]
        : !_catalogLoaded && _catalogFailure is not null ? S["play.catalog_failed"]
        : HardcoreRefusedMods is { Count: > 0 } refused ? HardcoreRefusal(refused)
        : RetiredOnlineMods is { Count: > 0 } retired ? RetiredOnlineRefusal(retired)
        : _modStateError is not null ? S["mods.state_unreadable"] : _modHealth.HasWarnings ? S["mods.play_warning"] : null;
    public bool ShowModPlayNotice => ModPlayNotice is not null && !Playing;

    /// <summary>OD-118 T1f: the catalog failed and is not loading: PLAY offers a retry next to the reason.</summary>
    /// <summary>Mods: the catalog failed (and none is shown): Try again and "Use a different catalog…" (T2b).</summary>
    public bool ModCatalogFailed => _catalogFailure is not null && !_catalogLoaded && !DevelopmentMods;

    public bool ShowCatalogRetry => _install is not null && !_catalogLoaded && !_catalogLoading && _catalogFailure is not null && !Playing;

    public string ModFolder => _install?.Root ?? S["setup.not_found"];

    /// <summary>The stable launcher data home: the bucket (game_backup) lives here, shared by every profile.</summary>
    // The game's mods are shared by every server's state: production and custom (CR-L39) state folders sit under the root.
    private string ModHome => _config.Production ? Directory.GetParent(_stateDirectory)!.FullName
        : _config.Custom ? Directory.GetParent(Directory.GetParent(_stateDirectory)!.FullName)!.FullName : _stateDirectory;

    /// <summary>The bucket lives in the stable launcher data home (shared by development and production profiles).</summary>
    private Func<ManagedModFiles>? ModManagerFactory()
    {
        if (_install is null) return null;
        var azimuth = _install.Root; var game = ContentLocation?.Mount;
        var state = ModHome;
        var cache = DevelopmentMods ? Environment.GetEnvironmentVariable("FI_DEV_MOD_PACKAGE_CACHE") : null;
        return () => new ManagedModFiles(azimuth, game, state, GameRunning, cache);
    }

    /// <summary>Constructs the manager; only for code already off the UI thread or the harness.</summary>
    private ManagedModFiles ManagedMods() => (ModManagerFactory() ?? throw new ModException("Azimuth folder not selected"))();

    public async Task OpenDevelopmentModsAsync(string? azimuth)
    {
        if (!DevelopmentMods || !ModsIdle) return;
        if (azimuth is not null) await SetInstallAsync(await Task.Run(() => Infinity.Core.Process.AzimuthInstall.Detect(azimuth)));
        else await FindAzimuthAsync(null, openSetupWhenNeeded: false);
        _onMods = true; _onSetup = false; _onBugReport = false; _onNews = false;
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
            ModStep(S["mods.progress.catalog"]);
            await FetchCatalogAsync();
            loaded = true;
        }, S["mods.catalog_loaded"], label: S["jobs.catalog"]);
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
                // OD-233: the cached signed catalog names a hand-copied folder's game as well as the fresh one does
                if (_install is not null && !Playing && ModsIdle && !GameRunning()) await PrepareModsLayoutAsync(receiptsOnly: false, background: true);
                await RefreshModsAsync(); // OD-115: the cached catalog's requirements decide Play while the network fetch runs
            }
            // owner 2026-11-10: "Users should not get blocked from interacting with the launcher while the mod catalog is loading": a
            // background toast (no PLAY hold); a failure also stays in it with Retry
            await RunJobOrThrowAsync("mod-catalog", S["jobs.catalog"], Infinity.Core.Shell.JobPresentation.Background, blocksPlay: false, _ => FetchCatalogAsync());
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
        {
            Log("mods: catalog unavailable: " + ErrorChain(error) + (error is ModException mod ? " " + mod.Code : ""));
            _catalogFailure = CatalogFailureText;
            if (!_catalogLoaded) _ = RetryCatalogOnceAsync(); // T1f: a first-start fetch failure (seen under Proton) gets one more try
        }
        finally { _catalogLoading = false; Changed(string.Empty); }
        // OD-233: with the catalog, an unmanaged folder's game is known: the per-game move runs now (receipts decide first)
        if (_install is not null && _catalogLoaded && !Playing && !GameRunning()) await PrepareModsLayoutAsync(receiptsOnly: false, background: true);
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


    /// <summary>OD-116: the player's catalog ("Use a different catalog"), else the official file on InfinityMods' main (OD-166).</summary>
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

    /// <summary>The requests that arrived while a read ran: the next read serves them all (it starts after each of them).</summary>
    private TaskCompletionSource? _modReadPending;
    private bool _modReadRunning;

    /// <summary>A mod state read is running or queued (the page says so instead of an empty "0 mods").</summary>
    public bool ModsReading => _modReadRunning;

    /// <summary>Test seam: a pause inside every mod state read (a slow disk, 44 large mods to hash), off the UI thread.</summary>
    internal TimeSpan ModStateReadDelayForTests { get; set; }

    /// <summary>Test seam: how many reads have been applied to the page.</summary>
    internal int ModReadsApplied { get; private set; }

    /// <summary>How long a read runs before it shows its toast ("Reading your mods…").</summary>
    private static readonly TimeSpan ModReadToastAfter = TimeSpan.FromMilliseconds(400);

    /// <summary>
    /// M10: reads mod state off the UI thread, then builds cards from that snapshot. One read at a time: a request while one runs is served
    /// by the next read, which starts after it (so the returned task ends with state at least as new as the request). Every read that ends
    /// is shown. 3.1.1 field bug (owner 2026-11-10, 44 installed mods): each refresh started its own read and only the newest was kept, so
    /// while the start-up, the game choice and the catalog kept asking, every read (hashing every mod file) was dropped and the page showed
    /// "0 mods", not even the BuiltIn card. A read longer than <see cref="ModReadToastAfter"/> is a bottom-right toast (no PLAY hold).
    /// </summary>
    internal Task RefreshModsAsync()
    {        // the built-in card (and whatever is already known) is there at once, never an empty page while the first read runs
        if (ModCards.Count == 0) { try { RefreshModCards(); } catch (Exception error) when (error is not OutOfMemoryException) { Log("mods: cards not built: " + ErrorChain(error)); } }
        var pending = _modReadPending ??= new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_modReadRunning) _modReadLoop = ModReadLoopAsync();
        return pending.Task;
    }

    /// <summary>The running read's place: installations read so far and in all (0 of 0 before it reaches the receipts).</summary>
    private (int Done, int Total) _modReadProgress;

    /// <summary>The running read's toast, to report its progress to; null when the read is quick (no toast).</summary>
    private Infinity.Core.Shell.JobContext? _modReadToast;

    /// <summary>Owner 2026-11-10: the Mods page says the read is running, with its progress ("Reading your mods… (3 of 44)").</summary>
    public string ModsReadingText => _modReadProgress.Total > 0
        ? S.Format("mods.reading_progress", ("done", Math.Min(_modReadProgress.Done + 1, _modReadProgress.Total).ToString(CultureInfo.CurrentCulture)),
            ("total", _modReadProgress.Total.ToString(CultureInfo.CurrentCulture)))
        : S["jobs.mods_read"];
    public bool ModsReadingHasProgress => _modReadProgress.Total > 0;
    public bool ModsReadingIndeterminate => !ModsReadingHasProgress;
    public double ModsReadingPercent => _modReadProgress.Total > 0 ? 100.0 * _modReadProgress.Done / _modReadProgress.Total : 0;

    private void OnModReadProgress((int Done, int Total) step)
    {
        if (!_modReadRunning) return;
        _modReadProgress = step;
        _modReadToast?.Report(step.Total > 0 ? (double)step.Done / step.Total : null, ModsReadingText);
        Changed(nameof(ModsReadingText)); Changed(nameof(ModsReadingHasProgress)); Changed(nameof(ModsReadingIndeterminate)); Changed(nameof(ModsReadingPercent));
    }

    private async Task ModReadLoopAsync()
    {
        _modReadRunning = true; _modReadProgress = (0, 0);
        Changed(nameof(ModsReading)); Changed(nameof(ModCatalogSummary)); Changed(nameof(ModsReadingText)); Changed(nameof(ModsReadingHasProgress));
        TaskCompletionSource? toastDone = null;
        try
        {
            while (_modReadPending is { } batch)
            {
                _modReadPending = null;
                var read = ReadAndApplyModsAsync();
                if (toastDone is null && await Task.WhenAny(read, Task.Delay(ModReadToastAfter)) != read)
                {
                    toastDone = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
                    var done = toastDone;
                    // the toast's work starts a moment later: it takes the progress only while this loop still runs (a read that ended meanwhile
                    // never leaves a finished toast as the progress target), and ends with the loop, whatever ends it
                    StartBackgroundJob("mods-read", S["jobs.mods_read"], blocksPlay: false, context =>
                    {
                        if (!done.Task.IsCompleted && _modReadRunning) _modReadToast = context;
                        return done.Task;
                    });
                }
                try { await read; }
                catch (Exception error) when (error is not OutOfMemoryException) { Log("mods: page not refreshed: " + ErrorChain(error)); }
                finally { batch.TrySetResult(); }
            }
        }
        finally
        {
            // every end (done, failed, superseded by nothing left to read): the toast and the page's progress card go away
            _modReadRunning = false; _modReadToast = null; _modReadProgress = (0, 0);
            toastDone?.TrySetResult();
            Changed(nameof(ModsReading)); Changed(nameof(ModCatalogSummary)); Changed(nameof(ModsReadingText));
            Changed(nameof(ModsReadingHasProgress)); Changed(nameof(ModsReadingIndeterminate)); Changed(nameof(ModsReadingPercent));
        }
    }

    /// <summary>Where the page's kept file hashes are persisted between launches (3.1.1: the first read of 45 large mods took 61 s).</summary>
    private string KeptHashesFile => Path.Combine(ModHome, "mods-hash-cache.json");
    private bool _keptHashesLoaded;

    private async Task ReadAndApplyModsAsync()
    {
        var factory = ModManagerFactory();
        var catalog = _manifestCatalog;
        var delay = ModStateReadDelayForTests;
        var watch = System.Diagnostics.Stopwatch.StartNew();
        var keptFile = KeptHashesFile;
        var loadKept = !_keptHashesLoaded && factory is not null;
        _keptHashesLoaded |= loadKept;
        var hashedBefore = ManagedModFiles.KeptHashesComputed;
        IProgress<(int, int)> progress = new Progress<(int Done, int Total)>(OnModReadProgress); // posts to the UI thread
        var (health, error, facts, phases) = await Task.Run(() =>
        {
            var phases = new System.Text.StringBuilder();
            var phase = System.Diagnostics.Stopwatch.StartNew();
            void Phase(string name) { phases.Append(name).Append(' ').Append(phase.ElapsedMilliseconds).Append(" ms, "); phase.Restart(); }
            if (loadKept) { var loaded = ManagedModFiles.LoadKeptHashes(keptFile); Phase("kept hashes loaded (" + loaded + ")"); }
            // the test pause is spread over the installations, as a slow disk is
            Action<int, int> step = delay > TimeSpan.Zero
                ? (done, total) => { progress.Report((done, total)); if (done < total) Thread.Sleep(delay / Math.Max(total, 1)); }
                : (done, total) => progress.Report((done, total));
            if (delay > TimeSpan.Zero && factory is null) Thread.Sleep(delay);
            var read = ReadModState(factory, catalog, step, Phase);
            if (factory is not null) { var saved = ManagedModFiles.SaveKeptHashes(keptFile); Phase(saved ? "kept hashes saved" : "kept hashes unchanged"); }
            return (read.Health, read.Error, read.Facts, phases.ToString());
        });
        var hashed = ManagedModFiles.KeptHashesComputed - hashedBefore;
        // owner 2026-11-10: a slow read says where its time went (phases), so the next one can be made faster
        if (watch.ElapsedMilliseconds >= 1000)
            Log("mods: state read in " + watch.ElapsedMilliseconds + " ms (" + health.Installed.Count + " installed, " + catalog.Count + " in the catalog, "
                + hashed + " files hashed; " + phases.TrimEnd(' ', ',') + ")");
        ModReadsForTests.Add((watch.Elapsed, hashed, catalog.Count));
        if (ModReadsForTests.Count > 64) ModReadsForTests.RemoveAt(0);
        if (error is not null && error != _modStateError) Log("mods: state could not be read: " + error);
        _modHealth = health; _modStateError = error; _modFacts = facts; _modFactsCatalog = catalog;
        ModReadsApplied++;
        // 3.1.1 field bug: a read that began before the catalog arrived ("0 in the catalog") is read again with it, so its cards appear
        if (!ReferenceEquals(catalog, _manifestCatalog)) _modReadPending ??= new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        try
        {
            NoteDuplicateDiscordMod(); // OD-136: once, when Bullet4fun's Discord RPC is installed
            RefreshModCards();
            await RefreshExtraOptionsModsAsync();
            if (_selectedMod is { HasOptions: true } && !_iniDirty) await LoadSelectedOptionsAsync();
        }
        finally { Changed(string.Empty); }
    }

    /// <summary>Test seam: every applied read's duration, files hashed (not answered from the kept hashes) and catalog size, in order.</summary>
    internal List<(TimeSpan Took, long Hashed, int Catalog)> ModReadsForTests { get; } = [];

    private static (ModHealth Health, string? Error, IReadOnlyList<ModCardFacts> Facts) ReadModState(Func<ManagedModFiles>? factory, IReadOnlyList<ModManifest> official,
        Action<int, int>? progress = null, Action<string>? phase = null)
    {
        // never an exception out of the read: what cannot be read is the page's attention panel (Try again), and the cards still show
        try { return ReadModStateUnguarded(factory, official, progress, phase ?? (_ => { })); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        {
            return (ModHealth.Empty, ManagedModFiles.Code(error), official.Select(m => new ModCardFacts(m, null, false, ManagedModFiles.Code(error), null)).ToArray());
        }
    }

    private static (ModHealth Health, string? Error, IReadOnlyList<ModCardFacts> Facts) ReadModStateUnguarded(Func<ManagedModFiles>? factory, IReadOnlyList<ModManifest> official,
        Action<int, int>? progress, Action<string> phase)
    {
        // Owner 2026-10-28: the catalog's mods only; a mod installed earlier that the catalog no longer lists (OD-232: a retired format
        // too) still has its card from its receipt (below), to disable or uninstall
        var catalog = official.ToArray();
        if (factory is null) return (ModHealth.Empty, null, catalog.Select(m => new ModCardFacts(m, null, false, null, null)).ToArray());
        ManagedModFiles manager;
        try { manager = factory(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        { return (ModHealth.Empty, ManagedModFiles.Code(error), catalog.Select(m => new ModCardFacts(m, null, false, ManagedModFiles.Code(error), null)).ToArray()); }
        // OD-125/127: a receipt from when the catalog installed ExtraOptions as a mod no longer stands for anything (it is built in)
        try { manager.RetireBuiltInReceipt(); } catch (Exception error) when (ManagedModFiles.IsStateError(error)) { }
        phase("manager");
        // 3.1.1 (45 mods, 61 s): from here this manager only reads for the cards: each receipt once, kept hashes, nothing written
        manager.ForDisplay = true;
        var health = manager.Inspect(progress);
        phase("installations read (" + health.Installed.Count + ")");
        var facts = new List<ModCardFacts>();
        foreach (var listed in catalog) facts.Add(ListedFact(manager, listed, health.Installed.FirstOrDefault(i => i.Id == listed.Id)));
        foreach (var installed in health.Installed.Where(i => i.Manifest is not null && catalog.All(m => m.Id != i.Id)))
            facts.Add(UnlistedFact(manager, installed));
        phase("cards (" + facts.Count + ")");
        return (health, null, facts);
    }

    /// <summary>What one catalog entry's card needs (the full read and the one-mod read after an operation decide it the same way).</summary>
    private static ModCardFacts ListedFact(ManagedModFiles manager, ModManifest listed, ModInstallation? installed)
    {
        // OD-116: a catalog entry rebuilds its manifest, so the reviewed manifest a 1.0.9 installed has other bytes for the same
        // content; the installed one stands for it (no update to review, receipts and settings stay bound to it).
        var manifest = installed?.Manifest is { } same && same.SameContent(listed) ? same : listed;
        if (installed?.Manifest is { } current && current.Digest != manifest.Digest)
            return new(current, installed, false, Blocker(() => { if (installed.Enabled) { manager.CheckDependencies(manifest); manager.CheckDependents(manifest.Id, manifest); } }), manifest);
        var adopt = false; var held = false;
        if (installed is null) try { adopt = manager.CanAdopt(manifest); held = manager.Held(manifest.Id) is not null; } catch (Exception error) when (ManagedModFiles.IsStateError(error)) { }
        return new(manifest, installed, adopt, installed?.Problem ?? Blocker(() =>
        {
            if (installed is { Enabled: true }) manager.CheckDependents(manifest.Id);
            else if (installed is null || installed.Drift.Count == 0) manager.CheckDependencies(manifest);
        }), null) { Held = held };
    }

    /// <summary>An installed mod the catalog no longer lists: its card from its receipt, to disable or uninstall.</summary>
    private static ModCardFacts UnlistedFact(ManagedModFiles manager, ModInstallation installed) => new(installed.Manifest!, installed, false, Blocker(() =>
    {
        if (installed.Enabled) manager.CheckDependents(installed.Id);
        else if (!installed.Manifest!.Retired) manager.CheckDependencies(installed.Manifest!); // OD-232: a retired one is never enabled
    }), null);

    /// <summary>Names of the enabled mods that depend on <paramref name="id"/> (T5d).</summary>
    private List<string> DependentNames(string id) => _modHealth.Installed
        .Where(i => i.Enabled && i.Manifest is { } m && m.Id != id && m.Dependencies.Any(d => d.Id == id))
        .Select(i => i.Manifest!.Text("name", S.Locale)).ToList();

    /// <summary>Names of <paramref name="mod"/>'s dependencies that are not installed and enabled (T5d).</summary>
    // OD-125: ExtraOptions comes with Infinity (ExtraOptions.Infinity); it is never offered as a dependency to install
    private ModManifest? MissingDependency(ModManifest mod) => mod.Dependencies
        .Where(d => d.Id != ManagedModFiles.BuiltInExtraOptions && !_modHealth.Installed.Any(i => i.Id == d.Id && i.Enabled))
        .Select(d => _manifestCatalog.FirstOrDefault(m => m.Id == d.Id && d.Versions.Contains(m.Version))).FirstOrDefault(m => m is not null);

    private List<string> MissingDependencyNames(ModManifest mod) => mod.Dependencies
        .Where(d => d.Id != ManagedModFiles.BuiltInExtraOptions && !_modHealth.Installed.Any(i => i.Id == d.Id && i.Enabled))
        .Select(d => _manifestCatalog.FirstOrDefault(m => m.Id == d.Id)?.Text("name", S.Locale) ?? d.Id).ToList();

    private static string? Blocker(Action check)
    {
        try { check(); return null; }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return ManagedModFiles.Code(error); }
    }

    private void RefreshModCards()
    {
        ModCards.Clear();
        // OD-125/127: ExtraOptions is part of Infinity (ExtraOptions.Infinity, the hook): pinned first, always on, no install, disable or
        // removal; its options are the only thing to change (with the Azimuth folder known)
        var game = CurrentGame;
        var extraOptions = Infinity.Core.Process.ExtraOptionsInfinity.ManifestFor(game);
        ModCards.Add(new(extraOptions, extraOptions.Text("name", S.Locale), extraOptions.Text("description", S.Locale),
            S.Format("mods.builtin_extraoptions", ("file", game.ExtraOptionsIni)),
            S["mods.no_dependencies"], "", false, _install is not null,
            extraOptions.Document.GetProperty("author").GetString() + " · " + extraOptions.Document.GetProperty("license").GetString() + " · " + extraOptions.Version,
            S["mods.badge_builtin"])
        { BuiltIn = true, Enabled = true, VersionText = S.Format("mods.builtin_version", ("version", extraOptions.Version)) });
        var expired = CatalogExpired;
        foreach (var fact in _modFacts)
        {
            // whatever an older catalog still lists for it
            if (fact.Manifest.Id == ManagedModFiles.BuiltInExtraOptions) continue;
            // W11: a mod belongs to the games it declares (none declared: FH2); an update that moves it between games is judged by the new manifest
            if (!(fact.Next ?? fact.Manifest).IsForGame(game.Id)) continue;
            var mod = fact.Manifest; var installed = fact.Installed;
            var compatible = _install is not null && mod.Document.GetProperty("compatibility").GetProperty("azimuthVersions").EnumerateArray().Any(v => v.GetString() == _install.Version);
            string status, badge, action; bool canAct; ModManifest? dependency = null;
            if (_modHealth.RecoveryHere) { status = S["mods.recovery_needed"]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (installed?.Problem is not null) { status = S[installed.Problem switch { ModErrors.Layout => "mods.layout_pending", ModErrors.Relink => "mods.card_other_folder", _ when installed.ResolveClears => "mods.recovery_problems", _ => "mods.card_retry" }]; badge = S["mods.badge_attention"]; action = S["mods.install"]; canAct = false; }
            else if (installed is not null && mod.Retired)
            {
                // OD-232: a retired format (game-folder files, a native or data mod, a custom mod): disable or remove only, never
                // enable or update; removing it restores what it replaced
                status = S[installed.Drift.Count > 0 ? "mods.drift_warning" : "mods.retired"]; badge = S["mods.badge_retired"];
                action = S[installed.Enabled ? "mods.disable_action" : "mods.enable_action"];
                canAct = installed.Enabled && installed.Drift.Count == 0 && fact.Blocker is null;
            }
            else if (fact.Next is { } next)
            {
                status = S.Format("mods.update_available", ("from", mod.Version), ("to", next.Version));
                badge = S["mods.update"]; action = S["mods.review_update"];
                canAct = !expired && next.SupportedForInstall && fact.Blocker is null && installed!.Drift.Count == 0;
            }
            else if (installed is not null)
            {
                var drift = installed.Drift.Count > 0;
                badge = S[drift || installed.SettingsIssues.Count > 0 ? "mods.badge_attention" : installed.Enabled ? "mods.badge_on" : "mods.badge_off"];
                status = drift ? S["mods.drift_warning"] : installed.SettingsIssues.Count > 0 ? S["mods.settings_issues"] : S[installed.Enabled ? "mods.managed_on" : "mods.managed_off"];
                action = S[installed.Enabled ? "mods.disable_action" : "mods.enable_action"];
                canAct = fact.Blocker is null && !(installed.Enabled && drift);
            }
            else if (fact.CanAdopt)
            {
                status = S["mods.unmanaged"]; badge = status; action = S["mods.adopt"];
                canAct = compatible && fact.Blocker is null;
            }
            else
            {
                status = expired ? S["mods.catalog_expired"]
                    : mod.IsExternal ? S.Format("external.card_status", ("hint", mod.ExternalSources[0].Hint ?? mod.ExternalSources[0].LabelText(S.Locale)))
                    : S["mods.not_installed"];
                // OD-117: a mod of the player's own files opens its set-up pane instead of downloading anything.
                badge = S[mod.IsExternal ? "external.badge" : "mods.not_installed"]; action = S[mod.IsExternal ? "external.setup" : "mods.install"];
                // An official mod waits for a fresh catalog.
                canAct = compatible && fact.Blocker is null && !expired && _catalog is not null;
            }
            if (fact.Blocker is not null && installed?.Problem is null && fact.Next is null && !mod.Retired && !(installed is { Enabled: true } && installed.Drift.Count > 0))
            {
                // T5d: a dependency state says which mod, never a generic "Needs attention". An enabled mod others depend on is fine:
                // it keeps its badge, and only its Disable waits.
                var dependents = fact.Blocker == ModErrors.Dependency && installed is { Enabled: true } ? DependentNames(mod.Id) : [];
                var missing = fact.Blocker == ModErrors.Dependency && installed is not { Enabled: true } ? MissingDependencyNames(mod) : [];
                // The first missing dependency the catalog offers: the card's action installs (or enables) it, with its own preview.
                dependency = installed is null && missing.Count > 0 ? MissingDependency(mod) : null;
                if (dependency is not null)
                {
                    var off = _modHealth.Installed.Any(i => i.Id == dependency.Id && !i.Enabled);
                    action = S.Format(off ? "mods.enable_dependency" : "mods.install_dependency", ("mod", dependency.Text("name", S.Locale)));
                    canAct = true;
                }
                if (dependents.Count > 0) status = S.Format("mods.depended_on_by", ("mods", string.Join(", ", dependents)));
                else if (missing.Count > 0)
                {
                    var names = string.Join(", ", missing);
                    badge = S.Format("mods.badge_needs", ("mod", names)); status = S.Format("mods.needs_dependency", ("mods", names));
                }
                else { status = ModErrorText(fact.Blocker); badge = S["mods.badge_attention"]; }
            }
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
                action, canAct, mod.ConfigPath is not null && installed is not null && installed.Problem is null && !mod.Retired,
                mod.Document.GetProperty("author").GetString() + " · " + mod.Document.GetProperty("license").GetString() + " · " + mod.Version, badge)
            {
                VersionText = "v" + mod.Version,
                Installed = installed is not null && installed.Problem is null, Enabled = installed?.Enabled == true, Dependency = dependency,
                Held = installed is null && fact.Held,
                Drift = installed?.Drift.Count > 0, SettingsIssues = installed?.SettingsIssues.Count > 0,
                UpdateAvailable = fact.Next is not null && !mod.Retired, Next = mod.Retired ? null : fact.Next,
                UpdateSummary = fact.Next is { } upcoming && !mod.Retired ? UpdateSummary(mod, upcoming) : "",
                // OD-116: every mod of a player-chosen catalog says so.
                SourceBadge = UsingCustomCatalog && _manifestCatalog.Any(m => m.Id == mod.Id) || installed is not null && UnofficialSource(mod.Id) is not null
                    ? S["catalog.badge"] : "",
            });
        }
        if (_selectedMod is { } selected) _selectedMod = ModCards.FirstOrDefault(card => card.Manifest.Id == selected.Manifest.Id);
        RefreshModSetCards(); // OD-232: the sets' packs, as rows of one card per set
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
        if (!ModsIdle || _install is null) { if (_install is not null) Message = S[ModsWaitReason]; return; }
        // owner 2026-11-10 (3.1.1 build #3): the card's button acts on the state the page shows (the operation checks the files again under
        // its lock); no read of every mod first, and no side pane
        var current = ModCards.FirstOrDefault(c => c.Manifest.Digest == card.Manifest.Digest);
        if (current is not { CanAct: true }) return;
        // T5d: a card waiting for a dependency installs (or enables) that dependency first, as that card's own button does.
        if (current.Dependency is { } needed && ModCards.FirstOrDefault(c => c.Manifest.Id == needed.Id) is { } first) { await ActOnModAsync(first); return; }
        if (_installConfirmedDigest != current.Manifest.Digest) _previewReplaced = null; // what is there now, never an earlier preview's
        if (current.UpdateAvailable)
        {
            // M5: the reviewed change first, in the small confirmation (a player's-files mod: its own set-up pane, after the review)
            if (current.Next is { IsExternal: true }) { ShowModInformation(current); return; }
            if (current.Next is { } next) await ReplacedByAsync(next);
            OpenModConfirm(current, update: true); return;
        }
        if (!current.Installed && current.Manifest.IsExternal) { await OpenExternalSetupAsync(current); return; }
        // Alex (1.0.11): an install that replaces files already there names them first (the small confirmation); otherwise it installs
        if (NeedsInstallPreview(current) && (await ReplacedByAsync(current.Manifest)).Count > 0) { OpenModConfirm(current, update: false); return; }
        if (!ModsIdle) { Message = S[ModsWaitReason]; return; }
        var mod = current.Manifest;
        var factory = ModManagerFactory()!;
        var hosts = CatalogHosts;
        var hardcore = ProfileHardcore;
        var adopt = !current.Installed && current.Action == S["mods.adopt"];
        var what = S.Format(adopt ? "mods.lock.adopt" : !current.Installed ? "mods.lock.install" : current.Enabled ? "mods.lock.disable" : "mods.lock.enable",
            ("mod", ModName(mod)));
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => ModStep(S["mods.progress." + stage]));
            var manager = await Task.Run(factory);
            if (adopt) await Task.Run(() => manager.Adopt(mod, hardcore));
            else if (!current.Installed)
            {
                using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
                await manager.InstallAsync(ModDownloads, mod, hosts, timeout.Token, progress, hardcore: hardcore);
                RecordModSource(mod.Id);
            }
            else await Task.Run(() => manager.SetEnabled(mod, !current.Enabled, progress, hardcore));
        }, S["mods.action_done"], label: what, only: [mod.Id]);
    }

    /// <summary>This catalog's download host rule (OD-116: any HTTPS host, every download pinned); nothing without a catalog.</summary>
    private Func<string, bool> CatalogHosts => _catalog?.AllowsHost ?? (_ => false);

    public Task ConfirmModUpdateAsync() => _selectedMod is { } selected ? UpdateModAsync(selected) : Task.CompletedTask;

    /// <summary>The reviewed update of <paramref name="card"/> (the information pane's or the small confirmation's button).</summary>
    private Task UpdateModAsync(ModCard card)
    {
        if (ModCards.FirstOrDefault(c => c.Manifest.Id == card.Manifest.Id) is { } latest) card = latest;
        if (card is not { UpdateAvailable: true, CanAct: true, Next: { } next }) return Task.CompletedTask;
        if (next.IsExternal) return OpenExternalSetupAsync(card, update: true); // after its review; only files whose pins changed are asked for again
        var factory = ModManagerFactory()!; var hosts = CatalogHosts; var hardcore = ProfileHardcore;
        return RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => ModStep(S["mods.progress." + stage]));
            using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3));
            var manager = await Task.Run(factory);
            await manager.InstallAsync(ModDownloads, next, hosts, timeout.Token, progress, update: true, hardcore: hardcore);
            RecordModSource(next.Id);
        }, S["mods.updated"], label: S.Format("mods.lock.update", ("mod", ModName(next))), only: [next.Id]);
    }

    private Task SelectedModOperation(Action<ManagedModFiles, ModManifest> operation, string done, string labelKey)
    {
        if (_selectedMod is not { } selected || ModManagerFactory() is not { } factory) return Task.CompletedTask;
        return RunModOperationAsync(() => Task.Run(() => operation(factory(), selected.Manifest)), done,
            label: S.Format(labelKey, ("mod", ModName(selected.Manifest))), only: [selected.Manifest.Id]);
    }

    public Task AcceptModFilesAsync() => SelectedModOperation((m, mod) => m.AcceptCurrentFiles(mod), S["mods.accepted"], "mods.lock.accept");
    public Task DisableModKeepingFilesAsync() => SelectedModOperation((m, mod) => m.DisableKeepingCurrentFiles(mod), S["mods.disabled_kept"], "mods.lock.disable_keep");
    public Task RepairModSettingsAsync() => SelectedModOperation((m, mod) => m.RepairSettings(mod), S["mods.repaired"], "mods.lock.repair");
    public Task UninstallModAsync() => SelectedModOperation((m, mod) => m.Uninstall(mod.Id), S["mods.uninstalled"], "mods.lock.uninstall");
    public Task PurgeModAsync() => SelectedModOperation((m, mod) => m.Purge(mod.Id), S["mods.purged"], "mods.lock.purge");

    private Task ManagerOperation(Action<ManagedModFiles> operation, string done, string label) =>
        ModManagerFactory() is { } factory ? RunModOperationAsync(() => Task.Run(() => operation(factory())), done, label: label) : Task.CompletedTask;

    public async Task RecoverModsAsync() { await ManagerOperation(m => m.Recover(), S["mods.recovered"], S["mods.lock.recover"]); await AfterModRecoveryAsync(); }
    // Resolve changes no mod file, so no move follows it: a pending move keeps its own panel and "Move mods now"
    public Task ResolveModRecoveryAsync() => ManagerOperation(m => m.ResolveRecovery(), S["mods.resolved"], S["mods.lock.resolve"]);
    public Task RelinkModsAsync() => ManagerOperation(m =>
    {
        foreach (var folder in m.RelinkCandidates().Take(1)) m.Relink(folder);
        if (m.GameRelinkCandidates().Count > 0) m.RelinkGame();
    }, S["mods.relinked"], S["mods.lock.relink"]);

    /// <summary>Retention at startup (M5): in the background, only while nothing else runs; a refusal is only logged.</summary>
    private async Task MaintainModsAsync()
    {
        if (ModManagerFactory() is not { } factory || !ModsIdle || GameRunning()) return;
        try
        {
            // owner 2026-11-10: retention is a background toast (no PLAY hold)
            await RunJobOrThrowAsync("mods-retention", S["jobs.retention"], Infinity.Core.Shell.JobPresentation.Background, blocksPlay: false,
                _ => Task.Run(() => factory().Prune(DateTimeOffset.UtcNow)));
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { Log("mods: retention skipped: " + ManagedModFiles.Code(error)); }
    }

    /// <summary>Why a mod operation cannot run now (<see cref="ModsIdle"/> is false), as an i18n key.</summary>
    private string ModsWaitReason => Playing ? "mods.wait.playing" : LauncherUpdateBusy ? "mods.wait.update" : IntegrityChecking ? "mods.wait.integrity" : _modOperation ? "mods.wait.busy" : "mods.wait.launcher";

    /// <summary>
    /// Every mod change the player asks for (install, install all, enable/disable, update, uninstall, recover, resolve, relink, settings,
    /// "Move mods now"…): owner 2026-11-10, "SOME loads must lock the screen, for example, enabling/disabling 40 mods at once with INSTALL
    /// ALL". A Lock job of <see cref="Jobs"/>: the window's overlay with <paramref name="label"/> (what), its "keep the launcher open"
    /// line (why) and the operation's steps (<see cref="ModStep"/>), open before the first await; refused with its reason while
    /// something else runs (<see cref="ModsWaitReason"/>).
    /// Owner 2026-11-10 (build #3, "every action I take, install, enable, disable, makes a new read of all mods"): an operation on one mod
    /// names it in <paramref name="only"/>, and afterwards only that mod (and the cards its dependencies tie to it) is read again
    /// (<see cref="RefreshModEntriesAsync"/>); bulk operations (a set, Recover, Resolve, Relink, the layout move, the catalog) leave it null
    /// and read every mod once.
    /// </summary>
    private async Task RunModOperationAsync(Func<Task> operation, string done, bool preserveOptionsOnFailure = false, string? label = null,
        IReadOnlyCollection<string>? only = null)
    {
        if (!ModsIdle) { Message = S[ModsWaitReason]; return; } // never a silent no-op: the player learns what it waits for
        // the short per-game move (OD-233), when one runs, finishes first (its toast shows it meanwhile)
        if (_layoutGate.CurrentCount == 0) { await _layoutGate.WaitAsync(); _layoutGate.Release(); }
        if (!ModsIdle) { Message = S[ModsWaitReason]; return; }
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        var what = label ?? S["mods.lock.working"];
        _modOperation = true; _modLockLabel = what; _modProgress = S["mods.progress.prepare"]; Busy = true;
        var succeeded = false;
        try
        {
            await RunJobOrThrowAsync("mods-operation", what, Infinity.Core.Shell.JobPresentation.Lock, blocksPlay: true, _ => operation());
            Message = done; succeeded = true;
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
        {
            Log("mods: operation failed: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : ""));
            Message = ModErrorText(error);
        }
        finally
        {
            _modOperation = false; _modLockLabel = null; Busy = false;
            if (succeeded && preserveOptionsOnFailure) _iniDirty = false; // a saved draft reloads from the file; a failed one stays
            if (only is not null) await RefreshModEntriesAsync(only);
            else await RefreshModsAsync();
            Changed(string.Empty);
        }
    }
}
