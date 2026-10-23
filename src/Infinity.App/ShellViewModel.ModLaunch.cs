// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Process;
using Infinity.Core.Session;
using Infinity.Core.Windows;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    /// <summary>The signed native mod runtime next to the launcher (OD-113), located off the UI thread by <see cref="RefreshModsAsync"/>; null: none or unverified.</summary>
    private ModRuntimePackage? _nativeRuntime;

    /// <summary>The key the runtime package must verify with: the development key against a development server, else the release key (OD-49).</summary>
    private byte[] NativeModKey => DevHookKey ?? HookInstaller.ReleaseKey.ToArray();

    private static string LauncherDirectory => AppContext.BaseDirectory;

    /// <summary>Why the last launch could not prepare its mods (with the reason), shown on Play until the next attempt.</summary>
    private string? _modLaunchFailure;

    /// <summary>The hook's exit when selected mods failed after they began loading (mod_host.c, N-M4): never a crash report.</summary>
    internal const uint ModRuntimeExitCode = 0x46490004;

    /// <summary>The current session ended through <see cref="ModRuntimeExitCode"/>; its crash capture is not presented.</summary>
    private bool _modRuntimeEnded;

    /// <summary>This launch's plan file (FI_MOD_PLAN); the runtime writes runtime.log beside it.</summary>
    private string? _nativePlanPath;

    /// <summary>
    /// The runtime's own log for this launch, copied into launcher.log (its last 40 lines), and its last failure or refusal
    /// ("failed: …" / "refused: …"), or null.
    /// </summary>
    private string? RuntimeReason()
    {
        if (_nativePlanPath is not { } plan) return null;
        try
        {
            var path = Path.Combine(Path.GetDirectoryName(plan)!, "runtime.log");
            if (!File.Exists(path) || new FileInfo(path).Length > 1024 * 1024) return null;
            var lines = File.ReadAllLines(path).Select(l => new string(l.Select(c => c is >= ' ' and <= '~' ? c : '?').ToArray())).ToArray();
            foreach (var line in lines.TakeLast(40)) Log("mod runtime: " + line);
            var reason = lines.LastOrDefault(l => l.Contains("] failed: ", StringComparison.Ordinal) || l.Contains("] refused: ", StringComparison.Ordinal));
            return reason is null ? null : reason[(reason.IndexOf("] ", StringComparison.Ordinal) + 2)..];
        }
        catch (IOException) { return null; }
        catch (UnauthorizedAccessException) { return null; }
    }

    /// <summary>A failed mod preparation: logged with its message, shown on Play (never only the status line) and returned for <see cref="Message"/>.</summary>
    private string ModLaunchFailed(string where, Exception error)
    {
        Log("mods: " + where + " failed: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "") + ": " + error.Message);
        var text = error is ModException code && code.Code != ModErrors.Failed ? ModErrorText(code) : S["mods.prepare_failed"] + " (" + error.Message + ")";
        _modLaunchFailure = text; Changed(nameof(ModPlayNotice)); Changed(nameof(ShowModPlayNotice));
        return text;
    }

    /// <summary>The first hook build with the production mod host (FIMOD003, the N-H2 report and the runtime signature check).</summary>
    internal const uint NativeModOnlineHookBuild = 82;

    /// <summary>Native mods play online only through a verified hook that hosts the runtime (<see cref="NativeModOnlineHookBuild"/>+).</summary>
    private bool NativeModHookReady => _hook is { Verified: true } && HookInstaller.Build(_hook.Version) >= NativeModOnlineHookBuild;

    /// <summary>
    /// FIMOD004 (data-mod SQL): hook 0.3.0+fh2online.83 (WINDOWS, <c>feature/mods-data-sql</c>) is the first that loads data-only plans,
    /// applies their SQL files and gates mods on the dependency being active; the install/enable gate and the FIMOD004 online minimum.
    /// </summary>
    internal const uint DataModSqlHookBuild = 83;

    /// <summary>Data mods with SQL need the verified runtime package and a verified hook of <see cref="DataModSqlHookBuild"/>+.</summary>
    private bool DataModSqlReady => _nativeRuntime is not null && _hook is { Verified: true } && HookInstaller.Build(_hook.Version) >= DataModSqlHookBuild;

    /// <summary>
    /// OD-113: why an online launch with enabled native mods cannot start (en/pt-BR), or null with the verified runtime package.
    /// Checked off the UI thread right before the launch, so a package changed since the Mods page looked is seen.
    /// </summary>
    private async Task<(ModRuntimePackage? Runtime, string? Refusal)> NativeModsForOnlineAsync(bool dataSql = false)
    {
        var key = NativeModKey;
        var runtime = await Task.Run(() => ModRuntimePackage.Locate(LauncherDirectory, key));
        _nativeRuntime = runtime;
        if (runtime is null) return (null, S["mods.native_unavailable_play"]);
        RefreshHook();
        if (dataSql && !DataModSqlReady) return (null, S["mods.data_sql_runtime"]);
        return NativeModHookReady ? (runtime, null) : (null, S["mods.native_hook_required"]);
    }

    /// <summary>
    /// OD-113: the online launch's native plan (FIMOD003 with this launch's nonce), built off the UI thread after every other check
    /// passed: locks the runtime, helper and selected files and publishes the session database (when a mod declares one) until
    /// <see cref="EndNativeModSessionAsync"/>. Null when it could not be prepared (the reason is shown); nothing is left published then.
    /// </summary>
    private async Task<NativeModPlan?> PrepareOnlineNativePlanAsync(ModRuntimePackage runtime)
    {
        if (ModManagerFactory() is not { } factory) return null;
        if (GameRunning()) { Message = S["mods.close_game"]; return null; }
        var key = NativeModKey;
        _modOperation = true; _modProgress = S["mods.session_prepare"]; Busy = true; Changed(string.Empty);
        try
        {
            var plan = await Task.Run(() => factory().PrepareNativeLaunch(runtime.Runtime, runtime.Helper, key));
            _nativePlanPath = plan.Environment.FirstOrDefault(v => v.Name == "FI_MOD_PLAN").Value;
            return plan;
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is System.ComponentModel.Win32Exception)
        {
            Message = ModLaunchFailed("online native plan", error);
            return null;
        }
        finally { _modOperation = false; Busy = false; Changed(string.Empty); }
    }

    /// <summary>After the game exited (as offline): the session database's baseline returns and the plan's locks are released.</summary>
    private async Task EndNativeModSessionAsync(NativeModPlan plan)
    {
        try { await Task.Run(plan.Dispose); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        {
            Log("mods: session database restore refused: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : ""));
            Message = S["mods.recovery_needed"];
        }
    }

    /// <summary>DEV: an offline session with the enabled native mods, when every one of them lists this Azimuth version (no version is built in).</summary>
    public bool CanPlayModdedOffline => DevelopmentMods && ModsIdle && _install?.Version is { } version && !ModPlayBlocked && !RequiredModBlocked && _modHealth.HasEnabledNative
        && _modHealth.Installed.Where(i => i.Enabled && i.Manifest is not null).All(i => i.Manifest!.Document.GetProperty("compatibility")
            .GetProperty("azimuthVersions").EnumerateArray().Any(v => v.GetString() == version));

    public async Task PlayModdedOfflineAsync()
    {
        if (!CanPlayModdedOffline || _install is null || !OperatingSystem.IsWindows() || ModManagerFactory() is not { } factory) return;
        _modLaunchFailure = null;
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
        { Message = ModLaunchFailed("offline mod session", error); }
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
