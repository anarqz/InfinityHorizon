// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Process;
using Infinity.Core.Session;
using Infinity.Core.Windows;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    /// <summary>DEV: an offline session with the enabled native mods, when every one of them lists this Azimuth version (no version is built in).</summary>
    public bool CanPlayModdedOffline => DevelopmentMods && ModsIdle && _install?.Version is { } version && !ModPlayBlocked && _modHealth.HasEnabledNative
        && _modHealth.Installed.Where(i => i.Enabled && i.Manifest is not null).All(i => i.Manifest!.Document.GetProperty("compatibility")
            .GetProperty("azimuthVersions").EnumerateArray().Any(v => v.GetString() == version));

    public async Task PlayModdedOfflineAsync()
    {
        if (!CanPlayModdedOffline || _install is null || !OperatingSystem.IsWindows() || ModManagerFactory() is not { } factory) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        RefreshHook();
        if (_hook is not { Verified: true } || HookInstaller.Build(_hook.Version) < 80 || DevHookKey is not { } key)
        { Message = S["mods.runtime_failed"]; return; }
        var install = _install;
        NativeModPlan? session = null;
        _modOperation = true; _modProgress = S["mods.session_prepare"]; Busy = true;
        _play = new CancellationTokenSource(); Changed(string.Empty);
        try
        {
            var runtimeFolder = Path.Combine(AppContext.BaseDirectory, "mod");
            IReadOnlyList<string> overrides = [];
            session = await Task.Run(() =>
            {
                var manager = factory();
                var prepared = manager.PrepareForLaunch(); // locks enforced, Wine overrides reconciled (paired with their removal)
                if (!prepared.HasEnabledNative) throw new ModException("no native mods enabled", ModErrors.Unsupported);
                overrides = prepared.DllOverrides;
                return manager.PrepareNativeLaunch(Path.Combine(runtimeFolder, "infinity_mod.dll"), Path.Combine(runtimeFolder, "infinity-mod-prepare.exe"), key);
            });
            var environment = session.Environment.ToList();
            environment.Add((Launch.EnvLaunch, Launch.EnvOffline));
            environment.Add((Launch.EnvLauncherExe, Environment.ProcessPath ?? "infinity.exe"));
            if (WineHost.Detected)
            {
                var inherited = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
                foreach (var module in overrides) inherited = WineHost.WithNativeDll(inherited, module) ?? inherited;
                if (inherited is not null) environment.Add(("WINEDLLOVERRIDES", inherited));
            }
            if (GameRunning()) throw new ModException("another game started during preparation", ModErrors.GameRunning);
            using var child = GameProcess.Spawn(new LaunchPlan(install.HostExe, [], install.Root, environment, []));
            _modOperation = false; Busy = false; SetStatus(S["supervisor.starting"]);
            var wait = child.WaitForExitAsync(); _playTask = wait;
            var code = await wait;
            SetStatus(code == 0 ? S["status.ended"] : S["mods.runtime_failed"]);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is System.ComponentModel.Win32Exception)
        { Log("mod session: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "")); Message = error is ModException m ? ModErrorText(m) : S["mods.runtime_failed"]; }
        finally
        {
            _modOperation = true; Busy = true; _modProgress = S["mods.session_prepare"]; Changed(string.Empty);
            try { if (session is not null) await Task.Run(session.Dispose); }
            catch (Exception error) when (ManagedModFiles.IsStateError(error))
            { Message = S["mods.recovery_needed"]; }
            _play?.Dispose(); _play = null; _playTask = null; _modOperation = false; Busy = false;
            await RefreshModsAsync(); Changed(string.Empty);
        }
    }
}
