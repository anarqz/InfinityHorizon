// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Windows.Threading;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10: "We need to be absolute sure the user should NEVER be presented with a freezed screen, even for 1s." The dispatcher
/// watchdog (<see cref="UiStallWatchdog"/>) times the window's thread from a background thread; every stall past the threshold is logged
/// with what the overlay said (at most 20 lines a minute), so a regression shows in launcher.log. The harness fails on it.
/// </summary>
public sealed partial class ShellViewModel
{
    private UiStallWatchdog? _watchdog;
    private long _watchdogMinute;
    private int _watchdogLines;

    /// <summary>Starts the watchdog on <paramref name="dispatcher"/> (once).</summary>
    internal UiStallWatchdog StartUiWatchdog(Dispatcher dispatcher, TimeSpan? threshold = null)
    {
        ArgumentNullException.ThrowIfNull(dispatcher);
        if (_watchdog is { } running) return running;
        var watchdog = _watchdog = new UiStallWatchdog(action =>
        {
            if (dispatcher.HasShutdownStarted) return false;
            dispatcher.BeginInvoke(DispatcherPriority.Send, action);
            return true;
        }, () => BusyActivity) { Threshold = threshold ?? TimeSpan.FromMilliseconds(100) };
        watchdog.Stalled += stall =>
        {
            var minute = DateTimeOffset.UtcNow.ToUnixTimeSeconds() / 60;
            if (minute != Interlocked.Exchange(ref _watchdogMinute, minute)) Interlocked.Exchange(ref _watchdogLines, 0);
            if (Interlocked.Increment(ref _watchdogLines) > 20) return;
            Log("ui: the window's thread was blocked " + ((long)stall.Duration.TotalMilliseconds).ToString(CultureInfo.InvariantCulture) + " ms"
                + (stall.Activity.Length > 0 ? " (overlay: " + stall.Activity + ")" : ""));
        };
        watchdog.Start();
        return watchdog;
    }

    private void StopUiWatchdog()
    {
        _watchdog?.Dispose();
        _watchdog = null;
    }
}
