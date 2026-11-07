// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Matchmaking;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>One launcher per state directory, and the running play session's overlay bridge and lobby mod set.</summary>
public sealed partial class ShellViewModel
{
    private LauncherLock? _instanceLock;
    // The play session's overlay bridge (OD-54), read off the UI thread.
    private volatile WorldsBridge? _playWorlds;
    /// <summary>The running online launch's lobby mod set (N-H2 reports go to it when it is a runtime source).</summary>
    private IModSetSource? _playModSet;

    /// <summary>One launcher per state directory (ORCHESTRATOR §1.8): taken at startup, retried briefly for an update handoff.</summary>
    internal async Task<bool> AcquireInstanceLockAsync(int attempts = 5)
    {
        for (var attempt = 0; ; attempt++)
        {
            if (_instanceLock is not null) return true;
            try
            {
                using var self = System.Diagnostics.Process.GetCurrentProcess();
                var started = (ulong)new DateTimeOffset(self.StartTime.ToUniversalTime(), TimeSpan.Zero).ToUnixTimeSeconds();
                if (LauncherLock.TryAcquire(_stateDirectory, started, out var acquired, out var holder))
                {
                    _instanceLock = acquired;
                    StartLinkListener(); // CR-L41.2: a second launch hands its join link here
                    return true;
                }
                if (attempt + 1 >= attempts)
                {
                    Log("launcher.lock: held by another launcher" + (holder is { } h ? " (pid " + h.Pid.ToString(CultureInfo.InvariantCulture) + ")" : string.Empty));
                    return false;
                }
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidOperationException or System.ComponentModel.Win32Exception)
            {
                Log("launcher.lock: unavailable (" + error.GetType().Name + ")");
                return false;
            }
            await Task.Delay(TimeSpan.FromSeconds(1));
        }
    }

    private void ReleaseInstanceLock()
    {
        _handoff?.Dispose();
        _handoff = null;
        _instanceLock?.Dispose();
        _instanceLock = null;
    }
}
