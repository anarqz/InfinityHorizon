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
/// <para>
/// Owner 2026-11-10 ("nothing should get freezed"): loading work (startup, a game switch, the local save scan) opens a
/// <see cref="Loading"/> scope (<c>using (Loading(...)) await ...</c>), which nests and always closes. The slow part runs off the UI
/// thread; the spinner turns on its own thread (<see cref="ThreadedSpinner"/>). When the last scope closes and no action runs, the screen
/// behind is rebuilt (<see cref="SettleAfterBusy"/>): cards built while busy carried disabled buttons and older data.
/// </para>
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly BusyState _busyState = new();
    private DispatcherTimer? _busyTimer;
    private bool _busyWired, _busyShown, _settling;

    /// <summary>Test seam: the busy state's transitions (entered with a label, steps, exit), in order.</summary>
    internal List<string>? BusyTrace { get; set; }

    /// <summary>Test seam: every rebuild of the screen after the overlay closed.</summary>
    internal int SettleCount { get; private set; }

    /// <summary>The modal overlay is up.</summary>
    public bool ShowBusyOverlay => _busyState.IsBusy;

    public string BusyLabel => _busyState.Label;

    /// <summary>Owner 2026-11-10: the overlay also says why the screen does not take clicks now.</summary>
    public string BusyWhy => !_busyState.IsBusy ? string.Empty : S[_lockDepth > 0 ? "busy.why_lock" : "busy.why"];

    public string BusyElapsed => _busyState.ElapsedSeconds is { } seconds
        ? S.Format("busy.elapsed", ("seconds", seconds.ToString(CultureInfo.CurrentCulture))) : string.Empty;

    public bool HasBusyElapsed => _busyState.ElapsedSeconds is not null;

    /// <summary>A sign-in waiting on the network can still be cancelled from the overlay (it writes nothing); a write never can.</summary>
    public bool CanCancelBusy => _signIn is not null && _busyState.IsBusy;

    /// <summary>The label the overlay shows now, for the dispatcher watchdog's thread (a field read).</summary>
    internal string BusyActivity => _busyActivity;
    private volatile string _busyActivity = string.Empty;

    private void WireBusy()
    {
        if (_busyWired) return;
        _busyWired = true;
        _busyState.Changed += OnBusyChanged;
    }

    /// <summary>
    /// A loading scope over the screen: the overlay with <paramref name="label"/> until it is disposed (nested scopes show the innermost
    /// label). The caller moves its slow work off the UI thread (<c>await Task.Run(...)</c>) inside it.
    /// </summary>
    internal BusyState.BusyScope Loading(string label)
    {
        WireBusy();
        BusyTrace?.Add("push:" + label);
        return _busyState.Push(label);
    }

    private void EnterBusy(string label)
    {
        WireBusy();
        if (!_busyState.TryEnter(label)) return;
        BusyTrace?.Add("enter:" + label);
    }

    /// <summary>The running action's next step ("Downloading your HARDCORE save…").</summary>
    private void BusyStep(string label)
    {
        if (!_busyState.IsBusy) return;
        _busyState.Step(label);
        BusyTrace?.Add("step:" + label);
    }

    private void ExitBusy()
    {
        if (!_busyState.ActionRunning) return;
        _busyState.Exit();
        BusyTrace?.Add("exit");
    }

    private void OnBusyChanged()
    {
        _busyActivity = _busyState.Label;
        var shown = _busyState.IsBusy;
        var closed = !shown && _busyShown;
        if (shown != _busyShown)
        {
            _busyShown = shown;
            if (shown) StartBusyTimer();
            else { _busyTimer?.Stop(); BusyTrace?.Add("idle"); }
        }
        ChangedBusy();
        if (closed) SettleAfterBusy();
    }

    private void StartBusyTimer()
    {
        try
        {
            _busyTimer ??= new DispatcherTimer(TimeSpan.FromSeconds(1), DispatcherPriority.Background, (_, _) => ChangedBusy(), Dispatcher.CurrentDispatcher);
            _busyTimer.Start();
        }
        catch (InvalidOperationException)
        {
            // no dispatcher (a test thread): the overlay still shows, without the elapsed seconds
        }
    }

    private void ChangedBusy()
    {
        Changed(nameof(ShowBusyOverlay));
        Changed(nameof(BusyLabel));
        Changed(nameof(BusyWhy));
        Changed(nameof(BusyElapsed));
        Changed(nameof(HasBusyElapsed));
        Changed(nameof(CanCancelBusy));
        Changed(nameof(ShowModProgressOverlay));
    }

    /// <summary>
    /// The overlay closed (no scope, no action): what it covered is rebuilt from the data now in memory, so nothing behind it stays as it
    /// was drawn while busy (PLAY's buttons were disabled then, cards may hold an older /me). No request and no slow read: the local save
    /// facts were scanned off the UI thread before the overlay closed (<see cref="PrimeLocalFactsAsync"/>).
    /// </summary>
    private bool CanSettle => !_busy && !_busyState.IsBusy && !_settling;

    private void SettleAfterBusy()
    {
        if (!CanSettle) return;
        _settling = true;
        using var span = Span();
        try
        {
            SettleCount++;
            BusyTrace?.Add("settle");
            if (InShell)
            {
                FillPlayBoard();
                if (ShowSaveManager) FillSaveCards();
            }
            Changed(string.Empty);
        }
        finally { _settling = false; }
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
