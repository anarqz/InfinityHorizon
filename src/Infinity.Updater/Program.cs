// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using Infinity.Core.Process;
using Infinity.Core.Update;

namespace Infinity.Updater;

internal static class Program
{
    private static async Task<int> Main(string[] args)
    {
        UpdateApplyPlan? parentExitedPlan = null;
        var replacementMayBeRunning = false;
        try
        {
            var recovery = args.Length == 6 && args[0] == "--recover" && args[2] == "--parent-pid" && args[4] == "--parent-start-ticks";
            if (!recovery && (args.Length != 2 || args[0] != "--transaction")) return 2;
            var plan = UpdateInstaller.ReadPlan(args[1]);
            var worker = Path.Combine(plan.InstallRoot, ".infinity-update", plan.TransactionId, "worker.exe");
            if (!StringComparer.OrdinalIgnoreCase.Equals(Path.GetFullPath(Environment.ProcessPath ?? string.Empty), worker) ||
                !ReleaseSignature.IsOfficialFile(worker)) return 3;
            if (recovery)
            {
                if (!int.TryParse(args[3], NumberStyles.None, CultureInfo.InvariantCulture, out var parent) ||
                    !long.TryParse(args[5], NumberStyles.None, CultureInfo.InvariantCulture, out var started)) return 2;
                plan = plan with { ParentProcessId = parent, ParentStartTimeUtcTicks = started };
            }
            await UpdateInstaller.WaitForParentAsync(plan, TimeSpan.FromSeconds(45)).ConfigureAwait(false);
            parentExitedPlan = plan;
            if (GameActive()) { TryReopenPreflight(plan, replacementMayBeRunning); return 4; }
            if (recovery)
            {
                UpdateInstaller.Recover(plan.InstallRoot);
                UpdateInstaller.RestartRecovered(plan);
            }
            else
            {
                var installer = new UpdateInstaller(new UpdatePackageVerifier(), GameActive);
                try
                {
                    await installer.ApplyAsync(plan, (replacement, cancellation) =>
                    {
                        replacementMayBeRunning = true;
                        return UpdateInstaller.RestartAndConfirmAsync(replacement, cancellation);
                    }).ConfigureAwait(false);
                }
                catch (UpdateInstallException error) when (error.RollbackCompleted)
                {
                    UpdateInstaller.RestartRecovered(plan);
                    return 5;
                }
            }
            return 0;
        }
        catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or InvalidOperationException or
                                      ArgumentException or System.ComponentModel.Win32Exception or OperationCanceledException or
                                      System.Text.Json.JsonException or TimeoutException)
        {
            // Stable exit status only: no credential, server response or arbitrary local path reaches console output.
            if (parentExitedPlan is not null) TryReopenPreflight(parentExitedPlan, replacementMayBeRunning);
            return 6;
        }
    }

    private static void TryReopenPreflight(UpdateApplyPlan plan, bool replacementMayBeRunning)
    {
        try
        {
            if (UpdateInstaller.CanRestartAfterPreflightFailure(plan, replacementMayBeRunning))
                UpdateInstaller.RestartRecovered(plan);
        }
        catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or InvalidOperationException or
                                      ArgumentException or System.ComponentModel.Win32Exception or System.Text.Json.JsonException)
        {
            // Best effort only; pending recovery and a live replacement are never bypassed to reopen UI.
        }
    }

    private static bool GameActive()
    {
        var processes = Process.GetProcessesByName("azimuth")
            .Concat(Process.GetProcessesByName("forza_durango_release_final")).ToArray();
        try { return processes.Length != 0; }
        finally { foreach (var process in processes) process.Dispose(); }
    }
}
