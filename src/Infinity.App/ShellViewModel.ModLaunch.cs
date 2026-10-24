// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
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

    /// <summary>
    /// OD-116: the runtime installed from the catalog's <c>infinity-mod-runtime</c> entry. The launcher no longer bundles one; a
    /// development build still falls back to its own <c>mod\</c> folder, and production adopts once the signed one a 1.0.9 left there.
    /// </summary>
    private ModRuntimePackage? LocateRuntime(string home, byte[] key) =>
        ModRuntimePackage.LocateInstalled(home, key)
        ?? (DevelopmentMods ? ModRuntimePackage.Locate(LauncherDirectory, key) : ModRuntimePackage.AdoptBundled(LauncherDirectory, home, key));

    /// <summary>
    /// Installs (or updates to) the catalog's runtime when it differs from the installed one: before a native mod is installed,
    /// enabled or updated, and before an online launch with native mods. With no catalog runtime the installed one is used; a
    /// failed update keeps an earlier runtime that still verifies.
    /// </summary>
    private async Task<ModRuntimePackage?> EnsureRuntimeAsync(CancellationToken cancellation)
    {
        var key = NativeModKey; var home = ModHome; var entry = _catalog?.Runtime;
        var installed = await Task.Run(() => LocateRuntime(home, key), cancellation);
        if (!ModRuntimePackage.NeedsInstall(installed, entry)) { _nativeRuntime = installed; return installed; }
        // The runtime is shared by every catalog: an unofficial one may provide it when none is installed, never replace (or roll back) one.
        if (UsingCustomCatalog && installed is not null) { Log("mods: unofficial catalog's runtime not applied over the installed " + installed.Version); _nativeRuntime = installed; return installed; }
        _modProgress = S["mods.progress.runtime"]; Changed(nameof(ModProgress));
        try { installed = await ModRuntimePackage.InstallAsync(ModDownloads, home, entry!, key, cancellation); Log("mods: runtime " + entry!.Version + " installed"); }
        catch (Exception error) when (installed is not null && (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException))
        { Log("mods: runtime update failed, keeping " + installed.Version + ": " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "")); }
        _nativeRuntime = installed;
        return installed;
    }

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

    /// <summary>
    /// The minimum hook build for native mods online: .82 introduced the production mod host (FIMOD003, the N-H2 report and the
    /// runtime signature check); .84/.85 bound its link checks at the trusted roots (Proton) and end a failed session without a hang.
    /// </summary>
    internal const uint NativeModOnlineHookBuild = 85;

    /// <summary>Native mods play online only through a verified hook that hosts the runtime (<see cref="NativeModOnlineHookBuild"/>+).</summary>
    private bool NativeModHookReady => _hook is { Verified: true } && HookInstaller.Build(_hook.Version) >= NativeModOnlineHookBuild;

    /// <summary>
    /// FIMOD004 (data-mod SQL): hook 0.3.0+fh2online.83 (WINDOWS, <c>feature/mods-data-sql</c>) is the first that loads data-only plans,
    /// applies their SQL files and gates mods on the dependency being active; the install/enable gate and the FIMOD004 online minimum.
    /// Raised to .85 with <see cref="NativeModOnlineHookBuild"/> for the Proton link checks and the failed-session exit.
    /// </summary>
    internal const uint DataModSqlHookBuild = 85;

    /// <summary>Data mods with SQL need the verified runtime package and a verified hook of <see cref="DataModSqlHookBuild"/>+.</summary>
    private bool DataModSqlReady => _nativeRuntime is not null && _hook is { Verified: true } && HookInstaller.Build(_hook.Version) >= DataModSqlHookBuild;

    /// <summary>
    /// OD-113: why a launch with enabled native mods (online, or Play offline since OD-118) cannot start (en/pt-BR), or null with the
    /// verified runtime package.
    /// Checked off the UI thread right before the launch, so a package changed since the Mods page looked is seen.
    /// </summary>
    private async Task<(ModRuntimePackage? Runtime, string? Refusal)> NativeModsForOnlineAsync(bool dataSql = false)
    {
        ModRuntimePackage? runtime;
        // The update from 1.0.9 removes the bundled runtime: the catalog's comes first, so a Play right after startup waits for it.
        if (_catalog is null && !DevelopmentMods && LocateRuntime(ModHome, NativeModKey) is null)
        {
            if (!_catalogLoading) await LoadModCatalogQuietlyAsync();
            else for (var i = 0; i < 300 && _catalogLoading; i++) await Task.Delay(100);
            if (_catalog?.Runtime is null) return (null, S["mods.runtime_catalog_needed"]);
        }
        using (var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3)))
            try { runtime = await EnsureRuntimeAsync(timeout.Token); }
            catch (Exception error) when (ManagedModFiles.IsStateError(error) || error is HttpRequestException or OperationCanceledException)
            { Log("mods: runtime unavailable: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "")); runtime = null; }
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
}
