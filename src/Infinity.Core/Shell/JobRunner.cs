// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// How a job is shown (owner 2026-11-10): "SOME loads must lock the screen, for example, enabling/disabling 40 mods at once with INSTALL ALL.
/// Others must appear as an overlay in the center of the screen, like the sign-in or loading career data. [The rest] a kind of a toast
/// that appears on bottom-right of the screen showing the queue of small jobs happening in background."
/// </summary>
public enum JobPresentation
{
    /// <summary>The whole window is blocked (nothing is clickable) until it ends: bulk mod changes, a mods-folder move, hook install, verification.</summary>
    Lock,
    /// <summary>The centre overlay (sign-in, the service status and minimum version, the profile and career data, a forced update).</summary>
    Centre,
    /// <summary>A toast at the bottom right; the launcher stays usable (<see cref="JobSpec.ShouldBlockPlay"/> may still hold PLAY).</summary>
    Background,
}

public enum JobState { Running, Succeeded, Failed, Cancelled }

/// <summary>What one job is: its dedupe key, its words (already in the player's language), how it shows and what it holds.</summary>
/// <param name="Key">Dedupe key: a second start with the key of a running job joins it (two clicks never send twice).</param>
/// <param name="ShouldBlockPlay">A Background job PLAY waits for (its tooltip names the job); Lock and Centre jobs block everything anyway.</param>
/// <param name="Cancellable">The toast offers Cancel (the work receives the cancellation).</param>
/// <param name="Work">The job; its slow parts run off the UI thread (network awaits, <c>Task.Run</c> for files and hashes).</param>
public sealed record JobSpec(string Key, string Label, JobPresentation Presentation, bool ShouldBlockPlay, Func<JobContext, Task> Work,
    bool Cancellable = false);

/// <summary>What a running job can tell its toast.</summary>
public sealed class JobContext
{
    private readonly Job _job;

    internal JobContext(Job job, CancellationToken cancellation) { _job = job; Cancellation = cancellation; }

    public CancellationToken Cancellation { get; }

    /// <summary>Progress, when known: a fraction (0..1) and/or "3 of 40"; null for the indeterminate spinner.</summary>
    public void Report(double? fraction, string? text = null) => _job.SetProgress(fraction, text);

    /// <summary>A new label for the running job ("Downloading 3 of 40…").</summary>
    public void Relabel(string label) => _job.SetLabel(label);
}

/// <summary>One started job, as its toast shows it.</summary>
public sealed class Job : IDisposable
{
    private readonly JobRunner _runner;
    private CancellationTokenSource? _cancel;

    internal Job(JobRunner runner, JobSpec spec, long id) { _runner = runner; Spec = spec; Id = id; Label = spec.Label; }

    public long Id { get; }

    public JobSpec Spec { get; private set; }

    public string Key => Spec.Key;

    public string Label { get; private set; }

    public JobState State { get; private set; } = JobState.Running;

    public double? Progress { get; private set; }

    public string? ProgressText { get; private set; }

    /// <summary>Why it failed, in the player's words (the toast stays with it and a Retry).</summary>
    public string? Error { get; private set; }

    /// <summary>The running (or the last) attempt: awaiting it awaits the job, joined starts included. It never throws.</summary>
    public Task Completion { get; private set; } = Task.CompletedTask;

    /// <summary>
    /// The running (or the last) attempt's outcome, as soon as its work ended (before a finished toast's linger): what every caller that
    /// started or joined it observes, the shared job's failure included. It never throws.
    /// </summary>
    public Task<JobState> Finished => _finished.Task;
    private TaskCompletionSource<JobState> _finished = Settled(JobState.Succeeded);

    /// <summary>What the last attempt failed with (null when it did not fail); a joiner rethrows it (<see cref="JobRunner.RunOrThrowAsync"/>).</summary>
    public Exception? Failure { get; private set; }

    private static TaskCompletionSource<JobState> Settled(JobState state)
    {
        var done = new TaskCompletionSource<JobState>(TaskCreationOptions.RunContinuationsAsynchronously);
        done.SetResult(state);
        return done;
    }

    public bool Running => State == JobState.Running;

    internal void SetProgress(double? fraction, string? text)
    {
        Progress = fraction is { } f ? Math.Clamp(f, 0, 1) : null;
        ProgressText = text;
        _runner.Raise();
    }

    internal void SetLabel(string label)
    {
        Label = label ?? string.Empty;
        _runner.Raise();
    }

    internal CancellationToken Begin()
    {
        _cancel?.Dispose();
        _cancel = new CancellationTokenSource();
        State = JobState.Running; Error = null; Failure = null; Progress = null; ProgressText = null; Label = Spec.Label;
        _finished = new TaskCompletionSource<JobState>(TaskCreationOptions.RunContinuationsAsynchronously);
        return _cancel.Token;
    }

    internal void End(JobState state, string? error, Exception? failure = null)
    {
        State = state;
        Error = error;
        Failure = failure;
        Progress = state == JobState.Succeeded ? 1 : Progress;
        _finished.TrySetResult(state);
    }

    internal void Settle() => _finished.TrySetResult(JobState.Failed);

    internal void Attach(Task completion) => Completion = completion;

    /// <summary>The toast's Cancel (only a <see cref="JobSpec.Cancellable"/> job).</summary>
    public void Cancel()
    {
        if (Spec.Cancellable && Running) _cancel?.Cancel();
    }

    /// <summary>The toast's Retry of a failed job: the same work again, same key.</summary>
    public Task Retry() => State == JobState.Failed ? _runner.Restart(this) : Completion;

    /// <summary>Releases the attempt's cancellation (the runner does it when the job ends).</summary>
    public void Dispose()
    {
        _cancel?.Dispose();
        _cancel = null;
    }

    /// <summary>The toast's ✕ on a failed job: it goes away (the failure was shown).</summary>
    public void Dismiss() => _runner.Remove(this);
}

/// <summary>
/// The launcher's one job runner (owner 2026-11-10). Every load or write is a job with one <see cref="JobPresentation"/>: Lock and Centre
/// open the window's overlay through <c>overlay</c> (a scope closed when the job ends, also on an exception); Background jobs are toasts
/// (<see cref="Toasts"/>). A finished Background job shows its check for <see cref="DoneLinger"/>, then goes away; a failed one stays
/// with its reason and Retry (never silent). Used on the UI thread only (its state and <see cref="Changed"/>).
/// </summary>
public sealed class JobRunner(Func<JobPresentation, string, IDisposable> overlay, Func<Exception, string> describe, TimeProvider? clock = null)
{
    private readonly Func<JobPresentation, string, IDisposable> _overlay = overlay ?? throw new ArgumentNullException(nameof(overlay));
    private readonly Func<Exception, string> _describe = describe ?? throw new ArgumentNullException(nameof(describe));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly List<Job> _jobs = [];
    private long _next;

    /// <summary>How long a finished toast shows its check.</summary>
    public TimeSpan DoneLinger { get; init; } = TimeSpan.FromSeconds(1.5);

    /// <summary>Any job started, progressed, ended or went away.</summary>
    public event Action? Changed;

    /// <summary>Raised on the UI thread when a job ends (success or not): what it changed is shown again.</summary>
    public event Action<Job>? Ended;

    /// <summary>The Background jobs on screen, oldest first (running, just finished, failed).</summary>
    public IReadOnlyList<Job> Toasts => [.. _jobs.Where(j => j.Spec.Presentation == JobPresentation.Background)];

    /// <summary>Every job running now.</summary>
    public IReadOnlyList<Job> Running => [.. _jobs.Where(j => j.Running)];

    /// <summary>The running jobs PLAY waits for (their labels are PLAY's tooltip).</summary>
    public IReadOnlyList<Job> BlockingPlay => [.. _jobs.Where(j => j.Running && (j.Spec.ShouldBlockPlay || j.Spec.Presentation != JobPresentation.Background))];

    /// <summary>The running job with <paramref name="key"/>, or null.</summary>
    public Job? Find(string key) => _jobs.FirstOrDefault(j => j.Running && j.Key == key);

    /// <summary>Starts <paramref name="spec"/>, or joins the running job with its key. The returned job's <see cref="Job.Completion"/> never throws.</summary>
    public Job Start(JobSpec spec)
    {
        ArgumentNullException.ThrowIfNull(spec);
        if (Find(spec.Key) is { } running) return running;
        // a failed or finished toast of the same key gives way to the new attempt
        _jobs.RemoveAll(j => !j.Running && j.Key == spec.Key);
        var job = new Job(this, spec, ++_next);
        _jobs.Add(job);
        job.Attach(RunAsync(job));
        Raise();
        return job;
    }

    /// <summary>Starts and awaits <paramref name="spec"/>; true when it succeeded.</summary>
    public async Task<bool> RunAsync(JobSpec spec)
    {
        var job = Start(spec);
        await job.Completion.ConfigureAwait(true);
        return job.State == JobState.Succeeded;
    }

    /// <summary>
    /// Starts <paramref name="spec"/> (or joins the running job with its key) and awaits its attempt's end: a failure is rethrown here as
    /// it happened, a cancel is an <see cref="OperationCanceledException"/>. A joiner observes the shared job's real outcome (never a
    /// silent success for a job that failed); the failure also stays in its toast with Retry.
    /// </summary>
    public async Task RunOrThrowAsync(JobSpec spec)
    {
        var job = Start(spec);
        var state = await job.Finished.ConfigureAwait(true);
        switch (state)
        {
            case JobState.Failed when job.Failure is { } failure:
                System.Runtime.ExceptionServices.ExceptionDispatchInfo.Throw(failure);
                break;
            case JobState.Failed:
                throw new JobFailedException(job.Error ?? "the job failed");
            case JobState.Cancelled:
                throw new OperationCanceledException("the job was cancelled");
        }
    }

    internal Task Restart(Job job)
    {
        if (job.Running) return job.Completion;
        if (!_jobs.Contains(job)) _jobs.Add(job);
        job.Attach(RunAsync(job));
        Raise();
        return job.Completion;
    }

    internal void Remove(Job job)
    {
        if (job.Running) return;
        if (_jobs.Remove(job)) Raise();
    }

    /// <summary>
    /// The subscribers' turn. 3.1.1 (a "Reading your mods… 45 of 45" toast that never closed): a subscriber that throws must never stop a
    /// job's own lifecycle (its end, its toast's removal), so its error is described (logged by the shell) and the job goes on.
    /// </summary>
    internal void Raise()
    {
        try { Changed?.Invoke(); }
        catch (Exception error) when (error is not OutOfMemoryException) { _ = _describe(error); }
    }

    private void RaiseEnded(Job job)
    {
        try { Ended?.Invoke(job); }
        catch (Exception error) when (error is not OutOfMemoryException) { _ = _describe(error); }
    }

    private async Task RunAsync(Job job)
    {
        var token = job.Begin();
        IDisposable? scope = null;
        try
        {
            if (job.Spec.Presentation != JobPresentation.Background) scope = _overlay(job.Spec.Presentation, job.Spec.Label);
            await Task.Yield(); // the toast or overlay is drawn before the work begins
            await job.Spec.Work(new JobContext(job, token)).ConfigureAwait(true);
            job.End(JobState.Succeeded, null);
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested)
        {
            job.End(JobState.Cancelled, null);
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            job.End(JobState.Failed, _describe(error), error);
        }
        finally
        {
            job.Settle(); // a fatal error left it unanswered: its waiters still end
            scope?.Dispose();
            job.Dispose(); // this attempt's cancellation; a Retry makes a new one
        }
        Raise();
        RaiseEnded(job);
        if (job.State is JobState.Succeeded or JobState.Cancelled)
        {
            if (job.Spec.Presentation == JobPresentation.Background && DoneLinger > TimeSpan.Zero)
            {
                try { await Task.Delay(DoneLinger, _clock).ConfigureAwait(true); }
                catch (OperationCanceledException) { }
            }
            if (!job.Running && _jobs.Remove(job)) Raise();
        }
    }
}
