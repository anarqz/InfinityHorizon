// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Windows.Threading;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-08: "a proper spinner with a 'loading XXX' feedback, blocking the interaction while the request goes". Every network or
/// save action goes through <see cref="Run(string, Func{Task})"/> (or the sign-in paths), which holds <see cref="BusyState"/>: the window
/// shows a modal overlay over the content with a spinner and <see cref="BusyLabel"/>, blocks clicks and keys, and past 15 s shows the
/// seconds spent. <see cref="Idle"/> stays the gate of every button; the overlay is cleared on success and on any error.
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly BusyState _busyState = new();
    private DispatcherTimer? _busyTimer;

    /// <summary>Test seam: the busy state's transitions (entered with a label, steps, exit), in order.</summary>
    internal List<string>? BusyTrace { get; set; }

    /// <summary>The modal overlay is up.</summary>
    public bool ShowBusyOverlay => _busyState.IsBusy;

    public string BusyLabel => _busyState.Label;

    public string BusyElapsed => _busyState.ElapsedSeconds is { } seconds
        ? S.Format("busy.elapsed", ("seconds", seconds.ToString(CultureInfo.CurrentCulture))) : string.Empty;

    public bool HasBusyElapsed => _busyState.ElapsedSeconds is not null;

    /// <summary>A sign-in waiting on the network can still be cancelled from the overlay (it writes nothing); a write never can.</summary>
    public bool CanCancelBusy => _signIn is not null && _busyState.IsBusy;

    private void EnterBusy(string label)
    {
        if (!_busyState.TryEnter(label)) return;
        BusyTrace?.Add("enter:" + label);
        try
        {
            _busyTimer ??= new DispatcherTimer(TimeSpan.FromSeconds(1), DispatcherPriority.Background, (_, _) => ChangedBusy(), Dispatcher.CurrentDispatcher);
            _busyTimer.Start();
        }
        catch (InvalidOperationException)
        {
            // no dispatcher (a test thread): the overlay still shows, without the elapsed seconds
        }
        ChangedBusy();
    }

    /// <summary>The running action's next step ("Downloading your HARDCORE save…").</summary>
    private void BusyStep(string label)
    {
        if (!_busyState.IsBusy) return;
        _busyState.Step(label);
        BusyTrace?.Add("step:" + label);
        ChangedBusy();
    }

    private void ExitBusy()
    {
        if (!_busyState.IsBusy) return;
        _busyState.Exit();
        _busyTimer?.Stop();
        BusyTrace?.Add("exit");
        ChangedBusy();
    }

    private void ChangedBusy()
    {
        Changed(nameof(ShowBusyOverlay));
        Changed(nameof(BusyLabel));
        Changed(nameof(BusyElapsed));
        Changed(nameof(HasBusyElapsed));
        Changed(nameof(CanCancelBusy));
    }

    /// <summary>
    /// <paramref name="work"/> under the overlay: a step of the running action when one is (a check inside a creation), else its own
    /// <see cref="Run(string, Func{Task})"/>.
    /// </summary>
    private async Task RunOrStep(string label, Func<Task> work)
    {
        if (!_busyState.IsBusy && !_busy) { await Run(label, work); return; }
        if (!_busyState.IsBusy) { await work(); return; } // a mod operation's own progress is showing: no second indicator
        var before = _busyState.Label;
        BusyStep(label);
        try { await work(); }
        finally { BusyStep(before); }
    }

    /// <summary>The label of a profile mode in the busy texts ("NOOB", "HARDCORE").</summary>
    private string BusyMode(string? mode) => S[mode == "hardcore" ? "busy.mode_hardcore" : "busy.mode_noob"];
}
