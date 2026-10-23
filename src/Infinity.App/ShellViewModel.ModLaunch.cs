// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Process;
using Infinity.Core.Session;
using Infinity.Core.Windows;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    public bool CanPlayModdedOffline => DevelopmentMods && ModsIdle && _install?.Version == "0.9.10" && !ModRecoveryRequired;

    /// <summary>Development-only session. Native mod admission online awaits the lobby-policy contract.</summary>
    public async Task PlayModdedOfflineAsync()
    {
        if (!CanPlayModdedOffline || _install is null || !OperatingSystem.IsWindows()) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        RefreshHook();
        if (_hook is not { Verified: true } || HookInstaller.Build(_hook.Version) < 80 || DevHookKey is not { } key)
        { Message = S["mods.runtime_failed"]; return; }
        var install = _install; var manager = ManagedMods();
        NativeModPlan? session = null;
        _modOperation = true; _modProgress = S["mods.session_prepare"]; Busy = true;
        _play = new CancellationTokenSource(); Changed(string.Empty);
        try
        {
            ValidateManagedMods();
            if (!manager.HasEnabledNativeMods) throw new ModException("no native mods enabled");
            var runtimeFolder = Path.Combine(AppContext.BaseDirectory, "mod");
            session = await Task.Run(() => manager.PrepareNativeLaunch(Path.Combine(runtimeFolder, "infinity_mod.dll"),
                Path.Combine(runtimeFolder, "infinity-mod-prepare.exe"), key));
            var environment = session.Environment.ToList();
            environment.Add((Launch.EnvLaunch, Launch.EnvOffline));
            environment.Add((Launch.EnvLauncherExe, Environment.ProcessPath ?? "infinity.exe"));
            if (WineHost.Detected)
            {
                var overrides = Environment.GetEnvironmentVariable("WINEDLLOVERRIDES");
                foreach (var module in manager.NativeDllOverrides())
                {
                    WineHost.EnsureNativeDllOverride(module, true);
                    overrides = WineHost.WithNativeDll(overrides, module) ?? overrides;
                }
                if (overrides is not null) environment.Add(("WINEDLLOVERRIDES", overrides));
            }
            if (GameRunning()) throw new ModException("another game started during preparation");
            using var child = GameProcess.Spawn(new LaunchPlan(install.HostExe, [], install.Root, environment, []));
            _modOperation = false; Busy = false; SetStatus(S["supervisor.starting"]);
            var wait = child.WaitForExitAsync(); _playTask = wait;
            var code = await wait; // Recovery only after the process handle is signalled, never on cancellation.
            SetStatus(code == 0 ? S["status.ended"] : S["mods.runtime_failed"]);
        }
        catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException or InvalidOperationException
            or System.ComponentModel.Win32Exception or ArgumentException or System.Text.Json.JsonException)
        { Log("mod session: " + error.GetType().Name); Message = S["mods.runtime_failed"]; }
        finally
        {
            _modOperation = true; Busy = true; _modProgress = S["mods.session_prepare"]; Changed(string.Empty);
            try { if (session is not null) await Task.Run(session.Dispose); }
            catch (Exception error) when (error is ModException or IOException or UnauthorizedAccessException)
            { Message = S["mods.recovery_needed"]; }
            _play?.Dispose(); _play = null; _playTask = null; _modOperation = false; Busy = false;
            RefreshMods(); Changed(string.Empty);
        }
    }
}
