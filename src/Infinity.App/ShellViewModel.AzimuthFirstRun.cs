// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.IO;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// Owner 2026-10-28: an Azimuth never opened (its <c>data</c> has only <c>bin</c>) blocks the launcher with a modal: open Azimuth
/// once and set Forza Horizon 2 up there, then Check again. Launch Azimuth starts its own launcher (no <c>--skip-launcher</c>).
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _azimuthNeverRun;
    private string _azimuthFirstRunStatus = "";

    public bool ShowAzimuthFirstRun => _azimuthNeverRun && _install is not null;
    public string AzimuthFirstRunBody => S.Format("azimuth_firstrun.body", ("path", _install?.Root ?? ""));
    public string AzimuthFirstRunStatus => _azimuthFirstRunStatus;
    public bool HasAzimuthFirstRunStatus => _azimuthFirstRunStatus.Length > 0;

    /// <summary>Checks the chosen Azimuth again (from <see cref="RefreshHook"/>, so every detection or folder change runs it).</summary>
    internal void RefreshAzimuthFirstRun()
    {
        var never = _install is { } install && AzimuthFirstRun.NeverRun(install.Root);
        if (never != _azimuthNeverRun) Log($"azimuth: first run {(never ? "missing (data has only bin)" : "done")} in {_install?.Root ?? "none"}");
        _azimuthNeverRun = never;
        if (!never) _azimuthFirstRunStatus = "";
    }

    public void LaunchAzimuthForFirstRun()
    {
        _azimuthFirstRunStatus = S[LaunchAzimuthDirectly("first run") ? "azimuth_firstrun.launched" : "azimuth_firstrun.launch_failed"];
        Changed(string.Empty);
    }

    /// <summary>Owner 2026-10-28, Configure: Azimuth's own launcher, to change the game's settings there (no Infinity session).</summary>
    public bool CanLaunchAzimuthDirectly => _install is not null && ModsIdle;

    public void LaunchAzimuthFromConfigure()
    {
        if (!CanLaunchAzimuthDirectly) return;
        if (LaunchAzimuthDirectly("configure")) ConfigurationMessage(S["setup.azimuth_launched"]);
        else ConfigurationMessage(S["azimuth_firstrun.launch_failed"], error: true);
        Changed(string.Empty);
    }

    /// <summary>Starts azimuth.exe as the player would (its own launcher; not <c>--skip-launcher</c>, no ticket, no supervisor).</summary>
    private bool LaunchAzimuthDirectly(string reason)
    {
        if (_install is not { } install) return false;
        try
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(install.HostExe) { WorkingDirectory = install.Root, UseShellExecute = false })?.Dispose();
            Log($"azimuth: opened directly ({reason}): {install.HostExe}");
            return true;
        }
        catch (Exception error) when (error is Win32Exception or IOException or InvalidOperationException)
        {
            Log($"azimuth: direct launch ({reason}) failed: {error.Message}");
            return false;
        }
    }

    public void CheckAzimuthFirstRunAgain()
    {
        RefreshAzimuthFirstRun();
        if (_azimuthNeverRun) _azimuthFirstRunStatus = S["azimuth_firstrun.still"];
        Changed(string.Empty);
    }
}
