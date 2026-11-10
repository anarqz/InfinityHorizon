// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;

namespace Infinity.Core.Shell;

/// <summary>One time the UI thread did not answer: how long, and what the overlay said meanwhile (empty: no overlay).</summary>
public sealed record UiStall(TimeSpan Duration, DateTimeOffset At, string Activity);

/// <summary>
/// Owner 2026-11-10: "the user should NEVER be presented with a freezed screen, even for 1s". A background thread posts a probe to the
/// UI thread every <see cref="Interval"/> (<c>post</c>: the window's dispatcher at its highest priority) and times how long the probe
/// waits; a wait past <see cref="Threshold"/> is a stall: the UI thread was busy with one piece of work (file IO, hashing, a big refresh)
/// and could neither draw nor take input. Each stall is reported once, when the thread answers again (<see cref="Stalled"/>). Debug builds
/// and the harness fail on it; release builds log it (rate-limited by the caller).
/// </summary>
public sealed class UiStallWatchdog : IDisposable
{
    private readonly Func<Action, bool> _post;
    private readonly Func<string> _activity;
    private readonly CancellationTokenSource _stop = new();
    private readonly Lock _gate = new();
    private readonly List<UiStall> _stalls = [];
    private Thread? _thread;

    /// <param name="post">Queues an action on the UI thread; false when the UI thread is gone (the watchdog then stops).</param>
    /// <param name="activity">What the overlay says now (read on the watchdog thread; a plain field read).</param>
    public UiStallWatchdog(Func<Action, bool> post, Func<string>? activity = null)
    {
        _post = post ?? throw new ArgumentNullException(nameof(post));
        _activity = activity ?? (() => string.Empty);
    }

    /// <summary>A probe that waits longer than this is a stall (the owner's bar is ~100 ms).</summary>
    public TimeSpan Threshold { get; init; } = TimeSpan.FromMilliseconds(100);

    /// <summary>How often the UI thread is probed.</summary>
    public TimeSpan Interval { get; init; } = TimeSpan.FromMilliseconds(40);

    /// <summary>Raised on the watchdog thread for each stall, once the UI thread answered again.</summary>
    public event Action<UiStall>? Stalled;

    /// <summary>Every stall seen since the start (or the last <see cref="Reset"/>).</summary>
    public IReadOnlyList<UiStall> Stalls { get { lock (_gate) return [.. _stalls]; } }

    /// <summary>The longest stall seen since the start (or the last <see cref="Reset"/>); zero when none.</summary>
    public TimeSpan Longest { get { lock (_gate) return _stalls.Count == 0 ? TimeSpan.Zero : _stalls.Max(s => s.Duration); } }

    public void Reset() { lock (_gate) _stalls.Clear(); }

    public void Start()
    {
        if (_thread is not null) return;
        _thread = new Thread(Loop) { IsBackground = true, Name = "ui-stall-watchdog", Priority = ThreadPriority.AboveNormal };
        _thread.Start();
    }

    private void Loop()
    {
        var token = _stop.Token;
        while (!token.IsCancellationRequested)
        {
            // not disposed: a probe still queued when the watchdog stops sets it later (it holds no kernel handle)
            var answered = new ManualResetEventSlim(false);
            var sent = Stopwatch.GetTimestamp();
            var activity = string.Empty;
            try { if (!_post(() => answered.Set())) return; }
            catch (Exception e) when (e is InvalidOperationException or TaskCanceledException or ObjectDisposedException) { return; }
            // wait for the answer; past the threshold, remember what the overlay said while the UI thread was stuck
            if (!answered.Wait(Threshold))
            {
                activity = _activity();
                while (!answered.Wait(Interval)) { if (token.IsCancellationRequested) return; }
            }
            var waited = Stopwatch.GetElapsedTime(sent);
            if (waited > Threshold)
            {
                var stall = new UiStall(waited, DateTimeOffset.UtcNow, activity);
                lock (_gate) _stalls.Add(stall);
                try { Stalled?.Invoke(stall); }
                catch (Exception e) when (e is IOException or InvalidOperationException) { }
            }
            try { if (token.WaitHandle.WaitOne(Interval)) return; }
            catch (ObjectDisposedException) { return; }
        }
    }

    public void Dispose()
    {
        _stop.Cancel();
        _thread?.Join(TimeSpan.FromSeconds(1));
        _stop.Dispose();
    }
}
