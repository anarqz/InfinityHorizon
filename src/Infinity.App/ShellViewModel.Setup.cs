// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// OD-46 and OD-47: the Azimuth screen (docs/launcher/AZIMUTH_SETUP.md). The launcher finds
/// Azimuth by itself; when it is missing or another version, this screen says what to do, points
/// to the official page and lets the player choose the folder (remembered). It installs the
/// Infinity hook from the package next to the launcher, only when its signature verifies, and
/// shows whether the installed hook is the signed official one. Opens on its own at start when
/// Azimuth is not ready.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _onSetup;
    private bool _searching;
    private InstalledHook? _hook;
    private HookPackage? _hookPackage;
    private string? _hookPackageError;
    private bool _configurationHasError, _messageForConfiguration, _configurationRuntimeIssue;
    private bool _configurationErrorFromIntegrity;
    private bool _azimuthChoiceFailed;

    /// <summary>
    /// The development hook key (<c>FI_DEV_HOOK_KEY</c>) counts only against a development server
    /// (plain http on loopback or the LAN, as the launch ticket's dev key): a hook signed with it
    /// never plays on a production server.
    /// </summary>
    private byte[]? DevHookKey => DevServer ? HookInstaller.DevKey(Environment.GetEnvironmentVariable(HookInstaller.DevKeyEnv)) : null;

    // A custom server (CR-L39) is never a development server: its own pinned keys, the release hook rules.
    private bool DevServer => !_config.Production && !_config.Custom && Infinity.Core.DevNet.PlainHttpUrl(_config.Server.GetLeftPart(UriPartial.Authority));

    public bool ShowSetup => InShell && _onSetup && !ShowPickSave;

    public AzimuthStatus SetupStatus => AzimuthSetup.StatusOf(_install, _hook);

    // OD-113: a pending or expired integrity check never sends the player to Configure; only a Hardcore profile's
    // missing game folder or a real mismatch does.
    private bool GameConfigurationReady => !_configurationHasError && AzimuthCardReady && IntegrityConfigurationReady;

    private bool HookUpdateNeeded => _hook is not null && _hookPackage is not null
        && HookInstaller.Build(_hookPackage.Version) > HookInstaller.Build(_hook.Version);

    /// <summary>Pending or failed configuration is explained only in Configure.</summary>
    public bool SetupNeeded
    {
        get { var needed = !GameConfigurationReady || _configurationRuntimeIssue; InspectConfiguration(ref needed); return needed; }
    }
    partial void InspectConfiguration(ref bool needsAttention);

    public string? DisplayMessage => _messageForConfiguration && !ShowSetup ? null : Message;

    private void ConfigurationMessage(string message, bool error = false, bool integrity = false)
    {
        Message = message;
        _messageForConfiguration = true;
        _configurationHasError = error;
        _configurationErrorFromIntegrity = integrity;
        Changed(string.Empty);
    }

    private void ClearRuntimeConfigurationIssue()
    {
        if (!_configurationRuntimeIssue) return;
        _configurationRuntimeIssue = false;
        if (!_configurationHasError && _messageForConfiguration) Message = null;
    }

    /// <summary>Set by the supervisor's HookOffline: the game's own quit closes the hook pipe first, so this is not a setup problem.</summary>
    private bool _hookOfflineNotice;

    /// <summary>A session that ended without an error takes back its hook-offline notice (and the Configure flag it raised).</summary>
    private void ClearHookOfflineNotice()
    {
        if (!_hookOfflineNotice) return;
        _hookOfflineNotice = false;
        // another setup message raised since then owns the flag; a play message (a mod failure) replaced it without being one
        if (_messageForConfiguration && Message != S["supervisor.hook_offline"]) return;
        _configurationHasError = false; _configurationErrorFromIntegrity = false;
        if (_messageForConfiguration) Message = null; // the setter also clears _messageForConfiguration
        Changed(string.Empty);
    }

    public bool Searching => _searching;

    public bool AzimuthFound => _install is not null;

    public bool AzimuthVersionOk => _install?.Version == AzimuthSetup.CompatibleVersion && _install.HostSha256 == AzimuthSetup.CompatibleHostSha256;

    public bool HookOk => _hook?.Verified == true;

    public string SetupFoundLine => _searching ? S["setup.searching"]
        : _install is null ? S["setup.not_found"] : S.Format("setup.found", ("path", _install.Root));

    public string SetupVersionLine => _install is null
        ? S.Format("setup.version_needed", ("version", AzimuthSetup.CompatibleVersion))
        : AzimuthVersionOk
            ? S.Format("setup.version_ok", ("version", AzimuthSetup.CompatibleVersion))
            : S.Format("setup.version_wrong", ("found", _install.Version ?? "?"), ("version", AzimuthSetup.CompatibleVersion));

    public string SetupHookLine => _install is null ? S["setup.hook_after"] : _hook switch
    {
        { Verified: true, Signer: HookSigner.Official } h => S.Format("setup.hook_official", ("version", h.Version!)),
        { Verified: true } h => S.Format("setup.hook_dev", ("version", h.Version!)),
        { Kind: HookKind.Ours } h => S.Format("setup.hook_unsigned", ("version", h.Version ?? "?")),
        { Kind: HookKind.Unknown } => S["setup.hook_unknown"],
        _ => S["setup.hook_missing"],
    };

    /// <summary>Install, update or replace with the signed package (Azimuth must be the compatible version).</summary>
    public bool CanInstallHook => (InShell || DevelopmentMods) && AzimuthVersionOk && ModsIdle && _hook is not null && HookInstaller.CanInstallManually(_hook, _hookPackage);

    public string HookInstallLabel => _hookPackage is null ? string.Empty
        : S.Format(_hook?.Kind != HookKind.Ours ? "setup.hook_install"
            : HookInstaller.Build(_hookPackage.Version) > HookInstaller.Build(_hook.Version) ? "setup.hook_update" : "setup.hook_reinstall",
            ("version", _hookPackage.Version));

    /// <summary>Why the launcher cannot install a hook (no package, or one whose signature failed).</summary>
    public string? HookPackageProblem => _hookPackageError is not null ? S["setup.package_bad"]
        : _hookPackage is null && _install is not null && !HookOk ? S["setup.package_missing"] : null;

    public bool HasHookPackageProblem => HookPackageProblem is not null;

    public bool AzimuthCardReady => !_searching && !_azimuthChoiceFailed && SetupStatus == AzimuthStatus.Ready
        && !HasHookPackageProblem && !HookUpdateNeeded;
    public string AzimuthCardStatus => S[_searching ? "configure.checking" : AzimuthCardReady ? "configure.ready" : "configure.attention"];

    public string SetupSummary => SetupStatus switch
    {
        AzimuthStatus.Missing => S["setup.summary_missing"],
        AzimuthStatus.WrongVersion => S.Format("setup.summary_wrong", ("version", AzimuthSetup.CompatibleVersion)),
        AzimuthStatus.NoHook => S["setup.summary_hook"],
        _ => S["setup.summary_ready"],
    };

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public string SetupStep1 => S.Format("setup.step1", ("version", AzimuthSetup.CompatibleVersion));

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public string SetupStep3 => S.Format("setup.step3", ("game", AzimuthSetup.GameVersion));

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public string AzimuthPage => AzimuthSetup.OfficialPage;

    public void OpenSetup()
    {
        if (!InShell) return;
        LeaveAccount();
        _onSetup = true;
        _onMods = false;
        _onBugReport = false; _onNews = false;
        RefreshHook();
        Changed(string.Empty);
    }

    /// <summary>Finds Azimuth off the UI thread (the look-around reads folders), then shows the setup when it is not ready.</summary>
    public async Task FindAzimuthAsync(string? explicitFolder, bool openSetupWhenNeeded)
    {
        _configurationHasError = false;
        _configurationRuntimeIssue = false;
        _azimuthChoiceFailed = false;
        if (_messageForConfiguration) Message = null;
        _searching = true;
        Changed(string.Empty);
        AzimuthInstall? found;
        var notes = new List<string>();
        try
        {
            // owner 2026-10-27: the location is the PC's (the launcher root), found in any server's folder
            var root = StateRoot;
            found = await Task.Run(() => AzimuthSetup.Find(explicitFolder, _stateDirectory, AppContext.BaseDirectory, AzimuthSearchRoots, root, notes.Add));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("setup: " + e.Message);
            found = null;
        }
        finally
        {
            _searching = false;
        }
        foreach (var note in notes) Log(note);
        Install = found;
        Log(found is null ? "setup: no Azimuth found" : $"setup: Azimuth {found.Version ?? "?"} at {found.Root} ({SetupStatus})");
        if (InShell && openSetupWhenNeeded && SetupStatus != AzimuthStatus.Ready)
        {
            OpenSetup();
        }
        Changed(string.Empty);
    }

    /// <summary>The folder the player picked: kept only when it holds azimuth.exe.</summary>
    public void ChooseAzimuthFolder(string folder)
    {
        if ((!InShell && !DevelopmentMods) || !ModsIdle) return;
        if (!AzimuthSetup.SaveChoice(_stateDirectory, folder, StateRoot))
        {
            _azimuthChoiceFailed = true;
            ConfigurationMessage(S["setup.not_azimuth_folder"], error: true);
            return;
        }
        Install = AzimuthInstall.Detect(folder);
        _azimuthChoiceFailed = false;
        ConfigurationMessage(SetupSummary);
        Changed(string.Empty);
    }

    public Task RecheckAzimuthAsync() => InShell && ModsIdle
        ? FindAzimuthAsync(_install?.Root, openSetupWhenNeeded: false) : Task.CompletedTask;

    /// <summary>Installs the signed hook package into the player's Azimuth (the game closed).</summary>
    public void InstallHook()
    {
        RefreshHook(); // the package and installed build may have changed since this pane opened
        Changed(string.Empty);
        if (!CanInstallHook || _install is null || _hookPackage is null)
        {
            Log($"hook: install not offered: package={(_hookPackage is null ? "none" : "ok")} azimuth={_install?.Version ?? "none"} " +
                $"versionOk={AzimuthVersionOk} busy={_busy} playing={Playing} " +
                $"shouldInstall={(_hook is null ? "no-installed-state" : HookInstaller.ShouldInstall(_hook, _hookPackage).ToString())}");
            return;
        }
        if (GameRunning())
        {
            Log("hook: install refused: azimuth.exe is running");
            Message = S["mods.close_game"];
            return;
        }
        Log($"hook: installing {_hookPackage.Version} ({_hookPackage.Signer}, sha256 {_hookPackage.Sha256[..16]}…) from {_hookPackage.Path} into {_install.Root}");
        try
        {
            var installed = HookInstaller.Install(_install.Root, _hookPackage, DevHookKey, GameRunning);
            Log($"hook: {installed.Version} installed ({installed.Signer}) in {_install.Root}");
            Install = AzimuthInstall.Detect(_install.Root);
            ConfigurationMessage(S.Format("setup.hook_installed", ("version", installed.Version!)));
        }
        catch (Exception e) when (e is HookException or IOException or UnauthorizedAccessException)
        {
            Log("hook: install failed: " + e.Message);
            ConfigurationMessage(S["setup.hook_install_failed"] + " (" + e.Message + ")", error: true);
        }
        RefreshHook();
        if (DevelopmentMods && _onMods) _messageForConfiguration = false;
        Changed(string.Empty);
    }

    /// <summary>The package next to the launcher (<c>hook\</c>, or <c>FI_HOOK_PACKAGE</c>) and the installed hook.</summary>
    internal void RefreshHook()
    {
        _hookPackageError = null;
        try
        {
            var custom = DevServer ? Environment.GetEnvironmentVariable("FI_HOOK_PACKAGE") : null;
            var folder = custom ?? Path.Combine(AppContext.BaseDirectory, "hook");
            _hookPackage = HookInstaller.LoadPackage(folder, DevHookKey);
        }
        catch (Exception e) when (e is HookException or IOException or UnauthorizedAccessException)
        {
            _hookPackage = null;
            _hookPackageError = e.Message;
            Log("hook: package refused: " + e.Message);
        }
        try
        {
            _hook = _install is null ? null : HookInstaller.Inspect(_install.Root, DevHookKey);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            // unreadable is not verified: never Ready on a failed check
            _hook = _install is null ? null : new InstalledHook(HookKind.Unknown, null, null, HookSigner.None);
            Log("hook: " + e.Message);
        }
        Changed(nameof(ShowBuildOriginWarning));
        // every check, once per change: what is next to the launcher, what is installed, and the decision
        var line = $"hook: check: devServer={DevServer} devKey={(DevHookKey is null ? "none" : "set")} " +
            $"package={(_hookPackage is { } p ? $"{p.Version} {p.Signer} {p.Sha256[..16]}… at {p.Path}" : _hookPackageError ?? "none at " + Path.Combine(AppContext.BaseDirectory, "hook"))} " +
            $"installed={(_hook is { } h ? $"{h.Kind} {h.Version ?? "?"} verified={h.Verified} ({h.Signer})" : "no Azimuth")} " +
            $"offer={(_hook is null ? "no" : HookInstaller.ShouldInstall(_hook, _hookPackage).ToString())}";
        if (line != _lastHookLine)
        {
            _lastHookLine = line;
            Log(line);
        }
    }

    private string? _lastHookLine;
}
