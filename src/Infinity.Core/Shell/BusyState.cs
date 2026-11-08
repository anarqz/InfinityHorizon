// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// Owner 2026-11-08: "a proper spinner with a 'loading XXX' feedback, blocking the interaction while the request goes". The one place every
/// network or save action of the shell runs through: while <see cref="IsBusy"/> the window shows a modal overlay with <see cref="Label"/>
/// (updated by <see cref="Step"/> as the action goes on) and, past <see cref="ElapsedAfter"/>, the seconds spent. Cleared on success and on
/// any exception; one action at a time (a second one while busy is not run).
/// </summary>
public sealed class BusyState(Func<DateTimeOffset>? clock = null)
{
    private readonly Func<DateTimeOffset> _clock = clock ?? (() => DateTimeOffset.UtcNow);
    private DateTimeOffset _since;

    /// <summary>The overlay shows the elapsed seconds from this long on.</summary>
    public static TimeSpan ElapsedAfter { get; } = TimeSpan.FromSeconds(15);

    public bool IsBusy { get; private set; }

    /// <summary>What the overlay says now ("Creating your NOOB profile…"), already in the player's language.</summary>
    public string Label { get; private set; } = string.Empty;

    /// <summary>Raised whenever <see cref="IsBusy"/> or <see cref="Label"/> changes.</summary>
    public event Action? Changed;

    /// <summary>The seconds since the action began, once past <see cref="ElapsedAfter"/>; null before (or when idle).</summary>
    public int? ElapsedSeconds
    {
        get
        {
            if (!IsBusy) return null;
            var elapsed = _clock() - _since;
            return elapsed >= ElapsedAfter ? (int)elapsed.TotalSeconds : null;
        }
    }

    /// <summary>Enters the busy state with <paramref name="label"/>; false (nothing changes) when an action is already running.</summary>
    public bool TryEnter(string label)
    {
        if (IsBusy) return false;
        IsBusy = true;
        Label = label ?? string.Empty;
        _since = _clock();
        Changed?.Invoke();
        return true;
    }

    /// <summary>The next step of the running action ("Downloading your HARDCORE save…"); ignored when idle.</summary>
    public void Step(string label)
    {
        if (!IsBusy) return;
        Label = label ?? string.Empty;
        Changed?.Invoke();
    }

    public void Exit()
    {
        if (!IsBusy) return;
        IsBusy = false;
        Label = string.Empty;
        Changed?.Invoke();
    }

    /// <summary>Runs <paramref name="work"/> busy: false when another action was running (not run). The state is cleared even when it throws.</summary>
    public async Task<bool> RunAsync(string label, Func<Task> work)
    {
        ArgumentNullException.ThrowIfNull(work);
        if (!TryEnter(label)) return false;
        try { await work().ConfigureAwait(true); }
        finally { Exit(); }
        return true;
    }
}
