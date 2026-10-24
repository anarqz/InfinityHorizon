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
        && GameConfigurationReady && !ModPlayBlocked && OperatingSystem.IsWindows();

    /// <summary>The local profile an offline Play plays: the chosen local one, or the cached copy of the chosen account profile.</summary>
    private LocalProfile? OfflinePlayProfile => _chosenLocal ?? LocalProfiles.Read().FirstOrDefault(p => p.Game == _gate.Game && p.DisplayName == _gate.ProfileTag);

    public bool CanPlayAny => PlaysOnline ? CanPlay : CanPlayOffline;

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

    public async Task PlayOfflineAsync()
    {
        if (!CanPlayOffline || _install is not { } install || OfflinePlayProfile is not { } profile || ModManagerFactory() is not { } factory) return;
        _modLaunchFailure = null;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        RefreshHook();
        if (_hook is not { Verified: true }) { Message = S["play.setup_needed"]; return; }
        NativeModPlan? plan = null;
        _modOperation = true; _modProgress = S["mods.session_prepare"]; Busy = true;
        _play = new CancellationTokenSource(); Changed(string.Empty);
        try
        {
            var key = NativeModKey;
            ModRuntimePackage? runtime = null;
            IReadOnlyList<string> overrides = [];
            var hasNative = false;
            await Task.Run(() =>
            {
                var prepared = factory().PrepareForLaunch(); // locks enforced, Wine overrides reconciled (paired with their removal)
                overrides = prepared.DllOverrides; hasNative = prepared.HasEnabledNative;
            });
            if (hasNative)
            {
                runtime = await Task.Run(() => ModRuntimePackage.Locate(LauncherDirectory, key));
                if (runtime is null || !NativeModHookReady) throw new ModException("native mods need the verified runtime and hook", ModErrors.Unsupported);
                plan = await Task.Run(() => factory().PrepareNativeLaunch(runtime.Runtime, runtime.Helper, key));
            }
            var environment = (plan?.Environment ?? []).ToList();
            environment.Add((Launch.EnvLaunch, Launch.EnvOffline));
            environment.Add((Launch.EnvLauncherExe, Environment.ProcessPath ?? "infinity.exe"));
            environment.Add((EnvOfflineXuid, profile.OfflinePlayXuid.ToString(CultureInfo.InvariantCulture)));
            environment.Add((EnvOfflineGamertag, profile.Name));
            if (WineHost.Detected)
            {
                var inherited = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
                foreach (var module in overrides) inherited = WineHost.WithNativeDll(inherited, module) ?? inherited;
                if (inherited is not null) environment.Add(("WINEDLLOVERRIDES", inherited));
            }
            if (GameRunning()) throw new ModException("another game started during preparation", ModErrors.GameRunning);
            // OD-118: the overlay comes too, named in the environment (no session pipe offline) and served on its own -ui pipe.
            using var uiPipe = OfflineOverlay(environment);
            Log("play: offline as local profile " + profile.Id + " (" + AzimuthSaves.FolderFor(profile.OfflinePlayXuid) + ")");
            using var child = GameProcess.Spawn(new LaunchPlan(install.HostExe, [], install.Root, environment, []));
            using var overlay = uiPipe is null ? null : new OverlayChannel(uiPipe, child.Pid,
                new UiFrame.Status(profile.Name, UiTier.Free, UiState.Offline, S["offline.playing_offline_status"]));
            if (overlay is not null)
            {
                // The first notice the overlay shows (queued until its hello): Playing offline.
                await overlay.ToastAsync(new UiFrame.Toast(UiLevel.Info, 0, 80, S["offline.playing_offline_status"]), CancellationToken.None);
                _ = overlay.RunAsync(child.Pid, child.Exited, TimeSpan.FromMinutes(5), _play.Token);
            }
            _modOperation = false; Busy = false; SetStatus(S["offline.playing_offline_status"]);
            var wait = child.WaitForExitAsync(); _playTask = wait;
            var code = await wait;
            SetStatus(code == 0 ? S["status.ended"] : S["mods.runtime_failed"]);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is System.ComponentModel.Win32Exception)
        { Message = ModLaunchFailed("offline session", error); }
        finally
        {
            _modOperation = true; Busy = true; _modProgress = S["mods.session_prepare"]; Changed(string.Empty);
            try { if (plan is not null) await Task.Run(plan.Dispose); }
            catch (Exception error) when (ManagedModFiles.IsStateError(error)) { Message = S["mods.recovery_needed"]; }
            _play?.Dispose(); _play = null; _playTask = null; _modOperation = false; Busy = false;
            await RefreshModsAsync(); Changed(string.Empty);
        }
    }
}
