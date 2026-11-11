// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Overlay;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Session;
using Infinity.Core.Shell;
using Infinity.Core.Supervisor;
using Infinity.Core.Windows;

namespace Infinity.App;

/// <summary>
/// OD-118: Play without an account (or with a profile that is not on the signed-in account). The game starts with
/// <c>FI_LAUNCH=v1;offline</c> through the verified hook, with the enabled mods prepared as for an online launch (locks, Wine
/// overrides, the native plan when native mods are on) and the chosen profile's offline identity (P1, CR-L36) for its save folder.
/// No session, ticket, integrity check or cloud save: offline progress stays on this PC.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The environment variables the hook reads on an offline launch to play this profile's identity (P1a).</summary>
    public const string EnvOfflineXuid = "FI_OFFLINE_XUID", EnvOfflineGamertag = "FI_OFFLINE_GAMERTAG";

    /// <summary>This Play goes online: signed in, with an account profile chosen (P1e: a local-only profile plays offline).</summary>
    public bool PlaysOnline => SignedIn && _session is not null && (_chosenLocal is null || _chosenLocal.OnAccount(_session.Me?.AccountId));

    /// <summary>PLAY ONLINE when this Play goes online, else PLAY OFFLINE (OD-118).</summary>
    public string PlayButtonLabel => S[PlaysOnline ? "offline.play_online_button" : "offline.play_offline_button"];

    /// <summary>The offline Play's own requirements: no account, session or online service involved.</summary>
    public bool CanPlayOffline => Ready && !PlaysOnline && _install is not null && OfflinePlayProfile is not null && !Playing && !_busy && CrashIdle
        && GameConfigurationReady && !ModPlayBlocked && !JobsBlockPlay && OperatingSystem.IsWindows();

    /// <summary>The local profile an offline Play plays: the chosen local one, or the cached copy of the chosen account profile.</summary>
    private LocalProfile? OfflinePlayProfile => _chosenLocal ?? LocalProfiles.Read().FirstOrDefault(p => p.Game == _gate.Game && p.Cloud?.ProfileId == _gate.ProfileKey);

    public bool CanPlayAny => PlaysOnline ? CanPlay : CanPlayOffline;

    private GameProcess? _offlineChild;

    /// <summary>How long Quit waits for the game to close its windows before ending it (the supervisor's 20 s; tests shorten it).</summary>
    internal TimeSpan OfflineQuitGrace { get; set; } = TimeSpan.FromSeconds(20);
    private bool _offlineQuitting;

    /// <summary>
    /// OD-118: Quit game during an offline Play (the launcher's button or the overlay's confirmed Quit). There is no supervisor or
    /// session pipe offline, so the launcher closes the game as the supervisor does: WM_CLOSE to its windows, then a forced end
    /// after 20 s.
    /// </summary>
    private void QuitOfflineGame()
    {
        if (_offlineChild is not { } child || _offlineQuitting) return;
        _offlineQuitting = true;
        SetStatus(S["supervisor.quit_asked"]);
        Log("play: offline quit asked");
        _ = QuitOfflineChildAsync(child);
    }

    private async Task QuitOfflineChildAsync(GameProcess child)
    {
        try { if (await child.QuitAsync(OfflineQuitGrace)) Log("play: offline game did not close in time; ended"); }
        catch (Exception error) when (error is ObjectDisposedException or System.ComponentModel.Win32Exception or InvalidOperationException)
        { Log("play: offline quit: " + error.GetType().Name); }
    }

    /// <summary>The overlay's events offline: its confirmed Quit ends the game; the rest has no online session to go to.</summary>
    internal async Task ReadOfflineOverlayAsync(System.Threading.Channels.ChannelReader<UiIncoming> incoming, CancellationToken cancellation)
    {
        try
        {
            await foreach (var item in incoming.ReadAllAsync(cancellation))
                if (item is UiIncoming.Event { Kind: UiEventKind.Quit }) QuitOfflineGame();
        }
        catch (OperationCanceledException) { }
    }

    private static readonly string[] OfflineOverlayNames = ["FI_OFFLINE_OVERLAY", "FI_OFFLINE_OVERLAY_SHA256", "FI_OFFLINE_UI"];

    /// <summary>The chosen local profile as stored now (an upload or account refresh since it was chosen links it to the account).</summary>
    private void RefreshChosenLocal()
    {
        if (_chosenLocal is { } chosen) _chosenLocal = LocalProfiles.Read().FirstOrDefault(p => p.Id == chosen.Id) ?? chosen;
    }

    /// <summary>
    /// The signed overlay for an offline Play (OD-118): its path and SHA-256 and a fresh -ui pipe nonce go into the game's environment
    /// (FI_OFFLINE_OVERLAY, FI_OFFLINE_OVERLAY_SHA256, FI_OFFLINE_UI; the hook checks the hash like the Overlay frame), and the pipe is
    /// created before the game starts. Null, and nothing added, when there is no signed overlay or the pipe cannot be made.
    /// </summary>
    private WindowsPipe? OfflineOverlay(List<(string Name, string Value)> environment)
    {
        if (LoadOverlayPackage() is not { } package) return null;
        try
        {
            var link = new Handoff(HandoffChannel.Pipe, System.Security.Cryptography.RandomNumberGenerator.GetBytes(Launch.NonceLength));
            var pipe = new WindowsPipe(PipeServer.Create(link.UiPipeName));
            environment.Add(("FI_OFFLINE_OVERLAY", package.Path));
            environment.Add(("FI_OFFLINE_OVERLAY_SHA256", Convert.ToHexStringLower(package.Sha256)));
            environment.Add(("FI_OFFLINE_UI", link.NonceHex));
            return pipe;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception)
        {
            Log("overlay: offline UI pipe unavailable (" + error.GetType().Name + "); playing without the overlay");
            return null;
        }
    }

    /// <summary>
    /// The game's environment for an offline Play (OD-118): <c>FI_LAUNCH=v1;offline</c> (the hook then never reaches a network), the
    /// launcher's path, the profile's offline identity and, under Wine, the mods' native DLL overrides. No server, ticket or token is
    /// ever part of it.
    /// </summary>
    internal static List<(string Name, string Value)> OfflineLaunchEnvironment(LocalProfile profile, IReadOnlyList<string> overrides)
    {
        var environment = new List<(string Name, string Value)> { (Launch.EnvLaunch, Launch.EnvOffline) };
        environment.Add((Launch.EnvLauncherExe, Environment.ProcessPath ?? "infinity.exe"));
        environment.Add((EnvOfflineXuid, profile.IdentityXuid.ToString(CultureInfo.InvariantCulture))); // the hook's identity; the folder is SaveXuid
        environment.Add((EnvOfflineGamertag, profile.Name));
        environment.Add((Launch.EnvOfflineMode, profile.Mode)); // OD-239: an Azimuth profile plays Azimuth's own save offline too
        if (WineHost.Detected)
        {
            var inherited = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
            // OD-125: the hook (ExtraOptions.Infinity's winmm.dll) loads native-first; the AppDefaults entry is set in PlayOfflineAsync
            inherited = WineHost.WithNativeDll(inherited, WineHost.HookModule) ?? inherited;
            foreach (var module in overrides) inherited = WineHost.WithNativeDll(inherited, module) ?? inherited;
            if (inherited is not null) environment.Add(("WINEDLLOVERRIDES", inherited));
        }
        return environment;
    }

    public async Task PlayOfflineAsync()
    {
        if (!CanPlayOffline) return;
        // owner 2026-11-10 (3.1.1): "Starting the game…" over the screen from the click until the game has started; every stop closes it
        // hotfix 3.1.2: the attempt can be cancelled from its overlay (after PlayCancelAfter), up to the game's spawn
        await RunPlayAttemptAsync(S["play.starting_offline"], PlayOfflineCoreAsync);
    }

    private async Task PlayOfflineCoreAsync(CancellationToken cancelled)
    {
        // T30: a signed hook whose bytes are not the package's is replaced first (game closed), or Play is refused
        await RefreshHookAsync();
        if (cancelled.IsCancellationRequested) return;
        if (!EnsureHookCurrentForPlay()) return;
        if (!CanPlayOffline || _install is not { } install || OfflinePlayProfile is not { } profile || ModManagerFactory() is not { } factory) { PlayStopped(); return; }
        _modLaunchFailure = null;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        await RefreshHookAsync();
        if (cancelled.IsCancellationRequested) return;
        if (_hook is not { Verified: true }) { Message = S["play.setup_needed"]; return; }
        _modOperation = true; _modProgress = S["mods.session_prepare"]; Busy = true;
        _play = new CancellationTokenSource(); Changed(string.Empty);
        try
        {
            // OD-233: the per-game mods folders before anything reads the mods, off the UI thread (a stop says why and blocks Play)
            if (!await PrepareModsLayoutAsync(receiptsOnly: false, background: false)) { Message = PrepareAzimuthFailed; return; }
            // the mod state already read for this game (read now, without a toast, only when none was)
            await EnsureModStateForLaunchAsync();
            IReadOnlyList<string> overrides = [];
            var prepared = ModLaunchPreparation.None;
            var fullFeatures = CurrentGame.FullFeatures;
            var launchGame = CurrentGame.Id;
            var held = ManagedHardcoreHeldFile(launchGame);
            var hold = ModHoldResult.None;
            await Task.Run(() =>
            {
                if (!fullFeatures) return; // OD-206: this game has no mods
                var manager = factory();
                hold = manager.HoldForHardcore(launchGame, hardcore: false, held); // what a Hardcore launch turned off comes back
                prepared = manager.PrepareForLaunch(game: launchGame); // this game's mods only: locks enforced, Wine overrides reconciled (paired with their removal)
                overrides = prepared.DllOverrides;
            }, CancellationToken.None);
            // OD-232: an enabled retired mod is refused only online; offline it plays as installed, with the warning
            foreach (var note in hold.Notes) Log(note);
            if (hold.Changed.Count > 0) await RefreshModEntriesAsync(hold.Changed);
            foreach (var warning in prepared.Warnings) Log("mods: " + warning);
            if (!await PrepareAzimuthAsync(install)) { Message = PrepareAzimuthFailed; return; }
            if (cancelled.IsCancellationRequested) return; // cancelled from the overlay: the game is never started
            var environment = OfflineLaunchEnvironment(profile, overrides);
            if (GameRunning()) throw new ModException("another game started during preparation", ModErrors.GameRunning);
            // OD-118: the overlay comes too, named in the environment (no session pipe offline) and served on its own -ui pipe.
            using var uiPipe = OfflineOverlay(environment);
            Log("play: offline as local profile " + profile.Id + " (" + AzimuthSaves.FolderFor(profile.SaveXuid) + ")");
            // Only this launcher names an overlay: one inherited from the player's own environment never reaches the game.
            if (WineHost.Detected) Log("play: wine " + WineHost.HookModule + " " + WineHost.EnsureNativeDllOverride(WineHost.HookModule, true));
            using var child = GameProcess.Spawn(new LaunchPlan(install.HostExe, [AzimuthSetup.SkipLauncher], install.Root, environment, [.. OfflineOverlayNames, .. Launch.OfflineIdentityNames]));
            using var overlay = uiPipe is null ? null : new OverlayChannel(uiPipe, child.Pid,
                new UiFrame.Status(profile.DisplayName, UiTier.Free, UiState.Offline, S["offline.playing_offline_status"]));
            if (overlay is not null)
            {
                // The first notice the overlay shows (queued until its hello): Playing offline.
                await overlay.ToastAsync(new UiFrame.Toast(UiLevel.Info, 0, 80, S["offline.playing_offline_status"] + "\n" + S["overlay.open_hint"]), CancellationToken.None);
                // OD-120: a Hardcore profile offline plays Noob-like on its own local folder; the player is told so in the game too.
                if (profile.Hardcore) await overlay.ToastAsync(new UiFrame.Toast(UiLevel.Info, 0, 160, S["offline.hardcore_offline"]), CancellationToken.None);
                _ = overlay.RunAsync(child.Pid, child.Exited, TimeSpan.FromMinutes(5), _play.Token);
                _ = ReadOfflineOverlayAsync(overlay.Incoming, _play.Token);
            }
            _offlineChild = child; _offlineQuitting = false; _playLaunched = true;
            _modOperation = false; Busy = false; SetStatus(S["offline.playing_offline_status"]);
            HidePlayOverlay(); // the game started
            if (profile.Hardcore) Message = S["offline.hardcore_offline"]; // OD-120: offline progress stays here, online continues from the cloud
            var wait = child.WaitForExitAsync(CancellationToken.None); _playTask = wait;
            var code = await wait;
            _offlineChild = null;
            // OD-118: an offline game ends here: no save sync, upload, integrity or session step follows, only local mod cleanup,
            // and the Infinity gate refuses anything until the launcher is back (even for a signed-in player's local profile).
            _offlineWrapUp = true;
            Log("play: offline game ended (code " + code.ToString(CultureInfo.InvariantCulture) + "); back to the launcher, nothing synced");
            // B1: the hook refuses to play a launcher profile on Azimuth's own save; any other code ended the session unexpectedly
            SetStatus(code == 0 || code == ExitDisposition.QuitForcedExit ? S["status.ended"]
                : code == ExitDisposition.IdentityRefusedExit ? S["supervisor.identity_refused"]
                : code == ExitDisposition.AzimuthSaveRefusedExit ? S["supervisor.azimuth_save_refused"] : S["mods.runtime_failed"]);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is System.ComponentModel.Win32Exception)
        { Message = ModLaunchFailed("offline session", error); }
        finally
        {
            EndPlayProgress(); // a refusal or failure before the game started closes it too (its reason is in Message)
            _modOperation = true; Busy = true; _modProgress = S["mods.session_prepare"]; Changed(string.Empty);
            _offlineChild = null;
            _play?.Dispose(); _play = null; _playTask = null;
            // the game wrote its saves meanwhile: scanned off the UI thread before the cards are rebuilt (the overlay is still up)
            try { await PrimeLocalFactsQuietlyAsync(); }
            finally { _modOperation = false; Busy = false; } // the overlay closes whatever the scan did
            // Owner 2026-11-10: a full mod read only at start, before Play and on Refresh; the cards only redraw for the session's end
            try { RefreshModCards(); }
            finally { _offlineWrapUp = false; }
            Changed(string.Empty);
        }
    }
}
