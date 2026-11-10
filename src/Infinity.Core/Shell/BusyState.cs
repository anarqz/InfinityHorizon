// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// Owner 2026-11-08: "a proper spinner with a 'loading XXX' feedback, blocking the interaction while the request goes". The one place every
/// network or save action of the shell runs through: while <see cref="IsBusy"/> the window shows a modal overlay with <see cref="Label"/>
/// (updated by <see cref="Step"/> as the action goes on) and, past <see cref="ElapsedAfter"/>, the seconds spent. Cleared on success and on
/// any exception; one action at a time (a second one while busy is not run).
/// <para>
/// Owner 2026-11-10 ("nothing should get freezed"): loading work also opens nested <see cref="Push"/> scopes (startup, a game switch, the
/// local save scan) that stack over the action: the innermost label is shown, and closing a scope (dispose, also on an exception) brings
/// the outer label back; the overlay closes only when the last scope and the action are done.
/// </para>
/// </summary>
public sealed class BusyState(Func<DateTimeOffset>? clock = null)
{
    private readonly Func<DateTimeOffset> _clock = clock ?? (() => DateTimeOffset.UtcNow);
    private readonly List<Frame> _frames = [];
    private DateTimeOffset _since;

    private sealed class Frame(string label, bool action)
    {
        public string Label { get; set; } = label;
        public bool Action { get; } = action;
    }

    /// <summary>The overlay shows the elapsed seconds from this long on.</summary>
    public static TimeSpan ElapsedAfter { get; } = TimeSpan.FromSeconds(15);

    public bool IsBusy => _frames.Count > 0;

    /// <summary>An action entered with <see cref="TryEnter"/> is running (scopes alone are not one).</summary>
    public bool ActionRunning => _frames.Exists(f => f.Action);

    /// <summary>How many scopes and actions are open.</summary>
    public int Depth => _frames.Count;

    /// <summary>What the overlay says now ("Creating your NOOB profile…"), already in the player's language: the innermost open label.</summary>
    public string Label => _frames.Count == 0 ? string.Empty : _frames[^1].Label;

    /// <summary>Raised whenever <see cref="IsBusy"/> or <see cref="Label"/> changes.</summary>
    public event Action? Changed;

    /// <summary>The seconds since the overlay opened, once past <see cref="ElapsedAfter"/>; null before (or when idle).</summary>
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
        if (ActionRunning) return false;
        Open(new Frame(label ?? string.Empty, action: true));
        return true;
    }

    /// <summary>
    /// Opens a loading scope over whatever is showing (nesting allowed). Dispose it (a <c>using</c>) to close it: always, also when the
    /// work throws; a second dispose does nothing.
    /// </summary>
    public BusyScope Push(string label)
    {
        var frame = new Frame(label ?? string.Empty, action: false);
        Open(frame);
        return new BusyScope(this, frame);
    }

    /// <summary>The next step of the innermost open label ("Downloading your HARDCORE save…"); ignored when idle.</summary>
    public void Step(string label)
    {
        if (!IsBusy) return;
        _frames[^1].Label = label ?? string.Empty;
        Changed?.Invoke();
    }

    /// <summary>The running action (<see cref="TryEnter"/>) is done; open scopes stay.</summary>
    public void Exit()
    {
        var at = _frames.FindLastIndex(f => f.Action);
        if (at < 0) return;
        _frames.RemoveAt(at);
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

    private void Open(Frame frame)
    {
        if (_frames.Count == 0) _since = _clock();
        _frames.Add(frame);
        Changed?.Invoke();
    }

    /// <summary>One open loading scope of <see cref="Push"/>.</summary>
    public sealed class BusyScope : IDisposable
    {
        private readonly BusyState _owner;
        private readonly Frame _frame;
        private bool _closed;

        internal BusyScope(BusyState owner, object frame) { _owner = owner; _frame = (Frame)frame; }

        /// <summary>This scope's next step (shown while it is the innermost one).</summary>
        public void Step(string label)
        {
            if (_closed) return;
            _frame.Label = label ?? string.Empty;
            _owner.Changed?.Invoke();
        }

        public void Dispose()
        {
            if (_closed) return;
            _closed = true;
            if (_owner._frames.Remove(_frame)) _owner.Changed?.Invoke();
        }
    }
}
